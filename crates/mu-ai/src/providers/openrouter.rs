//! OpenRouter provider — HTTP+key access to many models behind one
//! API. OpenAI-compatible chat-completions endpoint with streaming.
//!
//! See spec mu-017. Supports tools and streaming. Same shape as
//! AnthropicProvider but a different wire format (deltas-by-index
//! rather than content-blocks-with-explicit-events).

use std::collections::HashMap;
use std::pin::Pin;

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
    extract_call_id_from_span_id, ProviderMessage, ProviderMessages, ProviderRole,
};

use super::sse::{SseEvent, SseStream};

const OPENROUTER_API_BASE: &str = "https://openrouter.ai";

pub struct OpenRouterProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    api_base: String,
}

impl OpenRouterProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model,
            api_base: OPENROUTER_API_BASE.to_string(),
        }
    }

    /// API key from `OPENROUTER_API_KEY`. Fails if unset or empty.
    pub fn from_env(model: String) -> Result<Self, ProviderError> {
        let api_key = std::env::var("OPENROUTER_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ProviderError::Other("OPENROUTER_API_KEY not set or empty".into()))?;
        Ok(Self::new(api_key, model))
    }

    /// Test hook: override the API base URL.
    pub fn with_api_base(mut self, base: String) -> Self {
        self.api_base = base;
        self
    }
}

#[async_trait]
impl Provider for OpenRouterProvider {
    async fn stream(
        &self,
        system_prompt: Option<&str>,
        input: MessageInput<'_>,
        tools: &[ToolSpec],
        cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        // mu-yqeq.6: sealed-enum dispatch (Legacy + Projected). The
        // `_` arm remains for forward-compat with future MessageInput
        // variants — adding one will compile-warn here for review.
        //
        // Projected arm produces byte-identical wire JSON to the
        // Legacy arm; the `yqeq6_parity_*` tests in
        // openrouter_tests.rs assert that invariant for the canonical
        // scenarios. The agent loop's mod.rs:818 still passes Legacy
        // until mu-yqeq.8 wires the cutover.
        let body = match input {
            MessageInput::Legacy(msgs) => {
                build_request_body(&self.model, system_prompt, msgs, tools)
            }
            MessageInput::Projected(pmsgs) => {
                // The projection itself carries the session system
                // prompt (assemble_rope put it there from
                // `system_prompt`); the helper prepends it as a
                // `{role: "system", ...}` message when non-empty —
                // matching Legacy's `mu-n48` prepend logic.
                build_request_body_from_projection(&self.model, pmsgs, tools)
            }
            _ => {
                return Err(ProviderError::Other(
                    "OpenRouterProvider: unrecognized MessageInput variant".to_string(),
                ));
            }
        };
        let resp = self
            .client
            .post(format!("{}/api/v1/chat/completions", self.api_base))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .header("X-Title", "mu")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Other(format!("openrouter request: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Other(format!(
                "openrouter returned {status}: {text}"
            )));
        }

        let bytes = resp.bytes_stream();
        Ok(events_stream(bytes, cancel_rx))
    }

    /// Identify as `"openrouter"` so ContextAssembly events and
    /// downstream diagnostics don't see the default `"faux"` label.
    /// Matches the snake_case wire `provider_kind` enum.
    fn provider_label(&self) -> &'static str {
        "openrouter"
    }
}

// ============================================================================
// Request side
// ============================================================================

pub(crate) fn translate_tool_spec(spec: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": spec.name,
            "description": spec.description,
            "parameters": spec.input_schema,
        }
    })
}

