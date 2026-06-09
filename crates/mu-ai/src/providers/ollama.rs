//! Ollama provider — Anthropic Messages wire against a local ollama
//! server (default `http://10.1.1.143:11434`).
//!
//! Ollama (>= v0.14) serves the *Anthropic* Messages API at
//! `/v1/messages` — the exact wire format [`AnthropicProvider`]
//! already speaks (SSE `message_start` / `content_block_delta` /
//! `message_delta`, top-level `system`, native `tool_use` blocks).
//! Rather than duplicate that path, this provider **composes**
//! `AnthropicProvider` with an ollama-pointed base URL and an
//! `"ollama"` label. (bead mu-fmas; was mu-818c, which composed the
//! OpenAI-compat `OpenRouterProvider` against `/v1/chat/completions`.)
//!
//! Why the switch: the Anthropic wire gives ollama proper fidelity —
//! native `tool_use`, thinking blocks, top-level `system` — and lets
//! it advertise the matching [`capabilities`](Provider::capabilities)
//! (top-level `system`, `anthropic_style` usage). Prompt-caching is
//! the one cap we turn OFF: ollama's Anthropic-compat reports no cache
//! fields in `usage` and accepts no `cache_control` request field
//! (docs.ollama.com/api/anthropic-compatibility), so advertising it
//! would only surface an always-zero column. Context size/pressure is
//! mu's own bookkeeping (token accounting via `usage_semantics`), not
//! something the server reports — so it surfaces regardless of wire.
//!
//! CREDENTIAL ISOLATION (the load-bearing reason this is hand-wired
//! rather than `AnthropicProvider::from_env`): we construct via
//! `AnthropicProvider::new(key, model).with_api_base(...)` with the
//! key sourced from `OLLAMA_API_KEY` (default empty — ollama ignores
//! auth). We deliberately do NOT call `AnthropicProvider::from_env`,
//! because that reads `ANTHROPIC_API_KEY` into the `x-api-key` header
//! and consults `ANTHROPIC_BASE_URL`. Pointing that path at the LAN
//! ollama box would ship the real Anthropic credential to a local
//! socket. By never invoking `from_env` here, `ANTHROPIC_API_KEY` has
//! no wire to the ollama endpoint at all.
//!
//! Env overrides: `OLLAMA_API_BASE` (base URL), `OLLAMA_API_KEY`
//! (optional; defaults empty). The request path is fixed at
//! `/v1/messages` by `AnthropicProvider`, so the legacy
//! `OLLAMA_API_PATH` override (an OpenAI-compat-era knob) no longer
//! applies.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use serde::Deserialize;
use tokio::sync::oneshot;

use mu_core::agent::capabilities::ProviderCapabilities;
use mu_core::agent::{MessageInput, ProbedModel, Provider, ProviderError, ProviderEvent, ToolSpec};
use mu_core::context::{CacheStrategy, NoCacheStrategy, ProviderRenderer};

use super::anthropic::AnthropicProvider;

/// Default base URL for the local ollama box on the LAN. Baked in so
/// `--provider ollama` and `AGENT_SPAWN_PROVIDER=ollama` work with no
/// extra configuration — overridable via `OLLAMA_API_BASE`.
pub const OLLAMA_API_BASE: &str = "http://10.1.1.143:11434";

