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
    DoneEvent, ErrorEvent, TextDeltaEvent, ToolCallCompletedEvent, ToolCallStartedEvent,
    ToolOutcome,
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
