//! Offline health of one session log, for analytics.
//!
//! The same reads `mu recover` makes, run as a diagnosis only: load the log
//! once, tell gaps from a torn tail, and plan where a recovered head would
//! fork. Nothing is written and no model is involved, so this can run over
//! an archive one file at a time.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::continuation::{open_tool_calls, plan_recovery, DanglingCalls, RecoveryError};
use crate::event_log::{EventPayload, SessionEventLog};

/// One session log's condition, worst first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    /// Unreadable lines in the middle of the log: events are missing.
    /// Recover refuses it; the lines need inspecting.
    Gap,
    /// No head can be planned over the log (empty, no clean boundary, or
    /// an inherited history no provider would accept).
    Unrecoverable,
    /// A legacy head: recovering it needs its predecessor's log.
    NeedsPredecessor,
    /// The supplied predecessor log cannot seed this head: it has
    /// unreadable lines, is not the log the head forked from, or its history
    /// up to the fork does not project. The head itself may be fine with the
    /// right predecessor.
    BadPredecessor,
    /// No message or tool event at all (a worker that never ran a turn, a
    /// session created and closed). Nothing to recover, nothing damaged.
    NoConversation,
    /// The log ends mid-turn: the session died (or is still running) past
    /// its last clean boundary.
    Ragged,
    /// Clean apart from unreadable trailing lines a crash cut off.
    TornTail,
    Clean,
}

/// A tool call issued and never answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenCall {
    pub call_id: String,
    pub tool_name: String,
}

/// What [`diagnose_session_log`] found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHealth {
    pub status: HealthStatus,
    /// From the first readable event; None when nothing is readable.
    pub session_id: Option<String>,
    pub event_count: usize,
    pub last_event_id: Option<u64>,
    /// 1-based line numbers of unreadable lines that have events after
    /// them (or of every unreadable line when none is readable).
    pub gaps: Vec<usize>,
    /// Unreadable lines after the last readable event.
    pub torn_lines: usize,
    /// Where a recovered head would fork (the last clean boundary).
    pub fork_event_id: Option<u64>,
    /// What is ragged past the fork, when anything is.
    pub damage: Option<String>,
    /// Calls open at the end of the log: what `mu recover --close-calls`
    /// would answer.
    pub open_calls: Vec<OpenCall>,
    /// The predecessor a legacy head attached to, when one is needed.
    pub predecessor_session_id: Option<String>,
    /// Why no head can be planned, for `gap`, `unrecoverable` and
    /// `bad_predecessor`; a note when a supplied predecessor was not needed.
    pub detail: Option<String>,
}

