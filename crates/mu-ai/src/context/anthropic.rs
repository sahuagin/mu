//! Anthropic-shaped `ProviderRenderer` + `CacheStrategy` — first
//! production impls of the foundational traits landed in mu-ktq.
//!
//! Per `specs/architecture/event-sourced-context.md` lines 564-612.
//!
//! ## Renderer
//!
//! [`AnthropicProviderRenderer`] honors
//! [`ProjectionTarget::AgentView`] with verbatim span content and the
//! span-kind → role mapping inherited from
//! [`mu_core::context::ProviderRole`]. The [`OperatorView`] is not
//! the focus of this bead (mu-bn4); a follow-on can layer per-kind
//! summarization on top without re-shaping the trait surface.
//!
//! Output type is [`ProviderMessages`] (the neutral, provider-shared
//! shape). The Anthropic-specific wire JSON
//! (`{system: [...], messages: [...]}`) is produced by a downstream
//! adapter step — out of scope for this bead. System-kind spans
//! render as messages with [`ProviderRole::System`]; the future wire
//! adapter pulls them into Anthropic's top-level `system` field.
//!
//! ## Cache strategy
//!
//! [`AnthropicCacheStrategy`] places up to FOUR
//! [`CacheMarker::Ephemeral`] boundaries (the Anthropic per-request
//! cap) — mu-chiw extended the original two-marker intro-prefix rule
//! with two conversation anchors:
//!
//! 1. The first [`SpanKind::System`] span in the consecutive
//!    stable+cacheable prefix, when present.
//! 2. The last span in the consecutive stable+cacheable prefix,
//!    when it differs from (1).
//! 3. The last cacheable span BEFORE the latest User span — the
//!    cross-call anchor: the previous turn's history re-matches here
//!    even though the new user message extends the suffix.
//! 4. The last cacheable span in the rope — the within-turn anchor:
//!    in an agent loop, call N+1 = call N + one tool result, so
//!    marking the rope end lets each loop iteration re-read the
//!    turn-so-far at cache-read rates.
//!
//! (1)/(2) keep the original semantics: the intro prefix stops at
//! the first span failing the stable or cacheable predicate (spec
//! lines 567-578; `specs/mu-044-provider-messages-cutover.md`
//! §"Cache-annotation consolidation (Phase D)"). (3)/(4) gate on
//! `cacheable()` ONLY — stability is not required for reuse, because
//! Anthropic caching matches content-identical prefixes: a volatile
//! span that later compacts away merely misses the cache, which is
//! the pre-chiw status quo for every conversation span anyway.
//!
//! Why this matters (measured, 2026-06-04 replay benchmark): with
//! only the intro markers, the entire conversation re-billed at full
//! input rate every call — 607,865 uncached tokens (~45% of session
//! cost) vs claude-code's 110 on the same workload (bead mu-chiw).
//!
//! Phase D (mu-yqeq.8) made this strategy the SOLE source of
//! Anthropic cache_control emission. The previously-unconditional
//! annotation in
//! [`crate::providers::anthropic::build_request_body`] has been
//! retired; the Projected wire emitter reads each
//! [`ProviderMessage::cache_marker`] and propagates to the wire
//! position (system block or last tool spec) based on the source
//! span id discrimination.
//!
//! [`OperatorView`]: ProjectionTarget::OperatorView
//! [`RetentionClass::is_stable`]: mu_core::context::RetentionClass::is_stable
//! [`Span::cacheable`]: mu_core::context::Span

use std::sync::Arc;

use mu_core::context::{
    CacheBoundary, CacheMarker, CacheStrategy, ProjectionTarget, ProviderMessage, ProviderMessages,
    ProviderRenderer, RetainedRope, SpanKind,
};

/// Anthropic-shaped provider renderer (mu-bn4).
///
/// AgentView: one [`ProviderMessage`] per span, role from
/// [`SpanKind`](mu_core::context::SpanKind), content verbatim.
/// OperatorView falls through to the same verbatim shape — operator-
/// side differentiation is a follow-on (see bead body).
///
/// Unit struct: no per-impl configuration today. The trait receiver
/// is reserved for future policy fields (max-message coalescing,
/// content-block splitting, etc.).
#[derive(Debug, Default, Clone, Copy)]
pub struct AnthropicProviderRenderer;

