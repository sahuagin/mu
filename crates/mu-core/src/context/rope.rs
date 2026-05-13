//! Stub `RetainedRope` for the foundational [`ProviderRenderer`] +
//! [`CacheStrategy`] trait surfaces (mu-ktq, scope-narrowed).
//!
//! **This is a stub.** The full rope — addressable spans over an
//! event log, with retention-class evolution, file-watch rehydration,
//! and projection coordinates — is mu-nat scope. This file gives the
//! trait method signatures something concrete to type-check against
//! without committing to the full design. See
//! `specs/architecture/event-sourced-context.md` (sections "Memory as
//! rope/projection" and "Skills, tools, and the active context as a
//! retained pointer set") for the target shape.
//!
//! The stub is deliberately small:
//! - [`RetentionClass`] — the seven retention classes named in the spec
//! - [`SpanKind`] — coarse role discriminator (System/User/Assistant/
//!   ToolResult) so a stub renderer can map spans → message roles
//! - [`Span`] — id + kind + content + retention + cacheable flag
//! - [`RetainedRope`] — ordered `Vec<Span>` with a basic constructor
//!
//! Full-rope features deliberately omitted at this layer:
//! - source event/event-log linkage (provenance)
//! - file-watch handles, rehydration semantics
//! - synthetic/compaction spans
//! - byte/token-range slicing
//! - eviction policy
//!
//! [`ProviderRenderer`]: super::ProviderRenderer
//! [`CacheStrategy`]: super::CacheStrategy

use serde::{Deserialize, Serialize};

/// Retention classes for a span, mirroring the names in
/// `specs/architecture/event-sourced-context.md` ("Memory as rope/
/// projection" section). Stable ordering is implied by the variant
/// order (most-stable first); the stub does not enforce policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionClass {
    /// Always present (e.g., startup instruction).
    Startup,
    /// Pinned by operator or policy; resists eviction.
    Pinned,
    /// Recently active; high relevance.
    Hot,
    /// Moderately relevant; eligible for demote.
    Warm,
    /// Low relevance; eligible for eviction.
    Cold,
    /// Demoted out of prompt-active; reachable via rehydrate.
    Archived,
    /// Replaced by a newer span; kept for audit only.
    Superseded,
}

impl RetentionClass {
    /// True iff this class is considered *stable* — i.e., the span is
    /// not expected to change within the current session's working
    /// set. Stable spans are the candidates for cache prefix
    /// placement. See
    /// `specs/architecture/event-sourced-context.md` lines 567-578.
    pub fn is_stable(self) -> bool {
        matches!(self, Self::Startup | Self::Pinned | Self::Hot)
    }
}

/// Discriminator for what role a span plays when rendered into
/// provider messages and how it should project differently between
/// `AgentView` and `OperatorView` (see
/// `specs/architecture/event-sourced-context.md` lines 538+ and
/// 614-644). The variant set covers the four conversational roles
/// (`System`/`User`/`Assistant`/`ToolResult`) plus the six
/// projection-differentiated kinds the spec table at lines 627-634
/// names individually (tool calls, tool schemas, skill activations,
/// memory injections, compactions, file loads).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    /// System prompt / startup instruction content.
    System,
    /// User input.
    User,
    /// Assistant output.
    Assistant,
    /// Tool-call result delivered back to the model.
    ToolResult,
    /// An outgoing tool invocation emitted by the assistant
    /// (name + argument JSON). Distinct from `ToolResult`, which is
    /// the value flowing back in.
    ToolCall,
    /// A registered tool's schema (name + parameter shape) consumed
    /// as part of the active tool set. Projection-relevant: the
    /// operator typically wants the name; the agent needs the schema.
    ToolSchema,
    /// A span emitted when a skill becomes active (skill metadata +
    /// reference files entering the retained pointer set per spec
    /// :542). Operator sees a one-line badge; agent sees the
    /// activation payload.
    SkillActivation,
    /// Content pulled in via memory recall (`agent memory show`-style
    /// injection). Operator sees a collapsed reference; agent sees
    /// the full content.
    MemoryInjection,
    /// Synthetic span replacing N evicted spans (compaction event,
    /// spec :531/:634). Operator sees the summary with a
    /// drill-down handle; agent sees the summary content.
    Compaction,
    /// File content loaded into context (spec, skill, source). Often
    /// volatile under file-watch rehydration. Operator sees a path +
    /// length; agent sees the file content.
    FileLoad,
}

