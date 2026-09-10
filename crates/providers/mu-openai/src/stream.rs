//! Server-sent streaming events for `POST /v1/responses` with `stream: true`.
//!
//! `ResponseStreamEvent` is the full `response.*` event vocabulary (plus the
//! bare `error` frame), internally tagged on the event `type`. The byte-level
//! SSE framing (reading `data:` lines off the wire) is the CONSUMER's job — this
//! crate only types the decoded JSON of each event. An untagged `Unknown`
//! catch-all keeps a new event type from failing the stream.

use serde::{Deserialize, Serialize};

use crate::{JsonValue, OutputContent, OutputItem, Response, ResponseError, SteerInput};

/// One decoded streaming event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ResponseStreamEvent {
    // ---- lifecycle (carry the full Response snapshot) ----
    #[serde(rename = "response.created")]
    Created {
        response: Response,
        sequence_number: u64,
    },
    #[serde(rename = "response.in_progress")]
    InProgress {
        response: Response,
        sequence_number: u64,
    },
    #[serde(rename = "response.queued")]
    Queued {
        response: Response,
        sequence_number: u64,
    },
    #[serde(rename = "response.completed")]
    Completed {
        response: Response,
        sequence_number: u64,
    },
    #[serde(rename = "response.failed")]
    Failed {
        response: Response,
        sequence_number: u64,
    },
    #[serde(rename = "response.incomplete")]
    Incomplete {
        response: Response,
        sequence_number: u64,
    },

    // ---- output items ----
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded {
        output_index: u32,
        item: OutputItem,
        sequence_number: u64,
    },
    #[serde(rename = "response.output_item.done")]
    OutputItemDone {
        output_index: u32,
        item: OutputItem,
        sequence_number: u64,
    },
    #[serde(rename = "response.content_part.added")]
    ContentPartAdded {
        item_id: String,
        output_index: u32,
        content_index: u32,
        part: OutputContent,
        sequence_number: u64,
    },
    #[serde(rename = "response.content_part.done")]
    ContentPartDone {
        item_id: String,
        output_index: u32,
        content_index: u32,
        part: OutputContent,
        sequence_number: u64,
    },

    // ---- text ----
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        delta: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_index: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_index: Option<u32>,
        sequence_number: u64,
    },
    #[serde(rename = "response.output_text.done")]
    OutputTextDone {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_index: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_index: Option<u32>,
        sequence_number: u64,
    },

    // ---- function-call arguments ----
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta {
        item_id: String,
        output_index: u32,
        delta: String,
        sequence_number: u64,
    },
    /// Compatibility with the ChatGPT/Codex backend spelling observed in older
    /// mu fixtures (dot before `arguments`). The public OpenAPI spelling is
    /// `response.function_call_arguments.delta`; keep accepting this so the
    /// subscription path does not break if Codex lags or forks the public API.
    #[serde(rename = "response.function_call.arguments.delta")]
    FunctionCallArgumentsDeltaCompat {
        item_id: String,
        output_index: u32,
        delta: String,
        #[serde(default)]
        sequence_number: u64,
    },
    #[serde(rename = "response.function_call_arguments.done")]
    FunctionCallArgumentsDone {
        item_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        output_index: u32,
        arguments: String,
        sequence_number: u64,
    },

    // ---- reasoning ----
    #[serde(rename = "response.reasoning_summary_part.added")]
    ReasoningSummaryPartAdded {
        item_id: String,
        output_index: u32,
        summary_index: u32,
        part: JsonValue,
        sequence_number: u64,
    },
    #[serde(rename = "response.reasoning_summary_part.done")]
    ReasoningSummaryPartDone {
        item_id: String,
        output_index: u32,
        summary_index: u32,
        part: JsonValue,
        sequence_number: u64,
    },
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryTextDelta {
        item_id: String,
        output_index: u32,
        summary_index: u32,
        delta: String,
        sequence_number: u64,
    },
    #[serde(rename = "response.reasoning_summary_text.done")]
    ReasoningSummaryTextDone {
        item_id: String,
        output_index: u32,
        summary_index: u32,
        text: String,
        sequence_number: u64,
    },
    #[serde(rename = "response.reasoning_text.delta")]
    ReasoningTextDelta {
        item_id: String,
        output_index: u32,
        delta: String,
        sequence_number: u64,
    },
    #[serde(rename = "response.reasoning_text.done")]
    ReasoningTextDone {
        item_id: String,
        output_index: u32,
        text: String,
        sequence_number: u64,
    },

    // ---- refusal ----
    #[serde(rename = "response.refusal.delta")]
    RefusalDelta { delta: String, sequence_number: u64 },
    #[serde(rename = "response.refusal.done")]
    RefusalDone {
        refusal: String,
        sequence_number: u64,
    },

    // ---- errors ----
    #[serde(rename = "response.error")]
    ResponseError {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        message: String,
        sequence_number: u64,
    },
    /// The codex backend wraps the detail in a nested `error` body
    /// (`{"type":"error","status":429,"error":{"type":"usage_limit_reached",
    /// "message":...,"plan_type":...,"resets_at":...}}`); the public API uses
    /// flat top-level `message`/`code`. Both shapes land here.
    #[serde(rename = "error")]
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ResponseError>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sequence_number: Option<u64>,
        /// Over WebSocket (`ResponseWsError`): "the WebSocket lane that
        /// emitted this event … present when the originating
        /// `response.create` event supplied a `stream_id`."
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stream_id: Option<String>,
    },

    // ---- WebSocket steering (`ResponsesServerEvent`; never on the SSE
    // wire — a lane with a WebSocket transport is what decodes these) ----
    /// "Emitted when steering input has been validated and queued.
    /// Acceptance means the server owns the input, not that it has been
    /// applied. The successor's `response.created` event is the commit
    /// point."
    #[serde(rename = "response.steer.accepted")]
    SteerAccepted {
        sequence_number: u64,
        steer: SteerRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stream_id: Option<String>,
    },
    /// "Emitted when accepted steering input remains queued after the
    /// target response completes. The server still owns the input. Do not
    /// resend it." With `reason` `waiting_for_required_input` (an
    /// extensible enum, kept a string) the `required_input` stubs name the
    /// tool results or approval decisions to supply: "Copy those stubs, fill
    /// their result fields using the ordinary `response.create` input
    /// schemas, and submit one continuation per parent with the same
    /// `previous_response_id` and WebSocket lane." The stubs are seven
    /// output-item shapes (`ResponseSteerRequiredInput`), carried raw.
    #[serde(rename = "response.steer.pending")]
    SteerPending {
        sequence_number: u64,
        steer: SteerRef,
        reason: String,
        required_input: Vec<JsonValue>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stream_id: Option<String>,
    },
    /// "Emitted when steering input is rejected or cannot be committed to a
    /// successor response. Returns the original, uncommitted input so the
    /// client can carry it into `response.create` when appropriate."
    /// "Failures before an ID is allocated omit `steer.id`."
    #[serde(rename = "response.steer.failed")]
    SteerFailed {
        sequence_number: u64,
        steer: SteerRejected,
        error: SteerError,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stream_id: Option<String>,
    },

    /// An event type we don't model. Round-trips losslessly; never fails the
    /// stream. (Tool-specific events — web_search, code_interpreter, mcp, … —
    /// land here; they're out of scope for agent/text.)
    #[serde(untagged)]
    Unknown(JsonValue),
}

