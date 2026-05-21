//! OpenAI Codex provider — direct HTTP to
//! `https://chatgpt.com/backend-api/codex/responses` using OAuth
//! tokens stored by mu-018. Replaces the pi subprocess shell of
//! mu-015. See spec mu-019.
//!
//! Flow:
//!   1. Load token from `FileSystemTokenStore` (or use one supplied
//!      via `from_parts`).
//!   2. Pull `chatgpt_account_id` out of the access_token's JWT
//!      payload for the required `chatgpt-account-id` header.
//!   3. Send the request to the Codex backend; stream SSE.
//!   4. On 401, refresh exactly once and retry. Persist the rotated
//!      bundle unless ephemeral.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use bytes::Bytes;
use futures::stream::{BoxStream, Stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{oneshot, Mutex};

use mu_core::agent::{
    AgentMessage, AssistantMessage, ContentBlock, MessageInput, Provider, ProviderError,
    ProviderEvent, StopReason, ToolCall, ToolSpec, Usage,
};
use mu_core::context::{
    extract_call_id_from_span_id, ProviderMessage, ProviderMessages, ProviderRole,
};

use crate::auth::{self, FileSystemTokenStore, OAuthToken, TokenStore};

use super::sse::{SseEvent, SseStream};

// ============================================================================
// Constants
// ============================================================================

const DEFAULT_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
const ORIGINATOR: &str = "mu";
const DEFAULT_THINKING: &str = "medium";
const PROVIDER_KEY: &str = "openai-codex";

/// Default `instructions` (the Responses API's required system-prompt
/// field). Kept short and provider-agnostic — actual agent-loop
/// behavior (tool use, output style) is driven by mu's own loop, not
/// by this string. Callers can override via `with_instructions`.
const DEFAULT_INSTRUCTIONS: &str = "You are mu, a coding agent. Respond concisely. \
     When tools are provided, prefer to use them rather than asking \
     the user for information you could obtain yourself.";

// ============================================================================
// Provider struct
// ============================================================================

pub struct OpenaiCodexProvider {
    model: String,
    thinking: String,
    instructions: String,
    /// Wrapped in `Mutex` so a 401-refresh can swap it atomically.
    /// Reads clone the bundle and release the lock immediately so
    /// the lock is never held across an `await`.
    token: Mutex<OAuthToken>,
    /// `None` = ephemeral (don't persist refreshed tokens).
    store: Option<Arc<dyn TokenStore>>,
    http: reqwest::Client,
    /// Test seam — defaults to the codex endpoint.
    endpoint: String,
}

impl OpenaiCodexProvider {
    /// Load the bundle from `~/.config/mu/auth/openai-codex.json`.
    /// Fails if not logged in.
    pub fn from_store(model: String) -> Result<Self, ProviderError> {
        let store = FileSystemTokenStore::default_location().map_err(map_auth_err)?;
        let token = load_token(&store)?;
        Ok(Self {
            model,
            thinking: DEFAULT_THINKING.into(),
            instructions: DEFAULT_INSTRUCTIONS.into(),
            token: Mutex::new(token),
            store: Some(Arc::new(store)),
            http: reqwest::Client::new(),
            endpoint: DEFAULT_ENDPOINT.into(),
        })
    }

    /// Like `from_store` but won't persist refreshed tokens back
    /// to disk. The in-memory bundle still rotates on 401-refresh.
    pub fn from_store_ephemeral(model: String) -> Result<Self, ProviderError> {
        let store = FileSystemTokenStore::default_location().map_err(map_auth_err)?;
        let token = load_token(&store)?;
        Ok(Self {
            model,
            thinking: DEFAULT_THINKING.into(),
            instructions: DEFAULT_INSTRUCTIONS.into(),
            token: Mutex::new(token),
            store: None,
            http: reqwest::Client::new(),
            endpoint: DEFAULT_ENDPOINT.into(),
        })
    }

    /// Caller-supplied token and (optional) store. For tests and
    /// embedders.
    pub fn from_parts(
        model: String,
        token: OAuthToken,
        store: Option<Arc<dyn TokenStore>>,
    ) -> Self {
        Self {
            model,
            thinking: DEFAULT_THINKING.into(),
            instructions: DEFAULT_INSTRUCTIONS.into(),
            token: Mutex::new(token),
            store,
            http: reqwest::Client::new(),
            endpoint: DEFAULT_ENDPOINT.into(),
        }
    }

    pub fn with_thinking(mut self, thinking: String) -> Self {
        self.thinking = thinking;
        self
    }

    /// Override the Responses API `instructions` field — required
    /// by the endpoint, conceptually the system prompt.
    pub fn with_instructions(mut self, instructions: String) -> Self {
        self.instructions = instructions;
        self
    }

    /// Test seam: override the endpoint URL (for wiremock-style tests).
    pub fn with_endpoint(mut self, endpoint: String) -> Self {
        self.endpoint = endpoint;
        self
    }
}

fn map_auth_err(e: auth::AuthError) -> ProviderError {
    ProviderError::Other(format!("auth: {e}"))
}

fn load_token<S: TokenStore + ?Sized>(store: &S) -> Result<OAuthToken, ProviderError> {
    let opt = store.load(PROVIDER_KEY).map_err(map_auth_err)?;
    opt.ok_or_else(|| {
        ProviderError::Other(
            "not logged in to openai-codex — run \
             `mu login --provider openai-codex`"
                .into(),
        )
    })
}

// ============================================================================
// JWT claim extraction
//
// The access_token from mu-018 is a JWT. We don't verify the
// signature — it's *our own* token, freshly minted by OpenAI and
// stored locally; the trust root is the file's 0600 perms, not the
// signature. We just decode the middle segment to pull the
// `chatgpt_account_id` claim required for the request header.
// ============================================================================

pub(crate) fn extract_chatgpt_account_id(access_token: &str) -> Result<String, ProviderError> {
    let segments: Vec<&str> = access_token.split('.').collect();
    if segments.len() != 3 {
        return Err(ProviderError::Other(format!(
            "access_token is not a JWT (expected 3 segments, got {})",
            segments.len()
        )));
    }
    // JWT uses base64url *without* padding; the URL_SAFE_NO_PAD
    // engine matches that exactly.
    let payload_b64 = segments[1].trim_end_matches('=');
    let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64.as_bytes())
        .map_err(|e| ProviderError::Other(format!("decode JWT payload: {e}")))?;
    let payload: Value = serde_json::from_slice(&payload_bytes)
        .map_err(|e| ProviderError::Other(format!("parse JWT payload: {e}")))?;
    let claim_obj = payload.get("https://api.openai.com/auth").ok_or_else(|| {
        ProviderError::Other(
            "JWT missing `https://api.openai.com/auth` claim — \
             this is not an OpenAI Codex token"
                .into(),
        )
    })?;
    let account_id = claim_obj
        .get("chatgpt_account_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ProviderError::Other("JWT auth claim missing `chatgpt_account_id`".into())
        })?;
    Ok(account_id.to_string())
}

