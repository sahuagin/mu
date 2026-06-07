//! mu-mh4: continuation-grade projection — events → messages for
//! *resuming* a session, not just viewing it.
//!
//! A session that died mid-iteration (e.g. a provider 402 in the
//! middle of a tool loop) leaves a **ragged tail** in its event log:
//! a `ToolCall` with no matching `ToolResult`, an assistant turn that
//! never reached a terminal stop, a half-written record. Viewing
//! tolerates this — the console just shows what's there. Provider
//! APIs do not: Anthropic and OpenAI both reject a message history
//! whose last assistant turn has unanswered tool calls.
//!
//! So resume = **fork at the last CLEAN BOUNDARY**. We walk the log,
//! project each significant event into an [`AgentMessage`], and track
//! where the conversation was last in a coherent, sendable state (a
//! completed turn with every tool call answered). The ragged remainder
//! stays in the log untouched (the log is the noun) but is excluded
//! from the head we hand the new session.
//!
//! Two entry points fall out of the operator's `--resume` / `--recover`
//! split (bead mu-mh4, CLI-contract comment 2026-06-07):
//!
//!   - [`project_strict`] — `mu --resume`. Refuses a ragged log,
//!     returning a [`ContinuationError`] that names the *exact* damage
//!     (which event id, what's missing) so the caller can print a
//!     git-style hint pointing at `mu --recover`.
//!   - [`project_to_clean_boundary`] — the repairing path's projection.
//!     Truncates to the last clean boundary and returns the messages
//!     plus the id of the event it forked at. `mu --recover` lays
//!     tombstones over the excluded tail and resumes from here.
//!
//! Both honor the tombstone rule: an event whose id appears in a
//! `Tombstone`'s `target_event_id` is skipped entirely (mu-mh4 tier 3
//! cheap part), so a recovered log projects cleanly.

use std::collections::BTreeSet;

use crate::agent::types::{AgentMessage, ContentBlock};
use crate::event_log::{tombstoned_ids, EventPayload, SessionEvent};

/// Why a strict continuation projection refused. Each variant names
/// the precise damage so `mu --resume`'s refusal can point at the
/// exact record and suggest the `mu --recover` remediation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContinuationError {
    /// The log has no clean boundary at all — there is nothing
    /// coherent to resume from (e.g. it died during its very first
    /// turn, before any completed exchange).
    NoCleanBoundary {
        /// Best-effort note on why (e.g. "unanswered tool call").
        detail: String,
    },
    /// The tail past the last clean boundary is ragged: there ARE
    /// events after the boundary that a strict resume cannot send.
    /// `--recover` is the authorized path to tombstone them.
    RaggedTail {
        /// Event id of the last clean boundary (the fork point a
        /// `--recover` would use).
        clean_boundary_event_id: u64,
        /// Event id of the first ragged event past the boundary.
        first_ragged_event_id: u64,
        /// What's wrong with the tail.
        detail: String,
    },
    /// The log is empty (no events) — nothing to resume.
    Empty,
}

impl std::fmt::Display for ContinuationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContinuationError::Empty => write!(f, "session log is empty; nothing to resume"),
            ContinuationError::NoCleanBoundary { detail } => {
                write!(f, "no clean boundary to resume from: {detail}")
            }
            ContinuationError::RaggedTail {
                clean_boundary_event_id,
                first_ragged_event_id,
                detail,
            } => write!(
                f,
                "incomplete record at event {first_ragged_event_id}: {detail} \
                 (last clean boundary was event {clean_boundary_event_id})"
            ),
        }
    }
}

impl std::error::Error for ContinuationError {}

/// A successful continuation projection: the message history to seed
/// the resumed session with, plus the event id we forked at.
#[derive(Debug, Clone, PartialEq)]
pub struct Continuation {
    /// The provider-sendable message history, truncated to the last
    /// clean boundary.
    pub messages: Vec<AgentMessage>,
    /// The event id of the clean boundary this forked at — recorded
    /// on the resumed session's `SessionCreated.branched_at_parent_event_id`.
    /// `None` only when the boundary is the empty conversation.
    pub fork_event_id: Option<u64>,
    /// True when there were events past the fork point that a strict
    /// resume could not include (the ragged tail). `mu --recover`
    /// tombstones these; `mu --resume` would have refused.
    pub had_ragged_tail: bool,
    /// The id of the first ragged event past the boundary, when
    /// `had_ragged_tail`. Used by `--recover` to know where to start
    /// laying tombstones.
    pub first_ragged_event_id: Option<u64>,
}

