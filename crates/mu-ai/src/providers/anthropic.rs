//! Anthropic API Provider — direct API access via `ANTHROPIC_API_KEY`.
//!
//! Streams responses from `/v1/messages` with `stream: true`, parses
//! the SSE event format, translates to mu-core's `ProviderEvent`.
//!
//! See spec mu-006. Supports streamed text, tool calls (streamed deltas +
//! finalized), and extended thinking — inbound display plus the outbound
//! `thinking` request directive (mu-upk2). Image content is still deferred.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{BoxStream, Stream, StreamExt};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use mu_anthropic::{
    BlockDelta, BlockStart, CacheControl, Content, ContentBlock as AnthBlock,
    Message as AnthMessage, MessagesRequest, StopReason as AnthropicStopReason, StreamEvent,
    ThinkingConfig, Tool, ToolDef, Usage as AnthropicUsage,
};
use mu_core::agent::{
    AgentMessage, AssistantMessage, ContentBlock, MessageInput, Provider, ProviderError,
    ProviderEvent, StopReason, ToolCall, ToolSpec, Usage,
};
use mu_core::context::{
    extract_call_id_from_span_id, CacheMarker, CacheStrategy, CacheTtl, ProviderMessage,
    ProviderMessages, ProviderRenderer, ProviderRole,
};

use crate::context::{AnthropicCacheStrategy, AnthropicProviderRenderer};

use super::sse::{SseEvent, SseStream};

const ANTHROPIC_API_BASE: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// mu-upk2: when an explicit thinking budget is set, raise `max_tokens` to the
/// budget plus this answer headroom — the model must have room to think AND
/// answer (Anthropic counts thinking against output and requires
/// `max_tokens > budget_tokens`).
const THINKING_ANSWER_HEADROOM: u32 = 4096;

/// Direct API Provider. Holds an API key (ENV-sourced is fine — this
/// isn't an OAuth token).
pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    api_base: String,
    /// mu-f1a0: cache TTL tier applied to every `cache_control`
    /// emission. FiveMinutes = bare ephemeral (wire-identical to
    /// pre-f1a0); OneHour adds `"ttl": "1h"` (2.0x write billing,
    /// survives human thinking gaps).
    cache_ttl: CacheTtl,
    /// mu-upk2: extended-thinking directive sent on every request. `None`
    /// leaves the wire body unchanged (no `thinking` field). Set via
    /// [`with_thinking`](Self::with_thinking) from the `--thinking` flag.
    thinking: Option<ThinkingConfig>,
}

impl AnthropicProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model,
            api_base: ANTHROPIC_API_BASE.to_string(),
            cache_ttl: CacheTtl::default(),
            thinking: None,
        }
    }

    /// mu-f1a0: select the prompt-cache TTL tier for this provider's
    /// requests. Builder-style, applied at session construction.
    pub fn with_cache_ttl(mut self, ttl: CacheTtl) -> Self {
        self.cache_ttl = ttl;
        self
    }

    /// mu-upk2: enable extended thinking on this provider's requests. The
    /// directive is injected into the wire body by [`apply_thinking`]; for an
    /// explicit token budget, `max_tokens` is raised so it exceeds the budget
    /// and still leaves room for the answer (Anthropic requires
    /// `max_tokens > budget_tokens` and bills thinking against output).
    pub fn with_thinking(mut self, thinking: ThinkingConfig) -> Self {
        self.thinking = Some(thinking);
        self
    }

    /// mu-upk2: set thinking from a raw `--thinking` flag value (the provider
    /// owns the Anthropic-specific interpretation — see [`parse_thinking_flag`]).
    /// An empty/whitespace flag leaves thinking off.
    pub fn with_thinking_flag(mut self, flag: &str) -> Self {
        if let Some(cfg) = parse_thinking_flag(flag) {
            self.thinking = Some(cfg);
        }
        self
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
            cache_ttl: CacheTtl::default(),
            thinking: None,
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
                build_request_body_from_projection(&self.model, pmsgs, tools, self.cache_ttl)
            }
            _ => {
                return Err(ProviderError::Other(
                    "AnthropicProvider: unrecognized MessageInput variant".to_string(),
                ));
            }
        };

        // mu-upk2: layer the extended-thinking directive onto the built wire
        // body. Done here (not in build_request_body*) so the byte-parity test
        // suite keeps asserting the no-thinking shape; thinking is additive.
        let mut body = body;
        apply_thinking(&mut body, self.thinking.as_ref());

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

    /// Anthropic's Messages API:
    /// - Dedicated top-level `system` field, no observed size cap
    ///   (handles arbitrary length; cache_control is the practical
    ///   tuning knob).
    /// - First-class prompt caching via `cache_control` markers on
    ///   system content blocks and message content.
    /// - No `developer` role; uses standard system/user/assistant.
    fn capabilities(&self) -> mu_core::agent::capabilities::ProviderCapabilities {
        use mu_core::agent::capabilities::{
            ProviderCapabilities, SystemPromptCapability, UsageSemantics,
        };
        ProviderCapabilities {
            system_prompt: SystemPromptCapability::TopLevelField { max_bytes: None },
            supports_prompt_caching: true,
            supports_developer_role: false,
            max_tools: None,
            context_window_tokens: None,
            // Messages API: input/cache_read/cache_creation are
            // disjoint buckets; thinking bills inside output_tokens.
            usage_semantics: UsageSemantics::anthropic_style(),
        }
    }
}

