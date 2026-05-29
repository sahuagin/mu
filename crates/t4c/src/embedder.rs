//! Embeddings for the semantic ranker (mu-d2iy.4).
//!
//! [`Embedder`] is the pluggable seam. [`FakeEmbedder`] is deterministic — for
//! tests, CI, and offline runs — so the ranker and benchmark are reproducible
//! without a network or a model. [`ConfigEmbedder`] hits a real embedding
//! endpoint using the centralized key store. The catalog is tiny (dozens of
//! entries), so the caller embeds it once at `discover` and caches the vectors;
//! query-time is a single embed + brute-force cosine. No vector DB.
//!
//! NOTE (mu-d2iy.6 gate): `~/.config/agent/config.toml` holds provider keys
//! (`[openrouter]`, `[anthropic]`) but NOT an embedding endpoint/model — the
//! recall tooling constructs those itself. So `ConfigEmbedder` reads the key from
//! config and takes endpoint+model from env-or-default; whether that endpoint
//! actually serves `qwen3-embedding-8b` is the live question the gate answers.

use anyhow::{Context, Result};

/// Produce one embedding vector per input text. Batch is the primitive — the
/// whole catalog embeds in one call at discover time.
pub trait Embedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// Cosine similarity of two vectors. Returns 0.0 if either is a zero vector or
/// lengths differ.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// Deterministic, offline embedder: a hashing-trick bag-of-words into a
/// fixed-dimension vector. NOT semantic — shared tokens raise cosine — but stable,
/// which is exactly what CI and the benchmark need. Real semantic signal comes
/// from [`ConfigEmbedder`], validated at the gate.
pub struct FakeEmbedder {
    dim: usize,
}

impl FakeEmbedder {
    pub fn new() -> Self {
        Self { dim: 64 }
    }

    fn embed_one(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; self.dim];
        for tok in text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
        {
            // FNV-1a over the lowercased token -> bucket.
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in tok.to_lowercase().bytes() {
                h ^= b as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
            let idx = (h % self.dim as u64) as usize;
            v[idx] += 1.0;
        }
        v
    }
}

impl Default for FakeEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

impl Embedder for FakeEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.embed_one(t)).collect())
    }
}

/// Real embedder against an OpenAI-style `/embeddings` endpoint. Key from the
/// centralized config (`[<provider>].api_key`); endpoint + model from env or
/// defaults. Network path — end-to-end validation is the mu-d2iy.6 gate.
pub struct ConfigEmbedder {
    endpoint: String,
    api_key: String,
    model: String,
}

impl ConfigEmbedder {
    /// Default OpenRouter embeddings endpoint (overridable via `$T4C_EMBED_ENDPOINT`).
    pub const DEFAULT_ENDPOINT: &'static str = "https://openrouter.ai/api/v1/embeddings";
    /// Default model (overridable via `$T4C_EMBED_MODEL`) — what `agent memory recall` uses.
    pub const DEFAULT_MODEL: &'static str = "qwen/qwen3-embedding-8b";

    /// Build from the centralized agent config + env. Returns `Err` when the
    /// config/key is missing, so callers can fall back to [`FakeEmbedder`].
    pub fn from_config() -> Result<Self> {
        let home = std::env::var("HOME").context("HOME unset")?;
        let path = std::env::var("T4C_AGENT_CONFIG")
            .unwrap_or_else(|_| format!("{home}/.config/agent/config.toml"));
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading agent config {path}"))?;
        let provider =
            std::env::var("T4C_EMBED_PROVIDER").unwrap_or_else(|_| "openrouter".to_string());
        let api_key = Self::api_key_from(&text, &provider)?;
        Ok(Self {
            endpoint: std::env::var("T4C_EMBED_ENDPOINT")
                .unwrap_or_else(|_| Self::DEFAULT_ENDPOINT.to_string()),
            api_key,
            model: std::env::var("T4C_EMBED_MODEL")
                .unwrap_or_else(|_| Self::DEFAULT_MODEL.to_string()),
        })
    }

    /// Extract `[<provider>].api_key` from the agent config TOML. Split out so the
    /// key-resolution is testable without I/O.
    pub fn api_key_from(text: &str, provider: &str) -> Result<String> {
        let value: toml::Value = toml::from_str(text).context("parsing agent config TOML")?;
        value
            .get(provider)
            .and_then(|t| t.get("api_key"))
            .and_then(|k| k.as_str())
            .map(str::to_string)
            .with_context(|| format!("no [{provider}].api_key in agent config"))
    }
}

impl Embedder for ConfigEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let client = reqwest::blocking::Client::new();
        let body = serde_json::json!({ "model": self.model, "input": texts });
        let resp = client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .context("embedding request failed")?
            .error_for_status()
            .context("embedding endpoint returned an error status")?;
        #[derive(serde::Deserialize)]
        struct EmbResp {
            data: Vec<EmbData>,
        }
        #[derive(serde::Deserialize)]
        struct EmbData {
            embedding: Vec<f32>,
        }
        let parsed: EmbResp = resp.json().context("parsing embedding response")?;
        Ok(parsed.data.into_iter().map(|d| d.embedding).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_basics() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
        assert_eq!(cosine(&[1.0], &[1.0, 1.0]), 0.0); // length mismatch
    }

    #[test]
    fn fake_embedder_is_deterministic_and_shared_tokens_raise_cosine() {
        let e = FakeEmbedder::new();
        let v = e
            .embed(&[
                "semantic code search".to_string(),
                "search code symbols".to_string(),
                "compress an archive".to_string(),
            ])
            .unwrap();
        // determinism
        let again = e.embed(&["semantic code search".to_string()]).unwrap();
        assert_eq!(v[0], again[0]);
        // shared tokens ("code"/"search") -> higher cosine than the disjoint text
        let near = cosine(&v[0], &v[1]);
        let far = cosine(&v[0], &v[2]);
        assert!(near > far, "near={near} should exceed far={far}");
    }

    #[test]
    fn config_embedder_reads_provider_key() {
        let text = "[openrouter]\napi_key = \"sk-test-123\"\n[anthropic]\napi_key = \"sk-ant\"\n";
        assert_eq!(
            ConfigEmbedder::api_key_from(text, "openrouter").unwrap(),
            "sk-test-123"
        );
        assert_eq!(
            ConfigEmbedder::api_key_from(text, "anthropic").unwrap(),
            "sk-ant"
        );
        assert!(ConfigEmbedder::api_key_from(text, "nope").is_err());
    }
}