/// Translate mu's AgentMessage into the OpenAI/OpenRouter shape.
/// Returns None for messages that don't have a wire equivalent in
/// v1 (Thinking content blocks, etc.).
pub(crate) fn translate_message(m: &AgentMessage) -> Option<Value> {
    match m {
        AgentMessage::User { content } => Some(json!({
            "role": "user",
            "content": content,
        })),
        AgentMessage::Assistant(a) => {
            // Concatenate text blocks; collect tool calls separately.
            let mut text_parts: Vec<String> = Vec::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            for block in &a.content {
                match block {
                    ContentBlock::Text { text } => text_parts.push(text.to_string()),
                    ContentBlock::ToolCall(tc) => {
                        // OpenAI puts arguments as a string-encoded JSON.
                        let args_str = serde_json::to_string(&tc.arguments)
                            .unwrap_or_else(|_| "{}".to_string());
                        tool_calls.push(json!({
                            "id": tc.id,
                            "type": "function",
                            "function": {
                                "name": tc.name,
                                "arguments": args_str,
                            }
                        }));
                    }
                    ContentBlock::Thinking { .. } => {
                        // OpenRouter doesn't have a public "thinking"
                        // content type in this API; v1 drops them.
                    }
                }
            }
            let content = text_parts.join("");
            let mut obj = json!({"role": "assistant"});
            if !content.is_empty() {
                obj["content"] = Value::String(content);
            }
            if !tool_calls.is_empty() {
                obj["tool_calls"] = Value::Array(tool_calls);
            }
            // OpenAI requires content to be present (can be null) when
            // tool_calls is present. Set null explicitly if neither
            // is set, but normally one of them is.
            if obj.get("content").is_none() && obj.get("tool_calls").is_none() {
                return None;
            }
            Some(obj)
        }
        AgentMessage::ToolResult {
            call_id,
            content,
            is_error,
        } => {
            // OpenAI's tool message has no is_error field; embed it
            // in the content text so the model knows.
            let content = if *is_error {
                format!("[error] {content}")
            } else {
                content.clone()
            };
            Some(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": content,
            }))
        }
    }
}

pub(crate) fn build_request_body(
    model: &str,
    system_prompt: Option<&str>,
    messages: &[AgentMessage],
    tools: &[ToolSpec],
) -> Value {
    // mu-n48: OpenAI-style providers express the system prompt as the
    // first message in the array with role="system". Build the
    // messages list with the system message PREPENDED (when set) so
    // the rest of the wire format stays untouched.
    let mut api_messages: Vec<Value> = Vec::new();
    if let Some(s) = system_prompt {
        if !s.is_empty() {
            api_messages.push(json!({ "role": "system", "content": s }));
        }
    }
    api_messages.extend(messages.iter().filter_map(translate_message));
    let mut body = json!({
        "model": model,
        "max_tokens": super::output_limits::max_tokens_for_model(model),
        "stream": true,
        // Ask the streamer to emit a final usage chunk; without this,
        // most OpenAI-compatible backends omit usage from streaming
        // responses entirely.
        "stream_options": {"include_usage": true},
        "messages": api_messages,
    });
    if !tools.is_empty() {
        body["tools"] = json!(tools.iter().map(translate_tool_spec).collect::<Vec<_>>());
    }
    body
}

// ============================================================================
// mu-yqeq.6: Projected path — wire body built from &ProviderMessages
// ============================================================================
//
// Mirrors `translate_message` + `build_request_body` semantics but
// reads structural `ContentBlock`s from `ProviderMessage.blocks`
// instead of `AgentMessage::Assistant.content`. The session
// system-prompt span (`source_span_ids[0] == "system-prompt"`) is
// PREPENDED to the messages array as `{role: "system", content: ...}`
// (matching Legacy's `mu-n48` behavior). Tool-schema spans are
// silently dropped (the `tools` parameter is authoritative for
// `body.tools`).
//
// Wire-format byte equivalence with the Legacy path is the contract;
// see `yqeq6_parity_*` tests in openrouter_tests.rs for the canonical
// scenarios.