/// Timeout for the capability probe (mu-1gx5). Bounds a slow / half-open ollama
/// so `mu models refresh` skips it best-effort instead of hanging — parity with
/// `discover_models`'s timeout (review-gate finding).
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Resolve the ollama base URL: `OLLAMA_API_BASE` if set and non-empty,
/// else the baked-in [`OLLAMA_API_BASE`] default.
pub fn base_from_env() -> String {
    std::env::var("OLLAMA_API_BASE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| OLLAMA_API_BASE.to_string())
}

pub struct OllamaProvider {
    inner: AnthropicProvider,
    /// The ollama base URL (e.g. `http://10.1.1.143:11434`), kept so the
    /// capability probe can reach the NATIVE `/api/tags` endpoint — distinct
    /// from the `/v1/messages` wire the inner Anthropic provider uses for chat.
    base: String,
}

impl OllamaProvider {
    /// Construct from env. Base defaults to the local ollama box; the
    /// API key defaults to empty (ollama needs none).
    ///
    /// Key isolation: the key comes from `OLLAMA_API_KEY`, never
    /// `ANTHROPIC_API_KEY`. We build the inner provider with
    /// `AnthropicProvider::new(..).with_api_base(..)` rather than
    /// `from_env` precisely so the real Anthropic credential cannot
    /// leak to the LAN ollama socket (see the module docs).
    pub fn from_env(model: String) -> Result<Self, ProviderError> {
        let base = base_from_env();
        let key = std::env::var("OLLAMA_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_default();
        let inner = AnthropicProvider::new(key, model).with_api_base(base.clone());
        Ok(Self { inner, base })
    }

    /// Query the ollama server for its locally-available models via the
    /// native `/api/tags` endpoint. Best-effort: callers (the daemon's
    /// route-catalog probe) should treat any error as "ollama not
    /// reachable, no entries" rather than fatal. `timeout` bounds the
    /// whole request so a down endpoint can't stall daemon startup.
    pub async fn discover_models(
        base: &str,
        timeout: Duration,
    ) -> Result<Vec<String>, ProviderError> {
        let url = format!("{}/api/tags", base.trim_end_matches('/'));
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| ProviderError::Other(format!("ollama client build: {e}")))?;
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| ProviderError::Other(format!("ollama /api/tags request: {e}")))?;
        if !resp.status().is_success() {
            return Err(ProviderError::Other(format!(
                "ollama /api/tags returned {}",
                resp.status()
            )));
        }
        let parsed: TagsResponse = resp
            .json()
            .await
            .map_err(|e| ProviderError::Other(format!("ollama /api/tags decode: {e}")))?;
        Ok(parsed.models.into_iter().map(|m| m.name).collect())
    }
}

#[async_trait]
impl Provider for OllamaProvider {
    async fn stream(
        &self,
        system_prompt: Option<&str>,
        input: MessageInput<'_>,
        tools: &[ToolSpec],
        cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        // Identical wire protocol to Anthropic; delegate wholesale.
        let inner = self
            .inner
            .stream(system_prompt, input, tools, cancel_rx)
            .await?;
        // Locally-served models flakily emit tool calls as plain text
        // in their training-native dialect, and ollama's template
        // parser doesn't always recover them into native tool_use
        // blocks. Rewrite the terminal message when that happens
        // (mu-ollama-qwen-tool-dialect-yfl0). Belt-and-suspenders on
        // the Anthropic wire, where ollama usually emits tool_use
        // natively but Qwen can still leak.
        let specs: Vec<ToolSpec> = tools.to_vec();
        Ok(inner
            .map(move |ev| match ev {
                ProviderEvent::Done(msg) => {
                    ProviderEvent::Done(super::tool_dialect::rescue_assistant_message(msg, &specs))
                }
                other => other,
            })
            .boxed())
    }

