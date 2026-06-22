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

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use bytes::Bytes;
use futures::stream::{BoxStream, Stream, StreamExt};
use serde::Deserialize;
#[cfg(test)]
use serde_json::json;
use serde_json::Value;
use tokio::sync::{oneshot, Mutex};
use tracing::debug;

#[cfg(test)]
use mu_core::agent::{AgentMessage, AssistantMessage, ContentBlock, StopReason, ToolCall, Usage};
use mu_core::agent::{MessageInput, Provider, ProviderError, ProviderEvent, ToolSpec};
#[cfg(test)]
use mu_core::context::{
    extract_call_id_from_span_id, ProviderMessage, ProviderMessages, ProviderRole,
};

use crate::auth::{self, FileSystemTokenStore, OAuthToken, TokenStore};

#[cfg(test)]
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

#[cfg(test)]
pub(crate) fn translate_tool_spec(spec: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "name": spec.name,
        "description": spec.description,
        "parameters": spec.input_schema,
    })
}

#[cfg(test)]
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

/// Soft cap for the codex Responses API `instructions` field.
///
/// Empirically, codex closes the SSE stream with zero events when
/// `instructions` is much larger than this (verified at ~108 KB, where
/// the daemon was concatenating CLAUDE.md / AGENTS.md / memory content
/// into the system prompt). 8 KB sits comfortably under any plausible
/// limit while accommodating a substantial DEFAULT_INSTRUCTIONS + a
/// modest session-supplied prefix.
///
/// Content that exceeds the cap is moved out of `instructions` and
/// prepended to `input` as a synthetic user message — codex handles
/// long input messages fine, it's specifically the `instructions`
/// field that chokes. Anthropic's `system` parameter has no such
/// constraint, which is why this only surfaced on codex.
pub(crate) const INSTRUCTIONS_SOFT_CAP: usize = 8 * 1024;

/// Apply [`INSTRUCTIONS_SOFT_CAP`] to a candidate instructions string.
/// Returns the value to put in the `instructions` field and an optional
/// overflow body to prepend to `input`.
///
/// Below the cap → `(actual, None)`, identical to the pre-cap behavior.
/// At or above → `(DEFAULT_INSTRUCTIONS, Some(actual))`, so the model
/// still receives a short "you are mu" instruction in the dedicated
/// field and the long context lands as a regular input message.
#[cfg(test)]
fn split_oversized_instructions(actual: &str) -> (&str, Option<&str>) {
    if actual.len() > INSTRUCTIONS_SOFT_CAP {
        (DEFAULT_INSTRUCTIONS, Some(actual))
    } else {
        (actual, None)
    }
}

/// Build a single synthetic `role: "user"` message that wraps the
/// overflow with a brief framing line. Goes at `input[0]`. We use
/// `role: "user"` rather than `developer` because the Responses API's
/// developer role is newer and not universally supported on all model
/// variants we might target via the same wire path.
#[cfg(test)]
fn make_instructions_overflow_message(content: &str) -> Value {
    let framed = format!(
        "[System context — too large for the instructions field. \
         Treat this as your standing instructions and project context, \
         not as a question to respond to directly.]\n\n{content}"
    );
    json!({
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": framed}],
    })
}

