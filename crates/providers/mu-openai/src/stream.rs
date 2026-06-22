//! Server-sent streaming events for `POST /v1/responses` with `stream: true`.
//!
//! `ResponseStreamEvent` is the full `response.*` event vocabulary (plus the
//! bare `error` frame), internally tagged on the event `type`. The byte-level
//! SSE framing (reading `data:` lines off the wire) is the CONSUMER's job — this
//! crate only types the decoded JSON of each event. An untagged `Unknown`
//! catch-all keeps a new event type from failing the stream.

use serde::{Deserialize, Serialize};

use crate::{JsonValue, OutputContent, OutputItem, Response};

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
    #[serde(rename = "error")]
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sequence_number: Option<u64>,
    },

    /// An event type we don't model. Round-trips losslessly; never fails the
    /// stream. (Tool-specific events — web_search, code_interpreter, mcp, … —
    /// land here; they're out of scope for agent/text.)
    #[serde(untagged)]
    Unknown(JsonValue),
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
    fn unknown_event_type_degrades_not_errors() {
        assert!(matches!(
            parse(json!({"type": "response.web_search_call.searching",
                         "output_index": 0, "sequence_number": 3})),
            ResponseStreamEvent::Unknown(_)
        ));
    }
}
