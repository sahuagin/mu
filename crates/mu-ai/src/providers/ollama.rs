//! Ollama provider — OpenAI-compatible chat-completions against a
//! local ollama server (default `http://10.1.1.143:11434`).
//!
//! Ollama exposes the *exact* OpenAI chat-completions wire format that
//! [`OpenRouterProvider`] already speaks (`/v1/chat/completions`, SSE
//! deltas, `stream_options.include_usage`). Rather than duplicate the
//! ~800 lines of request-building / SSE-parsing, this provider
//! **composes** `OpenRouterProvider` with an ollama-pointed base URL
//! and an `"ollama"` label. Any future fix to the OpenAI-compatible
//! streaming path therefore applies to both. (bead mu-818c)
//!
//! Env overrides: `OLLAMA_API_BASE`, `OLLAMA_API_PATH`,
//! `OLLAMA_API_KEY` (the last is optional — ollama ignores auth, so it
//! defaults to empty rather than erroring like OpenRouter does against
//! openrouter.ai).

use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use serde::Deserialize;
use tokio::sync::oneshot;

use mu_core::agent::capabilities::ProviderCapabilities;
use mu_core::agent::{MessageInput, Provider, ProviderError, ProviderEvent, ToolSpec};

use super::openrouter::OpenRouterProvider;

/// Default base URL for the local ollama box on the LAN. Baked in so
/// `--provider ollama` and `AGENT_SPAWN_PROVIDER=ollama` work with no
/// extra configuration — overridable via `OLLAMA_API_BASE`.
pub const OLLAMA_API_BASE: &str = "http://10.1.1.143:11434";
/// Ollama serves the OpenAI-compatible endpoint at `/v1/chat/completions`.
pub const OLLAMA_API_PATH: &str = "/v1/chat/completions";

/// Resolve the ollama base URL: `OLLAMA_API_BASE` if set and non-empty,
/// else the baked-in [`OLLAMA_API_BASE`] default.
pub fn base_from_env() -> String {
    std::env::var("OLLAMA_API_BASE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| OLLAMA_API_BASE.to_string())
}

pub struct OllamaProvider {
    inner: OpenRouterProvider,
}

impl OllamaProvider {
    /// Construct from env. Base/path default to the local ollama box;
    /// the API key defaults to empty (ollama needs none) instead of
    /// erroring the way `OpenRouterProvider::from_env` does for the
    /// real openrouter.ai endpoint.
    pub fn from_env(model: String) -> Result<Self, ProviderError> {
        let base = base_from_env();
        let path = std::env::var("OLLAMA_API_PATH")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| OLLAMA_API_PATH.to_string());
        let key = std::env::var("OLLAMA_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_default();
        let inner = OpenRouterProvider::new(key, model)
            .with_api_base(base)
            .with_api_path(path);
        Ok(Self { inner })
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
        // Identical wire protocol to OpenRouter; delegate wholesale.
        let inner = self
            .inner
            .stream(system_prompt, input, tools, cancel_rx)
            .await?;
        // Locally-served models flakily emit tool calls as plain text
        // in their training-native dialect, and ollama's template
        // parser doesn't always recover them. Rewrite the terminal
        // message when that happens (mu-ollama-qwen-tool-dialect-yfl0).
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

    /// Label as `"ollama"` (not the inner `"openrouter"`) so events,
    /// route-catalog `provider_kind`, and diagnostics attribute traffic
    /// to the right backend.
    fn provider_label(&self) -> &'static str {
        "ollama"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        // Same OpenAI-compatible shape as the inner provider.
        self.inner.capabilities()
    }
}

/// `/api/tags` response shape — only the model names are needed.
#[derive(Debug, Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagModel>,
}

#[derive(Debug, Deserialize)]
struct TagModel {
    name: String,
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
    fn tags_response_parses_names() {
        let json = r#"{"models":[{"name":"qwen3-coder:30b","size":1},{"name":"deepseek-r1:32b"}]}"#;
        let parsed: TagsResponse = serde_json::from_str(json).unwrap();
        let names: Vec<String> = parsed.models.into_iter().map(|m| m.name).collect();
        assert_eq!(names, vec!["qwen3-coder:30b", "deepseek-r1:32b"]);
    }
}
