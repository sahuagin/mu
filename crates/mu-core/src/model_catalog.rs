//! Config-backed provider/model/favorites catalog.
//!
//! Built-in defaults are embedded TOML, optionally overlaid by
//! `~/.config/mu/models.toml` and `MU_MODELS_*` env vars via Figment.
//! This is the configuration half; [`crate::route_catalog`] turns it
//! into provider×model route entries for front ends.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ModelCatalogConfig {
    pub providers: BTreeMap<String, ProviderCatalogConfig>,
    pub models: BTreeMap<String, ModelCatalogEntry>,
    pub model_rules: BTreeMap<String, ModelRuleConfig>,
    pub favorites: BTreeMap<String, FavoriteConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProviderCatalogConfig {
    pub kind: Option<String>,
    pub label: Option<String>,
    pub aliases: Vec<String>,
    pub requires_api_key: Option<bool>,
    pub usage_semantics: Option<String>,
    /// Does this provider bill by the model's catalog card? `false` for a
    /// lane that serves a model under a shared id but is not the metered
    /// (or subscription) lane the card describes — a self-hosted server, a
    /// gateway with its own tariff — so the card is never applied to it and
    /// its cost prices as unknown. Unset = `true`. mu-1x0ze.
    pub priced: Option<bool>,
    pub quirks: Vec<String>,
    pub base_url: Option<String>,
    pub api_path: Option<String>,
}

/// `[models.<key>.pricing]` / `[model_rules.<key>.pricing]`: the model's rate
/// card, USD per million tokens. The cost MATH lives in `crate::pricing`; the
/// NUMBERS live here, in the catalog — the shipped `models.default.toml`, a
/// generated layer (`mu models sync`), or the operator's `models.toml` — so a
/// price change never needs a build (mu-1x0ze; pricing used to be a compiled
/// table). Which tokens count as fresh input is not on the card: it follows
/// the provider's registered `usage_semantics`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct PricingConfig {
    /// USD per million input tokens. Optional at the config layer so a
    /// layer can override ONE rate (Figment merges tables field by field
    /// and `load_operator_only` parses the operator file on its own); the
    /// pricing layer needs both, and a card missing one prices as unknown.
    pub input_per_mtok: Option<f64>,
    /// USD per million output tokens.
    pub output_per_mtok: Option<f64>,
    /// Cache reads as a fraction of the input rate (the "cached input"
    /// column: 0.10 on most Claude and OpenAI cards, 0.025 on Claude
    /// Fable/Mythos 5.1). ABSENT means no discount — reads price at the
    /// full input rate — because a discount the card does not state is not
    /// assumed (a synced card carries the provider's reported cache price).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_ratio: Option<f64>,
    /// Cache writes into the short-lived (5-minute) tier — and the flat
    /// write total when no tier split is reported — as a multiple of the
    /// input rate (1.25 on the shipped Claude and OpenAI cards). ABSENT
    /// means no surcharge: writes price at the input rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_5m_ratio: Option<f64>,
    /// Cache writes into the one-hour tier as a multiple of the input rate
    /// (2.0 on the shipped Claude cards). ABSENT means the card has one
    /// write price: the 5m ratio applies (no surcharge if that is absent
    /// too).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h_ratio: Option<f64>,
    /// A per-request surcharge above a prompt-size threshold (gpt-6-astra:
    /// over 272k prompt tokens the whole request is 2x input/cache, 1.5x
    /// output). Absent on every other card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_context: Option<LongContextConfig>,
}

/// `pricing.long_context = { prompt_threshold = …, input_mult = …, output_mult = … }`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LongContextConfig {
    /// Requests with prompt tokens strictly above this are surcharged.
    pub prompt_threshold: u64,
    /// Multiplier on fresh input, cache reads and cache writes.
    pub input_mult: f64,
    /// Multiplier on output.
    pub output_mult: f64,
}

// mu-y8gp: per-model sampling (temperature/top_p) is `f64`, which is not `Eq`,
// so the three sampling-carrying catalog structs drop the `Eq` derive (they
// keep `PartialEq`). `Eq` was unused — the `ModelCatalogConfig` container is
// `PartialEq`-only and nothing keys a HashSet/HashMap on these.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ModelCatalogEntry {
    pub model: Option<String>,
    pub family: Option<String>,
    pub label: Option<String>,
    pub aliases: Vec<String>,
    pub context_soft_limit: Option<u64>,
    pub context_hard_limit: Option<u64>,
    pub max_output_tokens: Option<u32>,
    pub reasoning_in_output: Option<bool>,
    pub effort_levels: Vec<String>,
    pub default_effort: Option<String>,
    pub quirks: Vec<String>,
    /// mu-y8gp: per-model sampling forwarded to providers that take it on the
    /// wire (OpenRouter / vLLM today). `None` → provider default. ollama is
    /// deliberately NOT wired to these — sending sampling reloads the model;
    /// bake those into the Modelfile instead.
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    /// mu-4sivd: the rest of the model card's sampling set. presence_penalty
    /// is the anti-repetition knob — before this field it could ride ONLY on
    /// a serve-side launcher flag (single point of failure; the Aug-25 loop
    /// class re-arms if a serve line is rebuilt without it).
    pub presence_penalty: Option<f64>,
    pub top_k: Option<u32>,
    /// mu-g1f2: per-model system-prompt addendum appended to the system message
    /// by providers that consume it on the wire (OpenRouter / vLLM today) — a
    /// behavioral nudge (e.g. "call tools via the function interface, never as
    /// text"). `None` / empty → nothing appended.
    pub system_prompt_addendum: Option<String>,
    /// The model's rate card; see [`PricingConfig`].
    pub pricing: Option<PricingConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ModelRuleConfig {
    pub prefix: Option<String>,
    pub prefixes: Vec<String>,
    pub family: Option<String>,
    pub context_soft_limit: Option<u64>,
    pub context_hard_limit: Option<u64>,
    pub max_output_tokens: Option<u32>,
    pub reasoning_in_output: Option<bool>,
    pub effort_levels: Vec<String>,
    pub default_effort: Option<String>,
    pub quirks: Vec<String>,
    /// mu-y8gp: prefix-rule sampling defaults; see [`ModelCatalogEntry`].
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    /// mu-4sivd: prefix-rule sampling; see [`ModelCatalogEntry`].
    pub presence_penalty: Option<f64>,
    pub top_k: Option<u32>,
    /// mu-g1f2: prefix-rule system-prompt addendum; see [`ModelCatalogEntry`].
    pub system_prompt_addendum: Option<String>,
    /// A family rate card for every model the prefix matches; an exact
    /// `[models.*]` entry's `pricing` wins over it.
    pub pricing: Option<PricingConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct FavoriteConfig {
    pub provider: String,
    pub model: String,
    pub label: Option<String>,
    pub aliases: Vec<String>,
    pub default_effort: Option<String>,
    pub tools: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedModelSettings {
    pub label: Option<String>,
    pub aliases: Vec<String>,
    pub family: Option<String>,
    pub context_soft_limit: Option<u64>,
    pub context_hard_limit: Option<u64>,
    pub max_output_tokens: Option<u32>,
    pub reasoning_in_output: Option<bool>,
    pub effort_levels: Vec<String>,
    pub default_effort: Option<String>,
    pub quirks: Vec<String>,
    /// mu-y8gp: resolved per-model sampling; see [`ModelCatalogEntry`].
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    /// mu-4sivd: resolved per-model sampling; see [`ModelCatalogEntry`].
    pub presence_penalty: Option<f64>,
    pub top_k: Option<u32>,
    /// mu-g1f2: resolved per-model system-prompt addendum; see [`ModelCatalogEntry`].
    pub system_prompt_addendum: Option<String>,
    /// The model's rate card (entry over rule); see [`PricingConfig`].
    pub pricing: Option<PricingConfig>,
}

static DEFAULT_CATALOG: OnceLock<ModelCatalogConfig> = OnceLock::new();
static LOADED_CATALOG: OnceLock<ModelCatalogConfig> = OnceLock::new();

pub fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("mu").join("models.toml"))
}

