//! mu-kgu.6 — integration tests for the composition of
//! [`CompactionPolicy`] (mu-core) and
//! [`AnthropicCacheStrategy`](super::AnthropicCacheStrategy) (mu-ai).
//!
//! ## Invariant under test
//!
//! Running `AnthropicCacheStrategy::boundaries()` on a post-compaction
//! rope produced by any [`CompactionPolicy`] places its boundary
//! AT or AFTER the position of the last span in the post-compaction
//! rope that was either:
//!
//! 1. kept verbatim from the pre-compaction rope AND was itself
//!    stable + cacheable, OR
//! 2. is a newly-inserted [`SpanKind::CompactionSummary`] span —
//!    which the policy constructs with `RetentionClass::Pinned`, and
//!    therefore via [`Span::new`] is also cacheable.
//!
//! Equivalently: compaction NEVER shrinks the cacheable prefix below
//! the kept-stable span set in the post-rope. It MAY EXTEND the
//! prefix when a Pinned summary span replaces a volatile span that
//! previously truncated it.
//!
//! Per the parent bead (mu-kgu) acceptance: *"cache-boundary
//! composition works (compacted prefix becomes a new cacheable
//! boundary)."*
//!
//! ## Why this is a real test, not a tautology
//!
//! `AnthropicCacheStrategy` walks the rope front-to-back and stops at
//! the FIRST non-stable or non-cacheable span. So composition asks
//! structural questions of the policy + spec:
//!
//! - Does the policy preserve relative order of kept spans?
//!   (Yes — `hash_summary::surgery` iterates in original index order.)
//! - Does a Pinned summary heal a volatile hole that previously
//!   truncated the prefix?
//!   (Yes — Pinned via `Span::new` ⇒ cacheable.)
//! - Does the post-compaction boundary ever shrink relative to "last
//!   kept-stable position in the post-rope"?
//!   (No — this file is the regression test.)

use std::sync::Arc;

use mu_core::context::compaction::hash_summary::{
    Blake3Hasher, HashAndSummaryPolicy, Judge, JudgeError, SpanHasher,
};
use mu_core::context::compaction::CompactionPolicy;
use mu_core::context::{
    prefix_forensics, CacheBoundary, CacheMarker, CacheStrategy, ProjectionTarget,
    ProviderRenderer, RetainedRope, RetentionClass, Span, SpanKind,
};

use super::{AnthropicCacheStrategy, AnthropicProviderRenderer};

// ---------------------------------------------------------------------
// Mock compaction policy — drops spans by id, summarizes the rest.
// ---------------------------------------------------------------------

/// Structural mock: `compact()` returns a rope built by taking every
/// pre-rope span whose id is in `keep_ids` verbatim, and replacing
/// every absorbed span with a single Pinned `CompactionSummary` span
/// inserted at the position of the earliest absorbed span.
///
/// Mirrors `HashAndSummaryPolicy`'s surgery shape but skips the
/// judge call — lets the test pick which spans get kept by id rather
/// than by hash, without coupling to blake3 output.
struct MockDropPolicy {
    keep_ids: Vec<&'static str>,
    summary_text: &'static str,
}

