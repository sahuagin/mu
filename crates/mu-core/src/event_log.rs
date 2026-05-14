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

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
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
    /// and elapsed_ms here are what was sent on the wire's `session.done`
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
    /// Record of the prompt assembled for a provider call (mu-032).
    /// Emitted BEFORE `provider.stream()`. The agent loop records
    /// what was about to be sent so postmortem analysis can answer
    /// "what did the model see right before this?"
    ///
    /// v1 records counts + provider info. Per-message source-event
    /// mapping (the full source-map vision from
    /// specs/architecture/event-sourced-context.md) is reconstructable
    /// from the log by walking events of the relevant roles; an
    /// explicit per-segment field is reserved for v2.
    ContextAssembly {
        /// Monotonic counter, unique within this session. Links a
        /// ContextAssembly to the subsequent AssistantMessage/Done
        /// for this same model call.
        model_call_id: u32,
        message_count: u32,
        user_message_count: u32,
        assistant_message_count: u32,
        tool_result_count: u32,
        /// Number of tool specs in the request.
        tool_count: u32,
        /// Token count estimate, when available. v1 leaves None
        /// (no tokenizer wired); future provider-specific hooks
        /// can populate.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_count_estimate: Option<u64>,
        /// Provider + model from the session's selector.
        provider_kind: String,
        model: String,
    },
    /// mu-036: autonomous loop iteration began. `iteration` is
    /// 0-indexed across the run; `motivation` is the model-reported
    /// one-sentence "what I'm doing this turn and why" (after a
    /// schedule_wakeup, this is the wake reason).
    AutonomousIterationStarted { iteration: u32, motivation: String },
    /// mu-036: autonomous loop iteration ended. Outcome tells the
    /// caller whether the loop continues, exits, escalates, or errors.
    AutonomousIterationCompleted {
        iteration: u32,
        outcome: crate::protocol::AutonomousIterationOutcome,
    },
    /// mu-036: session has been parked via session.schedule_wakeup.
    /// While sleeping, no provider calls fire (INV-5). On wake, the
    /// next `AutonomousIterationStarted` carries `reason` as its
    /// motivation.
    AutonomousScheduledWakeup {
        wake_at_unix_ms: u64,
        reason: String,
    },
    /// mu-036: autonomous loop terminated. Always the final autonomy
    /// event for this run (INV-7); session returns to RunMode::Idle
    /// and is addressable via ask_session again.
    AutonomousTerminated {
        reason: crate::protocol::AutonomousTerminationReason,
    },
    /// Durable mirror of the wire-side `session.provider_status`
    /// notification (mu-035). Emitted on state transitions and on
    /// periodic ticks during non-streaming waits, both for
    /// observability (live TUI / firehose) and for post-hoc
    /// aggregation (mu-pex: TTFT = AwaitingFirstToken→Streaming gap,
    /// streaming_ms = time in Streaming per call). Field shape
    /// mirrors `crate::protocol::ProviderStatusEvent` minus
    /// `session_id` — the log already knows its session.
    ProviderStatusUpdate {
        state: crate::protocol::ProviderStatusKind,
        /// Unix milliseconds the session entered this state.
        started_at_unix_ms: u64,
        /// Milliseconds since `started_at_unix_ms` at emit time.
        /// Periodic ticks during a long wait re-emit with this
        /// value advancing; transitions emit with elapsed_ms = 0.
        elapsed_ms: u64,
        /// Cumulative bytes from the provider's SSE stream so far.
        /// None when not meaningful (Idle, pre-first-byte
        /// AwaitingFirstToken, providers that don't surface counts).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bytes_received: Option<u64>,
        /// Set only when `state` is ToolExecuting or AwaitingToolResult.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
    },
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
    /// Optional on-disk JSONL writer (mu-upb). When set, every
    /// append() also writes the encoded event as one line. IO
    /// failures are logged but never block the in-memory append —
    /// disk persistence is best-effort, not load-bearing.
    disk_writer: Mutex<Option<File>>,
}

