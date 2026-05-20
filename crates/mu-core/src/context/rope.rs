//! `RetainedRope` — the retained pointer set substrate.
//!
//! Originally landed as a stub for mu-ktq's [`ProviderRenderer`] /
//! [`CacheStrategy`] trait surfaces. **mu-nat extends it in place**
//! with the four API methods named in
//! `specs/architecture/event-sourced-context.md` lines 538-562
//! ("Skills, tools, and the active context as a retained pointer
//! set"):
//!
//! - [`RetainedRope::activate_skill`] / [`deactivate_skill`] — skill
//!   activation IS pointer-set membership change.
//! - [`RetainedRope::register_tool_schema`] /
//!   [`unregister_tool_schema`] — a registered tool's schema IS an
//!   addressable span; the active tool set IS a retained pointer
//!   subset.
//! - [`RetainedRope::filter_tools`] — capability attenuation as a
//!   pointer-set filter over tool-schema spans. Returns a view
//!   (`Vec<&Span>`); the rope itself is the immutable substrate.
//! - [`RetainedRope::provenance`] — `span_id → originating
//!   `RopeEvent`. Returns `Option<&RopeEvent>` for None-on-not-found
//!   plus zero-copy access.
//!
//! The existing API (push / from_spans / iter / spans / len /
//! is_empty) is unchanged so the mu-ktq tests and any
//! [`ProviderRenderer`] / [`CacheStrategy`] consumer keeps working.
//!
//! Internal model:
//! - `spans` is the active retained set (callers iterate this for
//!   rendering).
//! - `events` is the append-only audit log
//!   ([`super::event::RopeEvent`]). Deactivation does NOT remove
//!   prior activation events.
//! - `origins` maps every span id ever introduced into the rope to
//!   the index of its originating event. Survives deactivation so
//!   `provenance(span_id)` answers historically.
//!
//! Full-rope features still out of scope (separate beads / spec
//! sections):
//! - file-watch handles, rehydration semantics (mu-56p)
//! - byte/token-range slicing (full source map; spec :167-228)
//! - eviction policy / retention-class evolution (spec :246-258)
//!
//! [`ProviderRenderer`]: super::ProviderRenderer
//! [`CacheStrategy`]: super::CacheStrategy
//! [`deactivate_skill`]: RetainedRope::deactivate_skill
//! [`unregister_tool_schema`]: RetainedRope::unregister_tool_schema

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::event::RopeEvent;

/// Per-conceptual-type alias for span identifiers (mu-yqeq.2).
///
/// Backing storage is `Arc<str>` so rope clones and provenance-map
/// lookups bump a refcount rather than allocating a fresh `String`.
/// Public alias so external crates can name the type explicitly when
/// they need to construct a `Vec<SpanId>` (e.g., a
/// [`ProviderMessage::source_span_ids`](super::renderer::ProviderMessage)
/// field).
pub type SpanId = Arc<str>;

/// Per-conceptual-type alias for span content payloads (mu-yqeq.2).
///
/// Backing storage is `Arc<str>`. See [`SpanId`] for the rationale.
/// Distinct alias from `SpanId` so future changes to either storage
/// strategy (e.g., a lazy-rehydration handle for `content` only) can
/// happen without touching the other.
pub type SpanText = Arc<str>;

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
///
/// Not `Copy` because [`SpanKind::CompactionSummary`] (added by
/// mu-kgu.3) carries owned data (the list of span ids it absorbed
/// plus policy metadata). The other variants are still effectively
/// cheap to clone; existing call sites that previously moved out of
/// `&Span` now use `&span.kind` or `.clone()`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    /// Synthetic span produced by mu-kgu.3's `HashAndSummaryPolicy`:
    /// a single natural-language paragraph that absorbs every span
    /// the judge model decided NOT to keep verbatim. Distinct from
    /// the broader `Compaction` variant — `CompactionSummary` carries
    /// audit-grade metadata about *which* spans were absorbed so the
    /// operator can answer "what disappeared and why?" mechanically.
    CompactionSummary {
        /// Span ids that this summary span absorbed (i.e., the
        /// non-kept spans from the pre-compaction rope). Preserves
        /// the structural audit trail even after the
        /// `CompactionResult::decisions` log is dropped.
        absorbed_span_ids: Vec<SpanId>,
        /// Unix-milliseconds timestamp recording when the summary
        /// was generated. `0` is a valid sentinel for tests / fixtures
        /// that don't bother with a clock.
        generated_at_unix_ms: u64,
        /// Short stable identifier of the policy that produced the
        /// summary (e.g., `"hash-and-summary-v1"`). Lets the
        /// operator view group compaction events by policy.
        policy_id: String,
    },
}

