//! OpenAI Responses API provider — welds the typed `mu-openai` wire
//! crate (request build + stream parse) to mu-side transport + auth.
//!
//! Two auth/endpoint modes, one provider:
//!
//! - **Codex (OAuth):** direct HTTP to
//!   `https://chatgpt.com/backend-api/codex/responses` using OAuth
//!   tokens stored by `mu login --provider openai-codex`. Token Mutex +
//!   optional TokenStore + 401-refresh-and-retry; the `chatgpt-account-id`
//!   header is pulled out of the access_token JWT.
//! - **Public (API key):** direct HTTP to
//!   `https://api.openai.com/v1/responses` with a Bearer key (from
//!   `OPENAI_API_KEY` env or `~/.config/agent/config.toml`'s
//!   `[openai].api_key`). No account-id header, no refresh.
//!
//! Mirrors `providers/anthropic.rs`: the typed crate (`mu_openai`) owns
//! the wire shape; this module owns reqwest transport, the SSE byte
//! framing (via the shared `super::sse::SseStream`), and the
//! mu↔wire translation. See spec mu-019 (codex), the `mu-openai`
//! crate's `INTEGRATION.md` for the seam, and `anthropic.rs` for the
//! template.
//!
//! REASONING THREADING (PR-B): the OpenAI Responses contract requires a
//! `reasoning` item returned in a prior response's `output` to be fed
//! back verbatim on the next turn (alongside
//! `include: ["reasoning.encrypted_content"]` when running stateless
//! with `store=false`), or the model loses its chain-of-thought across
//! tool calls and can stall with an empty reasoning-only turn. We thread
//! it through `ContentBlock::Thinking`:
//!
//! - INBOUND (terminal snapshot → `AssistantMessage`):
//!   [`adopt_snapshot_output`] turns each `OutputItem::Reasoning` into a
//!   `ContentBlock::Thinking { text, opaque }` where `text` is the
//!   summary's concatenated display text and `opaque` is the encoded
//!   reasoning item (id + encrypted_content + summary + content), kept
//!   in output ORDER ahead of the function_call it reasoned about.
//! - OUTBOUND ([`translate_assistant_blocks`]): a `Thinking` block WITH
//!   `opaque` decodes back to an `InputItem::Reasoning`, emitted BEFORE
//!   the turn's `function_call` items. A `Thinking` block WITHOUT
//!   `opaque` (anthropic-origin, or none) is still dropped.
//! - REQUEST ([`build_request`]): adds
//!   `include: ["reasoning.encrypted_content"]` so the backend returns
//!   `encrypted_content`.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use bytes::Bytes;
use futures::stream::{BoxStream, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{oneshot, Mutex};
use tracing::debug;

use mu_openai::{
    CreateResponseRequest, FunctionTool, IncompleteDetails as OpenaiIncompleteDetails, InputItem,
    JsonValue as OpenaiJsonValue, OutputContent, OutputItem, Reasoning, Response, ResponseStatus,
    ResponseStreamEvent, Tool, ToolChoice, Usage as OpenaiUsage,
};

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

const CODEX_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
const PUBLIC_ENDPOINT: &str = "https://api.openai.com/v1/responses";
const ORIGINATOR: &str = "mu";
const DEFAULT_THINKING: &str = "medium";
/// TokenStore key for the codex OAuth bundle.
const CODEX_PROVIDER_KEY: &str = "openai-codex";

/// Default `instructions` (the Responses API's required system-prompt
/// field). Kept short and provider-agnostic — actual agent-loop
/// behavior (tool use, output style) is driven by mu's own loop, not
/// by this string. Callers can override via `with_instructions`.
const DEFAULT_INSTRUCTIONS: &str = "You are mu, a coding agent. Respond concisely. \
     When tools are provided, prefer to use them rather than asking \
     the user for information you could obtain yourself.";

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

// ============================================================================
// Auth mode
// ============================================================================

/// How this provider authenticates and where it sends requests.
enum AuthMode {
    /// OAuth subscription path: a rotating bundle (swapped atomically on
    /// 401-refresh) plus an optional persistence store. `None` store =
    /// ephemeral (refresh in-memory only).
    Codex {
        /// Wrapped in `Mutex` so a 401-refresh can swap it atomically.
        /// Reads clone the bundle and release the lock immediately so
        /// the lock is never held across an `await`.
        token: Mutex<OAuthToken>,
        /// `None` = ephemeral (don't persist refreshed tokens).
        store: Option<Arc<dyn TokenStore>>,
    },
    /// Direct API-key path: a static Bearer key. No refresh.
    Public { api_key: String },
}

// ============================================================================
// Provider struct
// ============================================================================

pub struct OpenaiProvider {
    model: String,
    thinking: String,
    instructions: String,
    mode: AuthMode,
    http: reqwest::Client,
    /// Test seam — defaults to the mode's canonical endpoint.
    endpoint: String,
}

impl OpenaiProvider {
    // ---- Codex (OAuth) constructors ----

    /// Load the bundle from `~/.config/mu/auth/openai-codex.json`.
    /// Fails if not logged in.
    pub fn from_store(model: String) -> Result<Self, ProviderError> {
        let store = FileSystemTokenStore::default_location().map_err(map_auth_err)?;
        let token = load_token(&store)?;
        Ok(Self::codex(model, token, Some(Arc::new(store))))
    }

    /// Like `from_store` but won't persist refreshed tokens back
    /// to disk. The in-memory bundle still rotates on 401-refresh.
    pub fn from_store_ephemeral(model: String) -> Result<Self, ProviderError> {
        let store = FileSystemTokenStore::default_location().map_err(map_auth_err)?;
        let token = load_token(&store)?;
        Ok(Self::codex(model, token, None))
    }

    /// Caller-supplied token and (optional) store. For tests and
    /// embedders.
    pub fn from_parts(
        model: String,
        token: OAuthToken,
        store: Option<Arc<dyn TokenStore>>,
    ) -> Self {
        Self::codex(model, token, store)
    }

    fn codex(model: String, token: OAuthToken, store: Option<Arc<dyn TokenStore>>) -> Self {
        Self {
            model,
            thinking: DEFAULT_THINKING.into(),
            instructions: DEFAULT_INSTRUCTIONS.into(),
            mode: AuthMode::Codex {
                token: Mutex::new(token),
                store,
            },
            http: reqwest::Client::new(),
            endpoint: CODEX_ENDPOINT.into(),
        }
    }

    // ---- Public (API key) constructors ----

    /// Direct API-key provider against `api.openai.com`. When
    /// `OPENAI_BASE_URL` is set (and non-empty), the Responses endpoint
    /// becomes `{OPENAI_BASE_URL}/v1/responses` instead — e.g. a LAN ollama
    /// box that serves the Responses API. Mirrors `providers/anthropic.rs`'s
    /// `ANTHROPIC_BASE_URL` handling.
    pub fn from_api_key(model: String, api_key: String) -> Self {
        let endpoint = std::env::var("OPENAI_BASE_URL")
            .ok()
            .filter(|b| !b.trim().is_empty())
            .map(|b| format!("{}/v1/responses", b.trim_end_matches('/')))
            .unwrap_or_else(|| PUBLIC_ENDPOINT.into());
        Self {
            model,
            thinking: DEFAULT_THINKING.into(),
            instructions: DEFAULT_INSTRUCTIONS.into(),
            mode: AuthMode::Public { api_key },
            http: reqwest::Client::new(),
            endpoint,
        }
    }

    /// Resolve the public API key from `OPENAI_API_KEY` (env) or, failing
    /// that, `[openai].api_key` in `~/.config/agent/config.toml` (the
    /// operator's centralized agent config — same file `t4c` reads keys
    /// from; `T4C_AGENT_CONFIG` overrides the path). Errors with guidance
    /// when neither is set.
    pub fn from_env(model: String) -> Result<Self, ProviderError> {
        let key = resolve_public_api_key()?;
        Ok(Self::from_api_key(model, key))
    }

    // ---- shared builders ----

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

    #[cfg(test)]
    fn is_codex(&self) -> bool {
        matches!(self.mode, AuthMode::Codex { .. })
    }
}

fn map_auth_err(e: auth::AuthError) -> ProviderError {
    ProviderError::Other(format!("auth: {e}"))
}

fn load_token<S: TokenStore + ?Sized>(store: &S) -> Result<OAuthToken, ProviderError> {
    let opt = store.load(CODEX_PROVIDER_KEY).map_err(map_auth_err)?;
    opt.ok_or_else(|| {
        ProviderError::Other(
            "not logged in to openai-codex — run \
             `mu login --provider openai-codex`"
                .into(),
        )
    })
}

/// Resolve the public OpenAI API key: `OPENAI_API_KEY` env first, then
/// `[openai].api_key` from the agent config TOML.
fn resolve_public_api_key() -> Result<String, ProviderError> {
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        if !key.is_empty() {
            return Ok(key);
        }
    }
    if let Some(key) = api_key_from_agent_config() {
        if !key.is_empty() {
            return Ok(key);
        }
    }
    Err(ProviderError::Other(
        "no OpenAI API key — set OPENAI_API_KEY or put `[openai] api_key = \"...\"` \
         in ~/.config/agent/config.toml (or use `--provider openai-codex` for the \
         subscription path)"
            .into(),
    ))
}

