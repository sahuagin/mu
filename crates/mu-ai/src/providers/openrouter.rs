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
use serde_json::{json, Value};
use tokio::sync::oneshot;

use mu_core::agent::{
    AgentMessage, AssistantMessage, ContentBlock, MessageInput, Provider, ProviderError,
    ProviderEvent, StopReason, ToolCall, ToolSpec, Usage,
};
use mu_core::context::{
    extract_call_id_from_span_id, ProviderMessage, ProviderMessages, ProviderRole,
};

use super::sse::{ByteSse, SseStream};

const OPENROUTER_API_BASE: &str = "https://openrouter.ai";
/// Default chat-completions path. OpenRouter nests under `/api/v1`; the
/// OpenAI-compatible endpoints exposed by ollama / LM Studio / vLLM serve
/// `/v1/chat/completions` directly, so the path is overridable (mu-spawn).
const OPENROUTER_API_PATH: &str = "/api/v1/chat/completions";

pub struct OpenRouterProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    api_base: String,
    api_path: String,
    /// Provider label for diagnostics/event provenance. Defaults to
    /// `"openrouter"`; a config-defined openai-chat endpoint (mu-v8ye) reuses
    /// this provider but overrides the label with its configured name so the
    /// trait-path label matches the event-path label (`with_label`).
    label: &'static str,
    /// mu-6fj1b: construction-time default reasoning effort (the daemon's
    /// `--thinking` flag). A per-turn `effort` on `stream` wins; this fills
    /// in when the turn carries none. `None` = no default (server decides).
    default_effort: Option<String>,
}

impl OpenRouterProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model,
            api_base: OPENROUTER_API_BASE.to_string(),
            api_path: OPENROUTER_API_PATH.to_string(),
            label: "openrouter",
            default_effort: None,
        }
    }

    /// Construct from env, supporting OpenAI-compatible local backends.
    ///
    /// `OPENROUTER_API_BASE` overrides the base URL (default
    /// `https://openrouter.ai`) and `OPENROUTER_API_PATH` overrides the
    /// chat-completions path (default `/api/v1/chat/completions`). Pointing
    /// at a local ollama box means
    /// `OPENROUTER_API_BASE=http://10.1.1.143:11434` +
    /// `OPENROUTER_API_PATH=/v1/chat/completions`.
    ///
    /// `OPENROUTER_API_KEY` is required only when the base is the real
    /// OpenRouter endpoint; local backends (ollama) ignore the key, so an
    /// overridden base relaxes the requirement — mirroring AnthropicProvider's
    /// `ANTHROPIC_BASE_URL` handling.
    pub fn from_env(model: String) -> Result<Self, ProviderError> {
        let api_base = std::env::var("OPENROUTER_API_BASE")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| OPENROUTER_API_BASE.to_string());
        let api_path = std::env::var("OPENROUTER_API_PATH")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| OPENROUTER_API_PATH.to_string());
        let api_key = std::env::var("OPENROUTER_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_default();
        if api_key.is_empty() && is_hosted_openrouter(&api_base) {
            return Err(ProviderError::Other(
                "OPENROUTER_API_KEY not set or empty (required when OPENROUTER_API_BASE points at openrouter.ai)".into(),
            ));
        }
        Ok(Self {
            client: reqwest::Client::new(),
            api_key,
            model,
            api_base,
            api_path,
            label: "openrouter",
            default_effort: None,
        })
    }

    /// Test hook: override the API base URL.
    pub fn with_api_base(mut self, base: String) -> Self {
        self.api_base = base;
        self
    }

    /// Override the chat-completions path (default `/api/v1/chat/completions`).
    pub fn with_api_path(mut self, path: String) -> Self {
        self.api_path = path;
        self
    }

    /// mu-6fj1b: set the default reasoning effort from a raw `--thinking`
    /// flag value. Stored verbatim; the wire mappers normalize it (and treat
    /// unrecognized values as absent), so the accepted vocabulary lives in
    /// exactly one place per dialect.
    pub fn with_thinking_flag(mut self, flag: &str) -> Self {
        let f = flag.trim();
        self.default_effort = (!f.is_empty()).then(|| f.to_string());
        self
    }

    /// Override the provider label (mu-v8ye). A config-defined openai-chat
    /// endpoint reuses this provider but should report its configured name
    /// (e.g. `"card1"`) rather than the default `"openrouter"` so the
    /// trait-path label agrees with the event-path label. The name is leaked
    /// to `&'static str` — providers are long-lived (one per session) and few,
    /// so the bounded, one-time leak is acceptable and keeps the trait's
    /// `-> &'static str` contract.
    pub fn with_label(mut self, label: &str) -> Self {
        self.label = Box::leak(label.to_string().into_boxed_str());
        self
    }
}

