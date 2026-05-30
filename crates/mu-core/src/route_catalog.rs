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

use crate::pricing;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteEntry {
    pub provider_kind: Arc<str>,
    pub model: Arc<str>,
    pub configured: bool,

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

        let mut entries = Vec::new();

        for (model, soft, hard) in ANTHROPIC_MODELS {
            entries.push(build_entry(
                "anthropic_api",
                model,
                anthropic_key,
                *soft,
                *hard,
            ));
        }

        for (model, soft, hard) in OPENAI_CODEX_MODELS {
            entries.push(build_entry("openai_codex", model, openai_key, *soft, *hard));
        }

        for (model, soft, hard) in OPENROUTER_MODELS {
            entries.push(build_entry(
                "openrouter",
                model,
                openrouter_key,
                *soft,
                *hard,
            ));
        }

        entries.push(build_entry("faux", "faux", true, 128_000, 128_000));

        Self { entries }
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
    context_soft: u64,
    context_hard: u64,
) -> RouteEntry {
    let pricing = pricing::for_model(provider_kind, model);

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

    let hash_input = format!("{provider_kind}:{model}:{context_soft}:{context_hard}");
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
        context_soft_limit: Some(context_soft),
        context_hard_limit: Some(context_hard),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faux_always_configured() {
        let catalog = RouteCatalog::from_env();
        let faux = catalog.find("faux", "faux").expect("faux must exist");
        assert!(faux.configured);
    }

    #[test]
    fn hash_is_deterministic() {
        let a = build_entry("anthropic_api", "claude-opus-4-7", true, 200_000, 1_000_000);
        let b = build_entry("anthropic_api", "claude-opus-4-7", true, 200_000, 1_000_000);
        assert_eq!(a.hash, b.hash);
    }

    #[test]
    fn hash_changes_with_limits() {
        let a = build_entry("anthropic_api", "claude-opus-4-7", true, 200_000, 1_000_000);
        let b = build_entry("anthropic_api", "claude-opus-4-7", true, 500_000, 1_000_000);
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
}