/// Read `[openai].api_key` from `~/.config/agent/config.toml`
/// (`T4C_AGENT_CONFIG` overrides the path). Returns `None` on any
/// failure (missing file, unparseable TOML, absent key) — the caller
/// turns that into a single actionable error.
fn api_key_from_agent_config() -> Option<String> {
    let path = std::env::var("T4C_AGENT_CONFIG").ok().or_else(|| {
        std::env::var("HOME")
            .ok()
            .map(|home| format!("{home}/.config/agent/config.toml"))
    })?;
    let text = std::fs::read_to_string(&path).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    value
        .get("openai")
        .and_then(|t| t.get("api_key"))
        .and_then(|k| k.as_str())
        .map(str::to_string)
}

// ============================================================================
// JWT claim extraction (codex mode)
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
// mu -> mu_openai request mapping (typed; request bytes come from the crate)
//
// Distinct from Chat Completions: the input is an array of typed items
// (message, function_call, function_call_output) rather than role-tagged
// messages. Tool definitions are flat — name/description/parameters at
// the tool's top level (no nested "function" wrapper). Streaming is via
// typed SSE events, not JSON chunks of choices[].
// ============================================================================

/// Wrap a `serde_json::Value` as a mu_openai `JsonValue`. Tool schemas
/// are finite by construction; the impossible non-finite case degrades
/// to an empty object rather than panicking.
fn openai_json(v: Value) -> OpenaiJsonValue {
    OpenaiJsonValue::new(v).unwrap_or_else(|_| OpenaiJsonValue::empty_object())
}

// ============================================================================
// Reasoning round-trip token (PR-B)
//
// `ContentBlock::Thinking.opaque` is a provider-owned string mu-core never
// interprets. For OpenAI it carries one `OutputItem::Reasoning` (id +
// encrypted_content + summary + content) serialized as a compact JSON object so
// the next turn can re-emit it verbatim as `InputItem::Reasoning`, preserving
// chain-of-thought across tool calls.
// ============================================================================

/// The reasoning-item fields we round-trip. `summary` and `content` are the
/// wire-typed `JsonValue` vecs (echoed back untouched); `encrypted_content` is
/// the opaque blob the backend returns under `include:
/// ["reasoning.encrypted_content"]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReasoningToken {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    encrypted_content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    summary: Vec<OpenaiJsonValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    content: Vec<OpenaiJsonValue>,
}

/// Serialize a reasoning item into the compact JSON string stored in
/// `ContentBlock::Thinking.opaque`. Infallible in practice (the fields hold only
/// finite, serializable data); a failure degrades to `None` so the block becomes
/// a plain (drop-on-resend) Thinking block rather than poisoning the turn.
fn encode_reasoning_token(
    id: &str,
    encrypted_content: &Option<String>,
    summary: &[OpenaiJsonValue],
    content: &[OpenaiJsonValue],
) -> Option<String> {
    let tok = ReasoningToken {
        id: id.to_string(),
        encrypted_content: encrypted_content.clone(),
        summary: summary.to_vec(),
        content: content.to_vec(),
    };
    serde_json::to_string(&tok)
        .map_err(|e| tracing::warn!(error = %e, "failed to encode reasoning token"))
        .ok()
}

/// Decode a `ContentBlock::Thinking.opaque` token back into an
/// `InputItem::Reasoning`. Returns `None` for any token that isn't our encoding
/// (e.g. a future/foreign opaque shape), so a non-OpenAI Thinking block is
/// simply dropped rather than mistranslated.
fn decode_reasoning_token(opaque: &str) -> Option<InputItem> {
    let tok: ReasoningToken = serde_json::from_str(opaque).ok()?;
    Some(InputItem::Reasoning {
        id: tok.id,
        summary: tok.summary,
        encrypted_content: tok.encrypted_content,
        content: tok.content,
    })
}