/// The sync tool writes one generated layer **per provider** next to the
/// operator `models.toml`: `models.generated.<provider>.toml`. Each is
/// merged BELOW the operator file (operator wins, per field). Per-provider
/// files make the sync's "replace one provider, preserve the rest on
/// failure" trivially atomic — one temp+rename per file, no read-modify-
/// write of a shared file. Written by `mu models sync`.
pub fn generated_path_for_provider(operator_config: &Path, provider: &str) -> PathBuf {
    operator_config.with_file_name(format!("models.generated.{provider}.toml"))
}

/// `~/.config/mu/models.generated.<provider>.toml` — the sync tool's write
/// target for `provider`.
pub fn default_generated_path_for_provider(provider: &str) -> Option<PathBuf> {
    default_config_path().map(|p| generated_path_for_provider(&p, provider))
}

/// Enumerate the existing `models.generated.*.toml` layers next to
/// `operator_config`, sorted for a deterministic merge order. Providers are
/// disjoint across files, so order doesn't affect the merged result — the
/// sort just keeps it stable. Missing dir / none present -> empty.
pub fn generated_layers_for(operator_config: &Path) -> Vec<PathBuf> {
    let Some(dir) = operator_config.parent() else {
        return Vec::new();
    };
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("models.generated.") && n.ends_with(".toml"))
        })
        .collect();
    out.sort();
    out
}

pub fn built_in() -> ModelCatalogConfig {
    DEFAULT_CATALOG
        .get_or_init(|| {
            Figment::from(Toml::string(include_str!("../config/models.default.toml")))
                .extract()
                .expect("built-in models.default.toml must parse")
        })
        .clone()
}

pub fn load(config_path: Option<&Path>) -> ModelCatalogConfig {
    let mut fig = Figment::from(Serialized::defaults(built_in()));
    let path = config_path
        .map(Path::to_path_buf)
        .or_else(default_config_path);
    // Merge order (ascending precedence): built-in defaults < generated
    // layers < operator models.toml < env. The generated layers
    // (`models.generated.<provider>.toml`, written by `mu models sync`) sit
    // BELOW the operator file on purpose: a hand edit in models.toml always
    // wins over a probed value, and re-running the sync never clobbers
    // operator overrides. No-op until the sync tool writes them.
    if let Some(p) = path.as_ref() {
        for g in generated_layers_for(p) {
            fig = fig.merge(Toml::file(&g));
        }
    }
    if let Some(p) = path.as_ref() {
        if p.exists() {
            warn_mis_keyed_model_tables(p);
            fig = fig.merge(Toml::file(p));
        }
    }
    let mut merged: ModelCatalogConfig = fig
        .merge(Env::prefixed("MU_MODELS_").split("__"))
        .extract()
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "invalid model catalog config; using built-in defaults");
            built_in()
        });
    // mu-ply3: an operator [models] override keyed under a custom alias (e.g.
    // [models.opus]) must win over a built-in/generated entry for the SAME
    // `model` under a DIFFERENT key (e.g. the built-in [models.claude_opus_4_8]).
    // Figment merges by KEY, so the differently-keyed operator entry became a
    // duplicate that resolve_model's key-ordered scan silently shadowed — the
    // operator could edit their values forever with no effect. Collapse the
    // duplicates, operator-wins, so resolve_model sees one preferred entry.
    if let Some(p) = path.as_ref().filter(|p| p.exists()) {
        let operator_keys: BTreeSet<String> = load_operator_only(p).models.into_keys().collect();
        fold_operator_overrides(&mut merged.models, &operator_keys);
    }
    for key in prefixless_rules(&merged) {
        tracing::warn!(
            rule = %key,
            "model catalog: [model_rules.{key}] has no `prefix`/`prefixes` and matches \
             nothing — an override keyed on a name the built-in catalog no longer uses?"
        );
    }
    merged
}

/// Rule keys that can never match a model: no `prefix` and no `prefixes`.
/// Figment merges by key, so an operator override of a rule the built-in
/// catalog has since renamed deserializes into exactly this — a rule that
/// silently applies to nothing — which is why [`load`] warns about each one.
fn prefixless_rules(cfg: &ModelCatalogConfig) -> Vec<String> {
    cfg.model_rules
        .iter()
        .filter(|(_, r)| r.prefix.is_none() && r.prefixes.is_empty())
        .map(|(k, _)| k.clone())
        .collect()
}