impl AnthropicProviderRenderer {
    pub fn new() -> Self {
        Self
    }
}

impl ProviderRenderer for AnthropicProviderRenderer {
    fn render(&self, rope: &RetainedRope, target: ProjectionTarget) -> ProviderMessages {
        let messages = rope
            .iter()
            .map(|span| {
                let msg = ProviderMessage::new(
                    span.kind().into(),
                    span.content(),
                    vec![Arc::from(span.id())],
                );
                match span.blocks() {
                    Some(blocks) => msg.with_blocks(blocks.to_vec()),
                    None => msg,
                }
            })
            .collect();

        ProviderMessages { messages, target }
    }
}

/// Anthropic ephemeral-cache strategy (mu-bn4; extended mu-chiw).
///
/// Places up to four [`CacheBoundary`]s: two in the consecutive
/// stable+cacheable intro prefix (first [`SpanKind::System`] span +
/// last span of the prefix — the original mu-bn4 rule, spec lines
/// 567-578), and two conversation anchors (last cacheable span
/// before the latest User span; last cacheable span in the rope).
/// See the module docs for the full rationale and the measured cost
/// of the two-marker shape this replaces.
#[derive(Debug, Default, Clone, Copy)]
pub struct AnthropicCacheStrategy;

impl AnthropicCacheStrategy {
    pub fn new() -> Self {
        Self
    }
}

impl CacheStrategy for AnthropicCacheStrategy {
    fn boundaries(&self, rope: &RetainedRope) -> Vec<CacheBoundary> {
        // Intro prefix: track the first System-kind index and the
        // last index within the consecutive stable+cacheable prefix.
        // mu-chiw: also stop at the first CONVERSATION-kind span —
        // pre-chiw, conversation spans were uncacheable so the prefix
        // ended there implicitly; now that they're cacheable the stop
        // must be explicit, or this boundary drifts into the
        // conversation and stops protecting system+tools from
        // conversation-driven invalidation.
        let mut system_idx: Option<usize> = None;
        let mut last_in_prefix: Option<usize> = None;
        for (idx, span) in rope.iter().enumerate() {
            let is_conversation = matches!(
                span.kind(),
                SpanKind::User | SpanKind::Assistant | SpanKind::ToolResult
            );
            if !is_conversation && span.retention().is_stable() && span.cacheable() {
                if matches!(span.kind(), SpanKind::System) && system_idx.is_none() {
                    system_idx = Some(idx);
                }
                last_in_prefix = Some(idx);
            } else {
                break;
            }
        }

        // Conversation anchors (mu-chiw): placed within the
        // CONTIGUOUS cacheable run from index 0 — stability is not
        // required (see module docs), but an anchor past an
        // uncacheable hole would write cache prefixes that can never
        // re-match (the hole's content varies call-to-call), so the
        // run ends at the first uncacheable span.
        let mut run_end: Option<usize> = None;
        for (idx, span) in rope.iter().enumerate() {
            if !span.cacheable() {
                break;
            }
            run_end = Some(idx);
        }
        // Latest User span within the run.
        let last_user_idx: Option<usize> = run_end.and_then(|end| {
            rope.iter()
                .enumerate()
                .take(end + 1)
                .rev()
                .find(|(_, s)| matches!(s.kind(), SpanKind::User))
                .map(|(i, _)| i)
        });
        // Cross-call anchor: span just before the latest User span
        // (= everything up to the previous turn re-matches here even
        // though the new user message extends the suffix).
        let pre_turn: Option<usize> =
            last_user_idx.and_then(|u| if u > 0 { Some(u - 1) } else { None });

        // Dedup while preserving ≤4 by construction (2 intro + 2
        // conversation candidates). Within-turn anchor = run end.
        let mut idxs: Vec<usize> = Vec::new();
        for cand in [system_idx, last_in_prefix, pre_turn, run_end]
            .into_iter()
            .flatten()
        {
            if !idxs.contains(&cand) {
                idxs.push(cand);
            }
        }
        idxs.sort_unstable();
        idxs.into_iter().map(CacheBoundary::at).collect()
    }