/// Concatenate the human-displayable text out of a reasoning item's `summary`
/// parts. Each summary part is typically `{"type":"summary_text","text":"…"}`;
/// we pull `.text` from each and join with "\n". Returns "" when the summary is
/// empty (encrypted-only reasoning) — the chain-of-thought still rides in
/// `opaque`.
fn summary_display_text(summary: &[OpenaiJsonValue]) -> String {
    let mut pieces: Vec<&str> = Vec::new();
    for part in summary {
        if let Some(t) = part.as_value().get("text").and_then(Value::as_str) {
            if !t.is_empty() {
                pieces.push(t);
            }
        }
    }
    pieces.join("\n")
}

/// Translate a mu `ToolSpec` into a mu_openai `Tool::Function`.
fn translate_tool_spec(spec: &ToolSpec) -> Tool {
    Tool::Function(FunctionTool {
        name: spec.name.clone(),
        description: Some(spec.description.clone()),
        parameters: openai_json(spec.input_schema.clone()),
        // strict omitted (None) — matches the hand-rolled provider's
        // wire shape, which never set `strict`.
        strict: None,
    })
}

/// Translate a Legacy `AgentMessage` into zero or more `InputItem`s.
///
/// The Responses API treats text and function_calls as separate
/// top-level items (NOT nested under one message). `Thinking` blocks
/// are dropped — the model can't consume its own prior reasoning text
/// as input on this API (PR-B will thread reasoning items instead).
fn translate_message(m: &AgentMessage) -> Vec<InputItem> {
    match m {
        AgentMessage::User { content } => vec![InputItem::user_text(content.clone())],
        AgentMessage::Assistant(a) => translate_assistant_blocks(&a.content),
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
            vec![InputItem::FunctionCallOutput {
                call_id: call_id.clone(),
                output: body,
            }]
        }
    }
}

/// Translate an assistant's content blocks into the Responses API's
/// split shape: any `reasoning` items and `function_call` items in block
/// order, with a single `message` item (carrying the combined text)
/// prepended. Returns an empty `Vec` when the assistant has no
/// wire-bearing blocks.
///
/// Reasoning threading (PR-B): a `Thinking` block WITH `opaque` decodes
/// to an `InputItem::Reasoning` and is emitted IN ORDER — crucially
/// BEFORE the `function_call` it reasoned about, which the Responses API
/// requires. A `Thinking` block WITHOUT `opaque` (anthropic-origin or
/// none) is dropped: the model can't consume foreign reasoning text as
/// input on this API.
fn translate_assistant_blocks(blocks: &[ContentBlock]) -> Vec<InputItem> {
    let mut out: Vec<InputItem> = Vec::new();
    let mut text_parts: Vec<String> = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => text_parts.push(text.to_string()),
            ContentBlock::ToolCall(tc) => {
                let args_str =
                    serde_json::to_string(tc.arguments.as_value()).unwrap_or_else(|_| "{}".into());
                out.push(InputItem::FunctionCall {
                    call_id: tc.id.clone(),
                    name: tc.name.clone(),
                    arguments: args_str,
                    id: None,
                });
            }
            // Reasoning round-trip: re-emit the verbatim reasoning item
            // (kept ahead of the function_call below). A Thinking block
            // without an opaque token, or one whose token we can't
            // decode, is dropped.
            ContentBlock::Thinking {
                opaque: Some(opaque),
                ..
            } => {
                if let Some(item) = decode_reasoning_token(opaque) {
                    out.push(item);
                }
            }
            ContentBlock::Thinking { opaque: None, .. } => {}
        }
    }
    if !text_parts.is_empty() {
        // Prepend the assistant text message so it sits before the
        // reasoning / function_call items in turn order.
        out.insert(0, InputItem::assistant_text(text_parts.join("")));
    }
    out
}

/// Apply [`INSTRUCTIONS_SOFT_CAP`] to a candidate instructions string.
/// Returns the value to put in the `instructions` field and an optional
/// overflow body to prepend to `input`.
///
/// Below the cap → `(actual, None)`, identical to the pre-cap behavior.
/// At or above → `(DEFAULT_INSTRUCTIONS, Some(actual))`, so the model
/// still receives a short "you are mu" instruction in the dedicated
/// field and the long context lands as a regular input message.
fn split_oversized_instructions(actual: &str) -> (&str, Option<&str>) {
    if actual.len() > INSTRUCTIONS_SOFT_CAP {
        (DEFAULT_INSTRUCTIONS, Some(actual))
    } else {
        (actual, None)
    }
}

/// Build a single synthetic `role: "user"` input item wrapping the
/// overflow with a brief framing line. Goes at `input[0]`. We use
/// `role: "user"` rather than `developer` because the Responses API's
/// developer role is newer and not universally supported on all model
/// variants we might target via the same wire path.
fn make_instructions_overflow_item(content: &str) -> InputItem {
    let framed = format!(
        "[System context — too large for the instructions field. \
         Treat this as your standing instructions and project context, \
         not as a question to respond to directly.]\n\n{content}"
    );
    InputItem::user_text(framed)
}

/// Assemble a [`CreateResponseRequest`] from resolved instructions, the
/// already-translated `input` items, and tools. Shared by both the
/// Legacy and Projected paths so they produce byte-identical wire JSON.
fn build_request(
    model: &str,
    thinking: &str,
    instructions: &str,
    mut input: Vec<InputItem>,
    tools: &[ToolSpec],
) -> CreateResponseRequest {
    let (instructions_field, overflow) = split_oversized_instructions(instructions);
    if let Some(o) = overflow {
        input.insert(0, make_instructions_overflow_item(o));
    }

    let mut req = CreateResponseRequest::new(model, input)
        .with_instructions(instructions_field)
        .with_reasoning(Reasoning {
            effort: Some(thinking.to_string()),
            summary: Some("auto".to_string()),
        })
        .streaming();
    // mu runs stateless (store=false); CreateResponseRequest::new
    // already defaults store to Some(false), but be explicit.
    req.store = Some(false);
    // Ask the backend to return the reasoning item's encrypted_content
    // so we can thread it back verbatim on the next turn (stateless
    // chain-of-thought; PR-B). Required because store=false means the
    // backend won't recall the reasoning server-side.
    req.include = vec!["reasoning.encrypted_content".to_string()];

    if !tools.is_empty() {
        req = req.with_tools(tools.iter().map(translate_tool_spec).collect());
        req.tool_choice = Some(ToolChoice::auto());
        req.parallel_tool_calls = Some(false);
    }
    req
}

/// Legacy path: build the request from a `&[AgentMessage]` slice.
pub(crate) fn build_request_value(
    model: &str,
    thinking: &str,
    instructions: &str,
    messages: &[AgentMessage],
    tools: &[ToolSpec],
) -> Value {
    let input: Vec<InputItem> = messages.iter().flat_map(translate_message).collect();
    request_to_value(build_request(model, thinking, instructions, input, tools))
}