/// Collapse operator overrides keyed under a custom alias onto the
/// built-in/generated entry they're meant to override (bead mu-ply3; called
/// from [`load`]). For each model an operator-layer entry defines, every OTHER
/// entry (a non-operator key) with the same `model` is folded into the operator
/// entry — operator values win, the shadow fills only the fields the operator
/// left unset (matching same-key Figment merge) — then the shadow is removed.
/// One operator-preferred entry survives, so [`ModelCatalogConfig::resolve_model`]'s
/// `.values().find()` can no longer return a built-in entry that sorts ahead of it.
fn fold_operator_overrides(
    models: &mut BTreeMap<String, ModelCatalogEntry>,
    operator_keys: &BTreeSet<String>,
) {
    let op_pairs: Vec<(String, String)> = models
        .iter()
        .filter(|(k, _)| operator_keys.contains(k.as_str()))
        .filter_map(|(k, m)| m.model.clone().map(|model| (k.clone(), model)))
        .collect();
    for (op_key, model) in op_pairs {
        let shadow_keys: Vec<String> = models
            .iter()
            .filter(|(k, m)| {
                k.as_str() != op_key
                    && !operator_keys.contains(k.as_str())
                    && m.model.as_deref() == Some(model.as_str())
            })
            .map(|(k, _)| k.clone())
            .collect();
        for sk in shadow_keys {
            if let Some(shadow) = models.get(&sk).cloned() {
                if let Some(op_entry) = models.get_mut(&op_key) {
                    fill_missing_fields(op_entry, &shadow);
                }
            }
            models.remove(&sk);
        }
    }
}

/// Copy `src`'s set fields into `dst` ONLY where `dst` left them unset, so an
/// operator override keeps its explicit values and inherits the built-in's for
/// everything it didn't specify. (bead mu-ply3)
fn fill_missing_fields(dst: &mut ModelCatalogEntry, src: &ModelCatalogEntry) {
    if dst.model.is_none() {
        dst.model = src.model.clone();
    }
    if dst.family.is_none() {
        dst.family = src.family.clone();
    }
    if dst.label.is_none() {
        dst.label = src.label.clone();
    }
    if dst.aliases.is_empty() {
        dst.aliases = src.aliases.clone();
    }
    if dst.context_soft_limit.is_none() {
        dst.context_soft_limit = src.context_soft_limit;
    }
    if dst.context_hard_limit.is_none() {
        dst.context_hard_limit = src.context_hard_limit;
    }
    if dst.max_output_tokens.is_none() {
        dst.max_output_tokens = src.max_output_tokens;
    }
    if dst.reasoning_in_output.is_none() {
        dst.reasoning_in_output = src.reasoning_in_output;
    }
    if dst.effort_levels.is_empty() {
        dst.effort_levels = src.effort_levels.clone();
    }
    if dst.default_effort.is_none() {
        dst.default_effort = src.default_effort.clone();
    }
    if dst.quirks.is_empty() {
        dst.quirks = src.quirks.clone();
    }
    if dst.temperature.is_none() {
        dst.temperature = src.temperature;
    }
    if dst.top_p.is_none() {
        dst.top_p = src.top_p;
    }
    if dst.presence_penalty.is_none() {
        dst.presence_penalty = src.presence_penalty;
    }
    if dst.top_k.is_none() {
        dst.top_k = src.top_k;
    }
    if dst.system_prompt_addendum.is_none() {
        dst.system_prompt_addendum = src.system_prompt_addendum.clone();
    }
    // field by field, like the same-key Figment merge: a re-keyed entry
    // that states one rate keeps the shipped card's other fields
    match (&mut dst.pricing, &src.pricing) {
        (None, Some(src_card)) => dst.pricing = Some(src_card.clone()),
        (Some(card), Some(src_card)) => {
            if card.input_per_mtok.is_none() {
                card.input_per_mtok = src_card.input_per_mtok;
            }
            if card.output_per_mtok.is_none() {
                card.output_per_mtok = src_card.output_per_mtok;
            }
            if card.cache_read_ratio.is_none() {
                card.cache_read_ratio = src_card.cache_read_ratio;
            }
            if card.cache_write_5m_ratio.is_none() {
                card.cache_write_5m_ratio = src_card.cache_write_5m_ratio;
            }
            if card.cache_write_1h_ratio.is_none() {
                card.cache_write_1h_ratio = src_card.cache_write_1h_ratio;
            }
            if card.long_context.is_none() {
                card.long_context = src_card.long_context.clone();
            }
        }
        _ => {}
    }
}

/// Warn on the `["models.x:y"]` footgun. Quoting the *whole* dotted path
/// makes a TOP-LEVEL key literally named `models.x:y` instead of an entry
/// under `[models]`, so the `#[serde(default)]` catalog silently drops it
/// and the operator's override never applies (the 2026-06-19 ollama
/// incident: `["models.qwen3.6:27b"]` -> ignored -> fell to the placeholder).
/// Detect such stray top-level keys and point at the correct form. Best
/// effort: unreadable/unparseable files are left to the normal load path.
/// Pure detector: stray top-level keys shaped like `models.x` /
/// `model_rules.x` (the `["models.x:y"]` mis-key). Returns them so the
/// warner can report and tests can assert. Unparseable TOML -> empty
/// (the normal load path surfaces parse errors).
fn mis_keyed_model_tables(text: &str) -> Vec<String> {
    let Ok(value) = toml::from_str::<toml::Value>(text) else {
        return Vec::new();
    };
    let Some(table) = value.as_table() else {
        return Vec::new();
    };
    table
        .keys()
        .filter(|k| k.starts_with("models.") || k.starts_with("model_rules."))
        .cloned()
        .collect()
}

/// The OTHER mis-key flavor: an UNQUOTED dotted key like `[models.gpt-5.5]`.
/// An unquoted `.` is a table separator in TOML, so that parses as `models`
/// -> `gpt-5` -> `5` (a table two levels down) instead of a model named
/// `gpt-5.5`; the real fields land in a nested table the `#[serde(default)]`
/// catalog ignores, so the entry is silently dropped (the 2026-06-20
/// `[models.gpt-5.5]` incident). [`mis_keyed_model_tables`] above misses this:
/// it makes a *valid* top-level `models` key, just over-nested. The tell — a
/// `[models]` / `[model_rules]` entry whose value holds a NESTED TABLE other
/// than the schema's own `pricing` table, since
/// real entries carry only scalar/array fields. Returns `(section,
/// reconstructed_dotted_key)` so the warner can point at the quoted form.
/// Pure; unparseable -> empty.
fn dotted_nested_model_entries(text: &str) -> Vec<(String, String)> {
    let Ok(value) = toml::from_str::<toml::Value>(text) else {
        return Vec::new();
    };
    let Some(root) = value.as_table() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for section in ["models", "model_rules"] {
        let Some(tbl) = root.get(section).and_then(|v| v.as_table()) else {
            continue;
        };
        for (entry_key, entry_val) in tbl {
            let Some(entry_tbl) = entry_val.as_table() else {
                continue;
            };
            for (nested_key, nested_val) in entry_tbl {
                // `pricing` is the one nested table the schema defines
                // (`[models.<key>.pricing]`, mu-1x0ze); it is consumed, not
                // a mis-key.
                if nested_val.is_table() && nested_key != "pricing" {
                    // best-effort reconstruct: `gpt-5` + `5` -> `gpt-5.5`.
                    out.push((section.to_string(), format!("{entry_key}.{nested_key}")));
                }
            }
        }
    }
    out
}

