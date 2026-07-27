//! Implicit capability discovery — mu-uz0n layer 1, reworked by mu-0x5i.
//!
//! Discovery loses the tool-choice auction when it's opt-in: the model
//! already believes it knows its tools, grep/bash have massive training
//! priors, `discover` has none, and the cost of ignorance is invisible
//! (observed live: sessions working ON t4c never called discover,
//! session 7517502faa5f7ed2). Always-AVAILABLE is not always-USED.
//!
//! So the daemon stops waiting to be asked: each turn, the user's
//! message (or the autonomous iteration motivation — whatever the last
//! user-role message is) is run through the same ranking the `discover`
//! tool uses, and the qualifying hits are INJECTED as a compact hint
//! span — the same push-not-pull posture as session-start recall.
//!
//! ## What mu-0x5i changed, and why
//!
//! Shipped-on, the feature was judged not worth using. Three causes:
//!
//! 1. **Everything above zero counted.** The cut was `score > 0.0`, but
//!    ranker scores are UNNORMALIZED (`tool.read` measured at `2.0`), so
//!    "top 3 above zero" routinely meant three barely-related entries.
//!    The cut is now a fraction of the turn's BEST hit
//!    ([`RankOptions::min_score_ratio`]) — scale-free, so it survives the
//!    lexical→semantic switch that an absolute floor would not.
//!
//! 2. **The memo was on the wrong axis.** It keyed on intent text and
//!    only skipped re-RANKING within one ask. The span itself was
//!    transient, so identical capabilities were re-injected every turn
//!    while the previous turn's hint vanished — repeat noise AND cache
//!    churn at every turn boundary. Now a hint is ANCHORED to the user
//!    message it accompanied ([`InjectedHint`]) and persists in the rope
//!    at that position, and a capability already present in context is
//!    never re-injected ([`RankOptions::already`]).
//!
//! 3. **Half the feature ignored its own flag.** [`suggest_for_unknown_tool`]
//!    (layer 2) ran unconditionally; `[index].discover_injection = false`
//!    never silenced it. Both halves are gated on the flag now.
//!
//! ## Cache discipline
//!
//! Anchoring is what makes this cacheable. A hint sits immediately after
//! the `msg-{idx}-user` span it was ranked for and never moves, so once
//! written the prefix through it is byte-stable across every later turn —
//! strictly better than the old transient span, which mutated the tail of
//! the prefix on each ask. New turns append; they don't rewrite.
//!
//! When compaction drops the anchoring user span, the hint goes with it
//! (nothing to anchor to) and its capabilities become eligible again —
//! which is exactly right: "already in context" stopped being true.
//!
//! ## Injection sizing (the 21k-wall gate)
//!
//! The hint is machine-view compact — one line per capability, path +
//! truncated summary — and hard-capped at [`HINT_MAX_BYTES`]. Top-3
//! default is ~300-400 bytes ≈ ~100 tokens. Compare the measured 15.9K
//! token full-memory wall (config.rs, session c76f6949) this repo
//! already walked back once. Dedup bounds the accumulation: each
//! capability is named at most once per compaction epoch.

use std::collections::HashSet;
use std::sync::Arc;

use crate::agent::Tool;
use crate::capability::Capability;
use crate::context::rope::{RetainedRope, RetentionClass, Span, SpanKind};
use crate::skill::loader::LoadedSkill;
use crate::t4c_source::{self, CapabilityView};

/// Prefix for injected hint span ids; the anchoring message index is
/// appended (see [`hint_span_id`]). One span per anchoring user message,
/// so `ContextAssembly::prefix_span_hashes` can name any of them if it
/// ever needs diagnosing.
pub const HINT_SPAN_PREFIX: &str = "capability-hint";

/// Hard byte ceiling on a single rendered hint. The formatter drops
/// entries rather than exceed it — injection must never become the wall
/// it exists to replace.
pub const HINT_MAX_BYTES: usize = 700;

/// Per-entry summary truncation (chars).
const SUMMARY_MAX_CHARS: usize = 90;

