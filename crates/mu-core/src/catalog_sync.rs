//! Catalog sync — the data half of `mu models sync` (bead
//! context-limit-harden-sync, work item 3).
//!
//! This module is deliberately **HTTP-free**: `mu-core` has no network
//! client, so the actual probing lives in `mu-ai`
//! ([`mu_ai::catalog_probe`]). The split mirrors the existing route-catalog
//! seam — `mu-ai` reaches the wire and fills the plain [`ProbedModel`] DTO;
//! this module turns those probed facts into a generated catalog layer.
//! Nothing here (or in the probe) touches the `Provider` trait or the agent
//! loop — the rejected capability-probe branch bolted probing onto
//! `Provider`, dragging network I/O into the hot path; the sync tool is a
//! standalone, run-once utility instead.
//!
//! ## Selection: `models.toml` is the one selection surface
//!
//! [`operator_selection`] reads the operator's `models.toml` and keys every
//! identifier it mentions (table key, `model` field, aliases) back to the
//! table key. The sync writes a generated entry **only** for probed models
//! that match the selection — so a provider's full catalog (openrouter
//! returns ~300 models in one `/v1/models` call) collapses to the handful
//! the operator actually routes to. To add a model, reference it in
//! `models.toml`; the sync fills in the tedious probed numbers.
//!
//! ## The load-bearing safety invariant
//!
//! [`GeneratedModelEntry`] has no `context_soft_limit` field — generated
//! files write the **hard** limit (objective served window) and never the
//! soft limit. The soft limit is policy (operator override or the safe
//! `DEFAULT_COMPACTION_THRESHOLD`) and drives compaction; keeping it out of
//! the probed layer means a wrong/hostile probe can corrupt the *displayed*
//! window but can structurally never crank compaction. That is the exact
//! failure class this bead exists to kill (the 2026-06-19 ollama 32k churn).

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::model_catalog::{generated_path_for_provider, ModelCatalogConfig};

/// One model's probed facts, as reported by a provider's discovery surface.
/// Plain DTO: `mu-ai`'s probe fills it from the wire, this crate's writer
/// consumes it. Every field beyond `id` is optional — a provider that
/// doesn't report a value leaves it `None` (never a fabricated placeholder).
///
/// Note: `context_soft_limit` is intentionally absent. Soft is policy, not a
/// probed fact — see the module docs.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProbedModel {
    /// Provider-native model id (e.g. `anthropic/claude-opus-4.7`,
    /// `qwen3.6:35b-a3b-q8_0`). This is what `models.toml` references.
    pub id: String,
    /// Objective served context window. ollama: baked `num_ctx` only (the
    /// architecture max is *not* the served window). openrouter/vllm:
    /// reported `context_length` / `max_model_len`.
    pub context_hard_limit: Option<u64>,
    /// Hard per-model output cap, if the provider reports one.
    pub max_output_tokens: Option<u32>,
    /// USD per million input tokens (openrouter `pricing.prompt` × 1e6).
    pub pricing_input_per_mtok: Option<f64>,
    /// USD per million output tokens (openrouter `pricing.completion` × 1e6).
    pub pricing_output_per_mtok: Option<f64>,
}

/// A generated `[models."<key>"]` entry — serialize-only, minimal, and with
/// **no** `context_soft_limit` field by construction (see module docs). The
/// pricing fields are written as keys that today's `#[serde(default)]`
/// `ModelCatalogEntry` does not consume (no `deny_unknown_fields`, so they
/// are silently ignored on load); they are captured now for a later struct
/// field to promote, per the bead.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GeneratedModelEntry {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_hard_limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pricing_input_per_mtok: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pricing_output_per_mtok: Option<f64>,
}

#[derive(Serialize)]
struct GeneratedFile<'a> {
    models: &'a BTreeMap<String, GeneratedModelEntry>,
}

