//! Catalog probe — the HTTP half of `mu models sync` (bead
//! context-limit-harden-sync, work item 3).
//!
//! `mu-core` is HTTP-free, so the network reach lives here and fills the
//! plain [`ProbedModel`] DTO that `mu-core`'s
//! [`catalog_sync`](mu_core::catalog_sync) writer consumes. These are free
//! functions, **not** `Provider` trait methods — the rejected
//! capability-probe branch (mu-1gx5) bolted probing onto `Provider` and
//! dragged network I/O into the agent hot path; the sync tool is a
//! standalone run-once utility instead.
//!
//! Each provider reports the **served** context window differently:
//! - **openrouter**: `GET /api/v1/models` → `context_length` +
//!   `top_provider.max_completion_tokens` + `pricing.{prompt,completion}`.
//! - **vllm**: `GET /v1/models` → `max_model_len` (vLLM's OpenAI extension).
//! - **ollama**: `GET /api/tags` lists ids, then `POST /api/show` per model
//!   → the **baked `num_ctx`** parameter. We deliberately do NOT use the
//!   architecture `context_length` from `model_info`: that is the model's
//!   *maximum*, not what the server actually serves, and loading at the max
//!   is exactly what triggers eviction/reload churn. If no `num_ctx` is
//!   baked, the served window is the server's `OLLAMA_CONTEXT_LENGTH`
//!   default, which the API does not expose — so we report `None` (honest)
//!   rather than fabricate, and the consumer falls back to a safe default.

use std::time::Duration;

use serde::Deserialize;

use mu_core::agent::ProviderError;
use mu_core::catalog_sync::ProbedModel;

fn client(timeout: Duration) -> Result<reqwest::Client, ProviderError> {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| ProviderError::Other(format!("catalog probe client build: {e}")))
}

// ---------------------------------------------------------------------------
// openrouter: GET /api/v1/models
// ---------------------------------------------------------------------------

/// Probe openrouter's full catalog in one request. The caller intersects the
/// result with the operator selection — openrouter returns ~300 models, but
/// only the referenced handful are written.
pub async fn probe_openrouter(
    base: &str,
    api_key: &str,
    timeout: Duration,
) -> Result<Vec<ProbedModel>, ProviderError> {
    let url = format!("{}/api/v1/models", base.trim_end_matches('/'));
    let mut req = client(timeout)?.get(&url);
    if !api_key.is_empty() {
        req = req.bearer_auth(api_key);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| ProviderError::Other(format!("openrouter /api/v1/models request: {e}")))?;
    if !resp.status().is_success() {
        return Err(ProviderError::Other(format!(
            "openrouter /api/v1/models returned {}",
            resp.status()
        )));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| ProviderError::Other(format!("openrouter /api/v1/models body: {e}")))?;
    parse_openrouter_models(&text)
}

/// Pure parser for an openrouter `/api/v1/models` body (no I/O), so it is
/// unit-testable against a captured sample.
pub fn parse_openrouter_models(json: &str) -> Result<Vec<ProbedModel>, ProviderError> {
    let parsed: OpenRouterModels = serde_json::from_str(json)
        .map_err(|e| ProviderError::Other(format!("openrouter /api/v1/models parse: {e}")))?;
    Ok(parsed
        .data
        .into_iter()
        .map(|m| ProbedModel {
            context_hard_limit: m.context_length,
            max_output_tokens: m
                .top_provider
                .as_ref()
                .and_then(|t| t.max_completion_tokens),
            pricing_input_per_mtok: m.pricing.as_ref().and_then(|p| per_mtok(&p.prompt)),
            pricing_output_per_mtok: m.pricing.as_ref().and_then(|p| per_mtok(&p.completion)),
            id: m.id,
        })
        .collect())
}