/// The tunables, in a cell the daemon and the running agent loop SHARE
/// (mu-0x5i). `session.set_config` writes here; the loop reads at the
/// top of each turn, so `/config index.discover_injection=true` takes
/// effect on the next turn with no restart.
///
/// Atomics rather than a lock because the loop reads these on its hot
/// path and a config write must never be able to block a turn.
///
/// Enabling mid-session starts injecting from the NEXT turn; disabling
/// stops adding hints but deliberately LEAVES the ones already in
/// context. Retracting them would rewrite the prefix and throw away the
/// cache — a strictly worse trade than a few stale pointer lines.
#[derive(Debug)]
pub struct LiveHintConfig {
    enabled: std::sync::atomic::AtomicBool,
    limit: std::sync::atomic::AtomicUsize,
    /// `f64` bits — `AtomicF64` doesn't exist.
    min_score_ratio_bits: std::sync::atomic::AtomicU64,
    semantic: std::sync::atomic::AtomicBool,
}

impl LiveHintConfig {
    pub fn new(enabled: bool, limit: usize, min_score_ratio: f64, semantic: bool) -> Self {
        use std::sync::atomic::*;
        Self {
            enabled: AtomicBool::new(enabled),
            limit: AtomicUsize::new(limit),
            min_score_ratio_bits: AtomicU64::new(min_score_ratio.to_bits()),
            semantic: AtomicBool::new(semantic),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn set_enabled(&self, v: bool) {
        self.enabled.store(v, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn limit(&self) -> usize {
        self.limit.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn set_limit(&self, v: usize) {
        self.limit.store(v, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn min_score_ratio(&self) -> f64 {
        f64::from_bits(
            self.min_score_ratio_bits
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }
    pub fn set_min_score_ratio(&self, v: f64) {
        self.min_score_ratio_bits
            .store(v.to_bits(), std::sync::atomic::Ordering::Relaxed);
    }
    pub fn semantic(&self) -> bool {
        self.semantic.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn set_semantic(&self, v: bool) {
        self.semantic.store(v, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Default for LiveHintConfig {
    fn default() -> Self {
        Self::new(false, 3, 0.5, false)
    }
}

/// Per-session wiring for implicit discovery, carried on
/// `AgentConfig::discover_hints`. `None` there ⇒ the feature is not
/// wired at all, which is what `--bare` forces and what every default
/// `AgentConfig` (tests, embedders) gets.
///
/// `Some` does NOT mean "on": the daemon wires `Some` for any non-bare
/// session and puts the on/off decision in [`LiveHintConfig::enabled`],
/// seeded from `[index].discover_injection`. That split is what lets an
/// operator flip the feature on mid-session — the skills the ranker
/// needs are already aboard.
#[derive(Clone)]
pub struct DiscoverHints {
    /// Daemon-discovered skills — same set the `discover` tool ranks.
    /// The agent loop doesn't otherwise hold skills, so they ride in.
    pub skills: Arc<Vec<LoadedSkill>>,
    /// Live tunables, shared with the daemon's `session.set_config`.
    pub live: Arc<LiveHintConfig>,
}

impl DiscoverHints {
    /// Wire with a fixed configuration — the convenient path for tests
    /// and any caller that doesn't need live updates.
    pub fn fixed(
        skills: Arc<Vec<LoadedSkill>>,
        enabled: bool,
        limit: usize,
        min_score_ratio: f64,
        semantic: bool,
    ) -> Self {
        Self {
            skills,
            live: Arc::new(LiveHintConfig::new(
                enabled,
                limit,
                min_score_ratio,
                semantic,
            )),
        }
    }
}

impl std::fmt::Debug for DiscoverHints {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoverHints")
            .field("skills", &self.skills.len())
            .field("live", &self.live)
            .finish()
    }
}

/// One hint that has been injected into this session's context, anchored
/// to the user message it was ranked for.
///
/// `text: None` records a turn that ranked to NOTHING (everything was
/// below the floor, or already in context). Keeping the negative result
/// is what stops the loop re-ranking — and, when semantic is on,
/// re-embedding — on every tool round of the same ask.
#[derive(Clone, Debug, PartialEq)]
pub struct InjectedHint {
    /// Index into the agent's `messages` of the anchoring user message.
    /// Matches the `msg-{idx}-user` span id the rope assembler emits.
    pub anchor_msg_idx: usize,
    /// Rendered hint, or `None` for "this turn had nothing new to say".
    pub text: Option<String>,
    /// Capability paths named by this hint — the dedup key set.
    pub paths: Vec<String>,
}

/// Span id of the user message a hint anchors to. Mirrors
/// [`crate::context::assembly`]'s `msg-{idx}-user` scheme; the two must
/// agree or hints silently stop being placed (pinned by
/// `hint_anchors_match_assembled_rope_ids`).
pub fn anchor_span_id(msg_idx: usize) -> String {
    format!("msg-{msg_idx}-user")
}

/// Span id of the injected hint for the message at `msg_idx`.
pub fn hint_span_id(msg_idx: usize) -> String {
    format!("{HINT_SPAN_PREFIX}-{msg_idx}")
}

/// Whether the rope still contains the user span a hint anchors to.
/// `false` ⇒ compaction dropped it, so the hint is no longer in context.
pub fn rope_has_anchor(rope: &RetainedRope, msg_idx: usize) -> bool {
    let anchor = anchor_span_id(msg_idx);
    rope.spans().iter().any(|s| s.id.as_ref() == anchor)
}

/// Knobs for one ranking pass.
#[derive(Debug)]
pub struct RankOptions<'a> {
    /// Max entries in the rendered hint.
    pub limit: usize,
    /// Relevance floor as a FRACTION of the best-scoring hit this turn
    /// (`0.0..=1.0`, clamped). `0.5` ⇒ keep only entries scoring at
    /// least half the top hit; `0.0` ⇒ the old "anything above zero".
    ///
    /// Relative rather than absolute because ranker scores are
    /// unnormalized and the lexical and semantic rankers do not share a
    /// scale — an absolute floor would mean something different after
    /// either changed.
    pub min_score_ratio: f64,
    /// Capability paths already present in this session's context. Never
    /// re-injected; see the module doc.
    pub already: &'a HashSet<String>,
    /// Rank semantically (local embedder only) rather than lexically.
    pub semantic: bool,
}

/// Rank `intent` against the session's capability surface (same
/// manifest the `discover` tool builds: tools + skills + host catalog,
/// permission-attenuated) and render the compact hint, with the
/// qualifying capability paths.
///
/// `None` when the intent is empty, the manifest can't be built, or
/// nothing clears the floor / everything is already in context — no
/// match means no injection, never noise.
///
/// **May block** when `opts.semantic` is set (synchronous HTTP embed);
/// async callers must wrap it in `spawn_blocking`.
pub fn rank_hint(
    tools: &[Arc<dyn Tool>],
    capability: &Capability,
    skills: &[LoadedSkill],
    intent: &str,
    opts: &RankOptions<'_>,
) -> Option<(String, Vec<String>)> {
    if intent.trim().is_empty() {
        return None;
    }
    let registry = t4c_source::build_manifest_for_tools(tools, capability, skills);
    // Manifest-build failure ⇒ no hint, never an error: the injection
    // is best-effort sugar on the turn, not load-bearing.
    let tree = registry.build().ok()?;
    // Rank deeper than `limit`: dedup and the score floor both remove
    // candidates, and asking for exactly `limit` would leave the hint
    // short whenever the top hits are already in context.
    let depth = opts.limit.max(1).saturating_mul(4).max(8);
    let views = if opts.semantic {
        // Local-only, and lexical is a correct answer — never fail the
        // turn because an embedder was unreachable.
        t4c_source::discover_view_semantic_local(&tree, intent, depth).unwrap_or_else(|e| {
            tracing::debug!(
                error = %e,
                "capability hints: semantic rank unavailable, using lexical"
            );
            t4c_source::discover_view(&tree, intent, depth)
        })
    } else {
        t4c_source::discover_view(&tree, intent, depth)
    };
    format_hint(&views, opts)
}

/// Render ranked views as the compact machine-view hint, returning the
/// text and the capability paths it names. Public for tests; production
/// goes through [`rank_hint`].
pub fn format_hint(
    views: &[CapabilityView],
    opts: &RankOptions<'_>,
) -> Option<(String, Vec<String>)> {
    // The floor is a fraction of the best ALLOWED hit of this turn,
    // computed BEFORE dedup: if the top hit is already in context, the
    // remaining entries still have to clear the same bar. Otherwise
    // dropping the leader would promote whatever weak entry came next.
    let top = views
        .iter()
        .filter(|v| v.allowed_by_session && v.score > 0.0)
        .map(|v| v.score)
        .fold(0.0_f64, f64::max);
    if top <= 0.0 {
        return None;
    }
    let floor = top * opts.min_score_ratio.clamp(0.0, 1.0);

    let mut out = String::from(
        "[capability hints — auto-ranked against this turn; \
         call `discover` with your intent for the full list]",
    );
    let mut paths = Vec::new();
    for v in views
        .iter()
        .filter(|v| v.allowed_by_session && v.score > 0.0 && v.score >= floor)
        .filter(|v| !opts.already.contains(&v.path))
        .take(opts.limit)
    {
        let summary: String = v.summary.chars().take(SUMMARY_MAX_CHARS).collect();
        let line = format!("\n• {} — {}", v.path, summary.trim());
        if out.len() + line.len() > HINT_MAX_BYTES {
            break;
        }
        out.push_str(&line);
        paths.push(v.path.clone());
    }
    (!paths.is_empty()).then_some((out, paths))
}

/// Return a rope with each hint span inserted immediately after the user
/// span it anchors to. Hints whose anchor is absent (compacted away) are
/// skipped; the caller prunes them via [`rope_has_anchor`].
pub fn with_hints(rope: &RetainedRope, hints: &[InjectedHint]) -> RetainedRope {
    if hints.iter().all(|h| h.text.is_none()) {
        return rope.clone();
    }
    let spans = rope.spans();
    let mut out: Vec<Span> = Vec::with_capacity(spans.len() + hints.len());
    for span in spans {
        let anchored = if span.kind == SpanKind::User {
            hints
                .iter()
                .find(|h| h.text.is_some() && anchor_span_id(h.anchor_msg_idx) == *span.id)
        } else {
            None
        };
        out.push(span.clone());
        if let Some(h) = anchored {
            out.push(Span::new(
                hint_span_id(h.anchor_msg_idx),
                SpanKind::User,
                h.text.as_deref().unwrap_or_default(),
                RetentionClass::Hot,
            ));
        }
    }
    RetainedRope::from_spans(out)
}

/// mu-uz0n layer 2 — the error-path hook. When the model invents a
/// nonexistent tool name, the moment of failure is the one moment it
/// is receptive: rank the bad name against the real surface and name
/// the near-misses. Returns `None` when nothing scores (the bare
/// "tool not found" stands alone).
///
/// mu-0x5i: the CALLER gates this on `[index].discover_injection`. It
/// used to run unconditionally, which is why turning the feature off
/// appeared not to work.
pub fn suggest_for_unknown_tool(tools: &[Arc<dyn Tool>], name: &str) -> Option<String> {
    let tree = t4c_source::build_manifest(tools, &[]).build().ok()?;
    let views = t4c_source::discover_view(&tree, name, 3);
    let paths: Vec<&str> = views
        .iter()
        .filter(|v| v.score > 0.0)
        .take(3)
        .map(|v| v.path.as_str())
        .collect();
    (!paths.is_empty()).then(|| paths.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentMessage;

    fn view(path: &str, summary: &str, score: f64) -> CapabilityView {
        CapabilityView {
            path: path.to_string(),
            summary: summary.to_string(),
            keywords: Vec::new(),
            score,
            effects: None,
            allowed_by_session: true,
            disallowed_reason: None,
            source: None,
        }
    }

    fn opts(limit: usize, ratio: f64, already: &HashSet<String>) -> RankOptions<'_> {
        RankOptions {
            limit,
            min_score_ratio: ratio,
            already,
            semantic: false,
        }
    }

    fn none() -> HashSet<String> {
        HashSet::new()
    }

    #[test]
    fn format_hint_is_compact_and_limited() {
        let seen = none();
        let views = vec![
            view("tool.read", "Read file contents", 0.9),
            view("skill.code-index", "semantic + lexical code recall", 0.85),
            view("bash.rg", "ripgrep", 0.8),
            view("tool.write", "should be cut by limit", 0.75),
        ];
        let (hint, paths) = format_hint(&views, &opts(3, 0.5, &seen)).expect("hint");
        assert!(hint.starts_with("[capability hints"));
        assert!(hint.contains("• tool.read — Read file contents"));
        assert!(hint.contains("• skill.code-index"));
        assert!(hint.contains("• bash.rg"));
        assert!(!hint.contains("tool.write"), "limit must cap entries");
        assert_eq!(paths, vec!["tool.read", "skill.code-index", "bash.rg"]);
        assert!(hint.len() <= HINT_MAX_BYTES, "hint exceeded byte cap");
    }

    #[test]
    fn format_hint_skips_zero_scores_and_disallowed() {
        let seen = none();
        let mut blocked = view("tool.spawn", "spawn workers", 0.9);
        blocked.allowed_by_session = false;
        let views = vec![blocked, view("tool.noise", "no match", 0.0)];
        assert_eq!(
            format_hint(&views, &opts(3, 0.5, &seen)),
            None,
            "no qualifying entries ⇒ no hint"
        );
    }

    #[test]
    fn min_score_ratio_cuts_relative_to_the_turns_best_hit() {
        // The shipped bug: `score > 0.0` admitted anything. Scores are
        // unnormalized (2.0 here), so the cut has to be relative.
        let seen = none();
        let views = vec![
            view("tool.read", "the actual answer", 2.0),
            view("tool.middling", "half as good", 1.0),
            view("bash.tangential", "barely related", 0.2),
        ];
        let (hint, paths) = format_hint(&views, &opts(5, 0.5, &seen)).expect("hint");
        assert!(hint.contains("tool.read"));
        assert!(hint.contains("tool.middling"), "exactly at the floor stays");
        assert!(
            !hint.contains("bash.tangential"),
            "0.2 is below 50% of 2.0 and must be cut"
        );
        assert_eq!(paths, vec!["tool.read", "tool.middling"]);
    }

    #[test]
    fn min_score_ratio_zero_restores_above_zero_behavior() {
        let seen = none();
        let views = vec![
            view("tool.read", "top", 2.0),
            view("bash.tangential", "barely related", 0.2),
        ];
        let (_, paths) = format_hint(&views, &opts(5, 0.0, &seen)).expect("hint");
        assert_eq!(paths, vec!["tool.read", "bash.tangential"]);
    }

    #[test]
    fn ratio_is_computed_before_dedup_so_leftovers_still_clear_the_bar() {
        // tool.read is already in context. tool.weak must NOT be promoted
        // into the hint just because the leader was filtered out.
        let seen: HashSet<String> = ["tool.read".to_string()].into_iter().collect();
        let views = vec![
            view("tool.read", "top hit, already injected", 2.0),
            view("tool.weak", "well below half of 2.0", 0.3),
        ];
        assert_eq!(
            format_hint(&views, &opts(5, 0.5, &seen)),
            None,
            "dedup must not lower the bar for what remains"
        );
    }

    #[test]
    fn already_injected_capabilities_are_not_repeated() {
        let seen: HashSet<String> = ["tool.read".to_string()].into_iter().collect();
        let views = vec![
            view("tool.read", "already in context", 2.0),
            view("skill.code-index", "still relevant and new", 1.8),
        ];
        let (hint, paths) = format_hint(&views, &opts(3, 0.5, &seen)).expect("hint");
        assert!(!hint.contains("tool.read"), "must not repeat");
        assert_eq!(paths, vec!["skill.code-index"]);
    }

    #[test]
    fn format_hint_enforces_byte_cap() {
        let seen = none();
        let views: Vec<CapabilityView> = (0..50)
            .map(|i| view(&format!("tool.t{i}"), &"x".repeat(SUMMARY_MAX_CHARS), 1.0))
            .collect();
        let (hint, _) = format_hint(&views, &opts(50, 0.5, &seen)).expect("hint");
        assert!(
            hint.len() <= HINT_MAX_BYTES,
            "byte cap must hold at any limit"
        );
    }

    fn rope() -> RetainedRope {
        RetainedRope::from_spans(vec![
            Span::new(
                "sys",
                SpanKind::System,
                "you are mu",
                RetentionClass::Startup,
            ),
            Span::new("msg-0-user", SpanKind::User, "first", RetentionClass::Hot),
            Span::new(
                "msg-1-assistant",
                SpanKind::Assistant,
                "reply",
                RetentionClass::Hot,
            ),
            Span::new("msg-2-user", SpanKind::User, "second", RetentionClass::Hot),
            Span::new(
                "msg-3-assistant",
                SpanKind::Assistant,
                "working",
                RetentionClass::Hot,
            ),
        ])
    }

    #[test]
    fn hints_land_after_the_user_span_they_anchor_to() {
        let hints = vec![
            InjectedHint {
                anchor_msg_idx: 0,
                text: Some("[capability hints] • tool.read".to_string()),
                paths: vec!["tool.read".to_string()],
            },
            InjectedHint {
                anchor_msg_idx: 2,
                text: Some("[capability hints] • skill.code-index".to_string()),
                paths: vec!["skill.code-index".to_string()],
            },
        ];
        let out = with_hints(&rope(), &hints);
        let ids: Vec<&str> = out.spans().iter().map(|s| s.id.as_ref()).collect();
        assert_eq!(
            ids,
            vec![
                "sys",
                "msg-0-user",
                "capability-hint-0",
                "msg-1-assistant",
                "msg-2-user",
                "capability-hint-2",
                "msg-3-assistant",
            ],
            "each hint sits immediately after its own anchor"
        );
    }

    #[test]
    fn earlier_hints_are_byte_stable_when_a_later_one_is_added() {
        // The cache property: adding turn N's hint must not disturb any
        // span before it. This is what the old transient span broke.
        let first = vec![InjectedHint {
            anchor_msg_idx: 0,
            text: Some("[capability hints] • tool.read".to_string()),
            paths: vec!["tool.read".to_string()],
        }];
        let mut second = first.clone();
        second.push(InjectedHint {
            anchor_msg_idx: 2,
            text: Some("[capability hints] • skill.code-index".to_string()),
            paths: vec!["skill.code-index".to_string()],
        });
        let before = with_hints(&rope(), &first);
        let after = with_hints(&rope(), &second);
        let prefix_len = before
            .spans()
            .iter()
            .position(|s| s.id.as_ref() == "msg-2-user")
            .expect("anchor present")
            + 1;
        assert_eq!(
            before.spans()[..prefix_len],
            after.spans()[..prefix_len],
            "prefix through the new anchor must be untouched"
        );
    }

    #[test]
    fn hint_with_missing_anchor_is_not_placed() {
        // Compaction dropped the anchoring user span; the hint has
        // nowhere to go, and its capabilities become eligible again.
        let hints = vec![InjectedHint {
            anchor_msg_idx: 99,
            text: Some("orphan".to_string()),
            paths: vec!["tool.read".to_string()],
        }];
        let out = with_hints(&rope(), &hints);
        assert_eq!(out.spans(), rope().spans());
        assert!(!rope_has_anchor(&rope(), 99));
        assert!(rope_has_anchor(&rope(), 2));
    }

    #[test]
    fn negative_result_injects_nothing() {
        let hints = vec![InjectedHint {
            anchor_msg_idx: 0,
            text: None,
            paths: Vec::new(),
        }];
        assert_eq!(with_hints(&rope(), &hints).spans(), rope().spans());
    }

    #[test]
    fn hint_anchors_match_assembled_rope_ids() {
        // Pins the coupling to `context::assembly`'s `msg-{idx}-user`
        // scheme. If assembly renames its spans, hints would silently
        // stop being placed rather than fail loudly — so assert it here.
        let messages = vec![
            AgentMessage::User {
                content: "first".into(),
            },
            AgentMessage::User {
                content: "second".into(),
            },
        ];
        let assembled = crate::context::assemble_rope(None, &messages, &[]);
        for idx in 0..messages.len() {
            assert!(
                rope_has_anchor(&assembled, idx),
                "assembly must emit {} for message {idx}",
                anchor_span_id(idx)
            );
        }
    }
}