/// One retained span in the rope.
///
/// Stub shape — the full design (see module-level comment) extends
/// this with provenance (`source_event_id`), coordinates
/// (`prompt_range_start`/`end`), and policy metadata. The stub keeps
/// just the fields the foundational traits need.
///
/// Fields are `pub(crate)` so in-crate construction (rope internals,
/// renderer, compaction, hash-summary) keeps struct-literal access,
/// while external crates go through accessor methods and the
/// constructors below. See spec mu-044 §Encapsulation discipline for
/// the rationale: insulates external call sites from future field-
/// type evolution (e.g., the `String → Arc<str>` swap in mu-yqeq.2b).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub(crate) id: SpanId,
    pub(crate) kind: SpanKind,
    pub(crate) content: SpanText,
    pub(crate) retention: RetentionClass,
    pub(crate) cacheable: bool,
}

impl Span {
    /// Convenience constructor for the common case where a span is
    /// cacheable iff its retention class is stable.
    pub fn new(
        id: impl Into<SpanId>,
        kind: SpanKind,
        content: impl Into<SpanText>,
        retention: RetentionClass,
    ) -> Self {
        Self::with_cacheable(id, kind, content, retention, retention.is_stable())
    }

    /// Constructor for the less-common case where `cacheable` must
    /// be set independently of the retention class (e.g., a stable
    /// span whose content carries volatile data the model shouldn't
    /// anchor on — see `specs/architecture/event-sourced-context.md`
    /// lines 575-578).
    pub fn with_cacheable(
        id: impl Into<SpanId>,
        kind: SpanKind,
        content: impl Into<SpanText>,
        retention: RetentionClass,
        cacheable: bool,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            content: content.into(),
            retention,
            cacheable,
        }
    }

    /// Stable identifier within a rope.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// What role this span maps to when rendered as a provider
    /// message.
    pub fn kind(&self) -> &SpanKind {
        &self.kind
    }

    /// The literal text content of the span.
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Retention class. Determines stable-prefix eligibility.
    pub fn retention(&self) -> RetentionClass {
        self.retention
    }

    /// `true` iff this span is eligible for provider-side prompt
    /// caching.
    pub fn cacheable(&self) -> bool {
        self.cacheable
    }
}

/// The retained pointer set ("rope") — an ordered sequence of active
/// spans plus an append-only provenance log.
///
/// Iteration order is insertion order. Renderers and cache strategies
/// MUST treat this order as the canonical message order.
///
/// `events` is the append-only [`RopeEvent`] log: every skill
/// activation / deactivation and tool-schema (un)registration is
/// recorded here. `origins` maps every span id ever introduced into
/// the rope to the index of its originating event — even after a
/// span has been deactivated, [`Self::provenance`] still answers
/// "where did this span come from?"
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetainedRope {
    spans: Vec<Span>,
    /// Append-only provenance log. Indexed by `origins` for
    /// `provenance(span_id)` lookups.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    events: Vec<RopeEvent>,
    /// `span_id -> events[index]`. Records the *introducing* event
    /// for each span. Entries are never removed: a deactivated span
    /// still has a provenance answer.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    origins: HashMap<SpanId, usize>,
}

impl RetainedRope {
    /// Empty rope.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a rope from a span sequence. Order is preserved. No
    /// provenance events are emitted — `from_spans` is the
    /// constructor for tests and fixtures, not for the skill / tool
    /// activation paths (use [`Self::activate_skill`] /
    /// [`Self::register_tool_schema`] for those).
    pub fn from_spans(spans: Vec<Span>) -> Self {
        Self {
            spans,
            events: Vec::new(),
            origins: HashMap::new(),
        }
    }

