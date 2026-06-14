//! Streaming SSE event types. Each `data:` line of the SSE stream
//! deserializes into a [`StreamEvent`]; an async accumulator (slice 6) folds a
//! stream of these into a final [`ResponseMessage`](crate::ResponseMessage).
//!
//! Event sequence (spec :8840): message_start, then per content block
//! {content_block_start, content_block_delta*, content_block_stop}, then
//! message_delta, then message_stop. `ping` may arrive at any time; `error`
//! terminates.
//!
//! SCAR mu-yz48 (the sharpest one): in `message_delta`, `usage` is a SIBLING
//! of `delta` at the event's TOP LEVEL — NOT nested inside `delta` (spec
//! :16136: `{"type":"message_delta","delta":{...},"usage":{"output_tokens":15}}`).
//! A type that reads `delta.usage` gets None and freezes output_tokens at the
//! message_start baseline. [`StreamEvent::MessageDelta`] puts `usage` at the
//! variant top level; a test pins it.

use serde::Deserialize;
use serde_json::Value;

use crate::response::{StopReason, Usage};

/// One SSE event from the Messages streaming API. Internally tagged on `type`.
/// Unknown event types degrade to [`StreamEvent::Unknown`] (forward-compat).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    MessageStart {
        message: Value,
    },
    ContentBlockStart {
        index: u32,
        content_block: BlockStart,
    },
    ContentBlockDelta {
        index: u32,
        delta: BlockDelta,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        delta: MessageDeltaBody,
        /// mu-yz48: usage is HERE (event top level), not in `delta`.
        #[serde(default)]
        usage: Option<Usage>,
    },
    MessageStop,
    Ping,
    Error {
        error: StreamError,
    },
    #[serde(untagged)]
    Unknown(Value),
}

/// The `content_block` of a `content_block_start`. Only the fields we need to
/// open an accumulator; unknown block types degrade to [`BlockStart::Other`].
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockStart {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
    },
    #[serde(other)]
    Other,
}

/// The `delta` of a `content_block_delta`. `input_json_delta` carries the
/// streamed-in-pieces tool arguments (`partial_json`); accumulate and parse at
/// block stop.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockDelta {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        #[serde(default)]
        partial_json: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    SignatureDelta {
        signature: String,
    },
    #[serde(other)]
    Other,
}

/// The `delta` of a `message_delta` event — terminal metadata. NOTE: `usage`
/// is NOT here (it's a sibling at the event level — see mu-yz48 above).
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct MessageDeltaBody {
    #[serde(default)]
    pub stop_reason: Option<StopReason>,
    #[serde(default)]
    pub stop_sequence: Option<String>,
}

