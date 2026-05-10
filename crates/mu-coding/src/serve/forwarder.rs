//! Translate `AgentEvent`s from the loop into `session.*` JSON-RPC
//! notifications via mu-002's NotificationWriter.
//!
//! Five emissions, five drops. Lifecycle events (AgentStart,
//! TurnStart, TurnEnd, MessageStart, MessageEnd) are silently dropped
//! — mu-001's notification surface doesn't include them. We can amend
//! mu-001 to add them when a frontend (e.g., the TUI) needs them.

use tokio::sync::mpsc;

use mu_core::agent::AgentEvent;
use mu_core::protocol::{
    CalloutBody, CalloutEvent, DoneEvent, ErrorEvent, TextDeltaEvent, ToolCallCompletedEvent,
    ToolCallStartedEvent, ToolOutcome,
};
use mu_core::transport::NotificationWriter;

/// Read AgentEvents from `events_rx`, translate, emit via `notif`.
/// Exits when `events_rx` is closed (loop terminated).
pub async fn forward_events(
    session_id: String,
    mut events_rx: mpsc::Receiver<AgentEvent>,
    notif: NotificationWriter,
) {
    while let Some(event) = events_rx.recv().await {
        match event {
            AgentEvent::TextDelta { delta } => {
                let _ = notif
                    .emit(
                        TextDeltaEvent::METHOD,
                        TextDeltaEvent {
                            session_id: session_id.clone(),
                            delta,
                        },
                    )
                    .await;
            }
            AgentEvent::ToolCallStarted {
                tool_call_id,
                tool_name,
                arguments,
            } => {
                let _ = notif
                    .emit(
                        ToolCallStartedEvent::METHOD,
                        ToolCallStartedEvent {
                            session_id: session_id.clone(),
                            tool_call_id,
                            tool_name,
                            arguments,
                        },
                    )
                    .await;
            }
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
                        result: serde_json::Value::String(content),
                    }
                };
                let _ = notif
                    .emit(
                        ToolCallCompletedEvent::METHOD,
                        ToolCallCompletedEvent {
                            session_id: session_id.clone(),
                            tool_call_id,
                            outcome,
                        },
                    )
                    .await;
            }
            AgentEvent::Done { .. } => {
                let _ = notif
                    .emit(
                        DoneEvent::METHOD,
                        DoneEvent {
                            session_id: session_id.clone(),
                            usage: None,
                        },
                    )
                    .await;
            }
            AgentEvent::Error { message } => {
                let _ = notif
                    .emit(
                        ErrorEvent::METHOD,
                        ErrorEvent {
                            session_id: session_id.clone(),
                            message,
                            detail: None,
                        },
                    )
                    .await;
            }
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
                    serde_json::Value::String(s) => CalloutBody::Text(s),
                    other => CalloutBody::Structured(other),
                };
                let _ = notif
                    .emit(
                        CalloutEvent::METHOD,
                        CalloutEvent {
                            session_id: session_id.clone(),
                            kind: category,
                            title,
                            body: body_payload,
                            theme,
                            context_refs,
                        },
                    )
                    .await;
            }
            // Lifecycle events not in mu-001's notification surface —
            // see module doc for rationale. Silently dropped.
            AgentEvent::AgentStart
            | AgentEvent::TurnStart
            | AgentEvent::TurnEnd
            | AgentEvent::MessageStart { .. }
            | AgentEvent::MessageEnd { .. } => {}
        }
    }
}