fn warn_mis_keyed_model_tables(p: &Path) {
    let Ok(text) = std::fs::read_to_string(p) else {
        return;
    };
    for key in mis_keyed_model_tables(&text) {
        let section = if key.starts_with("models.") {
            "models"
        } else {
            "model_rules"
        };
        let entry = &key[section.len() + 1..];
        tracing::warn!(
            stray_key = %key,
            "model catalog: top-level key `{key}` looks like a mis-keyed table — \
             quoting the whole path makes it a top-level key, not an entry under \
             [{section}], so it is SILENTLY IGNORED. Use [{section}.\"{entry}\"] instead."
        );
    }
    for (section, dotted) in dotted_nested_model_entries(&text) {
        tracing::warn!(
            nested_key = %dotted,
            "model catalog: `[{section}.{dotted}]` was read as NESTED tables, not a \
             single entry named `{dotted}` — an unquoted `.` is a table separator in \
             TOML, so the fields are buried and SILENTLY IGNORED. Quote the key: \
             [{section}.\"{dotted}\"]."
        );
    }
}

pub fn global() -> &'static ModelCatalogConfig {
    LOADED_CATALOG.get_or_init(|| load(None))
}

/// Load ONLY the operator `models.toml` — no built-in defaults, generated
/// layers, or env. This is the explicit selection surface for `mu models
/// sync`: it must reflect exactly the models the operator referenced, not
/// the built-in catalog. Missing file -> empty (nothing selected). Parse
/// error -> empty + warn (the normal [`load`] path surfaces the detail).
pub fn load_operator_only(config_path: &Path) -> ModelCatalogConfig {
    if !config_path.exists() {
        return ModelCatalogConfig::default();
    }
    warn_mis_keyed_model_tables(config_path);
    Figment::from(Toml::file(config_path))
        .extract()
        .unwrap_or_else(|e| {
            tracing::warn!(
                error = %e,
                "operator models.toml parse failed; treating as empty for sync selection"
            );
            ModelCatalogConfig::default()
        })
}

impl ModelCatalogConfig {
    pub fn provider(&self, provider_kind: &str) -> Option<&ProviderCatalogConfig> {
        self.providers
            .get(provider_kind)
            .or_else(|| {
                self.providers
                    .values()
                    .find(|p| p.kind.as_deref() == Some(provider_kind))
            })
            .or_else(|| {
                self.providers
                    .values()
                    .find(|p| p.aliases.iter().any(|a| a == provider_kind))
            })
    }

    pub fn resolve_model_key(&self, model_or_alias: &str) -> Option<&str> {
        self.models.iter().find_map(|(key, m)| {
            if key == model_or_alias
                || m.model.as_deref() == Some(model_or_alias)
                || m.aliases.iter().any(|a| a == model_or_alias)
            {
                Some(key.as_str())
            } else {
                None
            }
        })
    }

    /// Resolve a SELECTION alias: a favorite name (the `[favorites.<name>]`
    /// table key) or one of its `aliases` → that favorite's
    /// `(provider, model)`. Returns `None` if `name` matches no favorite.
    ///
    /// This is the SELECTION counterpart to [`resolve_model`](Self::resolve_model)'s
    /// ENRICHMENT: `resolve_model` attaches metadata to a model you already
    /// chose; this *rewrites what to launch* — a short name standing in for a
    /// full `{provider, model}` pair, so the long, typo-prone tag lives in
    /// exactly one place (the favorite) instead of being retyped every run.
    /// (bead mu-eb98, work item 2)
    pub fn resolve_selection_alias(&self, name: &str) -> Option<(&str, &str)> {
        self.favorites.iter().find_map(|(key, fav)| {
            (key == name || fav.aliases.iter().any(|a| a == name))
                .then_some((fav.provider.as_str(), fav.model.as_str()))
        })
    }

    /// Resolve a model LABEL to a concrete model name.
    ///
    /// A `[models.<label>]` table key is a memorable, arbitrary alias (e.g.
    /// `architect`, `coding`); its `model` field is the name it stands for
    /// (which may itself be another label, or the exact upstream tag). This
    /// follows the chain — `coding → gpt-5.5`, `local → qwen3-6 →
    /// qwen3.6:35b-a3b-q8_0` — and returns the terminal name. An input that
    /// is NOT a `[models]` key is already a concrete name and passes through
    /// unchanged, so calling this on any model string is safe and idempotent.
    ///
    /// Loop-safe: a self-referential entry (`[models.x] model = "x"`) or a
    /// cycle (`a → b → a`) terminates at the first repeat. (bead mu-f7f6)
    pub fn resolve_model_name(&self, input: &str) -> String {
        let mut name = input.to_string();
        let mut seen = std::collections::HashSet::new();
        while seen.insert(name.clone()) {
            match self.models.get(&name).and_then(|m| m.model.as_deref()) {
                // Points at a *different* name → keep following the chain.
                Some(next) if next != name => name = next.to_string(),
                // No `model` field, or it points at itself → terminal.
                _ => break,
            }
        }
        name
    }

    pub fn favorites_for(&self, provider_kind: &str, model: &str) -> Vec<(&str, &FavoriteConfig)> {
        self.favorites
            .iter()
            .filter_map(|(name, fav)| {
                let provider_matches = fav.provider == provider_kind
                    || self.provider(&fav.provider).and_then(|p| p.kind.as_deref())
                        == Some(provider_kind);
                if !provider_matches {
                    return None;
                }
                let model_matches = fav.model == model
                    || self.resolve_model_key(model) == Some(fav.model.as_str())
                    || self.models.get(&fav.model).and_then(|m| m.model.as_deref()) == Some(model);
                model_matches.then_some((name.as_str(), fav))
            })
            .collect()
    }

