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
//! Two projection modes support strict resume now and an explicit repairing
//! command later (bead mu-mh4, CLI-contract comment 2026-06-07):
//!
//!   - [`project_strict`] — `mu --resume`. Refuses a ragged log,
//!     returning a [`ContinuationError`] that names the *exact* damage
//!     (which event id, what's missing) so the caller can print a
//!     precise refusal and preserve the damaged tail for a repairing path.
//!   - [`project_to_clean_boundary`] — the repairing path's projection.
//!     Truncates to the last clean boundary and returns the messages
//!     plus the id of the event it forked at. a future recovery command can lay tombstones over the excluded tail and
//!     resume from here.
//!
//! Both honor the tombstone rule: an event whose id appears in a
//! `Tombstone`'s `target_event_id` is skipped entirely (mu-mh4 tier 3
//! cheap part), so a recovered log projects cleanly.

use std::collections::BTreeSet;

use crate::agent::types::{AgentMessage, ContentBlock};
use crate::event_log::{tombstoned_ids, EventPayload, SessionEvent};

/// Why a strict continuation projection refused. Each variant names
/// the precise damage so `mu --resume`'s refusal can identify the exact
/// record without claiming an unavailable repair command exists.
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
    /// An explicit repairing path may tombstone them.
    RaggedTail {
        /// Event id of the last clean boundary (the fork point a
        /// repairing projection would use).
        clean_boundary_event_id: u64,
        /// Event id of the first ragged event past the boundary.
        first_ragged_event_id: u64,
        /// What's wrong with the tail.
        detail: String,
    },
    /// The log declares itself as a resumed head, but its durable inherited
    /// history is absent. Continuing would silently truncate the conversation.
    MissingContinuationSeed {
        /// Parent named by `HeadAttached`.
        predecessor_session_id: String,
    },
    /// A persisted inherited-history seed is not provider-sendable.
    InvalidContinuationSeed { detail: String },
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
            ContinuationError::MissingContinuationSeed {
                predecessor_session_id,
            } => write!(
                f,
                "resumed head names predecessor `{predecessor_session_id}` but its inherited-history seed is missing"
            ),
            ContinuationError::InvalidContinuationSeed { detail } => {
                write!(f, "inherited-history seed is not provider-sendable: {detail}")
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
    /// resume could not include (the ragged tail). A repairing caller can
    /// tombstone these; strict resume refuses them.
    pub had_ragged_tail: bool,
    /// The id of the first ragged event past the boundary, when
    /// `had_ragged_tail`. Used by a repairing caller to know where to
    /// start laying tombstones.
    pub first_ragged_event_id: Option<u64>,
    /// mu-t4l5e: deferred tools the predecessor loaded (`ToolLoaded`
    /// events, in log order, deduplicated). The resumed session seeds its
    /// loaded set from this so a tool the model already reached stays in
    /// the request. Loads persist across a context clear — the schema is
    /// presentation, not history — and are never unloaded.
    pub loaded_tools: Vec<String>,
}

/// One projected, sendable boundary: the messages up to and including
/// a coherent point, and the event id of that point.
struct Boundary {
    /// Message count at the boundary — an INDEX into the fold's growing
    /// vec, not a clone of it. The old shape cloned the entire message
    /// vector at every clean boundary, which made projection O(n^2) in
    /// conversation length: the 13k-turn incident log took ~100s to
    /// resume, nearly all of it deep-cloning strings (mu-lzkv6 finding).
    /// The final messages are produced by ONE truncate at the end.
    len: usize,
    event_id: u64,
}