/// OpenRouter prices are USD **per token** as decimal strings (e.g.
/// `"0.000005"`); convert to USD per million tokens. Unparseable / empty ->
/// `None` (no pricing claimed rather than a wrong zero).
fn per_mtok(per_token: &str) -> Option<f64> {
    let t = per_token.trim();
    if t.is_empty() {
        return None;
    }
    t.parse::<f64>().ok().map(|v| v * 1_000_000.0)
}

#[derive(Debug, Deserialize)]
struct OpenRouterModels {
    #[serde(default)]
    data: Vec<OpenRouterModel>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterModel {
    id: String,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    top_provider: Option<OpenRouterTopProvider>,
    #[serde(default)]
    pricing: Option<OpenRouterPricing>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterTopProvider {
    #[serde(default)]
    max_completion_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterPricing {
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    completion: String,
}

// ---------------------------------------------------------------------------
// vllm: GET /v1/models  (OpenAI-compatible + vLLM's max_model_len extension)
// ---------------------------------------------------------------------------

/// Probe a vLLM server's models in one request. vLLM adds `max_model_len` to
/// each OpenAI `/v1/models` entry — the served window. No pricing (local).
pub async fn probe_vllm(base: &str, timeout: Duration) -> Result<Vec<ProbedModel>, ProviderError> {
    let url = format!("{}/v1/models", base.trim_end_matches('/'));
    let resp = client(timeout)?
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
    let text = resp
        .text()
        .await
        .map_err(|e| ProviderError::Other(format!("vllm /v1/models body: {e}")))?;
    parse_vllm_models(&text)
}

/// Pure parser for a vLLM `/v1/models` body (no I/O).
pub fn parse_vllm_models(json: &str) -> Result<Vec<ProbedModel>, ProviderError> {
    let parsed: VllmModels = serde_json::from_str(json)
        .map_err(|e| ProviderError::Other(format!("vllm /v1/models parse: {e}")))?;
    Ok(parsed
        .data
        .into_iter()
        .map(|m| ProbedModel {
            context_hard_limit: m.max_model_len,
            id: m.id,
            ..Default::default()
        })
        .collect())
}

#[derive(Debug, Deserialize)]
struct VllmModels {
    #[serde(default)]
    data: Vec<VllmModel>,
}

#[derive(Debug, Deserialize)]
struct VllmModel {
    id: String,
    #[serde(default)]
    max_model_len: Option<u64>,
}

// ---------------------------------------------------------------------------
// ollama: GET /api/tags (list) + POST /api/show (served num_ctx, per model)
// ---------------------------------------------------------------------------

/// List the ollama server's local model ids via `/api/tags`. The discovery
/// surface for `mu models list ollama` and the first step of the sync (the
/// caller filters to the selection before the per-model `/api/show`, keeping
/// the N+1 bounded to models the operator actually uses).
pub async fn list_ollama(base: &str, timeout: Duration) -> Result<Vec<String>, ProviderError> {
    let url = format!("{}/api/tags", base.trim_end_matches('/'));
    let resp = client(timeout)?
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
    let parsed: OllamaTags = resp
        .json()
        .await
        .map_err(|e| ProviderError::Other(format!("ollama /api/tags decode: {e}")))?;
    Ok(parsed.models.into_iter().map(|m| m.name).collect())
}

/// Probe one ollama model's served window via `POST /api/show`. Returns the
/// baked `num_ctx` as `context_hard_limit`, or `None` if not baked (see the
/// module docs — the server default isn't exposed, so we don't fabricate).
pub async fn show_ollama(
    base: &str,
    model: &str,
    timeout: Duration,
) -> Result<ProbedModel, ProviderError> {
    let url = format!("{}/api/show", base.trim_end_matches('/'));
    let resp = client(timeout)?
        .post(&url)
        .json(&serde_json::json!({ "name": model }))
        .send()
        .await
        .map_err(|e| ProviderError::Other(format!("ollama /api/show request: {e}")))?;
    if !resp.status().is_success() {
        return Err(ProviderError::Other(format!(
            "ollama /api/show returned {}",
            resp.status()
        )));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| ProviderError::Other(format!("ollama /api/show body: {e}")))?;
    let show: OllamaShow = serde_json::from_str(&text)
        .map_err(|e| ProviderError::Other(format!("ollama /api/show parse: {e}")))?;
    Ok(ProbedModel {
        id: model.to_string(),
        context_hard_limit: parse_ollama_num_ctx(&show.parameters),
        ..Default::default()
    })
}

/// Parse the served `num_ctx` from an ollama `/api/show` `parameters` blob.
/// The blob is whitespace-aligned `key   value` lines; we take the integer
/// after the `num_ctx` key. Absent -> `None`. Pure, so it is unit-testable.
pub fn parse_ollama_num_ctx(parameters: &str) -> Option<u64> {
    for line in parameters.lines() {
        let mut it = line.split_whitespace();
        if it.next() == Some("num_ctx") {
            if let Some(v) = it.next() {
                return v.parse::<u64>().ok();
            }
        }
    }
    None
}

#[derive(Debug, Deserialize)]
struct OllamaTags {
    #[serde(default)]
    models: Vec<OllamaTag>,
}

#[derive(Debug, Deserialize)]
struct OllamaTag {
    name: String,
}

#[derive(Debug, Deserialize)]
struct OllamaShow {
    /// Whitespace-aligned parameter lines (e.g. `num_ctx 8192`). Absent on
    /// models with no baked parameters -> empty.
    #[serde(default)]
    parameters: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openrouter_parses_limits_and_pricing() {
        let json = r#"{
          "data": [
            {
              "id": "anthropic/claude-opus-4.7",
              "context_length": 200000,
              "pricing": {"prompt": "0.000005", "completion": "0.000025"},
              "top_provider": {"max_completion_tokens": 64000, "context_length": 200000}
            },
            {
              "id": "free/model",
              "context_length": 131072,
              "pricing": {"prompt": "0", "completion": "0"}
            }
          ]
        }"#;
        let models = parse_openrouter_models(json).unwrap();
        assert_eq!(models.len(), 2);
        let opus = &models[0];
        assert_eq!(opus.id, "anthropic/claude-opus-4.7");
        assert_eq!(opus.context_hard_limit, Some(200_000));
        assert_eq!(opus.max_output_tokens, Some(64_000));
        // per-token 0.000005 USD -> 5.00 / Mtok
        assert_eq!(opus.pricing_input_per_mtok, Some(5.0));
        assert_eq!(opus.pricing_output_per_mtok, Some(25.0));
        // free model: 0 -> Some(0.0), no top_provider -> no max_output
        assert_eq!(models[1].pricing_input_per_mtok, Some(0.0));
        assert_eq!(models[1].max_output_tokens, None);
    }

    #[test]
    fn vllm_parses_max_model_len() {
        let json =
            r#"{"data":[{"id":"Qwen/Qwen3-Coder-30B-A3B-Instruct-FP8","max_model_len":32768}]}"#;
        let models = parse_vllm_models(json).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].context_hard_limit, Some(32_768));
        assert_eq!(
            models[0].pricing_input_per_mtok, None,
            "local => no pricing"
        );
    }

    #[test]
    fn ollama_num_ctx_parsed_from_baked_params() {
        let params = "num_ctx                        8192\nstop                           \"<|im_end|>\"\ntemperature                    0.7";
        assert_eq!(parse_ollama_num_ctx(params), Some(8192));
    }

    #[test]
    fn ollama_num_ctx_absent_is_none() {
        // No baked num_ctx: the served window is the server default, which
        // the API doesn't expose — None, never a fabricated placeholder.
        let params =
            "stop                           \"<|im_end|>\"\ntemperature                    0.7";
        assert_eq!(parse_ollama_num_ctx(params), None);
        assert_eq!(parse_ollama_num_ctx(""), None);
    }

    #[test]
    fn malformed_bodies_error_not_panic() {
        assert!(parse_openrouter_models("not json").is_err());
        assert!(parse_vllm_models("{").is_err());
    }
}
