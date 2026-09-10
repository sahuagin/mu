//! The WebSocket-only shapes of the Responses API (2026-09-09 spec capture):
//! the client events a Responses WebSocket server accepts and the input
//! form steering takes. The server side — `response.steer.accepted` /
//! `.pending` / `.failed` and the WebSocket `error` frame — lives with the
//! rest of the event vocabulary in [`crate::ResponseStreamEvent`]. This
//! crate has no WebSocket transport; these are the typed shapes a lane will
//! speak when one exists. mu-openai-protocol-2026q3-yyg3j.4.

use serde::{Deserialize, Serialize};

use crate::{CreateResponseRequest, InputItem};

/// `ResponsesClientEvent`: "Client events accepted by the Responses
/// WebSocket server."
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ResponsesClientEvent {
    /// `response.create` over a persistent connection: "the same top-level
    /// fields as `POST /v1/responses`, plus WebSocket-only envelope
    /// metadata." `stream_id` is the lane: "Requests with the same
    /// `stream_id` are processed FIFO, and events for the response echo the
    /// same `stream_id`. `stream_id` controls routing; `previous_response_id`
    /// controls conversation lineage, so a new lane can fork from a response
    /// created on another lane." Over WebSocket "`stream` is implicit … and
    /// should not be sent" and "`background` is not supported"; the request
    /// type still carries `stream`, so a WebSocket lane leaves it `None`.
    #[serde(rename = "response.create")]
    ResponseCreate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stream_id: Option<String>,
        /// Boxed: a request is large next to a steer, and the event is a
        /// message, not a hot-path value.
        #[serde(flatten)]
        request: Box<CreateResponseRequest>,
    },
    /// `response.steer`: "Queues user input to steer a response on this
    /// WebSocket connection." Accepted only for "single-agent responses on
    /// models and execution modes that support steering"; "`response.steer.
    /// accepted` acknowledges that the server owns the queued input, not that
    /// it has been applied. The successor's `response.created` event is the
    /// commit point." The event "accepts only `type`, `previous_response_id`,
    /// and `input`. Do not send `stream_id`; the target response determines
    /// the WebSocket lane."
    #[serde(rename = "response.steer")]
    Steer {
        previous_response_id: String,
        input: SteerInput,
    },
}

/// `ResponseSteerInput`: "the same string or input-item shape as
/// `response.create.input`, with a non-empty array when supplying input
/// items. Steering accepts only messages with the `user` role … Other roles,
/// tool outputs, and item types are not supported for steering." (The spec's
/// item union also admits `function_call_output`, for the required-input
/// continuation.) The items reuse [`InputItem`], so the constraint is the
/// server's to enforce, as it is for `response.create`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SteerInput {
    /// "A text input, equivalent to a message with the `user` role."
    Text(String),
    Items(Vec<InputItem>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The two client events, as the spec's examples print them (the
    /// `response.create` example's string `input` becomes the equivalent
    /// user message, since the request type carries items).
    #[test]
    fn client_events_round_trip() {
        let create = json!({
            "type": "response.create",
            "stream_id": "agent_1",
            "model": "gpt-6-astra",
            "input": [{"type": "message", "role": "user",
                       "content": [{"type": "input_text", "text": "Say hello."}]}]
        });
        let e: ResponsesClientEvent = serde_json::from_value(create.clone()).unwrap();
        match &e {
            ResponsesClientEvent::ResponseCreate { stream_id, request } => {
                assert_eq!(stream_id.as_deref(), Some("agent_1"));
                assert_eq!(request.model, "gpt-6-astra");
                assert!(request.stream.is_none());
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(serde_json::to_value(&e).unwrap(), create);

        let steer = json!({
            "type": "response.steer",
            "previous_response_id": "resp_123",
            "input": [{"type": "message", "role": "user",
                       "content": [{"type": "input_text",
                                    "text": "Prioritize the database rollout."}]}]
        });
        let e: ResponsesClientEvent = serde_json::from_value(steer.clone()).unwrap();
        assert!(matches!(
            &e,
            ResponsesClientEvent::Steer { previous_response_id, input: SteerInput::Items(items) }
                if previous_response_id == "resp_123" && items.len() == 1
        ));
        assert_eq!(serde_json::to_value(&e).unwrap(), steer);

        let text = json!({"type": "response.steer", "previous_response_id": "resp_1",
                          "input": "Stop and summarize."});
        let e: ResponsesClientEvent = serde_json::from_value(text.clone()).unwrap();
        assert!(
            matches!(&e, ResponsesClientEvent::Steer { input: SteerInput::Text(t), .. } if t == "Stop and summarize.")
        );
        assert_eq!(serde_json::to_value(&e).unwrap(), text);
    }
}