// ============================================================================
// Projected path — wire body built from &ProviderMessages
// ============================================================================
//
// Mirrors `translate_message` + `build_request` semantics but reads
// structural `ContentBlock`s from `ProviderMessage.blocks` instead of
// `AgentMessage::Assistant.content`. System spans (except tool-schema
// spans) are hoisted into the top-level `instructions` field; multiple
// system spans concatenate with "\n\n" (the Responses API has only one
// instructions slot). The `tools` parameter is authoritative for
// `body.tools`; tool-schema spans are dropped.
//
// Wire-format byte equivalence with the Legacy path is the contract —
// see the `parity_*` tests.

fn translate_provider_messages(pmsgs: &ProviderMessages) -> (Vec<InputItem>, Option<String>) {
    let mut out: Vec<InputItem> = Vec::with_capacity(pmsgs.messages.len());
    let mut system_text: Option<String> = None;

    for msg in &pmsgs.messages {
        match msg.role() {
            ProviderRole::System => {
                // Hoist ALL System-role spans into the Responses API's
                // `instructions` field EXCEPT tool-schema spans (those
                // are passed separately via `body.tools`). This includes
                // the system-prompt span plus memory-recall:* /
                // project-file:* / future System-role span kinds.
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
                            None => system_text = Some(content.to_string()),
                        }
                    }
                }
            }
            ProviderRole::User => {
                out.push(InputItem::user_text(msg.content()));
            }
            ProviderRole::Assistant => {
                if let Some(blocks) = msg.blocks() {
                    out.extend(translate_assistant_blocks(blocks));
                }
            }
            ProviderRole::ToolResult => {
                out.push(translate_provider_tool_result(msg));
            }
        }
    }

    (out, system_text)
}

/// Translate one tool-result [`ProviderMessage`] into a
/// `FunctionCallOutput` item. `call_id` is recovered from the
/// synthesized span id; `is_error` from the `"error: "` content prefix
/// `assembly.rs` adds. Errors are re-encoded as `"[error] {content}"` to
/// match the Legacy `translate_message` shape.
fn translate_provider_tool_result(msg: &ProviderMessage) -> InputItem {
    let call_id: String = msg
        .source_span_ids()
        .first()
        .and_then(|sid| extract_call_id_from_span_id(sid.as_ref()))
        .unwrap_or("")
        .to_string();
    let body: String = match msg.content().strip_prefix("error: ") {
        Some(stripped) => format!("[error] {stripped}"),
        None => msg.content().to_string(),
    };
    InputItem::FunctionCallOutput {
        call_id,
        output: body,
    }
}

/// Projected sibling of [`build_request_value`]: build the request body
/// from a [`ProviderMessages`] projection. `default_instructions` is the
/// provider's static fallback (used when the projection has no hoisted
/// system span or it's empty) — matching Legacy's
/// `system_prompt.filter(|s| !s.is_empty()).unwrap_or(&self.instructions)`.
pub(crate) fn build_request_value_from_projection(
    model: &str,
    thinking: &str,
    default_instructions: &str,
    pmsgs: &ProviderMessages,
    tools: &[ToolSpec],
) -> Value {
    let (input, hoisted_system) = translate_provider_messages(pmsgs);
    let instructions: &str = match hoisted_system.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => default_instructions,
    };
    request_to_value(build_request(model, thinking, instructions, input, tools))
}

/// Serialize a built `CreateResponseRequest` to the wire `Value`.
/// Infallible in practice (the request holds only finite, serializable
/// data); a serialization failure degrades to an empty object and is
/// logged rather than panicking.
fn request_to_value(req: CreateResponseRequest) -> Value {
    serde_json::to_value(&req).unwrap_or_else(|e| {
        tracing::error!(error = %e, "failed to serialize CreateResponseRequest");
        Value::Object(serde_json::Map::new())
    })
}

// ============================================================================
// HTTP error rendering (mu-rb4u)
// ============================================================================

/// Render a non-2xx Responses-backend reply into a legible provider
/// error. The common case in the wild is a 429 `usage_limit_reached` —
/// a per-subscription cap with a `resets_in_seconds` window, NOT a
/// transient rate limit. Surfacing it cleanly (plan + reset window)
/// tells the operator it's a cap and when it clears, instead of dumping
/// the raw JSON into the agent's error event. (mu-rb4u)
fn render_codex_http_error(status: reqwest::StatusCode, body: &str) -> String {
    #[derive(Deserialize)]
    struct ErrBody {
        error: Option<ErrInner>,
    }
    #[derive(Deserialize)]
    struct ErrInner {
        #[serde(default, rename = "type")]
        type_: Option<String>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        plan_type: Option<String>,
        #[serde(default)]
        resets_in_seconds: Option<u64>,
    }

    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        if let Ok(ErrBody { error: Some(e) }) = serde_json::from_str::<ErrBody>(body) {
            if e.type_.as_deref() == Some("usage_limit_reached") {
                let plan = e.plan_type.as_deref().unwrap_or("unknown");
                let resets = match e.resets_in_seconds {
                    Some(s) => format!("; resets in ~{}m{:02}s", s / 60, s % 60),
                    None => String::new(),
                };
                return format!(
                    "codex usage limit reached (plan {plan}){resets}. This is a \
                     per-subscription cap, not a transient rate limit — wait for the \
                     reset or switch providers (e.g. `/model`, or `--provider`)."
                );
            }
            if let Some(msg) = e.message.as_deref() {
                return format!("codex rate limited (429): {msg}");
            }
        }
        return format!("codex rate limited (429): {body}");
    }

    format!("openai returned {status}: {body}")
}

// ============================================================================
// SSE → ProviderEvent fold
//
// Responses API typed events (`mu_openai::ResponseStreamEvent`) we fold:
//   - OutputTextDelta              → TextDelta
//   - OutputItemAdded(message)     → paragraph break between message
//                                     items (mu-s545) once text exists
//   - OutputItemAdded(function_call) → seed the tool-call accumulator
//   - FunctionCallArgumentsDelta / *Compat → append args, emit ToolCallDelta
//   - FunctionCallArgumentsDone / OutputItemDone(function_call) → finalize
//   - ReasoningSummaryTextDelta / ReasoningTextDelta → ThinkingDelta
//   - Completed / Incomplete       → Done(...) using the response snapshot
//   - Failed / ResponseError / Error → Error
// Other events are noise — ignored.
// ============================================================================