fn validate_seed_messages(messages: &[AgentMessage]) -> Result<(), String> {
    let mut pending = BTreeSet::new();
    for message in messages {
        match message {
            AgentMessage::Assistant(assistant) => {
                for block in &assistant.content {
                    if let ContentBlock::ToolCall(call) = block {
                        pending.insert(call.id.clone());
                    }
                }
            }
            AgentMessage::ToolResult { call_id, .. } => {
                if !pending.remove(call_id) {
                    return Err(format!(
                        "seeded tool result for call `{call_id}` has no matching tool call"
                    ));
                }
            }
            AgentMessage::User { .. } => {}
        }
    }
    if let Some(call_id) = pending.into_iter().next() {
        return Err(format!(
            "seeded tool call `{call_id}` has no matching tool result"
        ));
    }
    Ok(())
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
    let mut loaded_tools: Vec<String> = Vec::new();
    let mut saw_conversational_event = false;
    let mut saw_continuation_seed = false;

    // The last coherent snapshot we could resume from.
    let mut last_clean: Option<Boundary> = None;
    // The first event id (and reason) where coherence broke, if any.
    let mut first_ragged: Option<(u64, String)> = None;

    // Capture a clean boundary: a point with no pending tool calls,
    // where the conversation is naturally sendable to a provider.
    let capture = |messages: &[AgentMessage], id: u64, last_clean: &mut Option<Boundary>| {
        *last_clean = Some(Boundary {
            len: messages.len(),
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
            // mu-lzkv6: a clear marker restarts history — everything
            // before it stays on the log (queryable) but is not replayed
            // into a continuation. Pending tool calls cannot straddle a
            // clear (the loop drops them with the history), so the
            // post-clear state is itself a clean boundary.
            EventPayload::ContextCleared { .. } => {
                // Clear-race rule (mirrors the live loop's
                // apply_context_clear): trailing not-yet-answered user
                // messages survive the marker, so an ask accepted just
                // before a clear replays coherently with its post-clear
                // answer.
                let tail_users = messages
                    .iter()
                    .rev()
                    .take_while(|m| matches!(m, AgentMessage::User { .. }))
                    .count();
                messages.drain(..messages.len() - tail_users);
                pending_tool_calls.clear();
                capture(&messages, ev.id, &mut last_clean);
            }
            EventPayload::UserMessage { content } => {
                saw_conversational_event = true;
                messages.push(AgentMessage::User {
                    content: content.clone(),
                });
                if pending_tool_calls.is_empty() {
                    capture(&messages, ev.id, &mut last_clean);
                }
            }
            EventPayload::ContinuationSeeded {
                messages: inherited,
                ..
            } => {
                if saw_conversational_event || saw_continuation_seed {
                    return Err(ContinuationError::InvalidContinuationSeed {
                        detail:
                            "seed must precede all conversational events and appear exactly once"
                                .into(),
                    });
                }
                if let Err(detail) = validate_seed_messages(inherited) {
                    return Err(ContinuationError::InvalidContinuationSeed { detail });
                }
                // A resumed head stores its inherited provider-sendable
                // history as one seed event. Replace (rather than append
                // to) the running projection: this event is emitted before
                // any child-local conversational events and is the exact
                // base the live loop received. Older logs simply lack it.
                messages = inherited.clone();
                saw_continuation_seed = true;
                pending_tool_calls.clear();
                first_ragged = None;
                capture(&messages, ev.id, &mut last_clean);
            }
            EventPayload::AssistantMessageEvent { message } => {
                saw_conversational_event = true;
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
                saw_conversational_event = true;
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
                saw_conversational_event = true;
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
            } if first_ragged.is_none() => {
                first_ragged = Some((
                    ev.id,
                    format!("invalid provider message: {validation_error}"),
                ));
            }
            // mu-t4l5e: a load is session state outside the message
            // history — replayed into the loaded set, never a boundary.
            EventPayload::ToolLoaded { name, .. } if !loaded_tools.iter().any(|n| n == name) => {
                loaded_tools.push(name.clone());
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

    messages.truncate(boundary.len);
    Ok(Continuation {
        messages,
        fork_event_id: Some(boundary.event_id),
        had_ragged_tail,
        first_ragged_event_id,
        loaded_tools,
    })
}

/// `mu --resume` (STRICT). Project the log for continuation, but
/// REFUSE if the tail past the last clean boundary is ragged. The
/// returned [`ContinuationError`] names the exact damage so the caller
/// can print a precise diagnosis without silently truncating.
pub fn project_strict(events: &[SessionEvent]) -> Result<Continuation, ContinuationError> {
    let dead = tombstoned_ids(events);
    let required_seed = events.iter().find_map(|event| {
        if dead.contains(&event.id) {
            return None;
        }
        match &event.payload {
            EventPayload::ContinuationSeeded {
                predecessor_session_id,
                branched_at_event_id,
                ..
            } => Some((
                event.id,
                predecessor_session_id.as_str(),
                *branched_at_event_id,
            )),
            _ => None,
        }
    });
    if let Some((seed_event_id, seeded_predecessor, seeded_branch_event_id)) = required_seed {
        let matching_head = events.iter().any(|event| {
            event.id > seed_event_id
                && !dead.contains(&event.id)
                && matches!(
                    &event.payload,
                    EventPayload::HeadAttached {
                        predecessor_session_id,
                        branched_at_event_id,
                        ..
                    } if predecessor_session_id == seeded_predecessor
                        && branched_at_event_id == &seeded_branch_event_id
                )
        });
        if !matching_head {
            return Err(ContinuationError::InvalidContinuationSeed {
                detail: "seed has no matching later HeadAttached lineage marker".into(),
            });
        }
    }

    let attached_predecessor = events.iter().find_map(|event| {
        if dead.contains(&event.id) {
            return None;
        }
        match &event.payload {
            EventPayload::HeadAttached {
                predecessor_session_id,
                branched_at_event_id,
                ..
            } => Some((
                event.id,
                predecessor_session_id.as_str(),
                *branched_at_event_id,
            )),
            _ => None,
        }
    });
    if let Some((head_attached_event_id, predecessor_session_id, attached_branch_event_id)) =
        attached_predecessor
    {
        let matching_seed = events.iter().any(|event| {
            event.id < head_attached_event_id
                && !dead.contains(&event.id)
                && matches!(
                    &event.payload,
                    EventPayload::ContinuationSeeded {
                        predecessor_session_id: seeded_predecessor,
                        branched_at_event_id: seeded_branch_event_id,
                        ..
                    } if seeded_predecessor == predecessor_session_id
                        && seeded_branch_event_id == &attached_branch_event_id
                )
        });
        if !matching_seed {
            return Err(ContinuationError::MissingContinuationSeed {
                predecessor_session_id: predecessor_session_id.to_string(),
            });
        }
    }

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

/// The repairing path's projection (CLI wiring is tracked separately).
/// Truncates to the last clean boundary and returns the messages plus fork
/// point. It tolerates an ordinary ragged tail, but still refuses an invalid
/// inherited-history seed because projecting past one would fabricate context.
/// The future recovery command must decide explicitly whether to tombstone such
/// a seed, follow predecessor lineage, or stop for operator input.
pub fn project_to_clean_boundary(
    events: &[SessionEvent],
) -> Result<Continuation, ContinuationError> {
    project_internal(events)
}

/// Re-derive a precise, human-readable description of what's wrong in
/// the ragged tail starting at `first_ragged_event_id`. Used to enrich
/// the strict refusal so `mu --resume` can name the exact damage and
/// support a future explicit recovery command.
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
    for ev in events
        .iter()
        .filter(|e| e.id >= start && !dead.contains(&e.id))
    {
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
            // mu-lzkv6: a clear discards any dangling calls with the
            // history — nothing before it can be ragged for the tail scan.
            EventPayload::ContextCleared { .. } => {
                pending.clear();
                terminal_err = None;
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
/// record a future recovery preflight could match
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

    // ── mu-t4l5e: ToolLoaded replays into the loaded set ──

    #[test]
    fn tool_loaded_events_project_into_loaded_tools_and_survive_a_clear() {
        use crate::agent::deferred_tools::{DeferredTools, ToolLoadReason};
        let load = |id: u64, name: &str, reason: ToolLoadReason| {
            ev(
                id,
                EventPayload::ToolLoaded {
                    name: name.into(),
                    reason,
                },
            )
        };
        let events = vec![
            user(1, "watch the build"),
            load(2, "watch", ToolLoadReason::Preselect),
            assistant_text(3, "ok"),
            // A repeat load is one name; loads persist across a clear.
            load(4, "watch", ToolLoadReason::Touch),
            load(5, "mailbox", ToolLoadReason::Touch),
            ev(
                6,
                EventPayload::ContextCleared {
                    reason: "test".into(),
                },
            ),
            user(7, "again"),
            assistant_text(8, "ok"),
        ];
        let c = project_strict(&events).expect("projection");
        assert_eq!(
            c.loaded_tools,
            vec!["watch", "mailbox"],
            "log order, deduplicated, across a clear"
        );
        assert_eq!(c.messages.len(), 2, "loads are not messages");
        // What resume does with the projection: the handle rehydrates.
        let d = DeferredTools::new(
            ["watch", "mailbox", "aws_recon"]
                .into_iter()
                .map(str::to_owned),
        );
        d.seed_loaded(c.loaded_tools);
        assert!(d.is_visible("watch"));
        assert!(d.is_visible("mailbox"));
        assert!(d.is_withheld("aws_recon"));
    }

    /// mu-t4l5e: the SECOND-generation resume. A resumed head's log
    /// carries its inherited loads as `Inherited` rows (written next to
    /// `ContinuationSeeded`), so projecting THAT log recovers them and a
    /// resume of the resumed head keeps the original session's loads.
    #[test]
    fn inherited_loads_project_off_a_resumed_head_so_they_survive_a_second_resume() {
        use crate::agent::deferred_tools::{DeferredTools, ToolLoadReason};
        // Head B's log as `session.resume` writes it: seed events, then
        // B's own exchange. No preselect/touch row of its own.
        let events = vec![
            ev(
                1,
                EventPayload::ContinuationSeeded {
                    predecessor_session_id: "a".into(),
                    branched_at_event_id: Some(9),
                    messages: vec![AgentMessage::User {
                        content: "watch the build".into(),
                    }],
                },
            ),
            ev(
                2,
                EventPayload::HeadAttached {
                    daemon_id: "d1".into(),
                    claimed_actor: "operator".into(),
                    predecessor_session_id: "a".into(),
                    branched_at_event_id: Some(9),
                },
            ),
            ev(
                3,
                EventPayload::ToolLoaded {
                    name: "watch".into(),
                    reason: ToolLoadReason::Inherited,
                },
            ),
            ev(
                4,
                EventPayload::ToolLoaded {
                    name: "mailbox".into(),
                    reason: ToolLoadReason::Inherited,
                },
            ),
            user(5, "and again"),
            assistant_text(6, "ok"),
        ];
        let c = project_strict(&events).expect("projection");
        assert_eq!(
            c.loaded_tools,
            vec!["watch", "mailbox"],
            "head C inherits head A's loads through head B's own rows"
        );
        let d = DeferredTools::new(
            ["watch", "mailbox", "aws_recon"]
                .into_iter()
                .map(str::to_owned),
        );
        d.seed_loaded(c.loaded_tools);
        assert!(d.is_visible("watch"));
        assert!(d.is_visible("mailbox"));
        assert!(d.is_withheld("aws_recon"));
    }

    // ── mu-lzkv6: ContextCleared restarts continuation history ──

    #[test]
    fn context_cleared_restarts_history_at_marker() {
        let events = vec![
            user(1, "old question"),
            assistant_text(2, "old answer"),
            ev(
                3,
                EventPayload::ContextCleared {
                    reason: "test".into(),
                },
            ),
            user(4, "new question"),
            assistant_text(5, "new answer"),
        ];
        let c = project_strict(&events).expect("projection");
        // Only post-clear history is replayed; pre-clear stays on the
        // log but out of the continuation.
        assert_eq!(c.messages.len(), 2, "messages: {:?}", c.messages);
        match &c.messages[0] {
            AgentMessage::User { content } => assert_eq!(content, "new question"),
            other => panic!("expected post-clear user message, got {other:?}"),
        }
    }

    #[test]
    fn context_cleared_preserves_trailing_unanswered_user_messages() {
        // Clear-race: an ask accepted just before the clear must survive
        // it (else the model receives an empty conversation — the vllm
        // 400 observed live). Its post-clear answer then pairs with it.
        let events = vec![
            user(1, "old question"),
            assistant_text(2, "old answer"),
            user(3, "pending question"),
            ev(
                4,
                EventPayload::ContextCleared {
                    reason: "test".into(),
                },
            ),
            assistant_text(5, "fresh answer"),
        ];
        let c = project_strict(&events).expect("projection");
        assert_eq!(c.messages.len(), 2, "messages: {:?}", c.messages);
        match &c.messages[0] {
            AgentMessage::User { content } => assert_eq!(content, "pending question"),
            other => panic!("expected preserved pending question, got {other:?}"),
        }
    }

    #[test]
    fn context_cleared_alone_is_clean_and_empty() {
        let events = vec![
            user(1, "old"),
            assistant_text(2, "answer"),
            ev(
                3,
                EventPayload::ContextCleared {
                    reason: "test".into(),
                },
            ),
        ];
        let c = project_strict(&events).expect("projection");
        assert!(c.messages.is_empty(), "messages: {:?}", c.messages);
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

    fn continuation_seeded(
        id: u64,
        predecessor: &str,
        messages: Vec<AgentMessage>,
    ) -> SessionEvent {
        ev(
            id,
            EventPayload::ContinuationSeeded {
                predecessor_session_id: predecessor.into(),
                branched_at_event_id: Some(42),
                messages,
            },
        )
    }

    fn head_attached(id: u64, predecessor: &str) -> SessionEvent {
        ev(
            id,
            EventPayload::HeadAttached {
                daemon_id: "d1".into(),
                claimed_actor: "operator".into(),
                predecessor_session_id: predecessor.into(),
                branched_at_event_id: Some(42),
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
    fn legacy_resumed_head_without_seed_fails_closed() {
        let log = vec![
            session_created(1),
            head_attached(2, "predecessor"),
            user(3, "child-local question"),
            assistant_text(4, "child-local answer"),
            done(5),
        ];

        let err = project_strict(&log).expect_err("legacy head cannot prove inherited history");
        assert_eq!(
            err,
            ContinuationError::MissingContinuationSeed {
                predecessor_session_id: "predecessor".into(),
            }
        );
    }

    #[test]
    fn resumed_head_with_seed_after_attachment_fails_closed() {
        let log = vec![
            session_created(1),
            head_attached(2, "predecessor"),
            continuation_seeded(3, "predecessor", vec![]),
            user(4, "child-local question"),
            assistant_text(5, "child-local answer"),
            done(6),
        ];

        let err = project_strict(&log).expect_err("late inherited history must refuse");
        assert!(matches!(
            err,
            ContinuationError::InvalidContinuationSeed { .. }
        ));
    }

    #[test]
    fn resumed_head_with_mismatched_seed_fails_closed() {
        let log = vec![
            session_created(1),
            continuation_seeded(2, "wrong-predecessor", vec![]),
            head_attached(3, "predecessor"),
            done(4),
        ];

        let err = project_strict(&log).expect_err("mismatched inherited history must refuse");
        assert!(matches!(
            err,
            ContinuationError::InvalidContinuationSeed { .. }
        ));
    }

    #[test]
    fn repair_projection_can_inspect_seed_required_head_without_seed() {
        let log = vec![
            session_created(1),
            head_attached(2, "predecessor"),
            user(3, "child-local question"),
            assistant_text(4, "child-local answer"),
            done(5),
        ];

        let cont = project_to_clean_boundary(&log)
            .expect("repair projection can inspect a partial seed-required head");
        assert_eq!(cont.messages.len(), 2);
        assert_eq!(cont.fork_event_id, Some(5));
    }

    #[test]
    fn orphan_continuation_seed_refuses_strict_projection() {
        let log = vec![
            session_created(1),
            continuation_seeded(2, "predecessor", vec![]),
            done(3),
        ];
        let err = project_strict(&log).expect_err("orphan seed must not establish lineage");
        assert!(matches!(
            err,
            ContinuationError::InvalidContinuationSeed { .. }
        ));
    }

    #[test]
    fn continuation_seed_after_conversation_refuses() {
        let log = vec![
            session_created(1),
            user(2, "local-before-seed"),
            continuation_seeded(3, "predecessor", vec![]),
        ];
        let err = project_strict(&log).expect_err("late seed must not rewrite history");
        assert!(matches!(
            err,
            ContinuationError::InvalidContinuationSeed { .. }
        ));
    }

    #[test]
    fn continuation_seed_with_dangling_tool_call_refuses() {
        let inherited = vec![AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: "dangling".into(),
                name: "read".into(),
                arguments: ToolArgs::new(json!({})).expect("valid args"),
            })],
            stop_reason: StopReason::ToolUse,
            usage: None,
        })];
        let log = vec![
            session_created(1),
            continuation_seeded(2, "predecessor", inherited),
            head_attached(3, "predecessor"),
        ];

        let err = project_strict(&log).expect_err("ragged seed must refuse");
        assert!(matches!(
            err,
            ContinuationError::InvalidContinuationSeed { .. }
        ));
    }

    #[test]
    fn resumed_head_persists_inherited_history_for_transitive_resume() {
        let inherited = vec![
            AgentMessage::User {
                content: "first question".into(),
            },
            AgentMessage::Assistant(AssistantMessage {
                content: vec![ContentBlock::Text {
                    text: "first answer".into(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: None,
            }),
        ];
        let log = vec![
            session_created(1),
            continuation_seeded(2, "predecessor", inherited.clone()),
            head_attached(3, "predecessor"),
            user(4, "second question"),
            assistant_text(5, "second answer"),
            done(6),
        ];

        let cont = project_strict(&log).expect("resumed head can itself resume");
        assert_eq!(cont.messages.len(), 4);
        assert_eq!(&cont.messages[..2], inherited.as_slice());
        assert_eq!(cont.fork_event_id, Some(6));
    }

    #[test]
    fn continuation_seed_after_local_projection_refuses() {
        let inherited = vec![AgentMessage::User {
            content: "inherited".into(),
        }];
        let log = vec![
            session_created(1),
            user(2, "must not be discarded"),
            continuation_seeded(3, "predecessor", inherited),
            done(4),
        ];

        let err = project_strict(&log).expect_err("late seed cannot replace local history");
        assert!(matches!(
            err,
            ContinuationError::InvalidContinuationSeed { .. }
        ));
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
                assert_eq!(
                    clean_boundary_event_id, 2,
                    "last clean boundary is the user msg"
                );
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
                assert!(
                    detail.contains("result") || detail.contains("402") || detail.contains("c1")
                );
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
        // After a repairing caller lays a tombstone over the dangling tool call,
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
