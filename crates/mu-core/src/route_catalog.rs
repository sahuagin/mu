//! Route catalog — the single source of truth for available
//! provider×model combinations and their properties.
//!
//! Built at daemon startup from environment probing (which API keys
//! are set?) and hardcoded model lists. Queryable by front ends via
//! `daemon.list_routes` RPC or `mu://routes/available` MCP resource.
//!
//! Each entry carries a blake3 hash so clients can send `set_route`
//! with the hash they selected from, and the daemon can reject stale
//! picks if the catalog changed between query and submit.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{model_catalog, pricing};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteFavorite {
    pub name: Arc<str>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<Arc<str>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<Arc<str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<Arc<str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Arc<str>>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteEntry {
    pub provider_kind: Arc<str>,
    pub model: Arc<str>,
    pub configured: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_label: Option<Arc<str>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_aliases: Vec<Arc<str>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_quirks: Vec<Arc<str>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<Arc<str>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<Arc<str>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quirks: Vec<Arc<str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub favorites: Vec<RouteFavorite>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_soft_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_hard_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_effort_levels: Option<Vec<Arc<str>>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_input_per_mtok: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_output_per_mtok: Option<f64>,

    pub hash: Arc<str>,
}

#[derive(Debug, Clone)]
pub struct RouteCatalog {
    entries: Vec<RouteEntry>,
}

