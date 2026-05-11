//! Anthropic API Provider — direct API access via `ANTHROPIC_API_KEY`.
//!
//! Streams responses from `/v1/messages` with `stream: true`, parses
//! the SSE event format, translates to mu-core's `ProviderEvent`.
//!
//! See spec mu-006. v1 supports text-only responses; tools, extended
//! thinking, and image content are deferred.

use std::collections::HashMap;
use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{BoxStream, Stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use mu_core::agent::{
    AgentMessage, AssistantMessage, ContentBlock, Provider, ProviderError, ProviderEvent,
    StopReason, ToolCall, ToolSpec, Usage,
};

use super::sse::{SseEvent, SseStream};

const ANTHROPIC_API_BASE: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Direct API Provider. Holds an API key (ENV-sourced is fine — this
/// isn't an OAuth token).
pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    api_base: String,
}

impl AnthropicProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model,
            api_base: ANTHROPIC_API_BASE.to_string(),
        }
    }

    /// API key from `ANTHROPIC_API_KEY`. Fails if unset or empty.
    pub fn from_env(model: String) -> Result<Self, ProviderError> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ProviderError::Other("ANTHROPIC_API_KEY not set or empty".into()))?;
        Ok(Self::new(api_key, model))
    }

    /// Test hook: override the API base URL for mock servers.
    pub fn with_api_base(mut self, base: String) -> Self {
        self.api_base = base;
        self
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn stream(
        &self,
        messages: &[AgentMessage],
        tools: &[ToolSpec],
        cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let body = build_request_body(&self.model, messages, tools);

        let resp = self
            .client
            .post(format!("{}/v1/messages", self.api_base))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Other(format!("anthropic request: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Other(format!(
                "anthropic returned {status}: {text}"
            )));
        }

        let bytes = resp.bytes_stream();
        Ok(events_stream(bytes, cancel_rx))
    }
}

/// Translate a mu `ToolSpec` into Anthropic's tool descriptor shape.
pub(crate) fn translate_tool_spec(spec: &ToolSpec) -> Value {
    json!({
        "name": spec.name,
        "description": spec.description,
        "input_schema": spec.input_schema,
    })
}

/// Translate messages into Anthropic's API message shape.
///
/// Consecutive tool results are batched into a single user message
/// containing multiple `tool_result` content blocks, as required by
/// Anthropic's tool-use protocol.
pub(crate) fn translate_messages(messages: &[AgentMessage]) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len());
    let mut tool_result_buf = Vec::new();

    for message in messages {
        match message {
            AgentMessage::ToolResult {
                call_id,
                content,
                is_error,
            } => {
                tool_result_buf.push(json!({
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": content,
                    "is_error": is_error,
                }));
            }
            other => {
                if !tool_result_buf.is_empty() {
                    out.push(json!({
                        "role": "user",
                        "content": std::mem::take(&mut tool_result_buf),
                    }));
                }
                if let Some(translated) = translate_message_single(other) {
                    out.push(translated);
                }
            }
        }
    }

    if !tool_result_buf.is_empty() {
        out.push(json!({
            "role": "user",
            "content": tool_result_buf,
        }));
    }

    out
}

/// Single-message translation for non-ToolResult variants. ToolResult
/// is handled by `translate_messages` because Anthropic requires
/// consecutive tool results to be grouped in one user message.
fn translate_message_single(m: &AgentMessage) -> Option<Value> {
    match m {
        AgentMessage::User { content } => Some(json!({
            "role": "user",
            "content": content,
        })),
        AgentMessage::Assistant(a) => {
            let blocks: Vec<Value> = a
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(json!({
                        "type": "text",
                        "text": text,
                    })),
                    ContentBlock::ToolCall(tool_call) => Some(json!({
                        "type": "tool_use",
                        "id": tool_call.id,
                        "name": tool_call.name,
                        "input": tool_call.arguments,
                    })),
                    ContentBlock::Thinking { .. } => None,
                })
                .collect();
            if blocks.is_empty() {
                None
            } else {
                Some(json!({
                    "role": "assistant",
                    "content": blocks,
                }))
            }
        }
        AgentMessage::ToolResult { .. } => None,
    }
}

