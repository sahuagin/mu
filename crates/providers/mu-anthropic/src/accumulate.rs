//! Async accumulator — folds a stream of [`StreamEvent`]s into a final
//! [`ResponseMessage`]. The SDK's `get_final_message()` shape: consume the SSE
//! event stream, assemble content blocks in index order, merge usage across
//! `message_start` and `message_delta`, and yield the assembled message at
//! `message_stop`.
//!
//! Async + streaming only (no synchronous API). The caller owns transport and
//! SSE-line framing; this consumes the already-typed events.
//!
//! Scars encoded:
//! - usage merges across TWO events: message_start.message.usage gives the
//!   input_tokens baseline (+output_tokens:1), message_delta.usage gives the
//!   final output_tokens (spec :16118 / :16136). Reading only one undercounts.
//! - content blocks are keyed by `index`; final order follows first-seen index
//!   order, independent of any map iteration order.
//! - tool args stream as input_json_delta `partial_json` fragments; parse the
//!   concatenation at the end. Malformed / non-object JSON falls back to an
//!   empty object rather than failing the whole message.

use std::collections::BTreeMap;

use futures::{Stream, StreamExt};
use serde_json::Value;

use crate::json::JsonValue;

use crate::content::ContentBlock;
use crate::response::{StopReason, Usage};
use crate::stream::{BlockDelta, BlockStart, StreamEvent};

/// Error from accumulating a stream.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AccumulateError {
    /// The stream ended without a `message_stop` (truncated / dropped).
    #[error("stream ended before message_stop (degraded)")]
    UnexpectedEof,
    /// An `error` event arrived mid-stream.
    #[error("stream error event: {0}")]
    StreamError(String),
}

/// The assembled result of a completed stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accumulated {
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<StopReason>,
    pub stop_sequence: Option<String>,
    pub usage: Usage,
}

enum BlockBuilder {
    Text(String),
    Thinking {
        thinking: String,
        signature: String,
    },
    ToolUse {
        id: String,
        name: String,
        json: String,
    },
}

impl BlockBuilder {
    fn finish(self) -> ContentBlock {
        match self {
            BlockBuilder::Text(text) => ContentBlock::Text {
                text,
                cache_control: None,
            },
            BlockBuilder::Thinking {
                thinking,
                signature,
            } => ContentBlock::Thinking {
                thinking,
                signature: if signature.is_empty() {
                    None
                } else {
                    Some(signature)
                },
            },
            BlockBuilder::ToolUse { id, name, json } => ContentBlock::ToolUse {
                id,
                name,
                input: parse_tool_input(&json),
                cache_control: None,
            },
        }
    }
}

/// Parse accumulated tool-input JSON into a [`JsonValue`]. Empty → empty
/// object. Valid object → itself. Anything else (malformed, valid-but-not-
/// object, or carrying a non-finite number) → empty object, so a bad tool
/// payload degrades the one tool call rather than failing the whole message.
fn parse_tool_input(json: &str) -> JsonValue {
    if json.is_empty() {
        return JsonValue::empty_object();
    }
    match serde_json::from_str::<Value>(json) {
        Ok(v) if v.is_object() => JsonValue::new(v).unwrap_or_else(|_| JsonValue::empty_object()),
        _ => JsonValue::empty_object(),
    }
}

fn merge_usage(into: &mut Usage, from: &Usage) {
    if from.input_tokens.is_some() {
        into.input_tokens = from.input_tokens;
    }
    if from.output_tokens.is_some() {
        into.output_tokens = from.output_tokens;
    }
    if from.cache_read_input_tokens.is_some() {
        into.cache_read_input_tokens = from.cache_read_input_tokens;
    }
    if from.cache_creation_input_tokens.is_some() {
        into.cache_creation_input_tokens = from.cache_creation_input_tokens;
    }
    if from.cache_creation.is_some() {
        into.cache_creation = from.cache_creation.clone();
    }
}