    pub fn resolve_model(&self, model: &str) -> ResolvedModelSettings {
        let exact = self
            .models
            .values()
            .find(|m| m.model.as_deref() == Some(model) || m.aliases.iter().any(|a| a == model));
        let mut out = ResolvedModelSettings::default();

        if let Some(rule) = self.matching_rule(model) {
            out.family = rule.family.clone();
            out.context_soft_limit = rule.context_soft_limit;
            out.context_hard_limit = rule.context_hard_limit;
            out.max_output_tokens = rule.max_output_tokens;
            out.reasoning_in_output = rule.reasoning_in_output;
            out.effort_levels = rule.effort_levels.clone();
            out.default_effort = rule.default_effort.clone();
            out.quirks = rule.quirks.clone();
            out.temperature = rule.temperature;
            out.top_p = rule.top_p;
            out.presence_penalty = rule.presence_penalty;
            out.top_k = rule.top_k;
            out.system_prompt_addendum = rule.system_prompt_addendum.clone();
            out.pricing = rule.pricing.clone();
        }

        if let Some(m) = exact {
            if m.label.is_some() {
                out.label = m.label.clone();
            }
            if !m.aliases.is_empty() {
                out.aliases = m.aliases.clone();
            }
            if m.family.is_some() {
                out.family = m.family.clone();
            }
            if m.context_soft_limit.is_some() {
                out.context_soft_limit = m.context_soft_limit;
            }
            if m.context_hard_limit.is_some() {
                out.context_hard_limit = m.context_hard_limit;
            }
            if m.max_output_tokens.is_some() {
                out.max_output_tokens = m.max_output_tokens;
            }
            if m.reasoning_in_output.is_some() {
                out.reasoning_in_output = m.reasoning_in_output;
            }
            if !m.effort_levels.is_empty() {
                out.effort_levels = m.effort_levels.clone();
            }
            if m.default_effort.is_some() {
                out.default_effort = m.default_effort.clone();
            }
            if !m.quirks.is_empty() {
                out.quirks = merge_strings(&out.quirks, &m.quirks);
            }
            if m.temperature.is_some() {
                out.temperature = m.temperature;
            }
            if m.top_p.is_some() {
                out.top_p = m.top_p;
            }
            if m.presence_penalty.is_some() {
                out.presence_penalty = m.presence_penalty;
            }
            if m.top_k.is_some() {
                out.top_k = m.top_k;
            }
            if m.system_prompt_addendum.is_some() {
                out.system_prompt_addendum = m.system_prompt_addendum.clone();
            }
            if m.pricing.is_some() {
                out.pricing = m.pricing.clone();
            }
        }

        out
    }

    fn matching_rule(&self, model: &str) -> Option<&ModelRuleConfig> {
        self.model_rules
            .values()
            .filter(|r| r.matches(model))
            .max_by_key(|r| r.longest_prefix_len(model))
    }
}

impl ModelRuleConfig {
    fn prefixes_iter(&self) -> impl Iterator<Item = &str> {
        self.prefix
            .iter()
            .map(String::as_str)
            .chain(self.prefixes.iter().map(String::as_str))
    }

    fn matches(&self, model: &str) -> bool {
        let m = model.to_ascii_lowercase();
        self.prefixes_iter()
            .any(|p| m.starts_with(&p.to_ascii_lowercase()))
    }

    fn longest_prefix_len(&self, model: &str) -> usize {
        let m = model.to_ascii_lowercase();
        self.prefixes_iter()
            .filter(|p| m.starts_with(&p.to_ascii_lowercase()))
            .map(str::len)
            .max()
            .unwrap_or(0)
    }
}

fn merge_strings(a: &[String], b: &[String]) -> Vec<String> {
    let mut out = a.to_vec();
    for s in b {
        if !out.iter().any(|x| x == s) {
            out.push(s.clone());
        }
    }
    out
}