    /// Label as `"ollama"` (not the inner `"anthropic"`) so events,
    /// route-catalog `provider_kind`, and diagnostics attribute traffic
    /// to the right backend.
    fn provider_label(&self) -> &'static str {
        "ollama"
    }

    /// Inherit the inner Anthropic wire's capabilities (top-level
    /// `system`, `anthropic_style` usage) — EXCEPT prompt caching,
    /// which we force OFF. ollama's Anthropic-compat reports no cache
    /// fields and accepts no `cache_control`
    /// (docs.ollama.com/api/anthropic-compatibility), so claiming the
    /// capability would surface a phantom always-zero column. We don't
    /// advertise what the backend doesn't do. The actual no-emission
    /// guarantee is enforced by [`cache_strategy`](Self::cache_strategy)
    /// below, not by this boolean.
    fn capabilities(&self) -> ProviderCapabilities {
        let mut caps = self.inner.capabilities();
        caps.supports_prompt_caching = false;
        caps
    }

    /// Use the Anthropic renderer so rope projections are
    /// Anthropic-shaped. This is LIVE, not forward-looking: since
    /// mu-yqeq.8 the agent loop renders each turn via
    /// `provider.renderer()` (loop_/mod.rs), so ollama MUST render with
    /// the Anthropic renderer to match the Anthropic wire its inner
    /// provider speaks. Delegating to the inner provider keeps the
    /// renderer and wire format in lockstep.
    fn renderer(&self) -> Arc<dyn ProviderRenderer> {
        self.inner.renderer()
    }

    /// Pin to `NoCacheStrategy` — the load-bearing guarantee that
    /// ollama is never sent `cache_control`. Since mu-yqeq.8,
    /// `cache_control` emission on the Anthropic wire is driven by
    /// per-message `CacheMarker` flags that the cache strategy sets via
    /// `annotate()`; `NoCacheStrategy::annotate` is a no-op, so no
    /// markers are set and no `cache_control` reaches ollama (which
    /// reports no cache fields and accepts no such request field). We
    /// override explicitly rather than leaning on the trait default so
    /// the guarantee is visible at this provider, not implicit
    /// elsewhere — and robust if the default ever changes.
    fn cache_strategy(&self) -> Arc<dyn CacheStrategy> {
        Arc::new(NoCacheStrategy::new())
    }

    /// mu-1gx5: probe via the NATIVE `/api/tags` (ollama's `/v1/*` surface is
    /// OpenAI-thin and carries no metadata). Each entry's `details.context_length`
    /// is the model's architectural context ceiling. `max_output_tokens` is left
    /// `None` ON PURPOSE: ollama has no hard per-model output cap — output is
    /// bounded by `num_ctx - input`, not a reported ceiling — so the probe
    /// informs context, not output (capability flags live in `/api/show`, a
    /// per-model N+1 call deferred to a follow-up).
    async fn probe_model_capabilities(&self) -> Result<Vec<ProbedModel>, ProviderError> {
        let url = format!("{}/api/tags", self.base.trim_end_matches('/'));
        let resp = reqwest::Client::new()
            .get(&url)
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .map_err(|e| ProviderError::Other(format!("ollama /api/tags GET {url}: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(ProviderError::Other(format!(
                "ollama /api/tags GET {url}: HTTP {status}"
            )));
        }
        let text = resp
            .text()
            .await
            .map_err(|e| ProviderError::Other(format!("ollama /api/tags read: {e}")))?;
        parse_ollama_tags(&text)
    }
}

/// Parse an ollama `/api/tags` body into [`ProbedModel`]s. Pure (no I/O) so it
/// is unit-testable against a captured sample. mu-1gx5.
pub(crate) fn parse_ollama_tags(json: &str) -> Result<Vec<ProbedModel>, ProviderError> {
    let parsed: TagsResponse = serde_json::from_str(json)
        .map_err(|e| ProviderError::Other(format!("ollama /api/tags parse: {e}")))?;
    Ok(parsed
        .models
        .into_iter()
        .map(|m| ProbedModel {
            id: m.name,
            // ollama has no hard output cap — see probe_model_capabilities.
            max_output_tokens: None,
            context_length: m.details.and_then(|d| d.context_length),
            capabilities: Vec::new(),
        })
        .collect())
}

/// `/api/tags` response shape — model name + architectural context length.
#[derive(Debug, Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagModel>,
}

#[derive(Debug, Deserialize)]
struct TagModel {
    name: String,
    // Option (not bare `#[serde(default)]`) so an explicit `"details": null` —
    // not just an ABSENT field — degrades to None instead of failing the whole
    // probe with "invalid type: null, expected struct" (review-gate finding).
    #[serde(default)]
    details: Option<TagDetails>,
}