// ============================================================================
// mu -> mu_anthropic request mapping (typed; request bytes come from the crate)
// ============================================================================

/// mu cache-TTL tier -> a mu_anthropic `CacheControl` directive.
fn cache_control(ttl: CacheTtl) -> CacheControl {
    match ttl {
        CacheTtl::FiveMinutes => CacheControl::ephemeral(),
        CacheTtl::OneHour => CacheControl::ephemeral_with_ttl("1h"),
    }
}

/// Wrap a `serde_json::Value` as a mu_anthropic `JsonValue`. The values we feed
/// (tool input schemas, already-validated tool arguments) are finite by
/// construction; the impossible non-finite case degrades to an empty object
/// rather than panicking.
fn json_value(v: Value) -> mu_anthropic::JsonValue {
    mu_anthropic::JsonValue::new(v).unwrap_or_else(|_| mu_anthropic::JsonValue::empty_object())
}

/// Translate a mu `ToolSpec` into a mu_anthropic `ToolDef`, optionally caching it.
fn map_tool_spec(spec: &ToolSpec, cache: Option<CacheControl>) -> ToolDef {
    let mut tool = Tool::new(
        spec.name.clone(),
        spec.description.clone(),
        json_value(spec.input_schema.clone()),
    );
    tool.cache_control = cache;
    tool.into()
}

/// Build the request `tools`, attaching `cache_control` to the LAST tool when
/// `cache_last` (mu's tool-cache position).
fn map_tools(tools: &[ToolSpec], cache_last: bool, ttl: CacheTtl) -> Vec<ToolDef> {
    let last = tools.len().saturating_sub(1);
    tools
        .iter()
        .enumerate()
        .map(|(i, spec)| map_tool_spec(spec, (cache_last && i == last).then(|| cache_control(ttl))))
        .collect()
}

/// The top-level `system` content: a single text block (the historical wire
/// shape), carrying `cache_control` when the system span is marked.
fn system_content(text: String, cache: bool, ttl: CacheTtl) -> Content {
    Content::Blocks(vec![AnthBlock::Text {
        text,
        citations: Vec::new(),
        cache_control: cache.then(|| cache_control(ttl)),
    }])
}

/// Attach `cache_control` to whichever block kind carries it (the last block of
/// a marked message).
fn set_block_cache_control(block: &mut AnthBlock, cc: CacheControl) {
    match block {
        AnthBlock::Text { cache_control, .. }
        | AnthBlock::ToolUse { cache_control, .. }
        | AnthBlock::ToolResult { cache_control, .. } => *cache_control = Some(cc),
        _ => {}
    }
}

