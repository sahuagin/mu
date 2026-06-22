//! Async accumulator — folds a stream of [`ResponseStreamEvent`]s into the final
//! [`Response`]. The SDK convenience shape: consume the SSE event stream and
//! yield the assembled response at the terminal lifecycle event.
//!
//! Async + streaming only. The caller owns transport and SSE-line framing; this
//! consumes the already-typed events.
//!
//! Unlike Anthropic (where the final message is assembled purely from deltas),
//! the OpenAI Responses API sends the **authoritative full `Response`** on the
//! terminal `response.completed` / `.failed` / `.incomplete` event — so that is
//! the primary source of truth. Text and function-call-argument deltas are
//! accumulated too, and used only to BACKFILL a terminal response whose `output`
//! came back empty (a degraded backend), so a partial turn is still usable.

use std::collections::BTreeMap;

use futures::{Stream, StreamExt};

use crate::response::{OutputContent, OutputItem, Response};
use crate::stream::ResponseStreamEvent;

/// Error from accumulating a stream.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AccumulateError {
    /// The stream ended without a terminal lifecycle event (truncated/dropped).
    #[error("stream ended before a terminal response event (degraded)")]
    UnexpectedEof,
    /// An `error` / `response.error` event arrived mid-stream.
    #[error("stream error event: {0}")]
    StreamError(String),
}

/// Accumulated, in first-seen order, for the degraded backfill path.
#[derive(Default)]
struct Deltas {
    text: String,
    /// item_id → builder, plus first-seen order.
    calls: BTreeMap<String, CallBuilder>,
    call_order: Vec<String>,
}

#[derive(Default)]
struct CallBuilder {
    call_id: Option<String>,
    name: Option<String>,
    args: String,
}

impl Deltas {
    fn call_mut(&mut self, item_id: &str) -> &mut CallBuilder {
        if !self.calls.contains_key(item_id) {
            self.call_order.push(item_id.to_string());
            self.calls
                .insert(item_id.to_string(), CallBuilder::default());
        }
        self.calls.get_mut(item_id).expect("just inserted")
    }

    /// Build `output` items from the accumulated deltas (degraded backfill).
    fn into_output(self) -> Vec<OutputItem> {
        let Deltas {
            text,
            mut calls,
            call_order,
        } = self;
        let mut out = Vec::new();
        if !text.is_empty() {
            out.push(OutputItem::Message {
                id: String::new(),
                role: Some("assistant".into()),
                status: None,
                content: vec![OutputContent::OutputText {
                    text,
                    annotations: Vec::new(),
                }],
            });
        }
        for id in call_order {
            if let Some(b) = calls.remove(&id) {
                out.push(OutputItem::FunctionCall {
                    id,
                    call_id: b.call_id,
                    name: b.name,
                    arguments: Some(b.args),
                    status: None,
                });
            }
        }
        out
    }
}

