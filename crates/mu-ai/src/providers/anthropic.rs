//! Anthropic API Provider — direct API access via `ANTHROPIC_API_KEY`.
//!
//! Streams responses from `/v1/messages` with `stream: true`, parses
//! the SSE event format, translates to mu-core's `ProviderEvent`.
//!
//! See spec mu-006. v1 supports text-only responses; tools, extended
//! thinking, and image content are deferred.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{BoxStream, Stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use mu_core::agent::{
    AgentMessage, AssistantMessage, ContentBlock, MessageInput, Provider, ProviderError,
    ProviderEvent, StopReason, ToolCall, ToolSpec, Usage,
};
use mu_core::context::{
    extract_call_id_from_span_id, CacheMarker, CacheStrategy, ProviderMessage, ProviderMessages,
    ProviderRenderer, ProviderRole,
};

use crate::context::{AnthropicCacheStrategy, AnthropicProviderRenderer};

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

    /// API key from `ANTHROPIC_API_KEY`, base URL from
    /// `ANTHROPIC_BASE_URL` (defaults to api.anthropic.com).
    ///
    /// When `ANTHROPIC_BASE_URL` points at a non-default base (a local
    /// proxy or gateway), the API key is allowed to be unset or empty —
    /// the proxy handles its own auth (OAuth passthrough, OpenRouter
    /// fallback, etc.) and the `x-api-key` header value is irrelevant.
    /// When the base is the default api.anthropic.com endpoint,
    /// `ANTHROPIC_API_KEY` must be set; otherwise the request would be
    /// rejected.
    pub fn from_env(model: String) -> Result<Self, ProviderError> {
        let api_base = std::env::var("ANTHROPIC_BASE_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| ANTHROPIC_API_BASE.to_string());
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_default();
        if api_key.is_empty() && api_base == ANTHROPIC_API_BASE {
            return Err(ProviderError::Other(
                "ANTHROPIC_API_KEY not set or empty (required when ANTHROPIC_BASE_URL points at api.anthropic.com)".into(),
            ));
        }
        Ok(Self {
            client: reqwest::Client::new(),
            api_key,
            model,
            api_base,
        })
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
        system_prompt: Option<&str>,
        input: MessageInput<'_>,
        tools: &[ToolSpec],
        cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        // mu-yqeq.3: sealed-enum dispatch (Legacy + Projected). The
        // `_` arm remains for forward-compat with future MessageInput
        // variants — adding one will compile-warn here for review.
        //
        // mu-yqeq.4: Projected arm produces byte-identical wire JSON
        // to the Legacy arm; the parity test in this file asserts that
        // invariant for the canonical scenarios (text-only, single
        // tool call, consecutive tool results, system+tools, Thinking-
        // block skip). The agent loop's mod.rs:818 still passes Legacy
        // until mu-yqeq.8 wires the cutover.
        let body = match input {
            MessageInput::Legacy(msgs) => {
                build_request_body(&self.model, system_prompt, msgs, tools)
            }
            MessageInput::Projected(pmsgs) => {
                // The projection's first ProviderRole::System message
                // carries the system prompt (assemble_rope put it
                // there from the agent loop's system_prompt). The
                // `system_prompt` parameter on this method is the
                // Legacy-path channel; in the Projected path we
                // source from the projection itself for
                // self-containedness, ignoring the parameter.
                build_request_body_from_projection(&self.model, pmsgs, tools)
            }
            _ => {
                return Err(ProviderError::Other(
                    "AnthropicProvider: unrecognized MessageInput variant".to_string(),
                ));
            }
        };

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

    fn renderer(&self) -> Arc<dyn ProviderRenderer> {
        Arc::new(AnthropicProviderRenderer::new())
    }

    fn cache_strategy(&self) -> Arc<dyn CacheStrategy> {
        Arc::new(AnthropicCacheStrategy::new())
    }

    fn provider_label(&self) -> &'static str {
        "anthropic"
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

/// mu-yqeq.8 retired the unconditional `cache_control: ephemeral`
/// annotation that used to live here. `AnthropicCacheStrategy` is
/// now the sole source of Anthropic cache markers, and the Projected
/// wire emitter ([`build_request_body_from_projection`]) propagates
/// per-message `cache_marker` flags to the wire. The Legacy path is
/// retained for rollback and out-of-loop callers (parity tests,
/// embedders); it emits NO cache_control and therefore NO caching —
/// in-loop callers should always use the Projected path post-mu-yqeq.8.
pub(crate) fn build_request_body(
    model: &str,
    system_prompt: Option<&str>,
    messages: &[AgentMessage],
    tools: &[ToolSpec],
) -> Value {
    let api_messages = translate_messages(messages);
    let mut body = json!({
        "model": model,
        "max_tokens": super::output_limits::max_tokens_for_model(model),
        "stream": true,
        "messages": api_messages,
    });
    if let Some(s) = system_prompt {
        if !s.is_empty() {
            body["system"] = json!([
                {
                    "type": "text",
                    "text": s,
                }
            ]);
        }
    }
    if !tools.is_empty() {
        let tool_specs: Vec<Value> = tools.iter().map(translate_tool_spec).collect();
        body["tools"] = json!(tool_specs);
    }
    body
}

// ============================================================================
// mu-yqeq.4: Projected path — wire body built from &ProviderMessages
// ============================================================================
//
// Mirrors `translate_messages` + `build_request_body` semantics but
// reads structural `ContentBlock`s from `ProviderMessage.blocks`
// instead of `AgentMessage::Assistant.content`. Tool-result binding
// (`tool_use_id`) is recovered from each `ToolResult` message's
// `source_span_ids[0]` via the `extract_call_id_from_span_id` helper
// (mu-yqeq.3).
//
// Wire-format byte equivalence with the Legacy path is the contract;
// see `parity_*` tests below for the canonical scenarios.

/// Translate a [`ProviderMessages`] projection into Anthropic's API
/// message array shape, returning the hoisted system-prompt text (if
/// any) separately for [`build_request_body_from_projection`] to
/// place in the top-level `system` field.
///
/// Hoisting rule: the FIRST `ProviderRole::System` message's content
/// is captured as the system prompt; subsequent System messages are
/// silently dropped (assemble_rope produces at most one). Tool-schema
/// messages are skipped (the `tools` parameter on `Provider::stream`
/// is authoritative for `body.tools`). All other roles are translated
/// per the Legacy `translate_messages` rules, with consecutive
/// `ToolResult` messages batched into a single user message — Anthropic's
/// tool-use protocol requires that grouping.
fn translate_provider_messages(pmsgs: &ProviderMessages) -> (Vec<Value>, Option<String>) {
    let mut out: Vec<Value> = Vec::with_capacity(pmsgs.messages.len());
    let mut tool_result_buf: Vec<Value> = Vec::new();
    let mut system_text: Option<String> = None;

    for msg in &pmsgs.messages {
        match msg.role() {
            ProviderRole::System => {
                // System-role messages come from two distinct rope
                // sources mapped to the same provider role by
                // `From<&SpanKind> for ProviderRole`:
                //   1. The session system prompt — source span id
                //      "system-prompt" (from assemble_rope).
                //   2. Tool-schema spans — source span ids
                //      "tool-schema:{name}".
                // Only (1) is hoisted into Anthropic's top-level
                // `system` field. (2) is skipped because the `tools`
                // parameter on Provider::stream is authoritative for
                // `body.tools`; the rope's ToolSchema spans exist for
                // operator-view + cache positioning, not wire emission.
                let is_session_prompt = msg
                    .source_span_ids()
                    .first()
                    .map(|sid| sid.as_ref() == "system-prompt")
                    .unwrap_or(false);
                if is_session_prompt && system_text.is_none() {
                    system_text = Some(msg.content().to_string());
                }
            }
            ProviderRole::ToolResult => {
                tool_result_buf.push(translate_provider_tool_result(msg));
            }
            ProviderRole::User => {
                flush_tool_result_buf(&mut out, &mut tool_result_buf);
                out.push(json!({
                    "role": "user",
                    "content": msg.content(),
                }));
            }
            ProviderRole::Assistant => {
                flush_tool_result_buf(&mut out, &mut tool_result_buf);
                if let Some(translated) = translate_provider_assistant(msg) {
                    out.push(translated);
                }
            }
        }
    }
    flush_tool_result_buf(&mut out, &mut tool_result_buf);

    (out, system_text)
}

fn flush_tool_result_buf(out: &mut Vec<Value>, buf: &mut Vec<Value>) {
    if !buf.is_empty() {
        out.push(json!({
            "role": "user",
            "content": std::mem::take(buf),
        }));
    }
}

fn translate_provider_tool_result(msg: &ProviderMessage) -> Value {
    // call_id recovered from the synthesized span id
    // (`msg-{idx}-tool-result:{call_id}` — see assembly.rs). Fall back
    // to empty string if absent so a malformed projection produces a
    // visibly wrong wire payload rather than silently swallowing the
    // tool-call binding.
    let call_id: &str = msg
        .source_span_ids()
        .first()
        .and_then(|sid| extract_call_id_from_span_id(sid.as_ref()))
        .unwrap_or("");
    // is_error recovered from the "error: " prefix that
    // assembly.rs:message_to_span adds when AgentMessage::ToolResult
    // had is_error=true.
    let (is_error, content) = match msg.content().strip_prefix("error: ") {
        Some(stripped) => (true, stripped),
        None => (false, msg.content()),
    };
    json!({
        "type": "tool_result",
        "tool_use_id": call_id,
        "content": content,
        "is_error": is_error,
    })
}

/// Translate one assistant-role [`ProviderMessage`] into Anthropic's
/// `{role: "assistant", content: [...]}` block. Reads structural
/// blocks from `msg.blocks()` — populated by `assemble_rope` for
/// `AgentMessage::Assistant`. `Thinking` blocks are intentionally
/// skipped per spec mu-044 §"Thinking-block skip" (provider never
/// receives the model's own reasoning trace as input). Returns `None`
/// if the assistant produced no wire-bearing blocks (e.g. only
/// thinking) — mirrors `translate_message_single`'s elision rule.
fn translate_provider_assistant(msg: &ProviderMessage) -> Option<Value> {
    let blocks = msg.blocks()?;
    let translated: Vec<Value> = blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(json!({
                "type": "text",
                "text": text.as_ref(),
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
    if translated.is_empty() {
        None
    } else {
        Some(json!({
            "role": "assistant",
            "content": translated,
        }))
    }
}

/// Sibling of [`build_request_body`] that builds Anthropic's request
/// body from a [`ProviderMessages`] projection instead of a raw
/// `&[AgentMessage]` slice. After mu-yqeq.8, `cache_control` emission
/// is driven by per-message [`CacheMarker`] flags set by
/// [`AnthropicCacheStrategy`]: a marker on the projection's
/// `"system-prompt"` message triggers `cache_control` on `body.system`;
/// a marker on any `"tool-schema:*"` message triggers `cache_control`
/// on the last tool spec. The wire shape (minus cache_control) is
/// byte-identical to the Legacy path for the canonical scenarios —
/// asserted by `yqeq4_parity_*` tests in anthropic_tests.rs.
pub(crate) fn build_request_body_from_projection(
    model: &str,
    pmsgs: &ProviderMessages,
    tools: &[ToolSpec],
) -> Value {
    let (api_messages, hoisted_system) = translate_provider_messages(pmsgs);
    let (system_should_cache, tools_should_cache) = detect_cache_targets(pmsgs);
    let mut body = json!({
        "model": model,
        "max_tokens": super::output_limits::max_tokens_for_model(model),
        "stream": true,
        "messages": api_messages,
    });
    if let Some(s) = hoisted_system {
        if !s.is_empty() {
            let mut system_block = json!({
                "type": "text",
                "text": s,
            });
            if system_should_cache {
                system_block["cache_control"] = json!({ "type": "ephemeral" });
            }
            body["system"] = json!([system_block]);
        }
    }
    if !tools.is_empty() {
        let mut tool_specs: Vec<Value> = tools.iter().map(translate_tool_spec).collect();
        if tools_should_cache {
            if let Some(last) = tool_specs.last_mut() {
                last["cache_control"] = json!({ "type": "ephemeral" });
            }
        }
        body["tools"] = json!(tool_specs);
    }
    body
}

/// Walk the projection and determine which Anthropic wire positions
/// should carry `cache_control`. The rope strategy puts cache markers
/// on the ProviderMessage layer; this helper maps marker positions
/// back to wire positions via source-span-id discrimination.
fn detect_cache_targets(pmsgs: &ProviderMessages) -> (bool, bool) {
    let mut system_should_cache = false;
    let mut tools_should_cache = false;
    for msg in &pmsgs.messages {
        if msg.cache_marker() != Some(CacheMarker::Ephemeral) {
            continue;
        }
        let Some(sid) = msg.source_span_ids().first() else {
            continue;
        };
        let sid_str = sid.as_ref();
        if sid_str == "system-prompt" {
            system_should_cache = true;
        } else if sid_str.starts_with("tool-schema:") {
            tools_should_cache = true;
        }
        // Markers on other span kinds (User/Assistant/ToolResult)
        // don't have a current wire target — Anthropic allows
        // cache_control on content blocks but the legacy strategy
        // only marked system + tools, so we preserve that mapping.
    }
    (system_should_cache, tools_should_cache)
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
    ContentBlockStart {
        index: u32,
        content_block: AnthropicBlock,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: u32, delta: AnthropicDelta },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: u32 },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: AnthropicMessageDelta,
        /// mu-yz48: Anthropic puts the cumulative stream `usage` at the
        /// TOP level of the message_delta event, sibling to `delta` —
        /// not nested inside it. Reading `delta.usage` always returns
        /// None and leaves `output_tokens` stuck at the message_start
        /// baseline (1-5). Capture it here.
        #[serde(default)]
        usage: Option<AnthropicUsage>,
    },
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
                // Stream ended without message_stop. Emit Done with DegradedEof
                // to signal the degraded condition.
                state.finished = true;
                if !state.emitted_done {
                    state.emitted_done = true;
                    let usage = state.usage.to_usage();
                    return Some((
                        ProviderEvent::Done(AssistantMessage {
                            content: assemble_content(&state.blocks, &state.block_order),
                            stop_reason: StopReason::DegradedEof,
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
            AnthropicEvent::ContentBlockStart {
                index,
                content_block,
            } => {
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
            AnthropicEvent::MessageDelta { delta, usage } => {
                state.stop_reason = delta.stop_reason;
                // Prefer the top-level usage (the real wire location).
                // Fall back to nested delta.usage so older fixtures /
                // servers that put it inside delta still work.
                if let Some(u) = usage.as_ref().or(delta.usage.as_ref()) {
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
fn assemble_content(blocks: &HashMap<u32, BlockBuilder>, block_order: &[u32]) -> Vec<ContentBlock> {
    block_order
        .iter()
        .filter_map(|idx| blocks.get(idx))
        .map(|builder| match builder {
            BlockBuilder::Text(text) => ContentBlock::Text {
                text: text.as_str().into(),
            },
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