/// Consume an async stream of [`StreamEvent`]s and assemble the final message.
/// Returns on `message_stop` (Ok), on an `error` event (Err::StreamError), or
/// on end-of-stream-without-stop (Err::UnexpectedEof — the caller decides
/// whether a partial assembly is usable, e.g. mu's DegradedEof handling).
pub async fn accumulate<S>(mut events: S) -> Result<Accumulated, AccumulateError>
where
    S: Stream<Item = StreamEvent> + Unpin,
{
    let mut builders: BTreeMap<u32, BlockBuilder> = BTreeMap::new();
    let mut order: Vec<u32> = Vec::new();
    let mut stop_reason = None;
    let mut stop_sequence = None;
    let mut usage = Usage::default();

    while let Some(ev) = events.next().await {
        match ev {
            StreamEvent::MessageStart { message } => {
                let raw = message.as_value();
                if let Some(u) = raw
                    .get("message")
                    .or(Some(raw))
                    .and_then(|m| m.get("usage"))
                    .and_then(|u| serde_json::from_value::<Usage>(u.clone()).ok())
                {
                    merge_usage(&mut usage, &u);
                }
            }
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                let b = match content_block {
                    BlockStart::Text { text } => BlockBuilder::Text(text),
                    BlockStart::Thinking { thinking } => BlockBuilder::Thinking {
                        thinking,
                        signature: String::new(),
                    },
                    BlockStart::ToolUse { id, name } => BlockBuilder::ToolUse {
                        id,
                        name,
                        json: String::new(),
                    },
                    BlockStart::Other => continue,
                };
                if !builders.contains_key(&index) {
                    order.push(index);
                }
                builders.insert(index, b);
            }
            StreamEvent::ContentBlockDelta { index, delta } => {
                if let Some(b) = builders.get_mut(&index) {
                    match (b, delta) {
                        (BlockBuilder::Text(s), BlockDelta::TextDelta { text }) => {
                            s.push_str(&text)
                        }
                        (
                            BlockBuilder::Thinking { thinking, .. },
                            BlockDelta::ThinkingDelta { thinking: t },
                        ) => thinking.push_str(&t),
                        (
                            BlockBuilder::Thinking { signature, .. },
                            BlockDelta::SignatureDelta { signature: sig },
                        ) => signature.push_str(&sig),
                        (
                            BlockBuilder::ToolUse { json, .. },
                            BlockDelta::InputJsonDelta { partial_json },
                        ) => json.push_str(&partial_json),
                        _ => {} // mismatched delta/block kind, or Other — ignore.
                    }
                }
            }
            StreamEvent::ContentBlockStop { .. } => {}
            StreamEvent::MessageDelta { delta, usage: u } => {
                if delta.stop_reason.is_some() {
                    stop_reason = delta.stop_reason;
                }
                if delta.stop_sequence.is_some() {
                    stop_sequence = delta.stop_sequence;
                }
                if let Some(u) = u {
                    merge_usage(&mut usage, &u);
                }
            }
            StreamEvent::MessageStop => {
                let content = order
                    .into_iter()
                    .filter_map(|i| builders.remove(&i))
                    .map(BlockBuilder::finish)
                    .collect();
                return Ok(Accumulated {
                    content,
                    stop_reason,
                    stop_sequence,
                    usage,
                });
            }
            StreamEvent::Error { error } => {
                return Err(AccumulateError::StreamError(
                    error.message.unwrap_or_else(|| "(no message)".into()),
                ));
            }
            StreamEvent::Ping | StreamEvent::Unknown(_) => {}
        }
    }
    Err(AccumulateError::UnexpectedEof)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(v: serde_json::Value) -> StreamEvent {
        serde_json::from_value(v).unwrap()
    }

    fn stream(evs: Vec<StreamEvent>) -> impl Stream<Item = StreamEvent> + Unpin {
        Box::pin(futures::stream::iter(evs))
    }

    #[tokio::test]
    async fn assembles_text_message_and_merges_usage() {
        // message_start gives input_tokens=25,output=1; message_delta gives
        // final output=15. Both must show in the merged usage (scar).
        let s = stream(vec![
            ev(
                json!({"type":"message_start","message":{"id":"x","type":"message",
                "role":"assistant","content":[],"model":"m",
                "usage":{"input_tokens":25,"output_tokens":1}}}),
            ),
            ev(json!({"type":"content_block_start","index":0,
                "content_block":{"type":"text","text":""}})),
            ev(json!({"type":"content_block_delta","index":0,
                "delta":{"type":"text_delta","text":"Hello"}})),
            ev(json!({"type":"content_block_delta","index":0,
                "delta":{"type":"text_delta","text":" world"}})),
            ev(json!({"type":"content_block_stop","index":0})),
            ev(
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},
                "usage":{"output_tokens":15}}),
            ),
            ev(json!({"type":"message_stop"})),
        ]);
        let acc = accumulate(s).await.unwrap();
        assert_eq!(acc.content, vec![ContentBlock::text("Hello world")]);
        assert_eq!(acc.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(acc.usage.input_tokens, Some(25), "input from message_start");
        assert_eq!(
            acc.usage.output_tokens,
            Some(15),
            "output from message_delta"
        );
    }

    #[tokio::test]
    async fn assembles_streamed_tool_call() {
        // input_json_delta fragments concatenate and parse at the end.
        let s = stream(vec![
            ev(
                json!({"type":"message_start","message":{"usage":{"input_tokens":5,"output_tokens":1}}}),
            ),
            ev(json!({"type":"content_block_start","index":0,
                "content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{}}})),
            ev(json!({"type":"content_block_delta","index":0,
                "delta":{"type":"input_json_delta","partial_json":"{\"loc"}})),
            ev(json!({"type":"content_block_delta","index":0,
                "delta":{"type":"input_json_delta","partial_json":"ation\":\"Paris\"}"}})),
            ev(json!({"type":"content_block_stop","index":0})),
            ev(
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":20}}),
            ),
            ev(json!({"type":"message_stop"})),
        ]);
        let acc = accumulate(s).await.unwrap();
        assert_eq!(
            acc.content,
            vec![ContentBlock::ToolUse {
                id: "toolu_1".into(),
                name: "get_weather".into(),
                input: JsonValue::new(json!({"location": "Paris"})).unwrap(),
                cache_control: None,
            }]
        );
        assert_eq!(acc.stop_reason, Some(StopReason::ToolUse));
    }

    #[tokio::test]
    async fn preserves_block_order_thinking_then_text() {
        // spec :8840 — thinking block (index 0) precedes text (index 1).
        let s = stream(vec![
            ev(json!({"type":"content_block_start","index":0,
                "content_block":{"type":"thinking","thinking":""}})),
            ev(json!({"type":"content_block_delta","index":0,
                "delta":{"type":"thinking_delta","thinking":"reasoning"}})),
            ev(json!({"type":"content_block_delta","index":0,
                "delta":{"type":"signature_delta","signature":"sig"}})),
            ev(json!({"type":"content_block_stop","index":0})),
            ev(json!({"type":"content_block_start","index":1,
                "content_block":{"type":"text","text":""}})),
            ev(json!({"type":"content_block_delta","index":1,
                "delta":{"type":"text_delta","text":"answer"}})),
            ev(json!({"type":"content_block_stop","index":1})),
            ev(json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}})),
            ev(json!({"type":"message_stop"})),
        ]);
        let acc = accumulate(s).await.unwrap();
        assert_eq!(
            acc.content,
            vec![
                ContentBlock::Thinking {
                    thinking: "reasoning".into(),
                    signature: Some("sig".into())
                },
                ContentBlock::text("answer"),
            ]
        );
    }

    #[tokio::test]
    async fn malformed_tool_json_degrades_to_empty_object() {
        let s = stream(vec![
            ev(json!({"type":"content_block_start","index":0,
                "content_block":{"type":"tool_use","id":"t","name":"n","input":{}}})),
            ev(json!({"type":"content_block_delta","index":0,
                "delta":{"type":"input_json_delta","partial_json":"{not json"}})),
            ev(json!({"type":"content_block_stop","index":0})),
            ev(json!({"type":"message_stop"})),
        ]);
        let acc = accumulate(s).await.unwrap();
        match &acc.content[0] {
            ContentBlock::ToolUse { input, .. } => assert_eq!(input.as_value(), &json!({})),
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn eof_without_message_stop_is_error() {
        let s = stream(vec![ev(json!({"type":"content_block_start","index":0,
                "content_block":{"type":"text","text":"partial"}}))]);
        assert_eq!(accumulate(s).await, Err(AccumulateError::UnexpectedEof));
    }

    #[tokio::test]
    async fn error_event_surfaces() {
        let s = stream(vec![ev(
            json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}),
        )]);
        assert_eq!(
            accumulate(s).await,
            Err(AccumulateError::StreamError("Overloaded".into()))
        );
    }
}