#[derive(Default)]
struct ToolCallBuilder {
    /// item id (e.g. `fc_...`) — keys the accumulator
    item_id: String,
    /// call id (e.g. `call_...`) — matches function_call_output later
    call_id: String,
    name: String,
    args_json: String,
}

struct StreamState {
    sse: Pin<Box<dyn Stream<Item = SseEvent> + Send>>,
    accumulated_text: String,
    /// Tool-call builders keyed by `output_index`.
    tool_calls: HashMap<u32, ToolCallBuilder>,
    tool_call_order: Vec<u32>,
    /// `item_id` → `output_index`, so argument-delta events (which carry
    /// only `item_id`) can find their accumulator.
    item_id_to_index: HashMap<String, u32>,
    /// Reasoning items captured from streamed `response.output_item.done`
    /// (codex sends them there, with `encrypted_content`, and the terminal
    /// snapshot often omits them), keyed by `output_index` so they stay
    /// ahead of the function_call they reason about. Each is an encoded
    /// `ContentBlock::Thinking { opaque }`. Used by [`assemble_content`]'s
    /// streamed fallback (the snapshot path carries its own reasoning).
    streamed_reasoning: Vec<(u32, ContentBlock)>,
    final_status: Option<ResponseStatus>,
    incomplete_reason: Option<String>,
    /// Authoritative, fully-ordered content blocks adopted from a terminal
    /// response snapshot (reasoning / text / tool calls in `output` order).
    /// `Some` once a terminal snapshot with non-empty `output` has been
    /// applied; [`assemble_content`] prefers it so reasoning stays threaded
    /// ahead of the function_call it belongs to. The streamed `tool_calls` /
    /// `accumulated_text` are still populated alongside (they drive
    /// `map_stop` and the EOF diagnostics).
    snapshot_content: Option<Vec<ContentBlock>>,
    /// Usage from the terminal lifecycle event's `response.usage`.
    usage: Option<Usage>,
    cancel_rx: Option<oneshot::Receiver<()>>,
    finished: bool,
    emitted_done: bool,
    /// Errors that arrived via response.failed / response.error / error.
    error_message: Option<String>,
}

fn events_stream(
    bytes: impl Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
    cancel_rx: oneshot::Receiver<()>,
) -> BoxStream<'static, ProviderEvent> {
    let bytes: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>> = Box::pin(bytes);
    let sse = SseStream::new(bytes);
    let state = new_stream_state(Box::pin(sse), cancel_rx);
    Box::pin(futures::stream::unfold(state, next_event))
}

fn new_stream_state(
    sse: Pin<Box<dyn Stream<Item = SseEvent> + Send>>,
    cancel_rx: oneshot::Receiver<()>,
) -> StreamState {
    StreamState {
        sse,
        accumulated_text: String::new(),
        tool_calls: HashMap::new(),
        tool_call_order: Vec::new(),
        item_id_to_index: HashMap::new(),
        streamed_reasoning: Vec::new(),
        final_status: None,
        incomplete_reason: None,
        snapshot_content: None,
        usage: None,
        cancel_rx: Some(cancel_rx),
        finished: false,
        emitted_done: false,
        error_message: None,
    }
}

/// Project mu_openai's `Usage` onto mu-core's `Usage`. The Responses
/// API's `input_tokens` is the total prompt (with `cached_tokens` a
/// subset); `output_tokens` includes reasoning.
fn openai_usage_to_mu(u: &OpenaiUsage) -> Usage {
    Usage {
        input_tokens: u.input_tokens.unwrap_or(0),
        output_tokens: u.output_tokens.unwrap_or(0),
        cache_read_input_tokens: u
            .input_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens),
        cache_creation_input_tokens: None,
        cache_creation_5m_input_tokens: None,
        cache_creation_1h_input_tokens: None,
        reasoning_tokens: u
            .output_tokens_details
            .as_ref()
            .and_then(|d| d.reasoning_tokens),
    }
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
        match state.final_status {
            Some(ResponseStatus::Completed) => StopReason::EndTurn,
            Some(ResponseStatus::Incomplete) => StopReason::MaxTokens,
            Some(ResponseStatus::Failed) => StopReason::Error,
            _ => StopReason::EndTurn,
        }
    }
}

/// Capture a streamed reasoning item (from `response.output_item.added/done`)
/// into `streamed_reasoning`, keyed by `output_index`. A later event at the same
/// index supersedes an earlier one (`.done` over `.added`). The item is encoded
/// into a `ContentBlock::Thinking { opaque }` that round-trips the full reasoning
/// item (id + encrypted_content + summary + content) for re-emission next turn.
fn capture_streamed_reasoning(
    state: &mut StreamState,
    output_index: u32,
    id: String,
    encrypted_content: Option<String>,
    summary: Vec<OpenaiJsonValue>,
    content: Vec<OpenaiJsonValue>,
) {
    let opaque = encode_reasoning_token(&id, &encrypted_content, &summary, &content);
    let block = ContentBlock::Thinking {
        text: summary_display_text(&summary).as_str().into(),
        opaque: opaque.map(|s| s.as_str().into()),
    };
    match state
        .streamed_reasoning
        .iter_mut()
        .find(|(i, _)| *i == output_index)
    {
        Some(slot) => slot.1 = block,
        None => state.streamed_reasoning.push((output_index, block)),
    }
}

