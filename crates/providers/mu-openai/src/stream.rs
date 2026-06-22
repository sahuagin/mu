use crate::{JsonValue, OutputItem, Response};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ResponseStreamEvent {
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
        part: crate::OutputContent,
        sequence_number: u64,
    },
    #[serde(rename = "response.content_part.done")]
    ContentPartDone {
        item_id: String,
        output_index: u32,
        content_index: u32,
        part: crate::OutputContent,
        sequence_number: u64,
    },
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        delta: String,
        #[serde(default)]
        item_id: Option<String>,
        #[serde(default)]
        output_index: Option<u32>,
        #[serde(default)]
        content_index: Option<u32>,
        sequence_number: u64,
    },
    #[serde(rename = "response.output_text.done")]
    OutputTextDone {
        text: String,
        #[serde(default)]
        item_id: Option<String>,
        #[serde(default)]
        output_index: Option<u32>,
        #[serde(default)]
        content_index: Option<u32>,
        sequence_number: u64,
    },
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta {
        item_id: String,
        output_index: u32,
        delta: String,
        sequence_number: u64,
    },
    /// Compatibility with the ChatGPT/Codex backend spelling observed in older
    /// mu fixtures. The public OpenAPI spelling is
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
        #[serde(default)]
        name: Option<String>,
        output_index: u32,
        arguments: String,
        sequence_number: u64,
    },
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
    #[serde(rename = "response.refusal.delta")]
    RefusalDelta { delta: String, sequence_number: u64 },
    #[serde(rename = "response.refusal.done")]
    RefusalDone {
        refusal: String,
        sequence_number: u64,
    },
    #[serde(rename = "response.error")]
    ResponseError {
        #[serde(default)]
        code: Option<String>,
        message: String,
        sequence_number: u64,
    },
    #[serde(rename = "error")]
    Error {
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        code: Option<String>,
        #[serde(default)]
        sequence_number: Option<u64>,
    },
    #[serde(untagged)]
    Unknown(JsonValue),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamError {
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn parse(v: serde_json::Value) -> ResponseStreamEvent {
        serde_json::from_value(v).unwrap()
    }
    #[test]
    fn official_function_call_arguments_delta_parses() {
        match parse(
            json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":0,"delta":"{\"x\":","sequence_number":4}),
        ) {
            ResponseStreamEvent::FunctionCallArgumentsDelta { delta, .. } => {
                assert_eq!(delta, "{\"x\":")
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn codex_compat_function_call_arguments_delta_parses() {
        match parse(
            json!({"type":"response.function_call.arguments.delta","item_id":"fc_1","output_index":0,"delta":"{\"x\":"}),
        ) {
            ResponseStreamEvent::FunctionCallArgumentsDeltaCompat { delta, .. } => {
                assert_eq!(delta, "{\"x\":")
            }
            other => panic!("got {other:?}"),
        }
    }
    #[test]
    fn content_part_and_refusal_events_parse() {
        assert!(matches!(
            parse(
                json!({"type":"response.content_part.added","item_id":"msg_1","output_index":0,"content_index":0,"part":{"type":"output_text","text":"hi","annotations":[]},"sequence_number":2})
            ),
            ResponseStreamEvent::ContentPartAdded { .. }
        ));
        assert!(matches!(
            parse(json!({"type":"response.refusal.done","refusal":"no","sequence_number":7})),
            ResponseStreamEvent::RefusalDone { .. }
        ));
    }

    #[test]
    fn reasoning_and_response_error_events_parse() {
        assert!(matches!(
            parse(
                json!({"type":"response.reasoning_summary_part.added","item_id":"rs_1","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":"plan"},"sequence_number":3})
            ),
            ResponseStreamEvent::ReasoningSummaryPartAdded { .. }
        ));
        assert!(matches!(
            parse(
                json!({"type":"response.reasoning_text.done","item_id":"rs_1","output_index":0,"text":"hidden","sequence_number":4})
            ),
            ResponseStreamEvent::ReasoningTextDone { .. }
        ));
        assert!(matches!(
            parse(
                json!({"type":"response.error","code":"rate_limit","message":"slow down","sequence_number":5})
            ),
            ResponseStreamEvent::ResponseError { .. }
        ));
    }

    #[test]
    fn completed_event_parses() {
        assert!(matches!(
            parse(
                json!({"type":"response.completed","sequence_number":9,"response":{"id":"r","status":"completed","output":[]}})
            ),
            ResponseStreamEvent::Completed { .. }
        ));
    }
}