/// Diagnose the log at `path`. `predecessor` is the log a legacy head forked
/// from; without it such a head is reported as `needs_predecessor`. A
/// predecessor supplied for a log that does not need one is ignored. Fails
/// only when a file cannot be read.
pub fn diagnose_session_log(
    path: &Path,
    predecessor: Option<&Path>,
) -> std::io::Result<SessionHealth> {
    let (log, damage) = SessionEventLog::from_jsonl_checked(path)?;
    let events = log.snapshot();
    let mut health = SessionHealth {
        status: HealthStatus::Clean,
        session_id: events.first().map(|e| e.session_id.clone()),
        event_count: events.len(),
        last_event_id: events.iter().map(|e| e.id).max(),
        gaps: damage.gaps,
        torn_lines: damage.torn_lines,
        fork_event_id: None,
        damage: None,
        open_calls: open_tool_calls(&events)
            .into_iter()
            .map(|(call_id, tool_name)| OpenCall { call_id, tool_name })
            .collect(),
        predecessor_session_id: None,
        detail: None,
    };
    if !health.gaps.is_empty() {
        health.status = HealthStatus::Gap;
        health.detail = Some(format!("unreadable line(s) {:?}", health.gaps));
        return Ok(health);
    }
    let conversational = events.iter().any(|e| {
        matches!(
            e.payload,
            EventPayload::UserMessage { .. }
                | EventPayload::AssistantMessageEvent { .. }
                | EventPayload::ToolCall { .. }
                | EventPayload::ToolResult { .. }
                // A head carries its history in its lineage, not in local turns.
                | EventPayload::HeadAttached { .. }
                | EventPayload::ContinuationSeeded { .. }
        )
    });
    if !conversational {
        health.status = HealthStatus::NoConversation;
        return Ok(health);
    }
    let predecessor_events = match predecessor {
        Some(p) => {
            // Recover refuses a predecessor with missing lines; so does this.
            let (log, damage) = SessionEventLog::from_jsonl_checked(p)?;
            if !damage.gaps.is_empty() {
                health.status = HealthStatus::BadPredecessor;
                health.detail = Some(format!(
                    "predecessor has unreadable line(s) {:?}",
                    damage.gaps
                ));
                return Ok(health);
            }
            Some(log.snapshot())
        }
        None => None,
    };
    let mut planned = plan_recovery(&events, predecessor_events.as_deref(), DanglingCalls::Rerun);
    if matches!(planned, Err(RecoveryError::PredecessorNotNeeded)) {
        health.detail = Some("predecessor not needed; ignored".into());
        planned = plan_recovery(&events, None, DanglingCalls::Rerun);
    }
    match planned {
        Ok(plan) => {
            health.fork_event_id = plan.continuation.fork_event_id;
            health.predecessor_session_id = plan.rebuilt_from;
            health.status = if plan.damage.is_some() {
                HealthStatus::Ragged
            } else if health.torn_lines > 0 {
                HealthStatus::TornTail
            } else {
                HealthStatus::Clean
            };
            health.damage = plan.damage;
        }
        Err(RecoveryError::PredecessorRequired {
            predecessor_session_id,
            branched_at_event_id,
        }) => {
            health.status = HealthStatus::NeedsPredecessor;
            health.predecessor_session_id = Some(predecessor_session_id);
            health.fork_event_id = branched_at_event_id;
        }
        Err(e @ RecoveryError::PredecessorMismatch { .. }) => {
            health.status = HealthStatus::BadPredecessor;
            health.detail = Some(e.to_string());
        }
        Err(
            ref e @ RecoveryError::PredecessorUnusable {
                ref predecessor_session_id,
                ..
            },
        ) => {
            health.status = HealthStatus::BadPredecessor;
            health.predecessor_session_id = Some(predecessor_session_id.clone());
            health.detail = Some(e.to_string());
        }
        Err(e) => {
            health.status = HealthStatus::Unrecoverable;
            health.detail = Some(e.to_string());
        }
    }
    Ok(health)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::types::{AssistantMessage, ContentBlock, StopReason, ToolArgs, ToolCall};
    use crate::event_log::{EventActor, SessionEvent};
    use std::io::Write;

    fn ev(id: u64, payload: EventPayload) -> SessionEvent {
        SessionEvent {
            id,
            session_id: "s1".into(),
            parent_event_ids: vec![],
            timestamp_unix_ms: 1_700_000_000_000 + id,
            actor: EventActor::Agent,
            payload,
        }
    }

    fn user(id: u64) -> SessionEvent {
        ev(
            id,
            EventPayload::UserMessage {
                content: "q".into(),
            },
        )
    }

    fn assistant(id: u64, block: ContentBlock, stop_reason: StopReason) -> SessionEvent {
        ev(
            id,
            EventPayload::AssistantMessageEvent {
                message: AssistantMessage {
                    content: vec![block],
                    stop_reason,
                    usage: None,
                },
            },
        )
    }

    fn answer(id: u64) -> SessionEvent {
        assistant(
            id,
            ContentBlock::Text { text: "a".into() },
            StopReason::EndTurn,
        )
    }

    fn grep_call(id: u64) -> SessionEvent {
        assistant(
            id,
            ContentBlock::ToolCall(ToolCall {
                id: "c1".into(),
                name: "grep".into(),
                arguments: ToolArgs::new(serde_json::json!({})).unwrap(),
            }),
            StopReason::ToolUse,
        )
    }

    fn write_log(dir: &Path, events: &[SessionEvent], tail: &str) -> std::path::PathBuf {
        write_named(dir, "session-1.jsonl", events, tail)
    }

    fn write_named(
        dir: &Path,
        name: &str,
        events: &[SessionEvent],
        tail: &str,
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).expect("create");
        for e in events {
            writeln!(f, "{}", serde_json::to_string(e).unwrap()).expect("write");
        }
        f.write_all(tail.as_bytes()).expect("tail");
        path
    }

    #[test]
    fn a_finished_turn_is_clean_and_a_torn_write_is_a_torn_tail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_log(dir.path(), &[user(1), answer(2)], "");
        let h = diagnose_session_log(&path, None).expect("diagnose");
        assert_eq!(h.status, HealthStatus::Clean);
        assert_eq!((h.event_count, h.fork_event_id), (2, Some(2)));

        let path = write_log(dir.path(), &[user(1), answer(2)], r#"{"id":3,"sess"#);
        let h = diagnose_session_log(&path, None).expect("diagnose");
        assert_eq!(h.status, HealthStatus::TornTail);
        assert_eq!(h.torn_lines, 1);
    }

    #[test]
    fn a_session_that_died_in_a_tool_call_is_ragged_with_the_call_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_log(dir.path(), &[user(1), answer(2), user(3), grep_call(4)], "");
        let h = diagnose_session_log(&path, None).expect("diagnose");
        assert_eq!(h.status, HealthStatus::Ragged);
        // The prompt that started the abandoned turn is kept: a recovered
        // head runs it again.
        assert_eq!(h.fork_event_id, Some(3));
        assert!(h.damage.is_some());
        assert_eq!(
            h.open_calls,
            vec![OpenCall {
                call_id: "c1".into(),
                tool_name: "grep".into()
            }]
        );
    }

    #[test]
    fn missing_lines_are_a_gap_and_a_worker_stub_has_no_conversation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("gap.jsonl");
        let mut f = std::fs::File::create(&path).expect("create");
        writeln!(f, "{}", serde_json::to_string(&user(1)).unwrap()).unwrap();
        writeln!(f, "{{not an event").unwrap();
        writeln!(f, "{}", serde_json::to_string(&answer(3)).unwrap()).unwrap();
        drop(f);
        let h = diagnose_session_log(&path, None).expect("diagnose");
        assert_eq!((h.status, h.gaps.clone()), (HealthStatus::Gap, vec![2]));

        let stub = ev(
            1,
            EventPayload::SessionCreated {
                provider_kind: "faux".into(),
                model: "m".into(),
                parent_session_id: None,
                branched_at_parent_event_id: None,
                usage_semantics: None,
            },
        );
        let path = write_log(dir.path(), &[stub], "");
        let h = diagnose_session_log(&path, None).expect("diagnose");
        assert_eq!(h.status, HealthStatus::NoConversation);
    }

    /// A head resumed from `s1` at event 2 that has run no turn of its own.
    fn legacy_head_without_a_turn() -> Vec<SessionEvent> {
        vec![
            ev(
                1,
                EventPayload::SessionCreated {
                    provider_kind: "faux".into(),
                    model: "m".into(),
                    parent_session_id: None,
                    branched_at_parent_event_id: None,
                    usage_semantics: None,
                },
            ),
            ev(
                2,
                EventPayload::HeadAttached {
                    daemon_id: "d1".into(),
                    claimed_actor: "operator".into(),
                    predecessor_session_id: "s1".into(),
                    branched_at_event_id: Some(2),
                },
            ),
        ]
    }

    #[test]
    fn a_legacy_head_with_no_turn_of_its_own_needs_its_predecessor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let head = write_named(dir.path(), "head.jsonl", &legacy_head_without_a_turn(), "");
        let h = diagnose_session_log(&head, None).expect("diagnose");
        assert_eq!(h.status, HealthStatus::NeedsPredecessor);
        assert_eq!(h.predecessor_session_id.as_deref(), Some("s1"));

        let pred = write_named(dir.path(), "pred.jsonl", &[user(1), answer(2)], "");
        let h = diagnose_session_log(&head, Some(&pred)).expect("diagnose");
        assert_eq!(h.status, HealthStatus::Clean);
        assert_eq!(h.predecessor_session_id.as_deref(), Some("s1"));
    }

    #[test]
    fn a_damaged_or_wrong_predecessor_is_reported_as_the_predecessor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let head = write_named(dir.path(), "head.jsonl", &legacy_head_without_a_turn(), "");

        let gapped = write_named(
            dir.path(),
            "gapped.jsonl",
            &[user(1)],
            &format!(
                "{{not an event\n{}\n",
                serde_json::to_string(&answer(3)).unwrap()
            ),
        );
        let h = diagnose_session_log(&head, Some(&gapped)).expect("diagnose");
        assert_eq!(h.status, HealthStatus::BadPredecessor);
        assert!(h.gaps.is_empty(), "the head's own log is intact");

        // no event 2, where the head forked
        let wrong = write_named(dir.path(), "wrong.jsonl", &[user(1)], "");
        let h = diagnose_session_log(&head, Some(&wrong)).expect("diagnose");
        assert_eq!(h.status, HealthStatus::BadPredecessor);
    }

    #[test]
    fn a_predecessor_the_log_does_not_need_is_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = write_named(dir.path(), "log.jsonl", &[user(1), answer(2)], "");
        let pred = write_named(dir.path(), "pred.jsonl", &[user(1), answer(2)], "");
        let h = diagnose_session_log(&log, Some(&pred)).expect("diagnose");
        assert_eq!(h.status, HealthStatus::Clean);
        assert!(h.detail.is_some());
    }
}
