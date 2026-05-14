//! Translate `AgentEvent`s from the loop into `session.*` JSON-RPC
//! notifications.
//!
//! ## Why this is split
//!
//! `translate_event` is a pure function: given a session id and an
//! AgentEvent, it returns either `None` (silently-dropped lifecycle
//! event) or a `(method_name, params_value)` pair that's ready to
//! emit. Tests target this function directly without spawning
//! anything — the queue topology test is separate (in serve_smoke).
//!
//! `forward_events` is the IO loop: queue read → translate → emit
//! via `NotificationWriter`. Thin glue.
//!
//! Lifecycle events (AgentStart, TurnStart, TurnEnd, MessageStart,
//! MessageEnd) translate to `None` — mu-001's notification surface
//! doesn't include them. We can amend mu-001 to add them when a
//! frontend needs them.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;

use mu_core::agent::{AgentEvent, AgentMessage};
use mu_core::event_log::{EventActor, EventPayload, SessionEventLog};
use mu_core::protocol::{
    AutonomousIterationCompletedEvent, AutonomousIterationStartedEvent, AutonomousTerminatedEvent,
    CalloutBody, CalloutEvent, DoneEvent, ErrorEvent, InputRequiredEvent, ProviderStatusEvent,
    TextDeltaEvent, ToolCallCompletedEvent, ToolCallStartedEvent, ToolOutcome,
};
use mu_core::transport::NotificationWriter;

/// Pure translation: AgentEvent → (method_name, params_value), or
/// None for events that don't have a wire-level representation in
/// mu-001 yet (lifecycle).
///
/// Returns None on `serde_json::to_value` failure too. In practice
/// the wire types are all `Serialize`-derived structs that can't
/// fail; the early return guards against any future struct gaining
/// a custom Serialize impl that errors.
pub fn translate_event(session_id: &str, event: AgentEvent) -> Option<(&'static str, Value)> {
    match event {
        AgentEvent::TextDelta { delta } => to_pair(
            TextDeltaEvent::METHOD,
            TextDeltaEvent {
                session_id: session_id.to_string(),
                delta,
            },
        ),
        AgentEvent::ToolCallStarted {
            tool_call_id,
            tool_name,
            arguments,
        } => to_pair(
            ToolCallStartedEvent::METHOD,
            ToolCallStartedEvent {
                session_id: session_id.to_string(),
                tool_call_id,
                tool_name,
                arguments,
            },
        ),
        AgentEvent::ToolCallCompleted {
            tool_call_id,
            content,
            is_error,
        } => {
            let outcome = if is_error {
                ToolOutcome::Err { message: content }
            } else {
                // v1 wraps the textual result as a JSON string.
                // Real Provider impls will eventually produce
                // structured results; that's a future spec.
                ToolOutcome::Ok {
                    result: Value::String(content),
                }
            };
            to_pair(
                ToolCallCompletedEvent::METHOD,
                ToolCallCompletedEvent {
                    session_id: session_id.to_string(),
                    tool_call_id,
                    outcome,
                },
            )
        }
        AgentEvent::Done {
            stop_reason,
            usage,
            elapsed_ms,
            ..
        } => to_pair(
            DoneEvent::METHOD,
            DoneEvent {
                session_id: session_id.to_string(),
                stop_reason,
                usage,
                elapsed_ms,
            },
        ),
        AgentEvent::Error { message } => to_pair(
            ErrorEvent::METHOD,
            ErrorEvent {
                session_id: session_id.to_string(),
                message,
                detail: None,
            },
        ),
        AgentEvent::InputRequired {
            request_id,
            tool_call_id,
            tool_name,
            arguments,
            summary,
        } => to_pair(
            InputRequiredEvent::METHOD,
            InputRequiredEvent {
                session_id: session_id.to_string(),
                request_id,
                tool_call_id,
                tool_name,
                arguments,
                summary,
            },
        ),
        AgentEvent::Callout {
            category,
            title,
            body,
            theme,
            context_refs,
        } => {
            // Coerce the agent-loop-side Value into the wire-side
            // CalloutBody enum. JSON strings collapse to Text;
            // anything else stays Structured. The agent-loop's
            // `category` becomes the wire's `kind` (the agent-loop
            // already uses `kind` as the AgentEvent serde tag).
            let body_payload = match body {
                Value::String(s) => CalloutBody::Text(s),
                other => CalloutBody::Structured(other),
            };
            to_pair(
                CalloutEvent::METHOD,
                CalloutEvent {
                    session_id: session_id.to_string(),
                    kind: category,
                    title,
                    body: body_payload,
                    theme,
                    context_refs,
                },
            )
        }
        AgentEvent::ProviderStatus {
            state,
            started_at_unix_ms,
            elapsed_ms,
            bytes_received,
            tool_call_id,
        } => to_pair(
            "session.provider_status",
            ProviderStatusEvent {
                session_id: session_id.to_string(),
                kind: state,
                started_at_unix_ms,
                elapsed_ms,
                bytes_received,
                tool_call_id,
            },
        ),
        AgentEvent::AutonomousIterationStarted {
            iteration,
            motivation,
        } => to_pair(
            AutonomousIterationStartedEvent::METHOD,
            AutonomousIterationStartedEvent {
                session_id: session_id.to_string(),
                iteration,
                motivation,
            },
        ),
        AgentEvent::AutonomousIterationCompleted { iteration, outcome } => to_pair(
            AutonomousIterationCompletedEvent::METHOD,
            AutonomousIterationCompletedEvent {
                session_id: session_id.to_string(),
                iteration,
                outcome,
            },
        ),
        AgentEvent::AutonomousTerminated { reason } => to_pair(
            AutonomousTerminatedEvent::METHOD,
            AutonomousTerminatedEvent {
                session_id: session_id.to_string(),
                reason,
            },
        ),
        // Lifecycle events not in mu-001's notification surface.
        // ContextAssembly lands only in the durable event log
        // (mu-032 v1); wire-level exposure is a future TUI/web-ui
        // feature when there's a consumer.
        AgentEvent::AgentStart
        | AgentEvent::TurnStart
        | AgentEvent::TurnEnd
        | AgentEvent::MessageStart { .. }
        | AgentEvent::MessageEnd { .. }
        | AgentEvent::ContextAssembly { .. } => None,
    }
}