pub(crate) fn build_request_body(
    model: &str,
    messages: &[AgentMessage],
    tools: &[ToolSpec],
) -> Value {
    let api_messages = translate_messages(messages);
    let mut body = json!({
        "model": model,
        "max_tokens": 4096,
        "stream": true,
        "messages": api_messages,
    });
    if !tools.is_empty() {
        body["tools"] = json!(tools.iter().map(translate_tool_spec).collect::<Vec<_>>());
    }
    body
}

// ============================================================================
// Anthropic SSE event types — minimal subset we care about
// ============================================================================

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
#[allow(dead_code)] // Some fields are present for future use; keep deserializer faithful to API.
enum AnthropicEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: AnthropicMessageMeta },
    #[serde(rename = "content_block_start")]
    ContentBlockStart { index: u32, content_block: AnthropicBlock },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: u32, delta: AnthropicDelta },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: u32 },
    #[serde(rename = "message_delta")]
    MessageDelta { delta: AnthropicMessageDelta },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "error")]
    Error { error: AnthropicErrorBody },
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct AnthropicMessageMeta {
    id: Option<String>,
    role: Option<String>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[allow(dead_code)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
#[allow(dead_code)] // `text` is read from content_block_start to seed the
                    // text block; other fields are present for future use.
enum AnthropicBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AnthropicDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct AnthropicMessageDelta {
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
struct AnthropicErrorBody {
    #[serde(rename = "type")]
    err_type: Option<String>,
    message: Option<String>,
}

fn map_stop_reason(s: Option<&str>) -> StopReason {
    match s {
        Some("end_turn") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("stop_sequence") => StopReason::EndTurn,
        Some(other) => {
            tracing::warn!(stop_reason = %other, "unrecognized anthropic stop_reason");
            StopReason::EndTurn
        }
        None => StopReason::EndTurn,
    }
}

// ============================================================================
// SSE → ProviderEvent translation
// ============================================================================

/// Build a stream of `ProviderEvent`s from a stream of bytes (the raw
/// HTTP body). Owns `cancel_rx`; when it fires, the stream terminates.
fn events_stream(
    bytes: impl Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
    cancel_rx: oneshot::Receiver<()>,
) -> BoxStream<'static, ProviderEvent> {
    // Box::pin the input to satisfy SseStream's `S: Unpin` bound;
    // reqwest::Response::bytes_stream() returns a !Unpin type.
    let bytes: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>> = Box::pin(bytes);
    let sse = SseStream::new(bytes);
    let state = StreamState {
        sse: Box::pin(sse),
        blocks: HashMap::new(),
        block_order: Vec::new(),
        stop_reason: None,
        usage: AnthropicUsage::default(),
        cancel_rx: Some(cancel_rx),
        finished: false,
        emitted_done: false,
    };
    Box::pin(futures::stream::unfold(state, next_event))
}

/// Per-content-block accumulator. Anthropic streams content in
/// indexed blocks; each `content_block_start` opens one, deltas
/// append to it, `content_block_stop` closes it. Blocks may be text
/// or tool_use (or others we ignore for v1).
enum BlockBuilder {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        /// Accumulated input JSON; parsed at `message_stop` time.
        input_json: String,
    },
}

struct StreamState {
    sse: Pin<Box<dyn Stream<Item = SseEvent> + Send>>,
    /// Open content blocks indexed by Anthropic's block index. Built
    /// up as start/delta/stop events arrive; finalized into
    /// `AssistantMessage::content` at `message_stop`.
    blocks: HashMap<u32, BlockBuilder>,
    /// Order in which blocks were opened. Used so the final content
    /// vec reflects Anthropic's intended order regardless of
    /// HashMap iteration order.
    block_order: Vec<u32>,
    stop_reason: Option<String>,
    /// Combined usage from message_start (input tokens, cache stats)
    /// and message_delta (output tokens). Anthropic splits across two
    /// events; we merge as we see them.
    usage: AnthropicUsage,
    cancel_rx: Option<oneshot::Receiver<()>>,
    finished: bool,
    emitted_done: bool,
}

impl AnthropicUsage {
    fn merge(&mut self, other: &AnthropicUsage) {
        if other.input_tokens.is_some() {
            self.input_tokens = other.input_tokens;
        }
        if other.output_tokens.is_some() {
            self.output_tokens = other.output_tokens;
        }
        if other.cache_creation_input_tokens.is_some() {
            self.cache_creation_input_tokens = other.cache_creation_input_tokens;
        }
        if other.cache_read_input_tokens.is_some() {
            self.cache_read_input_tokens = other.cache_read_input_tokens;
        }
    }

    fn to_usage(&self) -> Option<Usage> {
        if self.input_tokens.is_none() && self.output_tokens.is_none() {
            return None;
        }
        Some(Usage {
            input_tokens: self.input_tokens.unwrap_or(0),
            output_tokens: self.output_tokens.unwrap_or(0),
            cache_read_input_tokens: self.cache_read_input_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens,
            reasoning_tokens: None,
        })
    }
}

async fn next_event(mut state: StreamState) -> Option<(ProviderEvent, StreamState)> {
    if state.finished {
        return None;
    }

    loop {
        // Cancel?
        if let Some(rx) = state.cancel_rx.as_mut() {
            match rx.try_recv() {
                Ok(_) => {
                    state.finished = true;
                    state.cancel_rx = None;
                    let usage = state.usage.to_usage();
                    return Some((
                        ProviderEvent::Done(AssistantMessage {
                            content: assemble_content(&state.blocks, &state.block_order),
                            stop_reason: StopReason::Aborted,
                            usage,
                        }),
                        state,
                    ));
                }
                Err(oneshot::error::TryRecvError::Empty) => {} // continue
                Err(oneshot::error::TryRecvError::Closed) => {
                    state.cancel_rx = None;
                }
            }
        }

        // Pull next SSE event.
        let sse_event = match state.sse.next().await {
            Some(e) => e,
            None => {
                // Stream ended without message_stop. Emit Done if we
                // haven't yet.
                state.finished = true;
                if !state.emitted_done {
                    state.emitted_done = true;
                    let stop = map_stop_reason(state.stop_reason.as_deref());
                    let usage = state.usage.to_usage();
                    return Some((
                        ProviderEvent::Done(AssistantMessage {
                            content: assemble_content(&state.blocks, &state.block_order),
                            stop_reason: stop,
                            usage,
                        }),
                        state,
                    ));
                }
                return None;
            }
        };

        // Parse the JSON payload as an Anthropic event.
        let parsed: Result<AnthropicEvent, _> = serde_json::from_str(&sse_event.data);
        let parsed = match parsed {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, data = %sse_event.data, "failed to parse anthropic event");
                continue;
            }
        };

        match parsed {
            AnthropicEvent::ContentBlockStart { index, content_block } => {
                // Register a new block. Re-registering an index is a
                // protocol violation; we replace silently.
                let builder = match content_block {
                    AnthropicBlock::Text { text } => BlockBuilder::Text(text),
                    AnthropicBlock::ToolUse { id, name } => BlockBuilder::ToolUse {
                        id,
                        name,
                        input_json: String::new(),
                    },
                    AnthropicBlock::Other => continue,
                };
                if !state.blocks.contains_key(&index) {
                    state.block_order.push(index);
                }
                state.blocks.insert(index, builder);
            }
            AnthropicEvent::ContentBlockDelta { index, delta } => match delta {
                AnthropicDelta::TextDelta { text } => {
                    if let Some(BlockBuilder::Text(buf)) = state.blocks.get_mut(&index) {
                        buf.push_str(&text);
                    } else {
                        // No matching text block — uncommon; treat as
                        // implicit start. Push to a fresh text block.
                        if !state.blocks.contains_key(&index) {
                            state.block_order.push(index);
                        }
                        state.blocks.insert(index, BlockBuilder::Text(text.clone()));
                    }
                    return Some((ProviderEvent::TextDelta(text), state));
                }
                AnthropicDelta::InputJsonDelta { partial_json } => {
                    if let Some(BlockBuilder::ToolUse { input_json, .. }) =
                        state.blocks.get_mut(&index)
                    {
                        input_json.push_str(&partial_json);
                    } else {
                        // Delta for a tool block we never saw start —
                        // protocol violation. Log and ignore.
                        tracing::warn!(
                            index,
                            "input_json_delta arrived for unknown or non-tool block"
                        );
                    }
                    // v1: don't emit ProviderEvent::ToolCallDelta; the
                    // loop ignores it. Final tool calls are surfaced
                    // in the Done payload.
                }
                AnthropicDelta::Other => {
                    // Unknown delta type (e.g., future thinking_delta);
                    // ignore.
                }
            },
            AnthropicEvent::ContentBlockStop { .. } => {
                // No-op for v1; the block stays in the map until
                // assembled at message_stop.
            }
            AnthropicEvent::MessageDelta { delta } => {
                state.stop_reason = delta.stop_reason;
                if let Some(u) = delta.usage.as_ref() {
                    state.usage.merge(u);
                }
            }
            AnthropicEvent::MessageStop => {
                state.finished = true;
                state.emitted_done = true;
                let stop = map_stop_reason(state.stop_reason.as_deref());
                let usage = state.usage.to_usage();
                return Some((
                    ProviderEvent::Done(AssistantMessage {
                        content: assemble_content(&state.blocks, &state.block_order),
                        stop_reason: stop,
                        usage,
                    }),
                    state,
                ));
            }
            AnthropicEvent::Error { error } => {
                state.finished = true;
                state.emitted_done = true;
                let msg = format!(
                    "anthropic stream error ({}): {}",
                    error.err_type.unwrap_or_else(|| "unknown".to_string()),
                    error.message.unwrap_or_else(|| "(no message)".to_string()),
                );
                return Some((ProviderEvent::Error(msg), state));
            }
            AnthropicEvent::MessageStart { message } => {
                if let Some(u) = message.usage.as_ref() {
                    state.usage.merge(u);
                }
            }
            AnthropicEvent::Ping => {
                // No-op.
            }
        }
    }
}

