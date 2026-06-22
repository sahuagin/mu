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
            // openrouter's catalog has no machine-readable effort surface in
            // mu's request shape — left empty (mu-ggb3).
            effort_levels: Vec::new(),
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
// anthropic: GET /v1/models (paginated list) + GET /v1/models/{id} (caps)
// ---------------------------------------------------------------------------
//
// Anthropic splits the facts across two endpoints: the paginated list gives
// `id` + `max_input_tokens` (context window) + `max_tokens` (output cap), and
// the per-model retrieve adds the `capabilities` tree — including
// `effort.<level>.supported`, the machine-readable per-model reasoning-effort
// set (opus-4-8 has `xhigh`, sonnet-4-6 does not). So the probe is N+1: one
// list (paginated) + one retrieve per model. All FREE metadata — no
// `/v1/messages`, no tokens billed.
//
// Limits come from the LIST (it matched the published caps); the retrieve is
// mined only for `effort`. The retrieve's `max_tokens` was observed to disagree
// with the list (sonnet-4-6: list 64k vs retrieve 128k), so we trust the list
// for limits and only `warn!` on the discrepancy rather than pick the odd one.

const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Canonical reasoning-effort levels in dial order. The Models API reports each
/// as `capabilities.effort.<level>.supported`; we keep the supported ones in
/// this order. A level Anthropic adds later is picked up by appending here.
const ANTHROPIC_EFFORT_ORDER: &[&str] = &["low", "medium", "high", "xhigh", "max"];

async fn anthropic_get_text(
    cl: &reqwest::Client,
    url: &str,
    api_key: &str,
    what: &str,
) -> Result<String, ProviderError> {
    let resp = cl
        .get(url)
        .header("x-api-key", api_key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .send()
        .await
        .map_err(|e| ProviderError::Other(format!("anthropic {what} request: {e}")))?;
    if !resp.status().is_success() {
        return Err(ProviderError::Other(format!(
            "anthropic {what} returned {}",
            resp.status()
        )));
    }
    resp.text()
        .await
        .map_err(|e| ProviderError::Other(format!("anthropic {what} body: {e}")))
}

/// Enumerate every model the API key's account can see, following pagination.
/// Returns list entries (id + limits) only — no per-model capability retrieve.
/// Backs `mu models list anthropic` and the first half of [`probe_anthropic`].
pub async fn list_anthropic(
    base: &str,
    api_key: &str,
    timeout: Duration,
) -> Result<Vec<AnthropicListEntry>, ProviderError> {
    let base = base.trim_end_matches('/');
    let cl = client(timeout)?;
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let mut url = format!("{base}/v1/models?limit=100");
        if let Some(a) = &after {
            url.push_str("&after_id=");
            url.push_str(a);
        }
        let text = anthropic_get_text(&cl, &url, api_key, "/v1/models").await?;
        let mut page = parse_anthropic_list(&text)?;
        let has_more = page.has_more;
        after = page.last_id.take();
        out.append(&mut page.data);
        if !has_more || after.is_none() {
            break;
        }
    }
    Ok(out)
}