fn to_pair<T: serde::Serialize>(method: &'static str, value: T) -> Option<(&'static str, Value)> {
    serde_json::to_value(value).ok().map(|v| (method, v))
}

/// IO loop: read events from `events_rx`, append durable events to
/// `event_log`, and emit wire notifications via `notif`. Exits when
/// `events_rx` closes.
///
/// The wire projection (notifications) and the durable projection
/// (event log) are derived from the same `AgentEvent` source. They
/// don't share rows — the wire wants per-delta granularity; the log
/// wants per-significant-event granularity.
pub async fn forward_events(
    session_id: String,
    mut events_rx: mpsc::Receiver<AgentEvent>,
    notif: NotificationWriter,
    event_log: Arc<SessionEventLog>,
) {
    while let Some(event) = events_rx.recv().await {
        // Durable projection: append significant events to the log.
        // Streaming deltas + lifecycle ticks are dropped.
        if let Some((actor, mut payload)) = to_log_event(&event) {
            // ContextAssembly payload doesn't know the session's
            // provider/model when produced (the AgentLoop has the
            // info but doesn't pass it through). Patch it in here
            // by reading the SessionCreated event we already
            // recorded at session-start.
            if let EventPayload::ContextAssembly {
                provider_kind,
                model,
                ..
            } = &mut payload
            {
                if let Some((kind, m)) = event_log.provider_info() {
                    *provider_kind = kind;
                    *model = m;
                }
            }
            event_log.append(actor, payload);
        }
        // Wire projection: translate to mu-001 notification surface.
        if let Some((method, params)) = translate_event(&session_id, event) {
            let _ = notif.emit(method, params).await;
        }
    }
}