/// An `error` event body.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct StreamError {
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: serde_json::Value) -> StreamEvent {
        serde_json::from_value(v).expect("parse StreamEvent")
    }

    #[test]
    fn message_delta_usage_is_top_level_not_nested() {
        // SCAR mu-yz48 — spec :16136. usage is a sibling of delta.
        let ev = parse(json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": null},
            "usage": {"output_tokens": 15}
        }));
        match ev {
            StreamEvent::MessageDelta { delta, usage } => {
                assert_eq!(delta.stop_reason, Some(StopReason::EndTurn));
                assert_eq!(
                    usage
                        .expect("usage must parse from TOP LEVEL")
                        .output_tokens,
                    Some(15),
                    "mu-yz48: reading usage from event top level, not delta.usage"
                );
            }
            other => panic!("expected MessageDelta, got {other:?}"),
        }
    }

    #[test]
    fn message_delta_without_usage_is_fine() {
        // spec :8871 — a message_delta with no usage sibling.
        let ev = parse(json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": null}
        }));
        match ev {
            StreamEvent::MessageDelta { usage, .. } => assert!(usage.is_none()),
            other => panic!("expected MessageDelta, got {other:?}"),
        }
    }

    #[test]
    fn full_event_sequence_parses() {
        // spec :8840-8874 — the documented stream.
        assert!(matches!(
            parse(json!({"type":"message_start","message":{"id":"x"}})),
            StreamEvent::MessageStart { .. }
        ));
        assert!(matches!(
            parse(json!({"type":"content_block_start","index":1,
                "content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{}}})),
            StreamEvent::ContentBlockStart {
                index: 1,
                content_block: BlockStart::ToolUse { .. }
            }
        ));
        assert!(matches!(
            parse(json!({"type":"content_block_delta","index":1,
                "delta":{"type":"input_json_delta","partial_json":"{\"location\""}})),
            StreamEvent::ContentBlockDelta {
                delta: BlockDelta::InputJsonDelta { .. },
                ..
            }
        ));
        assert!(matches!(
            parse(json!({"type":"content_block_stop","index":1})),
            StreamEvent::ContentBlockStop { index: 1 }
        ));
        assert!(matches!(
            parse(json!({"type":"message_stop"})),
            StreamEvent::MessageStop
        ));
        assert!(matches!(parse(json!({"type":"ping"})), StreamEvent::Ping));
    }

    #[test]
    fn text_and_thinking_deltas_parse() {
        assert!(matches!(
            parse(json!({"type":"content_block_delta","index":0,
                "delta":{"type":"text_delta","text":"hi"}})),
            StreamEvent::ContentBlockDelta {
                delta: BlockDelta::TextDelta { .. },
                ..
            }
        ));
        assert!(matches!(
            parse(json!({"type":"content_block_delta","index":0,
                "delta":{"type":"thinking_delta","thinking":"reasoning"}})),
            StreamEvent::ContentBlockDelta {
                delta: BlockDelta::ThinkingDelta { .. },
                ..
            }
        ));
        assert!(matches!(
            parse(json!({"type":"content_block_delta","index":0,
                "delta":{"type":"signature_delta","signature":"sig"}})),
            StreamEvent::ContentBlockDelta {
                delta: BlockDelta::SignatureDelta { .. },
                ..
            }
        ));
    }

    #[test]
    fn error_event_parses() {
        let ev = parse(json!({
            "type": "error",
            "error": {"type": "overloaded_error", "message": "Overloaded"}
        }));
        match ev {
            StreamEvent::Error { error } => {
                assert_eq!(error.kind.as_deref(), Some("overloaded_error"));
                assert_eq!(error.message.as_deref(), Some("Overloaded"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_event_type_degrades() {
        let ev = parse(json!({"type": "some_future_event", "payload": 1}));
        assert!(matches!(ev, StreamEvent::Unknown(_)));
    }

    #[test]
    fn unknown_block_delta_degrades_to_other() {
        let ev = parse(json!({"type":"content_block_delta","index":0,
            "delta":{"type":"future_delta","x":1}}));
        assert!(matches!(
            ev,
            StreamEvent::ContentBlockDelta {
                delta: BlockDelta::Other,
                ..
            }
        ));
    }
}

#[cfg(test)]
mod f8_partial_json_default {
    use super::*;
    use serde_json::json;

    #[test]
    fn input_json_delta_with_absent_partial_json_defaults() {
        // A content_block_delta whose input_json_delta omits partial_json must
        // parse (default ""), not fail. (#[serde(default)] on the field.)
        let ev: StreamEvent = serde_json::from_value(json!({
            "type": "content_block_delta", "index": 1,
            "delta": {"type": "input_json_delta"}
        }))
        .expect("absent partial_json must default, not error");
        match ev {
            StreamEvent::ContentBlockDelta {
                delta: BlockDelta::InputJsonDelta { partial_json },
                ..
            } => assert_eq!(partial_json, ""),
            other => panic!("expected InputJsonDelta, got {other:?}"),
        }
    }

    #[test]
    fn input_json_delta_with_empty_partial_json_parses() {
        // spec :16565 — "partial_json":"" appears in real streams.
        let ev: StreamEvent = serde_json::from_value(json!({
            "type": "content_block_delta", "index": 1,
            "delta": {"type": "input_json_delta", "partial_json": ""}
        }))
        .unwrap();
        assert!(matches!(
            ev,
            StreamEvent::ContentBlockDelta {
                delta: BlockDelta::InputJsonDelta { .. },
                ..
            }
        ));
    }
}