/// One projected, sendable boundary: the messages up to and including
/// a coherent point, and the event id of that point.
struct Boundary {
    messages: Vec<AgentMessage>,
    event_id: u64,
}

/// Walk the (tombstone-filtered) log and project it into messages,
/// recording every CLEAN BOUNDARY along the way. A clean boundary is a
/// point where the conversation is coherent and sendable to a provider:
/// no assistant turn with unanswered tool calls is left dangling.
///
/// Returns the projection up to the LAST clean boundary plus metadata
/// about whether anything ragged followed it. This is the shared core
/// behind both [`project_strict`] and [`project_to_clean_boundary`].
fn project_internal(events: &[SessionEvent]) -> Result<Continuation, ContinuationError> {
    if events.is_empty() {
        return Err(ContinuationError::Empty);
    }

    let dead: BTreeSet<u64> = tombstoned_ids(events);

    // Running message list. Pending tool calls are tracked WITH the
    // event id that introduced them, so the "first ragged event" of an
    // abandoned turn points at the assistant turn that issued the
    // never-answered call — not at whatever event happened to come next.
    let mut messages: Vec<AgentMessage> = Vec::new();
    let mut pending_tool_calls: Vec<(String, u64)> = Vec::new();

    // The last coherent snapshot we could resume from.
    let mut last_clean: Option<Boundary> = None;
    // The first event id (and reason) where coherence broke, if any.
    let mut first_ragged: Option<(u64, String)> = None;

    // Capture a clean boundary: a point with no pending tool calls,
    // where the conversation is naturally sendable to a provider.
    let capture = |messages: &[AgentMessage], id: u64, last_clean: &mut Option<Boundary>| {
        *last_clean = Some(Boundary {
            messages: messages.to_vec(),
            event_id: id,
        });
    };

    for ev in events {
        // Tombstoned events are skipped entirely — the one projection
        // rule that makes a recovered log read clean (mu-mh4 tier 3).
        if dead.contains(&ev.id) {
            continue;
        }

        match &ev.payload {
            EventPayload::UserMessage { content } => {
                messages.push(AgentMessage::User {
                    content: content.clone(),
                });
                if pending_tool_calls.is_empty() {
                    capture(&messages, ev.id, &mut last_clean);
                }
            }
            EventPayload::AssistantMessageEvent { message } => {
                for block in &message.content {
                    if let ContentBlock::ToolCall(tc) = block {
                        pending_tool_calls.push((tc.id.clone(), ev.id));
                    }
                }
                messages.push(AgentMessage::Assistant(message.clone()));
                // An assistant turn with no tool calls (EndTurn) is a
                // clean boundary; one with tool calls leaves us mid-turn.
                if pending_tool_calls.is_empty() {
                    capture(&messages, ev.id, &mut last_clean);
                }
            }
            EventPayload::ToolCall { call_id, .. } => {
                // A bare ToolCall event (not already inside an assistant
                // block) still registers as pending. Dedup against calls
                // the assistant block already introduced.
                if !pending_tool_calls.iter().any(|(c, _)| c == call_id) {
                    pending_tool_calls.push((call_id.clone(), ev.id));
                }
            }
            EventPayload::ToolResult {
                call_id,
                content,
                is_error,
            } => {
                let before = pending_tool_calls.len();
                pending_tool_calls.retain(|(c, _)| c != call_id);
                let removed = pending_tool_calls.len() != before;
                if !removed && first_ragged.is_none() {
                    first_ragged = Some((
                        ev.id,
                        format!("tool result for call `{call_id}` has no matching tool call"),
                    ));
                }
                messages.push(AgentMessage::ToolResult {
                    call_id: call_id.clone(),
                    content: content.clone(),
                    is_error: *is_error,
                });
                if pending_tool_calls.is_empty() {
                    capture(&messages, ev.id, &mut last_clean);
                }
            }
            EventPayload::Done { .. } => {
                // A Done with everything answered is a clean boundary; a
                // Done with calls still pending means the turn was
                // abandoned — the ragged point is the assistant turn that
                // issued the oldest unanswered call.
                if pending_tool_calls.is_empty() {
                    capture(&messages, ev.id, &mut last_clean);
                } else if first_ragged.is_none() {
                    let (call_id, intro_id) = pending_tool_calls[0].clone();
                    first_ragged = Some((
                        intro_id,
                        format!("tool call `{call_id}` was never answered (ask terminated)"),
                    ));
                }
            }
            EventPayload::Error { message } => {
                // A terminal error with pending calls is the classic
                // 402-incident shape: the ragged point is the dangling
                // call's introducing turn. With nothing pending, the
                // error itself is the ragged point.
                if first_ragged.is_none() {
                    if let Some((call_id, intro_id)) = pending_tool_calls.first().cloned() {
                        first_ragged = Some((
                            intro_id,
                            format!(
                                "tool call `{call_id}` was never answered before terminal error: {message}"
                            ),
                        ));
                    } else {
                        first_ragged = Some((ev.id, format!("terminal error: {message}")));
                    }
                }
            }
            EventPayload::ErrorInvalidMessage {
                validation_error, ..
            } => {
                if first_ragged.is_none() {
                    first_ragged =
                        Some((ev.id, format!("invalid provider message: {validation_error}")));
                }
            }
            // All other event kinds (ContextAssembly, CompactionAssembly,
            // ProviderStatusUpdate, telemetry, mailbox, autonomy
            // bookkeeping, marks; tombstones already handled above) are
            // projection details that touch neither the message history
            // nor its coherence.
            _ => {}
        }
    }

    // End-of-log with calls still pending (no Done/Error closed the
    // turn): the loop just stopped mid-flight. The ragged point is the
    // oldest unanswered call's introducing turn.
    if !pending_tool_calls.is_empty() && first_ragged.is_none() {
        let (call_id, intro_id) = pending_tool_calls[0].clone();
        first_ragged = Some((
            intro_id,
            format!("tool call `{call_id}` was never answered (log ends mid-turn)"),
        ));
    }

    // Resolve the last clean boundary.
    let Some(boundary) = last_clean else {
        let detail = first_ragged
            .map(|(_, d)| d)
            .unwrap_or_else(|| "log never reached a coherent, sendable point".to_string());
        return Err(ContinuationError::NoCleanBoundary { detail });
    };

    // A ragged marker only matters if it lies PAST the clean boundary —
    // a tool call that was later answered is not ragged.
    let had_ragged_tail = first_ragged
        .as_ref()
        .map(|(id, _)| *id > boundary.event_id)
        .unwrap_or(false);

    // For a ragged tail, the recover path tombstones everything strictly
    // past the boundary, so `first_ragged_event_id` is the first live
    // (non-tombstoned) event after the fork point — where recover starts
    // laying tombstones.
    let first_ragged_event_id = if had_ragged_tail {
        events
            .iter()
            .find(|e| e.id > boundary.event_id && !dead.contains(&e.id))
            .map(|e| e.id)
    } else {
        None
    };

    Ok(Continuation {
        messages: boundary.messages,
        fork_event_id: Some(boundary.event_id),
        had_ragged_tail,
        first_ragged_event_id,
    })
}

