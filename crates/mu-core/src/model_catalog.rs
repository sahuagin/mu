//! Config-backed provider/model/favorites catalog.
//!
//! Built-in defaults are embedded TOML, optionally overlaid by
//! `~/.config/mu/models.toml` and `MU_MODELS_*` env vars via Figment.
//! This is the configuration half; [`crate::route_catalog`] turns it
//! into provider×model route entries for front ends.

use std::collections::BTreeMap;
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
    pub quirks: Vec<String>,
    pub base_url: Option<String>,
    pub api_path: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
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
    pub quirks: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ModelRuleConfig {
    pub prefix: Option<String>,
    pub prefixes: Vec<String>,
    pub family: Option<String>,
    pub context_soft_limit: Option<u64>,
    pub context_hard_limit: Option<u64>,
    pub max_output_tokens: Option<u32>,
    pub reasoning_in_output: Option<bool>,
    pub quirks: Vec<String>,
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedModelSettings {
    pub label: Option<String>,
    pub aliases: Vec<String>,
    pub family: Option<String>,
    pub context_soft_limit: Option<u64>,
    pub context_hard_limit: Option<u64>,
    pub max_output_tokens: Option<u32>,
    pub reasoning_in_output: Option<bool>,
    pub quirks: Vec<String>,
}

static DEFAULT_CATALOG: OnceLock<ModelCatalogConfig> = OnceLock::new();
static LOADED_CATALOG: OnceLock<ModelCatalogConfig> = OnceLock::new();

pub fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("mu").join("models.toml"))
}

/// Path to the MACHINE-written probed catalog (mu-1gx5): `mu models refresh`
/// writes provider-reported limits here. It layers ABOVE the built-in default
/// but BELOW the user's `models.toml`, so a hand override always wins over a
/// probed value.
pub fn probed_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("mu").join("models.probed.toml"))
}

/// Convert provider-probed models (mu-1gx5) into a catalog fragment suitable
/// for writing to `models.probed.toml`. Each probed model becomes a
/// `[models.<id>]` entry carrying its reported `max_output_tokens` and
/// `context_hard_limit`; the `model` field holds the wire id so `resolve_model`
/// matches it. Models reporting NEITHER limit are skipped — an empty entry adds
/// no signal. Pure (no I/O): the CLI does the serialize + write.
pub fn probed_models_to_catalog(models: &[crate::agent::ProbedModel]) -> ModelCatalogConfig {
    let mut out = ModelCatalogConfig::default();
    for m in models {
        if m.max_output_tokens.is_none() && m.context_length.is_none() {
            continue;
        }
        out.models.insert(
            m.id.clone(),
            ModelCatalogEntry {
                model: Some(m.id.clone()),
                max_output_tokens: m.max_output_tokens,
                context_hard_limit: m.context_length,
                ..Default::default()
            },
        );
    }
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
    // mu-1gx5: probed catalog layers between built-in and the user file, so a
    // machine-probed limit beats the built-in default but a hand override in
    // models.toml still beats the probe.
    if let Some(pp) = probed_config_path() {
        if pp.exists() {
            fig = fig.merge(Toml::file(pp));
        }
    }
    let path = config_path
        .map(Path::to_path_buf)
        .or_else(default_config_path);
    if let Some(p) = path.as_ref() {
        if p.exists() {
            fig = fig.merge(Toml::file(p));
        }
    }
    fig.merge(Env::prefixed("MU_MODELS_").split("__"))
        .extract()
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "invalid model catalog config; using built-in defaults");
            built_in()
        })
}

pub fn global() -> &'static ModelCatalogConfig {
    LOADED_CATALOG.get_or_init(|| load(None))
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
            out.quirks = rule.quirks.clone();
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
            if !m.quirks.is_empty() {
                out.quirks = merge_strings(&out.quirks, &m.quirks);
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
    fn probed_to_catalog_maps_limits_and_skips_empty() {
        use crate::agent::ProbedModel;
        let probed = vec![
            ProbedModel {
                id: "deepseek/deepseek-v4-pro".into(),
                max_output_tokens: Some(384_000),
                context_length: Some(1_048_576),
                capabilities: vec!["tools".into()],
            },
            // Reports neither limit → skipped (no signal to record).
            ProbedModel {
                id: "nothing/useful".into(),
                max_output_tokens: None,
                context_length: None,
                capabilities: vec![],
            },
        ];
        let cat = probed_models_to_catalog(&probed);
        assert_eq!(cat.models.len(), 1, "empty-limit entry must be skipped");
        let e = cat.models.get("deepseek/deepseek-v4-pro").unwrap();
        assert_eq!(e.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
        assert_eq!(e.max_output_tokens, Some(384_000));
        assert_eq!(e.context_hard_limit, Some(1_048_576));
        // resolve_model finds it by the wire id carried in `model`.
        assert_eq!(
            cat.resolve_model("deepseek/deepseek-v4-pro")
                .max_output_tokens,
            Some(384_000)
        );
    }

    // mu-1gx5: the layering ORDER load() relies on — built-in < probed < user.
    #[test]
    fn probed_layer_sits_below_user_override() {
        let built_in = r#"[models.x]
            model = "m"
            max_output_tokens = 4096"#;
        let probed = r#"[models.x]
            model = "m"
            max_output_tokens = 16384"#;
        let user = r#"[models.x]
            max_output_tokens = 32768"#;

        // Probe beats built-in when the user hasn't overridden.
        let no_user: ModelCatalogConfig = Figment::from(Toml::string(built_in))
            .merge(Toml::string(probed))
            .extract()
            .unwrap();
        assert_eq!(no_user.resolve_model("m").max_output_tokens, Some(16384));

        // User override beats the probe (the precedence we promise).
        let with_user: ModelCatalogConfig = Figment::from(Toml::string(built_in))
            .merge(Toml::string(probed))
            .merge(Toml::string(user))
            .extract()
            .unwrap();
        assert_eq!(with_user.resolve_model("m").max_output_tokens, Some(32768));
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
}