/// Translate an `AgentEvent` into a durable-log entry, or None if the
/// event isn't worth recording (text deltas, lifecycle ticks). Kept
/// pure for testability.
pub(crate) fn to_log_event(event: &AgentEvent) -> Option<(EventActor, EventPayload)> {
    match event {
        AgentEvent::MessageEnd { message } => match message {
            AgentMessage::User { content } => Some((
                EventActor::User,
                EventPayload::UserMessage {
                    content: content.clone(),
                },
            )),
            AgentMessage::Assistant(am) => Some((
                EventActor::Agent,
                EventPayload::AssistantMessageEvent {
                    message: am.clone(),
                },
            )),
            AgentMessage::ToolResult { .. } => {
                // Tool results are recorded via the dedicated
                // ToolCallCompleted event; skip the duplicate.
                None
            }
        },
        AgentEvent::ToolCallStarted {
            tool_call_id,
            tool_name,
            arguments,
        } => Some((
            EventActor::Agent,
            EventPayload::ToolCall {
                call_id: tool_call_id.clone(),
                name: tool_name.clone(),
                arguments: arguments.clone(),
            },
        )),
        AgentEvent::ToolCallCompleted {
            tool_call_id,
            content,
            is_error,
        } => Some((
            EventActor::Tool {
                name: "unknown".to_owned(), // tool name not on the event; future hardening
            },
            EventPayload::ToolResult {
                call_id: tool_call_id.clone(),
                content: content.clone(),
                is_error: *is_error,
            },
        )),
        AgentEvent::Done {
            stop_reason,
            turn_count,
            usage,
            elapsed_ms,
        } => Some((
            EventActor::Agent,
            EventPayload::Done {
                stop_reason: *stop_reason,
                turn_count: *turn_count,
                usage: *usage,
                elapsed_ms: *elapsed_ms,
            },
        )),
        AgentEvent::Error { message } => Some((
            EventActor::Agent,
            EventPayload::Error {
                message: message.clone(),
            },
        )),
        AgentEvent::Callout {
            category,
            title,
            body,
            theme,
            context_refs,
        } => Some((
            EventActor::Agent,
            EventPayload::Callout {
                category: category.clone(),
                title: title.clone(),
                body: body.clone(),
                theme: theme.clone(),
                context_refs: context_refs.clone(),
            },
        )),
        AgentEvent::ContextAssembly {
            model_call_id,
            message_count,
            user_message_count,
            assistant_message_count,
            tool_result_count,
            tool_count,
            renderer,
            cache_strategy,
            span_count,
            cache_boundary_count,
            first_span_ids,
        } => {
            // ContextAssembly is recorded with the session's
            // provider/model from the log's SessionCreated event.
            // We need access to the log here to look that up; the
            // forwarder receives `event_log` as a parameter so we
            // could pass it down — but to_log_event is a pure
            // function for testability. Solution: the caller
            // (forward_events loop) injects the provider info into
            // the payload right before append. Here we just emit a
            // "placeholder" payload; the actual log append happens
            // in forward_events with provider info filled in.
            //
            // For the wire-level projection, we also need to emit
            // a notification — that happens via translate_event
            // which doesn't go through to_log_event. So the wire
            // surface is separate.
            //
            // Encode as ContextAssembly with empty provider info;
            // forward_events will replace before storing.
            Some((
                EventActor::System,
                EventPayload::ContextAssembly {
                    model_call_id: *model_call_id,
                    message_count: *message_count,
                    user_message_count: *user_message_count,
                    assistant_message_count: *assistant_message_count,
                    tool_result_count: *tool_result_count,
                    tool_count: *tool_count,
                    token_count_estimate: None,
                    provider_kind: String::new(),
                    model: String::new(),
                    renderer: renderer.clone(),
                    cache_strategy: cache_strategy.clone(),
                    span_count: *span_count,
                    cache_boundary_count: *cache_boundary_count,
                    first_span_ids: first_span_ids.clone(),
                },
            ))
        }
        AgentEvent::ProviderStatus {
            state,
            started_at_unix_ms,
            elapsed_ms,
            bytes_received,
            tool_call_id,
        } => Some((
            EventActor::System,
            EventPayload::ProviderStatusUpdate {
                state: *state,
                started_at_unix_ms: *started_at_unix_ms,
                elapsed_ms: *elapsed_ms,
                bytes_received: *bytes_received,
                tool_call_id: tool_call_id.clone(),
            },
        )),
        AgentEvent::AutonomousIterationStarted { iteration, motivation } => Some((
            EventActor::Agent,
            EventPayload::AutonomousIterationStarted {
                iteration: *iteration,
                motivation: motivation.clone(),
            },
        )),
        AgentEvent::AutonomousIterationCompleted { iteration, outcome } => Some((
            EventActor::Agent,
            EventPayload::AutonomousIterationCompleted {
                iteration: *iteration,
                outcome: outcome.clone(),
            },
        )),
        AgentEvent::AutonomousTerminated { reason } => Some((
            EventActor::Agent,
            EventPayload::AutonomousTerminated {
                reason: reason.clone(),
            },
        )),
        AgentEvent::TextDelta { .. }
        | AgentEvent::MessageStart { .. }
        | AgentEvent::AgentStart
        | AgentEvent::TurnStart
        | AgentEvent::TurnEnd
        // InputRequired is a transient wire-level prompt; the
        // resulting ToolCall (approved) or ToolResult (denied)
        // already lands in the log.
        | AgentEvent::InputRequired { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use mu_core::agent::AgentEvent;

    #[test]
    fn translate_text_delta() {
        let (method, params) = translate_event("s1", AgentEvent::TextDelta { delta: "hi".into() })
            .expect("expected Some");
        assert_eq!(method, "session.text_delta");
        assert_eq!(params["session_id"], "s1");
        assert_eq!(params["delta"], "hi");
    }

    #[test]
    fn translate_callout_text_body() {
        let (method, params) = translate_event(
            "s2",
            AgentEvent::Callout {
                category: "observation".into(),
                title: "spotted".into(),
                body: json!("line 5"),
                theme: Some("info".into()),
                context_refs: vec!["spec:mu-016".into()],
            },
        )
        .expect("expected Some");
        assert_eq!(method, "session.callout");
        assert_eq!(params["kind"], "observation");
        assert_eq!(params["title"], "spotted");
        assert_eq!(params["body"], "line 5");
        assert_eq!(params["theme"], "info");
        assert_eq!(params["context_refs"][0], "spec:mu-016");
    }

    #[test]
    fn translate_callout_structured_body() {
        let (method, params) = translate_event(
            "s3",
            AgentEvent::Callout {
                category: "memory".into(),
                title: "recalled".into(),
                body: json!({"id": "abc", "preview": "..."}),
                theme: None,
                context_refs: vec![],
            },
        )
        .expect("expected Some");
        assert_eq!(method, "session.callout");
        assert_eq!(params["body"]["id"], "abc");
        // Optionals omitted when empty/None.
        assert!(params.as_object().unwrap().get("theme").is_none());
        assert!(params.as_object().unwrap().get("context_refs").is_none());
    }

    #[test]
    fn translate_tool_call_completed_ok() {
        let (method, params) = translate_event(
            "s",
            AgentEvent::ToolCallCompleted {
                tool_call_id: "t1".into(),
                content: "result".into(),
                is_error: false,
            },
        )
        .expect("expected Some");
        assert_eq!(method, "session.tool_call_completed");
        assert_eq!(params["outcome"]["kind"], "ok");
        assert_eq!(params["outcome"]["result"], "result");
    }

    #[test]
    fn translate_tool_call_completed_err() {
        let (_, params) = translate_event(
            "s",
            AgentEvent::ToolCallCompleted {
                tool_call_id: "t1".into(),
                content: "boom".into(),
                is_error: true,
            },
        )
        .expect("expected Some");
        assert_eq!(params["outcome"]["kind"], "err");
        assert_eq!(params["outcome"]["message"], "boom");
    }

    #[test]
    fn translate_lifecycle_events_drop() {
        for event in [
            AgentEvent::AgentStart,
            AgentEvent::TurnStart,
            AgentEvent::TurnEnd,
            AgentEvent::MessageStart {
                message: mu_core::agent::AgentMessage::User {
                    content: "x".into(),
                },
            },
            AgentEvent::MessageEnd {
                message: mu_core::agent::AgentMessage::User {
                    content: "x".into(),
                },
            },
        ] {
            assert!(
                translate_event("s", event).is_none(),
                "lifecycle events should translate to None"
            );
        }
    }

    // ─── event-log projection tests ──────────────────────────────

    #[test]
    fn log_drops_streaming_and_lifecycle_events() {
        // None of these should produce a log entry.
        let cases = vec![
            AgentEvent::TextDelta { delta: "x".into() },
            AgentEvent::AgentStart,
            AgentEvent::TurnStart,
            AgentEvent::TurnEnd,
            AgentEvent::MessageStart {
                message: mu_core::agent::AgentMessage::User {
                    content: "x".into(),
                },
            },
            AgentEvent::InputRequired {
                request_id: "req-x".into(),
                tool_call_id: "call-x".into(),
                tool_name: "edit".into(),
                arguments: json!({}),
                summary: "...".into(),
            },
        ];
        for ev in cases {
            assert!(
                to_log_event(&ev).is_none(),
                "expected no log entry for {ev:?}"
            );
        }
    }

    #[test]
    fn log_records_provider_status_update() {
        // mu-pex Phase 1.5: AgentEvent::ProviderStatus is now durable
        // (previously dropped). The shape carries through with all
        // fields preserved and EventActor::System.
        let ev = AgentEvent::ProviderStatus {
            state: mu_core::protocol::ProviderStatusKind::Streaming,
            started_at_unix_ms: 1_000_000,
            elapsed_ms: 0,
            bytes_received: Some(512),
            tool_call_id: None,
        };
        let (actor, payload) = to_log_event(&ev).expect("ProviderStatus → log");
        assert!(matches!(actor, EventActor::System));
        match payload {
            EventPayload::ProviderStatusUpdate {
                state,
                started_at_unix_ms,
                elapsed_ms,
                bytes_received,
                tool_call_id,
            } => {
                assert_eq!(state, mu_core::protocol::ProviderStatusKind::Streaming);
                assert_eq!(started_at_unix_ms, 1_000_000);
                assert_eq!(elapsed_ms, 0);
                assert_eq!(bytes_received, Some(512));
                assert!(tool_call_id.is_none());
            }
            other => panic!("expected ProviderStatusUpdate, got {other:?}"),
        }
    }

    #[test]
    fn log_records_user_message_from_message_end() {
        let ev = AgentEvent::MessageEnd {
            message: mu_core::agent::AgentMessage::User {
                content: "hello".into(),
            },
        };
        let (actor, payload) = to_log_event(&ev).expect("user MessageEnd → log");
        assert!(matches!(actor, EventActor::User));
        assert!(matches!(
            payload,
            EventPayload::UserMessage { ref content } if content == "hello"
        ));
    }

    #[test]
    fn log_records_assistant_message_with_usage_intact() {
        use mu_core::agent::{AssistantMessage, ContentBlock, StopReason, Usage};
        let am = AssistantMessage {
            content: vec![ContentBlock::Text {
                text: "hi back".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: Some(Usage {
                input_tokens: 100,
                output_tokens: 5,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                reasoning_tokens: None,
            }),
        };
        let ev = AgentEvent::MessageEnd {
            message: mu_core::agent::AgentMessage::Assistant(am.clone()),
        };
        let (actor, payload) = to_log_event(&ev).expect("assistant MessageEnd → log");
        assert!(matches!(actor, EventActor::Agent));
        match payload {
            EventPayload::AssistantMessageEvent { message } => {
                assert_eq!(message, am);
            }
            other => panic!("expected AssistantMessageEvent, got {other:?}"),
        }
    }

    #[test]
    fn log_records_tool_call_started_as_tool_call() {
        let ev = AgentEvent::ToolCallStarted {
            tool_call_id: "call_xyz".into(),
            tool_name: "bash".into(),
            arguments: json!({"command": "echo hi"}),
        };
        let (actor, payload) = to_log_event(&ev).expect("ToolCallStarted → log");
        assert!(matches!(actor, EventActor::Agent));
        match payload {
            EventPayload::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                assert_eq!(call_id, "call_xyz");
                assert_eq!(name, "bash");
                assert_eq!(arguments["command"], "echo hi");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn log_records_tool_call_completed_as_tool_result() {
        let ev = AgentEvent::ToolCallCompleted {
            tool_call_id: "call_xyz".into(),
            content: "hi\nelapsed: 7ms".into(),
            is_error: false,
        };
        let (actor, payload) = to_log_event(&ev).expect("ToolCallCompleted → log");
        // Tool actor — we know the result came from a tool but the
        // tool's name isn't on the AgentEvent (future hardening
        // could thread it through).
        assert!(matches!(actor, EventActor::Tool { .. }));
        match payload {
            EventPayload::ToolResult {
                call_id,
                content,
                is_error,
            } => {
                assert_eq!(call_id, "call_xyz");
                assert!(content.contains("hi"));
                assert!(!is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn log_records_tool_error_with_is_error_true() {
        let ev = AgentEvent::ToolCallCompleted {
            tool_call_id: "call_abc".into(),
            content: "bash: command not allowed".into(),
            is_error: true,
        };
        let (_actor, payload) = to_log_event(&ev).expect("error tool result → log");
        match payload {
            EventPayload::ToolResult { is_error, .. } => {
                assert!(is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn log_full_tool_loop_round_trip() {
        // Simulate a complete tool-use turn: text → tool call →
        // (later) tool result. Confirm the log captures both
        // sides of the loop in order.
        let log = Arc::new(SessionEventLog::new("tool-loop"));

        // 1. Agent decides to call a tool.
        let started = AgentEvent::ToolCallStarted {
            tool_call_id: "call_1".into(),
            tool_name: "bash".into(),
            arguments: json!({"command": "echo audit"}),
        };
        if let Some((actor, payload)) = to_log_event(&started) {
            log.append(actor, payload);
        }

        // 2. Tool returns.
        let completed = AgentEvent::ToolCallCompleted {
            tool_call_id: "call_1".into(),
            content: "audit\nelapsed: 5ms".into(),
            is_error: false,
        };
        if let Some((actor, payload)) = to_log_event(&completed) {
            log.append(actor, payload);
        }

        // Verify both sides landed, in order, with matching call_id.
        let entries = log.snapshot();
        assert_eq!(entries.len(), 2);
        match &entries[0].payload {
            EventPayload::ToolCall { call_id, name, .. } => {
                assert_eq!(call_id, "call_1");
                assert_eq!(name, "bash");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
        match &entries[1].payload {
            EventPayload::ToolResult {
                call_id, is_error, ..
            } => {
                assert_eq!(call_id, "call_1");
                assert!(!is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
        assert_eq!(log.tool_call_count(), 1);
    }

    #[test]
    fn log_records_done_with_full_payload() {
        use mu_core::agent::{StopReason, Usage};
        let ev = AgentEvent::Done {
            stop_reason: StopReason::EndTurn,
            turn_count: 2,
            usage: Some(Usage {
                input_tokens: 200,
                output_tokens: 50,
                cache_read_input_tokens: Some(15),
                cache_creation_input_tokens: None,
                reasoning_tokens: None,
            }),
            elapsed_ms: Some(1234),
        };
        let (actor, payload) = to_log_event(&ev).expect("Done → log");
        assert!(matches!(actor, EventActor::Agent));
        match payload {
            EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 2,
                usage: Some(u),
                elapsed_ms: Some(1234),
            } => {
                assert_eq!(u.input_tokens, 200);
                assert_eq!(u.output_tokens, 50);
                assert_eq!(u.cache_read_input_tokens, Some(15));
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn log_projection_over_a_full_event_sequence() {
        use mu_core::agent::{AssistantMessage, ContentBlock, StopReason, Usage};

        let log = Arc::new(SessionEventLog::new("test-session"));

        // Hand-run the log-projection step the forwarder takes
        // internally. (The wire-projection branch is exercised by
        // the serve_smoke integration tests; here we focus on the
        // log + cumulative-usage path.)
        let sequence = vec![
            AgentEvent::AgentStart,
            AgentEvent::TurnStart,
            AgentEvent::TextDelta { delta: "hi".into() },
            AgentEvent::MessageEnd {
                message: mu_core::agent::AgentMessage::Assistant(AssistantMessage {
                    content: vec![ContentBlock::Text { text: "hi".into() }],
                    stop_reason: StopReason::EndTurn,
                    usage: Some(Usage {
                        input_tokens: 42,
                        output_tokens: 7,
                        cache_read_input_tokens: None,
                        cache_creation_input_tokens: None,
                        reasoning_tokens: None,
                    }),
                }),
            },
            AgentEvent::TurnEnd,
            AgentEvent::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 1,
                usage: Some(Usage {
                    input_tokens: 42,
                    output_tokens: 7,
                    cache_read_input_tokens: None,
                    cache_creation_input_tokens: None,
                    reasoning_tokens: None,
                }),
                elapsed_ms: Some(123),
            },
        ];
        for ev in sequence {
            if let Some((actor, payload)) = to_log_event(&ev) {
                log.append(actor, payload);
            }
        }

        // Log projection: only the significant events landed.
        let entries = log.snapshot();
        let kinds: Vec<&str> = entries
            .iter()
            .map(|e| match &e.payload {
                EventPayload::SessionCreated { .. } => "session_created",
                EventPayload::UserMessage { .. } => "user_message",
                EventPayload::AssistantMessageEvent { .. } => "assistant_message",
                EventPayload::ToolCall { .. } => "tool_call",
                EventPayload::ToolResult { .. } => "tool_result",
                EventPayload::Done { .. } => "done",
                EventPayload::Error { .. } => "error",
                EventPayload::Callout { .. } => "callout",
                EventPayload::SessionClosed => "session_closed",
                EventPayload::ContextAssembly { .. } => "context_assembly",
                EventPayload::ProviderStatusUpdate { .. } => "provider_status_update",
                EventPayload::AutonomousIterationStarted { .. } => "autonomous_iteration_started",
                EventPayload::AutonomousIterationCompleted { .. } => {
                    "autonomous_iteration_completed"
                }
                EventPayload::AutonomousScheduledWakeup { .. } => "autonomous_scheduled_wakeup",
                EventPayload::AutonomousTerminated { .. } => "autonomous_terminated",
            })
            .collect();
        assert_eq!(kinds, vec!["assistant_message", "done"]);

        // Cumulative usage derivation.
        let cumulative = log.cumulative_usage().expect("Done had usage");
        assert_eq!(cumulative.input_tokens, 42);
        assert_eq!(cumulative.output_tokens, 7);
        assert_eq!(log.ask_count(), 1);
        assert_eq!(log.elapsed_total_ms(), 123);
    }

    #[test]
    fn log_cumulative_sums_across_multiple_asks() {
        use mu_core::agent::{StopReason, Usage};

        let log = Arc::new(SessionEventLog::new("multi-ask"));
        // Three asks. Cumulative input = 100+50+25 = 175.
        for (input, output, elapsed) in [(100u64, 30u64, 500u64), (50, 12, 400), (25, 8, 200)] {
            let ev = AgentEvent::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 1,
                usage: Some(Usage {
                    input_tokens: input,
                    output_tokens: output,
                    cache_read_input_tokens: None,
                    cache_creation_input_tokens: None,
                    reasoning_tokens: None,
                }),
                elapsed_ms: Some(elapsed),
            };
            let (actor, payload) = to_log_event(&ev).unwrap();
            log.append(actor, payload);
        }

        let cumulative = log.cumulative_usage().unwrap();
        assert_eq!(cumulative.input_tokens, 175);
        assert_eq!(cumulative.output_tokens, 50);
        assert_eq!(log.ask_count(), 3);
        assert_eq!(log.elapsed_total_ms(), 1100);
    }
}