/// `steer` on `response.steer.accepted` / `.pending`: the steering id the
/// server allocated and the response it targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteerRef {
    pub id: String,
    pub previous_response_id: String,
}

/// `steer` on `response.steer.failed`: the original, uncommitted input, with
/// the id when one had been allocated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteerRejected {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub previous_response_id: String,
    pub input: SteerInput,
}

/// `error` on `response.steer.failed`: `type` is `invalid_request_error`;
/// `code` is `ResponseSteerErrorCode`, an extensible enum — `response_not_found`,
/// `invalid_input`, `steering_not_supported`, `too_many_pending_steers`,
/// `response_already_completed`, `response_not_active`,
/// `successor_creation_failed` are the documented values — kept a string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteerError {
    #[serde(rename = "type")]
    pub kind: String,
    pub code: String,
    pub message: String,
}

/// Render an `Error` stream event's fields as one displayable message,
/// whichever shape (flat or nested) carried the detail.
pub fn stream_error_message(
    message: Option<String>,
    code: Option<String>,
    status: Option<u16>,
    error: Option<ResponseError>,
) -> String {
    let e = error.unwrap_or_default();
    let text = message.or(e.message);
    let code = code.or(e.code).or(e.kind);
    let mut msg = match (code, text) {
        (Some(c), Some(t)) => format!("{c}: {t}"),
        (Some(c), None) => c,
        (None, Some(t)) => t,
        (None, None) => "openai stream error".into(),
    };
    if let Some(s) = status {
        msg.push_str(&format!(" (http {s})"));
    }
    if let Some(p) = e.plan_type {
        msg.push_str(&format!(" [plan {p}]"));
    }
    if let Some(r) = e.resets_at {
        msg.push_str(&format!(" [resets_at {r}]"));
    }
    // A misalignment stop carries the public explanation and, when the
    // service offers one, a continuation instruction; both belong in the
    // line the operator reads (the guide: "Show the available error
    // information to the user or operator responsible for the task").
    if let Some(m) = e.misalignment {
        if let Some(t) = m.error_type {
            msg.push_str(&format!(" [{t}]"));
        }
        if let Some(x) = m.detailed_explanation {
            msg.push_str(&format!(" — {x}"));
        }
        if let Some(st) = m.steer {
            msg.push_str(&format!(" [steer: {}]", st.message));
        }
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: serde_json::Value) -> ResponseStreamEvent {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn lifecycle_and_text_events_parse() {
        assert!(matches!(
            parse(json!({"type": "response.queued", "sequence_number": 0,
                         "response": {"id": "r", "status": "queued"}})),
            ResponseStreamEvent::Queued { .. }
        ));
        assert!(matches!(
            parse(json!({"type": "response.completed", "sequence_number": 9,
                         "response": {"id": "r", "status": "completed"}})),
            ResponseStreamEvent::Completed { .. }
        ));
        match parse(json!({"type": "response.output_text.delta", "delta": "hi",
                           "item_id": "msg_1", "output_index": 0, "content_index": 0,
                           "sequence_number": 4}))
        {
            ResponseStreamEvent::OutputTextDelta { delta, .. } => assert_eq!(delta, "hi"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn function_call_arguments_official_and_codex_compat() {
        assert!(matches!(
            parse(
                json!({"type": "response.function_call_arguments.delta", "item_id": "fc",
                         "output_index": 0, "delta": "{\"x\":", "sequence_number": 4})
            ),
            ResponseStreamEvent::FunctionCallArgumentsDelta { .. }
        ));
        assert!(matches!(
            parse(
                json!({"type": "response.function_call.arguments.delta", "item_id": "fc",
                         "output_index": 0, "delta": "{\"x\":"})
            ),
            ResponseStreamEvent::FunctionCallArgumentsDeltaCompat { .. }
        ));
    }

    #[test]
    fn reasoning_refusal_and_error_events_parse() {
        assert!(matches!(
            parse(
                json!({"type": "response.reasoning_text.done", "item_id": "rs",
                         "output_index": 0, "text": "hidden", "sequence_number": 4})
            ),
            ResponseStreamEvent::ReasoningTextDone { .. }
        ));
        assert!(matches!(
            parse(json!({"type": "response.refusal.done", "refusal": "no", "sequence_number": 7})),
            ResponseStreamEvent::RefusalDone { .. }
        ));
        assert!(matches!(
            parse(json!({"type": "response.error", "code": "rate_limit",
                         "message": "slow", "sequence_number": 5})),
            ResponseStreamEvent::ResponseError { .. }
        ));
    }

    #[test]
    fn wrapped_error_event_parses_and_renders_detail() {
        // The codex backend's shape: detail nested under `error`, flat
        // top-level message/code absent.
        let ev = parse(json!({
            "type": "error",
            "status": 429,
            "error": {
                "type": "usage_limit_reached",
                "message": "The usage limit has been reached",
                "plan_type": "pro",
                "resets_at": 1738888888
            }
        }));
        let (message, code, status, error) = match ev {
            ResponseStreamEvent::Error {
                message,
                code,
                status,
                error,
                ..
            } => (message, code, status, error),
            other => panic!("expected Error event, got {other:?}"),
        };
        assert_eq!(status, Some(429));
        let msg = stream_error_message(message, code, status, error);
        assert_eq!(
            msg,
            "usage_limit_reached: The usage limit has been reached \
             (http 429) [plan pro] [resets_at 1738888888]"
        );
    }

    #[test]
    fn flat_error_event_still_renders_message() {
        let ev = parse(json!({"type": "error", "code": "rate_limit_exceeded",
                              "message": "slow down", "sequence_number": 2}));
        let (message, code, status, error) = match ev {
            ResponseStreamEvent::Error {
                message,
                code,
                status,
                error,
                ..
            } => (message, code, status, error),
            other => panic!("expected Error event, got {other:?}"),
        };
        assert_eq!(
            stream_error_message(message, code, status, error),
            "rate_limit_exceeded: slow down"
        );
    }

    #[test]
    fn empty_error_event_keeps_generic_message() {
        let ev = parse(json!({"type": "error"}));
        let (message, code, status, error) = match ev {
            ResponseStreamEvent::Error {
                message,
                code,
                status,
                error,
                ..
            } => (message, code, status, error),
            other => panic!("expected Error event, got {other:?}"),
        };
        assert_eq!(
            stream_error_message(message, code, status, error),
            "openai stream error"
        );
    }

    /// The misalignment details ride the rendered line: type, explanation
    /// and steer, after the code, message and status.
    #[test]
    fn misalignment_details_render_in_the_message() {
        let error = ResponseError {
            code: Some("misalignment_policy_violation".into()),
            message: Some("Stopped for review.".into()),
            misalignment: Some(crate::MisalignmentErrorDetails {
                error_type: Some("potentially_unintended_data_access".into()),
                detailed_explanation: Some("The agent read credentials outside the task.".into()),
                steer: Some(crate::MisalignmentSteer {
                    message: "Ask before reading secrets.".into(),
                }),
            }),
            ..Default::default()
        };
        assert_eq!(
            stream_error_message(None, None, Some(403), Some(error)),
            "misalignment_policy_violation: Stopped for review. (http 403) \
             [potentially_unintended_data_access] — The agent read credentials outside the \
             task. [steer: Ask before reading secrets.]"
        );
    }

    /// The three steering server events and the WebSocket error frame, as
    /// the spec's examples print them, round-trip through the event enum.
    #[test]
    fn steer_server_events_and_ws_error_round_trip() {
        let accepted = json!({"type": "response.steer.accepted", "sequence_number": 2,
                              "steer": {"id": "steer_456", "previous_response_id": "resp_123"}});
        assert_eq!(
            parse(accepted.clone()),
            ResponseStreamEvent::SteerAccepted {
                sequence_number: 2,
                steer: SteerRef {
                    id: "steer_456".into(),
                    previous_response_id: "resp_123".into()
                },
                stream_id: None,
            }
        );
        assert_eq!(
            serde_json::to_value(parse(accepted.clone())).unwrap(),
            accepted
        );

        let pending = json!({"type": "response.steer.pending", "sequence_number": 10,
                             "steer": {"id": "steer_456", "previous_response_id": "resp_123"},
                             "reason": "waiting_for_required_input",
                             "required_input": [{"type": "function_call_output",
                                                 "call_id": "call_789", "name": "lookup"}]});
        match parse(pending.clone()) {
            ResponseStreamEvent::SteerPending {
                reason,
                required_input,
                ..
            } => {
                assert_eq!(reason, "waiting_for_required_input");
                assert_eq!(required_input.len(), 1);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            serde_json::to_value(parse(pending.clone())).unwrap(),
            pending
        );

        let failed = json!({"type": "response.steer.failed", "sequence_number": 5,
            "steer": {"id": "steer_456", "previous_response_id": "resp_123",
                      "input": [{"type": "message", "role": "user",
                                 "content": [{"type": "input_text",
                                              "text": "Prioritize the database rollout."}]}]},
            "error": {"type": "invalid_request_error", "code": "successor_creation_failed",
                      "message": "We couldn't start the next response. Send this steering input again with response.create."}});
        match parse(failed.clone()) {
            ResponseStreamEvent::SteerFailed { steer, error, .. } => {
                assert_eq!(steer.id.as_deref(), Some("steer_456"));
                assert!(matches!(steer.input, SteerInput::Items(ref i) if i.len() == 1));
                assert_eq!(error.code, "successor_creation_failed");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(serde_json::to_value(parse(failed.clone())).unwrap(), failed);

        let ws_error = json!({"type": "error", "status": 400, "stream_id": "agent_1",
            "error": {"type": "invalid_request_error", "code": "websocket_stream_limit_reached",
                      "message": "This WebSocket connection has reached its stream limit."}});
        match parse(ws_error.clone()) {
            ResponseStreamEvent::Error {
                stream_id, error, ..
            } => {
                assert_eq!(stream_id.as_deref(), Some("agent_1"));
                assert_eq!(
                    error.unwrap().code.as_deref(),
                    Some("websocket_stream_limit_reached")
                );
            }
            other => panic!("{other:?}"),
        }
        // `param: null` in the spec example is not modeled on ResponseError;
        // the example above omits it so the round-trip is exact.
        assert_eq!(
            serde_json::to_value(parse(ws_error.clone())).unwrap(),
            ws_error
        );
    }

    #[test]
    fn unknown_event_type_degrades_not_errors() {
        assert!(matches!(
            parse(json!({"type": "response.web_search_call.searching",
                         "output_index": 0, "sequence_number": 3})),
            ResponseStreamEvent::Unknown(_)
        ));
    }
}
