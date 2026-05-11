//! Per-session append-only event log.
//!
//! The durable record of what happened in a session. Sessions append
//! significant events (user message arrived, assistant message
//! completed, tool called, tool result returned, ask round-trip
//! finished, error). Streaming-only events (text deltas, lifecycle
//! ticks) do NOT go in the log — they're projection details for the
//! wire layer.
//!
//! v1 is in-memory only. Future work (per architecture doc
//! `specs/architecture/event-sourced-context.md`): JSONL/SQLite
//! persistence, `ContextAssembly` events, `MemoryWrite` events,
//! `Compaction` events, branching/lineage via `parent_event_ids`.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::{AssistantMessage, StopReason, Usage};

/// A single event in a session's durable log.
///
/// Envelope is shared across all kinds; payload is a tagged enum so
/// each kind keeps its own typed shape. See architecture doc for
/// the full design rationale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEvent {
    /// Monotonic per-session id, starting at 1. NOT globally unique.
    pub id: u64,
    pub session_id: String,
    /// Causal links to prior events (e.g. ToolResult points back to
    /// the ToolCall it answers, AssistantMessage points to the
    /// UserMessage it replies to). v1 leaves these mostly empty;
    /// future work populates them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parent_event_ids: Vec<u64>,
    /// Unix milliseconds at append time.
    pub timestamp_unix_ms: u64,
    pub actor: EventActor,
    pub payload: EventPayload,
}

/// Who/what produced the event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventActor {
    User,
    Agent,
    Tool { name: String },
    Provider { name: String },
    System,
}

/// Typed event payload. Common envelope, different shapes per kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventPayload {
    /// Session opened. Records provider+model selection. When the
    /// session is a delegate (born via `session.delegate`), carries
    /// the parent's id and the event in the parent's log this
    /// branched from. Both fields are None for root sessions.
    SessionCreated {
        provider_kind: String,
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branched_at_parent_event_id: Option<u64>,
    },
    /// User-side input message arrived.
    UserMessage { content: String },
    /// Assistant turn completed. Carries text content, tool calls,
    /// stop reason, and per-turn usage (when the provider reports it).
    AssistantMessageEvent { message: AssistantMessage },
    /// A tool was invoked by the agent.
    ToolCall {
        call_id: String,
        name: String,
        arguments: Value,
    },
    /// The tool returned.
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
    /// One `ask_session` round-trip terminated. The aggregated usage
    /// + elapsed_ms here is what was sent on the wire's `session.done`
    /// notification. Summing across all Done events in a session
    /// gives session-cumulative usage.
    Done {
        stop_reason: StopReason,
        turn_count: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        elapsed_ms: Option<u64>,
    },
    /// Terminal error event.
    Error { message: String },
    /// Free-form agent observation (memory recall, status note, etc).
    /// Mirrors the wire-level `session.callout`.
    Callout {
        category: String,
        title: String,
        body: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        theme: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        context_refs: Vec<String>,
    },
    /// Session closed (via `close_session` RPC or daemon shutdown).
    SessionClosed,
}

/// Append-only per-session log.
///
/// Cheap to clone (Arc-wrapped internally). Safe to share across
/// the dispatch loop, forwarder, and tests.
#[derive(Debug)]
pub struct SessionEventLog {
    session_id: String,
    events: Mutex<Vec<SessionEvent>>,
    next_id: AtomicU64,
}

impl SessionEventLog {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            events: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Append an event. Returns the assigned id. Lock poisoning is
    /// silently recovered (the daemon should not crash on a poisoned
    /// log mutex; missing one append is better than a crash).
    pub fn append(&self, actor: EventActor, payload: EventPayload) -> u64 {
        self.append_with_parents(actor, payload, Vec::new())
    }