#[derive(Debug, Default, Deserialize)]
struct TagDetails {
    /// The model's architectural context ceiling (GGUF metadata). May be
    /// absent (`null`) for some tags.
    #[serde(default)]
    context_length: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_constructs_with_default_base() {
        // No env set → baked-in default; construction must succeed
        // without any API key (ollama needs none).
        let p = OllamaProvider::from_env("qwen3-coder:30b".to_string())
            .expect("ollama provider should construct without a key");
        assert_eq!(p.provider_label(), "ollama");
    }

    #[test]
    fn base_from_env_defaults_to_baked_in() {
        // Hermetic: only assert the default when the override is unset.
        if std::env::var("OLLAMA_API_BASE").is_err() {
            assert_eq!(base_from_env(), OLLAMA_API_BASE);
        }
    }

    #[test]
    fn advertises_anthropic_caps_with_caching_off() {
        // On the Anthropic wire, ollama advertises the matching caps
        // (top-level system, anthropic_style usage) — but prompt
        // caching is forced OFF, because ollama's Anthropic-compat
        // has no cache fields / no cache_control. Guard both: the
        // promotion to Anthropic caps AND the caching exception.
        use mu_core::agent::capabilities::SystemPromptCapability;
        let p = OllamaProvider::from_env("qwen3-coder:30b".to_string()).unwrap();
        let caps = p.capabilities();
        assert!(
            !caps.supports_prompt_caching,
            "ollama doesn't cache — prompt caching must be turned OFF"
        );
        assert!(
            matches!(
                caps.system_prompt,
                SystemPromptCapability::TopLevelField { .. }
            ),
            "ollama-on-anthropic-wire should use the top-level system field"
        );
        // anthropic_style usage carried through from the inner provider.
        assert_eq!(caps.usage_semantics.cache_read_in_input, Some(false));
    }

    #[test]
    fn tags_response_parses_names() {
        let json = r#"{"models":[{"name":"qwen3-coder:30b","size":1},{"name":"deepseek-r1:32b"}]}"#;
        let parsed: TagsResponse = serde_json::from_str(json).unwrap();
        let names: Vec<String> = parsed.models.into_iter().map(|m| m.name).collect();
        assert_eq!(names, vec!["qwen3-coder:30b", "deepseek-r1:32b"]);
    }

    // mu-1gx5: capability probe — parse /api/tags into ProbedModel, reading the
    // architectural context ceiling and (deliberately) NO output cap.
    #[test]
    fn probe_parses_ollama_tags() {
        // Captured shape: details.context_length present on one, absent on
        // another (some tags report null).
        let json = r#"{"models":[
          {"name":"qwen3.6:35b-a3b-q8_0","details":{"context_length":262144,"quantization_level":"Q8_0"}},
          {"name":"gpt-oss:20b","details":{"family":"gptoss"}}
        ]}"#;
        let models = parse_ollama_tags(json).expect("parse");
        assert_eq!(models.len(), 2);

        let q = &models[0];
        assert_eq!(q.id, "qwen3.6:35b-a3b-q8_0");
        assert_eq!(q.context_length, Some(262_144));
        // ollama has no hard output cap — the probe reports None, never a guess.
        assert_eq!(q.max_output_tokens, None);

        // Missing details.context_length → None, not an error.
        assert_eq!(models[1].context_length, None);
    }

    // mu-1gx5 (review-gate finding, gpt-5.5): an explicit `"details": null` must
    // degrade to context_length=None, NOT fail the whole probe. #[serde(default)]
    // only covers an ABSENT field; a present null needs Option<TagDetails>.
    #[test]
    fn probe_handles_null_details() {
        let json = r#"{"models":[{"name":"weird:tag","details":null}]}"#;
        let models = parse_ollama_tags(json).expect("null details must not fail the probe");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "weird:tag");
        assert_eq!(models[0].context_length, None);
    }
}