/// Map every model identifier the operator declared in `models.toml` — each
/// entry's table key, its `model` field, and any aliases — to the operator's
/// table KEY. The sync writes generated entries under that key so figment's
/// per-field merge lands on the operator's entry (operator wins, generated
/// fills the gaps). Built-in catalog keys are often opaque (`claude_opus_4_7`
/// keys a `model = "claude-opus-4-7"`), so matching by key alone is not
/// enough — hence the `model`/alias indirection.
pub fn operator_selection(cfg: &ModelCatalogConfig) -> BTreeMap<String, String> {
    let mut sel = BTreeMap::new();
    for (key, entry) in &cfg.models {
        // The key itself is a valid id to match (operators may key directly
        // by model id, e.g. `[models."x-ai/grok-3"]`).
        sel.entry(key.clone()).or_insert_with(|| key.clone());
        for alias in &entry.aliases {
            sel.entry(alias.clone()).or_insert_with(|| key.clone());
        }
        // The explicit `model` field is the most authoritative id for this
        // key — let it win any collision with another entry's key/alias.
        if let Some(m) = &entry.model {
            sel.insert(m.clone(), key.clone());
        }
    }
    sel
}

/// Intersect probed models with the operator selection, producing the
/// generated entries to write — keyed by the operator's table key so the
/// per-field merge lands. Probed models the operator didn't reference are
/// dropped (this is the bloat fix). Soft limit is never carried.
pub fn build_generated_entries(
    probed: &[ProbedModel],
    selection: &BTreeMap<String, String>,
) -> BTreeMap<String, GeneratedModelEntry> {
    let mut out = BTreeMap::new();
    for p in probed {
        let Some(key) = selection.get(&p.id) else {
            continue;
        };
        out.insert(
            key.clone(),
            GeneratedModelEntry {
                model: p.id.clone(),
                context_hard_limit: p.context_hard_limit,
                max_output_tokens: p.max_output_tokens,
                pricing_input_per_mtok: p.pricing_input_per_mtok,
                pricing_output_per_mtok: p.pricing_output_per_mtok,
            },
        );
    }
    out
}

fn header(provider: &str) -> String {
    format!(
        "# GENERATED by `mu models sync` for provider `{provider}` — DO NOT EDIT.\n\
         # Hand edits belong in models.toml, which is layered ABOVE this file and\n\
         # wins per field. Re-run `mu models sync` to refresh. Probed values:\n\
         # context_hard_limit (served window), max_output_tokens, pricing. The\n\
         # SOFT limit is policy (operator/default), never probed — so a bad probe\n\
         # cannot drive compaction.\n\n"
    )
}