/// Translate a [`ProviderMessages`] projection into the OpenAI
/// chat-completions `messages` array shape. The session
/// system-prompt (if any, and non-empty) is emitted as the FIRST
/// message with `role: "system"`, matching Legacy's `mu-n48`
/// prepend behavior.
fn translate_provider_messages_openrouter(pmsgs: &ProviderMessages) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(pmsgs.messages.len());
    // mu-745h: accumulator for all non-tool-schema System-role spans.
    // Concatenated and prepended as a single leading role=system
    // message at the end of the loop. Pre-fix this branch only
    // emitted the "system-prompt"-id span and dropped every other
    // System-role span (memory-recall:*, project-file:*) silently —
    // invisible in yqeq6_parity_* tests because pre-mu-phl ropes had
    // no such spans. Codex sibling fix is mu-2puu.
    let mut system_content: Option<String> = None;

    for msg in &pmsgs.messages {
        match msg.role() {
            ProviderRole::System => {
                // Hoist ALL System-role spans EXCEPT tool-schema:*
                // (tool schemas are passed separately via the wire's
                // `tools` array). Includes:
                //   - "system-prompt" (session system_prompt)
                //   - "memory-recall:*" (SubprocessRecallProvider)
                //   - "project-file:*" (ProjectFileRecallProvider)
                //   - any other future System-role span kind
                let is_tool_schema = msg
                    .source_span_ids()
                    .first()
                    .map(|sid| sid.as_ref().starts_with("tool-schema:"))
                    .unwrap_or(false);
                if !is_tool_schema {
                    let content = msg.content();
                    if !content.is_empty() {
                        match system_content.as_mut() {
                            Some(existing) => {
                                existing.push_str("\n\n");
                                existing.push_str(content);
                            }
                            None => {
                                system_content = Some(content.to_string());
                            }
                        }
                    }
                }
            }
            ProviderRole::User => {
                out.push(json!({
                    "role": "user",
                    "content": msg.content(),
                }));
            }
            ProviderRole::Assistant => {
                if let Some(translated) = translate_provider_assistant_openrouter(msg) {
                    out.push(translated);
                }
            }
            ProviderRole::ToolResult => {
                out.push(translate_provider_tool_result_openrouter(msg));
            }
        }
    }

    // Prepend the accumulated system content as a single leading
    // role=system message. The Chat Completions API canonically
    // expects one system slot at the start; concatenating
    // produces consistent behavior across upstream OpenRouter
    // models that handle multiple system messages inconsistently.
    if let Some(content) = system_content {
        out.insert(
            0,
            json!({
                "role": "system",
                "content": content,
            }),
        );
    }

    out
}

/// Translate one assistant-role [`ProviderMessage`] into the
/// OpenAI chat-completions shape: a single message with combined
/// `content` text plus a `tool_calls` array. Mirrors the Legacy
/// `translate_message` Assistant arm exactly. `Thinking` blocks are
/// skipped per spec mu-044 §"Thinking-block skip". Returns `None`
/// if neither text nor tool calls are present (mirrors the Legacy
/// elision rule at openrouter.rs:166-169).
fn translate_provider_assistant_openrouter(msg: &ProviderMessage) -> Option<Value> {
    let blocks = msg.blocks()?;
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => text_parts.push(text.to_string()),
            ContentBlock::ToolCall(tc) => {
                let args_str =
                    serde_json::to_string(&tc.arguments).unwrap_or_else(|_| "{}".to_string());
                tool_calls.push(json!({
                    "id": tc.id,
                    "type": "function",
                    "function": {
                        "name": tc.name,
                        "arguments": args_str,
                    }
                }));
            }
            ContentBlock::Thinking { .. } => {}
        }
    }
    let content = text_parts.join("");
    let mut obj = json!({"role": "assistant"});
    if !content.is_empty() {
        obj["content"] = Value::String(content);
    }
    if !tool_calls.is_empty() {
        obj["tool_calls"] = Value::Array(tool_calls);
    }
    if obj.get("content").is_none() && obj.get("tool_calls").is_none() {
        return None;
    }
    Some(obj)
}

/// Translate one tool-result [`ProviderMessage`] into a
/// `{role: "tool", tool_call_id, content}` message. `tool_call_id`
/// recovered from the synthesized span id via
/// `extract_call_id_from_span_id` (mu-yqeq.3 helper); `is_error`
/// recovered from the `"error: "` prefix added by
/// `assembly.rs::message_to_span`. Errors re-encoded as
/// `"[error] {content}"` matching Legacy
/// `translate_message::AgentMessage::ToolResult`.
fn translate_provider_tool_result_openrouter(msg: &ProviderMessage) -> Value {
    let call_id: &str = msg
        .source_span_ids()
        .first()
        .and_then(|sid| extract_call_id_from_span_id(sid.as_ref()))
        .unwrap_or("");
    let content: String = match msg.content().strip_prefix("error: ") {
        Some(stripped) => format!("[error] {stripped}"),
        None => msg.content().to_string(),
    };
    json!({
        "role": "tool",
        "tool_call_id": call_id,
        "content": content,
    })
}

