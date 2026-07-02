//! Route catalog — the single source of truth for available
//! provider×model combinations and their properties.
//!
//! Built at daemon startup from environment probing (which API keys
//! are set?), hardcoded seed routes, `mu models sync` generated
//! provider layers, and explicit operator favorites. Queryable by
//! front ends via
//! `daemon.list_routes` RPC or `mu://routes/available` MCP resource.
//!
//! Each entry carries a blake3 hash so clients can send `set_route`
//! with the hash they selected from, and the daemon can reject stale
//! picks if the catalog changed between query and submit.

use std::path::Path;
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
    pub default_effort: Option<Arc<str>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_input_per_mtok: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_output_per_mtok: Option<f64>,

    pub hash: Arc<str>,
}

#[derive(Debug, Clone, Copy)]
struct ProviderAvailability {
    anthropic_key: bool,
    openai_key: bool,
    openrouter_key: bool,
    vllm_configured: bool,
}

impl ProviderAvailability {
    fn from_env() -> Self {
        Self {
            anthropic_key: env_nonempty("ANTHROPIC_API_KEY"),
            openai_key: env_nonempty("OPENAI_API_KEY"),
            openrouter_key: env_nonempty("OPENROUTER_API_KEY"),
            vllm_configured: env_nonempty("VLLM_API_BASE"),
        }
    }

    #[cfg(test)]
    fn all_configured() -> Self {
        Self {
            anthropic_key: true,
            openai_key: true,
            openrouter_key: true,
            vllm_configured: true,
        }
    }

    fn configured(self, provider_kind: &str) -> bool {
        match provider_kind {
            "anthropic_api" | "anthropic_oauth" => self.anthropic_key,
            "openai_codex" | "openai_api" => self.openai_key,
            "openrouter" => self.openrouter_key,
            "vllm" => self.vllm_configured,
            // Local providers have no credential bit. A generated ollama layer
            // means the operator deliberately synced/selected those tags; live
            // reachability is still checked by the provider when a turn runs.
            "ollama" | "faux" => true,
            _ => false,
        }
    }
}

fn env_nonempty(key: &str) -> bool {
    std::env::var(key).ok().is_some_and(|s| !s.is_empty())
}

#[derive(Debug, Clone)]
pub struct RouteCatalog {
    entries: Vec<RouteEntry>,
}

impl RouteCatalog {
    pub fn from_env() -> Self {
        let availability = ProviderAvailability::from_env();

        // mu-nzxa: resolve the enrichment catalog ONCE and thread it into
        // build_entry, rather than each entry reaching for global(). The
        // public path passes the process-global catalog; tests pass a
        // controlled one so a user's models.toml can't change asserted values.
        let catalog = model_catalog::global();
        let mut out = Self::from_catalog_with_availability(catalog, availability);

        // mu-generated-model-routes-uc3n: `mu models sync` writes provider-
        // scoped generated layers that `model_catalog::load()` already uses
        // for enrichment. Route discovery must consume the same layers as
        // route SOURCES too; otherwise the daemon/frontends know the limits for
        // synced models but still cannot select them.
        if let Some(p) = model_catalog::default_config_path() {
            out = out
                .with_generated_layers_for_config(catalog, &p, availability)
                .with_favorite_routes(catalog, availability);
        }
        out
    }