    /// Append a span. Returns `&mut self` for chaining. No
    /// provenance event is emitted — `push` is the low-level
    /// primitive; the skill / tool entry points use the higher-level
    /// methods that record provenance.
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

    /// The append-only provenance event log.
    pub fn events(&self) -> &[RopeEvent] {
        &self.events
    }

    /// Activate a skill: push its spans into the retained set and
    /// emit a [`RopeEvent::SkillActivated`] event. Returns
    /// `&mut self` for chaining.
    ///
    /// Per the experiment spec, v1 granularity is per-file — each
    /// reference file / SKILL.md section becomes one [`Span`]. The
    /// caller (typically [`crate::skill::SkillManager`]) constructs
    /// the spans with stable ids; this method records each span's
    /// id in [`Self::origins`] so [`Self::provenance`] can look it
    /// up later.
    ///
    /// Spans whose id already exists in `origins` (i.e., the rope
    /// has seen them before) are still appended to the active
    /// set, but the origin event is NOT updated — first activation
    /// is the canonical origin for provenance purposes. Callers who
    /// need to re-introduce a span fresh should give it a new id.
    pub fn activate_skill(
        &mut self,
        skill_id: impl Into<String>,
        span_refs: Vec<Span>,
    ) -> &mut Self {
        let skill_id = skill_id.into();
        let span_ids: Vec<SpanId> = span_refs.iter().map(|s| s.id.clone()).collect();
        let event_index = self.events.len();
        self.events.push(RopeEvent::SkillActivated {
            skill_id,
            span_ids: span_ids.clone(),
        });
        for span in span_refs {
            self.origins.entry(span.id.clone()).or_insert(event_index);
            self.spans.push(span);
        }
        self
    }

    /// Deactivate a skill: remove from the active span set every
    /// span whose origin is the most-recent (still-undeactivated)
    /// [`RopeEvent::SkillActivated`] for `skill_id`, and emit a
    /// [`RopeEvent::SkillDeactivated`] event recording which span
    /// ids were retired.
    ///
    /// `origins` entries are NOT removed: a deactivated span's
    /// provenance still resolves to its activation event, which is
    /// the right answer for "where did this span come from?"
    ///
    /// Idempotent: deactivating a skill that has no active spans
    /// emits a `SkillDeactivated` with an empty `span_ids`. No
    /// error — over-deactivation is benign.
    pub fn deactivate_skill(&mut self, skill_id: &str) -> &mut Self {
        let active_span_ids: Vec<SpanId> = self
            .spans
            .iter()
            .filter(|span| {
                self.origins
                    .get(&span.id)
                    .and_then(|&i| self.events.get(i))
                    .is_some_and(|ev| {
                        matches!(
                            ev,
                            RopeEvent::SkillActivated { skill_id: id, .. } if id == skill_id,
                        )
                    })
            })
            .map(|s| s.id.clone())
            .collect();
        self.spans
            .retain(|span| !active_span_ids.contains(&span.id));
        self.events.push(RopeEvent::SkillDeactivated {
            skill_id: skill_id.to_string(),
            span_ids: active_span_ids,
        });
        self
    }

    /// Register a tool's schema as a span in the rope. The span is
    /// appended to the active set; a [`RopeEvent::ToolSchemaRegistered`]
    /// event records the (tool_name, span_id) pair.
    ///
    /// If a span with the same id already exists in `origins`, it
    /// is still appended to the active set (callers can re-register
    /// freely), but the origin event is the first one. Use
    /// [`Self::unregister_tool_schema`] first if a clean re-register
    /// is intended.
    pub fn register_tool_schema(
        &mut self,
        tool_name: impl Into<String>,
        schema_span: Span,
    ) -> &mut Self {
        let tool_name = tool_name.into();
        let span_id = schema_span.id.clone();
        let event_index = self.events.len();
        self.events.push(RopeEvent::ToolSchemaRegistered {
            tool_name,
            span_id: span_id.clone(),
        });
        self.origins.entry(span_id).or_insert(event_index);
        self.spans.push(schema_span);
        self
    }