/// Sibling of [`build_request_body`] that builds the OpenRouter
/// (OpenAI chat-completions) request body from a
/// [`ProviderMessages`] projection instead of a raw
/// `&[AgentMessage]` slice. Wire JSON is byte-identical to the
/// Legacy path for the canonical scenarios (asserted by
/// `yqeq6_parity_*` tests).
pub(crate) fn build_request_body_from_projection(
    model: &str,
    pmsgs: &ProviderMessages,
    tools: &[ToolSpec],
) -> Value {
    let api_messages = translate_provider_messages_openrouter(pmsgs);
    let mut body = json!({
        "model": model,
        "max_tokens": super::output_limits::max_tokens_for_model(model),
        "stream": true,
        "stream_options": {"include_usage": true},
        "messages": api_messages,
    });
    if !tools.is_empty() {
        body["tools"] = json!(tools.iter().map(translate_tool_spec).collect::<Vec<_>>());
    }
    body
}

// ============================================================================
// Response side: SSE → ProviderEvent
// ============================================================================

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct OpenAiChunk {
    #[serde(default)]
    choices: Vec<OpenAiChoice>,
    /// Usage may arrive in a separate chunk after choices stop
    /// streaming. With `stream_options.include_usage = true`, the
    /// backend emits one final chunk with `choices: []` and `usage`
    /// populated. We capture it whenever present (defensive — some
    /// providers attach it to the last content chunk instead).
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    prompt_tokens_details: Option<OpenAiPromptTokensDetails>,
    #[serde(default)]
    completion_tokens_details: Option<OpenAiCompletionTokensDetails>,
}

#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
struct OpenAiPromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
struct OpenAiCompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