fn assemble_content(state: &StreamState) -> Vec<ContentBlock> {
    // Streamed reasoning blocks (from `output_item.done`), ordered by
    // output_index so they precede the function_call they reason about.
    let reasoning: Vec<ContentBlock> = {
        let mut v: Vec<&(u32, ContentBlock)> = state.streamed_reasoning.iter().collect();
        v.sort_by_key(|(idx, _)| *idx);
        v.into_iter().map(|(_, b)| b.clone()).collect()
    };

    // A terminal snapshot, when present, is authoritative AND fully ordered.
    // The public Responses API includes reasoning items in it; the ChatGPT/
    // codex backend OMITS them from the snapshot but streams them on
    // `output_item.done`. So: use the snapshot as-is when it already carries
    // reasoning (or we caught none); otherwise prepend the streamed reasoning
    // we captured, which is the codex path's only reasoning source.
    if let Some(blocks) = &state.snapshot_content {
        let snapshot_has_reasoning = blocks.iter().any(|b| {
            matches!(
                b,
                ContentBlock::Thinking {
                    opaque: Some(_),
                    ..
                }
            )
        });
        if snapshot_has_reasoning || reasoning.is_empty() {
            return blocks.clone();
        }
        let mut out = reasoning;
        out.extend(blocks.iter().cloned());
        return out;
    }

    // Streamed fallback (no terminal snapshot): reasoning, then text, then
    // tool calls.
    let mut out: Vec<ContentBlock> = reasoning;
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

/// Get-or-create the tool-call builder for an `output_index`,
/// registering it in `tool_call_order` exactly once.
fn tool_call_entry(state: &mut StreamState, output_index: u32) -> &mut ToolCallBuilder {
    if !state.tool_calls.contains_key(&output_index) {
        state.tool_call_order.push(output_index);
        state
            .tool_calls
            .insert(output_index, ToolCallBuilder::default());
    }
    state
        .tool_calls
        .get_mut(&output_index)
        .expect("just inserted")
}

/// Seed a tool-call builder from a `function_call` output item (fields
/// arrive on `output_item.added` and again, authoritatively, on
/// `output_item.done`). The final `arguments` on `done` overrides any
/// streamed accumulation.
fn seed_function_call(
    state: &mut StreamState,
    output_index: u32,
    id: Option<String>,
    call_id: Option<String>,
    name: Option<String>,
    arguments: Option<String>,
    args_authoritative: bool,
) {
    if let Some(ref item_id) = id {
        state.item_id_to_index.insert(item_id.clone(), output_index);
    }
    let entry = tool_call_entry(state, output_index);
    if let Some(v) = id {
        entry.item_id = v;
    }
    if let Some(v) = call_id {
        entry.call_id = v;
    }
    if let Some(v) = name {
        entry.name = v;
    }
    if let Some(v) = arguments {
        if !v.is_empty() {
            if args_authoritative {
                entry.args_json = v;
            } else {
                entry.args_json.push_str(&v);
            }
        }
    }
}

/// Apply a terminal lifecycle snapshot (`Completed` / `Incomplete` /
/// `Failed`) to state: record status/usage/incomplete-reason and, when
/// the model returned a full `output`, prefer it over the streamed
/// accumulation (authoritative, mirrors `mu_openai::accumulate`).
fn apply_terminal_response(state: &mut StreamState, response: &Response) {
    state.final_status = response.status;
    state.incomplete_reason = response
        .incomplete_details
        .as_ref()
        .and_then(|d: &OpenaiIncompleteDetails| d.reason.clone());
    if let Some(u) = response.usage.as_ref() {
        state.usage = Some(openai_usage_to_mu(u));
    }
    if !response.output.is_empty() {
        adopt_snapshot_output(state, &response.output);
    }
}

/// Replace streamed accumulation with the authoritative `output` items
/// from a terminal response snapshot. Builds a fully-ordered
/// `Vec<ContentBlock>` (reasoning → `Thinking{opaque}`, message → `Text`,
/// function_call → `ToolCall`) into [`StreamState::snapshot_content`],
/// preserving output order so each `reasoning` item precedes the
/// `function_call` it reasoned about. Text from contiguous `message`
/// items is joined with "\n\n" (the mu-s545 message-boundary rule) into a
/// single `Text` block. The streamed `tool_calls` / `accumulated_text`
/// are repopulated alongside so `map_stop` and the EOF diagnostics keep
/// working.
fn adopt_snapshot_output(state: &mut StreamState, output: &[OutputItem]) {
    state.tool_calls.clear();
    state.tool_call_order.clear();
    // Clear the streamed item_id→index map too: we're replacing the
    // tool-call state with fresh snapshot indices, so the old mappings are
    // stale. (Harmless today since this runs on the terminal event and the
    // map is not read afterward, but leaving stale entries is a trap for any
    // future post-snapshot delta handling.)
    state.item_id_to_index.clear();

    let mut blocks: Vec<ContentBlock> = Vec::new();
    let mut all_text = String::new();
    // Pending text from a contiguous run of message items, flushed as one
    // Text block when a non-message item interrupts the run (or at end).
    let mut pending_text = String::new();
    let mut next_idx: u32 = 0;

    fn flush_text(pending: &mut String, blocks: &mut Vec<ContentBlock>) {
        if !pending.is_empty() {
            blocks.push(ContentBlock::Text {
                text: pending.as_str().into(),
            });
            pending.clear();
        }
    }

    for item in output {
        match item {
            OutputItem::Message { content, .. } => {
                let mut piece = String::new();
                for part in content {
                    if let OutputContent::OutputText { text: t, .. } = part {
                        piece.push_str(t);
                    }
                }
                if !piece.is_empty() {
                    if !pending_text.is_empty() {
                        pending_text.push_str("\n\n");
                    }
                    pending_text.push_str(&piece);
                    if !all_text.is_empty() {
                        all_text.push_str("\n\n");
                    }
                    all_text.push_str(&piece);
                }
            }
            OutputItem::FunctionCall {
                call_id,
                name,
                arguments,
                id,
                ..
            } => {
                flush_text(&mut pending_text, &mut blocks);
                let idx = next_idx;
                next_idx += 1;
                state.tool_call_order.push(idx);
                state.tool_calls.insert(
                    idx,
                    ToolCallBuilder {
                        item_id: id.clone(),
                        call_id: call_id.clone().unwrap_or_default(),
                        name: name.clone().unwrap_or_default(),
                        args_json: arguments.clone().unwrap_or_default(),
                    },
                );
                let arguments = parse_tool_input(arguments.as_deref().unwrap_or(""));
                blocks.push(ContentBlock::ToolCall(ToolCall {
                    id: call_id.clone().unwrap_or_default(),
                    name: name.clone().unwrap_or_default(),
                    arguments,
                }));
            }
            OutputItem::Reasoning {
                id,
                summary,
                encrypted_content,
                content,
                ..
            } => {
                flush_text(&mut pending_text, &mut blocks);
                // `text` = displayable summary (may be empty); `opaque`
                // = the encoded reasoning item (id + encrypted_content +
                // summary + content) for verbatim re-emission next turn.
                let opaque =
                    encode_reasoning_token(id, encrypted_content, summary, content).map(Into::into);
                blocks.push(ContentBlock::Thinking {
                    text: summary_display_text(summary).into(),
                    opaque,
                });
            }
            // Unknown items: ignored (the drift canary in mu-openai owns
            // surfacing un-modeled shapes).
            OutputItem::Unknown(_) => {}
        }
    }
    flush_text(&mut pending_text, &mut blocks);

    state.accumulated_text = all_text;
    state.snapshot_content = Some(blocks);
}

/// Build the terminal `Done` event from current state.
fn done_event(state: &StreamState, stop: StopReason) -> ProviderEvent {
    ProviderEvent::Done(AssistantMessage {
        content: assemble_content(state),
        stop_reason: stop,
        usage: state.usage,
    })
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
                    let ev = done_event(&state, StopReason::Aborted);
                    return Some((ev, state));
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
                            target: "mu_ai::providers::openai",
                            "openai stream ended with error: {msg}"
                        );
                        return Some((ProviderEvent::Error(msg), state));
                    }
                    let stop = map_stop(&state);
                    debug!(
                        target: "mu_ai::providers::openai",
                        "openai stream EOF: text_len={} tool_calls={} final_status={:?} \
                         incomplete_reason={:?} usage_present={} stop={:?}",
                        state.accumulated_text.len(),
                        state.tool_calls.len(),
                        state.final_status,
                        state.incomplete_reason,
                        state.usage.is_some(),
                        stop,
                    );
                    let ev = done_event(&state, stop);
                    return Some((ev, state));
                }
                return None;
            }
        };

        // Skip empty data lines (keep-alive style).
        if sse_event.data.trim().is_empty() {
            continue;
        }
        // Some backends emit explicit `[DONE]`; treat like EOF.
        if sse_event.data.trim() == "[DONE]" {
            state.finished = true;
            if !state.emitted_done {
                state.emitted_done = true;
                let stop = map_stop(&state);
                let ev = done_event(&state, stop);
                return Some((ev, state));
            }
            return None;
        }

        let frame: ResponseStreamEvent = match serde_json::from_str(&sse_event.data) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, data = %sse_event.data, "failed to parse openai SSE frame");
                continue;
            }
        };

        if let Some(emit) = fold_frame(&mut state, frame) {
            return Some((emit, state));
        }
        // Loop and pull the next frame.
    }
}