impl CompactionPolicy for MockDropPolicy {
    fn compact(
        &self,
        rope: &RetainedRope,
        _target_tokens: usize,
    ) -> mu_core::context::CompactionResult {
        let spans = rope.spans();
        let mut absorbed_ids: Vec<String> = Vec::new();
        let mut earliest_absorbed_idx: Option<usize> = None;
        let mut new_spans: Vec<Span> = Vec::with_capacity(spans.len() + 1);

        // First pass: identify earliest absorbed index.
        for (i, span) in spans.iter().enumerate() {
            if !self.keep_ids.contains(&span.id()) && earliest_absorbed_idx.is_none() {
                earliest_absorbed_idx = Some(i);
            }
            if !self.keep_ids.contains(&span.id()) {
                absorbed_ids.push(span.id().to_string());
            }
        }

        if absorbed_ids.is_empty() {
            return mu_core::context::CompactionResult::identity(rope.clone());
        }

        let absorbed_arcs: Vec<Arc<str>> =
            absorbed_ids.iter().map(|s| Arc::from(s.as_str())).collect();
        let summary_span = Span::new(
            "mock-summary",
            SpanKind::CompactionSummary {
                absorbed_span_ids: absorbed_arcs,
                generated_at_unix_ms: 0,
                policy_id: "mock-drop-v1".to_string(),
            },
            self.summary_text,
            RetentionClass::Pinned,
        );

        let insertion_idx = earliest_absorbed_idx.expect("absorbed non-empty");
        let mut inserted = false;
        for (i, span) in spans.iter().enumerate() {
            if i == insertion_idx {
                new_spans.push(summary_span.clone());
                inserted = true;
            }
            if self.keep_ids.contains(&span.id()) {
                new_spans.push(span.clone());
            }
        }
        if !inserted {
            new_spans.push(summary_span);
        }

        mu_core::context::CompactionResult {
            rope: RetainedRope::from_spans(new_spans),
            decisions: Vec::new(),
            tokens_before: 0,
            tokens_after: 0,
            wall_clock_us: 0,
        }
    }
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

fn boundaries(rope: &RetainedRope) -> Vec<CacheBoundary> {
    AnthropicCacheStrategy::new().boundaries(rope)
}

/// Index in `rope` of the last span that is either stable+cacheable
/// itself OR a `CompactionSummary` (which is Pinned/cacheable by
/// construction). Used as the "floor" the boundary must reach.
fn last_eligible_index(rope: &RetainedRope) -> Option<usize> {
    rope.spans()
        .iter()
        .enumerate()
        .rev()
        .find(|(_, s)| {
            let is_summary = matches!(s.kind(), SpanKind::CompactionSummary { .. });
            (s.retention().is_stable() && s.cacheable()) || is_summary
        })
        .map(|(i, _)| i)
}

/// Index in `rope` of the last kept-stable span (i.e., the last span
/// in the contiguous stable+cacheable prefix). `None` if the very
/// first span is volatile.
fn last_contiguous_stable_index(rope: &RetainedRope) -> Option<usize> {
    let mut last: Option<usize> = None;
    for (i, s) in rope.spans().iter().enumerate() {
        if s.retention().is_stable() && s.cacheable() {
            last = Some(i);
        } else {
            break;
        }
    }
    last
}

// ---------------------------------------------------------------------
// Tests — structural mock policy
// ---------------------------------------------------------------------

/// Scenario A: drop the volatile tail, keep the stable head.
///
/// Pre-rope:  [sys(Startup), u1(Hot), a1(Hot), u2(Warm), a2(Warm)]
///   pre-boundary  = at(2)  (a1 is the last stable+cacheable span)
///
/// Mock keeps [sys, u1, a1]; absorbs [u2, a2].
/// Post-rope: [sys, u1, a1, summary(Pinned)]
///   post-boundary = at(3)  (summary is Pinned/cacheable)
///
/// Invariant: new boundary ≥ index of last kept-stable span (a1=2).
/// New boundary lands ON the summary span — intentional per spec
/// (CompactionSummary inherits Pinned, extending the cacheable
/// prefix).
#[test]
fn drop_volatile_tail_extends_boundary_to_summary_span() {
    let pre_rope = RetainedRope::from_spans(vec![
        Span::new(
            "sys",
            SpanKind::System,
            "you are mu",
            RetentionClass::Startup,
        ),
        Span::new("u1", SpanKind::User, "hi", RetentionClass::Hot),
        Span::new("a1", SpanKind::Assistant, "hello", RetentionClass::Hot),
        Span::new("u2", SpanKind::User, "now?", RetentionClass::Warm),
        Span::new("a2", SpanKind::Assistant, "noon", RetentionClass::Warm),
    ]);
    let pre = boundaries(&pre_rope);
    // mu-yqeq.8: two boundaries — system (0) + last-in-prefix (2).
    assert_eq!(
        pre,
        vec![CacheBoundary::at(0), CacheBoundary::at(2)],
        "sanity: pre-rope shape",
    );

    let policy = MockDropPolicy {
        keep_ids: vec!["sys", "u1", "a1"],
        summary_text: "u2/a2 absorbed",
    };
    let result = policy.compact(&pre_rope, 1_000);
    let post_rope = result.rope;

    assert_eq!(post_rope.len(), 4, "kept(3) + 1 summary span");
    assert!(matches!(
        post_rope.spans()[3].kind(),
        SpanKind::CompactionSummary { .. }
    ));

    let post = boundaries(&post_rope);
    assert_eq!(
        post,
        vec![CacheBoundary::at(0), CacheBoundary::at(3)],
        "boundaries: system (0) + Pinned summary at the new tail (3)",
    );

    // Invariant: post-LAST-boundary ≥ position of last kept-stable span.
    // Last kept-stable in post-rope is index 2 ("a1"); last boundary is 3. ✓
    let floor = last_contiguous_stable_index(&post_rope).unwrap();
    assert!(post.last().unwrap().message_index >= floor);
}

/// Scenario B: pre-rope has a volatile *prefix* — no cacheable prefix
/// existed at all. Compaction absorbs the volatile head into a Pinned
/// summary, CREATING a cacheable prefix where none was.
///
/// Pre-rope:  [u1(Warm), a1(Hot)]
///   pre-boundary  = []    (leading volatile span ⇒ empty prefix)
///
/// Mock keeps [a1]; absorbs [u1] → summary inserted at position 0.
/// Post-rope: [summary(Pinned), a1(Hot)]
///   post-boundary = at(1) (both stable+cacheable, contiguous)
///
/// This is the load-bearing case: the operator's prediction
/// (parent-bead body) that *"compaction creates a natural new stable
/// prefix → composes with CacheStrategy"* — exercised here.
#[test]
fn absorb_volatile_prefix_creates_cacheable_prefix() {
    let pre_rope = RetainedRope::from_spans(vec![
        Span::new("u1", SpanKind::User, "noise", RetentionClass::Warm),
        Span::new("a1", SpanKind::Assistant, "answer", RetentionClass::Hot),
    ]);
    assert!(
        boundaries(&pre_rope).is_empty(),
        "sanity: leading volatile ⇒ no cacheable prefix",
    );

    let policy = MockDropPolicy {
        keep_ids: vec!["a1"],
        summary_text: "u1 absorbed",
    };
    let post_rope = policy.compact(&pre_rope, 1_000).rope;

    // [summary(Pinned), a1(Hot)] — both stable+cacheable.
    assert_eq!(post_rope.len(), 2);
    assert!(matches!(
        post_rope.spans()[0].kind(),
        SpanKind::CompactionSummary { .. }
    ));
    assert_eq!(post_rope.spans()[1].id(), "a1");

    let post = boundaries(&post_rope);
    // mu-chiw: the summary span (0) is the intro-prefix end — the
    // Assistant span (1) is a conversation span, so the intro stops
    // before it — and (1) is the conversation run-end anchor. Both
    // marked: compaction still CREATED a cacheable prefix where none
    // existed, now with the conversation anchor on top.
    assert_eq!(
        post,
        vec![CacheBoundary::at(0), CacheBoundary::at(1)],
        "compaction CREATED a cacheable prefix that did not exist pre-compaction",
    );
}

/// Scenario C: pre-rope has a volatile span in the *middle* that
/// truncates the cacheable prefix. Compaction absorbs the interior
/// volatile span into a Pinned summary, "healing" the hole and
/// extending the cacheable prefix all the way to the post-rope tail.
///
/// Pre-rope:  [sys(Startup), u1(Hot), u2(Warm), a1(Hot)]
///   pre-boundary  = at(1)  (prefix truncated at u2)
///
/// Mock keeps [sys, u1, a1]; absorbs [u2] → summary at position 2.
/// Post-rope: [sys, u1, summary(Pinned), a1(Hot)]
///   post-boundary = at(3)  (entire post-rope is stable+cacheable)
#[test]
fn absorb_interior_volatile_extends_prefix_past_old_truncation() {
    let pre_rope = RetainedRope::from_spans(vec![
        Span::new("sys", SpanKind::System, "boot", RetentionClass::Startup),
        Span::new("u1", SpanKind::User, "ping", RetentionClass::Hot),
        Span::new("u2", SpanKind::User, "transient", RetentionClass::Warm),
        Span::new("a1", SpanKind::Assistant, "pong", RetentionClass::Hot),
    ]);
    // mu-yqeq.8: system (0) + last-in-prefix (1) before the volatile break at u2.
    assert_eq!(
        boundaries(&pre_rope),
        vec![CacheBoundary::at(0), CacheBoundary::at(1)],
    );

    let policy = MockDropPolicy {
        keep_ids: vec!["sys", "u1", "a1"],
        summary_text: "u2 absorbed",
    };
    let post_rope = policy.compact(&pre_rope, 1_000).rope;

    assert_eq!(post_rope.len(), 4);
    assert!(matches!(
        post_rope.spans()[2].kind(),
        SpanKind::CompactionSummary { .. }
    ));

    let post = boundaries(&post_rope);
    // mu-yqeq.8: system (0) + summary tail (3).
    assert_eq!(
        post,
        vec![CacheBoundary::at(0), CacheBoundary::at(3)],
        "post-compaction prefix should cover the entire post-rope",
    );
    let pre_idx = 1usize;
    let post_idx = post.last().unwrap().message_index;
    assert!(
        post_idx > pre_idx,
        "compaction must STRICTLY extend the last boundary (pre={pre_idx}, post={post_idx})",
    );
}

/// Scenario D: compaction keeps everything (judge keep-all). Post-rope
/// shape is unchanged; boundary is unchanged.
#[test]
fn keep_all_leaves_boundary_unchanged() {
    let pre_rope = RetainedRope::from_spans(vec![
        Span::new(
            "sys",
            SpanKind::System,
            "you are mu",
            RetentionClass::Startup,
        ),
        Span::new("u1", SpanKind::User, "hi", RetentionClass::Hot),
        Span::new("a1", SpanKind::Assistant, "hello", RetentionClass::Hot),
        Span::new("u2", SpanKind::User, "now?", RetentionClass::Warm),
    ]);
    let pre = boundaries(&pre_rope);
    // mu-yqeq.8: system (0) + last-in-prefix (2).
    assert_eq!(pre, vec![CacheBoundary::at(0), CacheBoundary::at(2)]);

    let policy = MockDropPolicy {
        keep_ids: vec!["sys", "u1", "a1", "u2"],
        summary_text: "unused",
    };
    let post_rope = policy.compact(&pre_rope, 1_000).rope;

    assert_eq!(post_rope.spans(), pre_rope.spans(), "rope shape unchanged");
    assert_eq!(boundaries(&post_rope), pre);
}

/// Scenario E: degenerate — compaction absorbs EVERY span. Post-rope is
/// a single Pinned summary span. Boundary lands at index 0 (the
/// summary is the entire cacheable prefix).
#[test]
fn absorb_all_yields_single_summary_with_boundary_at_zero() {
    let pre_rope = RetainedRope::from_spans(vec![
        Span::new("u1", SpanKind::User, "old1", RetentionClass::Warm),
        Span::new("a1", SpanKind::Assistant, "old2", RetentionClass::Warm),
    ]);
    assert!(boundaries(&pre_rope).is_empty());

    let policy = MockDropPolicy {
        keep_ids: vec![],
        summary_text: "everything absorbed",
    };
    let post_rope = policy.compact(&pre_rope, 1_000).rope;

    assert_eq!(post_rope.len(), 1);
    assert!(matches!(
        post_rope.spans()[0].kind(),
        SpanKind::CompactionSummary { .. }
    ));
    assert_eq!(boundaries(&post_rope), vec![CacheBoundary::at(0)]);
}

/// Empty rope is the no-op edge: compaction returns identity; both
/// boundaries are empty.
#[test]
fn empty_rope_compaction_is_identity_under_boundaries() {
    let pre_rope = RetainedRope::new();
    assert!(boundaries(&pre_rope).is_empty());

    let policy = MockDropPolicy {
        keep_ids: vec![],
        summary_text: "",
    };
    let post_rope = policy.compact(&pre_rope, 1_000).rope;
    assert!(post_rope.is_empty());
    assert!(boundaries(&post_rope).is_empty());
}

// ---------------------------------------------------------------------
// Property-style invariant sweep
// ---------------------------------------------------------------------

/// Across all five structural scenarios, the post-boundary message
/// index (when present) must be ≥ the post-rope's last
/// stable+cacheable contiguous prefix index, AND must be ≤ the last
/// "eligible" position (stable+cacheable OR CompactionSummary).
///
/// Catches a regression where the strategy started returning a
/// boundary BEFORE the last kept-stable span (would silently shrink
/// the cached prefix), or pointed PAST the rope length (would silently
/// no-op via the out-of-range tolerance).
#[test]
fn boundary_falls_within_kept_stable_region_for_every_scenario() {
    struct Case {
        name: &'static str,
        rope: RetainedRope,
        keep: Vec<&'static str>,
    }
    let cases = vec![
        Case {
            name: "drop-tail",
            rope: RetainedRope::from_spans(vec![
                Span::new("sys", SpanKind::System, "s", RetentionClass::Startup),
                Span::new("u1", SpanKind::User, "u", RetentionClass::Hot),
                Span::new("u2", SpanKind::User, "v", RetentionClass::Warm),
            ]),
            keep: vec!["sys", "u1"],
        },
        Case {
            name: "drop-prefix",
            rope: RetainedRope::from_spans(vec![
                Span::new("u1", SpanKind::User, "v", RetentionClass::Warm),
                Span::new("a1", SpanKind::Assistant, "s", RetentionClass::Hot),
            ]),
            keep: vec!["a1"],
        },
        Case {
            name: "drop-interior",
            rope: RetainedRope::from_spans(vec![
                Span::new("sys", SpanKind::System, "s", RetentionClass::Startup),
                Span::new("u1", SpanKind::User, "v", RetentionClass::Warm),
                Span::new("a1", SpanKind::Assistant, "s", RetentionClass::Hot),
            ]),
            keep: vec!["sys", "a1"],
        },
        Case {
            name: "keep-all",
            rope: RetainedRope::from_spans(vec![
                Span::new("sys", SpanKind::System, "s", RetentionClass::Startup),
                Span::new("u1", SpanKind::User, "u", RetentionClass::Hot),
            ]),
            keep: vec!["sys", "u1"],
        },
        Case {
            name: "drop-all",
            rope: RetainedRope::from_spans(vec![Span::new(
                "u1",
                SpanKind::User,
                "v",
                RetentionClass::Warm,
            )]),
            keep: vec![],
        },
    ];

    for case in cases {
        let policy = MockDropPolicy {
            keep_ids: case.keep.clone(),
            summary_text: "ignored",
        };
        let post_rope = policy.compact(&case.rope, 1_000).rope;
        let bs = boundaries(&post_rope);

        if post_rope.is_empty() {
            assert!(bs.is_empty(), "{}: empty rope ⇒ no boundary", case.name);
            continue;
        }

        if bs.is_empty() {
            // No cacheable prefix at all is acceptable iff there is
            // no eligible span at position 0.
            let head_eligible = {
                let s = &post_rope.spans()[0];
                let is_summary = matches!(s.kind(), SpanKind::CompactionSummary { .. });
                (s.retention().is_stable() && s.cacheable()) || is_summary
            };
            assert!(
                !head_eligible,
                "{}: post-rope head is eligible but no boundary emitted",
                case.name,
            );
            continue;
        }

        // Floor: last kept-stable contiguous span index (must reach).
        let floor = last_contiguous_stable_index(&post_rope);
        // Ceiling: last eligible span in the post-rope.
        let ceiling = last_eligible_index(&post_rope).expect("non-empty rope, bs non-empty");

        // mu-yqeq.8: the strategy emits up to two boundaries (system
        // + last-in-prefix). EVERY boundary must land within the
        // post-rope range and at-or-below the eligible ceiling; the
        // LAST boundary must reach the floor (cover the prefix).
        for b in &bs {
            assert!(
                b.message_index <= ceiling,
                "{}: boundary {} past last eligible index {}",
                case.name,
                b.message_index,
                ceiling,
            );
            assert!(
                b.message_index < post_rope.len(),
                "{}: boundary {} out of post-rope range (len={})",
                case.name,
                b.message_index,
                post_rope.len(),
            );
        }
        if let Some(f) = floor {
            let last = bs.last().unwrap();
            assert!(
                last.message_index >= f,
                "{}: last boundary {} below last kept-stable index {}",
                case.name,
                last.message_index,
                f,
            );
        }
    }
}

// ---------------------------------------------------------------------
// End-to-end smoke through the real HashAndSummaryPolicy
// ---------------------------------------------------------------------

/// Exercises the real production compaction path
/// (`HashAndSummaryPolicy`) end-to-end with a mock judge, then runs
/// renderer + strategy and asserts the ephemeral cache marker lands
/// on the post-rope's Pinned summary span. Catches regressions where
/// the policy's `surgery` shape (Pinned summary, kept-span order)
/// drifts from what the cache strategy expects.
#[test]
fn real_hash_and_summary_policy_produces_cacheable_summary_span() {
    let pre_rope = RetainedRope::from_spans(vec![
        Span::new(
            "sys",
            SpanKind::System,
            "you are mu",
            RetentionClass::Startup,
        ),
        Span::new("u1", SpanKind::User, "ping", RetentionClass::Hot),
        Span::new("a1", SpanKind::Assistant, "pong", RetentionClass::Hot),
        Span::new(
            "t1",
            SpanKind::ToolResult,
            "{\"ok\":true}",
            RetentionClass::Warm,
        ),
        Span::new(
            "a2",
            SpanKind::Assistant,
            "summary of tool result",
            RetentionClass::Warm,
        ),
    ]);
    let pre = boundaries(&pre_rope);
    // mu-yqeq.8: system (0) + last-in-prefix (2, "a1").
    assert_eq!(
        pre,
        vec![CacheBoundary::at(0), CacheBoundary::at(2)],
        "sanity",
    );

    // Compute hashes the same way the production policy will, so the
    // mock judge can refer to spans by their real hash.
    let hasher = Blake3Hasher;
    let hashes: Vec<String> = pre_rope.spans().iter().map(|s| hasher.hash(s, 8)).collect();
    // Judge will keep sys, u1, a1; absorb t1, a2.
    let response = format!(
        "{{\"keep\":[\"{}\",\"{}\",\"{}\"],\"summary\":\"tool returned ok\"}}",
        hashes[0], hashes[1], hashes[2]
    );

    struct CannedJudge {
        response: String,
    }
    impl Judge for CannedJudge {
        fn judge(&self, _prompt: &str) -> Result<String, JudgeError> {
            Ok(self.response.clone())
        }
    }
    let policy = HashAndSummaryPolicy::new(Arc::new(CannedJudge { response }));
    let post_rope = policy.compact(&pre_rope, 1_000).rope;

    // [sys, u1, a1, summary(Pinned)] — summary at the position of the
    // earliest absorbed span (t1 was index 3).
    assert_eq!(post_rope.len(), 4);
    assert_eq!(post_rope.spans()[0].id(), "sys");
    assert_eq!(post_rope.spans()[1].id(), "u1");
    assert_eq!(post_rope.spans()[2].id(), "a1");
    assert!(matches!(
        post_rope.spans()[3].kind(),
        SpanKind::CompactionSummary { .. }
    ));

    // mu-yqeq.8: boundaries on system (0) AND summary tail (3) —
    // the post-rope is entirely stable+cacheable.
    let post = boundaries(&post_rope);
    assert_eq!(post, vec![CacheBoundary::at(0), CacheBoundary::at(3)]);

    // Render + annotate ⇒ ephemeral marker on system AND summary spans.
    let renderer = AnthropicProviderRenderer::new();
    let strategy = AnthropicCacheStrategy::new();
    let mut rendered = renderer.render(&post_rope, ProjectionTarget::AgentView);
    strategy.annotate(&mut rendered, &post);

    assert_eq!(rendered.messages.len(), 4);
    for (i, m) in rendered.messages.iter().enumerate() {
        let expected = if i == 0 || i == 3 {
            Some(CacheMarker::Ephemeral)
        } else {
            None
        };
        assert_eq!(
            m.cache_marker(),
            expected,
            "message {i}: expected {expected:?}",
        );
    }
}

/// mu-814o: prefix forensics against the REAL Anthropic renderer +
/// strategy pair, simulating the incident's prime suspect — a
/// project-file FileLoad span whose content changes on rehydration
/// while its `project-file:<path>` id stays put. The session-log diff
/// must (a) flag the invalidation via `prefix_hash`, (b) name the
/// mutated span via its `"<id>=<hash>"` entry, and (c) leave every
/// other span hash untouched.
#[test]
fn prefix_forensics_names_a_rehydration_mutation() {
    let rope_with = |claude_md: &str| {
        RetainedRope::from_spans(vec![
            Span::new(
                "system-prompt",
                SpanKind::System,
                "you are mu",
                RetentionClass::Startup,
            ),
            Span::new(
                "project-file:/repo/CLAUDE.md",
                SpanKind::FileLoad,
                claude_md,
                RetentionClass::Startup,
            ),
            Span::new(
                "memory-recall:abc123",
                SpanKind::MemoryInjection,
                "remembered fact",
                RetentionClass::Startup,
            ),
            Span::new("u1", SpanKind::User, "hi", RetentionClass::Hot),
        ])
    };

    let forensics = |rope: &RetainedRope| {
        let renderer = AnthropicProviderRenderer::new();
        let strategy = AnthropicCacheStrategy::new();
        let bounds = strategy.boundaries(rope);
        assert!(
            !bounds.is_empty(),
            "sanity: the stable prefix must produce boundaries"
        );
        let mut rendered = renderer.render(rope, ProjectionTarget::AgentView);
        strategy.annotate(&mut rendered, &bounds);
        prefix_forensics(&rendered, &bounds, rope)
    };

    let (h_before, s_before) = forensics(&rope_with("# rules v1"));
    // Same content → bit-identical forensics across calls.
    let (h_same, s_same) = forensics(&rope_with("# rules v1"));
    assert_eq!(h_before, h_same);
    assert_eq!(s_before, s_same);

    // The rehydration mutation: same id, new bytes.
    let (h_after, s_after) = forensics(&rope_with("# rules v2 (file changed on disk)"));

    assert!(h_before.is_some());
    assert_ne!(h_before, h_after, "prefix_hash must flag the invalidation");
    assert_eq!(s_before.len(), s_after.len(), "boundary did not move");
    for (b, a) in s_before.iter().zip(s_after.iter()) {
        if b.starts_with("project-file:/repo/CLAUDE.md=") {
            assert_ne!(b, a, "mutated FileLoad span must be named");
        } else {
            assert_eq!(b, a, "untouched span hashes must be stable");
        }
    }
}