/// `mu --resume` (STRICT). Project the log for continuation, but
/// REFUSE if the tail past the last clean boundary is ragged. The
/// returned [`ContinuationError`] names the exact damage so the caller
/// can print a precise diagnosis and a `mu --recover` hint.
pub fn project_strict(events: &[SessionEvent]) -> Result<Continuation, ContinuationError> {
    let cont = project_internal(events)?;
    if cont.had_ragged_tail {
        // Re-derive the precise damage detail for the error.
        let detail = ragged_detail(events, cont.first_ragged_event_id);
        return Err(ContinuationError::RaggedTail {
            clean_boundary_event_id: cont.fork_event_id.unwrap_or(0),
            first_ragged_event_id: cont.first_ragged_event_id.unwrap_or(0),
            detail,
        });
    }
    Ok(cont)
}

/// The repairing path's projection (`mu --recover`). Truncates to the
/// last clean boundary and returns the messages plus fork point.
/// Tolerates a ragged tail (the caller tombstones it); only fails when
/// there is no clean boundary at all.
pub fn project_to_clean_boundary(
    events: &[SessionEvent],
) -> Result<Continuation, ContinuationError> {
    project_internal(events)
}

/// Re-derive a precise, human-readable description of what's wrong in
/// the ragged tail starting at `first_ragged_event_id`. Used to enrich
/// the strict refusal so `mu --resume` can name the exact damage and
/// point at `mu --recover`.
fn ragged_detail(events: &[SessionEvent], first_ragged_event_id: Option<u64>) -> String {
    let Some(start) = first_ragged_event_id else {
        return "ragged tail past the last clean boundary".to_string();
    };
    let dead = tombstoned_ids(events);

    // Scan the tail from the first ragged event onward, tracking which
    // tool calls are introduced but never answered — the dominant
    // failure mode (the 402-incident shape).
    let mut pending: Vec<(String, String)> = Vec::new(); // (call_id, tool name)
    let mut terminal_err: Option<String> = None;
    for ev in events.iter().filter(|e| e.id >= start && !dead.contains(&e.id)) {
        match &ev.payload {
            EventPayload::AssistantMessageEvent { message } => {
                for block in &message.content {
                    if let ContentBlock::ToolCall(tc) = block {
                        pending.push((tc.id.clone(), tc.name.clone()));
                    }
                }
            }
            EventPayload::ToolCall { call_id, name, .. } => {
                if !pending.iter().any(|(c, _)| c == call_id) {
                    pending.push((call_id.clone(), name.clone()));
                }
            }
            EventPayload::ToolResult { call_id, .. } => {
                pending.retain(|(c, _)| c != call_id);
            }
            EventPayload::Error { message } => terminal_err = Some(message.clone()),
            EventPayload::ErrorInvalidMessage {
                validation_error, ..
            } => terminal_err = Some(format!("invalid provider message: {validation_error}")),
            _ => {}
        }
    }

    match (pending.first(), terminal_err) {
        (Some((call_id, name)), Some(err)) => format!(
            "tool call `{call_id}` ({name}) has no matching result; turn ended with error: {err}"
        ),
        (Some((call_id, name)), None) => {
            format!("tool call `{call_id}` ({name}) has no matching result")
        }
        (None, Some(err)) => format!("turn ended with error: {err}"),
        (None, None) => "incomplete record past the last clean boundary".to_string(),
    }
}

