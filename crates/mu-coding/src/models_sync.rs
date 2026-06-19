//! `mu models sync` / `mu models list` — the orchestration half of the
//! catalog sync (bead context-limit-harden-sync, work item 3).
//!
//! Ties the two halves together: [`mu_ai::catalog_probe`] reaches each
//! provider over HTTP, [`mu_core::catalog_sync`] selects the operator-
//! referenced models and writes the per-provider generated layer. This
//! module owns only the control flow (which providers, best-effort skip,
//! dry-run, printing) — no HTTP, no TOML writing.
//!
//! `sync` is **selection-driven**: it enriches only the models referenced in
//! `models.toml`, so openrouter's ~300-model catalog collapses to the
//! handful actually routed to. `list` is **discovery**: a live query that
//! prints a provider's models and writes nothing.
//!
//! Failure policy mirrors the bead: a provider that errors (unreachable, no
//! key) is **skipped**, leaving its existing generated layer untouched
//! (preserve-last-known). Only providers reached this run are rewritten.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use mu_ai::catalog_probe;
use mu_core::catalog_sync::{self, ProbedModel};
use mu_core::model_catalog;

const OPENROUTER_DEFAULT_BASE: &str = "https://openrouter.ai";

fn openrouter_base() -> String {
    std::env::var("OPENROUTER_API_BASE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| OPENROUTER_DEFAULT_BASE.to_string())
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

/// Providers to sync when `--provider` isn't given: those with credentials /
/// a configured endpoint. ollama is always attempted (local, no key) — if
/// the box is down the probe errors and the provider is skipped.
fn configured_providers() -> Vec<String> {
    let mut v = Vec::new();
    if env_nonempty("OPENROUTER_API_KEY").is_some() {
        v.push("openrouter".to_string());
    }
    if env_nonempty("VLLM_API_BASE").is_some() {
        v.push("vllm".to_string());
    }
    v.push("ollama".to_string());
    v
}

/// Probe one provider for the models in `selection`. openrouter/vllm return
/// their whole catalog in one call (the caller filters); ollama lists ids
/// then issues a per-model `/api/show` ONLY for selected ids, bounding the
/// N+1 to models the operator uses.
async fn probe_provider(
    provider: &str,
    timeout: Duration,
    selection: &BTreeMap<String, String>,
) -> Result<Vec<ProbedModel>> {
    match provider {
        "openrouter" => {
            let key = env_nonempty("OPENROUTER_API_KEY").unwrap_or_default();
            catalog_probe::probe_openrouter(&openrouter_base(), &key, timeout)
                .await
                .map_err(Into::into)
        }
        "vllm" => {
            let base = mu_ai::providers::vllm::base_from_env();
            catalog_probe::probe_vllm(&base, timeout)
                .await
                .map_err(Into::into)
        }
        "ollama" => {
            let base = mu_ai::providers::ollama::base_from_env();
            let ids = catalog_probe::list_ollama(&base, timeout).await?;
            let mut out = Vec::new();
            for id in ids.into_iter().filter(|id| selection.contains_key(id)) {
                match catalog_probe::show_ollama(&base, &id, timeout).await {
                    Ok(m) => out.push(m),
                    // One model's /api/show failing shouldn't sink the rest.
                    Err(e) => tracing::warn!(model = %id, error = %e, "ollama /api/show skipped"),
                }
            }
            Ok(out)
        }
        other => bail!(
            "unknown provider `{other}` (supported: openrouter, vllm, ollama; \
             anthropic/codex limits are static, not probed)"
        ),
    }
}

/// `mu models sync`: probe configured providers and write the per-provider
/// generated catalog layers for operator-referenced models.
pub async fn sync(
    config: Option<PathBuf>,
    only_providers: Vec<String>,
    timeout: Duration,
    dry_run: bool,
) -> Result<()> {
    let op_path = config
        .or_else(model_catalog::default_config_path)
        .context("could not resolve a models.toml path (pass --config)")?;

    let selection = catalog_sync::operator_selection(&model_catalog::load_operator_only(&op_path));
    if selection.is_empty() {
        println!(
            "No models referenced in {} — nothing to sync.\n\
             Add `[models.\"<provider-model-id>\"]` entries to select models to enrich.",
            op_path.display()
        );
        return Ok(());
    }

    let providers = if only_providers.is_empty() {
        configured_providers()
    } else {
        only_providers
    };

    println!(
        "Syncing {} provider(s) against {} selected model(s) in {}{}",
        providers.len(),
        selection.len(),
        op_path.display(),
        if dry_run { "  [dry-run]" } else { "" }
    );

    for provider in &providers {
        match probe_provider(provider, timeout, &selection).await {
            Ok(probed) => {
                let entries = catalog_sync::build_generated_entries(&probed, &selection);
                if dry_run {
                    let keys: Vec<&str> = entries.keys().map(String::as_str).collect();
                    println!(
                        "  {provider}: probed {}, would write {} -> {:?}",
                        probed.len(),
                        entries.len(),
                        keys
                    );
                } else {
                    let path = catalog_sync::write_generated_provider(&op_path, provider, &entries)
                        .with_context(|| format!("writing generated layer for {provider}"))?;
                    println!(
                        "  {provider}: probed {}, wrote {} -> {}",
                        probed.len(),
                        entries.len(),
                        path.display()
                    );
                }
            }
            Err(e) => {
                // Preserve-last-known: leave this provider's existing layer in
                // place rather than clobbering it with nothing.
                println!("  {provider}: skipped (unreachable) — {e}");
            }
        }
    }
    Ok(())
}

/// `mu models list`: live discovery of a provider's models. Prints id +
/// known hard window; writes nothing. ollama lists ids only (cheap — no
/// per-model `/api/show`).
pub async fn list(provider: &str, query: Option<&str>, timeout: Duration) -> Result<()> {
    let mut models: Vec<(String, Option<u64>)> = match provider {
        "openrouter" => {
            let key = env_nonempty("OPENROUTER_API_KEY").unwrap_or_default();
            catalog_probe::probe_openrouter(&openrouter_base(), &key, timeout)
                .await?
                .into_iter()
                .map(|m| (m.id, m.context_hard_limit))
                .collect()
        }
        "vllm" => {
            let base = mu_ai::providers::vllm::base_from_env();
            catalog_probe::probe_vllm(&base, timeout)
                .await?
                .into_iter()
                .map(|m| (m.id, m.context_hard_limit))
                .collect()
        }
        "ollama" => {
            let base = mu_ai::providers::ollama::base_from_env();
            catalog_probe::list_ollama(&base, timeout)
                .await?
                .into_iter()
                .map(|id| (id, None))
                .collect()
        }
        other => bail!("unknown provider `{other}` (supported: openrouter, vllm, ollama)"),
    };

    if let Some(q) = query.map(str::to_ascii_lowercase) {
        models.retain(|(id, _)| id.to_ascii_lowercase().contains(&q));
    }
    models.sort_by(|a, b| a.0.cmp(&b.0));

    for (id, hard) in &models {
        match hard {
            Some(h) => println!("{id}\t(context: {h})"),
            None => println!("{id}"),
        }
    }
    println!("{} model(s)", models.len());
    Ok(())
}
