//! vLLM provider — OpenAI-compatible chat-completions against a local
//! `vllm serve` process.
//!
//! vLLM exposes `/v1/chat/completions`, which is the same streaming wire
//! already implemented by [`OpenRouterProvider`]. This provider composes
//! that implementation with vLLM-specific env knobs and a `"vllm"` label.
//!
//! Env overrides: `VLLM_API_BASE` (default `http://10.1.1.143:8000`),
//! `VLLM_API_PATH` (default `/v1/chat/completions`), `VLLM_API_KEY`
//! (optional; defaults empty — local vLLM usually ignores auth).

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use serde::Deserialize;
use tokio::sync::oneshot;

use mu_core::agent::{MessageInput, Provider, ProviderError, ProviderEvent, ToolSpec};
use mu_core::context::ProviderRenderer;
use std::sync::Arc;

use super::openrouter::OpenRouterProvider;

pub const VLLM_API_BASE: &str = "http://10.1.1.143:8000";
pub const VLLM_API_PATH: &str = "/v1/chat/completions";

pub fn base_from_env() -> String {
    std::env::var("VLLM_API_BASE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| VLLM_API_BASE.to_string())
}

fn path_from_env() -> String {
    std::env::var("VLLM_API_PATH")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| VLLM_API_PATH.to_string())
}

pub struct VllmProvider {
    inner: OpenRouterProvider,
}

impl VllmProvider {
    pub fn from_env(model: String) -> Result<Self, ProviderError> {
        let key = std::env::var("VLLM_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_default();
        let inner = OpenRouterProvider::new(key, model)
            .with_api_base(base_from_env())
            .with_api_path(path_from_env());
        Ok(Self { inner })
    }

    pub async fn discover_models(
        base: &str,
        timeout: std::time::Duration,
    ) -> Result<Vec<String>, ProviderError> {
        let url = format!("{}/v1/models", base.trim_end_matches('/'));
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| ProviderError::Other(format!("vllm client build: {e}")))?;
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| ProviderError::Other(format!("vllm /v1/models request: {e}")))?;
        if !resp.status().is_success() {
            return Err(ProviderError::Other(format!(
                "vllm /v1/models returned {}",
                resp.status()
            )));
        }
        let parsed: ModelsResponse = resp
            .json()
            .await
            .map_err(|e| ProviderError::Other(format!("vllm /v1/models decode: {e}")))?;
        Ok(parsed.data.into_iter().map(|m| m.id).collect())
    }
}

#[async_trait]
impl Provider for VllmProvider {
    async fn stream(
        &self,
        system_prompt: Option<&str>,
        input: MessageInput<'_>,
        tools: &[ToolSpec],
        cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let inner = self
            .inner
            .stream(system_prompt, input, tools, cancel_rx)
            .await?;
        Ok(inner.boxed())
    }

    fn provider_label(&self) -> &'static str {
        "vllm"
    }

    fn capabilities(&self) -> mu_core::agent::capabilities::ProviderCapabilities {
        self.inner.capabilities()
    }

    fn renderer(&self) -> Arc<dyn ProviderRenderer> {
        self.inner.renderer()
    }
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_constructs_without_key() {
        let p = VllmProvider::from_env("Qwen/Qwen3-Coder-30B-A3B-Instruct-FP8".to_string())
            .expect("local vllm provider should not require an API key");
        assert_eq!(p.provider_label(), "vllm");
    }

    #[test]
    fn base_from_env_defaults_to_baked_in() {
        if std::env::var("VLLM_API_BASE").is_err() {
            assert_eq!(base_from_env(), VLLM_API_BASE);
        }
    }
}