// ============================================================================
// Request body — Responses API shape
//
// Distinct from Chat Completions: the input is an array of typed
// items (message, function_call, function_call_output) rather than
// role-tagged messages. Tool definitions are flatter — no nested
// "function" wrapper. Streaming is via SSE typed events, not JSON
// chunks of choices[].
// ============================================================================

pub(crate) fn translate_tool_spec(spec: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "name": spec.name,
        "description": spec.description,
        "parameters": spec.input_schema,
    })
}

pub(crate) fn translate_message(m: &AgentMessage) -> Vec<Value> {
    match m {
        AgentMessage::User { content } => vec![json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": content}],
        })],
        AgentMessage::Assistant(a) => {
            // The Responses API treats text and function_calls as
            // separate top-level items (NOT nested under one message).
            let mut out: Vec<Value> = Vec::new();
            let mut text_parts: Vec<String> = Vec::new();
            for block in &a.content {
                match block {
                    ContentBlock::Text { text } => text_parts.push(text.to_string()),
                    ContentBlock::ToolCall(tc) => {
                        let args_str = serde_json::to_string(&tc.arguments)
                            .unwrap_or_else(|_| "{}".to_string());
                        out.push(json!({
                            "type": "function_call",
                            "call_id": tc.id,
                            "name": tc.name,
                            "arguments": args_str,
                        }));
                    }
                    ContentBlock::Thinking { .. } => {
                        // Don't re-send reasoning to the model — it
                        // can't consume its own prior reasoning text
                        // as input on this API.
                    }
                }
            }
            if !text_parts.is_empty() {
                let combined = text_parts.join("");
                // Prepend the assistant text message so it sits
                // before function_calls in turn order.
                out.insert(
                    0,
                    json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": combined}],
                    }),
                );
            }
            out
        }
        AgentMessage::ToolResult {
            call_id,
            content,
            is_error,
        } => {
            let body = if *is_error {
                format!("[error] {content}")
            } else {
                content.clone()
            };
            vec![json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": body,
            })]
        }
    }
}