    /// Unregister a tool: remove its schema span from the active set
    /// and emit a [`RopeEvent::ToolSchemaUnregistered`] event. The
    /// span id removed is whichever span's origin is the
    /// most-recent (still-active) `ToolSchemaRegistered` for
    /// `tool_name`. Idempotent: unregistering an unknown tool emits
    /// an event with empty `span_id` and is otherwise a no-op.
    pub fn unregister_tool_schema(&mut self, tool_name: &str) -> &mut Self {
        let span_id: SpanId = self
            .spans
            .iter()
            .find(|span| {
                self.origins
                    .get(&span.id)
                    .and_then(|&i| self.events.get(i))
                    .is_some_and(|ev| matches!(
                        ev,
                        RopeEvent::ToolSchemaRegistered { tool_name: name, .. } if name == tool_name,
                    ))
            })
            .map(|s| s.id.clone())
            .unwrap_or_else(|| Arc::from(""));
        if !span_id.is_empty() {
            self.spans.retain(|span| span.id != span_id);
        }
        self.events.push(RopeEvent::ToolSchemaUnregistered {
            tool_name: tool_name.to_string(),
            span_id,
        });
        self
    }

    /// Capability attenuation as a pointer-set filter over the
    /// active tool-schema spans. Returns a borrowed view (the rope
    /// itself is unchanged); callers iterate the view to render the
    /// attenuated tool set or pass `predicate` results to a
    /// dispatcher.
    ///
    /// The predicate receives the full [`Span`] (id + kind +
    /// content + retention + cacheable). Only spans with
    /// `kind == SpanKind::ToolSchema` are considered — other span
    /// kinds are filtered out at this entry point.
    ///
    /// Spec line 558 ("Capability attenuation produces a pointer-set
    /// difference observable via the rope API"): the difference
    /// between `rope.spans()` (full tool set) and
    /// `rope.filter_tools(pred)` (attenuated subset) is exactly the
    /// set of tools the predicate rejected.
    pub fn filter_tools<F>(&self, predicate: F) -> Vec<&Span>
    where
        F: Fn(&Span) -> bool,
    {
        self.spans
            .iter()
            .filter(|s| s.kind == SpanKind::ToolSchema && predicate(s))
            .collect()
    }