impl OpenAiUsage {
    fn to_usage(&self) -> Usage {
        Usage {
            input_tokens: self.prompt_tokens.unwrap_or(0),
            output_tokens: self.completion_tokens.unwrap_or(0),
            cache_read_input_tokens: self
                .prompt_tokens_details
                .as_ref()
                .and_then(|d| d.cached_tokens),
            cache_creation_input_tokens: None,
            reasoning_tokens: self
                .completion_tokens_details
                .as_ref()
                .and_then(|d| d.reasoning_tokens),
        }
    }
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct OpenAiChoice {
    #[serde(default)]
    delta: OpenAiDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
struct OpenAiDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAiToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct OpenAiToolCallDelta {
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "function")]
    function: Option<OpenAiFunctionDelta>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct OpenAiFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

fn map_finish_reason(s: Option<&str>) -> StopReason {
    match s {
        Some("stop") => StopReason::EndTurn,
        Some("tool_calls") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        Some(other) => {
            tracing::warn!(finish_reason = %other, "unrecognized openai finish_reason");
            StopReason::EndTurn
        }
        None => StopReason::EndTurn,
    }
}

#[derive(Default)]
struct ToolCallBuilder {
    id: String,
    name: String,
    args_json: String,
}

struct StreamState {
    sse: Pin<Box<dyn Stream<Item = SseEvent> + Send>>,
    accumulated_text: String,
    tool_calls: HashMap<u32, ToolCallBuilder>,
    tool_call_order: Vec<u32>,
    finish_reason: Option<String>,
    /// Most-recently-seen usage from any chunk. With `include_usage`,
    /// the final chunk carries the authoritative number.
    usage: Option<Usage>,
    cancel_rx: Option<oneshot::Receiver<()>>,
    finished: bool,
    emitted_done: bool,
}

fn events_stream(
    bytes: impl Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
    cancel_rx: oneshot::Receiver<()>,
) -> BoxStream<'static, ProviderEvent> {
    let bytes: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>> = Box::pin(bytes);
    let sse = SseStream::new(bytes);
    let state = StreamState {
        sse: Box::pin(sse),
        accumulated_text: String::new(),
        tool_calls: HashMap::new(),
        tool_call_order: Vec::new(),
        finish_reason: None,
        usage: None,
        cancel_rx: Some(cancel_rx),
        finished: false,
        emitted_done: false,
    };
    Box::pin(futures::stream::unfold(state, next_event))
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
                    return Some((
                        ProviderEvent::Done(AssistantMessage {
                            content: assemble_content(&state),
                            stop_reason: StopReason::Aborted,
                            usage: state.usage,
                        }),
                        state,
                    ));
                }
                Err(oneshot::error::TryRecvError::Empty) => {}
                Err(oneshot::error::TryRecvError::Closed) => {
                    state.cancel_rx = None;
                }
            }
        }

        // Pull next SSE event.
        let sse_event = match state.sse.next().await {
            Some(e) => e,
            None => {
                state.finished = true;
                if !state.emitted_done {
                    state.emitted_done = true;
                    let stop = map_finish_reason(state.finish_reason.as_deref());
                    return Some((
                        ProviderEvent::Done(AssistantMessage {
                            content: assemble_content(&state),
                            stop_reason: stop,
                            usage: state.usage,
                        }),
                        state,
                    ));
                }
                return None;
            }
        };

        // OpenAI's stream terminates with `data: [DONE]\n\n`.
        if sse_event.data.trim() == "[DONE]" {
            state.finished = true;
            state.emitted_done = true;
            let stop = map_finish_reason(state.finish_reason.as_deref());
            return Some((
                ProviderEvent::Done(AssistantMessage {
                    content: assemble_content(&state),
                    stop_reason: stop,
                    usage: state.usage,
                }),
                state,
            ));
        }

        let chunk: OpenAiChunk = match serde_json::from_str(&sse_event.data) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, data = %sse_event.data, "failed to parse openrouter chunk");
                continue;
            }
        };

        // Capture usage whenever a chunk includes it. With
        // `include_usage`, the final chunk has empty choices and just
        // populated usage; without it, some backends embed usage on
        // the last content chunk. Either way, latest non-None wins.
        if let Some(u) = chunk.usage.as_ref() {
            state.usage = Some(u.to_usage());
        }

        // Process every choice (typically just one, choices[0]).
        let mut emitted_event: Option<ProviderEvent> = None;
        for choice in chunk.choices {
            // Text delta?
            if let Some(content) = choice.delta.content {
                if !content.is_empty() {
                    state.accumulated_text.push_str(&content);
                    if emitted_event.is_none() {
                        emitted_event = Some(ProviderEvent::TextDelta(content));
                    }
                }
            }
            // Tool call delta(s)?
            if let Some(deltas) = choice.delta.tool_calls {
                for tc_delta in deltas {
                    let entry = state.tool_calls.entry(tc_delta.index).or_insert_with(|| {
                        // First time seeing this index — track its order.
                        state.tool_call_order.push(tc_delta.index);
                        ToolCallBuilder::default()
                    });
                    if let Some(id) = tc_delta.id {
                        entry.id = id;
                    }
                    if let Some(func) = tc_delta.function {
                        if let Some(name) = func.name {
                            entry.name = name;
                        }
                        if let Some(args) = func.arguments {
                            entry.args_json.push_str(&args);
                        }
                    }
                    // v1: don't emit ProviderEvent::ToolCallDelta;
                    // the loop ignores it. Final tool calls are
                    // surfaced in Done.
                }
            }
            // finish_reason landed?
            if let Some(reason) = choice.finish_reason {
                state.finish_reason = Some(reason);
            }
        }

        if let Some(event) = emitted_event {
            return Some((event, state));
        }
        // No emittable event for this chunk (e.g. it was a delta
        // with only tool_calls or a finish_reason). Loop and pull
        // the next SSE event.
    }
}

fn assemble_content(state: &StreamState) -> Vec<ContentBlock> {
    let mut out: Vec<ContentBlock> = Vec::new();
    if !state.accumulated_text.is_empty() {
        out.push(ContentBlock::Text {
            text: state.accumulated_text.as_str().into(),
        });
    }
    for idx in &state.tool_call_order {
        if let Some(builder) = state.tool_calls.get(idx) {
            let arguments = parse_tool_input(&builder.args_json);
            out.push(ContentBlock::ToolCall(ToolCall {
                id: builder.id.clone(),
                name: builder.name.clone(),
                arguments,
            }));
        }
    }
    out
}

fn parse_tool_input(input_json: &str) -> Value {
    if input_json.is_empty() {
        return Value::Object(serde_json::Map::new());
    }
    match serde_json::from_str::<Value>(input_json) {
        Ok(v) if v.is_object() => v,
        Ok(other) => {
            tracing::warn!(value = %other, "tool input JSON wasn't an object; using empty object");
            Value::Object(serde_json::Map::new())
        }
        Err(e) => {
            tracing::warn!(error = %e, raw = %input_json, "failed to parse tool input JSON; using empty object");
            Value::Object(serde_json::Map::new())
        }
    }
}

#[cfg(test)]
#[path = "openrouter_tests.rs"]
mod tests;
