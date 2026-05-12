//! Provider-message rendering — the [`ProviderRenderer`] trait and a
//! foundational [`FauxProviderRenderer`] impl.
//!
//! Per `specs/architecture/event-sourced-context.md` lines 592-612
//! ("Pluggable cache and provider strategies"), a renderer translates
//! a [`RetainedRope`] into provider-shaped messages. The trait is
//! orthogonal to [`CacheStrategy`](super::cache::CacheStrategy): the
//! rope is the controlled variable, the renderer is per-provider,
//! and the cache strategy is composable over either.
//!
//! ## Two projection targets
//!
//! [`ProjectionTarget`] selects between the [`AgentView`] (provider
//! messages — what the model sees) and the [`OperatorView`] (TUI /
//! log rendering — what the human sees). See spec lines 614-644.
//! Both views materialize from the same rope; they differ in *how*
//! each span is rendered. Faux's differentiation is intentionally
//! shallow (prefix tag for OperatorView) — production renderers will
//! produce richer differentiation (summaries, badges, collapsibles).
//!
//! ## Scope (mu-ktq foundational half)
//!
//! IN:
//! - [`ProviderRenderer`] trait with `render(rope, target) -> messages`
//! - [`ProjectionTarget`] enum (`AgentView`, `OperatorView`)
//! - [`ProviderMessages`] thin wrapper + [`ProviderMessage`] +
//!   [`ProviderRole`]
//! - [`FauxProviderRenderer`] — simple one-message-per-span impl
//!
//! DEFERRED (separate beads):
//! - `AnthropicProviderRenderer` — real adapter; needs design thought
//!   re: existing mu-i6j cache_control work and current Anthropic
//!   adapter shape.
//! - `OpenAIProviderRenderer`.
//! - Adoption of `ProviderRenderer` in the live agent loop.
//!
//! [`AgentView`]: ProjectionTarget::AgentView
//! [`OperatorView`]: ProjectionTarget::OperatorView
//! [`RetainedRope`]: super::rope::RetainedRope

use serde::{Deserialize, Serialize};

use super::rope::{RetainedRope, Span, SpanKind};

/// Which projection a renderer should produce.
///
/// The same retained pointer set materializes into two distinct
/// projections — the agent's provider messages and the operator's
/// TUI/log rendering. They share source spans but render differently.
/// See `specs/architecture/event-sourced-context.md` lines 614-644.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionTarget {
    /// What the model sees — full-fidelity provider messages.
    AgentView,
    /// What the human sees — may summarize, badge, collapse, etc.
    OperatorView,
}

/// The role of a single provider message.
///
/// Mirrors the role taxonomy supported by the underlying provider
/// APIs (Anthropic, OpenAI). Tool results are first-class because the
/// agent loop interleaves them with assistant turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderRole {
    System,
    User,
    Assistant,
    ToolResult,
}

impl From<SpanKind> for ProviderRole {
    fn from(kind: SpanKind) -> Self {
        match kind {
            SpanKind::System => ProviderRole::System,
            SpanKind::User => ProviderRole::User,
            SpanKind::Assistant => ProviderRole::Assistant,
            SpanKind::ToolResult => ProviderRole::ToolResult,
        }
    }
}

/// One provider-shaped message produced from a span (or sequence of
/// spans) by a [`ProviderRenderer`].
///
/// The `cache_marker` slot is reserved for [`CacheStrategy`] annotation
/// — see [`CacheMarker`]. The renderer leaves it `None`; the cache
/// strategy fills it in during [`annotate`].
///
/// `source_span_ids` is a back-pointer to the rope spans that
/// contributed to this message. The stub records the originating
/// span id; the full design will record byte/token ranges (see spec
/// "source map" mental model around lines 167-228).
///
/// [`CacheStrategy`]: super::cache::CacheStrategy
/// [`annotate`]: super::cache::CacheStrategy::annotate
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderMessage {
    pub role: ProviderRole,
    pub content: String,
    /// Source provenance: which span(s) produced this message.
    /// In the stub this is typically a singleton; future renderers
    /// may coalesce adjacent same-role spans into one message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_span_ids: Vec<String>,
    /// Cache annotation, populated by a [`CacheStrategy::annotate`]
    /// pass. `None` means "no cache directive at this position."
    ///
    /// [`CacheStrategy::annotate`]: super::cache::CacheStrategy::annotate
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_marker: Option<CacheMarker>,
}

/// Cache annotation attached to a [`ProviderMessage`] by a
/// [`CacheStrategy`](super::cache::CacheStrategy).
///
/// The variant set is intentionally small — Anthropic's
/// `cache_control: ephemeral` is the canonical case. Additional
/// provider-specific markers can be added as needed. Non-cache-
/// supporting providers never produce these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheMarker {
    /// Anthropic-style ephemeral cache point. Provider should cache
    /// the prefix up to and including this message.
    Ephemeral,
}

/// Thin wrapper carrying an ordered list of [`ProviderMessage`]s.
///
/// The wrapper exists (rather than `Vec<ProviderMessage>` directly)
/// for two reasons:
/// 1. Future per-batch metadata (token estimate, render-time, target
///    projection echoed back) without churning every caller.
/// 2. Allowing [`CacheStrategy::annotate`] to take `&mut
///    ProviderMessages` rather than `&mut Vec<...>`, which is the
///    spec's signature (see lines 597-603).
///
/// [`CacheStrategy::annotate`]: super::cache::CacheStrategy::annotate
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderMessages {
    pub messages: Vec<ProviderMessage>,
    /// Echo of the target projection this was rendered for. Helps
    /// downstream code (logging, diff) without needing the renderer
    /// to thread the target separately.
    #[serde(default = "default_target")]
    pub target: ProjectionTarget,
}