    /// Provenance lookup. Returns the originating
    /// [`RopeEvent`] for `span_id`, or `None` if the rope has never
    /// held a span with that id.
    ///
    /// Survives deactivation: a span whose activation has been
    /// retired still resolves here. The returned reference borrows
    /// from the rope's append-only event log.
    pub fn provenance(&self, span_id: &str) -> Option<&RopeEvent> {
        let &idx = self.origins.get(span_id)?;
        self.events.get(idx)
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
        assert!(
            !s2.cacheable,
            "non-stable retention should default to uncacheable"
        );
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

        let ids: Vec<&str> = rope.iter().map(|s| s.id()).collect();
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
            assert_eq!(encoded, expected_json, "snake_case wire form for {kind:?}",);
            let decoded: SpanKind = serde_json::from_str(&encoded).expect("round-trip");
            assert_eq!(decoded, kind);
        }
    }

    // ── mu-nat: skill activation / deactivation ───────────────────

    fn skill_span(skill_id: &str, file: &str, body: &str) -> Span {
        Span::new(
            format!("skill:{skill_id}:{file}"),
            SpanKind::SkillActivation,
            body,
            RetentionClass::Pinned,
        )
    }

    #[test]
    fn activate_skill_pushes_spans_and_emits_event() {
        let mut rope = RetainedRope::new();
        rope.activate_skill(
            "goal-protocol",
            vec![
                skill_span("goal-protocol", "SKILL.md", "hello"),
                skill_span("goal-protocol", "stop-criteria.md", "stop"),
            ],
        );
        assert_eq!(rope.len(), 2);
        assert_eq!(rope.events().len(), 1);
        match &rope.events()[0] {
            RopeEvent::SkillActivated { skill_id, span_ids } => {
                assert_eq!(skill_id, "goal-protocol");
                assert_eq!(span_ids.len(), 2);
            }
            other => panic!("expected SkillActivated, got {other:?}"),
        }
    }

    #[test]
    fn deactivate_skill_removes_only_its_spans() {
        let mut rope = RetainedRope::new();
        rope.activate_skill(
            "goal-protocol",
            vec![skill_span("goal-protocol", "SKILL.md", "g")],
        );
        rope.activate_skill("review", vec![skill_span("review", "SKILL.md", "r")]);
        assert_eq!(rope.len(), 2);

        rope.deactivate_skill("goal-protocol");
        // Only "review"'s span remains.
        assert_eq!(rope.len(), 1);
        assert_eq!(rope.spans()[0].id(), "skill:review:SKILL.md");

        // SkillDeactivated event recorded both spans retired.
        let last = rope.events().last().expect("event recorded");
        match last {
            RopeEvent::SkillDeactivated { skill_id, span_ids } => {
                assert_eq!(skill_id, "goal-protocol");
                let actual: Vec<&str> = span_ids.iter().map(AsRef::as_ref).collect();
                assert_eq!(actual, vec!["skill:goal-protocol:SKILL.md"]);
            }
            other => panic!("expected SkillDeactivated, got {other:?}"),
        }
    }

    #[test]
    fn deactivate_unknown_skill_emits_empty_event() {
        let mut rope = RetainedRope::new();
        rope.deactivate_skill("never-was");
        assert!(rope.is_empty());
        match rope.events().last().expect("event") {
            RopeEvent::SkillDeactivated { span_ids, .. } => assert!(span_ids.is_empty()),
            other => panic!("expected empty SkillDeactivated, got {other:?}"),
        }
    }

    #[test]
    fn provenance_returns_originating_skill_event() {
        let mut rope = RetainedRope::new();
        rope.activate_skill("review", vec![skill_span("review", "SKILL.md", "r")]);
        let prov = rope
            .provenance("skill:review:SKILL.md")
            .expect("provenance hit");
        match prov {
            RopeEvent::SkillActivated { skill_id, .. } => assert_eq!(skill_id, "review"),
            other => panic!("expected SkillActivated, got {other:?}"),
        }
    }

    #[test]
    fn provenance_survives_deactivation() {
        let mut rope = RetainedRope::new();
        rope.activate_skill("review", vec![skill_span("review", "SKILL.md", "r")]);
        rope.deactivate_skill("review");
        // The span is no longer in the active set...
        assert!(rope.is_empty());
        // ...but provenance still answers historically.
        let prov = rope
            .provenance("skill:review:SKILL.md")
            .expect("provenance survives");
        match prov {
            RopeEvent::SkillActivated { skill_id, .. } => assert_eq!(skill_id, "review"),
            other => panic!("expected SkillActivated, got {other:?}"),
        }
    }

    #[test]
    fn provenance_returns_none_for_unknown_span_id() {
        let rope = RetainedRope::new();
        assert!(rope.provenance("does-not-exist").is_none());
    }

    // ── mu-nat: tool schema registration / filtering ──────────────

    fn tool_schema_span(name: &str, schema: &str) -> Span {
        Span::new(
            format!("tool-schema:{name}"),
            SpanKind::ToolSchema,
            schema,
            RetentionClass::Hot,
        )
    }

    #[test]
    fn register_tool_schema_pushes_span_and_emits_event() {
        let mut rope = RetainedRope::new();
        rope.register_tool_schema("read", tool_schema_span("read", "{...read schema...}"));
        assert_eq!(rope.len(), 1);
        assert_eq!(rope.spans()[0].kind, SpanKind::ToolSchema);
        match rope.events().last().expect("event") {
            RopeEvent::ToolSchemaRegistered { tool_name, span_id } => {
                assert_eq!(tool_name, "read");
                assert_eq!(span_id.as_ref(), "tool-schema:read");
            }
            other => panic!("expected ToolSchemaRegistered, got {other:?}"),
        }
    }

    #[test]
    fn unregister_tool_schema_removes_span_and_emits_event() {
        let mut rope = RetainedRope::new();
        rope.register_tool_schema("read", tool_schema_span("read", "r"));
        rope.register_tool_schema("write", tool_schema_span("write", "w"));
        assert_eq!(rope.len(), 2);

        rope.unregister_tool_schema("read");
        assert_eq!(rope.len(), 1);
        assert_eq!(rope.spans()[0].id(), "tool-schema:write");
        match rope.events().last().expect("event") {
            RopeEvent::ToolSchemaUnregistered { tool_name, span_id } => {
                assert_eq!(tool_name, "read");
                assert_eq!(span_id.as_ref(), "tool-schema:read");
            }
            other => panic!("expected ToolSchemaUnregistered, got {other:?}"),
        }
    }

    #[test]
    fn filter_tools_returns_only_matching_tool_schemas() {
        let mut rope = RetainedRope::new();
        rope.register_tool_schema("read", tool_schema_span("read", "r"));
        rope.register_tool_schema("write", tool_schema_span("write", "w"));
        rope.register_tool_schema("bash", tool_schema_span("bash", "b"));
        // Non-tool-schema spans must be excluded by filter_tools.
        rope.activate_skill(
            "goal-protocol",
            vec![skill_span("goal-protocol", "SKILL.md", "g")],
        );

        // Predicate: allowed_tools = {read, bash}.
        let allowed = ["tool-schema:read", "tool-schema:bash"];
        let view = rope.filter_tools(|s| allowed.contains(&s.id()));
        assert_eq!(view.len(), 2);
        let ids: Vec<&str> = view.iter().map(|s| s.id()).collect();
        assert!(ids.contains(&"tool-schema:read"));
        assert!(ids.contains(&"tool-schema:bash"));
        // Skill span is excluded even if predicate is "true".
        let all_predicate = rope.filter_tools(|_| true);
        assert_eq!(
            all_predicate.len(),
            3,
            "filter_tools is scoped to ToolSchema spans regardless of predicate"
        );
    }

    #[test]
    fn filter_tools_substrate_is_immutable() {
        let mut rope = RetainedRope::new();
        rope.register_tool_schema("read", tool_schema_span("read", "r"));
        rope.register_tool_schema("write", tool_schema_span("write", "w"));
        let original_len = rope.len();
        let original_events = rope.events().len();
        let _view = rope.filter_tools(|s| s.id() == "tool-schema:read");
        // Substrate is unchanged.
        assert_eq!(rope.len(), original_len);
        assert_eq!(rope.events().len(), original_events);
    }

    #[test]
    fn filter_tools_pointer_set_difference_is_observable() {
        // Spec line 558: "Capability attenuation produces a pointer-
        // set difference observable via the rope API." The set diff
        // between rope.spans() (tool schemas only) and
        // rope.filter_tools(pred) IS the attenuated-out tools.
        let mut rope = RetainedRope::new();
        rope.register_tool_schema("read", tool_schema_span("read", "r"));
        rope.register_tool_schema("write", tool_schema_span("write", "w"));
        rope.register_tool_schema("bash", tool_schema_span("bash", "b"));

        let full: Vec<&str> = rope.spans().iter().map(|s| s.id()).collect();
        let attenuated_ids: Vec<&str> = rope
            .filter_tools(|s| s.id() == "tool-schema:read")
            .into_iter()
            .map(|s| s.id())
            .collect();
        let diff: Vec<&str> = full
            .iter()
            .filter(|id| !attenuated_ids.contains(id))
            .copied()
            .collect();
        assert_eq!(diff.len(), 2);
        assert!(diff.contains(&"tool-schema:write"));
        assert!(diff.contains(&"tool-schema:bash"));
    }

    #[test]
    fn provenance_returns_tool_schema_event() {
        let mut rope = RetainedRope::new();
        rope.register_tool_schema("read", tool_schema_span("read", "r"));
        let prov = rope.provenance("tool-schema:read").expect("hit");
        match prov {
            RopeEvent::ToolSchemaRegistered { tool_name, .. } => assert_eq!(tool_name, "read"),
            other => panic!("expected ToolSchemaRegistered, got {other:?}"),
        }
    }

    #[test]
    fn from_spans_keeps_legacy_constructor_working() {
        // mu-ktq fixtures and any external callers that built ropes
        // directly from a Vec<Span> must continue to work without
        // touching provenance.
        let spans = vec![
            Span::new("u1", SpanKind::User, "hi", RetentionClass::Hot),
            Span::new("a1", SpanKind::Assistant, "hello", RetentionClass::Hot),
        ];
        let rope = RetainedRope::from_spans(spans.clone());
        assert_eq!(rope.spans(), spans.as_slice());
        // No provenance events: from_spans is the bypass path.
        assert!(rope.events().is_empty());
        assert!(rope.provenance("u1").is_none());
    }
}