/// Assemble the final assistant content from accumulated blocks.
/// Walks `block_order` so the result reflects Anthropic's intended
/// order regardless of HashMap iteration order. Tool-use blocks
/// parse their accumulated input_json; on parse error or non-object
/// result, falls back to an empty object per INV-5.
fn assemble_content(
    blocks: &HashMap<u32, BlockBuilder>,
    block_order: &[u32],
) -> Vec<ContentBlock> {
    block_order
        .iter()
        .filter_map(|idx| blocks.get(idx))
        .map(|builder| match builder {
            BlockBuilder::Text(text) => ContentBlock::Text { text: text.clone() },
            BlockBuilder::ToolUse {
                id,
                name,
                input_json,
            } => {
                let arguments = parse_tool_input(input_json);
                ContentBlock::ToolCall(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments,
                })
            }
        })
        .collect()
}

/// Parse a tool's accumulated input JSON. On any failure (malformed
/// JSON, valid JSON that isn't an object), fall back to an empty
/// object and log a warning. Tools expect an object of arguments;
/// passing through arrays/strings/numbers would break the contract
/// with `Tool::execute`.
fn parse_tool_input(input_json: &str) -> Value {
    if input_json.is_empty() {
        return Value::Object(serde_json::Map::new());
    }
    match serde_json::from_str::<Value>(input_json) {
        Ok(v) if v.is_object() => v,
        Ok(other) => {
            tracing::warn!(
                value = %other,
                "tool input JSON was valid but not an object; using empty object"
            );
            Value::Object(serde_json::Map::new())
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                raw = %input_json,
                "failed to parse tool input JSON; using empty object"
            );
            Value::Object(serde_json::Map::new())
        }
    }
}

#[cfg(test)]
#[path = "anthropic_tests.rs"]
mod tests;