fn default_target() -> ProjectionTarget {
    ProjectionTarget::AgentView
}

impl Default for ProjectionTarget {
    fn default() -> Self {
        ProjectionTarget::AgentView
    }
}

impl ProviderMessages {
    /// Empty message set for `target`.
    pub fn empty(target: ProjectionTarget) -> Self {
        Self {
            messages: Vec::new(),
            target,
        }
    }

    /// Number of messages.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// True iff there are no messages.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

/// Provider-message renderer.
///
/// Per the spec proposal (lines 597-599):
/// ```text
/// trait ProviderRenderer
///   fn render(rope: &RetainedRope, target: ProjectionTarget) -> ProviderMessages
/// ```
///
/// Idiomatic Rust adds `&self` so the trait can carry per-impl
/// configuration (e.g., max-message-length policy, role-coalescing
/// flag); this does NOT change the spec's semantics — the impl
/// receiver is just where impl-specific state lives.
pub trait ProviderRenderer: Send + Sync {
    /// Render the given rope into provider-shaped messages for the
    /// chosen projection target.
    fn render(&self, rope: &RetainedRope, target: ProjectionTarget) -> ProviderMessages;
}

/// Foundational renderer impl — one message per span, role derived
/// from [`SpanKind`].
///
/// Differentiation between [`AgentView`] and [`OperatorView`] is
/// intentionally minimal: the operator view prefixes each message
/// content with `[span:<id>] ` to demonstrate that the renderer
/// respects the target. Production renderers will replace tool-
/// result JSON with structured summaries in OperatorView, surface
/// skill activations as one-line badges, etc. (see spec table at
/// lines 626-635).
///
/// [`AgentView`]: ProjectionTarget::AgentView
/// [`OperatorView`]: ProjectionTarget::OperatorView
#[derive(Debug, Default, Clone, Copy)]
pub struct FauxProviderRenderer;

impl FauxProviderRenderer {
    pub fn new() -> Self {
        Self
    }

    fn render_content(span: &Span, target: ProjectionTarget) -> String {
        match target {
            ProjectionTarget::AgentView => span.content.clone(),
            ProjectionTarget::OperatorView => format!("[span:{}] {}", span.id, span.content),
        }
    }
}

impl ProviderRenderer for FauxProviderRenderer {
    fn render(&self, rope: &RetainedRope, target: ProjectionTarget) -> ProviderMessages {
        let messages = rope
            .iter()
            .map(|span| ProviderMessage {
                role: span.kind.into(),
                content: Self::render_content(span, target),
                source_span_ids: vec![span.id.clone()],
                cache_marker: None,
            })
            .collect();

        ProviderMessages {
            messages,
            target,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::rope::{RetentionClass, Span, SpanKind};

    fn sample_rope() -> RetainedRope {
        RetainedRope::from_spans(vec![
            Span::new("sys", SpanKind::System, "you are mu", RetentionClass::Startup),
            Span::new("u1", SpanKind::User, "hi", RetentionClass::Hot),
            Span::new("a1", SpanKind::Assistant, "hello", RetentionClass::Hot),
            Span::new(
                "t1",
                SpanKind::ToolResult,
                "{\"ok\":true}",
                RetentionClass::Warm,
            ),
        ])
    }

    #[test]
    fn faux_renderer_produces_one_message_per_span() {
        let rope = sample_rope();
        let rendered = FauxProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
        assert_eq!(rendered.len(), rope.len());
        assert_eq!(rendered.target, ProjectionTarget::AgentView);
    }

    #[test]
    fn faux_renderer_maps_span_kind_to_role() {
        let rope = sample_rope();
        let rendered = FauxProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
        let roles: Vec<ProviderRole> = rendered.messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![
                ProviderRole::System,
                ProviderRole::User,
                ProviderRole::Assistant,
                ProviderRole::ToolResult,
            ]
        );
    }

    #[test]
    fn agent_view_renders_raw_content() {
        let rope = sample_rope();
        let rendered = FauxProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
        assert_eq!(rendered.messages[0].content, "you are mu");
        assert_eq!(rendered.messages[1].content, "hi");
    }

    #[test]
    fn operator_view_prefixes_with_span_id() {
        let rope = sample_rope();
        let rendered = FauxProviderRenderer::new().render(&rope, ProjectionTarget::OperatorView);
        assert_eq!(rendered.target, ProjectionTarget::OperatorView);
        assert_eq!(rendered.messages[0].content, "[span:sys] you are mu");
        assert_eq!(rendered.messages[1].content, "[span:u1] hi");
    }

    #[test]
    fn rendered_messages_carry_source_span_ids() {
        let rope = sample_rope();
        let rendered = FauxProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
        for (msg, span) in rendered.messages.iter().zip(rope.spans()) {
            assert_eq!(msg.source_span_ids, vec![span.id.clone()]);
        }
    }

    #[test]
    fn empty_rope_renders_empty_messages() {
        let rope = RetainedRope::new();
        let rendered = FauxProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
        assert!(rendered.is_empty());
    }

    #[test]
    fn fresh_render_has_no_cache_markers() {
        let rope = sample_rope();
        let rendered = FauxProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
        assert!(rendered.messages.iter().all(|m| m.cache_marker.is_none()));
    }

    #[test]
    fn renderer_trait_object_is_send_sync() {
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<dyn ProviderRenderer>();
    }
}