/// Fold one typed stream event into `state`, optionally yielding a
/// `ProviderEvent` to emit now. `None` ⇒ event consumed silently
/// (accumulated only), keep pulling.
fn fold_frame(state: &mut StreamState, frame: ResponseStreamEvent) -> Option<ProviderEvent> {
    match frame {
        ResponseStreamEvent::OutputTextDelta { delta, .. } => {
            if delta.is_empty() {
                return None;
            }
            state.accumulated_text.push_str(&delta);
            Some(ProviderEvent::TextDelta(delta))
        }
        ResponseStreamEvent::OutputItemAdded {
            output_index, item, ..
        } => match item {
            OutputItem::FunctionCall {
                id,
                call_id,
                name,
                arguments,
                ..
            } => {
                seed_function_call(
                    state,
                    output_index,
                    Some(id),
                    call_id,
                    name,
                    arguments,
                    false,
                );
                None
            }
            // mu-s545: one response can carry MULTIPLE message output
            // items. All output_text deltas funnel into the single
            // accumulator, so without a boundary the items fuse without
            // whitespace ("...or hold.No worries..."). Insert a
            // paragraph break between message items — in the accumulator
            // AND on the wire, so the streamed preview matches the
            // finalized text (mu-wk2 invariant). Guarded: a first/only
            // message item (empty accumulator — incl. message-after-
            // toolcall) gets no separator.
            OutputItem::Message { .. } if !state.accumulated_text.is_empty() => {
                state.accumulated_text.push_str("\n\n");
                Some(ProviderEvent::TextDelta("\n\n".into()))
            }
            OutputItem::Reasoning {
                id,
                summary,
                encrypted_content,
                content,
                ..
            } => {
                capture_streamed_reasoning(
                    state,
                    output_index,
                    id,
                    encrypted_content,
                    summary,
                    content,
                );
                None
            }
            _ => None,
        },
        ResponseStreamEvent::FunctionCallArgumentsDelta {
            item_id,
            output_index,
            delta,
            ..
        }
        | ResponseStreamEvent::FunctionCallArgumentsDeltaCompat {
            item_id,
            output_index,
            delta,
            ..
        } => {
            // The arg-delta event carries `item_id` + `output_index`. We
            // key accumulators on output_index; record the mapping too.
            state.item_id_to_index.insert(item_id, output_index);
            let entry = tool_call_entry(state, output_index);
            entry.args_json.push_str(&delta);
            let id = entry.call_id.clone();
            Some(ProviderEvent::ToolCallDelta {
                id,
                name_delta: None,
                arguments_delta: Some(delta),
            })
        }
        ResponseStreamEvent::FunctionCallArgumentsDone {
            item_id,
            name,
            output_index,
            arguments,
            ..
        } => {
            state.item_id_to_index.insert(item_id, output_index);
            let entry = tool_call_entry(state, output_index);
            // The `done` event carries the full JSON — authoritative.
            entry.args_json = arguments;
            if let Some(n) = name {
                entry.name = n;
            }
            None
        }
        ResponseStreamEvent::OutputItemDone {
            output_index, item, ..
        } => {
            match item {
                OutputItem::FunctionCall {
                    id,
                    call_id,
                    name,
                    arguments,
                    ..
                } => {
                    // Final `arguments` (if present) override accumulation.
                    seed_function_call(
                        state,
                        output_index,
                        Some(id),
                        call_id,
                        name,
                        arguments,
                        true,
                    );
                }
                // The finalized reasoning item carries the full
                // encrypted_content (the `.added` event does too, but `.done`
                // supersedes). This is the codex path's reasoning source.
                OutputItem::Reasoning {
                    id,
                    summary,
                    encrypted_content,
                    content,
                    ..
                } => {
                    capture_streamed_reasoning(
                        state,
                        output_index,
                        id,
                        encrypted_content,
                        summary,
                        content,
                    );
                }
                _ => {}
            }
            None
        }
        ResponseStreamEvent::ReasoningSummaryTextDelta { delta, .. }
        | ResponseStreamEvent::ReasoningTextDelta { delta, .. } => {
            if delta.is_empty() {
                None
            } else {
                Some(ProviderEvent::ThinkingDelta(delta))
            }
        }
        ResponseStreamEvent::Completed { response, .. }
        | ResponseStreamEvent::Incomplete { response, .. } => {
            apply_terminal_response(state, &response);
            state.finished = true;
            state.emitted_done = true;
            let stop = map_stop(state);
            Some(done_event(state, stop))
        }
        ResponseStreamEvent::Failed { response, .. } => {
            let err_msg = response
                .error
                .as_ref()
                .and_then(|e| e.message.clone())
                .unwrap_or_else(|| "openai response failed".into());
            state.error_message = Some(err_msg.clone());
            state.finished = true;
            state.emitted_done = true;
            Some(ProviderEvent::Error(err_msg))
        }
        ResponseStreamEvent::ResponseError { message, .. } => {
            state.error_message = Some(message.clone());
            state.finished = true;
            state.emitted_done = true;
            Some(ProviderEvent::Error(message))
        }
        ResponseStreamEvent::Error { message, code, .. } => {
            let msg = message
                .or(code)
                .unwrap_or_else(|| "openai stream error".into());
            state.error_message = Some(msg.clone());
            state.finished = true;
            state.emitted_done = true;
            Some(ProviderEvent::Error(msg))
        }
        // Lifecycle pre-terminal snapshots, content_part events,
        // reasoning summary-part / done, refusal, unknown — noise here.
        _ => None,
    }
}