pub(crate) fn build_request_body(
    model: &str,
    thinking: &str,
    instructions: &str,
    messages: &[AgentMessage],
    tools: &[ToolSpec],
) -> Value {
    let input: Vec<Value> = messages.iter().flat_map(translate_message).collect();
    let mut body = json!({
        "model": model,
        "instructions": instructions,
        "input": input,
        "stream": true,
        "store": false,
        "reasoning": {"effort": thinking, "summary": "auto"},
    });
    if !tools.is_empty() {
        body["tools"] = json!(tools.iter().map(translate_tool_spec).collect::<Vec<_>>());
        body["tool_choice"] = json!("auto");
        body["parallel_tool_calls"] = json!(false);
    }
    body
}

// ============================================================================
// mu-yqeq.5: Projected path — wire body built from &ProviderMessages
// ============================================================================
//
// Mirrors `translate_message` + `build_request_body` semantics but
// reads structural `ContentBlock`s from `ProviderMessage.blocks`
// instead of `AgentMessage::Assistant.content`. The session
// system-prompt span (`source_span_ids[0] == "system-prompt"`) is
// hoisted into the top-level `instructions` field with the same
// Legacy fallback rule: empty/missing hoisted text falls back to the
// provider's default `instructions`. Tool-schema spans are silently
// dropped (the `tools` parameter is authoritative for `body.tools`).
//
// Wire-format byte equivalence with the Legacy path is the contract;
// see `yqeq5_parity_*` tests in openai_codex_tests.rs for the
// canonical scenarios.

/// Translate a [`ProviderMessages`] projection into the OpenAI
/// Responses API `input` array, returning the hoisted system-prompt
/// text (if any) separately for [`build_request_body_from_projection`]
/// to place in the top-level `instructions` field.
///
/// Hoisting rule: the FIRST `ProviderRole::System` message whose
/// `source_span_ids[0] == "system-prompt"` has its content captured.
/// Tool-schema spans (`source_span_ids[0]` starts with `"tool-schema:"`)
/// are silently skipped — the `tools` parameter on `Provider::stream`
/// is authoritative for `body.tools`.
fn translate_provider_messages_codex(pmsgs: &ProviderMessages) -> (Vec<Value>, Option<String>) {
    let mut out: Vec<Value> = Vec::with_capacity(pmsgs.messages.len());
    let mut system_text: Option<String> = None;

    for msg in &pmsgs.messages {
        match msg.role() {
            ProviderRole::System => {
                let is_session_prompt = msg
                    .source_span_ids()
                    .first()
                    .map(|sid| sid.as_ref() == "system-prompt")
                    .unwrap_or(false);
                if is_session_prompt && system_text.is_none() {
                    let content = msg.content();
                    if !content.is_empty() {
                        system_text = Some(content.to_string());
                    }
                }
            }
            ProviderRole::User => {
                out.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": msg.content()}],
                }));
            }
            ProviderRole::Assistant => {
                out.extend(translate_provider_assistant_codex(msg));
            }
            ProviderRole::ToolResult => {
                out.push(translate_provider_tool_result_codex(msg));
            }
        }
    }

    (out, system_text)
}