    pub fn append_with_parents(
        &self,
        actor: EventActor,
        payload: EventPayload,
        parent_event_ids: Vec<u64>,
    ) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let event = SessionEvent {
            id,
            session_id: self.session_id.clone(),
            parent_event_ids,
            timestamp_unix_ms: now_unix_ms(),
            actor,
            payload,
        };
        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        } else {
            tracing::warn!(
                session_id = %self.session_id,
                "event log mutex poisoned; event dropped"
            );
        }
        id
    }

    /// Snapshot the log. Clones the inner vec — safe to read without
    /// holding the lock.
    pub fn snapshot(&self) -> Vec<SessionEvent> {
        self.events
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.events.lock().map(|g| g.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Sum usage across all `Done` events in the log. Returns None if
    /// no Done event ever reported usage (e.g. faux provider, or
    /// every ask hit an error before reporting).
    pub fn cumulative_usage(&self) -> Option<Usage> {
        let Ok(events) = self.events.lock() else {
            return None;
        };
        let mut acc: Option<Usage> = None;
        for ev in events.iter() {
            if let EventPayload::Done { usage: Some(u), .. } = &ev.payload {
                acc = Some(match acc {
                    Some(prev) => prev.add(*u),
                    None => *u,
                });
            }
        }
        acc
    }

    /// Total turns across all asks. Sums `Done.turn_count`.
    pub fn total_turn_count(&self) -> u32 {
        self.events
            .lock()
            .map(|events| {
                events
                    .iter()
                    .filter_map(|e| {
                        if let EventPayload::Done { turn_count, .. } = &e.payload {
                            Some(*turn_count)
                        } else {
                            None
                        }
                    })
                    .sum()
            })
            .unwrap_or(0)
    }

    /// Count of `ask_session` round-trips that terminated. Equals
    /// the number of `Done` events.
    pub fn ask_count(&self) -> u32 {
        self.events
            .lock()
            .map(|events| {
                events
                    .iter()
                    .filter(|e| matches!(e.payload, EventPayload::Done { .. }))
                    .count() as u32
            })
            .unwrap_or(0)
    }

    /// Sum of elapsed_ms across all asks. Excludes asks where the
    /// provider didn't report timing.
    pub fn elapsed_total_ms(&self) -> u64 {
        self.events
            .lock()
            .map(|events| {
                events
                    .iter()
                    .filter_map(|e| {
                        if let EventPayload::Done { elapsed_ms, .. } = &e.payload {
                            *elapsed_ms
                        } else {
                            None
                        }
                    })
                    .sum()
            })
            .unwrap_or(0)
    }

    /// Count of tool invocations.
    pub fn tool_call_count(&self) -> u32 {
        self.events
            .lock()
            .map(|events| {
                events
                    .iter()
                    .filter(|e| matches!(e.payload, EventPayload::ToolCall { .. }))
                    .count() as u32
            })
            .unwrap_or(0)
    }

    /// Timestamp of the first event (typically `SessionCreated`).
    /// None if the log is empty.
    pub fn started_at_unix_ms(&self) -> Option<u64> {
        self.events
            .lock()
            .ok()
            .and_then(|events| events.first().map(|e| e.timestamp_unix_ms))
    }

    /// Timestamp of the most recent event. None if the log is empty.
    pub fn last_activity_unix_ms(&self) -> Option<u64> {
        self.events
            .lock()
            .ok()
            .and_then(|events| events.last().map(|e| e.timestamp_unix_ms))
    }

    /// Pull (provider_kind, model) out of the first SessionCreated
    /// event. None if no such event has been recorded (e.g. log was
    /// constructed manually without going through dispatch).
    pub fn provider_info(&self) -> Option<(String, String)> {
        let events = self.events.lock().ok()?;
        for ev in events.iter() {
            if let EventPayload::SessionCreated {
                provider_kind,
                model,
                ..
            } = &ev.payload
            {
                return Some((provider_kind.clone(), model.clone()));
            }
        }
        None
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{ContentBlock, ToolCall};
    use serde_json::json;

    fn sample_usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            reasoning_tokens: None,
        }
    }

    #[test]
    fn append_assigns_monotonic_ids() {
        let log = SessionEventLog::new("s1");
        let a = log.append(EventActor::User, EventPayload::UserMessage { content: "hi".into() });
        let b = log.append(EventActor::Agent, EventPayload::Done {
            stop_reason: StopReason::EndTurn,
            turn_count: 1,
            usage: None,
            elapsed_ms: None,
        });
        assert_eq!(a, 1);
        assert_eq!(b, 2);
        assert_eq!(log.len(), 2);
    }

    #[test]
    fn cumulative_usage_sums_done_events() {
        let log = SessionEventLog::new("s1");
        // First ask: 100 in, 50 out
        log.append(EventActor::Agent, EventPayload::Done {
            stop_reason: StopReason::EndTurn,
            turn_count: 1,
            usage: Some(sample_usage(100, 50)),
            elapsed_ms: Some(500),
        });
        // Second ask: 200 in, 75 out
        log.append(EventActor::Agent, EventPayload::Done {
            stop_reason: StopReason::EndTurn,
            turn_count: 1,
            usage: Some(sample_usage(200, 75)),
            elapsed_ms: Some(800),
        });
        // Third ask: no usage reported (e.g. provider hiccup)
        log.append(EventActor::Agent, EventPayload::Done {
            stop_reason: StopReason::EndTurn,
            turn_count: 1,
            usage: None,
            elapsed_ms: Some(100),
        });
        let cumulative = log.cumulative_usage().expect("at least one Done had usage");
        assert_eq!(cumulative.input_tokens, 300);
        assert_eq!(cumulative.output_tokens, 125);
        assert_eq!(log.ask_count(), 3);
        assert_eq!(log.elapsed_total_ms(), 500 + 800 + 100);
    }

    #[test]
    fn cumulative_usage_none_when_no_done_reported() {
        let log = SessionEventLog::new("s1");
        log.append(EventActor::User, EventPayload::UserMessage { content: "hi".into() });
        log.append(EventActor::Agent, EventPayload::Done {
            stop_reason: StopReason::EndTurn,
            turn_count: 1,
            usage: None,
            elapsed_ms: None,
        });
        assert!(log.cumulative_usage().is_none());
        assert_eq!(log.ask_count(), 1);
        assert_eq!(log.elapsed_total_ms(), 0);
    }

    #[test]
    fn tool_call_count_includes_only_tool_calls() {
        let log = SessionEventLog::new("s1");
        log.append(EventActor::User, EventPayload::UserMessage { content: "do it".into() });
        log.append(
            EventActor::Agent,
            EventPayload::ToolCall {
                call_id: "c1".into(),
                name: "read".into(),
                arguments: json!({"path": "/x"}),
            },
        );
        log.append(
            EventActor::Tool { name: "read".into() },
            EventPayload::ToolResult {
                call_id: "c1".into(),
                content: "contents".into(),
                is_error: false,
            },
        );
        log.append(
            EventActor::Agent,
            EventPayload::ToolCall {
                call_id: "c2".into(),
                name: "edit".into(),
                arguments: json!({}),
            },
        );
        assert_eq!(log.tool_call_count(), 2);
    }

    #[test]
    fn total_turn_count_sums_done_turn_counts() {
        let log = SessionEventLog::new("s1");
        log.append(
            EventActor::Agent,
            EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 3,
                usage: None,
                elapsed_ms: None,
            },
        );
        log.append(
            EventActor::Agent,
            EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 1,
                usage: None,
                elapsed_ms: None,
            },
        );
        assert_eq!(log.total_turn_count(), 4);
    }

    #[test]
    fn session_event_round_trips() -> Result<(), serde_json::Error> {
        let samples = [
            SessionEvent {
                id: 1,
                session_id: "s1".into(),
                parent_event_ids: vec![],
                timestamp_unix_ms: 1_700_000_000_000,
                actor: EventActor::User,
                payload: EventPayload::UserMessage {
                    content: "hello".into(),
                },
            },
            SessionEvent {
                id: 2,
                session_id: "s1".into(),
                parent_event_ids: vec![1],
                timestamp_unix_ms: 1_700_000_000_500,
                actor: EventActor::Agent,
                payload: EventPayload::AssistantMessageEvent {
                    message: AssistantMessage {
                        content: vec![ContentBlock::Text { text: "hi back".into() }],
                        stop_reason: StopReason::EndTurn,
                        usage: Some(sample_usage(42, 7)),
                    },
                },
            },
            SessionEvent {
                id: 3,
                session_id: "s1".into(),
                parent_event_ids: vec![],
                timestamp_unix_ms: 1_700_000_001_000,
                actor: EventActor::Tool { name: "read".into() },
                payload: EventPayload::ToolResult {
                    call_id: "c1".into(),
                    content: "file contents".into(),
                    is_error: false,
                },
            },
            SessionEvent {
                id: 4,
                session_id: "s1".into(),
                parent_event_ids: vec![],
                timestamp_unix_ms: 1_700_000_002_000,
                actor: EventActor::Agent,
                payload: EventPayload::Done {
                    stop_reason: StopReason::EndTurn,
                    turn_count: 2,
                    usage: Some(sample_usage(100, 50)),
                    elapsed_ms: Some(750),
                },
            },
            SessionEvent {
                id: 5,
                session_id: "s1".into(),
                parent_event_ids: vec![],
                timestamp_unix_ms: 1_700_000_003_000,
                actor: EventActor::System,
                payload: EventPayload::SessionClosed,
            },
        ];

        for ev in samples {
            let value = serde_json::to_value(&ev)?;
            let decoded: SessionEvent = serde_json::from_value(value)?;
            assert_eq!(decoded, ev);
        }
        Ok(())
    }

    #[test]
    fn payload_wire_tags_are_snake_case() -> Result<(), serde_json::Error> {
        let event = SessionEvent {
            id: 1,
            session_id: "s1".into(),
            parent_event_ids: vec![],
            timestamp_unix_ms: 0,
            actor: EventActor::Agent,
            payload: EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 1,
                usage: None,
                elapsed_ms: None,
            },
        };
        let v = serde_json::to_value(&event)?;
        assert_eq!(v["payload"]["kind"], "done");
        assert_eq!(v["actor"]["kind"], "agent");
        Ok(())
    }
}
