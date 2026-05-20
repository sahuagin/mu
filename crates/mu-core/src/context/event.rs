//! Rope-local provenance events (mu-nat).
//!
//! These are the events emitted by [`RetainedRope`] operations that
//! introduce or retire spans (skill activation, tool-schema
//! registration, and their inverses). They live alongside the rope
//! itself, not on the per-session wire-level event log
//! ([`crate::event_log::EventPayload`]), so that mu-nat does not
//! widen the wire protocol — a future bead can absorb these into
//! `EventPayload` when the broader event log is ready to carry
//! rope-projection provenance.
//!
//! Per `specs/architecture/event-sourced-context.md` lines 538-562
//! ("Skills, tools, and the active context as a retained pointer
//! set"): activation/deactivation IS pointer-set membership change.
//! Every span entering or leaving the retained set is named by one
//! of these events; the rope's `origins` map records `span_id →
//! event index` so [`RetainedRope::provenance`] can answer "where
//! did this span come from?" for any span the rope has ever held.
//!
//! [`RetainedRope`]: super::rope::RetainedRope
//! [`RetainedRope::provenance`]: super::rope::RetainedRope::provenance

use serde::{Deserialize, Serialize};

use super::rope::SpanId;

/// Event introduced or retired by a [`RetainedRope`] operation.
///
/// The variant set mirrors the four operations [`RetainedRope`]
/// supports for skill/tool span management: activate/deactivate a
/// skill, register/unregister a tool schema. Every event carries the
/// span ids it affected so callers can iterate provenance without
/// re-walking the rope.
///
/// Encoded with `#[serde(tag = "kind", rename_all = "snake_case")]`
/// to match the existing wire-event convention in
/// [`crate::event_log::EventPayload`] — even though these events do
/// not currently cross the wire, the consistent encoding makes
/// future absorption mechanical.
///
/// [`RetainedRope`]: super::rope::RetainedRope
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RopeEvent {
    /// A skill became active. `span_ids` are the spans pushed into
    /// the rope as part of the activation (per-file granularity for
    /// v1 per the experiment spec). The originating
    /// `SkillManager::activate` call is the source of this event.
    SkillActivated {
        skill_id: String,
        span_ids: Vec<SpanId>,
    },
    /// A skill was deactivated. `span_ids` echo the spans removed
    /// from the rope as a result. The matching `SkillActivated`
    /// event is retained in the events log; only the active span set
    /// is mutated.
    SkillDeactivated {
        skill_id: String,
        span_ids: Vec<SpanId>,
    },
    /// A tool's schema span was added to the rope. `tool_name` and
    /// `span_id` together identify which schema; `tool_name` is the
    /// stable handle, `span_id` is the rope-local id (typically
    /// `format!("tool-schema:{tool_name}")`).
    ToolSchemaRegistered { tool_name: String, span_id: SpanId },
    /// A tool's schema span was removed from the rope (typically
    /// because the tool was unregistered or replaced).
    ToolSchemaUnregistered { tool_name: String, span_id: SpanId },
}

impl RopeEvent {
    /// The span ids this event introduced (for `*Registered` /
    /// `*Activated`) or retired (for the inverse variants). Useful
    /// for callers that want to iterate provenance without matching
    /// on every variant.
    pub fn affected_span_ids(&self) -> Vec<&str> {
        match self {
            RopeEvent::SkillActivated { span_ids, .. }
            | RopeEvent::SkillDeactivated { span_ids, .. } => {
                span_ids.iter().map(AsRef::as_ref).collect()
            }
            RopeEvent::ToolSchemaRegistered { span_id, .. }
            | RopeEvent::ToolSchemaUnregistered { span_id, .. } => vec![span_id.as_ref()],
        }
    }

    /// True iff this event introduces spans (vs retires them).
    pub fn is_introducing(&self) -> bool {
        matches!(
            self,
            RopeEvent::SkillActivated { .. } | RopeEvent::ToolSchemaRegistered { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_activated_round_trips_via_serde() -> Result<(), serde_json::Error> {
        let ev = RopeEvent::SkillActivated {
            skill_id: "goal-protocol".into(),
            span_ids: vec!["skill:goal-protocol:SKILL.md".into()],
        };
        let v = serde_json::to_value(&ev)?;
        assert_eq!(v["kind"], "skill_activated");
        assert_eq!(v["skill_id"], "goal-protocol");
        let decoded: RopeEvent = serde_json::from_value(v)?;
        assert_eq!(decoded, ev);
        Ok(())
    }

    #[test]
    fn tool_schema_registered_round_trips_via_serde() -> Result<(), serde_json::Error> {
        let ev = RopeEvent::ToolSchemaRegistered {
            tool_name: "read".into(),
            span_id: "tool-schema:read".into(),
        };
        let v = serde_json::to_value(&ev)?;
        assert_eq!(v["kind"], "tool_schema_registered");
        let decoded: RopeEvent = serde_json::from_value(v)?;
        assert_eq!(decoded, ev);
        Ok(())
    }

    #[test]
    fn affected_span_ids_collects_correctly_per_variant() {
        let skill = RopeEvent::SkillActivated {
            skill_id: "s".into(),
            span_ids: vec!["a".into(), "b".into()],
        };
        assert_eq!(skill.affected_span_ids(), vec!["a", "b"]);

        let tool = RopeEvent::ToolSchemaRegistered {
            tool_name: "read".into(),
            span_id: "tool-schema:read".into(),
        };
        assert_eq!(tool.affected_span_ids(), vec!["tool-schema:read"]);
    }

    #[test]
    fn is_introducing_distinguishes_directions() {
        assert!(RopeEvent::SkillActivated {
            skill_id: "s".into(),
            span_ids: vec![],
        }
        .is_introducing());
        assert!(RopeEvent::ToolSchemaRegistered {
            tool_name: "t".into(),
            span_id: "x".into(),
        }
        .is_introducing());
        assert!(!RopeEvent::SkillDeactivated {
            skill_id: "s".into(),
            span_ids: vec![],
        }
        .is_introducing());
        assert!(!RopeEvent::ToolSchemaUnregistered {
            tool_name: "t".into(),
            span_id: "x".into(),
        }
        .is_introducing());
    }
}