/// Translate one assistant-role [`ProviderMessage`] into the
/// Responses API's split shape: a `message` item carrying any text
/// (combined like the Legacy path), followed by one `function_call`
/// item per [`ContentBlock::ToolCall`]. `Thinking` blocks are
/// skipped per spec mu-044 §"Thinking-block skip" — the model never
/// receives its own reasoning trace as input. Returns an empty `Vec`
/// when the assistant has no wire-bearing blocks (mirrors the Legacy
/// `translate_message` behavior).
fn translate_provider_assistant_codex(msg: &ProviderMessage) -> Vec<Value> {
    let blocks = match msg.blocks() {
        Some(b) => b,
        None => return Vec::new(),
    };
    let mut out: Vec<Value> = Vec::new();
    let mut text_parts: Vec<String> = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => text_parts.push(text.to_string()),
            ContentBlock::ToolCall(tc) => {
                let args_str =
                    serde_json::to_string(&tc.arguments).unwrap_or_else(|_| "{}".to_string());
                out.push(json!({
                    "type": "function_call",
                    "call_id": tc.id,
                    "name": tc.name,
                    "arguments": args_str,
                }));
            }
            ContentBlock::Thinking { .. } => {}
        }
    }
    if !text_parts.is_empty() {
        let combined = text_parts.join("");
        out.insert(
            0,
            json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": combined}],
            }),
        );
    }
    out
}

/// Translate one tool-result [`ProviderMessage`] into a
/// `function_call_output` item. `call_id` is recovered from the
/// synthesized span id (`msg-{idx}-tool-result:{call_id}`); `is_error`
/// is recovered from the `"error: "` prefix that
/// `assembly.rs::message_to_span` adds when the original
/// `AgentMessage::ToolResult.is_error` was `true`. Errors are
/// re-encoded as `"[error] {content}"` to match the Legacy
/// `translate_message` shape.
fn translate_provider_tool_result_codex(msg: &ProviderMessage) -> Value {
    let call_id: &str = msg
        .source_span_ids()
        .first()
        .and_then(|sid| extract_call_id_from_span_id(sid.as_ref()))
        .unwrap_or("");
    let body: String = match msg.content().strip_prefix("error: ") {
        Some(stripped) => format!("[error] {stripped}"),
        None => msg.content().to_string(),
    };
    json!({
        "type": "function_call_output",
        "call_id": call_id,
        "output": body,
    })
}

/// Sibling of [`build_request_body`] that builds the Responses API
/// request body from a [`ProviderMessages`] projection instead of a
/// raw `&[AgentMessage]` slice. Wire JSON is byte-identical to the
/// Legacy path for the canonical scenarios (asserted by
/// `yqeq5_parity_*` tests).
///
/// `default_instructions` is the provider's static fallback (used
/// when the projection has no hoisted system span or the span's
/// content is empty) — matches Legacy's
/// `system_prompt.filter(|s| !s.is_empty()).unwrap_or(&self.instructions)`.
pub(crate) fn build_request_body_from_projection(
    model: &str,
    thinking: &str,
    default_instructions: &str,
    pmsgs: &ProviderMessages,
    tools: &[ToolSpec],
) -> Value {
    let (input, hoisted_system) = translate_provider_messages_codex(pmsgs);
    let instructions: &str = match hoisted_system.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => default_instructions,
    };
    let mut body = json!({
        "model": model,
        "instructions": instructions,
        "input": input,
        "stream": true,
        "store": false,
        "reasoning": {"effort": thinking, "summary": "auto"},
    });
    if !tools.is_empty() {
        body["tools"] = json!(tools.iter().map(translate_tool_spec).collect::<Vec<_>>());
        body["tool_choice"] = json!("auto");
        body["parallel_tool_calls"] = json!(false);
    }
    body
}