#[async_trait]
impl Provider for OpenRouterProvider {
    async fn stream(
        &self,
        system_prompt: Option<&str>,
        // mu-13ve: per-turn reasoning effort, mapped to OpenRouter's
        // normalized `reasoning` field below (mu-vcbm threaded it; the
        // OpenRouter arm previously accepted-and-ignored it).
        effort: Option<&str>,
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
        let mut body = match input {
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
        // mu-13ve: thread the per-turn reasoning effort into OpenRouter's
        // normalized `reasoning` field. None / "off" / unrecognized → no
        // key, so the pre-mu-13ve request body stays byte-for-byte intact.
        // mu-6fj1b: the per-turn selection falls back to the daemon's
        // `--thinking` default (previously dropped on this wire).
        let effort = effort.or(self.default_effort.as_deref());
        if let Some(reasoning) = reasoning_param(effort) {
            body["reasoning"] = reasoning;
        }
        // mu-6fj1b: the openai-chat wire fronts more than OpenRouter — the
        // same provider drives local ollama and vLLM serves, and neither
        // reads OpenRouter's `reasoning` object. Emit both local dialects
        // whenever an effort selection exists (including "off", which MUST
        // reach the serve: ollama 0.32.5 disables thinking only via
        // `"reasoning_effort":"none"` — `"think":false` is ignored there,
        // and omitting the field leaves the server default):
        //   - `reasoning_effort` — ollama's /v1/chat/completions ladder
        //     (none/low/medium/high, wire-verified on 0.32.5); also a valid
        //     OpenAI chat-completions param, and vLLM ignores it.
        //   - `chat_template_kwargs.enable_thinking` — vLLM + qwen3-family
        //     chat templates toggle thinking per request (wire-verified
        //     against the :11435 lane, 2026-09-01); other serves ignore it.
        // No selection at all still sends neither key (server default).
        // Gated OFF for hosted openrouter.ai (panel finding, conceded):
        // OpenRouter FORWARDS unknown params to the backing provider, and
        // "none" is not a valid OpenAI reasoning_effort value — a strict
        // backend could reject the request. Hosted OpenRouter keeps only
        // the normalized `reasoning` object above; every other base (local
        // ollama/vLLM serves, lab proxies) gets the local dialects.
        if emits_local_dialects(&self.api_base) {
            if let Some((ladder, enable)) = local_dialect_params(effort) {
                body["reasoning_effort"] = json!(ladder);
                body["chat_template_kwargs"] = json!({ "enable_thinking": enable });
            }
        }
        let resp = self
            .client
            .post(format!("{}{}", self.api_base, self.api_path))
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
        // mu-xblz: recover training-native tool-call dialect that GLM/Qwen-class
        // models (served via OpenRouter / vLLM) sometimes leak as assistant text
        // instead of structured `tool_calls`. No-op for well-behaved models.
        Ok(apply_dialect_rescue(events_stream(bytes, cancel_rx), tools))
    }

    /// Identify as `"openrouter"` (or the configured name for a mu-v8ye
    /// config-defined openai-chat endpoint) so ContextAssembly events and
    /// downstream diagnostics don't see the default `"faux"` label.
    /// Matches the snake_case wire `provider_kind` enum.
    fn provider_label(&self) -> &'static str {
        self.label
    }