/// Map an assistant's mu-core content blocks to mu_anthropic blocks. Text and
/// tool calls translate; `Thinking` is dropped outbound — mu never echoes the
/// model's own reasoning back as input (spec mu-044).
fn map_assistant_blocks(blocks: &[ContentBlock]) -> Vec<AnthBlock> {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(AnthBlock::Text {
                text: text.as_ref().to_string(),
                citations: Vec::new(),
                cache_control: None,
            }),
            ContentBlock::ToolCall(tc) => Some(AnthBlock::ToolUse {
                id: tc.id.clone(),
                name: tc.name.clone(),
                input: json_value(tc.arguments.as_value().clone()),
                cache_control: None,
            }),
            ContentBlock::Thinking { .. } => None,
        })
        .collect()
}

/// Serialize a built `MessagesRequest` to the wire `Value`. Infallible in
/// practice (the request holds only finite, serializable data); a serialization
/// failure degrades to an empty object and is logged rather than panicking.
fn request_to_value(req: MessagesRequest) -> Value {
    serde_json::to_value(&req).unwrap_or_else(|e| {
        tracing::error!(error = %e, "failed to serialize MessagesRequest");
        json!({})
    })
}

/// Translate Legacy `&[AgentMessage]` into mu_anthropic messages. Consecutive
/// tool results batch into one user message of `tool_result` blocks, as
/// Anthropic's tool-use protocol requires.
pub(crate) fn map_agent_messages(messages: &[AgentMessage]) -> Vec<AnthMessage> {
    let mut out = Vec::with_capacity(messages.len());
    let mut tool_result_buf: Vec<AnthBlock> = Vec::new();

    for message in messages {
        match message {
            AgentMessage::ToolResult {
                call_id,
                content,
                is_error,
            } => {
                tool_result_buf.push(AnthBlock::ToolResult {
                    tool_use_id: call_id.clone(),
                    content: content.clone(),
                    is_error: Some(*is_error),
                    cache_control: None,
                });
            }
            other => {
                if !tool_result_buf.is_empty() {
                    out.push(AnthMessage::user(std::mem::take(&mut tool_result_buf)));
                }
                if let Some(m) = map_agent_message_single(other) {
                    out.push(m);
                }
            }
        }
    }

    if !tool_result_buf.is_empty() {
        out.push(AnthMessage::user(tool_result_buf));
    }

    out
}

/// Single-message translation for non-ToolResult Legacy variants.
fn map_agent_message_single(m: &AgentMessage) -> Option<AnthMessage> {
    match m {
        AgentMessage::User { content } => Some(AnthMessage::user(content.as_str())),
        AgentMessage::Assistant(a) => {
            let blocks = map_assistant_blocks(&a.content);
            if blocks.is_empty() {
                None
            } else {
                Some(AnthMessage::assistant(blocks))
            }
        }
        AgentMessage::ToolResult { .. } => None,
    }
}

/// Inject the extended-thinking directive into an already-built request body.
/// `None` is a no-op (the body keeps its no-`thinking` shape — this is why the
/// byte-parity tests still pass). For an explicit budget, raise `max_tokens` so
/// it exceeds the budget with room for the answer ([`THINKING_ANSWER_HEADROOM`]);
/// `adaptive`/`disabled` only set the directive (model self-budgets / opts out).
/// (mu-upk2)
fn apply_thinking(body: &mut Value, thinking: Option<&ThinkingConfig>) {
    let Some(cfg) = thinking else {
        return;
    };
    match serde_json::to_value(cfg) {
        Ok(v) => body["thinking"] = v,
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize thinking config; sending request without it");
            return;
        }
    }
    if let ThinkingConfig::Enabled { budget_tokens } = cfg {
        let needed = budget_tokens.saturating_add(THINKING_ANSWER_HEADROOM);
        let current = body.get("max_tokens").and_then(Value::as_u64).unwrap_or(0) as u32;
        if needed > current {
            body["max_tokens"] = Value::from(needed);
        }
    }
}