// ============================================================================
// SSE → ProviderEvent
//
// Responses API event vocabulary (the ones we care about):
//   - response.output_text.delta        → TextDelta
//   - response.output_item.added        → start tool-call accumulator
//                                         (only when item.type=="function_call")
//   - response.function_call.arguments.delta → append to tool args,
//                                              emit ToolCallDelta
//   - response.output_item.done         → finalize tool call (or message)
//   - response.reasoning_summary.delta  → ThinkingDelta
//   - response.completed                → Done(...)
//   - response.failed / error           → Error
// Other events (response.created, response.output_text.done, etc.)
// are noise — ignored.
// ============================================================================

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
enum SseFrame {
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta { delta: String },
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded { output_index: u32, item: OutputItem },
    #[serde(rename = "response.function_call.arguments.delta")]
    FunctionCallArgumentsDelta {
        output_index: u32,
        #[serde(default, rename = "item_id")]
        _item_id: Option<String>,
        delta: String,
    },
    #[serde(rename = "response.output_item.done")]
    OutputItemDone { output_index: u32, item: OutputItem },
    #[serde(rename = "response.reasoning_summary.delta")]
    ReasoningSummaryDelta { delta: String },
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryTextDelta { delta: String },
    #[serde(rename = "response.completed")]
    Completed {
        #[serde(default)]
        response: Option<CompletedResponse>,
    },
    #[serde(rename = "response.failed")]
    Failed {
        #[serde(default)]
        response: Option<FailedResponse>,
    },
    #[serde(rename = "error")]
    Error {
        #[serde(default)]
        message: Option<String>,
    },
    /// Catch-all for events we don't model.
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
enum OutputItem {
    #[serde(rename = "function_call")]
    FunctionCall {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        call_id: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        arguments: Option<String>,
    },
    #[serde(rename = "message")]
    Message {
        #[serde(default)]
        id: Option<String>,
    },
    #[serde(rename = "reasoning")]
    Reasoning {
        #[serde(default)]
        id: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
struct CompletedResponse {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    incomplete_details: Option<IncompleteDetails>,
    #[serde(default)]
    usage: Option<ResponsesApiUsage>,
}

#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
struct ResponsesApiUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    input_tokens_details: Option<ResponsesApiInputDetails>,
    #[serde(default)]
    output_tokens_details: Option<ResponsesApiOutputDetails>,
}

#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
struct ResponsesApiInputDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
struct ResponsesApiOutputDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

impl ResponsesApiUsage {
    fn to_usage(&self) -> Usage {
        Usage {
            input_tokens: self.input_tokens.unwrap_or(0),
            output_tokens: self.output_tokens.unwrap_or(0),
            cache_read_input_tokens: self
                .input_tokens_details
                .as_ref()
                .and_then(|d| d.cached_tokens),
            cache_creation_input_tokens: None,
            reasoning_tokens: self
                .output_tokens_details
                .as_ref()
                .and_then(|d| d.reasoning_tokens),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
struct FailedResponse {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
struct IncompleteDetails {
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Default)]
struct ToolCallBuilder {
    /// item id (e.g. `fc_...`) — internal
    _item_id: String,
    /// call id (e.g. `call_...`) — matches function_call_output later
    call_id: String,
    name: String,
    args_json: String,
}

struct StreamState {
    sse: Pin<Box<dyn Stream<Item = SseEvent> + Send>>,
    accumulated_text: String,
    tool_calls: HashMap<u32, ToolCallBuilder>,
    tool_call_order: Vec<u32>,
    final_status: Option<String>,
    incomplete_reason: Option<String>,
    /// Usage from the `response.completed` event's `response.usage`.
    /// Codex reliably emits this once per stream.
    usage: Option<Usage>,
    cancel_rx: Option<oneshot::Receiver<()>>,
    finished: bool,
    emitted_done: bool,
    /// Errors that arrived via response.failed / error events.
    error_message: Option<String>,
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
        final_status: None,
        incomplete_reason: None,
        usage: None,
        cancel_rx: Some(cancel_rx),
        finished: false,
        emitted_done: false,
        error_message: None,
    };
    Box::pin(futures::stream::unfold(state, next_event))
}

fn map_stop(state: &StreamState) -> StopReason {
    if state.error_message.is_some() {
        return StopReason::Error;
    }
    if let Some(reason) = state.incomplete_reason.as_deref() {
        if reason == "max_output_tokens" {
            return StopReason::MaxTokens;
        }
    }
    if !state.tool_calls.is_empty() {
        StopReason::ToolUse
    } else {
        match state.final_status.as_deref() {
            Some("completed") => StopReason::EndTurn,
            Some("incomplete") => StopReason::MaxTokens,
            Some("failed") => StopReason::Error,
            _ => StopReason::EndTurn,
        }
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
                id: builder.call_id.clone(),
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

async fn next_event(mut state: StreamState) -> Option<(ProviderEvent, StreamState)> {
    if state.finished {
        return None;
    }
    loop {
        // Cancel check.
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

        let sse_event = match state.sse.next().await {
            Some(e) => e,
            None => {
                // End of stream.
                state.finished = true;
                if !state.emitted_done {
                    state.emitted_done = true;
                    if let Some(msg) = state.error_message.take() {
                        return Some((ProviderEvent::Error(msg), state));
                    }
                    let stop = map_stop(&state);
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

        // Skip empty data lines (keep-alive style).
        if sse_event.data.trim().is_empty() {
            continue;
        }
        // Codex sometimes emits explicit `[DONE]`; treat like EOF.
        if sse_event.data.trim() == "[DONE]" {
            state.finished = true;
            if !state.emitted_done {
                state.emitted_done = true;
                let stop = map_stop(&state);
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

        let frame: SseFrame = match serde_json::from_str(&sse_event.data) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, data = %sse_event.data, "failed to parse codex SSE frame");
                continue;
            }
        };

        match frame {
            SseFrame::OutputTextDelta { delta } => {
                if !delta.is_empty() {
                    state.accumulated_text.push_str(&delta);
                    return Some((ProviderEvent::TextDelta(delta), state));
                }
            }
            SseFrame::OutputItemAdded { output_index, item } => {
                if let OutputItem::FunctionCall {
                    id,
                    call_id,
                    name,
                    arguments,
                } = item
                {
                    let entry = state.tool_calls.entry(output_index).or_insert_with(|| {
                        state.tool_call_order.push(output_index);
                        ToolCallBuilder::default()
                    });
                    if let Some(v) = id {
                        entry._item_id = v;
                    }
                    if let Some(v) = call_id {
                        entry.call_id = v;
                    }
                    if let Some(v) = name {
                        entry.name = v;
                    }
                    if let Some(v) = arguments {
                        if !v.is_empty() {
                            entry.args_json.push_str(&v);
                        }
                    }
                }
                // Non-function_call items get an entry on `done`,
                // not here.
            }
            SseFrame::FunctionCallArgumentsDelta {
                output_index,
                _item_id: _,
                delta,
            } => {
                let entry = state.tool_calls.entry(output_index).or_insert_with(|| {
                    state.tool_call_order.push(output_index);
                    ToolCallBuilder::default()
                });
                entry.args_json.push_str(&delta);
                // Surface the delta — `id` is the call_id (matches
                // the tool_call_output we'll send back). Empty
                // call_id is possible if `added` hasn't arrived yet
                // (shouldn't happen with the documented event order
                // but we don't depend on it).
                let id = entry.call_id.clone();
                return Some((
                    ProviderEvent::ToolCallDelta {
                        id,
                        name_delta: None,
                        arguments_delta: Some(delta),
                    },
                    state,
                ));
            }
            SseFrame::OutputItemDone { output_index, item } => {
                if let OutputItem::FunctionCall {
                    id,
                    call_id,
                    name,
                    arguments,
                } = item
                {
                    // Fill any fields that the streamed deltas
                    // didn't already cover. Final `arguments`
                    // overrides accumulation if present (it should
                    // be the full JSON).
                    let entry = state.tool_calls.entry(output_index).or_insert_with(|| {
                        state.tool_call_order.push(output_index);
                        ToolCallBuilder::default()
                    });
                    if let Some(v) = id {
                        entry._item_id = v;
                    }
                    if let Some(v) = call_id {
                        entry.call_id = v;
                    }
                    if let Some(v) = name {
                        entry.name = v;
                    }
                    if let Some(v) = arguments {
                        if !v.is_empty() {
                            entry.args_json = v;
                        }
                    }
                }
            }
            SseFrame::ReasoningSummaryDelta { delta }
            | SseFrame::ReasoningSummaryTextDelta { delta } => {
                if !delta.is_empty() {
                    return Some((ProviderEvent::ThinkingDelta(delta), state));
                }
            }
            SseFrame::Completed { response } => {
                if let Some(r) = response {
                    state.final_status = r.status;
                    state.incomplete_reason = r.incomplete_details.and_then(|d| d.reason);
                    if let Some(u) = r.usage.as_ref() {
                        state.usage = Some(u.to_usage());
                    }
                } else {
                    state.final_status = Some("completed".into());
                }
                state.finished = true;
                state.emitted_done = true;
                let stop = map_stop(&state);
                return Some((
                    ProviderEvent::Done(AssistantMessage {
                        content: assemble_content(&state),
                        stop_reason: stop,
                        usage: state.usage,
                    }),
                    state,
                ));
            }
            SseFrame::Failed { response } => {
                let err_msg = response
                    .and_then(|r| r.error.map(|e| e.to_string()))
                    .unwrap_or_else(|| "codex response failed".into());
                state.error_message = Some(err_msg.clone());
                state.finished = true;
                state.emitted_done = true;
                return Some((ProviderEvent::Error(err_msg), state));
            }
            SseFrame::Error { message } => {
                let msg = message.unwrap_or_else(|| "codex stream error".into());
                state.error_message = Some(msg.clone());
                state.finished = true;
                state.emitted_done = true;
                return Some((ProviderEvent::Error(msg), state));
            }
            SseFrame::Other => {
                // Unmodeled event — ignore.
            }
        }
        // Loop and pull the next frame.
    }
}

// ============================================================================
// Provider impl
// ============================================================================

#[async_trait]
impl Provider for OpenaiCodexProvider {
    async fn stream(
        &self,
        system_prompt: Option<&str>,
        input: MessageInput<'_>,
        tools: &[ToolSpec],
        cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        // mu-yqeq.5: sealed-enum dispatch (Legacy + Projected). The
        // `_` arm remains for forward-compat with future MessageInput
        // variants — adding one will compile-warn here for review.
        //
        // Projected arm produces byte-identical wire JSON to the
        // Legacy arm; the `yqeq5_parity_*` tests in
        // openai_codex_tests.rs assert that invariant for the
        // canonical scenarios. The agent loop's mod.rs:818 still
        // passes Legacy until mu-yqeq.8 wires the cutover.
        let body = match input {
            MessageInput::Legacy(msgs) => {
                // mu-n48: a session-level system_prompt overrides
                // the provider's default `instructions`.
                let instructions: &str = system_prompt
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&self.instructions);
                build_request_body(&self.model, &self.thinking, instructions, msgs, tools)
            }
            MessageInput::Projected(pmsgs) => {
                // In the Projected path the projection itself carries
                // the session system prompt (assemble_rope put it
                // there from `system_prompt`); the helper hoists it
                // into `body.instructions` and falls back to
                // `self.instructions` when the projection has no
                // (or an empty) system span — matching Legacy's
                // `.filter(|s| !s.is_empty()).unwrap_or(...)` rule.
                build_request_body_from_projection(
                    &self.model,
                    &self.thinking,
                    &self.instructions,
                    pmsgs,
                    tools,
                )
            }
            _ => {
                return Err(ProviderError::Other(
                    "OpenaiCodexProvider: unrecognized MessageInput variant".to_string(),
                ));
            }
        };

        // First attempt with the current token.
        let initial_token = self.token.lock().await.clone();
        let resp = self
            .send_request(&initial_token, &body)
            .await
            .map_err(|e| ProviderError::Other(format!("codex request: {e}")))?;

        // Refresh-and-retry on 401.
        let resp = if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            let refreshed = self.refresh_token_if_unchanged(&initial_token).await?;
            self.send_request(&refreshed, &body)
                .await
                .map_err(|e| ProviderError::Other(format!("codex retry: {e}")))?
        } else {
            resp
        };

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ProviderError::Other(
                "codex credentials rejected even after refresh — \
                 run `mu login --provider openai-codex` again"
                    .into(),
            ));
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Other(format!(
                "codex returned {status}: {text}"
            )));
        }

        let bytes = resp.bytes_stream();
        Ok(events_stream(bytes, cancel_rx))
    }
}

impl OpenaiCodexProvider {
    async fn send_request(
        &self,
        token: &OAuthToken,
        body: &Value,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let account_id = match extract_chatgpt_account_id(&token.access_token) {
            Ok(id) => id,
            Err(e) => {
                // Surface as a 401-like response so the retry path can
                // refresh — but in practice, this is a token-shape
                // bug, not an expiry bug. Pass through as a request
                // build failure.
                tracing::error!(error = %e, "could not extract chatgpt_account_id");
                // Send the request anyway with an empty header;
                // backend will return 401 and the caller will
                // produce a meaningful error message after refresh
                // fails. (Alternative: return a fake error here.
                // Keeping the function infallible-on-build keeps
                // the call sites tidy.)
                String::new()
            }
        };

        self.http
            .post(&self.endpoint)
            .header("Authorization", format!("Bearer {}", token.access_token))
            .header("chatgpt-account-id", account_id)
            .header("originator", ORIGINATOR)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .json(body)
            .send()
            .await
    }