    /// OpenRouter proxies to many backing models with OpenAI-style
    /// chat completions:
    /// - System content is inlined as a `{role: "system", ...}`
    ///   message, not a separate top-level field. Limits inherit
    ///   from the backing model's per-message budget rather than a
    ///   separate cap on a system slot.
    /// - Caching support varies by backing model; conservative
    ///   default is false until per-model overrides exist.
    /// - No `developer` role in the chat-completions shape.
    fn capabilities(&self) -> mu_core::agent::capabilities::ProviderCapabilities {
        use mu_core::agent::capabilities::{
            ProviderCapabilities, SystemPromptCapability, UsageSemantics,
        };
        ProviderCapabilities {
            system_prompt: SystemPromptCapability::MessageRole,
            supports_prompt_caching: false,
            supports_developer_role: false,
            max_tools: None,
            context_window_tokens: None,
            // OpenAI chat-completions accounting: prompt_tokens is
            // the total (prompt_tokens_details.cached_tokens subset).
            // Ollama inherits this via its inner provider.
            usage_semantics: UsageSemantics::openai_style(),
            // Hosted API: rejects over-window requests itself; no silent truncation.
            truncates_over_window_prompts: false,
        }
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

/// Map mu's per-turn reasoning-effort selection to OpenRouter's
/// normalized `reasoning` request field (mu-13ve).
///
/// OpenRouter accepts `reasoning.effort` ∈ {`low`, `medium`, `high`}
/// and maps the requested level to the nearest one a given backing
/// model supports, so one knob works across providers (OpenRouter
/// "Reasoning Tokens" docs). mu's effort vocabulary
/// (`low|medium|high|xhigh|max`, via `parse_thinking_flag`) extends
/// above `high`; `xhigh`/`max` clamp to `high` (OpenRouter's ceiling).
///
/// Returns `None` for `None`, `off`/`""`, or any unrecognized value —
/// the caller then sends no `reasoning` key, leaving the request body
/// byte-for-byte as it was before mu-13ve.
fn reasoning_param(effort: Option<&str>) -> Option<Value> {
    let level = match effort?.trim().to_ascii_lowercase().as_str() {
        "low" => "low",
        "medium" => "medium",
        "high" | "xhigh" | "max" => "high",
        _ => return None,
    };
    Some(json!({ "effort": level }))
}

/// Whether a base URL is the hosted openrouter.ai endpoint, tolerant of the
/// operator-input variants raw string equality misses (panel finding on
/// mu-6fj1b): surrounding whitespace and trailing slashes, either of which
/// previously defeated BOTH the API-key requirement guard and the
/// local-dialect gate below.
fn is_hosted_openrouter(api_base: &str) -> bool {
    api_base.trim().trim_end_matches('/') == OPENROUTER_API_BASE
}

/// Whether this base URL gets the local openai-chat dialect fields
/// (mu-6fj1b). Hosted openrouter.ai does not: it forwards unknown params to
/// backing providers, where `reasoning_effort:"none"` is invalid OpenAI
/// vocabulary. Everything else — local serves, configured endpoints, lab
/// proxies — does.
fn emits_local_dialects(api_base: &str) -> bool {
    !is_hosted_openrouter(api_base)
}

/// Map mu's effort selection to the LOCAL openai-chat dialects (mu-6fj1b):
/// ollama's `reasoning_effort` ladder plus vLLM's
/// `chat_template_kwargs.enable_thinking` toggle. Unlike
/// [`reasoning_param`], the explicit OFF forms map to `("none", false)`
/// rather than to no key — disabling must reach the serve. `None`/empty/
/// unrecognized values return `None` (no keys, server default).
fn local_dialect_params(effort: Option<&str>) -> Option<(&'static str, bool)> {
    match effort?.trim().to_ascii_lowercase().as_str() {
        "off" | "none" | "false" | "0" | "disabled" => Some(("none", false)),
        "minimal" | "low" => Some(("low", true)),
        "medium" | "med" => Some(("medium", true)),
        "high" | "xhigh" | "max" => Some(("high", true)),
        _ => None,
    }
}

/// Wrap a provider event stream so a terminal [`ProviderEvent::Done`] whose
/// assistant message is actually a leaked training-native tool-call dialect
/// (XML `<function=…>` or `<tool_call>{…}` text instead of structured
/// `tool_calls`) is rewritten into real tool calls (mu-xblz).
///
/// GLM/Qwen-class models served over the OpenAI chat-completions wire
/// (OpenRouter, and vLLM via composition) do this nondeterministically; unlike
/// ollama's Anthropic wire — where `ollama.rs` already applies this same
/// rescue — that wire has no other recovery path, so a leaked call ends the
/// agent loop as if the turn were prose.
///
/// Conservative-by-construction: [`super::tool_dialect::rescue_assistant_message`]
/// is a no-op unless the turn EndTurns with no structured calls AND the text
/// parses cleanly as a call against a KNOWN tool, so well-behaved models pass
/// through untouched.
fn apply_dialect_rescue(
    stream: BoxStream<'static, ProviderEvent>,
    tools: &[ToolSpec],
) -> BoxStream<'static, ProviderEvent> {
    let specs: Vec<ToolSpec> = tools.to_vec();
    stream
        .map(move |ev| match ev {
            ProviderEvent::Done(msg) => {
                ProviderEvent::Done(super::tool_dialect::rescue_assistant_message(msg, &specs))
            }
            other => other,
        })
        .boxed()
}

/// Per-model sampling resolved from the model catalog (mu-y8gp), mirroring
/// [`super::output_limits::max_tokens_for_model`]'s catalog lookup. Returns
/// the full [`Sampling`] set; each field is `None` when the catalog declares
/// none, in which case the caller sends no such field and the request body
/// is byte-for-byte unchanged.
fn sampling_for_model(model: &str) -> Sampling {
    sampling_for_model_with_catalog(mu_core::model_catalog::global(), model)
}

/// mu-4sivd: the catalog's full per-model sampling set. Grown from the
/// original (temperature, top_p) pair so presence_penalty — the
/// anti-repetition knob — no longer rides solely on a serve-side
/// launcher flag.
#[derive(Debug, Default, PartialEq)]
struct Sampling {
    temperature: Option<f64>,
    top_p: Option<f64>,
    presence_penalty: Option<f64>,
    top_k: Option<u32>,
}

/// [`sampling_for_model`] against an explicit catalog — the testable seam (a
/// test must not depend on the operator's `~/.config/mu/models.toml`; mirrors
/// `output_limits::max_tokens_for_model_with_catalog`, bead mu-nzxa).
fn sampling_for_model_with_catalog(
    catalog: &mu_core::model_catalog::ModelCatalogConfig,
    model: &str,
) -> Sampling {
    let r = catalog.resolve_model(model);
    Sampling {
        temperature: r.temperature,
        top_p: r.top_p,
        presence_penalty: r.presence_penalty,
        top_k: r.top_k,
    }
}

/// Inject per-model sampling (mu-y8gp, mu-4sivd) into an OpenRouter request
/// body. No-op when the catalog declares no sampling fields for `model`, so
/// the body is byte-for-byte unchanged — preserving the yqeq6
/// Legacy/Projected parity.
fn apply_sampling(body: &mut Value, model: &str) {
    let s = sampling_for_model(model);
    // mu-y8gp: clamp the f64 knobs to provider-valid ranges and drop
    // non-finite, so an operator catalog typo (temperature = 5.0, a NaN, …)
    // can't ship an invalid value — temperature ∈ [0, 2], top_p ∈ [0, 1],
    // presence_penalty ∈ [-2, 2] (mu-4sivd). top_k is only floored (dropped
    // when 0): it has no principled upper bound here — providers clamp it to
    // their vocab size, and inventing a ceiling would silently rewrite a
    // deliberate operator value.
    inject_sampling(
        body,
        Sampling {
            temperature: clamp_sampling(s.temperature, 0.0, 2.0),
            top_p: clamp_sampling(s.top_p, 0.0, 1.0),
            presence_penalty: clamp_sampling(s.presence_penalty, -2.0, 2.0),
            top_k: s.top_k.filter(|k| *k > 0),
        },
    );
}

/// Clamp a catalog sampling value into `[lo, hi]` and drop non-finite
/// (NaN / ±Inf) → `None`, keeping the wire to valid numbers regardless of
/// operator config (mu-y8gp).
fn clamp_sampling(v: Option<f64>, lo: f64, hi: f64) -> Option<f64> {
    v.filter(|x| x.is_finite()).map(|x| x.clamp(lo, hi))
}

/// Per-model system-prompt addendum resolved from the model catalog (mu-g1f2),
/// mirroring [`sampling_for_model`]'s catalog lookup. Empty → `None`.
fn system_prompt_addendum_for_model(model: &str) -> Option<String> {
    addendum_for_model_with_catalog(mu_core::model_catalog::global(), model)
}

/// [`system_prompt_addendum_for_model`] against an explicit catalog — the
/// testable seam (a test must not depend on the operator's `models.toml`).
fn addendum_for_model_with_catalog(
    catalog: &mu_core::model_catalog::ModelCatalogConfig,
    model: &str,
) -> Option<String> {
    catalog
        .resolve_model(model)
        .system_prompt_addendum
        .filter(|s| !s.is_empty())
}

/// Compose the effective OpenRouter system-message content for `model` (mu-g1f2):
/// the base system content (when non-empty) with the per-model addendum appended.
/// `None` when there is neither — so no system message is emitted and the
/// pre-mu-g1f2 request body is byte-for-byte unchanged (yqeq6 parity).
fn system_with_addendum(base: Option<&str>, model: &str) -> Option<String> {
    combine_system(base, system_prompt_addendum_for_model(model))
}

/// Pure combine step of [`system_with_addendum`] — testable without the global
/// catalog. Base (when non-empty) joined to the addendum by a blank line;
/// neither present → `None`.
fn combine_system(base: Option<&str>, addendum: Option<String>) -> Option<String> {
    let base = base.filter(|s| !s.is_empty());
    match (base, addendum.filter(|s| !s.is_empty())) {
        (None, None) => None,
        (Some(b), None) => Some(b.to_string()),
        (None, Some(a)) => Some(a),
        (Some(b), Some(a)) => Some(format!("{b}\n\n{a}")),
    }
}

/// Pure injection step of [`apply_sampling`] — testable without the global
/// catalog. Adds each sampling field only when `Some`, so all-`None` leaves
/// the body byte-for-byte unchanged.
fn inject_sampling(body: &mut Value, s: Sampling) {
    if let Some(t) = s.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(p) = s.top_p {
        body["top_p"] = json!(p);
    }
    if let Some(pp) = s.presence_penalty {
        body["presence_penalty"] = json!(pp);
    }
    if let Some(k) = s.top_k {
        body["top_k"] = json!(k);
    }
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
    // mu-g1f2: base system content + per-model addendum (None → no system message).
    if let Some(system) = system_with_addendum(system_prompt, model) {
        api_messages.push(json!({ "role": "system", "content": system }));
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
    // mu-y8gp: per-model sampling from the catalog (no-op when unset).
    apply_sampling(&mut body, model);
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
fn translate_provider_messages_openrouter(pmsgs: &ProviderMessages, model: &str) -> Vec<Value> {
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
    // mu-g1f2: append the per-model system-prompt addendum to the accumulated
    // system content (parity with the Legacy path's system_with_addendum).
    if let Some(content) = system_with_addendum(system_content.as_deref(), model) {
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
    let api_messages = translate_provider_messages_openrouter(pmsgs, model);
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
    // mu-y8gp: per-model sampling from the catalog (no-op when unset).
    apply_sampling(&mut body, model);
    body
}

// ============================================================================
// Response side: SSE → ProviderEvent
// ============================================================================

// Chat-completions streaming chunk types come from the standalone
// `mu-openai-chat` wire crate (mu-14a4) rather than being redefined here: the
// crate's `ChatCompletionChunk`/`ChatChoice`/`ChatDelta`/`ToolCallDelta`/
// `FunctionDelta`/`Usage` are byte-matched to this same wire (promoted from
// these exact structs) and unit-tested there. This makes the crate a real
// consumer (was dead code) and removes the duplicate definitions. The
// mu↔wire translation (below) stays here — the consumer owns translation.
use mu_openai_chat::{ChatCompletionChunk, Usage as WireUsage};

/// Convert the wire crate's `Usage` into mu's accounting `Usage`. (Was
/// `OpenAiUsage::to_usage`; the crate type is pure wire data, so the mu-side
/// mapping lives here.) `prompt_tokens` is the TOTAL prompt; `cached_tokens`
/// is a subset reported as a cache read. Anthropic-style cache-creation
/// fields have no chat-completions equivalent.
fn wire_usage_to_mu(u: &WireUsage) -> Usage {
    Usage {
        input_tokens: u.prompt_tokens.unwrap_or(0),
        output_tokens: u.completion_tokens.unwrap_or(0),
        cache_read_input_tokens: u
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens),
        cache_creation_input_tokens: None,
        cache_creation_5m_input_tokens: None,
        cache_creation_1h_input_tokens: None,
        reasoning_tokens: u
            .completion_tokens_details
            .as_ref()
            .and_then(|d| d.reasoning_tokens),
    }
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
    sse: ByteSse,
    accumulated_text: String,
    accumulated_thinking: String,
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
    let bytes: Pin<Box<dyn Stream<Item = Result<Bytes, String>> + Send>> =
        Box::pin(bytes.map(|r| r.map_err(|e| e.to_string())));
    let sse = SseStream::new(bytes);
    let state = StreamState {
        sse,
        accumulated_text: String::new(),
        accumulated_thinking: String::new(),
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
                    // A dropped connection is not a clean close: surface it
                    // as an error instead of a complete-looking truncated
                    // turn (bead mu-openai-stream-retry-y0dw).
                    if let Some(e) = state.sse.take_transport_error() {
                        return Some((
                            ProviderEvent::Error(format!("openrouter stream transport error: {e}")),
                            state,
                        ));
                    }
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

        let chunk: ChatCompletionChunk = match serde_json::from_str(&sse_event.data) {
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
            state.usage = Some(wire_usage_to_mu(u));
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
            // Reasoning delta? (thinking models via ollama OpenAI-compat —
            // mu-mdds). Accumulate always so the final message carries it;
            // stream a ThinkingDelta when the single per-chunk event slot is
            // free. Reasoning and content almost always arrive in separate
            // chunks, so first-come on the slot is fine in practice.
            if let Some(reasoning) = choice.delta.reasoning {
                if !reasoning.is_empty() {
                    state.accumulated_thinking.push_str(&reasoning);
                    if emitted_event.is_none() {
                        emitted_event = Some(ProviderEvent::ThinkingDelta(reasoning));
                    }
                }
            }
            // Tool call delta(s)? Accumulate for Done-side assembly AND
            // stream a ToolCallDelta so the loop's stall watchdog counts
            // the bytes (mu-b82rr): a large file written via one tool call
            // streams arguments for minutes with no text/reasoning deltas
            // at all, and an unemitted fragment is invisible progress the
            // watchdog misreads as a dead connection.
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
                    let mut name_delta = None;
                    let mut arguments_delta = None;
                    if let Some(func) = tc_delta.function {
                        if let Some(name) = func.name {
                            name_delta = Some(name.clone());
                            entry.name = name;
                        }
                        if let Some(args) = func.arguments {
                            arguments_delta = Some(args.clone());
                            entry.args_json.push_str(&args);
                        }
                    }
                    if emitted_event.is_none()
                        && (name_delta.is_some() || arguments_delta.is_some())
                    {
                        // Continuation fragments may carry no id; the loop
                        // uses it only for status display.
                        emitted_event = Some(ProviderEvent::ToolCallDelta {
                            id: entry.id.clone(),
                            name_delta,
                            arguments_delta,
                        });
                    }
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
        // No emittable event for this chunk (e.g. it carried only a
        // finish_reason or usage). Loop and pull the next SSE event.
    }
}

fn assemble_content(state: &StreamState) -> Vec<ContentBlock> {
    let mut out: Vec<ContentBlock> = Vec::new();
    // Reasoning precedes the answer; emit the Thinking block first so the
    // persisted message preserves order (mu-mdds).
    if !state.accumulated_thinking.is_empty() {
        out.push(ContentBlock::Thinking {
            text: state.accumulated_thinking.as_str().into(),
            // OpenRouter has no opaque reasoning round-trip token; leave
            // it None (and the v1 outbound path drops Thinking anyway).
            opaque: None,
        });
    }
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
#[path = "openrouter_tests.rs"]
mod tests;