/// Fold a stream of typed events into the final [`Response`].
pub async fn accumulate<S>(mut events: S) -> Result<Response, AccumulateError>
where
    S: Stream<Item = ResponseStreamEvent> + Unpin,
{
    let mut snapshot: Option<Response> = None;
    let mut deltas = Deltas::default();

    while let Some(ev) = events.next().await {
        match ev {
            // Terminal: authoritative final snapshot. Backfill if output empty.
            ResponseStreamEvent::Completed { response, .. }
            | ResponseStreamEvent::Failed { response, .. }
            | ResponseStreamEvent::Incomplete { response, .. } => {
                let mut r = response;
                if r.output.is_empty() {
                    r.output = deltas.into_output();
                }
                return Ok(r);
            }
            // Non-terminal lifecycle snapshots: keep the latest as a fallback.
            ResponseStreamEvent::Created { response, .. }
            | ResponseStreamEvent::InProgress { response, .. }
            | ResponseStreamEvent::Queued { response, .. } => {
                snapshot = Some(response);
            }
            // Text deltas → backfill accumulator.
            ResponseStreamEvent::OutputTextDelta { delta, .. } => deltas.text.push_str(&delta),
            // Function-call argument deltas → per-item accumulator.
            ResponseStreamEvent::FunctionCallArgumentsDelta { item_id, delta, .. }
            | ResponseStreamEvent::FunctionCallArgumentsDeltaCompat { item_id, delta, .. } => {
                deltas.call_mut(&item_id).args.push_str(&delta);
            }
            ResponseStreamEvent::FunctionCallArgumentsDone {
                item_id,
                name,
                arguments,
                ..
            } => {
                let b = deltas.call_mut(&item_id);
                b.args = arguments; // the `done` event carries the full JSON
                if name.is_some() {
                    b.name = name;
                }
            }
            // Identify call_id/name as items are announced.
            ResponseStreamEvent::OutputItemAdded { item, .. }
            | ResponseStreamEvent::OutputItemDone { item, .. } => {
                if let OutputItem::FunctionCall {
                    id, call_id, name, ..
                } = item
                {
                    let b = deltas.call_mut(&id);
                    if call_id.is_some() {
                        b.call_id = call_id;
                    }
                    if name.is_some() {
                        b.name = name;
                    }
                }
            }
            // An error event ends the stream as an error.
            ResponseStreamEvent::ResponseError { message, .. } => {
                return Err(AccumulateError::StreamError(message));
            }
            ResponseStreamEvent::Error { message, code, .. } => {
                let msg = message
                    .or(code)
                    .unwrap_or_else(|| "openai stream error".into());
                return Err(AccumulateError::StreamError(msg));
            }
            // Everything else (content_part, reasoning deltas, refusal, unknown)
            // doesn't change the assembled output here — the terminal Response
            // carries the canonical content (incl. reasoning items for threading).
            _ => {}
        }
    }

    // Stream ended without a terminal event. A non-terminal snapshot is a
    // usable (if degraded) result; otherwise the stream was truncated.
    match snapshot {
        Some(mut r) => {
            if r.output.is_empty() {
                r.output = deltas.into_output();
            }
            Ok(r)
        }
        None => Err(AccumulateError::UnexpectedEof),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JsonValue;
    use futures::stream;
    use serde_json::json;

    fn ev(v: serde_json::Value) -> ResponseStreamEvent {
        serde_json::from_value(v).unwrap()
    }

    #[tokio::test]
    async fn returns_terminal_response_authoritatively() {
        let events = vec![
            ev(json!({"type": "response.created", "sequence_number": 0,
                      "response": {"id": "r", "status": "in_progress"}})),
            ev(json!({"type": "response.output_text.delta", "delta": "hel",
                      "sequence_number": 1})),
            ev(json!({"type": "response.output_text.delta", "delta": "lo",
                      "sequence_number": 2})),
            ev(
                json!({"type": "response.completed", "sequence_number": 3, "response": {
                    "id": "r", "status": "completed",
                    "output": [{"id": "msg_1", "type": "message", "role": "assistant",
                                "content": [{"type": "output_text", "text": "hello"}]}],
                    "usage": {"input_tokens": 1, "output_tokens": 2}
                }}),
            ),
        ];
        let r = accumulate(stream::iter(events)).await.unwrap();
        assert_eq!(r.output_text(), "hello");
        assert_eq!(r.usage.unwrap().output_tokens, Some(2));
    }

    #[tokio::test]
    async fn backfills_text_when_terminal_output_empty() {
        // Degraded backend: deltas arrive but completed.response.output is [].
        let events = vec![
            ev(
                json!({"type": "response.output_text.delta", "delta": "partial",
                      "sequence_number": 1}),
            ),
            ev(json!({"type": "response.completed", "sequence_number": 2,
                      "response": {"id": "r", "status": "completed"}})),
        ];
        let r = accumulate(stream::iter(events)).await.unwrap();
        assert_eq!(r.output_text(), "partial");
    }

    #[tokio::test]
    async fn backfills_function_call_from_deltas() {
        let events = vec![
            ev(
                json!({"type": "response.output_item.added", "output_index": 0,
                      "sequence_number": 0,
                      "item": {"type": "function_call", "id": "fc_1", "call_id": "c1",
                               "name": "read", "arguments": ""}}),
            ),
            ev(
                json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1",
                      "output_index": 0, "delta": "{\"p\":", "sequence_number": 1}),
            ),
            ev(
                json!({"type": "response.function_call_arguments.done", "item_id": "fc_1",
                      "output_index": 0, "arguments": "{\"p\":1}", "sequence_number": 2}),
            ),
            ev(json!({"type": "response.completed", "sequence_number": 3,
                      "response": {"id": "r", "status": "completed"}})),
        ];
        let r = accumulate(stream::iter(events)).await.unwrap();
        match &r.output[0] {
            OutputItem::FunctionCall {
                call_id,
                name,
                arguments,
                ..
            } => {
                assert_eq!(call_id.as_deref(), Some("c1"));
                assert_eq!(name.as_deref(), Some("read"));
                assert_eq!(arguments.as_deref(), Some("{\"p\":1}"));
            }
            other => panic!("expected function_call, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn error_event_is_an_error() {
        let events = vec![ev(
            json!({"type": "response.error", "message": "boom", "sequence_number": 1}),
        )];
        let err = accumulate(stream::iter(events)).await.unwrap_err();
        assert_eq!(err, AccumulateError::StreamError("boom".into()));
    }

    #[tokio::test]
    async fn truncated_stream_is_unexpected_eof() {
        let events = vec![ev(
            json!({"type": "response.output_text.delta", "delta": "x",
                                    "sequence_number": 1}),
        )];
        let err = accumulate(stream::iter(events)).await.unwrap_err();
        assert_eq!(err, AccumulateError::UnexpectedEof);
        // A JsonValue import keeps the dependency honest for fixtures.
        let _ = JsonValue::empty_object();
    }
}