/// Interpret a `--thinking` flag value as an Anthropic [`ThinkingConfig`].
/// Accepts effort levels (`minimal`/`low`/`medium`/`high`) mapped to token
/// budgets, a raw budget number, `adaptive` (model self-budgets), and
/// `off`/`none`/`disabled` (explicit opt-out). An empty/whitespace flag is
/// `None` (no thinking). An unrecognized non-empty value falls back to the
/// `medium` budget rather than silently doing nothing. Budgets are clamped to
/// Anthropic's 1024-token floor. (mu-upk2)
fn parse_thinking_flag(flag: &str) -> Option<ThinkingConfig> {
    let f = flag.trim().to_ascii_lowercase();
    // Empty OR an explicit "don't enable" word → send no `thinking` directive
    // at all (absent == no extended thinking, the API default). This is
    // distinct from the `disabled` keyword below, which sends the explicit
    // `{"type":"disabled"}` opt-out directive. (mu-upk2)
    if f.is_empty() || matches!(f.as_str(), "off" | "none" | "false" | "0") {
        return None;
    }
    let enabled = |n: u32| ThinkingConfig::Enabled {
        budget_tokens: n.max(1024),
    };
    Some(match f.as_str() {
        "adaptive" => ThinkingConfig::Adaptive,
        "disabled" => ThinkingConfig::Disabled,
        "minimal" => enabled(1024),
        "low" => enabled(4096),
        "medium" | "med" => enabled(10_240),
        "high" => enabled(24_576),
        other => match other.parse::<u32>() {
            Ok(n) => enabled(n),
            Err(_) => {
                tracing::debug!(
                    flag = %flag,
                    "unrecognized --thinking value for anthropic; using medium budget"
                );
                enabled(10_240)
            }
        },
    })
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
    let mut req = MessagesRequest::new(
        model,
        super::output_limits::max_tokens_for_model(model),
        map_agent_messages(messages),
    )
    .with_stream(true);
    if let Some(s) = system_prompt {
        if !s.is_empty() {
            // Legacy path emits no cache_control (mu-yqeq.8).
            req = req.with_system(system_content(s.to_string(), false, CacheTtl::default()));
        }
    }
    if !tools.is_empty() {
        req = req.with_tools(map_tools(tools, false, CacheTtl::default()));
    }
    request_to_value(req)
}

// ============================================================================
// mu -> mu_anthropic request mapping for the Projected (production) path
// ============================================================================
//
// Reads structural ContentBlocks from ProviderMessage.blocks; tool-result
// binding (tool_use_id) is recovered from each ToolResult message's
// source_span_ids[0] via extract_call_id_from_span_id (mu-yqeq.3). System spans
// are hoisted into the top-level system field; tool-schema spans are skipped
// (the `tools` parameter is authoritative for body.tools).