#[cfg(test)]
pub(crate) fn build_request_body(
    model: &str,
    thinking: &str,
    instructions: &str,
    messages: &[AgentMessage],
    tools: &[ToolSpec],
) -> Value {
    let (instructions_field, overflow) = split_oversized_instructions(instructions);
    let mut input: Vec<Value> = Vec::with_capacity(messages.len() + 1);
    if let Some(o) = overflow {
        input.push(make_instructions_overflow_message(o));
    }
    input.extend(messages.iter().flat_map(translate_message));
    let mut body = json!({
        "model": model,
        "instructions": instructions_field,
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
#[cfg(test)]
fn translate_provider_messages_codex(pmsgs: &ProviderMessages) -> (Vec<Value>, Option<String>) {
    let mut out: Vec<Value> = Vec::with_capacity(pmsgs.messages.len());
    let mut system_text: Option<String> = None;

    for msg in &pmsgs.messages {
        match msg.role() {
            ProviderRole::System => {
                // mu-2puu: hoist ALL System-role spans into the
                // Responses API's `instructions` field, EXCEPT
                // tool-schema spans (those are passed separately via
                // `body.tools`). This includes:
                //   - the "system-prompt" span (session system_prompt)
                //   - "memory-recall:*" spans (SubprocessRecallProvider)
                //   - "project-file:*" spans (ProjectFileRecallProvider)
                //   - any other future System-role span kind
                //     (SkillActivation, Compaction, etc.)
                // Multiple system-role spans concatenate with "\n\n"
                // because the Responses API has only one
                // `instructions` slot. Pre-fix this branch only
                // hoisted the `system-prompt` span verbatim, silently
                // dropping every other System-role span — invisible
                // in `yqeq5_parity_*` tests because pre-mu-phl ropes
                // had no other System-role spans. See bead mu-2puu.
                let is_tool_schema = msg
                    .source_span_ids()
                    .first()
                    .map(|sid| sid.as_ref().starts_with("tool-schema:"))
                    .unwrap_or(false);
                if !is_tool_schema {
                    let content = msg.content();
                    if !content.is_empty() {
                        match system_text.as_mut() {
                            Some(existing) => {
                                existing.push_str("\n\n");
                                existing.push_str(content);
                            }
                            None => {
                                system_text = Some(content.to_string());
                            }
                        }
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
#[cfg(test)]
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
#[cfg(test)]
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
#[cfg(test)]
#[cfg(test)]
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
    let (instructions_field, overflow) = split_oversized_instructions(instructions);
    // Reconstruct input with overflow prepended if needed. `input` was
    // built without it; we only allocate a new Vec on overflow.
    let input = if let Some(o) = overflow {
        let mut v = Vec::with_capacity(input.len() + 1);
        v.push(make_instructions_overflow_message(o));
        v.extend(input);
        v
    } else {
        input
    };
    let mut body = json!({
        "model": model,
        "instructions": instructions_field,
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

#[cfg(test)]
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
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

#[cfg(test)]
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
            cache_creation_5m_input_tokens: None,
            cache_creation_1h_input_tokens: None,
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
#[cfg(test)]
struct ToolCallBuilder {
    /// item id (e.g. `fc_...`) — internal
    _item_id: String,
    /// call id (e.g. `call_...`) — matches function_call_output later
    call_id: String,
    name: String,
    args_json: String,
}

#[cfg(test)]
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
    // New typed OpenAI protocol path. The old StreamState/next_event parser is
    // still kept below for fixture-level tests during the cutover, but live
    // Codex traffic now parses through mu-openai's Responses event enum and
    // this crate's mu-specific interpretive adapter.
    let sse = mu_openai::SseStream::new(Box::pin(bytes));
    let events = sse.filter_map(|ev| async move {
        match ev {
            Err(e) => Some(Err(e.to_string())),
            Ok(e) if e.data.trim().is_empty() || e.data.trim() == "[DONE]" => None,
            Ok(e) => Some(
                serde_json::from_str::<mu_openai::ResponseStreamEvent>(&e.data)
                    .map_err(|err| format!("parse openai SSE event: {err}; data={}", e.data)),
            ),
        }
    });
    super::openai_responses::events_from_openai_stream(Box::pin(events), cancel_rx)
}

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
fn parse_tool_input(input_json: &str) -> mu_core::agent::ToolArgs {
    use mu_core::agent::ToolArgs;

    let value = if input_json.is_empty() {
        Value::Object(serde_json::Map::new())
    } else {
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
    };
    ToolArgs::new(value).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "tool arguments contained non-finite number; using empty object");
        ToolArgs::new(Value::Object(serde_json::Map::new())).unwrap()
    })
}

#[cfg(test)]
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
                        debug!(
                            target: "mu_ai::providers::openai_codex",
                            "codex stream ended with error: {msg}"
                        );
                        return Some((ProviderEvent::Error(msg), state));
                    }
                    let stop = map_stop(&state);
                    debug!(
                        target: "mu_ai::providers::openai_codex",
                        "codex stream EOF: text_len={} tool_calls={} final_status={:?} \
                         incomplete_reason={:?} usage_present={} stop={:?}",
                        state.accumulated_text.len(),
                        state.tool_calls.len(),
                        state.final_status,
                        state.incomplete_reason,
                        state.usage.is_some(),
                        stop,
                    );
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
                debug!(
                    target: "mu_ai::providers::openai_codex",
                    "codex stream [DONE]: text_len={} tool_calls={} final_status={:?} \
                     usage_present={} stop={:?}",
                    state.accumulated_text.len(),
                    state.tool_calls.len(),
                    state.final_status,
                    state.usage.is_some(),
                    stop,
                );
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
                match item {
                    OutputItem::FunctionCall {
                        id,
                        call_id,
                        name,
                        arguments,
                    } => {
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
                    // mu-s545: one response can carry MULTIPLE message
                    // output items (observed in the wild: the model
                    // answering a backlog of user messages left
                    // unanswered by errored asks — daemon
                    // 2f270bcba43f305d, event 2515). All output_text
                    // deltas funnel into the single accumulator, so
                    // without a boundary the items fuse without
                    // whitespace ("...or hold.No worries..."). Insert
                    // a paragraph break between message items — in the
                    // accumulator AND on the wire, so the streamed
                    // preview matches the finalized text (mu-wk2
                    // invariant). Guarded: a first/only message item
                    // (empty accumulator — incl. message-after-
                    // toolcall) gets no separator.
                    OutputItem::Message { .. } if !state.accumulated_text.is_empty() => {
                        state.accumulated_text.push_str("\n\n");
                        return Some((ProviderEvent::TextDelta("\n\n".into()), state));
                    }
                    // First message item, reasoning, and unknown items
                    // carry nothing to accumulate here; function_call
                    // items get their entry above or on `done`.
                    _ => {}
                }
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
        effort: Option<&str>,
        input: MessageInput<'_>,
        tools: &[ToolSpec],
        cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        // mu-vcbm: a per-turn `/effort` selection maps onto Codex's
        // `reasoning.effort`, overriding the construction-time
        // `--thinking` default (`self.thinking`) for THIS call. Codex's
        // accepted vocabulary is `minimal|low|medium|high`; an
        // out-of-vocabulary level surfaces as a provider 400 (level
        // validation is config-driven in a later slice — mu-vcbm step 2).
        let eff_thinking: &str = effort.unwrap_or(&self.thinking);
        // mu-yqeq.5: sealed-enum dispatch (Legacy + Projected). The
        // `_` arm remains for forward-compat with future MessageInput
        // variants — adding one will compile-warn here for review.
        //
        // Projected arm produces byte-identical wire JSON to the
        // Legacy arm; the `yqeq5_parity_*` tests in
        // openai_codex_tests.rs assert that invariant for the
        // canonical scenarios. The agent loop's mod.rs:818 still
        // passes Legacy until mu-yqeq.8 wires the cutover.
        let req = match input {
            MessageInput::Legacy(msgs) => {
                // mu-n48: a session-level system_prompt overrides
                // the provider's default `instructions`.
                let instructions: &str = system_prompt
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&self.instructions);
                super::openai_responses::build_request_from_legacy(
                    &self.model,
                    eff_thinking,
                    instructions,
                    msgs,
                    tools,
                    true,
                )
            }
            MessageInput::Projected(pmsgs) => {
                // In the Projected path the projection itself carries
                // the session system prompt (assemble_rope put it
                // there from `system_prompt`); the helper hoists it
                // into `body.instructions` and falls back to
                // `self.instructions` when the projection has no
                // (or an empty) system span — matching Legacy's
                // `.filter(|s| !s.is_empty()).unwrap_or(...)` rule.
                super::openai_responses::build_request_from_projection(
                    &self.model,
                    eff_thinking,
                    &self.instructions,
                    pmsgs,
                    tools,
                    true,
                )
            }
            _ => {
                return Err(ProviderError::Other(
                    "OpenaiCodexProvider: unrecognized MessageInput variant".to_string(),
                ));
            }
        };
        let body = serde_json::to_value(&req)
            .map_err(|e| ProviderError::Other(format!("serialize codex request: {e}")))?;

        // mu-solo debug: surface the actual wire body for codex calls
        // so we can diff what's being sent across sessions. Gated by
        // RUST_LOG=mu_ai=debug (or mu_ai::providers::openai_codex=debug
        // for just this site). Pretty-prints the JSON; large but
        // useful when investigating empty-response cases.
        debug!(
            target: "mu_ai::providers::openai_codex",
            "codex request body: {}",
            serde_json::to_string_pretty(&body)
                .unwrap_or_else(|e| format!("(serialize failed: {e})"))
        );

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

    /// Identify as `"openai_codex"` so ContextAssembly events and
    /// downstream diagnostics (mu-solo's renderer-mismatch warning,
    /// etc.) don't see the default `"faux"` label and conclude the
    /// daemon silently fell back to FauxProvider when it didn't.
    /// Matches the snake_case wire `provider_kind` enum.
    fn provider_label(&self) -> &'static str {
        "openai_codex"
    }

    /// What we know empirically about the Responses API:
    /// - Dedicated `instructions` top-level field with a ~8KB soft
    ///   cap (see [`INSTRUCTIONS_SOFT_CAP`] — codex closes the SSE
    ///   stream with zero events when this is exceeded).
    /// - No published prompt-caching surface.
    /// - The `developer` role exists distinct from `system`/`user`/
    ///   `assistant`; not currently used by mu but available.
    /// - Context window varies by model — left as None pending
    ///   per-model capability lookup.
    fn capabilities(&self) -> mu_core::agent::capabilities::ProviderCapabilities {
        use mu_core::agent::capabilities::{
            ProviderCapabilities, SystemPromptCapability, UsageSemantics,
        };
        ProviderCapabilities {
            system_prompt: SystemPromptCapability::TopLevelField {
                max_bytes: Some(INSTRUCTIONS_SOFT_CAP),
            },
            supports_prompt_caching: false,
            supports_developer_role: true,
            max_tools: None,
            context_window_tokens: None,
            // Responses API: input_tokens is the total prompt
            // (input_tokens_details.cached_tokens is a subset);
            // output_tokens includes reasoning.
            usage_semantics: UsageSemantics::openai_style(),
        }
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