pub fn max_output_tokens_for_model(model: &str) -> u32 {
    global()
        .resolve_model(model)
        .max_output_tokens
        .unwrap_or(4096)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An operator override keyed on a rule name the built-in catalog no
    /// longer has (claude_gen_5_frontier was split in 6uqho.6) merges into a
    /// rule with no prefix, which matches nothing; the loader names it. The
    /// shipped catalog has none.
    #[test]
    fn orphaned_rule_override_is_named() {
        assert!(prefixless_rules(&built_in()).is_empty());
        let overlay = r#"
[model_rules.claude_gen_5_frontier]
max_output_tokens = 100000
"#;
        let cfg: ModelCatalogConfig = Figment::from(Serialized::defaults(built_in()))
            .merge(Toml::string(overlay))
            .extract()
            .expect("overlay parses");
        assert_eq!(
            prefixless_rules(&cfg),
            vec!["claude_gen_5_frontier".to_string()]
        );
        assert_eq!(
            cfg.resolve_model("claude-opus-5").max_output_tokens,
            Some(128000),
            "the orphaned override changes nothing"
        );
    }

    #[test]
    fn detects_mis_keyed_model_tables() {
        // The footgun: whole path quoted -> top-level stray key, silently
        // ignored. The correct forms (nested) must NOT be flagged.
        let toml = r#"
["models.qwen3.6:27b"]
model = "qwen3.6:27b"
context_soft_limit = 200000

["model_rules.deepseek:v4"]
prefix = "deepseek"

[models."qwen3-coder:30b"]
model = "qwen3-coder:30b"

[model_rules.deepseek]
prefix = "deepseek"

[models.gpt-oss-rev]
model = "gpt-oss-rev"
"#;
        let mut stray = mis_keyed_model_tables(toml);
        stray.sort();
        assert_eq!(
            stray,
            vec![
                "model_rules.deepseek:v4".to_string(),
                "models.qwen3.6:27b".to_string(),
            ],
            "only the whole-path-quoted tables are flagged; nested forms are fine"
        );
    }

    #[test]
    fn detects_dotted_nested_model_keys() {
        // The 2026-06-20 footgun: an UNQUOTED dotted key. `[models.gpt-5.5]`
        // parses as models -> gpt-5 -> 5, burying the fields in a nested table
        // the catalog drops. The quoted form and ordinary single-segment keys
        // must NOT be flagged.
        let toml = r#"
[models.gpt-5.5]
model = "gpt-5.5"
context_hard_limit = 1000000
context_soft_limit = 262144

[models."claude-opus-4-8"]
model = "claude-opus-4-8"
context_soft_limit = 200000

[models.gpt-oss-rev]
model = "gpt-oss-rev"

# the schema's own nested table (mu-1x0ze): consumed, not a mis-key
[models.gpt-oss-rev.pricing]
input_per_mtok = 1.0
output_per_mtok = 2.0

[model_rules.deepseek.v4]
prefix = "deepseek"

[model_rules.gpt_family]
prefix = "gpt-"
[model_rules.gpt_family.pricing]
input_per_mtok = 1.0
output_per_mtok = 2.0
"#;
        let mut found = dotted_nested_model_entries(toml);
        found.sort();
        assert_eq!(
            found,
            vec![
                ("model_rules".to_string(), "deepseek.v4".to_string()),
                ("models".to_string(), "gpt-5.5".to_string()),
            ],
            "only the unquoted-dotted keys nest; quoted and single-segment keys are fine"
        );
    }

    #[test]
    fn generated_layer_merges_under_operator() {
        // The sync tool's per-provider models.generated.<provider>.toml is
        // merged BELOW the operator models.toml: operator values win
        // per-field, generated fills the gaps the operator didn't set. The
        // loader discovers the layer by glob, so the provider suffix is
        // immaterial here.
        let dir = std::env::temp_dir().join(format!("mu-catalog-gen-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let op = dir.join("models.toml");
        let gen = generated_path_for_provider(&op, "openrouter");
        std::fs::write(
            &gen,
            "[models.\"m1\"]\nmodel = \"m1\"\ncontext_hard_limit = 999\nmax_output_tokens = 111\n",
        )
        .unwrap();
        std::fs::write(
            &op,
            "[models.\"m1\"]\nmodel = \"m1\"\ncontext_hard_limit = 222\n",
        )
        .unwrap();
        let cfg = load(Some(&op));
        let m = cfg.models.get("m1").expect("m1 present from merged layers");
        assert_eq!(m.context_hard_limit, Some(222), "operator value wins");
        assert_eq!(
            m.max_output_tokens,
            Some(111),
            "generated fills the field the operator left unset"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// mu-1x0ze: a rate card is config. An exact entry's `pricing` wins over
    /// the matching rule's; the rule prices every id its prefix matches; a
    /// missing `cache_read_ratio` means no discount at the pricing layer;
    /// `long_context` rides along; which tokens are fresh input follows the
    /// provider's `usage_semantics`; no card, or a provider not in the
    /// catalog, is None — never a guess.
    #[test]
    fn pricing_tables_resolve_entry_over_rule_and_follow_provider_semantics() {
        let toml = r#"
[providers.acme_api]
kind = "acme_api"
usage_semantics = "openai_style"

[providers.zed_api]
kind = "zed_api"

[models.big]
model = "big-1"
[models.big.pricing]
input_per_mtok = 10.0
output_per_mtok = 50.0
long_context = { prompt_threshold = 272000, input_mult = 2.0, output_mult = 1.5 }

[model_rules.big_family]
prefix = "big-"
[model_rules.big_family.pricing]
input_per_mtok = 4.0
output_per_mtok = 20.0
cache_read_ratio = 0.025

[models.unpriced]
model = "free-1"
"#;
        let cfg: ModelCatalogConfig = Figment::from(Toml::string(toml)).extract().unwrap();
        let exact = cfg.resolve_model("big-1").pricing.expect("entry card");
        assert_eq!(
            (exact.input_per_mtok, exact.output_per_mtok),
            (Some(10.0), Some(50.0))
        );
        assert_eq!(
            exact.long_context.as_ref().map(|t| t.prompt_threshold),
            Some(272_000)
        );
        let dated = cfg
            .resolve_model("big-1-20261201")
            .pricing
            .expect("rule card");
        assert_eq!(
            (dated.input_per_mtok, dated.cache_read_ratio),
            (Some(4.0), Some(0.025))
        );
        assert!(cfg.resolve_model("free-1").pricing.is_none());

        let inclusive = crate::pricing::for_model_in(&cfg, "acme_api", "big-1").expect("priced");
        assert!(inclusive.cache_read_in_input && inclusive.cache_creation_in_input);
        // the entry states no discount, so none: reads at the input rate
        assert_eq!(inclusive.cache_read_ratio, 1.0);
        assert!(inclusive.long_context.is_some());
        let disjoint = crate::pricing::for_model_in(&cfg, "zed_api", "big-1-x").expect("priced");
        assert!(!disjoint.cache_read_in_input && !disjoint.cache_creation_in_input);
        assert_eq!(disjoint.cache_read_ratio, 0.025);
        assert!(crate::pricing::for_model_in(&cfg, "acme_api", "free-1").is_none());
        assert!(crate::pricing::for_model_in(&cfg, "nobody", "big-1").is_none());
        // a provider that does not bill by the card (a self-hosted server
        // serving the same model id) gets no card, never another lane's
        let toml = r#"
[providers.local]
kind = "local"
priced = false

[models.big]
model = "big-1"
[models.big.pricing]
input_per_mtok = 10.0
output_per_mtok = 50.0
"#;
        let cfg: ModelCatalogConfig = Figment::from(Toml::string(toml)).extract().unwrap();
        assert!(crate::pricing::for_model_in(&cfg, "local", "big-1").is_none());
        // a card that states no cache discount gets none: reads at the
        // input rate, not an assumed 0.10
        let no_ratio = r#"
[providers.p]
kind = "p"
[models.m]
model = "m"
[models.m.pricing]
input_per_mtok = 10.0
output_per_mtok = 50.0
"#;
        let cfg: ModelCatalogConfig = Figment::from(Toml::string(no_ratio)).extract().unwrap();
        assert_eq!(
            crate::pricing::for_model_in(&cfg, "p", "m")
                .unwrap()
                .cache_read_ratio,
            1.0
        );
        // the shipped local lanes are not priced by the card
        let shipped = built_in();
        assert_eq!(
            shipped.provider("ollama").and_then(|p| p.priced),
            Some(false)
        );
        assert_eq!(shipped.provider("vllm").and_then(|p| p.priced), Some(false));
    }

    /// The shipped catalog prices every model mu routes to on the
    /// Anthropic and OpenAI lanes, so the operator never sees "unknown" for
    /// a first-party model; a price lives in `models.default.toml`, and an
    /// operator `models.toml` entry overrides it without a build.
    #[test]
    fn shipped_catalog_carries_the_first_party_rate_cards() {
        let cfg = built_in();
        for (provider, model, input, output) in [
            ("anthropic_api", "claude-opus-4-8", 5.0, 25.0),
            ("anthropic_api", "claude-opus-4-8-20260101", 5.0, 25.0),
            ("anthropic_api", "claude-sonnet-4-6", 3.0, 15.0),
            ("anthropic_api", "claude-haiku-4-5-20251001", 1.0, 5.0),
            ("anthropic_api", "claude-opus-4-1-20250805", 15.0, 75.0),
            ("anthropic_api", "claude-opus-4-20250514", 15.0, 75.0),
            ("anthropic_api", "claude-sonnet-4-20250514", 3.0, 15.0),
            ("anthropic_api", "claude-opus-5", 5.0, 25.0),
            ("anthropic_api", "claude-sonnet-5", 2.0, 10.0),
            ("anthropic_api", "claude-fable-5", 10.0, 50.0),
            ("anthropic_api", "claude-fable-5-1", 10.0, 50.0),
            ("anthropic_api", "claude-mythos-5-1", 10.0, 50.0),
            ("anthropic_oauth", "claude-opus-4-8", 5.0, 25.0),
            ("openai_api", "gpt-5.5", 5.0, 30.0),
            ("openai_api", "gpt-5.5-2026-06-01", 5.0, 30.0),
            ("openai_codex", "gpt-6-astra", 10.0, 50.0),
            ("openai_codex", "gpt-6-astra-2026-09-03", 10.0, 50.0),
        ] {
            let p = crate::pricing::for_model_in(&cfg, provider, model)
                .unwrap_or_else(|| panic!("{provider}/{model} priced"));
            assert_eq!(
                (p.input_per_mtok, p.output_per_mtok),
                (input, output),
                "{provider}/{model}"
            );
        }
        let fable51 =
            crate::pricing::for_model_in(&cfg, "anthropic_api", "claude-fable-5-1").unwrap();
        assert_eq!(fable51.cache_read_ratio, 0.025);
        let astra = crate::pricing::for_model_in(&cfg, "openai_api", "gpt-6-astra").unwrap();
        assert_eq!(
            astra.long_context.map(|t| t.prompt_threshold),
            Some(272_000)
        );
        // a date-stamped Astra keeps the tier too; a gpt-6 sibling that is
        // not Astra, and a gpt-5 sibling with no card, price as unknown
        let dated =
            crate::pricing::for_model_in(&cfg, "openai_api", "gpt-6-astra-2026-09-03").unwrap();
        assert_eq!(
            dated.long_context.map(|t| t.prompt_threshold),
            Some(272_000)
        );
        assert!(crate::pricing::for_model_in(&cfg, "openai_api", "gpt-6-nova").is_none());
        assert!(crate::pricing::for_model_in(&cfg, "openai_api", "gpt-5.4").is_none());
        // an operator override wins over the shipped number, no build needed
        let over = r#"
[models.claude_opus_4_8]
model = "claude-opus-4-8"
[models.claude_opus_4_8.pricing]
input_per_mtok = 7.0
output_per_mtok = 35.0
"#;
        let cfg: ModelCatalogConfig = Figment::from(Serialized::defaults(built_in()))
            .merge(Toml::string(over))
            .extract()
            .unwrap();
        let p = crate::pricing::for_model_in(&cfg, "anthropic_api", "claude-opus-4-8").unwrap();
        assert_eq!((p.input_per_mtok, p.output_per_mtok), (7.0, 35.0));
        // an operator entry keyed differently from the shipped one (mu-ply3
        // folds the shipped entry into it) keeps the shipped card when it
        // sets none of its own
        let rekeyed = r#"
[models.astra]
model = "gpt-6-astra"
context_soft_limit = 500000
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.toml");
        std::fs::write(&path, rekeyed).unwrap();
        let cfg = load(Some(&path));
        let astra = cfg.resolve_model("gpt-6-astra");
        assert_eq!(astra.context_soft_limit, Some(500_000));
        assert_eq!(
            astra.pricing.as_ref().and_then(|p| p.input_per_mtok),
            Some(10.0)
        );
        assert!(crate::pricing::for_model_in(&cfg, "openai_api", "gpt-6-astra").is_some());
        // a PARTIAL override — one rate — merges over the shipped card in
        // load() and parses on its own in load_operator_only(), which the
        // sync selection and the operator-key fold read (round-4 board:
        // required fields made the standalone parse fail and the whole
        // operator file read as empty)
        let partial = r#"
[models.gpt_6_astra]
model = "gpt-6-astra"
[models.gpt_6_astra.pricing]
input_per_mtok = 12.0
"#;
        std::fs::write(&path, partial).unwrap();
        let only = load_operator_only(&path);
        assert_eq!(
            only.models.len(),
            1,
            "operator-only load must not drop the file"
        );
        let cfg = load(Some(&path));
        let p = crate::pricing::for_model_in(&cfg, "openai_api", "gpt-6-astra").unwrap();
        assert_eq!((p.input_per_mtok, p.output_per_mtok), (12.0, 50.0));
        assert_eq!(p.long_context.map(|t| t.prompt_threshold), Some(272_000));
        // the same partial override under a DIFFERENT key (the fold path)
        // merges field by field too (round-5 board)
        let rekeyed_partial = r#"
[models.astra]
model = "gpt-6-astra"
[models.astra.pricing]
input_per_mtok = 12.0
"#;
        std::fs::write(&path, rekeyed_partial).unwrap();
        let cfg = load(Some(&path));
        let p = crate::pricing::for_model_in(&cfg, "openai_api", "gpt-6-astra").unwrap();
        assert_eq!((p.input_per_mtok, p.output_per_mtok), (12.0, 50.0));
        assert_eq!(p.cache_read_ratio, 0.10);
        assert_eq!(p.long_context.map(|t| t.prompt_threshold), Some(272_000));
    }

    #[test]
    fn built_in_qwen36_gets_reasoning_budget() {
        assert_eq!(
            built_in()
                .resolve_model("qwen3.6:35b-a3b-q8_0")
                .max_output_tokens,
            Some(16384)
        );
    }

    #[test]
    fn exact_model_overrides_rule() {
        let toml = r#"
            [model_rules.q]
            prefix = "qwen3.6:"
            max_output_tokens = 4096
            quirks = ["rule"]

            [models.q]
            model = "qwen3.6:35b"
            max_output_tokens = 24576
            quirks = ["exact"]
        "#;
        let cfg: ModelCatalogConfig = Figment::from(Toml::string(toml)).extract().unwrap();
        let s = cfg.resolve_model("qwen3.6:35b");
        assert_eq!(s.max_output_tokens, Some(24576));
        assert_eq!(s.quirks, vec!["rule".to_string(), "exact".to_string()]);
    }

    #[test]
    fn figment_overlay_changes_only_one_field() {
        let base = r#"
            [models.q]
            model = "qwen3.6:35b"
            family = "qwen3"
            max_output_tokens = 4096
        "#;
        let overlay = r#"
            [models.q]
            max_output_tokens = 16384
        "#;
        let cfg: ModelCatalogConfig = Figment::from(Toml::string(base))
            .merge(Toml::string(overlay))
            .extract()
            .unwrap();
        let q = cfg.models.get("q").unwrap();
        assert_eq!(q.family.as_deref(), Some("qwen3"));
        assert_eq!(q.max_output_tokens, Some(16384));
    }

    #[test]
    fn favorites_match_provider_and_model_aliases() {
        let toml = r#"
            [providers.ollama]
            kind = "ollama"
            aliases = ["local"]

            [models.q]
            model = "qwen3.6:35b"
            aliases = ["qwen"]

            [favorites.local_reasoner]
            provider = "local"
            model = "q"
            label = "Local Reasoner"
            aliases = ["lr"]
        "#;
        let cfg: ModelCatalogConfig = Figment::from(Toml::string(toml)).extract().unwrap();
        let favs = cfg.favorites_for("ollama", "qwen3.6:35b");
        assert_eq!(favs.len(), 1);
        assert_eq!(favs[0].0, "local_reasoner");
    }

    #[test]
    fn resolve_selection_alias_matches_favorite_name_and_aliases() {
        // A favorite is a selection alias: its table key OR any of its
        // `aliases` resolves to the favorite's {provider, model} — the full
        // tag lives only in the favorite (bead mu-eb98 item 2).
        let toml = r#"
            [favorites.local_reasoner]
            provider = "ollama"
            model = "qwen3.6:35b-a3b-q8_0"
            aliases = ["lr", "qwen35"]
        "#;
        let cfg: ModelCatalogConfig = Figment::from(Toml::string(toml)).extract().unwrap();
        // by table key
        assert_eq!(
            cfg.resolve_selection_alias("local_reasoner"),
            Some(("ollama", "qwen3.6:35b-a3b-q8_0"))
        );
        // by each alias
        assert_eq!(
            cfg.resolve_selection_alias("lr"),
            Some(("ollama", "qwen3.6:35b-a3b-q8_0"))
        );
        assert_eq!(
            cfg.resolve_selection_alias("qwen35"),
            Some(("ollama", "qwen3.6:35b-a3b-q8_0"))
        );
        // a non-favorite (e.g. a raw full tag) does not resolve
        assert_eq!(cfg.resolve_selection_alias("qwen3.6:35b-a3b-q8_0"), None);
        assert_eq!(cfg.resolve_selection_alias("nope"), None);
    }

    #[test]
    fn resolve_model_name_follows_label_chains_and_passes_raw_through() {
        // `[models.<label>]` keys are arbitrary aliases; `model` is the name
        // they stand for, which may be another label. resolve_model_name
        // follows the chain to the terminal name (bead mu-f7f6).
        let toml = r#"
            [models.architect]
            model = "gpt-5.5-codex"

            [models.coding]
            model = "gpt-5.5"

            [models.local]
            model = "qwen3-6"
            [models.qwen3-6]
            model = "qwen3.6:35b-a3b-q8_0"

            [models.selfref]
            model = "selfref"
        "#;
        let cfg: ModelCatalogConfig = Figment::from(Toml::string(toml)).extract().unwrap();
        // single hop: label -> upstream name
        assert_eq!(cfg.resolve_model_name("architect"), "gpt-5.5-codex");
        assert_eq!(cfg.resolve_model_name("coding"), "gpt-5.5");
        // two hops: local -> qwen3-6 -> the exact tag
        assert_eq!(cfg.resolve_model_name("local"), "qwen3.6:35b-a3b-q8_0");
        // a raw name that is not a label passes through unchanged
        assert_eq!(cfg.resolve_model_name("gpt-5.5"), "gpt-5.5");
        assert_eq!(
            cfg.resolve_model_name("qwen3.6:35b-a3b-q8_0"),
            "qwen3.6:35b-a3b-q8_0"
        );
        // self-reference terminates (does not loop) and yields the key
        assert_eq!(cfg.resolve_model_name("selfref"), "selfref");
        // unknown label passes through (it's treated as already-a-name)
        assert_eq!(cfg.resolve_model_name("nope"), "nope");
    }

    #[test]
    fn operator_alias_override_wins_over_builtin_same_model_different_key() {
        // The built-in keys opus under `claude_opus_4_8`; the operator overrides
        // it under a custom alias `opus` — different key, SAME model. Without the
        // fold, resolve_model's key-ordered scan returns the built-in (sorts
        // first) and the operator's edits are silently shadowed (bead mu-ply3,
        // the live 2026-06-21 opus-context bug).
        let toml = r#"
            [models.claude_opus_4_8]
            model = "claude-opus-4-8"
            family = "claude-opus-4"
            context_soft_limit = 200000
            context_hard_limit = 1000000
            max_output_tokens = 16384

            [models.opus]
            model = "claude-opus-4-8"
            context_soft_limit = 500000
            context_hard_limit = 750000
        "#;
        let mut cfg: ModelCatalogConfig = Figment::from(Toml::string(toml)).extract().unwrap();

        // Pre-fold: the built-in (sorts first) shadows the alias override.
        assert_eq!(
            cfg.resolve_model("claude-opus-4-8").context_soft_limit,
            Some(200000),
            "pre-fix: built-in shadows the operator's alias-keyed override"
        );

        // Mark `opus` as the operator-layer key and fold.
        let operator_keys: BTreeSet<String> = ["opus".to_string()].into_iter().collect();
        fold_operator_overrides(&mut cfg.models, &operator_keys);

        // The built-in shadow is gone; the operator entry kept its values and
        // inherited the built-in's for fields it didn't set.
        assert!(
            !cfg.models.contains_key("claude_opus_4_8"),
            "built-in shadow removed"
        );
        let opus = &cfg.models["opus"];
        assert_eq!(opus.context_soft_limit, Some(500000), "operator value wins");
        assert_eq!(opus.context_hard_limit, Some(750000), "operator value wins");
        assert_eq!(
            opus.max_output_tokens,
            Some(16384),
            "inherited from built-in"
        );
        assert_eq!(opus.family.as_deref(), Some("claude-opus-4"), "inherited");

        // Post-fold: resolve_model now returns the operator's values.
        assert_eq!(
            cfg.resolve_model("claude-opus-4-8").context_soft_limit,
            Some(500000)
        );
        assert_eq!(
            cfg.resolve_model("claude-opus-4-8").context_hard_limit,
            Some(750000)
        );
    }
}

#[cfg(test)]
mod vcbm_effort_tests {
    use super::*;
    use figment::providers::{Format, Toml};
    use figment::Figment;

    #[test]
    fn model_effort_levels_and_default_resolve() {
        let toml = r#"
            [model_rules.claude]
            prefix = "claude-opus"
            effort_levels = ["low", "medium", "high"]
            default_effort = "medium"

            [models.opus]
            model = "claude-opus-4-8"
            effort_levels = ["low", "medium", "high", "xhigh", "max"]
            default_effort = "xhigh"
        "#;
        let cfg: ModelCatalogConfig = Figment::from(Toml::string(toml)).extract().unwrap();
        let s = cfg.resolve_model("claude-opus-4-8");
        assert_eq!(
            s.effort_levels,
            vec!["low", "medium", "high", "xhigh", "max"]
        );
        assert_eq!(s.default_effort.as_deref(), Some("xhigh"));
    }
}