// ============================================================================
// Provider impl
// ============================================================================

#[async_trait]
impl Provider for OpenaiProvider {
    async fn stream(
        &self,
        system_prompt: Option<&str>,
        effort: Option<&str>,
        input: MessageInput<'_>,
        tools: &[ToolSpec],
        cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        // mu-vcbm: a per-turn `/effort` selection maps onto the
        // Responses API's `reasoning.effort`, overriding the
        // construction-time `--thinking` default for THIS call. The
        // accepted vocabulary is MODEL-dependent; an out-of-vocabulary
        // level surfaces as a provider 400.
        let eff_thinking: &str = effort.unwrap_or(&self.thinking);
        let body = match input {
            MessageInput::Legacy(msgs) => {
                // mu-n48: a session-level system_prompt overrides the
                // provider's default `instructions`.
                let instructions: &str = system_prompt
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&self.instructions);
                build_request_value(&self.model, eff_thinking, instructions, msgs, tools)
            }
            MessageInput::Projected(pmsgs) => {
                // The projection normally carries the session system prompt,
                // which the helper hoists into `body.instructions`. When the
                // projection has no (or an empty) system span, fall back to a
                // non-empty per-call `system_prompt` — consistent with the
                // Legacy arm (mu-n48) — and only then to the provider default,
                // so a passed `system_prompt` is never silently dropped. The
                // projection's own span still wins when present, so this never
                // double-applies the system prompt.
                let fallback: &str = system_prompt
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&self.instructions);
                build_request_value_from_projection(
                    &self.model,
                    eff_thinking,
                    fallback,
                    pmsgs,
                    tools,
                )
            }
            _ => {
                return Err(ProviderError::Other(
                    "OpenaiProvider: unrecognized MessageInput variant".to_string(),
                ));
            }
        };

        debug!(
            target: "mu_ai::providers::openai",
            "openai request body: {}",
            serde_json::to_string_pretty(&body)
                .unwrap_or_else(|e| format!("(serialize failed: {e})"))
        );

        let resp = self.send(&body).await?;
        let bytes = resp.bytes_stream();
        Ok(events_stream(bytes, cancel_rx))
    }

    /// Identify as `"openai_codex"` in BOTH modes so ContextAssembly
    /// events and downstream diagnostics (mu-solo's renderer-mismatch
    /// warning, capability lookups, route_catalog effort vocab keyed on
    /// `"openai_codex"`) see a known label rather than the default
    /// `"faux"`. The public (API-key) mode shares the wire shape and the
    /// same downstream expectations, so it uses the same label.
    /// (Provider-kind on the wire is carried separately by the
    /// `ProviderSelector` → `provider_kind` mapping in handlers.)
    fn provider_label(&self) -> &'static str {
        "openai_codex"
    }

    /// What we know empirically about the Responses API:
    /// - Dedicated `instructions` top-level field with a ~8KB soft cap
    ///   (see [`INSTRUCTIONS_SOFT_CAP`]).
    /// - No published prompt-caching surface.
    /// - The `developer` role exists distinct from system/user/assistant.
    /// - Context window varies by model — left as None.
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
            usage_semantics: UsageSemantics::openai_style(),
            // Hosted API: rejects over-window requests itself; no silent truncation.
            truncates_over_window_prompts: false,
        }
    }
}

impl OpenaiProvider {
    /// Send the request, dispatching on auth mode. Codex refreshes once
    /// on 401 and retries; the public path has no refresh. Non-2xx
    /// becomes a `ProviderError` via [`render_codex_http_error`].
    async fn send(&self, body: &Value) -> Result<reqwest::Response, ProviderError> {
        match &self.mode {
            AuthMode::Codex { token, store } => {
                let initial_token = token.lock().await.clone();
                let resp = self
                    .send_codex(&initial_token, body)
                    .await
                    .map_err(|e| ProviderError::Other(format!("openai request: {e}")))?;

                let resp = if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
                    let refreshed =
                        refresh_token_if_unchanged(token, store, &initial_token).await?;
                    self.send_codex(&refreshed, body)
                        .await
                        .map_err(|e| ProviderError::Other(format!("openai retry: {e}")))?
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
                check_status(resp).await
            }
            AuthMode::Public { api_key } => {
                let resp = self
                    .http
                    .post(&self.endpoint)
                    .header("Authorization", format!("Bearer {api_key}"))
                    .header("Content-Type", "application/json")
                    .header("Accept", "text/event-stream")
                    .json(body)
                    .send()
                    .await
                    .map_err(|e| ProviderError::Other(format!("openai request: {e}")))?;
                check_status(resp).await
            }
        }
    }

    /// Codex-mode POST: Bearer + `chatgpt-account-id` (from the JWT) +
    /// `originator` headers.
    async fn send_codex(
        &self,
        token: &OAuthToken,
        body: &Value,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let account_id = match extract_chatgpt_account_id(&token.access_token) {
            Ok(id) => id,
            Err(e) => {
                // Send with an empty header; the backend will 401 and the
                // caller produces a meaningful error after refresh fails.
                tracing::error!(error = %e, "could not extract chatgpt_account_id");
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
}

/// Map a non-success response to a `ProviderError`; pass success through.
async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response, ProviderError> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    Err(ProviderError::Other(render_codex_http_error(status, &text)))
}

/// If the in-memory token still matches `seen` (we're first to notice
/// 401), refresh and persist. Otherwise another caller already
/// refreshed; return the current bundle.
async fn refresh_token_if_unchanged(
    token: &Mutex<OAuthToken>,
    store: &Option<Arc<dyn TokenStore>>,
    seen: &OAuthToken,
) -> Result<OAuthToken, ProviderError> {
    let current = token.lock().await;
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
    // Release the lock across the await — refresh is HTTP, can be slow.
    drop(current);
    let new_bundle = auth::openai_codex::refresh_access_token(&refresh_token)
        .await
        .map_err(map_auth_err)?;
    let mut current = token.lock().await;
    // If someone else won the race, take theirs.
    if current.access_token != seen.access_token {
        return Ok(current.clone());
    }
    *current = new_bundle.clone();
    drop(current);
    if let Some(store) = store {
        if let Err(e) = store.save(CODEX_PROVIDER_KEY, &new_bundle) {
            tracing::warn!(error = %e, "failed to persist refreshed token; continuing in-memory");
        }
    }
    Ok(new_bundle)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
#[path = "openai_tests.rs"]
mod tests;