    /// If the in-memory token still matches `seen` (we're first to
    /// notice 401), refresh and persist. Otherwise, another caller
    /// already refreshed; just return the current bundle.
    async fn refresh_token_if_unchanged(
        &self,
        seen: &OAuthToken,
    ) -> Result<OAuthToken, ProviderError> {
        let current = self.token.lock().await;
        if current.access_token != seen.access_token {
            // Already rotated by someone else. Use the new one.
            return Ok(current.clone());
        }
        let refresh_token = current.refresh_token.clone().ok_or_else(|| {
            ProviderError::Other(
                "codex access_token expired and no refresh_token stored — \
                 run `mu login --provider openai-codex`"
                    .into(),
            )
        })?;
        // Release the lock across the await — refresh is HTTP, can
        // be slow. We re-check on the way back.
        drop(current);
        let new_bundle = auth::openai_codex::refresh_access_token(&refresh_token)
            .await
            .map_err(map_auth_err)?;
        let mut current = self.token.lock().await;
        // If someone else won the race, take theirs.
        if current.access_token != seen.access_token {
            return Ok(current.clone());
        }
        *current = new_bundle.clone();
        drop(current);
        if let Some(store) = &self.store {
            if let Err(e) = store.save(PROVIDER_KEY, &new_bundle) {
                tracing::warn!(error = %e, "failed to persist refreshed token; continuing in-memory");
            }
        }
        Ok(new_bundle)
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
#[path = "openai_codex_tests.rs"]
mod tests;