impl RouteCatalog {
    pub fn from_env() -> Self {
        let anthropic_key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .is_some();
        let openai_key = std::env::var("OPENAI_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .is_some();
        let openrouter_key = std::env::var("OPENROUTER_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .is_some();
        let vllm_configured = std::env::var("VLLM_API_BASE")
            .ok()
            .filter(|s| !s.is_empty())
            .is_some();

        let mut entries = Vec::new();

        for (model, soft, hard) in ANTHROPIC_MODELS {
            entries.push(build_entry(
                "anthropic_api",
                model,
                anthropic_key,
                Some(*soft),
                Some(*hard),
            ));
        }

        for (model, soft, hard) in OPENAI_CODEX_MODELS {
            entries.push(build_entry(
                "openai_codex",
                model,
                openai_key,
                Some(*soft),
                Some(*hard),
            ));
        }

        for (model, soft, hard) in OPENROUTER_MODELS {
            entries.push(build_entry(
                "openrouter",
                model,
                openrouter_key,
                Some(*soft),
                Some(*hard),
            ));
        }

        for (model, soft, hard) in VLLM_MODELS {
            entries.push(build_entry(
                "vllm",
                model,
                vllm_configured,
                Some(*soft),
                Some(*hard),
            ));
        }

        entries.push(build_entry(
            "faux",
            "faux",
            true,
            Some(128_000),
            Some(128_000),
        ));

        Self { entries }
    }

    /// Merge dynamically-discovered ollama models into the catalog.
    ///
    /// HTTP-free by design: `mu-core` has no HTTP client, so the caller
    /// (the daemon's startup probe in `serve::run`) fetches model names
    /// via `mu_ai::OllamaProvider::discover_models` and passes them in
    /// here. `configured = true` for every entry, because the names
    /// only exist if the endpoint answered `/api/tags`. Context limits
    /// are left `None` (unknown) — `/api/tags` doesn't report windows and
    /// a fabricated placeholder must never drive compaction. A per-model
    /// `models.toml` entry or the catalog sync tool supplies the real
    /// window; until then an unknown window falls back to the safe
    /// `DEFAULT_COMPACTION_THRESHOLD` for compaction. Pricing is `None`
    /// (local = free). (bead mu-818c; context-limit-harden-sync)
    pub fn with_ollama_models<I, S>(mut self, models: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for m in models {
            self.entries.push(build_entry(
                "ollama",
                m.as_ref(),
                true,
                // Unknown window: /api/tags reports none. Leave it None
                // (not a fabricated placeholder) so it can't drive
                // compaction; a per-model models.toml entry or the catalog
                // sync tool fills it in. Until then compaction uses the
                // safe DEFAULT, and the meter shows no fake denominator.
                None,
                None,
            ));
        }
        self
    }

    /// Merge dynamically-discovered vLLM models into the catalog.
    /// vLLM exposes OpenAI-compatible `/v1/models`; the daemon probes it
    /// at startup and passes the returned ids here. Context limits are
    /// left `None` (unknown) until a `models.toml` entry or the catalog
    /// sync tool supplies them — see `with_ollama_models`.
    pub fn with_vllm_models<I, S>(mut self, models: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for m in models {
            self.entries.push(build_entry(
                "vllm",
                m.as_ref(),
                true,
                // Unknown window (see with_ollama_models) — None, not a
                // fabricated placeholder.
                None,
                None,
            ));
        }
        self
    }

    pub fn entries(&self) -> &[RouteEntry] {
        &self.entries
    }

    pub fn find_by_hash(&self, hash: &str) -> Option<&RouteEntry> {
        self.entries.iter().find(|e| e.hash.as_ref() == hash)
    }

    pub fn find(&self, provider_kind: &str, model: &str) -> Option<&RouteEntry> {
        self.entries
            .iter()
            .find(|e| e.provider_kind.as_ref() == provider_kind && e.model.as_ref() == model)
    }

    pub fn configured_entries(&self) -> impl Iterator<Item = &RouteEntry> {
        self.entries.iter().filter(|e| e.configured)
    }
}

fn build_entry(
    provider_kind: &str,
    model: &str,
    configured: bool,
    context_soft: Option<u64>,
    context_hard: Option<u64>,
) -> RouteEntry {
    let catalog = model_catalog::global();
    let pricing = pricing::for_model(provider_kind, model);
    let model_settings = catalog.resolve_model(model);
    let provider_settings = catalog.provider(provider_kind);

    // The per-model catalog value wins; otherwise the caller's fallback.
    // Discovered-but-unenriched providers (ollama/vllm) pass `None` so an
    // unknown context window stays `None` rather than a fabricated small
    // placeholder — a placeholder must never drive compaction (it did:
    // ollama's 32k placeholder made the agent loop compact every turn).
    // Unknown -> None -> the compaction trigger falls back to the safe
    // DEFAULT_COMPACTION_THRESHOLD instead. (bead context-limit-harden-sync)
    let context_soft = model_settings.context_soft_limit.or(context_soft);
    let context_hard = model_settings.context_hard_limit.or(context_hard);
    let favorites = catalog
        .favorites_for(provider_kind, model)
        .into_iter()
        .map(|(name, fav)| RouteFavorite {
            name: Arc::from(name),
            label: fav.label.clone().map(Arc::from),
            aliases: fav.aliases.iter().cloned().map(Arc::from).collect(),
            default_effort: fav.default_effort.clone().map(Arc::from),
            tools: fav
                .tools
                .clone()
                .map(|tools| tools.into_iter().map(Arc::from).collect()),
        })
        .collect();

    let valid_effort = match provider_kind {
        "anthropic_api" | "anthropic_oauth" => Some(vec![
            Arc::from("low"),
            Arc::from("medium"),
            Arc::from("high"),
            Arc::from("max"),
        ]),
        "openai_codex" => Some(vec![
            Arc::from("minimal"),
            Arc::from("low"),
            Arc::from("medium"),
            Arc::from("high"),
        ]),
        _ => None,
    };

    let hash_input = format!(
        "{provider_kind}:{model}:{}:{}",
        context_soft.unwrap_or(0),
        context_hard.unwrap_or(0)
    );
    let hash: Arc<str> = Arc::from(
        blake3::hash(hash_input.as_bytes())
            .to_hex()
            .as_str()
            .get(..16)
            .unwrap_or(""),
    );

    RouteEntry {
        provider_kind: Arc::from(provider_kind),
        model: Arc::from(model),
        configured,
        provider_label: provider_settings
            .and_then(|p| p.label.clone())
            .map(Arc::from),
        provider_aliases: provider_settings
            .map(|p| p.aliases.iter().cloned().map(Arc::from).collect())
            .unwrap_or_default(),
        provider_quirks: provider_settings
            .map(|p| p.quirks.iter().cloned().map(Arc::from).collect())
            .unwrap_or_default(),
        label: model_settings.label.map(Arc::from),
        aliases: model_settings.aliases.into_iter().map(Arc::from).collect(),
        quirks: model_settings.quirks.into_iter().map(Arc::from).collect(),
        max_output_tokens: model_settings.max_output_tokens,
        favorites,
        context_soft_limit: context_soft,
        context_hard_limit: context_hard,
        valid_effort_levels: valid_effort,
        pricing_input_per_mtok: pricing.map(|p| p.input_per_mtok),
        pricing_output_per_mtok: pricing.map(|p| p.output_per_mtok),
        hash,
    }
}

// (model, soft_limit, hard_limit)
const ANTHROPIC_MODELS: &[(&str, u64, u64)] = &[
    ("claude-opus-4-8", 200_000, 1_000_000),
    ("claude-opus-4-7", 200_000, 1_000_000),
    ("claude-sonnet-4-6", 200_000, 200_000),
    ("claude-haiku-4-5", 200_000, 200_000),
];

const OPENAI_CODEX_MODELS: &[(&str, u64, u64)] = &[("gpt-5.5", 1_000_000, 1_000_000)];

const OPENROUTER_MODELS: &[(&str, u64, u64)] = &[
    ("anthropic/claude-opus-4.7", 200_000, 1_000_000),
    ("anthropic/claude-sonnet-4.6", 200_000, 200_000),
    ("anthropic/claude-haiku-4-5", 200_000, 200_000),
    ("x-ai/grok-3", 131_072, 131_072),
    ("google/gemini-2.5-pro", 1_000_000, 1_000_000),
];

const VLLM_MODELS: &[(&str, u64, u64)] =
    &[("Qwen/Qwen3-Coder-30B-A3B-Instruct-FP8", 32_768, 32_768)];

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn faux_always_configured() {
        let catalog = RouteCatalog::from_env();
        let faux = catalog.find("faux", "faux").expect("faux must exist");
        assert!(faux.configured);
    }

    #[test]
    fn hash_is_deterministic() {
        let a = build_entry(
            "anthropic_api",
            "claude-opus-4-7",
            true,
            Some(200_000),
            Some(1_000_000),
        );
        let b = build_entry(
            "anthropic_api",
            "claude-opus-4-7",
            true,
            Some(200_000),
            Some(1_000_000),
        );
        assert_eq!(a.hash, b.hash);
    }

    #[test]
    fn hash_changes_with_limits() {
        let a = build_entry(
            "anthropic_api",
            "claude-test-hash",
            true,
            Some(200_000),
            Some(1_000_000),
        );
        let b = build_entry(
            "anthropic_api",
            "claude-test-hash",
            true,
            Some(500_000),
            Some(1_000_000),
        );
        assert_ne!(a.hash, b.hash);
    }

    #[test]
    fn find_by_hash_works() {
        let catalog = RouteCatalog::from_env();
        let entry = catalog.find("faux", "faux").unwrap();
        let found = catalog.find_by_hash(&entry.hash).unwrap();
        assert_eq!(found.model, entry.model);
    }

    #[test]
    fn pricing_populated_for_known_models() {
        let catalog = RouteCatalog::from_env();
        if let Some(opus) = catalog.find("anthropic_api", "claude-opus-4-7") {
            assert_eq!(opus.pricing_input_per_mtok, Some(5.00));
            assert_eq!(opus.pricing_output_per_mtok, Some(25.00));
        }
    }

    #[test]
    fn configured_reflects_env() {
        let catalog = RouteCatalog::from_env();
        let faux = catalog.find("faux", "faux").unwrap();
        assert!(faux.configured, "faux is always configured");
    }

    #[test]
    fn with_ollama_models_merges_configured_entries() {
        let catalog =
            RouteCatalog::from_env().with_ollama_models(["qwen3-coder:30b", "deepseek-r1:32b"]);
        let q = catalog
            .find("ollama", "qwen3-coder:30b")
            .expect("ollama model should be present");
        assert!(q.configured, "discovered ollama models are configured");
        // Unknown window: discovered but not in the catalog and no longer
        // fabricated — stays None so it can't drive compaction.
        assert_eq!(q.context_soft_limit, None);
        assert_eq!(q.context_hard_limit, None);
        // Local inference: no pricing.
        assert_eq!(q.pricing_input_per_mtok, None);
        assert_eq!(q.pricing_output_per_mtok, None);
        assert!(catalog.find("ollama", "deepseek-r1:32b").is_some());
    }

    #[test]
    fn ollama_qwen36_route_is_catalog_enriched() {
        let catalog = RouteCatalog::from_env().with_ollama_models(["qwen3.6:35b-a3b-q8_0"]);
        let q = catalog
            .find("ollama", "qwen3.6:35b-a3b-q8_0")
            .expect("discovered qwen3.6 route should be present");
        assert_eq!(q.max_output_tokens, Some(16384));
        assert!(q
            .quirks
            .iter()
            .any(|q| q.as_ref() == "thinking_counts_against_max_tokens"));
        assert!(q
            .favorites
            .iter()
            .any(|f| f.name.as_ref() == "local_qwen36"));
    }

    #[test]
    fn vllm_qwen_route_is_cataloged_when_base_configured() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("VLLM_API_BASE").ok();
        std::env::set_var("VLLM_API_BASE", "http://rtx4000:8000");
        let catalog = RouteCatalog::from_env();
        let q = catalog
            .find("vllm", "Qwen/Qwen3-Coder-30B-A3B-Instruct-FP8")
            .expect("built-in vllm qwen route should be present");
        assert!(q.configured);
        assert_eq!(q.context_hard_limit, Some(32_768));
        match prev {
            Some(v) => std::env::set_var("VLLM_API_BASE", v),
            None => std::env::remove_var("VLLM_API_BASE"),
        }
    }

    #[test]
    fn with_ollama_models_empty_is_noop() {
        let base = RouteCatalog::from_env().entries().len();
        let merged = RouteCatalog::from_env()
            .with_ollama_models(Vec::<String>::new())
            .entries()
            .len();
        assert_eq!(base, merged);
    }
}