/// One retained span in the rope.
///
/// Stub shape — the full design (see module-level comment) extends
/// this with provenance (`source_event_id`), coordinates
/// (`prompt_range_start`/`end`), and policy metadata. The stub keeps
/// just the fields the foundational traits need.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    /// Stable identifier within a rope. Caller-assigned; the stub
    /// does not allocate ids.
    pub id: String,
    /// What role this span maps to when rendered as a provider
    /// message.
    pub kind: SpanKind,
    /// The literal text content of the span. The full rope addresses
    /// content by `(source_event_id, byte_range)`; the stub inlines
    /// it for simplicity.
    pub content: String,
    /// Retention class. Determines stable-prefix eligibility.
    pub retention: RetentionClass,
    /// `true` iff this span is eligible for provider-side prompt
    /// caching. A span can be stable but uncacheable (e.g., contains
    /// timestamps the model shouldn't anchor on). See
    /// `specs/architecture/event-sourced-context.md` lines 575-578.
    pub cacheable: bool,
}

impl Span {
    /// Convenience constructor for the common case where a span is
    /// cacheable iff its retention class is stable.
    pub fn new(
        id: impl Into<String>,
        kind: SpanKind,
        content: impl Into<String>,
        retention: RetentionClass,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            content: content.into(),
            retention,
            cacheable: retention.is_stable(),
        }
    }
}

/// Stub of the retained pointer set ("rope") — an ordered sequence of
/// spans. The full rope will be a projection over an event log; the
/// stub is a plain `Vec<Span>` with order-preserving operations.
///
/// Iteration order is insertion order. Renderers and cache strategies
/// MUST treat this order as the canonical message order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetainedRope {
    spans: Vec<Span>,
}

impl RetainedRope {
    /// Empty rope.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a rope from a span sequence. Order is preserved.
    pub fn from_spans(spans: Vec<Span>) -> Self {
        Self { spans }
    }

    /// Append a span. Returns `&mut self` for chaining.
    pub fn push(&mut self, span: Span) -> &mut Self {
        self.spans.push(span);
        self
    }

    /// Number of spans in the rope.
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// True iff the rope has no spans.
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// Slice view of the spans in order.
    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    /// Iterate spans in retained order.
    pub fn iter(&self) -> std::slice::Iter<'_, Span> {
        self.spans.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_class_stability_is_stable_for_top_three() {
        assert!(RetentionClass::Startup.is_stable());
        assert!(RetentionClass::Pinned.is_stable());
        assert!(RetentionClass::Hot.is_stable());
        assert!(!RetentionClass::Warm.is_stable());
        assert!(!RetentionClass::Cold.is_stable());
        assert!(!RetentionClass::Archived.is_stable());
        assert!(!RetentionClass::Superseded.is_stable());
    }

    #[test]
    fn span_new_sets_cacheable_from_retention_stability() {
        let s1 = Span::new("a", SpanKind::System, "hi", RetentionClass::Startup);
        assert!(s1.cacheable, "stable retention should default to cacheable");

        let s2 = Span::new("b", SpanKind::User, "bye", RetentionClass::Cold);
        assert!(!s2.cacheable, "non-stable retention should default to uncacheable");
    }

    #[test]
    fn rope_preserves_insertion_order() {
        let mut rope = RetainedRope::new();
        rope.push(Span::new(
            "s1",
            SpanKind::System,
            "sys",
            RetentionClass::Startup,
        ))
        .push(Span::new("u1", SpanKind::User, "hi", RetentionClass::Hot))
        .push(Span::new(
            "a1",
            SpanKind::Assistant,
            "hello",
            RetentionClass::Hot,
        ));

        let ids: Vec<&str> = rope.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["s1", "u1", "a1"]);
        assert_eq!(rope.len(), 3);
        assert!(!rope.is_empty());
    }

    #[test]
    fn rope_from_spans_round_trips() {
        let spans = vec![
            Span::new("x", SpanKind::User, "a", RetentionClass::Hot),
            Span::new("y", SpanKind::Assistant, "b", RetentionClass::Warm),
        ];
        let rope = RetainedRope::from_spans(spans.clone());
        assert_eq!(rope.spans(), spans.as_slice());
    }

    #[test]
    fn span_kind_serde_round_trips_all_variants() {
        let cases = [
            (SpanKind::System, "\"system\""),
            (SpanKind::User, "\"user\""),
            (SpanKind::Assistant, "\"assistant\""),
            (SpanKind::ToolResult, "\"tool_result\""),
            (SpanKind::ToolCall, "\"tool_call\""),
            (SpanKind::ToolSchema, "\"tool_schema\""),
            (SpanKind::SkillActivation, "\"skill_activation\""),
            (SpanKind::MemoryInjection, "\"memory_injection\""),
            (SpanKind::Compaction, "\"compaction\""),
            (SpanKind::FileLoad, "\"file_load\""),
        ];
        for (kind, expected_json) in cases {
            let encoded = serde_json::to_string(&kind).expect("serialize");
            assert_eq!(
                encoded, expected_json,
                "snake_case wire form for {kind:?}",
            );
            let decoded: SpanKind = serde_json::from_str(&encoded).expect("round-trip");
            assert_eq!(decoded, kind);
        }
    }
}