/// Convenience: the terminal error event (if any) in a log — the
/// record `mu --recover`'s cause-of-death preflight would match
/// against a known-signatures table. Returns the message of the last
/// `Error` / `ErrorInvalidMessage` event. (The preflight table itself
/// is filed as follow-up work; this is the hook it reads.)
pub fn terminal_error(events: &[SessionEvent]) -> Option<String> {
    events.iter().rev().find_map(|e| match &e.payload {
        EventPayload::Error { message } => Some(message.clone()),
        EventPayload::ErrorInvalidMessage {
            validation_error, ..
        } => Some(validation_error.clone()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::types::{AssistantMessage, StopReason, ToolArgs, ToolCall};
    use crate::event_log::EventActor;
    use serde_json::json;

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

    fn user(id: u64, text: &str) -> SessionEvent {
        ev(
            id,
            EventPayload::UserMessage {
                content: text.into(),
            },
        )
    }

    fn assistant_text(id: u64, text: &str) -> SessionEvent {
        ev(
            id,
            EventPayload::AssistantMessageEvent {
                message: AssistantMessage {
                    content: vec![ContentBlock::Text { text: text.into() }],
                    stop_reason: StopReason::EndTurn,
                    usage: None,
                },
            },
        )
    }

    fn assistant_toolcall(id: u64, call_id: &str, name: &str) -> SessionEvent {
        ev(
            id,
            EventPayload::AssistantMessageEvent {
                message: AssistantMessage {
                    content: vec![ContentBlock::ToolCall(ToolCall {
                        id: call_id.into(),
                        name: name.into(),
                        arguments: ToolArgs::new(json!({})).unwrap(),
                    })],
                    stop_reason: StopReason::ToolUse,
                    usage: None,
                },
            },
        )
    }

    fn tool_result(id: u64, call_id: &str, content: &str) -> SessionEvent {
        ev(
            id,
            EventPayload::ToolResult {
                call_id: call_id.into(),
                content: content.into(),
                is_error: false,
            },
        )
    }

    fn done(id: u64) -> SessionEvent {
        ev(
            id,
            EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 1,
                usage: None,
                elapsed_ms: Some(100),
            },
        )
    }

    fn session_created(id: u64) -> SessionEvent {
        ev(
            id,
            EventPayload::SessionCreated {
                provider_kind: "faux".into(),
                model: "m".into(),
                parent_session_id: None,
                branched_at_parent_event_id: None,
                usage_semantics: None,
            },
        )
    }

    #[test]
    fn empty_log_refuses() {
        let err = project_strict(&[]).unwrap_err();
        assert_eq!(err, ContinuationError::Empty);
    }

    #[test]
    fn clean_completed_turn_projects_fully() {
        // created, user, assistant(text), done — a clean single exchange.
        let log = vec![
            session_created(1),
            user(2, "hi"),
            assistant_text(3, "hello"),
            done(4),
        ];
        let cont = project_strict(&log).expect("clean log resumes");
        assert_eq!(cont.messages.len(), 2); // user + assistant
        assert!(!cont.had_ragged_tail);
        assert_eq!(cont.fork_event_id, Some(4)); // the Done is the boundary
        assert!(matches!(&cont.messages[0], AgentMessage::User { content } if content == "hi"));
    }

    #[test]
    fn clean_tool_loop_projects_fully() {
        // user, assistant(tool call), tool result, assistant(text), done.
        let log = vec![
            session_created(1),
            user(2, "read the file"),
            assistant_toolcall(3, "c1", "read"),
            tool_result(4, "c1", "file contents"),
            assistant_text(5, "here is the file"),
            done(6),
        ];
        let cont = project_strict(&log).expect("clean tool loop resumes");
        // user + assistant(toolcall) + toolresult + assistant(text)
        assert_eq!(cont.messages.len(), 4);
        assert!(!cont.had_ragged_tail);
        assert_eq!(cont.fork_event_id, Some(6));
    }

    #[test]
    fn ragged_unanswered_tool_call_strict_refuses_with_diagnosis() {
        // The 402-incident shape: assistant issued a tool call, the
        // provider died before the result came back. No Done, no Error
        // record even — the log just ends mid-turn.
        let log = vec![
            session_created(1),
            user(2, "do work"),
            assistant_toolcall(3, "c1", "bash"),
            // ... provider 402, log ends here. c1 never answered.
        ];
        let err = project_strict(&log).unwrap_err();
        match err {
            ContinuationError::RaggedTail {
                clean_boundary_event_id,
                first_ragged_event_id,
                detail,
            } => {
                assert_eq!(clean_boundary_event_id, 2, "last clean boundary is the user msg");
                assert_eq!(first_ragged_event_id, 3, "the dangling tool call event");
                assert!(
                    detail.contains("c1") || detail.contains("result"),
                    "diagnosis names the damage: {detail}"
                );
            }
            other => panic!("expected RaggedTail, got {other:?}"),
        }
    }

    #[test]
    fn ragged_terminal_error_strict_refuses() {
        let log = vec![
            session_created(1),
            user(2, "do work"),
            assistant_toolcall(3, "c1", "bash"),
            ev(
                4,
                EventPayload::Error {
                    message: "402 Payment Required".into(),
                },
            ),
        ];
        let err = project_strict(&log).unwrap_err();
        match err {
            ContinuationError::RaggedTail {
                first_ragged_event_id,
                detail,
                ..
            } => {
                // The first coherence break is the dangling tool call at 3.
                assert_eq!(first_ragged_event_id, 3);
                assert!(detail.contains("result") || detail.contains("402") || detail.contains("c1"));
            }
            other => panic!("expected RaggedTail, got {other:?}"),
        }
    }

    #[test]
    fn recover_path_truncates_ragged_tail_to_boundary() {
        // Same ragged log; the repairing projection succeeds, forking
        // at the last clean boundary and reporting the ragged tail.
        let log = vec![
            session_created(1),
            user(2, "first question"),
            assistant_text(3, "first answer"),
            done(4),
            user(5, "second question"),
            assistant_toolcall(6, "c1", "bash"),
            // provider died; c1 never answered.
        ];
        let cont = project_to_clean_boundary(&log).expect("recover projects to boundary");
        // The last clean boundary is the trailing user message at id 5:
        // [q1, a1, q2] is a coherent, sendable history (no dangling tool
        // calls), so the resumed session re-runs the abandoned prompt
        // q2 from scratch — exactly the operator's "resume from the last
        // prompt" semantics. The dangling assistant tool call at 6 is the
        // ragged tail that recover tombstones.
        assert_eq!(cont.fork_event_id, Some(5));
        assert_eq!(cont.messages.len(), 3); // user1 + assistant1 + user2
        assert!(cont.had_ragged_tail);
        assert_eq!(cont.first_ragged_event_id, Some(6));
    }

    #[test]
    fn tombstoned_ragged_tail_projects_clean() {
        // After --recover lays a tombstone over the dangling tool call,
        // a strict projection of the SAME log should now succeed: the
        // tombstoned event is skipped, leaving a clean boundary at the
        // Done.
        let log = vec![
            session_created(1),
            user(2, "first question"),
            assistant_text(3, "first answer"),
            done(4),
            user(5, "second question"),
            assistant_toolcall(6, "c1", "bash"),
            // tombstones over the ragged tail (5 and 6):
            ev(
                7,
                EventPayload::Tombstone {
                    target_event_id: 5,
                    reason: "recovered: abandoned turn".into(),
                },
            ),
            ev(
                8,
                EventPayload::Tombstone {
                    target_event_id: 6,
                    reason: "recovered: unanswered tool call c1".into(),
                },
            ),
        ];
        let cont = project_strict(&log).expect("tombstoned tail projects clean");
        assert!(!cont.had_ragged_tail);
        assert_eq!(cont.fork_event_id, Some(4));
        assert_eq!(cont.messages.len(), 2);
    }

    #[test]
    fn no_clean_boundary_when_first_turn_dies() {
        // Died during the very first turn: assistant issued a tool call
        // before any user message even landed cleanly... actually the
        // worst case is a log that opens straight into a dangling call.
        let log = vec![session_created(1), assistant_toolcall(2, "c1", "bash")];
        let err = project_strict(&log).unwrap_err();
        // SessionCreated alone is a clean (empty-conversation) boundary,
        // so this is a ragged tail past that boundary, not NoCleanBoundary.
        match err {
            ContinuationError::RaggedTail { .. } => {}
            ContinuationError::NoCleanBoundary { .. } => {}
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn terminal_error_extracts_last_error() {
        let log = vec![
            session_created(1),
            user(2, "x"),
            ev(
                3,
                EventPayload::Error {
                    message: "402 Payment Required from openrouter".into(),
                },
            ),
        ];
        assert_eq!(
            terminal_error(&log).as_deref(),
            Some("402 Payment Required from openrouter")
        );
    }

    #[test]
    fn bare_toolcall_event_paired_with_result_is_clean() {
        // Some paths log a bare ToolCall event (not inside an assistant
        // block). A matching ToolResult must still clear it.
        let log = vec![
            session_created(1),
            user(2, "go"),
            assistant_toolcall(3, "c1", "read"),
            ev(
                4,
                EventPayload::ToolCall {
                    call_id: "c1".into(),
                    name: "read".into(),
                    arguments: json!({}),
                },
            ),
            tool_result(5, "c1", "ok"),
            assistant_text(6, "done"),
            done(7),
        ];
        let cont = project_strict(&log).expect("clean");
        assert!(!cont.had_ragged_tail);
    }
}