    fn annotate(&self, messages: &mut ProviderMessages, boundaries: &[CacheBoundary]) {
        for boundary in boundaries {
            if let Some(message) = messages.messages.get_mut(boundary.message_index) {
                message.set_cache_marker(Some(CacheMarker::Ephemeral));
            }
            // Out-of-range indices are tolerated silently per trait
            // contract (cache.rs:100-107) — boundaries computed from
            // an older rope shape must not panic a re-render.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_core::context::{ProviderRole, RetentionClass, Span, SpanKind};

    /// Rope with a clean stable/volatile split: 3 stable Startup/Hot
    /// spans, then 2 Warm (volatile) spans.
    fn split_rope() -> RetainedRope {
        RetainedRope::from_spans(vec![
            Span::new(
                "sys",
                SpanKind::System,
                "you are mu",
                RetentionClass::Startup,
            ),
            Span::new("u1", SpanKind::User, "hi", RetentionClass::Hot),
            Span::new("a1", SpanKind::Assistant, "hello", RetentionClass::Hot),
            Span::new("u2", SpanKind::User, "what time", RetentionClass::Warm),
            Span::new("a2", SpanKind::Assistant, "noon", RetentionClass::Warm),
        ])
    }

    // ===== Renderer tests =====

    #[test]
    fn anthropic_renderer_emits_one_message_per_span() {
        let rope = split_rope();
        let rendered = AnthropicProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
        assert_eq!(rendered.len(), rope.len());
        assert_eq!(rendered.target, ProjectionTarget::AgentView);
    }

    #[test]
    fn anthropic_renderer_maps_span_kind_to_role() {
        let rope = split_rope();
        let rendered = AnthropicProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
        let roles: Vec<ProviderRole> = rendered.messages.iter().map(|m| m.role()).collect();
        assert_eq!(
            roles,
            vec![
                ProviderRole::System,
                ProviderRole::User,
                ProviderRole::Assistant,
                ProviderRole::User,
                ProviderRole::Assistant,
            ]
        );
    }

    #[test]
    fn anthropic_renderer_agent_view_is_verbatim() {
        let rope = split_rope();
        let rendered = AnthropicProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
        for (msg, span) in rendered.messages.iter().zip(rope.spans()) {
            assert_eq!(msg.content(), span.content());
            let ids: Vec<&str> = msg.source_span_ids().iter().map(AsRef::as_ref).collect();
            assert_eq!(ids, vec![span.id()]);
            assert!(
                msg.cache_marker().is_none(),
                "renderer leaves cache_marker None — strategy fills it"
            );
        }
    }

    #[test]
    fn anthropic_renderer_handles_system_kind_as_system_role() {
        // mu-n48: System-kind spans render with role=System; the
        // downstream wire adapter (not this bead) hoists them into
        // Anthropic's top-level `system` field.
        let rope = RetainedRope::from_spans(vec![Span::new(
            "sys-only",
            SpanKind::System,
            "system instruction",
            RetentionClass::Startup,
        )]);
        let rendered = AnthropicProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
        assert_eq!(rendered.messages.len(), 1);
        assert_eq!(rendered.messages[0].role(), ProviderRole::System);
        assert_eq!(rendered.messages[0].content(), "system instruction");
    }

    // ===== Cache strategy tests =====

    #[test]
    fn boundary_marks_system_and_last_in_prefix() {
        // mu-yqeq.8: split_rope has System at 0, User/Assistant at
        // 1/2 (all stable+cacheable), then 2 Warm spans (volatile).
        // Strategy emits TWO boundaries: system (0) and last-in-prefix (2).
        let rope = split_rope();
        let boundaries = AnthropicCacheStrategy::new().boundaries(&rope);
        assert_eq!(boundaries, vec![CacheBoundary::at(0), CacheBoundary::at(2)]);
    }

    #[test]
    fn no_boundary_when_first_span_is_volatile() {
        // Warm first ⇒ cacheable prefix is empty ⇒ no boundary.
        let rope = RetainedRope::from_spans(vec![
            Span::new("u1", SpanKind::User, "hi", RetentionClass::Warm),
            Span::new("a1", SpanKind::Assistant, "hello", RetentionClass::Hot),
        ]);
        let boundaries = AnthropicCacheStrategy::new().boundaries(&rope);
        assert!(
            boundaries.is_empty(),
            "leading volatile span ⇒ empty cacheable prefix"
        );
    }

    #[test]
    fn boundary_at_first_system_and_last_index_when_entire_rope_is_stable() {
        // mu-chiw: with TWO System spans + 1 User (all
        // stable+cacheable), the intro prefix now stops BEFORE the
        // User span (conversation kind), marking the first System (0)
        // and the prefix end (1, the second System — pre_turn lands
        // there too and dedups). The User span (2) is the
        // conversation run-end anchor.
        let rope = RetainedRope::from_spans(vec![
            Span::new("a", SpanKind::System, "s", RetentionClass::Startup),
            Span::new("b", SpanKind::System, "s2", RetentionClass::Pinned),
            Span::new("c", SpanKind::User, "u", RetentionClass::Hot),
        ]);
        let boundaries = AnthropicCacheStrategy::new().boundaries(&rope);
        assert_eq!(
            boundaries,
            vec![
                CacheBoundary::at(0),
                CacheBoundary::at(1),
                CacheBoundary::at(2)
            ]
        );
    }

    #[test]
    fn stable_but_uncacheable_ends_the_prefix() {
        // A span can be stable yet marked uncacheable (e.g.,
        // contains timestamps the model shouldn't anchor on — spec
        // :575-578). The cacheable prefix ends at the hole. With
        // mu-yqeq.8's two-marker strategy, system at index 0 still
        // gets marked, and the last-in-prefix (index 1, before the
        // hole) also gets marked.
        let rope = RetainedRope::from_spans(vec![
            Span::new("a", SpanKind::System, "intro", RetentionClass::Startup),
            Span::new("b", SpanKind::User, "hi", RetentionClass::Hot),
            // Stable-but-uncacheable hole at index 2:
            Span::with_cacheable(
                "ts",
                SpanKind::System,
                "now is 12:34",
                RetentionClass::Hot,
                false,
            ),
            // Even though this is stable+cacheable again, the
            // contiguous cacheable prefix already ended at index 1.
            Span::new("c", SpanKind::Assistant, "hello", RetentionClass::Hot),
        ]);
        let boundaries = AnthropicCacheStrategy::new().boundaries(&rope);
        assert_eq!(boundaries, vec![CacheBoundary::at(0), CacheBoundary::at(1)]);
    }

    #[test]
    fn chiw_conversation_anchors_mark_pre_turn_and_rope_end() {
        // Canonical agent-loop rope: intro (system, schema) + a prior
        // turn (user, assistant) + the current turn (user,
        // tool_result). All four anchors distinct: system (0), intro
        // end (1), pre_turn (3 — the span just before the latest
        // User, i.e. the whole prior conversation re-matches there),
        // run end (5 — the within-turn anchor that lets loop call
        // N+1 re-read the turn-so-far at cache-read rates).
        let rope = RetainedRope::from_spans(vec![
            Span::new("sys", SpanKind::System, "s", RetentionClass::Startup),
            Span::new("schema", SpanKind::ToolSchema, "t", RetentionClass::Startup),
            Span::with_cacheable("u1", SpanKind::User, "q1", RetentionClass::Hot, true),
            Span::with_cacheable("a1", SpanKind::Assistant, "r1", RetentionClass::Hot, true),
            Span::with_cacheable("u2", SpanKind::User, "q2", RetentionClass::Hot, true),
            Span::with_cacheable(
                "tr",
                SpanKind::ToolResult,
                "out",
                RetentionClass::Warm,
                true,
            ),
        ]);
        let boundaries = AnthropicCacheStrategy::new().boundaries(&rope);
        assert_eq!(
            boundaries,
            vec![
                CacheBoundary::at(0),
                CacheBoundary::at(1),
                CacheBoundary::at(3),
                CacheBoundary::at(5),
            ],
            "four distinct anchors: system, intro end, pre-turn, rope end"
        );
    }

    #[test]
    fn chiw_uncacheable_hole_caps_conversation_anchors() {
        // An uncacheable span mid-conversation ends the contiguous
        // cacheable run: anchors past it would write cache prefixes
        // that can never re-match (the hole's content varies
        // call-to-call).
        let rope = RetainedRope::from_spans(vec![
            Span::new("sys", SpanKind::System, "s", RetentionClass::Startup),
            Span::with_cacheable("u1", SpanKind::User, "q1", RetentionClass::Hot, true),
            Span::with_cacheable(
                "vol",
                SpanKind::System,
                "now: 12:34",
                RetentionClass::Hot,
                false,
            ),
            Span::with_cacheable("u2", SpanKind::User, "q2", RetentionClass::Hot, true),
        ]);
        let boundaries = AnthropicCacheStrategy::new().boundaries(&rope);
        assert_eq!(
            boundaries,
            vec![CacheBoundary::at(0), CacheBoundary::at(1)],
            "run ends at the hole; u2 (index 3) must NOT be marked"
        );
    }

    #[test]
    fn annotate_attaches_ephemeral_marker_at_each_boundary() {
        // mu-yqeq.8: two boundaries on split_rope (system at 0,
        // last-in-prefix at 2) ⇒ both messages carry the marker.
        let rope = split_rope();
        let strategy = AnthropicCacheStrategy::new();
        let mut rendered =
            AnthropicProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);

        let boundaries = strategy.boundaries(&rope);
        strategy.annotate(&mut rendered, &boundaries);

        for (i, msg) in rendered.messages.iter().enumerate() {
            let expected_marker = if i == 0 || i == 2 {
                Some(CacheMarker::Ephemeral)
            } else {
                None
            };
            assert_eq!(
                msg.cache_marker(),
                expected_marker,
                "message {i}: expected {expected_marker:?}, got {:?}",
                msg.cache_marker(),
            );
        }
    }

    #[test]
    fn annotate_tolerates_out_of_range_boundaries() {
        // Per cache.rs:100-107 trait contract: out-of-range indices
        // MUST be tolerated silently. Simulate a boundary computed
        // from a longer rope shape applied to a shorter rendered set.
        let rope = RetainedRope::from_spans(vec![Span::new(
            "only",
            SpanKind::User,
            "hi",
            RetentionClass::Hot,
        )]);
        let strategy = AnthropicCacheStrategy::new();
        let mut rendered =
            AnthropicProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
        let bogus = vec![CacheBoundary::at(0), CacheBoundary::at(99)];
        strategy.annotate(&mut rendered, &bogus);

        assert_eq!(
            rendered.messages[0].cache_marker(),
            Some(CacheMarker::Ephemeral)
        );
        assert_eq!(rendered.messages.len(), 1, "no panic on bogus index");
    }

    #[test]
    fn empty_rope_yields_no_boundaries() {
        let rope = RetainedRope::new();
        let boundaries = AnthropicCacheStrategy::new().boundaries(&rope);
        assert!(boundaries.is_empty());
    }

    #[test]
    fn compose_renderer_and_strategy_end_to_end() {
        // Full pipeline: rope → render → boundaries → annotate. The
        // markers land on both expected messages, content preserved.
        let rope = split_rope();
        let renderer = AnthropicProviderRenderer::new();
        let strategy = AnthropicCacheStrategy::new();

        let mut rendered = renderer.render(&rope, ProjectionTarget::AgentView);
        let boundaries = strategy.boundaries(&rope);
        strategy.annotate(&mut rendered, &boundaries);

        assert_eq!(rendered.len(), 5);
        // mu-yqeq.8: two boundaries — system (index 0) and last
        // in-prefix (index 2, "a1" / Assistant "hello").
        assert_eq!(boundaries, vec![CacheBoundary::at(0), CacheBoundary::at(2)]);
        let system_ids: Vec<&str> = rendered.messages[0]
            .source_span_ids()
            .iter()
            .map(AsRef::as_ref)
            .collect();
        assert_eq!(system_ids, vec!["sys"]);
        let last_ids: Vec<&str> = rendered.messages[2]
            .source_span_ids()
            .iter()
            .map(AsRef::as_ref)
            .collect();
        assert_eq!(last_ids, vec!["a1"]);
        assert_eq!(
            rendered.messages[0].cache_marker(),
            Some(CacheMarker::Ephemeral)
        );
        assert_eq!(
            rendered.messages[2].cache_marker(),
            Some(CacheMarker::Ephemeral)
        );
    }

    #[test]
    fn strategy_trait_object_is_send_sync() {
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<dyn CacheStrategy>();
        assert_send_sync::<dyn ProviderRenderer>();
    }
}