impl SessionEventLog {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            events: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            disk_writer: Mutex::new(None),
        }
    }

    /// mu-935: rebuild a SessionEventLog from a JSONL file previously
    /// written by `attach_disk_writer` (mu-upb's path). Used by the
    /// FileBackend discovery layer to read peer daemons' sessions
    /// off disk, and will be used by mu-mh4 (session persistence
    /// across daemon restart) when that lands.
    ///
    /// The returned log is in-memory only — no disk writer attached.
    /// `next_id` is set to `max(existing_id) + 1` so future appends
    /// don't collide. Malformed lines are skipped with a counter
    /// returned so callers can surface "we recovered N events,
    /// skipped M malformed ones." `session_id` is taken from the
    /// first event; if the file has no events, falls back to the
    /// filename stem.
    pub fn from_jsonl(path: &std::path::Path) -> std::io::Result<(Self, usize)> {
        use std::io::BufRead;

        let file = std::fs::File::open(path)?;
        let reader = std::io::BufReader::new(file);
        let mut events: Vec<SessionEvent> = Vec::new();
        let mut malformed: usize = 0;
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => {
                    malformed = malformed.saturating_add(1);
                    continue;
                }
            };
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionEvent>(&line) {
                Ok(ev) => events.push(ev),
                Err(_) => malformed = malformed.saturating_add(1),
            }
        }
        let session_id = events
            .first()
            .map(|e| e.session_id.clone())
            .unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("recovered")
                    .to_string()
            });
        let next_id = events
            .iter()
            .map(|e| e.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        Ok((
            Self {
                session_id,
                events: Mutex::new(events),
                next_id: AtomicU64::new(next_id),
                disk_writer: Mutex::new(None),
            },
            malformed,
        ))
    }

    /// Attach an on-disk JSONL writer (mu-upb). Creates the parent
    /// directories if needed and opens the file in append mode.
    ///
    /// Returns the path that was opened on success (useful for
    /// logging "events going to /path/to/file.jsonl"). On error,
    /// the writer stays None and append() continues in-memory only.
    pub fn attach_disk_writer(&self, path: &std::path::Path) -> std::io::Result<PathBuf> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let mut guard = self
            .disk_writer
            .lock()
            .map_err(|_| std::io::Error::other("disk_writer mutex poisoned"))?;
        *guard = Some(file);
        Ok(path.to_path_buf())
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
        // mu-upb: best-effort JSONL write before the in-memory push
        // so the disk record is at least as complete as memory. IO
        // failures are logged and ignored — disk persistence is not
        // load-bearing for the running daemon.
        self.write_to_disk(&event);
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

    fn write_to_disk(&self, event: &SessionEvent) {
        let Ok(mut guard) = self.disk_writer.lock() else {
            return;
        };
        let Some(file) = guard.as_mut() else {
            return;
        };
        match serde_json::to_string(event) {
            Ok(line) => {
                if let Err(e) = writeln!(file, "{line}") {
                    tracing::warn!(
                        session_id = %self.session_id,
                        error = %e,
                        "disk write failed; continuing in-memory only"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    session_id = %self.session_id,
                    error = %e,
                    "event serialization failed for disk write"
                );
            }
        }
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
                    Some(prev) => prev + *u,
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

    /// Count of `ContextAssembly` events — equals the number of
    /// provider calls made during this session. Each model call has
    /// exactly one ContextAssembly (emitted before the call).
    pub fn context_assembly_count(&self) -> u32 {
        self.events
            .lock()
            .map(|events| {
                events
                    .iter()
                    .filter(|e| matches!(e.payload, EventPayload::ContextAssembly { .. }))
                    .count() as u32
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
    use crate::agent::ContentBlock;
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
        let a = log.append(
            EventActor::User,
            EventPayload::UserMessage {
                content: "hi".into(),
            },
        );
        let b = log.append(
            EventActor::Agent,
            EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 1,
                usage: None,
                elapsed_ms: None,
            },
        );
        assert_eq!(a, 1);
        assert_eq!(b, 2);
        assert_eq!(log.len(), 2);
    }

    #[test]
    fn cumulative_usage_sums_done_events() {
        let log = SessionEventLog::new("s1");
        // First ask: 100 in, 50 out
        log.append(
            EventActor::Agent,
            EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 1,
                usage: Some(sample_usage(100, 50)),
                elapsed_ms: Some(500),
            },
        );
        // Second ask: 200 in, 75 out
        log.append(
            EventActor::Agent,
            EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 1,
                usage: Some(sample_usage(200, 75)),
                elapsed_ms: Some(800),
            },
        );
        // Third ask: no usage reported (e.g. provider hiccup)
        log.append(
            EventActor::Agent,
            EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 1,
                usage: None,
                elapsed_ms: Some(100),
            },
        );
        let cumulative = log.cumulative_usage().expect("at least one Done had usage");
        assert_eq!(cumulative.input_tokens, 300);
        assert_eq!(cumulative.output_tokens, 125);
        assert_eq!(log.ask_count(), 3);
        assert_eq!(log.elapsed_total_ms(), 500 + 800 + 100);
    }

    #[test]
    fn cumulative_usage_none_when_no_done_reported() {
        let log = SessionEventLog::new("s1");
        log.append(
            EventActor::User,
            EventPayload::UserMessage {
                content: "hi".into(),
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
        assert!(log.cumulative_usage().is_none());
        assert_eq!(log.ask_count(), 1);
        assert_eq!(log.elapsed_total_ms(), 0);
    }

    #[test]
    fn context_assembly_count_filters_to_assembly_events() {
        let log = SessionEventLog::new("ca-count");
        log.append(
            EventActor::User,
            EventPayload::UserMessage {
                content: "hi".into(),
            },
        );
        log.append(
            EventActor::System,
            EventPayload::ContextAssembly {
                model_call_id: 1,
                message_count: 1,
                user_message_count: 1,
                assistant_message_count: 0,
                tool_result_count: 0,
                tool_count: 0,
                token_count_estimate: None,
                provider_kind: "anthropic_api".into(),
                model: "x".into(),
            },
        );
        log.append(
            EventActor::Agent,
            EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 1,
                usage: None,
                elapsed_ms: Some(100),
            },
        );
        log.append(
            EventActor::System,
            EventPayload::ContextAssembly {
                model_call_id: 2,
                message_count: 3,
                user_message_count: 2,
                assistant_message_count: 1,
                tool_result_count: 0,
                tool_count: 0,
                token_count_estimate: None,
                provider_kind: "anthropic_api".into(),
                model: "x".into(),
            },
        );
        assert_eq!(log.context_assembly_count(), 2);
        // Unaffected derivations.
        assert_eq!(log.ask_count(), 1);
    }

    #[test]
    fn context_assembly_payload_round_trips() -> Result<(), serde_json::Error> {
        let ev = SessionEvent {
            id: 1,
            session_id: "s".into(),
            parent_event_ids: vec![],
            timestamp_unix_ms: 0,
            actor: EventActor::System,
            payload: EventPayload::ContextAssembly {
                model_call_id: 7,
                message_count: 5,
                user_message_count: 2,
                assistant_message_count: 2,
                tool_result_count: 1,
                tool_count: 3,
                token_count_estimate: Some(2048),
                provider_kind: "openai_codex".into(),
                model: "gpt-5.5".into(),
            },
        };
        let v = serde_json::to_value(&ev)?;
        assert_eq!(v["payload"]["kind"], "context_assembly");
        assert_eq!(v["payload"]["message_count"], 5);
        let decoded: SessionEvent = serde_json::from_value(v)?;
        assert_eq!(decoded, ev);
        Ok(())
    }

    #[test]
    fn tool_call_count_includes_only_tool_calls() {
        let log = SessionEventLog::new("s1");
        log.append(
            EventActor::User,
            EventPayload::UserMessage {
                content: "do it".into(),
            },
        );
        log.append(
            EventActor::Agent,
            EventPayload::ToolCall {
                call_id: "c1".into(),
                name: "read".into(),
                arguments: json!({"path": "/x"}),
            },
        );
        log.append(
            EventActor::Tool {
                name: "read".into(),
            },
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
                        content: vec![ContentBlock::Text {
                            text: "hi back".into(),
                        }],
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
                actor: EventActor::Tool {
                    name: "read".into(),
                },
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