/// Probe Anthropic's full account catalog: list every model, then retrieve each
/// model's capability tree for its supported effort levels. N+1 free metadata
/// calls. A per-model retrieve failure degrades gracefully — the model is still
/// emitted with its list limits and an empty effort set (warn, not fatal).
pub async fn probe_anthropic(
    base: &str,
    api_key: &str,
    timeout: Duration,
) -> Result<Vec<ProbedModel>, ProviderError> {
    let base = base.trim_end_matches('/');
    let cl = client(timeout)?;
    let listed = list_anthropic(base, api_key, timeout).await?;
    let mut out = Vec::with_capacity(listed.len());
    for entry in listed {
        let url = format!("{base}/v1/models/{}", entry.id);
        let mut effort = Vec::new();
        match anthropic_get_text(&cl, &url, api_key, "model retrieve").await {
            Ok(text) => {
                match parse_anthropic_effort_levels(&text) {
                    Ok(levels) => effort = levels,
                    Err(e) => {
                        tracing::warn!(model = %entry.id, error = %e, "anthropic effort parse failed; none")
                    }
                }
                if let (Some(detail_max), Some(list_max)) =
                    (parse_anthropic_max_tokens(&text), entry.max_tokens)
                {
                    if detail_max != list_max {
                        tracing::warn!(
                            model = %entry.id, list = list_max, retrieve = detail_max,
                            "anthropic max_tokens differs between list and retrieve; trusting list"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(model = %entry.id, error = %e, "anthropic model retrieve skipped; no effort levels")
            }
        }
        out.push(ProbedModel {
            id: entry.id,
            context_hard_limit: entry.max_input_tokens,
            max_output_tokens: entry.max_tokens,
            effort_levels: effort,
            ..Default::default()
        });
    }
    Ok(out)
}

/// One entry from the Anthropic `/v1/models` list: id + the limits the list
/// reports (the per-model retrieve adds capabilities). Public so the sync's
/// `list` path can print them.
#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicListEntry {
    pub id: String,
    #[serde(default)]
    pub max_input_tokens: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// One page of the paginated `/v1/models` list.
#[derive(Debug, Deserialize)]
pub struct AnthropicModelsPage {
    #[serde(default)]
    pub data: Vec<AnthropicListEntry>,
    #[serde(default)]
    pub has_more: bool,
    #[serde(default)]
    pub last_id: Option<String>,
}

/// Pure parser for an Anthropic `/v1/models` list page (no I/O).
pub fn parse_anthropic_list(json: &str) -> Result<AnthropicModelsPage, ProviderError> {
    serde_json::from_str(json)
        .map_err(|e| ProviderError::Other(format!("anthropic /v1/models parse: {e}")))
}

/// Extract supported reasoning-effort levels from a per-model
/// `GET /v1/models/{id}` body. Walks `capabilities.effort`: `supported:false`
/// (or absent) → no effort dial (empty); otherwise the levels in
/// [`ANTHROPIC_EFFORT_ORDER`] whose `<level>.supported` is true. Pure +
/// unit-tested against the live capability shape.
pub fn parse_anthropic_effort_levels(detail_json: &str) -> Result<Vec<String>, ProviderError> {
    let v: serde_json::Value = serde_json::from_str(detail_json)
        .map_err(|e| ProviderError::Other(format!("anthropic model detail parse: {e}")))?;
    let effort = &v["capabilities"]["effort"];
    if effort.get("supported").and_then(serde_json::Value::as_bool) != Some(true) {
        return Ok(Vec::new());
    }
    Ok(ANTHROPIC_EFFORT_ORDER
        .iter()
        .copied()
        .filter(|lvl| {
            effort
                .get(*lvl)
                .and_then(|o| o.get("supported"))
                .and_then(serde_json::Value::as_bool)
                == Some(true)
        })
        .map(str::to_string)
        .collect())
}

/// `max_tokens` from a per-model detail body, for the list-vs-retrieve
/// discrepancy warning only. `None` if absent/unparseable.
fn parse_anthropic_max_tokens(detail_json: &str) -> Option<u32> {
    serde_json::from_str::<serde_json::Value>(detail_json)
        .ok()
        .and_then(|v| v["max_tokens"].as_u64())
        .and_then(|n| u32::try_from(n).ok())
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
        assert!(parse_anthropic_list("nope").is_err());
        assert!(parse_anthropic_effort_levels("{").is_err());
    }

    #[test]
    fn anthropic_list_parses_ids_limits_and_pagination() {
        let json = r#"{
          "data": [
            {"id":"claude-opus-4-8","display_name":"Claude Opus 4.8","max_input_tokens":1000000,"max_tokens":128000},
            {"id":"claude-haiku-4-5-20251001","display_name":"Claude Haiku 4.5","max_input_tokens":200000,"max_tokens":64000}
          ],
          "has_more": false,
          "first_id": "claude-opus-4-8",
          "last_id": "claude-haiku-4-5-20251001"
        }"#;
        let page = parse_anthropic_list(json).unwrap();
        assert_eq!(page.data.len(), 2);
        assert!(!page.has_more);
        assert_eq!(page.last_id.as_deref(), Some("claude-haiku-4-5-20251001"));
        assert_eq!(page.data[0].id, "claude-opus-4-8");
        assert_eq!(page.data[0].max_input_tokens, Some(1_000_000));
        assert_eq!(page.data[0].max_tokens, Some(128_000));
        assert_eq!(page.data[1].max_input_tokens, Some(200_000));
    }

    // Captured from the live `GET /v1/models/claude-opus-4-8` capability tree.
    const OPUS_DETAIL: &str = r#"{
      "type":"model","id":"claude-opus-4-8","max_input_tokens":1000000,"max_tokens":128000,
      "capabilities":{
        "effort":{"supported":true,"low":{"supported":true},"medium":{"supported":true},
                  "high":{"supported":true},"xhigh":{"supported":true},"max":{"supported":true}},
        "thinking":{"supported":true,"types":{"enabled":{"supported":false},"adaptive":{"supported":true}}}
      }
    }"#;

    #[test]
    fn anthropic_effort_opus_includes_xhigh_in_order() {
        let levels = parse_anthropic_effort_levels(OPUS_DETAIL).unwrap();
        assert_eq!(levels, vec!["low", "medium", "high", "xhigh", "max"]);
    }

    #[test]
    fn anthropic_effort_sonnet_excludes_xhigh() {
        // sonnet-4-6 reports xhigh.supported=false — the exact per-model
        // distinction the route_catalog fallback got wrong (mu-53kt).
        let json = r#"{"capabilities":{"effort":{"supported":true,
          "low":{"supported":true},"medium":{"supported":true},"high":{"supported":true},
          "xhigh":{"supported":false},"max":{"supported":true}}}}"#;
        let levels = parse_anthropic_effort_levels(json).unwrap();
        assert_eq!(levels, vec!["low", "medium", "high", "max"]);
        assert!(!levels.iter().any(|l| l == "xhigh"));
    }

    #[test]
    fn anthropic_effort_unsupported_or_absent_is_empty() {
        // effort not supported at all (e.g. an older Sonnet/Haiku) -> empty,
        // so the model falls back to the route_catalog provider default rather
        // than getting a fabricated set written under it.
        let unsup = r#"{"capabilities":{"effort":{"supported":false}}}"#;
        assert!(parse_anthropic_effort_levels(unsup).unwrap().is_empty());
        // no capabilities block at all -> empty, no panic.
        assert!(parse_anthropic_effort_levels(r#"{"id":"x"}"#)
            .unwrap()
            .is_empty());
    }
}