/// Atomically write `provider`'s generated layer next to `operator_config`
/// as `models.generated.<provider>.toml`. Per-provider files give the sync
/// its "replace this provider, preserve the others on failure" semantics for
/// free: the caller writes only the providers it reached this run and never
/// touches the rest. Wholesale per provider — `entries` is the complete
/// current selected set, so a now-deselected model's stale entry is dropped.
/// An empty set writes a header-only file (clearing any prior entries).
///
/// Atomic via temp-in-same-dir + rename, so a reader (a starting daemon)
/// never observes a half-written file.
pub fn write_generated_provider(
    operator_config: &Path,
    provider: &str,
    entries: &BTreeMap<String, GeneratedModelEntry>,
) -> io::Result<PathBuf> {
    let target = generated_path_for_provider(operator_config, provider);
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;

    let body = if entries.is_empty() {
        String::new()
    } else {
        toml::to_string(&GeneratedFile { models: entries }).map_err(io::Error::other)?
    };
    let contents = format!("{}{}", header(provider), body);

    // Atomic via write-to-sibling-temp + rename (rename is atomic within a
    // dir on the same filesystem). A starting daemon globbing this dir thus
    // never observes a half-written file. std-only — `tempfile` is a
    // dev-dependency here, and the sync is single-writer (one `mu models
    // sync` at a time), so the pid suffix is enough to avoid collision.
    let tmp = target.with_extension(format!("tmp.{}", std::process::id()));
    let write_result = (|| -> io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.flush()?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_catalog;

    fn probed(id: &str, hard: Option<u64>, max_out: Option<u32>) -> ProbedModel {
        ProbedModel {
            id: id.to_string(),
            context_hard_limit: hard,
            max_output_tokens: max_out,
            ..Default::default()
        }
    }

    #[test]
    fn selection_maps_model_field_and_alias_to_key() {
        let toml = r#"
            [models.opus]
            model = "anthropic/claude-opus-4.7"
            aliases = ["opus47"]

            [models."x-ai/grok-3"]
        "#;
        use figment::providers::{Format, Toml};
        let cfg: ModelCatalogConfig = figment::Figment::from(Toml::string(toml))
            .extract()
            .unwrap();
        let sel = operator_selection(&cfg);
        // model field -> key
        assert_eq!(
            sel.get("anthropic/claude-opus-4.7"),
            Some(&"opus".to_string())
        );
        // alias -> key
        assert_eq!(sel.get("opus47"), Some(&"opus".to_string()));
        // bare key (operator keyed directly by id) -> itself
        assert_eq!(sel.get("x-ai/grok-3"), Some(&"x-ai/grok-3".to_string()));
        // unreferenced id is not selected
        assert!(!sel.contains_key("google/gemini-2.5-pro"));
    }

    #[test]
    fn build_entries_keeps_only_selected_and_drops_soft() {
        let mut selection = BTreeMap::new();
        selection.insert("anthropic/claude-opus-4.7".to_string(), "opus".to_string());
        let probed = vec![
            probed("anthropic/claude-opus-4.7", Some(1_000_000), Some(64_000)),
            // not in selection -> dropped (the openrouter bloat fix)
            probed("google/gemini-2.5-pro", Some(1_000_000), None),
        ];
        let entries = build_generated_entries(&probed, &selection);
        assert_eq!(entries.len(), 1, "only the selected model is written");
        let e = entries.get("opus").expect("written under operator key");
        assert_eq!(e.model, "anthropic/claude-opus-4.7");
        assert_eq!(e.context_hard_limit, Some(1_000_000));
        assert_eq!(e.max_output_tokens, Some(64_000));
    }

    #[test]
    fn generated_entry_has_no_soft_limit_field() {
        // Structural guarantee: serializing a generated entry can never emit
        // context_soft_limit, so a probe can't reach the compaction trigger.
        let e = GeneratedModelEntry {
            model: "m".to_string(),
            context_hard_limit: Some(123),
            max_output_tokens: None,
            pricing_input_per_mtok: None,
            pricing_output_per_mtok: None,
        };
        let mut models = BTreeMap::new();
        models.insert("m".to_string(), e);
        let s = toml::to_string(&GeneratedFile { models: &models }).unwrap();
        assert!(s.contains("context_hard_limit = 123"));
        assert!(
            !s.contains("context_soft_limit"),
            "generated layer must never carry a soft limit"
        );
    }

    #[test]
    fn write_then_load_merges_under_operator() {
        // End-to-end: write a generated layer for a provider, then load via
        // the real catalog loader (which globs models.generated.*.toml) and
        // confirm operator wins per field while generated fills the gaps.
        let dir = std::env::temp_dir().join(format!("mu-catalog-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let op = dir.join("models.toml");
        // Operator references opus and sets ONLY a soft limit override.
        std::fs::write(
            &op,
            "[models.opus]\nmodel = \"anthropic/claude-opus-4.7\"\ncontext_soft_limit = 250000\n",
        )
        .unwrap();

        let cfg = model_catalog::load(Some(&op));
        let sel = operator_selection(&cfg);
        let probed = vec![probed(
            "anthropic/claude-opus-4.7",
            Some(1_000_000),
            Some(64_000),
        )];
        let entries = build_generated_entries(&probed, &sel);
        let written = write_generated_provider(&op, "openrouter", &entries).unwrap();
        assert_eq!(written, dir.join("models.generated.openrouter.toml"));

        let merged = model_catalog::load(Some(&op));
        let opus = merged.models.get("opus").expect("opus present");
        // operator's soft override survives
        assert_eq!(opus.context_soft_limit, Some(250_000));
        // generated fills the hard limit + max output the operator omitted
        assert_eq!(opus.context_hard_limit, Some(1_000_000));
        assert_eq!(opus.max_output_tokens, Some(64_000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_selection_writes_header_only_file() {
        let dir = std::env::temp_dir().join(format!("mu-catalog-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let op = dir.join("models.toml");
        let entries = BTreeMap::new();
        let path = write_generated_provider(&op, "vllm", &entries).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# GENERATED"));
        assert!(!text.contains("[models"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