    fn from_catalog_with_availability(
        catalog: &model_catalog::ModelCatalogConfig,
        availability: ProviderAvailability,
    ) -> Self {
        let mut entries = Vec::new();

        for (model, soft, hard) in ANTHROPIC_MODELS {
            entries.push(build_entry(
                catalog,
                "anthropic_api",
                model,
                availability.configured("anthropic_api"),
                Some(*soft),
                Some(*hard),
            ));
        }

        for (model, soft, hard) in OPENAI_CODEX_MODELS {
            entries.push(build_entry(
                catalog,
                "openai_codex",
                model,
                availability.configured("openai_codex"),
                Some(*soft),
                Some(*hard),
            ));
        }

        for (model, soft, hard) in OPENROUTER_MODELS {
            entries.push(build_entry(
                catalog,
                "openrouter",
                model,
                availability.configured("openrouter"),
                Some(*soft),
                Some(*hard),
            ));
        }

        for (model, soft, hard) in VLLM_MODELS {
            entries.push(build_entry(
                catalog,
                "vllm",
                model,
                availability.configured("vllm"),
                Some(*soft),
                Some(*hard),
            ));
        }

        entries.push(build_entry(
            catalog,
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
    pub fn with_ollama_models<I, S>(self, models: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.with_ollama_models_using(model_catalog::global(), models)
    }

    /// [`with_ollama_models`] against an explicit enrichment catalog — the
    /// testable seam, so a unit test can supply the deterministic
    /// [`model_catalog::built_in`] instead of the operator's live config.
    /// The public method passes the process-global catalog. (bead mu-nzxa)
    pub fn with_ollama_models_using<I, S>(
        mut self,
        catalog: &model_catalog::ModelCatalogConfig,
        models: I,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for m in models {
            self.upsert_entry(build_entry(
                catalog,
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
        let catalog = model_catalog::global();
        for m in models {
            self.upsert_entry(build_entry(
                catalog,
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

    fn with_generated_layers_for_config(
        mut self,
        catalog: &model_catalog::ModelCatalogConfig,
        operator_config: &Path,
        availability: ProviderAvailability,
    ) -> Self {
        for path in model_catalog::generated_layers_for(operator_config) {
            let Some(provider_from_file) = generated_provider_from_path(&path) else {
                continue;
            };
            let provider_kind = canonical_provider_kind(catalog, &provider_from_file);
            let layer = model_catalog::load_operator_only(&path);
            self.add_model_entries_for_provider(
                catalog,
                &provider_kind,
                &layer.models,
                availability,
            );
        }
        self
    }

    fn with_favorite_routes(
        mut self,
        catalog: &model_catalog::ModelCatalogConfig,
        availability: ProviderAvailability,
    ) -> Self {
        for fav in catalog.favorites.values() {
            let provider_kind = canonical_provider_kind(catalog, &fav.provider);
            let model = catalog.resolve_model_name(&fav.model);
            if model.trim().is_empty() {
                continue;
            }
            self.upsert_entry(build_entry(
                catalog,
                &provider_kind,
                &model,
                availability.configured(&provider_kind),
                None,
                None,
            ));
        }
        self
    }

    fn add_model_entries_for_provider(
        &mut self,
        catalog: &model_catalog::ModelCatalogConfig,
        provider_kind: &str,
        models: &std::collections::BTreeMap<String, model_catalog::ModelCatalogEntry>,
        availability: ProviderAvailability,
    ) {
        for (key, entry) in models {
            let raw = entry.model.as_deref().unwrap_or(key.as_str());
            let model = catalog.resolve_model_name(raw);
            if model.trim().is_empty() {
                continue;
            }
            self.upsert_entry(build_entry(
                catalog,
                provider_kind,
                &model,
                availability.configured(provider_kind),
                None,
                None,
            ));
        }
    }

    fn upsert_entry(&mut self, entry: RouteEntry) {
        if let Some(existing) = self.entries.iter_mut().find(|e| {
            e.provider_kind.as_ref() == entry.provider_kind.as_ref()
                && e.model.as_ref() == entry.model.as_ref()
        }) {
            // A later source (generated layer / live discovery / favorite) can
            // prove an already-known route is configured. Keep the original
            // metadata, which was built against the same merged model catalog.
            existing.configured |= entry.configured;
            return;
        }
        self.entries.push(entry);
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

/// Resolve reasoning effort levels/default for a route. Operator/model catalog
/// values own the vocabulary; provider-kind fallbacks exist only when config
/// omits them (mu-vcbm slice 2).
pub fn effort_config_for(
    provider_kind: &str,
    model: &str,
) -> (Option<Vec<Arc<str>>>, Option<Arc<str>>) {
    let settings = model_catalog::global().resolve_model(model);
    effort_config(provider_kind, &settings)
}

fn effort_config(
    provider_kind: &str,
    model_settings: &model_catalog::ResolvedModelSettings,
) -> (Option<Vec<Arc<str>>>, Option<Arc<str>>) {
    let levels: Vec<Arc<str>> = if !model_settings.effort_levels.is_empty() {
        model_settings
            .effort_levels
            .iter()
            .map(|s| Arc::from(s.as_str()))
            .collect()
    } else {
        provider_effort_fallback(provider_kind)
            .into_iter()
            .map(Arc::from)
            .collect()
    };
    let levels = (!levels.is_empty()).then_some(levels);
    let default = model_settings
        .default_effort
        .as_deref()
        .map(Arc::from)
        .or_else(|| {
            levels
                .as_ref()
                .and_then(|l| provider_default_effort(provider_kind, l))
        });
    (levels, default)
}

fn provider_effort_fallback(provider_kind: &str) -> Vec<&'static str> {
    match provider_kind {
        // Anthropic's modern depth knob is output_config.effort; xhigh is
        // valid on Opus 4.7+ and was the missing stale value in the old route
        // catalog fallback.
        "anthropic_api" | "anthropic_oauth" => vec!["low", "medium", "high", "xhigh", "max"],
        // gpt-5.5 (the codex model in use) accepts none/low/medium/high/xhigh.
        // `minimal` was dropped — gpt-5.5 rejects it with a 400 (live-verified,
        // mu-53kt) — and `xhigh` added. Any other codex model sets its own set
        // via `[models.<label>]` (the slice-2 override path). `openai_api` — the
        // public-key Responses path (ProviderSelector::OpenaiApi -> gpt-5.5 over
        // api.openai.com/v1/responses) — shares gpt-5.5's effort vocabulary, and
        // the single OpenaiProvider already threads per-turn `effort` into
        // reasoning.effort for both wire kinds, so the same fallback applies
        // (mu-kaf8).
        "openai_codex" | "openai_api" => vec!["low", "medium", "high", "xhigh"],
        // Ollama Anthropic-compat exposes a thinking switch, not effort depth.
        "ollama" => vec!["off", "on"],
        _ => Vec::new(),
    }
}

fn provider_default_effort(provider_kind: &str, levels: &[Arc<str>]) -> Option<Arc<str>> {
    let preferred = match provider_kind {
        "anthropic_api" | "anthropic_oauth" | "openai_codex" | "openai_api" => "medium",
        "ollama" => "on",
        _ => return None,
    };
    levels
        .iter()
        .find(|l| l.as_ref() == preferred)
        .cloned()
        .or_else(|| levels.first().cloned())
}

fn generated_provider_from_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    Some(
        name.strip_prefix("models.generated.")?
            .strip_suffix(".toml")?
            .to_string(),
    )
}

fn canonical_provider_kind(catalog: &model_catalog::ModelCatalogConfig, provider: &str) -> String {
    catalog
        .provider(provider)
        .and_then(|p| p.kind.as_deref())
        .unwrap_or(provider)
        .to_string()
}

fn build_entry(
    catalog: &model_catalog::ModelCatalogConfig,
    provider_kind: &str,
    model: &str,
    configured: bool,
    context_soft: Option<u64>,
    context_hard: Option<u64>,
) -> RouteEntry {
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

    let (valid_effort, default_effort) = effort_config(provider_kind, &model_settings);

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
        default_effort,
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
            &model_catalog::built_in(),
            "anthropic_api",
            "claude-opus-4-7",
            true,
            Some(200_000),
            Some(1_000_000),
        );
        let b = build_entry(
            &model_catalog::built_in(),
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
            &model_catalog::built_in(),
            "anthropic_api",
            "claude-test-hash",
            true,
            Some(200_000),
            Some(1_000_000),
        );
        let b = build_entry(
            &model_catalog::built_in(),
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
        // Enrich against the deterministic built-in catalog, NOT the operator's
        // live models.toml — otherwise a user tuning their own qwen3.6
        // max_output_tokens breaks this test (bead mu-nzxa).
        let built_in = model_catalog::built_in();
        let catalog = RouteCatalog::from_catalog_with_availability(
            &built_in,
            ProviderAvailability::all_configured(),
        )
        .with_ollama_models_using(&built_in, ["qwen3.6:35b-a3b-q8_0"]);
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
        assert_eq!(q.default_effort.as_deref(), Some("on"));
        assert_eq!(
            q.valid_effort_levels
                .as_ref()
                .map(|levels| { levels.iter().map(|s| s.to_string()).collect::<Vec<_>>() }),
            Some(vec!["off".to_string(), "on".to_string()])
        );
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
    fn generated_layers_create_selectable_provider_routes() {
        let dir = std::env::temp_dir().join(format!("mu-route-gen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let op = dir.join("models.toml");
        std::fs::write(&op, "").unwrap();
        let gen = model_catalog::generated_path_for_provider(&op, "anthropic_api");
        std::fs::write(
            &gen,
            r#"
[models.claude-fable-5]
model = "claude-fable-5"
context_hard_limit = 1000000
max_output_tokens = 128000
effort_levels = ["low", "medium", "high", "xhigh", "max"]
"#,
        )
        .unwrap();

        let cfg = model_catalog::load(Some(&op));
        let catalog = RouteCatalog::from_catalog_with_availability(
            &cfg,
            ProviderAvailability::all_configured(),
        )
        .with_generated_layers_for_config(
            &cfg,
            &op,
            ProviderAvailability::all_configured(),
        );
        let fable = catalog
            .find("anthropic_api", "claude-fable-5")
            .expect("generated anthropic model should become a route");
        assert!(fable.configured);
        assert_eq!(fable.context_hard_limit, Some(1_000_000));
        assert_eq!(fable.max_output_tokens, Some(128_000));
        assert_eq!(
            fable
                .valid_effort_levels
                .as_ref()
                .map(|levels| levels.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
            Some(vec![
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
                "xhigh".to_string(),
                "max".to_string(),
            ])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generated_layers_do_not_duplicate_seed_routes() {
        let dir = std::env::temp_dir().join(format!("mu-route-dedupe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let op = dir.join("models.toml");
        std::fs::write(&op, "").unwrap();
        let gen = model_catalog::generated_path_for_provider(&op, "anthropic_api");
        std::fs::write(
            &gen,
            r#"
[models.claude-opus-4-8]
model = "claude-opus-4-8"
context_hard_limit = 1000000
max_output_tokens = 128000
"#,
        )
        .unwrap();

        let cfg = model_catalog::load(Some(&op));
        let catalog = RouteCatalog::from_catalog_with_availability(
            &cfg,
            ProviderAvailability::all_configured(),
        )
        .with_generated_layers_for_config(
            &cfg,
            &op,
            ProviderAvailability::all_configured(),
        );
        let matches = catalog
            .entries()
            .iter()
            .filter(|e| {
                e.provider_kind.as_ref() == "anthropic_api" && e.model.as_ref() == "claude-opus-4-8"
            })
            .count();
        assert_eq!(matches, 1, "generated layer should enrich, not duplicate");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn favorites_create_routes_for_operator_only_models() {
        let mut cfg = model_catalog::built_in();
        cfg.models.insert(
            "glm".into(),
            model_catalog::ModelCatalogEntry {
                model: Some("z-ai/glm-5.2".into()),
                context_hard_limit: Some(1_048_576),
                max_output_tokens: Some(32_768),
                ..Default::default()
            },
        );
        cfg.favorites.insert(
            "glm".into(),
            model_catalog::FavoriteConfig {
                provider: "openrouter".into(),
                model: "glm".into(),
                aliases: vec!["glm".into()],
                ..Default::default()
            },
        );

        let catalog = RouteCatalog::from_catalog_with_availability(
            &cfg,
            ProviderAvailability::all_configured(),
        )
        .with_favorite_routes(&cfg, ProviderAvailability::all_configured());
        let route = catalog
            .find("openrouter", "z-ai/glm-5.2")
            .expect("favorite should create route for its provider/model pair");
        assert!(route.configured);
        assert_eq!(route.context_hard_limit, Some(1_048_576));
        assert_eq!(route.max_output_tokens, Some(32_768));
        assert!(route.favorites.iter().any(|f| f.name.as_ref() == "glm"));
    }

    #[test]
    fn with_ollama_models_dedupes_generated_routes() {
        let mut cfg = model_catalog::built_in();
        cfg.models.insert(
            "local".into(),
            model_catalog::ModelCatalogEntry {
                model: Some("qwen3.6:35b-a3b-q8_0".into()),
                context_hard_limit: Some(262_144),
                max_output_tokens: Some(32_768),
                ..Default::default()
            },
        );
        let mut base = RouteCatalog::from_catalog_with_availability(
            &cfg,
            ProviderAvailability::all_configured(),
        );
        base.add_model_entries_for_provider(
            &cfg,
            "ollama",
            &cfg.models,
            ProviderAvailability::all_configured(),
        );
        let catalog = base.with_ollama_models_using(&cfg, ["qwen3.6:35b-a3b-q8_0"]);
        let matches = catalog
            .entries()
            .iter()
            .filter(|e| {
                e.provider_kind.as_ref() == "ollama" && e.model.as_ref() == "qwen3.6:35b-a3b-q8_0"
            })
            .count();
        assert_eq!(matches, 1);
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

#[cfg(test)]
mod vcbm_effort_tests {
    use crate::model_catalog::{ModelCatalogConfig, ModelCatalogEntry, ResolvedModelSettings};

    #[test]
    fn anthropic_fallback_includes_xhigh() {
        let (levels, default) =
            super::effort_config("anthropic_api", &ResolvedModelSettings::default());
        let levels: Vec<String> = levels.unwrap().iter().map(|s| s.to_string()).collect();
        assert_eq!(levels, vec!["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(default.as_deref(), Some("medium"));
    }

    #[test]
    fn codex_fallback_drops_minimal_adds_xhigh() {
        // mu-53kt: gpt-5.5 (the codex model in use) rejects `minimal` with a
        // 400 and supports `xhigh` (live-verified). The dial's fallback set
        // must match — no `minimal`, includes `xhigh`; default stays `medium`.
        let (levels, default) =
            super::effort_config("openai_codex", &ResolvedModelSettings::default());
        let levels: Vec<String> = levels.unwrap().iter().map(|s| s.to_string()).collect();
        assert_eq!(levels, vec!["low", "medium", "high", "xhigh"]);
        assert!(!levels.iter().any(|l| l == "minimal"));
        assert_eq!(default.as_deref(), Some("medium"));
    }

    #[test]
    fn openai_api_fallback_matches_codex() {
        // mu-kaf8: the public-key OpenAI provider (wire kind `openai_api`,
        // ProviderSelector::OpenaiApi -> gpt-5.5 over api.openai.com/v1/responses)
        // shares gpt-5.5's effort vocabulary with the codex path. Without an
        // `openai_api` arm the dial fell through to `_ => Vec::new()` and offered
        // nothing; the fallback must mirror codex — low/medium/high/xhigh, no
        // `minimal`, default `medium`.
        let (levels, default) =
            super::effort_config("openai_api", &ResolvedModelSettings::default());
        let levels: Vec<String> = levels.unwrap().iter().map(|s| s.to_string()).collect();
        assert_eq!(levels, vec!["low", "medium", "high", "xhigh"]);
        assert!(!levels.iter().any(|l| l == "minimal"));
        assert_eq!(default.as_deref(), Some("medium"));
    }

    #[test]
    fn model_catalog_effort_overrides_provider_fallback() {
        let mut cfg = ModelCatalogConfig::default();
        cfg.models.insert(
            "opus".into(),
            ModelCatalogEntry {
                model: Some("claude-opus-4-8".into()),
                effort_levels: vec!["low".into(), "max".into()],
                default_effort: Some("max".into()),
                ..Default::default()
            },
        );
        let entry = super::build_entry(&cfg, "anthropic_api", "claude-opus-4-8", true, None, None);
        let levels: Vec<String> = entry
            .valid_effort_levels
            .unwrap()
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(levels, vec!["low", "max"]);
        assert_eq!(entry.default_effort.as_deref(), Some("max"));
    }

    #[test]
    fn synced_generated_layer_drives_per_model_effort_end_to_end() {
        // mu-lcck: regression guard for the mu-ggb3 -> session integration. A
        // generated anthropic layer (exactly as `mu models sync` writes it) must
        // drive the resolved effort levels through the REAL
        // load -> resolve -> effort_config path that the mu-solo `/effort` dial
        // uses (via effort_config_for). Locks the mu-53kt distinction
        // end-to-end: opus-4-8 keeps xhigh, sonnet-4-6 must never gain it.
        use crate::catalog_sync::{
            build_generated_entries_all, operator_selection, write_generated_provider, ProbedModel,
        };
        use crate::model_catalog;

        let dir = std::env::temp_dir().join(format!("mu-effort-integ-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let op = dir.join("models.toml");
        // Operator references opus by table key (merge target); sonnet + haiku
        // unreferenced (id-keyed) — exercises both full-catalog write paths.
        std::fs::write(&op, "[models.opus]\nmodel = \"claude-opus-4-8\"\n").unwrap();

        let effort = |ls: &[&str]| ls.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let probed = vec![
            ProbedModel {
                id: "claude-opus-4-8".into(),
                effort_levels: effort(&["low", "medium", "high", "xhigh", "max"]),
                ..Default::default()
            },
            ProbedModel {
                id: "claude-sonnet-4-6".into(),
                effort_levels: effort(&["low", "medium", "high", "max"]),
                ..Default::default()
            },
            // Anthropic reports no effort surface -> omitted in the generated layer.
            ProbedModel {
                id: "claude-haiku-4-5-20251001".into(),
                effort_levels: Vec::new(),
                ..Default::default()
            },
        ];
        let sel = operator_selection(&model_catalog::load_operator_only(&op));
        let entries = build_generated_entries_all(&probed, &sel);
        write_generated_provider(&op, "anthropic_api", &entries).unwrap();

        let cfg = model_catalog::load(Some(&op));
        let levels_for = |model: &str| -> Vec<String> {
            super::effort_config("anthropic_api", &cfg.resolve_model(model))
                .0
                .unwrap_or_default()
                .iter()
                .map(|s| s.to_string())
                .collect()
        };

        // opus via the operator-key merge -> keeps xhigh.
        assert_eq!(
            levels_for("claude-opus-4-8"),
            vec!["low", "medium", "high", "xhigh", "max"]
        );
        // sonnet via the id-keyed entry -> xhigh stays ABSENT (the mu-53kt fix,
        // now flowing from the synced catalog all the way to the dial).
        let sonnet = levels_for("claude-sonnet-4-6");
        assert_eq!(sonnet, vec!["low", "medium", "high", "max"]);
        assert!(!sonnet.iter().any(|l| l == "xhigh"));

        // haiku has no effort surface: today the omitted effort_levels means the
        // provider fallback applies (the empty-vs-none gap, bead mu-xwbe). Pinned
        // as CURRENT behavior so its eventual fix updates this assertion on purpose.
        assert_eq!(
            levels_for("claude-haiku-4-5-20251001"),
            vec!["low", "medium", "high", "xhigh", "max"],
            "documents mu-xwbe: a no-effort model currently inherits the provider fallback"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
