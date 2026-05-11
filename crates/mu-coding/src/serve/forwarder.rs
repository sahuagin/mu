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

use serde_json::Value;
use tokio::sync::mpsc;

use mu_core::agent::AgentEvent;
use mu_core::protocol::{
    CalloutBody, CalloutEvent, DoneEvent, ErrorEvent, TextDeltaEvent, ToolCallCompletedEvent,
    ToolCallStartedEvent, ToolOutcome,
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
pub fn translate_event(
    session_id: &str,
    event: AgentEvent,
) -> Option<(&'static str, Value)> {
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
        // Lifecycle events not in mu-001's notification surface.
        AgentEvent::AgentStart
        | AgentEvent::TurnStart
        | AgentEvent::TurnEnd
        | AgentEvent::MessageStart { .. }
        | AgentEvent::MessageEnd { .. } => None,
    }
}

fn to_pair<T: serde::Serialize>(method: &'static str, value: T) -> Option<(&'static str, Value)> {
    serde_json::to_value(value).ok().map(|v| (method, v))
}

/// IO loop: read events from `events_rx`, translate via the pure
/// function above, emit via `notif`. Exits when `events_rx` closes.
pub async fn forward_events(
    session_id: String,
    mut events_rx: mpsc::Receiver<AgentEvent>,
    notif: NotificationWriter,
) {
    while let Some(event) = events_rx.recv().await {
        if let Some((method, params)) = translate_event(&session_id, event) {
            let _ = notif.emit(method, params).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use mu_core::agent::AgentEvent;

    #[test]
    fn translate_text_delta() {
        let (method, params) = translate_event(
            "s1",
            AgentEvent::TextDelta {
                delta: "hi".into(),
            },
        )
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
}