/// Translate a ProviderMessages projection into mu_anthropic messages plus the
/// hoisted system text (placed in the top-level `system` field by the caller).
///
/// Hoisting rule (mu-s855, post-mu-phl v0): every System-role span EXCEPT
/// tool-schema spans contributes to system_text, joined with a blank line.
/// Per-message cache markers land on the marked content block; system/tool
/// cache placement is handled separately by detect_cache_targets.
fn map_provider_messages(
    pmsgs: &ProviderMessages,
    ttl: CacheTtl,
) -> (Vec<AnthMessage>, Option<String>) {
    let mut out: Vec<AnthMessage> = Vec::with_capacity(pmsgs.messages.len());
    let mut tool_result_buf: Vec<AnthBlock> = Vec::new();
    let mut system_text: Option<String> = None;

    for msg in &pmsgs.messages {
        let marked = msg.cache_marker() == Some(CacheMarker::Ephemeral);
        match msg.role() {
            ProviderRole::System => {
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
            ProviderRole::ToolResult => {
                tool_result_buf.push(map_provider_tool_result(
                    msg,
                    marked.then(|| cache_control(ttl)),
                ));
            }
            ProviderRole::User => {
                flush_tool_results(&mut out, &mut tool_result_buf);
                if marked {
                    out.push(AnthMessage::user(vec![AnthBlock::Text {
                        text: msg.content().to_string(),
                        citations: Vec::new(),
                        cache_control: Some(cache_control(ttl)),
                    }]));
                } else {
                    out.push(AnthMessage::user(msg.content()));
                }
            }
            ProviderRole::Assistant => {
                flush_tool_results(&mut out, &mut tool_result_buf);
                if let Some(m) = map_provider_assistant(msg, marked.then(|| cache_control(ttl))) {
                    out.push(m);
                }
            }
        }
    }
    flush_tool_results(&mut out, &mut tool_result_buf);

    (out, system_text)
}

fn flush_tool_results(out: &mut Vec<AnthMessage>, buf: &mut Vec<AnthBlock>) {
    if !buf.is_empty() {
        out.push(AnthMessage::user(std::mem::take(buf)));
    }
}

/// One ToolResult ProviderMessage to a mu_anthropic tool_result block. call_id
/// comes from the synthesized span id; is_error from the "error: " content
/// prefix that assembly.rs adds for errored tool results.
fn map_provider_tool_result(msg: &ProviderMessage, cache: Option<CacheControl>) -> AnthBlock {
    let call_id = msg
        .source_span_ids()
        .first()
        .and_then(|sid| extract_call_id_from_span_id(sid.as_ref()))
        .unwrap_or("")
        .to_string();
    let (is_error, content) = match msg.content().strip_prefix("error: ") {
        Some(stripped) => (true, stripped.to_string()),
        None => (false, msg.content().to_string()),
    };
    AnthBlock::ToolResult {
        tool_use_id: call_id,
        content,
        is_error: Some(is_error),
        cache_control: cache,
    }
}

/// One assistant-role ProviderMessage to a mu_anthropic assistant message.
/// Reads structural blocks from msg.blocks(); Thinking blocks are dropped
/// outbound (spec mu-044). Returns None when the assistant produced no
/// wire-bearing blocks. A cache marker attaches cache_control to the last block.
fn map_provider_assistant(
    msg: &ProviderMessage,
    cache_last: Option<CacheControl>,
) -> Option<AnthMessage> {
    let mut mapped = map_assistant_blocks(msg.blocks()?);
    if mapped.is_empty() {
        return None;
    }
    if let (Some(cc), Some(last)) = (cache_last, mapped.last_mut()) {
        set_block_cache_control(last, cc);
    }
    Some(AnthMessage::assistant(mapped))
}

/// Sibling of build_request_body that builds the request from a ProviderMessages
/// projection. cache_control placement is driven by per-message CacheMarker
/// flags (AnthropicCacheStrategy): a marked system span caches body.system; a
/// marked tool-schema span caches the last tool spec.
pub(crate) fn build_request_body_from_projection(
    model: &str,
    pmsgs: &ProviderMessages,
    tools: &[ToolSpec],
    ttl: CacheTtl,
) -> Value {
    let (messages, hoisted_system) = map_provider_messages(pmsgs, ttl);
    let (system_should_cache, tools_should_cache) = detect_cache_targets(pmsgs);

    let mut req = MessagesRequest::new(
        model,
        super::output_limits::max_tokens_for_model(model),
        messages,
    )
    .with_stream(true);
    if let Some(s) = hoisted_system {
        if !s.is_empty() {
            req = req.with_system(system_content(s, system_should_cache, ttl));
        }
    }
    if !tools.is_empty() {
        req = req.with_tools(map_tools(tools, tools_should_cache, ttl));
    }
    request_to_value(req)
}

/// Walk the projection and determine which Anthropic wire positions
/// should carry `cache_control`. The rope strategy puts cache markers
/// on the ProviderMessage layer; this helper maps marker positions
/// back to wire positions.
///
/// mu-s855: post-mu-phl v0, multiple non-tool-schema System-role spans
/// (system-prompt, memory-recall:*, project-file:*) all contribute to
/// `body.system`. A cache marker on ANY of those spans triggers
/// `system_should_cache`. Pre-fix this helper only recognized
/// "system-prompt" — markers on memory or file spans were
/// effectively dead because their wire target was wrong.
///
/// Tool-schema marker still flows to `tools_should_cache` (separate
/// wire target).
fn detect_cache_targets(pmsgs: &ProviderMessages) -> (bool, bool) {
    let mut system_should_cache = false;
    let mut tools_should_cache = false;
    for msg in &pmsgs.messages {
        if msg.cache_marker() != Some(CacheMarker::Ephemeral) {
            continue;
        }
        // Only System-role markers map to wire-side cache_control —
        // markers on User/Assistant/ToolResult don't have a current
        // wire target (Anthropic allows cache_control on content
        // blocks but the legacy strategy only marked system + tools,
        // so we preserve that mapping).
        if msg.role() != ProviderRole::System {
            continue;
        }
        let Some(sid) = msg.source_span_ids().first() else {
            continue;
        };
        let sid_str = sid.as_ref();
        if sid_str.starts_with("tool-schema:") {
            tools_should_cache = true;
        } else {
            // Any other System-role span (system-prompt,
            // memory-recall:*, project-file:*, future SkillActivation
            // / Compaction kinds) lands in body.system, so its
            // marker flows there too.
            system_should_cache = true;
        }
    }
    (system_should_cache, tools_should_cache)
}

// SSE/stream wire types now come from the mu-anthropic crate (StreamEvent,
// BlockStart, BlockDelta, Usage, StopReason) — see the imports above.

/// Map mu_anthropic's wire `StopReason` onto mu-core's `StopReason`.
fn map_stop_reason(s: Option<&AnthropicStopReason>) -> StopReason {
    match s {
        Some(AnthropicStopReason::EndTurn) => StopReason::EndTurn,
        Some(AnthropicStopReason::ToolUse) => StopReason::ToolUse,
        Some(AnthropicStopReason::MaxTokens) => StopReason::MaxTokens,
        Some(AnthropicStopReason::StopSequence) => StopReason::EndTurn,
        Some(other) => {
            tracing::warn!(stop_reason = ?other, "unrecognized anthropic stop_reason");
            StopReason::EndTurn
        }
        None => StopReason::EndTurn,
    }
}

/// Fold a freshly-seen `usage` (from message_start or message_delta) into the
/// running accumulator: any field the incoming event populates wins. Anthropic
/// splits input/cache stats (message_start) from output_tokens (message_delta).
fn merge_usage(acc: &mut AnthropicUsage, other: &AnthropicUsage) {
    if other.input_tokens.is_some() {
        acc.input_tokens = other.input_tokens;
    }
    if other.output_tokens.is_some() {
        acc.output_tokens = other.output_tokens;
    }
    if other.cache_creation_input_tokens.is_some() {
        acc.cache_creation_input_tokens = other.cache_creation_input_tokens;
    }
    if other.cache_read_input_tokens.is_some() {
        acc.cache_read_input_tokens = other.cache_read_input_tokens;
    }
    if other.cache_creation.is_some() {
        acc.cache_creation = other.cache_creation.clone();
    }
    if other.output_tokens_details.is_some() {
        acc.output_tokens_details = other.output_tokens_details.clone();
    }
}

/// Project mu_anthropic's rich `Usage` onto mu-core's `Usage`. Returns `None`
/// when neither token count was reported (the loop treats that as "no usage").
fn anthropic_usage_to_mu(u: &AnthropicUsage) -> Option<Usage> {
    if u.input_tokens.is_none() && u.output_tokens.is_none() {
        return None;
    }
    let (cache_5m, cache_1h) = u
        .cache_creation
        .as_ref()
        .map(|cc| (cc.ephemeral_5m_input_tokens, cc.ephemeral_1h_input_tokens))
        .unwrap_or((None, None));
    Some(Usage {
        input_tokens: u.input_tokens.unwrap_or(0),
        output_tokens: u.output_tokens.unwrap_or(0),
        cache_read_input_tokens: u.cache_read_input_tokens,
        cache_creation_input_tokens: u.cache_creation_input_tokens,
        cache_creation_5m_input_tokens: cache_5m,
        cache_creation_1h_input_tokens: cache_1h,
        reasoning_tokens: u
            .output_tokens_details
            .as_ref()
            .and_then(|d| d.thinking_tokens),
    })
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
    /// Accumulated extended-thinking text. The block's `signature_delta`
    /// (mu_anthropic `BlockDelta::SignatureDelta`) is intentionally NOT
    /// retained: `ContentBlock::Thinking` carries display text only, and
    /// thinking is stripped on the way back TO the model (spec mu-044),
    /// so the signature has no consumer.
    Thinking(String),
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
    /// Terminal stop reason from `message_delta` (mu_anthropic wire type).
    stop_reason: Option<AnthropicStopReason>,
    /// Combined usage from message_start (input tokens, cache stats) and
    /// message_delta (output tokens); `merge_usage` folds them as we see them.
    usage: AnthropicUsage,
    cancel_rx: Option<oneshot::Receiver<()>>,
    finished: bool,
    emitted_done: bool,
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
                    let usage = anthropic_usage_to_mu(&state.usage);
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
                    let usage = anthropic_usage_to_mu(&state.usage);
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

        // Parse the SSE payload as a mu_anthropic stream event.
        let parsed: Result<StreamEvent, _> = serde_json::from_str(&sse_event.data);
        let parsed = match parsed {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, data = %sse_event.data, "failed to parse anthropic event");
                continue;
            }
        };

        match parsed {
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                // Register a new block. Re-registering an index is a
                // protocol violation; we replace silently.
                let builder = match content_block {
                    BlockStart::Text { text } => BlockBuilder::Text(text),
                    BlockStart::ToolUse { id, name } => BlockBuilder::ToolUse {
                        id,
                        name,
                        input_json: String::new(),
                    },
                    BlockStart::Thinking { thinking } => BlockBuilder::Thinking(thinking),
                    BlockStart::Other => continue,
                };
                if !state.blocks.contains_key(&index) {
                    state.block_order.push(index);
                }
                // Surface the block's opening as a live event before
                // storing it, so the agent loop sees thinking starts and
                // tool-call starts in stream order (mirrors how TextDelta
                // is surfaced live). A tool_use start announces the call
                // id+name; subsequent input_json deltas carry the args.
                let opening = match &builder {
                    BlockBuilder::Thinking(text) if !text.is_empty() => {
                        Some(ProviderEvent::ThinkingDelta(text.clone()))
                    }
                    BlockBuilder::ToolUse { id, name, .. } => Some(ProviderEvent::ToolCallDelta {
                        id: id.clone(),
                        name_delta: Some(name.clone()),
                        arguments_delta: None,
                    }),
                    _ => None,
                };
                state.blocks.insert(index, builder);
                if let Some(ev) = opening {
                    return Some((ev, state));
                }
            }
            StreamEvent::ContentBlockDelta { index, delta } => match delta {
                BlockDelta::TextDelta { text } => {
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
                BlockDelta::InputJsonDelta { partial_json } => {
                    // Accumulate for the final assembled tool call (the
                    // Done payload stays authoritative/complete) AND
                    // surface the fragment live so the loop can stream
                    // partial tool args.
                    match state.blocks.get_mut(&index) {
                        Some(BlockBuilder::ToolUse { id, input_json, .. }) => {
                            input_json.push_str(&partial_json);
                            let id = id.clone();
                            return Some((
                                ProviderEvent::ToolCallDelta {
                                    id,
                                    name_delta: None,
                                    arguments_delta: Some(partial_json),
                                },
                                state,
                            ));
                        }
                        _ => {
                            // Delta for a tool block we never saw start —
                            // protocol violation. Log and ignore.
                            tracing::warn!(
                                index,
                                "input_json_delta arrived for unknown or non-tool block"
                            );
                        }
                    }
                }
                BlockDelta::ThinkingDelta { thinking } => {
                    match state.blocks.get_mut(&index) {
                        Some(BlockBuilder::Thinking(buf)) => buf.push_str(&thinking),
                        _ => {
                            // No matching thinking block — treat as an
                            // implicit start (mirrors the TextDelta path).
                            if !state.blocks.contains_key(&index) {
                                state.block_order.push(index);
                            }
                            state
                                .blocks
                                .insert(index, BlockBuilder::Thinking(thinking.clone()));
                        }
                    }
                    return Some((ProviderEvent::ThinkingDelta(thinking), state));
                }
                BlockDelta::SignatureDelta { .. } => {
                    // Extended-thinking blocks carry a cryptographic
                    // signature here. Not retained — see the BlockBuilder
                    // ::Thinking doc (display-only; stripped outbound per
                    // mu-044, so no consumer).
                }
                BlockDelta::Other => {
                    // Unknown delta type (forward-compat); ignore.
                }
            },
            StreamEvent::ContentBlockStop { .. } => {
                // No-op; the block stays in the map until assembled at
                // message_stop.
            }
            StreamEvent::MessageDelta { delta, usage } => {
                state.stop_reason = delta.stop_reason;
                // mu-yz48: usage is the event-top-level sibling of `delta`
                // (mu_anthropic models it there, boxed).
                if let Some(u) = usage.as_deref() {
                    merge_usage(&mut state.usage, u);
                }
            }
            StreamEvent::MessageStop => {
                state.finished = true;
                state.emitted_done = true;
                let stop = map_stop_reason(state.stop_reason.as_ref());
                let usage = anthropic_usage_to_mu(&state.usage);
                return Some((
                    ProviderEvent::Done(AssistantMessage {
                        content: assemble_content(&state.blocks, &state.block_order),
                        stop_reason: stop,
                        usage,
                    }),
                    state,
                ));
            }
            StreamEvent::Error { error } => {
                state.finished = true;
                state.emitted_done = true;
                let msg = format!(
                    "anthropic stream error ({}): {}",
                    error.kind.unwrap_or_else(|| "unknown".to_string()),
                    error.message.unwrap_or_else(|| "(no message)".to_string()),
                );
                return Some((ProviderEvent::Error(msg), state));
            }
            StreamEvent::MessageStart { message } => {
                // Initial usage (input tokens, cache stats) rides on the
                // message_start envelope, which mu_anthropic models as raw JSON.
                // Absent `usage` is normal; a present-but-unparseable `usage` is
                // logged rather than silently dropped.
                if let Some(usage_val) = message.as_value().get("usage") {
                    match serde_json::from_value::<AnthropicUsage>(usage_val.clone()) {
                        Ok(u) => merge_usage(&mut state.usage, &u),
                        Err(e) => tracing::warn!(
                            error = %e,
                            "message_start usage failed to parse; token stats may be incomplete"
                        ),
                    }
                }
            }
            StreamEvent::Ping => {
                // No-op.
            }
            StreamEvent::Unknown(v) => {
                // Forward-compat: an event type mu_anthropic doesn't model. Log
                // it so a new SSE event type doesn't vanish without a trace (the
                // old hand-rolled parser surfaced these as parse warnings).
                tracing::debug!(
                    event_type = ?v.as_value().get("type"),
                    "unhandled anthropic stream event"
                );
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
            BlockBuilder::Thinking(text) => ContentBlock::Thinking {
                text: text.as_str().into(),
            },
        })
        .collect()
}

/// Parse a tool's accumulated input JSON. On any failure (malformed
/// JSON, valid JSON that isn't an object), fall back to an empty
/// object and log a warning. Tools expect an object of arguments;
/// passing through arrays/strings/numbers would break the contract
/// with `Tool::execute`.
fn parse_tool_input(input_json: &str) -> mu_core::agent::ToolArgs {
    use mu_core::agent::ToolArgs;

    let value = if input_json.is_empty() {
        Value::Object(serde_json::Map::new())
    } else {
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
    };
    // Safe: serde_json's parser rejects NaN/Inf per RFC 8259.
    ToolArgs::new(value).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "tool arguments contained non-finite number; using empty object");
        ToolArgs::new(Value::Object(serde_json::Map::new())).unwrap()
    })
}

#[cfg(test)]
#[path = "anthropic_tests.rs"]
mod tests;
