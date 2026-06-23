//! The CLI surface — the path-tree made typeable.
//!
//! Tuned for a model consumer (t4c = tools4claude): `find` is the semantic FRONT
//! DOOR (you arrive with an intent, not a path); a bare dotted path or prefix
//! invokes / walks once you hold the handle; a path miss is forgiving (it
//! fuzzy-falls-back to `find` with did-you-mean); every read surface speaks
//! `--json`; and we flush "what I ran" before a child writes, so a piped reader
//! never reads results blind.

use crate::capability::Capability;
use crate::catalog;
use crate::path::CapPath;
use crate::rank::{LexicalRanker, Ranked, Ranker};
use crate::registry::{Registry, Tree};
use crate::source::TomlConfigSource;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

/// tools4claude — find, learn, and invoke tools by intent.
#[derive(Parser, Debug)]
// `disable_help_subcommand`: we want `t4c help <path>` to be OUR command (show a
// capability's help), not clap's built-in `help` subcommand. The `--help` flag
// still works. (Without this, clap panics: "command name `help` is duplicated".)
#[command(name = "t4c", version, about, disable_help_subcommand = true)]
pub struct Cli {
    /// Emit JSON instead of human-pretty output (the model-facing surface).
    #[arg(long, global = true)]
    pub json: bool,

    /// Emit t4c's own --help-ai document and exit — t4c is a tool in its own
    /// registry (turtles). Conforming form: `t4c --help-ai --json`.
    #[arg(long = "help-ai", global = true)]
    pub help_ai: bool,

    #[command(subcommand)]
    pub cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Semantic front door: rank capabilities by intent; return paths, a help
    /// pointer (schema), and a suggested call. The model finalizes the args.
    Find {
        /// Free-text intent, e.g. `where is the App struct defined`.
        #[arg(trailing_var_arg = true)]
        intent: Vec<String>,
    },
    /// Walk a subtree, terse (one line per node). No prefix = the whole tree.
    Walk { prefix: Option<String> },
    /// List the curated catalog with present/absent markers (what you could have
    /// vs. what's installed here).
    List,
    /// Scan the curated catalog against this host (catalog ∩ installed), report
    /// present/absent, and persist a self-configured registry (unless --dry-run).
    Discover {
        #[arg(long)]
        dry_run: bool,
    },
    /// Terrain-check the persisted warm-start snapshot against the live world
    /// WITHOUT writing anything: is the snapshot still fresh, and if not, how
    /// badly? Exit 0 = fresh, 1 = stale-equivalent (lossless rediscover),
    /// 2 = stale-degraded (a capability category has no live tool), 3 = missing.
    /// `--diff` expands to the per-capability delta (present only in snapshot vs
    /// only live), tagged with each capability's resolved source.
    Verify {
        #[arg(long)]
        diff: bool,
    },
    /// Run the find-quality benchmark (known-answer intent-sets). `--fake` =
    /// deterministic baseline; default = the live embedder (the mu-d2iy.6 gate).
    Bench {
        #[arg(long)]
        fake: bool,
    },
    /// Show a capability's help (terse by default; `--full` for all, `--schema`
    /// for the raw `--help-ai --json` document).
    Help {
        path: String,
        #[arg(long)]
        full: bool,
        #[arg(long)]
        schema: bool,
    },
    /// Invoke a capability by path: `t4c run bash.jj.status -- [args]`.
    Run {
        path: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Probe a command and PROPOSE an effect classification (+ passthrough flag +
    /// confidence) for it and its subcommands — a heuristic candidate for review,
    /// the floor under the Phase-2 classification grind. Default: emit to stdout
    /// (`--json` for the agent); `--write` appends to the local candidates file.
    Classify {
        /// The command to probe and classify, e.g. `jj` or `curl`. A single token
        /// (NOT a trailing vararg) so `--write` parses in any position.
        cmd: String,
        #[arg(long)]
        write: bool,
    },
    /// Fallback: a bare dotted path (invoke / walk) or a natural intent (find).
    #[command(external_subcommand)]
    Bare(Vec<String>),
}

/// What a bare invocation resolves to. Pure routing, split out so it is
/// unit-testable without spawning anything.
#[derive(Debug, PartialEq, Eq)]
pub enum BareAction {
    Run { path: CapPath, args: Vec<String> },
    Walk { prefix: CapPath },
    Help { path: CapPath, schema: bool },
    Find { intent: String },
}

/// Resolve a bare token list against the tree: exact path → run; valid prefix →
/// walk; otherwise → find (the forgiving fall-through). `--help-ai`/`--schema`
/// after an exact path redirect to help (meta-ops are flags, not path segments).
pub fn route_bare(tree: &Tree, tokens: &[String]) -> BareAction {
    let Some((first, rest)) = tokens.split_first() else {
        return BareAction::Find {
            intent: String::new(),
        };
    };
    let wants_help = rest.iter().any(|t| t == "--help-ai" || t == "--help");
    let wants_schema = rest.iter().any(|t| t == "--schema");

    if let Ok(path) = CapPath::parse(first) {
        if tree.get(&path).is_some() {
            if wants_help || wants_schema {
                return BareAction::Help {
                    path,
                    schema: wants_schema,
                };
            }
            let args = rest
                .iter()
                .filter(|t| *t != "--help-ai" && *t != "--schema")
                .cloned()
                .collect();
            return BareAction::Run { path, args };
        }
        if !tree.walk(&path).is_empty() {
            return BareAction::Walk { prefix: path };
        }
    }
    // A path-shaped miss (has dots) reads better as space-separated words, so the
    // ranker matches individual segments — the forgiving fuzzy-fallback.
    BareAction::Find {
        intent: tokens.join(" ").replace('.', " "),
    }
}

/// Build the default registry: the curated catalog ∩ installed env, optionally
/// overridden by a user-authored TOML at `$T4C_CONFIG` or
/// `~/.config/t4c/overrides.toml`.
///
/// This is the COLD path — [`catalog::EnvCatalogSource`] probes the PATH
/// (`which` per catalogued tool) every time it's built. Read commands should
/// prefer [`warm_registry`], which mmaps the snapshot and skips that probing
/// entirely, falling back here only when the snapshot is stale or missing.
pub fn build_registry() -> Registry {
    let mut reg = Registry::new();
    reg.add_source(Box::new(catalog::EnvCatalogSource));
    reg.add_source(Box::new(crate::chain::ChainSource::new(
        catalog::default_chains(),
    )));
    if let Some(cfg) = overrides_path() {
        if cfg.exists() {
            reg.add_source(Box::new(TomlConfigSource::new(cfg)));
        }
    }
    reg
}

/// Build a registry WITHOUT probing the environment, by loading the rkyv
/// warm-start snapshot (mu-2332). On a fresh snapshot the present-set comes from
/// the archive — zero `which` calls — and chains + TOML overrides are re-layered
/// on top (cheap, and authoritative-last as before). On a stale/missing snapshot
/// this returns `None` and the caller falls back to the cold [`build_registry`].
///
/// Returns the registry AND the snapshot's archived embedding vectors (as a
/// [`VectorCache`]) when present, so warm `find` ranks semantically straight
/// from the archive — fulfilling the contract that a warm start needs neither
/// the JSON vector cache nor a re-embed. An empty vector set yields `None` for
/// the cache (no embedder ran at discover) and find degrades to lexical.
///
/// The snapshot is CACHE: a hash/schema mismatch is silently treated as a miss,
/// never an error, so a changed catalog or PATH self-heals on the next
/// `discover` without ever surfacing a failure to the user.
fn warm_registry() -> Option<(Registry, Option<crate::semantic::VectorCache>)> {
    let path = crate::snapshot::Snapshot::default_path()?;
    let catalog_hash = crate::snapshot::catalog_content_hash(&catalog_files());
    let path_hash = crate::snapshot::path_set_hash();
    match crate::snapshot::load_valid(&path, &catalog_hash, &path_hash) {
        crate::snapshot::LoadOutcome::Fresh {
            caps,
            vectors,
            model,
        } => {
            let mut reg = Registry::new();
            // Archived present-set stands in for EnvCatalogSource AND the
            // resolved ChainSource (both folded into the snapshot at discover) —
            // so the warm path probes ZERO commands. Only TOML overrides, which
            // are read-from-disk not probed, re-layer on top.
            reg.add_source(Box::new(crate::source::StaticSource::new("snapshot", caps)));
            if let Some(cfg) = overrides_path() {
                if cfg.exists() {
                    reg.add_source(Box::new(TomlConfigSource::new(cfg)));
                }
            }
            // Surface the archived vectors so the warm ranker uses them directly
            // instead of re-reading vectors.json. Empty => no embedder at
            // discover => no cache => lexical fallback (same as cold).
            let cache = if vectors.is_empty() {
                None
            } else {
                Some(crate::semantic::VectorCache {
                    model,
                    by_path: vectors,
                })
            };
            Some((reg, cache))
        }
        _ => None, // stale or missing -> caller does a cold build (self-heal on next discover)
    }
}

/// The registry for read commands, plus an optional in-memory vector cache from
/// the snapshot. Warm (snapshot, no probing, vectors-from-archive) when
/// available, else cold (probes, vectors-from-`vectors.json`-if-present). This
/// is the function `find`/`walk`/banner go through so the common path is
/// zero-probe and serves its own semantic vectors.
fn read_registry() -> (Registry, Option<crate::semantic::VectorCache>) {
    warm_registry().unwrap_or_else(|| (build_registry(), None))
}

fn overrides_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("T4C_CONFIG") {
        return Some(PathBuf::from(p));
    }
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config/t4c/overrides.toml"))
}

/// Where `discover` writes its self-configured registry (machine output, NOT
/// hand-edited). `$T4C_SELF_CONFIG` or `~/.cache/t4c/registry.toml`.
///
/// This is deliberately DISTINCT from [`overrides_path`]: discover overwrites
/// this file wholesale on every run, so it must never be the same file a user
/// hand-authors their additions into. Conflating the two (the pre-mu-2332 bug)
/// meant `discover` silently clobbered user overrides. Output lives in the cache
/// dir (regenerable); input lives in the config dir (durable, user-owned).
fn self_config_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("T4C_SELF_CONFIG") {
        return Some(PathBuf::from(p));
    }
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".cache/t4c/registry.toml"))
}

/// Where the catalog vector cache lives (`$T4C_VECTORS` or `~/.cache/t4c/vectors.json`).
fn vectors_path() -> PathBuf {
    if let Ok(p) = std::env::var("T4C_VECTORS") {
        return PathBuf::from(p);
    }
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".cache/t4c/vectors.json"))
        .unwrap_or_else(|_| PathBuf::from("vectors.json"))
}

/// Rank with semantic embeddings when vectors AND a live embedder are both
/// available AND their embedding spaces match; otherwise lexical. `find` never
/// re-embeds the catalog — vectors were built at `discover`. The preferred
/// vector source is `snapshot_cache` (the archive loaded by the warm path); it
/// falls back to the on-disk `vectors.json` only when the warm path didn't
/// supply one (cold start). Only the intent is embedded here (one call).
///
/// CRITICAL: the archived vectors carry the model that produced them, and we
/// only cosine-compare against the live query embedder when the two models
/// MATCH. If `$T4C_EMBED_MODEL` changed since discover (while catalog/PATH
/// hashes stayed the same, so the snapshot is still "fresh"), the archived
/// vectors live in a different embedding space — comparing them to fresh query
/// embeddings yields meaningless scores. On mismatch we drop to lexical, which
/// is correct-but-degraded rather than confidently-wrong (reviewer finding).
fn rank_caps<'a>(
    intent: &str,
    caps: &[&'a Capability],
    snapshot_cache: Option<&crate::semantic::VectorCache>,
) -> Vec<Ranked<'a>> {
    // Prefer the snapshot's in-memory cache; else the JSON cache on disk. Carry
    // the model alongside the vectors so we can verify the embedding space.
    let cache: Option<crate::semantic::VectorCache> = match snapshot_cache {
        Some(c) => Some(c.clone()),
        None => crate::semantic::VectorCache::load(&vectors_path()).ok(),
    };
    if let Some(cache) = cache {
        if let Ok(emb) = crate::embedder::ConfigEmbedder::from_config() {
            if emb.model() == cache.model {
                return crate::semantic::SemanticRanker::new(emb, cache.by_path).rank(intent, caps);
            }
            // Model drift: archived vectors are a different embedding space.
            // Lexical is honest; a rediscover (re-embed) will heal the cache.
            eprintln!(
                "t4c: embed model changed ({} archived vs {} live) — \
                 ranking lexically; run `t4c discover` to re-embed",
                cache.model,
                emb.model()
            );
        }
    }
    LexicalRanker.rank(intent, caps)
}

/// Embed the active catalog and persist the vector cache (the compile step for
/// semantic `find`). Returns the built [`VectorCache`] (path + vectors) when an
/// embedder is configured and reachable; `None` if not (find then stays lexical
/// — the live endpoint is the mu-d2iy.6 gate). The caller folds the returned
/// vectors into the rkyv snapshot so a warm `find` needs neither this JSON cache
/// nor a re-embed.
fn build_vector_cache() -> Result<Option<(PathBuf, crate::semantic::VectorCache)>> {
    let emb = match crate::embedder::ConfigEmbedder::from_config() {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };
    let tree = build_registry().build()?;
    let caps: Vec<&Capability> = tree.all().collect();
    let model = std::env::var("T4C_EMBED_MODEL")
        .unwrap_or_else(|_| crate::embedder::ConfigEmbedder::DEFAULT_MODEL.to_string());
    match crate::semantic::VectorCache::build(&emb, &model, &caps) {
        Ok(cache) => {
            let path = vectors_path();
            cache.save(&path)?;
            Ok(Some((path, cache)))
        }
        Err(_) => Ok(None), // endpoint down/wrong — the gate fixes the endpoint
    }
}

/// `list` — preference chains resolved 3-state (active / superseded / absent),
/// plus the distinct catalog (installed / absent). Unlike `walk`, which shows
/// only the live installed tree.
fn do_list(json: bool) -> Result<i32> {
    let chains = catalog::default_chains();
    let (_winners, resolved) = crate::chain::resolve_chains(&chains, catalog::which)?;
    let curated = catalog::curated();

    if json {
        #[derive(Serialize)]
        struct ChainRow {
            slot: String,
            impl_cmd: String,
            state: String,
            behind: Option<String>,
        }
        #[derive(Serialize)]
        struct CatRow {
            path: String,
            summary: String,
            installed: bool,
        }
        #[derive(Serialize)]
        struct Out {
            chains: Vec<ChainRow>,
            catalog: Vec<CatRow>,
        }
        let chain_rows = resolved
            .iter()
            .map(|r| {
                let (state, behind) = match &r.state {
                    crate::chain::ImplState::Active => ("active".to_string(), None),
                    crate::chain::ImplState::Superseded { behind } => {
                        ("superseded".to_string(), Some(behind.clone()))
                    }
                    crate::chain::ImplState::Absent => ("absent".to_string(), None),
                };
                ChainRow {
                    slot: r.slot.clone(),
                    impl_cmd: r.impl_cmd.clone(),
                    state,
                    behind,
                }
            })
            .collect();
        let cat_rows = curated
            .iter()
            .map(|c| CatRow {
                path: c.path.to_string(),
                summary: c.summary.clone(),
                installed: catalog::is_installed(c),
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&Out {
                chains: chain_rows,
                catalog: cat_rows
            })?
        );
        return Ok(0);
    }

    println!("chains (preference-resolved):");
    for r in &resolved {
        let (mark, note) = match &r.state {
            crate::chain::ImplState::Active => ("✓", String::new()),
            crate::chain::ImplState::Superseded { behind } => {
                ("⊘", format!("  (superseded by {behind})"))
            }
            crate::chain::ImplState::Absent => ("·", "  (absent)".to_string()),
        };
        println!("  {mark} {:<16} {:<8}{}", r.slot, r.impl_cmd, note);
    }
    println!("\ncatalog (distinct tools):");
    for c in &curated {
        let mark = if catalog::is_installed(c) {
            "✓"
        } else {
            "·"
        };
        println!("  {mark} {:<26} {}", c.path.to_string(), c.summary);
    }
    println!("\n(✓ active/installed  ⊘ superseded  · absent — `t4c discover` to persist)");
    Ok(0)
}

/// `discover` — catalog ∩ env: report present/absent and persist the installed
/// set as a self-configured registry (unless `--dry-run`). Note: probing
/// `--help-ai` is deliberately deferred to the first `help` call, not done here,
/// so discovery never spawns N subprocesses just to enumerate.
fn do_discover(json: bool, dry_run: bool) -> Result<i32> {
    let (present, absent): (Vec<Capability>, Vec<Capability>) = catalog::curated()
        .into_iter()
        .partition(catalog::is_installed);

    let wrote = if dry_run {
        None
    } else {
        Some(write_registry(&present)?)
    };
    let cache_wrote = if dry_run {
        None
    } else {
        build_vector_cache().unwrap_or(None)
    };

    // mu-2332 part 2/3: persist the rkyv warm-start snapshot. This is the ONLY
    // place probing + embedding happen; every later `find` mmaps this instead.
    // The snapshot folds in whatever vectors build_vector_cache produced (empty
    // if no live embedder — find then degrades to lexical, same as today).
    //
    // The snapshot's present-set = env-installed catalog ∩ PLUS the resolved
    // chain winners (chains layered last so they win on path collision, exactly
    // as the cold registry orders them). Folding the resolved chains in here is
    // what lets warm_registry drop the live ChainSource and probe ZERO commands.
    let snapshot_wrote = if dry_run {
        None
    } else {
        let mut present_for_snapshot = present.clone();
        if let Ok((chain_caps, _)) =
            crate::chain::resolve_chains(&catalog::default_chains(), catalog::which)
        {
            present_for_snapshot.extend(chain_caps);
        }
        let vectors = cache_wrote
            .as_ref()
            .map(|(_, c)| c.by_path.clone())
            .unwrap_or_default();
        let model = cache_wrote
            .as_ref()
            .map(|(_, c)| c.model.clone())
            .unwrap_or_default();
        write_snapshot(&present_for_snapshot, &vectors, &model)
            .ok()
            .flatten()
    };

    if json {
        #[derive(Serialize)]
        struct Disc {
            present: Vec<String>,
            absent: Vec<String>,
            wrote: Option<String>,
            snapshot: Option<String>,
            dry_run: bool,
        }
        let d = Disc {
            present: present.iter().map(|c| c.path.to_string()).collect(),
            absent: absent.iter().map(|c| c.path.to_string()).collect(),
            wrote: wrote.as_ref().map(|p| p.display().to_string()),
            snapshot: snapshot_wrote.as_ref().map(|p| p.display().to_string()),
            dry_run,
        };
        println!("{}", serde_json::to_string_pretty(&d)?);
        return Ok(0);
    }

    println!(
        "discovered {} present, {} absent (of {} catalogued)",
        present.len(),
        absent.len(),
        present.len() + absent.len()
    );
    for c in &present {
        println!("  ✓ {:<28} {}", c.path.to_string(), c.summary);
    }
    for c in &absent {
        println!(
            "  · {:<28} {}  (not installed)",
            c.path.to_string(),
            c.summary
        );
    }
    match &wrote {
        Some(p) => println!("\nwrote self-configured registry: {}", p.display()),
        None => println!("\n(dry-run — no registry written)"),
    }
    if dry_run {
        println!("(dry-run — vector cache not built)");
    } else if let Some((p, _)) = &cache_wrote {
        println!("embedded + cached catalog vectors: {}", p.display());
    } else {
        println!("(no live embedder/endpoint — semantic find disabled; lexical fallback)");
    }
    match &snapshot_wrote {
        Some(p) => println!("wrote warm-start snapshot: {}", p.display()),
        None if dry_run => println!("(dry-run — snapshot not written)"),
        None => println!("(snapshot not written — no HOME/$T4C_SNAPSHOT)"),
    }
    Ok(0)
}

/// The live PROBED present-set t4c would archive right now, by capability path,
/// each tagged with the source it resolved from. Recomputed exactly as
/// `discover` builds the *snapshot* present-set: curated catalog ∩ installed
/// (`catalog`) plus resolved chain winners (`chain`). Pure read — probes PATH
/// via `which` but writes nothing. The `BTreeMap` gives a stable, sorted path
/// set for diffing against the snapshot.
///
/// Deliberately EXCLUDES the user override layer. Overrides re-layer at read
/// time and are intentionally not baked into the snapshot body — they're
/// fingerprinted into `catalog_hash` instead, so editing one flips `verify` to
/// stale via the hash verdict (the correct signal). Including them here would
/// make every override show as a phantom "rediscover would gain" delta even
/// immediately after a clean discover, because the snapshot never archived them.
/// So `verify` diffs like-for-like (probed-vs-archived) and lets the hash
/// verdict carry override drift.
fn live_present_sources() -> std::collections::BTreeMap<String, String> {
    use std::collections::BTreeMap;
    let mut by_path: BTreeMap<String, String> = BTreeMap::new();

    // catalog ∩ installed
    for c in catalog::curated().into_iter().filter(catalog::is_installed) {
        by_path.insert(c.path.to_string(), "catalog".to_string());
    }
    // resolved chain winners (chains layer last, so they win on path collision)
    if let Ok((chain_caps, _)) =
        crate::chain::resolve_chains(&catalog::default_chains(), catalog::which)
    {
        for c in chain_caps {
            by_path.insert(c.path.to_string(), "chain".to_string());
        }
    }
    by_path
}

/// `verify` — terrain-check the persisted snapshot against the live world,
/// WRITING NOTHING. Answers "is the warm-start snapshot still fresh, and if not,
/// how badly?" without overwriting the snapshot or config (unlike `discover`).
///
/// Exit code is the cheap machine gate — a caller checks `$?` and only parses
/// output (or runs `--diff`) when it's nonzero:
///
/// | verdict           | exit | meaning                                            |
/// |-------------------|------|----------------------------------------------------|
/// | `fresh`           | 0    | hashes match AND no new tools — no work            |
/// | `stale-equivalent`| 1    | world moved, but every snapshot capability still   |
/// |                   |      | resolves to *something* — rediscover is LOSSLESS   |
/// | `stale-augmentable`| 1   | hashes match, but a tool was installed since        |
/// |                   |      | discover — rediscover only GAINS (nothing lost)    |
/// | `stale-degraded`  | 2    | world moved AND ≥1 capability path has no live      |
/// |                   |      | implementation — rediscover LOSES it               |
/// | `missing`         | 3    | no snapshot — cold start needed                     |
///
/// `stale-augmentable` is the case the hash checks structurally can't catch: a
/// catalogued tool absent at discover, later installed into an EXISTING PATH
/// directory (path-set hash unchanged, and it wasn't in the present-set to be
/// tool-fingerprinted). `verify` catches it because it recomputes the live
/// present-set — which is exactly why this lives in `verify` (an explicit
/// opt-in check) and NOT on the warm `find` load path (which must stay
/// zero-probe). A reviewer flagged the gap; the answer is "verify detects it,
/// find doesn't pay for it."
///
/// Severity is monotonic (`$? -ge 2` = "a capability is gone"; `-eq 0` = skip).
/// Crucially, the equivalent/degraded split is judged on capability PATHS, not
/// tool identity: `bash.search` filled by `rg` here and `grep` in CI is still
/// `stale-equivalent` because the path survives — which is what makes a verify
/// assertion portable across hosts (FreeBSD dev box ↔ Ubuntu Actions runner).
/// `--diff` expands the per-capability delta, each tagged with its source.
fn do_verify(json: bool, diff: bool) -> Result<i32> {
    let Some(path) = crate::snapshot::Snapshot::default_path() else {
        if json {
            println!(
                r#"{{"verdict":"missing","reason":"no snapshot path (no HOME/$T4C_SNAPSHOT)"}}"#
            );
        } else {
            println!("verify: missing — no snapshot path (set HOME or $T4C_SNAPSHOT)");
        }
        return Ok(3);
    };

    let catalog_hash = crate::snapshot::catalog_content_hash(&catalog_files());
    let path_hash = crate::snapshot::path_set_hash();
    let outcome = crate::snapshot::load_valid(&path, &catalog_hash, &path_hash);

    let live = live_present_sources();

    // The snapshot's archived present-set (paths), read REGARDLESS of freshness
    // via archived_paths — so a stale snapshot (hash mismatch) still yields its
    // categories, which is exactly what we need to judge equivalent-vs-degraded.
    // (load_valid's Fresh.caps would be empty on any stale load, defeating the
    // degraded check.) None => unreadable/corrupt archive => no categories to
    // compare; the load outcome below still classifies it.
    let snapshot_paths: Vec<String> = crate::snapshot::archived_paths(&path).unwrap_or_default();

    // Capability paths in the snapshot that NO live tool fills now = lost
    // categories. (Path-based, so a swapped implementation isn't counted.)
    let only_snapshot: Vec<String> = snapshot_paths
        .iter()
        .filter(|p| !live.contains_key(*p))
        .cloned()
        .collect();

    // Live paths absent from the snapshot (a rediscover would GAIN these — e.g.
    // a catalogued tool that was absent at discover and has since been installed
    // into an existing PATH dir, which the hash checks can't see). Computed
    // before the verdict so it can upgrade an otherwise-Fresh load.
    let snap_set: std::collections::BTreeSet<&String> = snapshot_paths.iter().collect();
    let only_live: Vec<(&String, &String)> =
        live.iter().filter(|(p, _)| !snap_set.contains(p)).collect();

    let (verdict, reason, exit): (&str, Option<String>, i32) = match &outcome {
        crate::snapshot::LoadOutcome::Fresh { .. } => {
            // Hashes match — but the live present-set may have tools the snapshot
            // lacks (newly installed since discover). That's not degraded (we
            // lost nothing) and not a hard "moved" — it's augmentable: rediscover
            // would GAIN capabilities, losslessly. Exit 1 (rediscover-worthwhile),
            // distinguished from stale-equivalent by verdict string + only_live.
            if only_live.is_empty() {
                ("fresh", None, 0)
            } else {
                (
                    "stale-augmentable",
                    Some(format!(
                        "{} tool(s) installed since discover — rediscover would gain them",
                        only_live.len()
                    )),
                    1,
                )
            }
        }
        crate::snapshot::LoadOutcome::Stale(why) => {
            if only_snapshot.is_empty() {
                ("stale-equivalent", Some(why.clone()), 1)
            } else {
                ("stale-degraded", Some(why.clone()), 2)
            }
        }
        crate::snapshot::LoadOutcome::Missing => ("missing", None, 3),
    };

    if json {
        #[derive(Serialize)]
        struct DeltaEntry {
            path: String,
            source: String,
        }
        #[derive(Serialize)]
        struct VerifyOut {
            verdict: String,
            reason: Option<String>,
            snapshot_path: String,
            /// snapshot paths NO live tool fills now — the lost categories that
            /// make a verdict `stale-degraded`.
            dropped: Vec<String>,
            /// in live recompute but NOT in snapshot (rediscover would gain).
            only_live: Vec<DeltaEntry>,
        }
        let out = VerifyOut {
            verdict: verdict.to_string(),
            reason,
            snapshot_path: path.display().to_string(),
            dropped: only_snapshot.clone(),
            only_live: only_live
                .iter()
                .map(|(p, s)| DeltaEntry {
                    path: (*p).clone(),
                    source: (*s).clone(),
                })
                .collect(),
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(exit);
    }

    // Human form.
    match (verdict, &reason) {
        ("fresh", _) => println!(
            "verify: fresh — snapshot matches the live world ({})",
            path.display()
        ),
        ("stale-equivalent", Some(why)) => println!(
            "verify: STALE (equivalent) — {why}; every capability still resolves, rediscover is lossless"
        ),
        ("stale-augmentable", Some(why)) => println!(
            "verify: STALE (augmentable) — {why}; nothing lost, rediscover only adds"
        ),
        ("stale-degraded", Some(why)) => println!(
            "verify: STALE (degraded) — {why}; {} capability path(s) have no live tool",
            only_snapshot.len()
        ),
        ("missing", _) => println!("verify: missing — no snapshot at {}", path.display()),
        _ => {}
    }

    if diff {
        if only_live.is_empty() && only_snapshot.is_empty() {
            println!("  diff: none — snapshot and live present-set are identical");
        } else {
            for p in &only_snapshot {
                println!("  - {p:<28} (snapshot only, NO live tool) — capability lost");
            }
            for (p, src) in &only_live {
                println!("  + {p:<28} (live only, source: {src}) — rediscover would gain");
            }
        }
    }
    Ok(exit)
}

/// `bench` — run the find-quality benchmark. `--fake` uses the deterministic
/// embedder (CI baseline); default uses the live embedder (the mu-d2iy.6 gate).
fn do_bench(json: bool, fake: bool) -> Result<i32> {
    let report = if fake {
        crate::bench::run(crate::embedder::FakeEmbedder::new())?
    } else {
        match crate::embedder::ConfigEmbedder::from_config() {
            Ok(e) => crate::bench::run(e)?,
            Err(_) => {
                eprintln!(
                    "no embedder configured (~/.config/agent/config.toml); \
                     use `t4c bench --fake` for the deterministic baseline"
                );
                return Ok(2);
            }
        }
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(0);
    }
    let kind = if fake {
        "fake (deterministic baseline)"
    } else {
        "live embedder"
    };
    println!(
        "find benchmark [{kind}]: {}/{} correct\n",
        report.passed, report.total
    );
    for r in &report.results {
        let mark = if r.ok { "✓" } else { "✗" };
        if r.ok {
            println!("  {mark} {:<46} -> {}", r.intent, r.got);
        } else {
            println!(
                "  {mark} {:<46} -> {}  (expected {})",
                r.intent, r.got, r.expect
            );
        }
    }
    Ok(if report.passed == report.total { 0 } else { 1 })
}

/// Review-only banner prepended to the persisted registry. The registry is a
/// generated EXPORT, not an input: t4c never reads it back at runtime (read
/// commands warm-start from the rkyv snapshot, or live-probe PATH — see
/// [`read_registry`]), and `discover` overwrites it wholesale. It shares
/// `overrides.toml`'s TOML grammar, so without this banner it reads as
/// hand-editable; the banner sends edits to the override layer instead.
const REGISTRY_BANNER: &str = "\
# t4c registry — GENERATED by `t4c discover` (catalog ∩ installed on this host).
#
# REVIEW ONLY. t4c does NOT read this file at runtime — read commands warm-start
# from the rkyv snapshot (snapshot.rkyv) beside it, or live-probe PATH. This is a
# human-readable export of the last discover. Edits here are NOT read and are
# OVERWRITTEN on the next `discover`.
#
# To change what t4c knows, edit your override layer instead:
#   ~/.config/t4c/overrides.toml   (operator-local; never clobbered by discover)
# See crates/t4c/AGENTS.md for the full model.

";

const CLASSIFY_BANNER: &str = "\
# t4c classify — PROPOSED effect classifications. UNVERIFIED: a deterministic
# heuristic floor, NOT a verdict. Review (or let the grind agent refine with a
# model) before trusting — low-confidence rows especially. PASSTHROUGH rows run
# arbitrary commands; their effects are invocation-determined and the worst case
# is emitted, so gate them at the shell boundary, not by this label.

";

/// One classified capability candidate: the proposed [`Capability`] (with
/// `effects` set) plus the markers that don't live on `Capability`.
struct Candidate {
    cap: Capability,
    passthrough: bool,
    confidence: crate::classify::Confidence,
}

/// `T4C_CANDIDATES` or `~/.config/t4c/candidates.toml` — the `--write` sink.
/// Deliberately NOT the shipped catalog or the override input: candidates are
/// unverified proposals; promoting one is a reviewed step the agent/human takes.
fn candidates_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("T4C_CANDIDATES") {
        return Some(PathBuf::from(p));
    }
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config/t4c/candidates.toml"))
}

/// Try `<cmd> --help-ai --json`; `None` if absent/non-conforming.
fn probe_help_ai(cmd: &str) -> Option<crate::capability::HelpAiDoc> {
    let out = Command::new(cmd)
        .arg("--help-ai")
        .arg("--json")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// Capture plain `<cmd> --help` text (a heuristic signal source, not parsed
/// structure). Some tools print help to stderr; fall back to it. Capped so a
/// pathological help page can't bloat the signal.
fn capture_help(cmd: &str) -> Option<String> {
    let out = Command::new(cmd).arg("--help").output().ok()?;
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    if s.trim().is_empty() {
        s = String::from_utf8_lossy(&out.stderr).into_owned();
    }
    if s.trim().is_empty() {
        return None;
    }
    Some(s.chars().take(8000).collect())
}

/// Run the heuristic over one probed capability (token = its last path segment),
/// stamping the proposed effects onto a clone.
fn classify_cap(mut cap: Capability, token: &str, haystack: &str) -> Candidate {
    let cls = crate::classify::classify(token, haystack);
    cap.effects = Some(cls.effects);
    Candidate {
        cap,
        passthrough: cls.passthrough,
        confidence: cls.confidence,
    }
}

/// Probe a command and classify it (+ subcommands, if it speaks `--help-ai`).
fn probe_and_classify(cmd: &str) -> Result<Vec<Candidate>> {
    if let Some(doc) = probe_help_ai(cmd) {
        let caps = crate::source::HelpAiProbeSource::doc_to_caps("bash", cmd, doc)?;
        return Ok(caps
            .into_iter()
            .map(|cap| {
                let token = cap
                    .path
                    .to_string()
                    .rsplit('.')
                    .next()
                    .unwrap_or("")
                    .to_string();
                let hay = format!("{token} {} {}", cap.summary, cap.keywords.join(" "));
                classify_cap(cap, &token, &hay)
            })
            .collect());
    }
    // Non-conforming: classify the single tool node off its plain --help text.
    let Some(help) = capture_help(cmd) else {
        return Ok(Vec::new());
    };
    // Pick the first useful line as the summary, skipping the option-error noise
    // BSD base tools emit when they don't grok `--help` (e.g. `cat: illegal
    // option -- -`, then a `usage:` synopsis). Classification still comes from
    // the name/keyword signal; this just keeps the candidate's summary honest.
    let looks_like_opt_error = |l: &str| {
        let l = l.to_lowercase();
        l.contains("illegal option")
            || l.contains("unknown option")
            || l.contains("invalid option")
            || l.contains("unrecognized option")
    };
    let summary = help
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !looks_like_opt_error(l))
        .unwrap_or("")
        .to_string();
    let cap = Capability {
        path: CapPath::parse(&format!("bash.{cmd}"))?,
        summary,
        keywords: vec![],
        priority: 0,
        invoke: vec![cmd.to_string()],
        help: Some(crate::capability::HelpSpec {
            argv: vec![cmd.to_string(), "--help".to_string()],
            ai: false,
        }),
        requires: vec![],
        effects: None,
    };
    let hay = format!("{cmd} {help}");
    Ok(vec![classify_cap(cap, cmd, &hay)])
}

/// `t4c classify <cmd>`: probe + PROPOSE effect classifications. Emits candidate
/// `[[capability]]` entries (+ confidence/passthrough markers) for review; never
/// writes the shipped catalog. Exit 0 = emitted, 1 = unprobeable.
fn do_classify(cmd: &str, write: bool, json: bool) -> Result<i32> {
    let candidates = probe_and_classify(cmd)?;
    if candidates.is_empty() {
        eprintln!("t4c classify: could not probe `{cmd}` (absent, or no --help/--help-ai)");
        return Ok(1);
    }

    if json {
        let arr: Vec<_> = candidates
            .iter()
            .map(|c| {
                serde_json::json!({
                    "path": c.cap.path.to_string(),
                    "summary": c.cap.summary,
                    "invoke": c.cap.invoke,
                    "effects": c.cap.effects,
                    "passthrough": c.passthrough,
                    "confidence": c.confidence.as_str(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "candidates": arr }))?
        );
    } else {
        print!("{CLASSIFY_BANNER}");
        for c in &candidates {
            let tag = if c.passthrough {
                " PASSTHROUGH (effects worst-case; gate at shell boundary)"
            } else {
                ""
            };
            println!(
                "# {}  [confidence={}]{tag}",
                c.cap.path,
                c.confidence.as_str()
            );
        }
        println!();
        let caps: Vec<Capability> = candidates.iter().map(|c| c.cap.clone()).collect();
        print!("{}", TomlConfigSource::to_toml(&caps)?);
    }

    if write {
        let caps: Vec<Capability> = candidates.iter().map(|c| c.cap.clone()).collect();
        let path = candidates_path().context("no candidates path (set HOME or T4C_CANDIDATES)")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating candidates dir {}", parent.display()))?;
        }
        let body = format!("{CLASSIFY_BANNER}{}", TomlConfigSource::to_toml(&caps)?);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening candidates {}", path.display()))?;
        f.write_all(body.as_bytes())
            .with_context(|| format!("appending candidates {}", path.display()))?;
        eprintln!(
            "t4c classify: appended {} candidate(s) to {}",
            caps.len(),
            path.display()
        );
    }
    Ok(0)
}

/// The full registry document as persisted: the review-only [`REGISTRY_BANNER`]
/// followed by the serialized capabilities. Kept distinct from
/// [`TomlConfigSource::to_toml`] so the serializer stays pure (and its
/// round-trip test unaffected) — only the on-disk artifact carries the banner.
fn registry_document(caps: &[Capability]) -> Result<String> {
    Ok(format!(
        "{REGISTRY_BANNER}{}",
        TomlConfigSource::to_toml(caps)?
    ))
}

/// Persist a capability set to the self-config path as TOML (the
/// self-configuring half of `discover`). Writes to [`self_config_path`] — the
/// cache-dir output file — NOT the user's override layer, so a discover run
/// never clobbers hand-authored additions.
fn write_registry(caps: &[Capability]) -> Result<PathBuf> {
    let path = self_config_path().context("no self-config path (set HOME or T4C_SELF_CONFIG)")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating self-config dir {}", parent.display()))?;
    }
    let text = registry_document(caps)?;
    std::fs::write(&path, text).with_context(|| format!("writing registry {}", path.display()))?;
    Ok(path)
}

/// The catalog files whose content fingerprints the snapshot: the user-authored
/// override TOML if it exists (the baked-in default is hashed unconditionally
/// inside [`crate::snapshot::catalog_content_hash`]). We hash the OVERRIDE
/// (input) layer, not the self-config output — editing the input is what should
/// invalidate the snapshot; the output is derived and would create a
/// hash-of-its-own-result feedback loop.
fn catalog_files() -> Vec<PathBuf> {
    overrides_path()
        .filter(|p| p.exists())
        .into_iter()
        .collect()
}

/// Persist the rkyv warm-start snapshot (mu-2332 part 2/3). Returns the path
/// written, or `None` if there's nowhere to write it (no HOME / $T4C_SNAPSHOT).
/// Fingerprints the current world (catalog content + PATH set) so a later load
/// can tell fresh from stale.
fn write_snapshot(
    present: &[Capability],
    vectors: &std::collections::HashMap<String, Vec<f32>>,
    model: &str,
) -> Result<Option<PathBuf>> {
    let Some(path) = crate::snapshot::Snapshot::default_path() else {
        return Ok(None);
    };
    let catalog_hash = crate::snapshot::catalog_content_hash(&catalog_files());
    let path_hash = crate::snapshot::path_set_hash();
    let snap = crate::snapshot::Snapshot::build(present, vectors, model, catalog_hash, path_hash);
    snap.save(&path)?;
    Ok(Some(path))
}

/// Dispatch a parsed [`Cli`] to its handler. Returns the process exit code.
pub fn run(cli: Cli) -> Result<i32> {
    if cli.help_ai {
        // Self-registration via clap-catalog — the single canonical clap→JSON
        // introspector. t4c is a tool in its own registry (the turtle): this
        // document is consumed by HelpAiProbeSource exactly like any other
        // tool's `--help-ai`. (Replaced the hand-rolled helpai::from_clap.)
        let cat = clap_catalog::catalog::<Cli>();
        println!("{}", serde_json::to_string_pretty(&cat)?);
        return Ok(0);
    }
    // Commands that DON'T need the resolved registry tree dispatch first, before
    // any registry construction — so a malformed override TOML (which would make
    // tree.build() fail) can't break them. `verify` especially must always
    // return its freshness exit code, never error on registry construction
    // (reviewer finding). `discover`/`list`/`bench` recompute their own view.
    match &cli.cmd {
        Some(Cmd::Verify { diff }) => return do_verify(cli.json, *diff),
        Some(Cmd::Discover { dry_run }) => return do_discover(cli.json, *dry_run),
        Some(Cmd::List) => return do_list(cli.json),
        Some(Cmd::Bench { fake }) => return do_bench(cli.json, *fake),
        Some(Cmd::Classify { cmd, write }) => return do_classify(cmd, *write, cli.json),
        _ => {}
    }

    let (tree, snap_cache) = read_registry();
    let tree = tree.build()?;
    let cache_ref = snap_cache.as_ref();
    match cli.cmd {
        None => {
            print_banner(&tree);
            Ok(0)
        }
        Some(Cmd::Find { intent }) => do_find(&tree, &intent.join(" "), cli.json, cache_ref),
        Some(Cmd::Walk { prefix }) => do_walk(&tree, prefix.as_deref(), cli.json),
        Some(Cmd::Help { path, full, schema }) => do_help(&tree, &path, full, schema, cli.json),
        Some(Cmd::Run { path, args }) => do_run(&tree, &path, &args),
        Some(Cmd::Bare(tokens)) => match route_bare(&tree, &tokens) {
            BareAction::Run { path, args } => do_run(&tree, &path.to_string(), &args),
            BareAction::Walk { prefix } => do_walk(&tree, Some(&prefix.to_string()), cli.json),
            BareAction::Help { path, schema } => {
                do_help(&tree, &path.to_string(), false, schema, cli.json)
            }
            BareAction::Find { intent } => do_find(&tree, &intent, cli.json, cache_ref),
        },
        // Handled above (registry-free dispatch); unreachable here.
        Some(
            Cmd::Verify { .. }
            | Cmd::Discover { .. }
            | Cmd::List
            | Cmd::Bench { .. }
            | Cmd::Classify { .. },
        ) => {
            unreachable!("registry-free commands dispatched before tree build")
        }
    }
}

fn print_banner(tree: &Tree) {
    println!(
        "t4c {} — {} capabilities. Find by intent, then invoke by path.",
        crate::version(),
        tree.len()
    );
    println!("  t4c find <intent>           rank capabilities for what you want to do");
    println!("  t4c <prefix>                walk a subtree (e.g. `t4c bash`)");
    println!("  t4c help <path> [--schema]  learn a capability");
    println!("  t4c run <path> -- [args]    invoke it");
}

#[derive(Serialize)]
struct FindOutput {
    intent: String,
    hits: Vec<Hit>,
}

#[derive(Serialize)]
struct Hit {
    path: String,
    summary: String,
    score: f64,
    /// A starting template — the model finalizes args from the schema (finding #3).
    suggested_call: String,
    help: Option<HelpPtr>,
}

#[derive(Serialize)]
struct HelpPtr {
    argv: Vec<String>,
    ai: bool,
}

fn do_find(
    tree: &Tree,
    intent: &str,
    json: bool,
    snapshot_cache: Option<&crate::semantic::VectorCache>,
) -> Result<i32> {
    let caps: Vec<&Capability> = tree.all().collect();
    let ranked = rank_caps(intent, &caps, snapshot_cache);
    let hits: Vec<Hit> = ranked
        .iter()
        .take(8)
        .map(|r| Hit {
            path: r.cap.path.to_string(),
            summary: r.cap.summary.clone(),
            score: r.score,
            suggested_call: format!("t4c run {}", r.cap.path),
            help: r.cap.help.as_ref().map(|h| HelpPtr {
                argv: h.argv.clone(),
                ai: h.ai,
            }),
        })
        .collect();

    if json {
        let out = FindOutput {
            intent: intent.to_string(),
            hits,
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }

    if hits.is_empty() {
        println!("(no capabilities — try `t4c list`)");
        return Ok(0);
    }
    println!("intent: {intent:?}\n");
    for h in &hits {
        let mark = if h.score > 0.0 { "->" } else { "  " };
        let ai = if h.help.as_ref().is_some_and(|x| x.ai) {
            "  · ai-help"
        } else {
            ""
        };
        println!("  {mark} {:<26} {}{}", h.path, h.summary, ai);
    }
    if let Some(top) = hits.first() {
        println!("\nsuggested: {}", top.suggested_call);
        println!(
            "next: t4c help {} --schema   (then build the args from the schema)",
            top.path
        );
    }
    Ok(0)
}

#[derive(Serialize)]
struct Node {
    path: String,
    summary: String,
}

fn do_walk(tree: &Tree, prefix: Option<&str>, json: bool) -> Result<i32> {
    let caps: Vec<&Capability> = match prefix {
        Some(p) => {
            let path = CapPath::parse(p).with_context(|| format!("bad prefix {p:?}"))?;
            tree.walk(&path)
        }
        None => tree.all().collect(),
    };

    if json {
        let nodes: Vec<Node> = caps
            .iter()
            .map(|c| Node {
                path: c.path.to_string(),
                summary: c.summary.clone(),
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&nodes)?);
        return Ok(0);
    }

    if caps.is_empty() {
        println!("(nothing under that prefix — `t4c list` for all)");
        return Ok(0);
    }
    for c in &caps {
        println!("  {:<28} {}", c.path.to_string(), c.summary);
    }
    Ok(0)
}

fn do_help(tree: &Tree, path_str: &str, full: bool, schema: bool, json: bool) -> Result<i32> {
    let path = match CapPath::parse(path_str) {
        Ok(p) => p,
        Err(_) => return fuzzy_miss(tree, path_str, json),
    };
    let Some(cap) = tree.get(&path) else {
        return fuzzy_miss(tree, path_str, json);
    };
    let Some(help) = &cap.help else {
        println!(
            "no help registered for {} (invoke: {})",
            path,
            cap.invoke.join(" ")
        );
        return Ok(0);
    };

    let mut argv = help.argv.clone();
    if argv.is_empty() {
        anyhow::bail!("help for {path} has empty argv");
    }

    // --schema / --json: hand back the raw machine document. When the tool
    // speaks --help-ai, ask for --json explicitly.
    if schema || json {
        if help.ai {
            argv.push("--json".to_string());
        }
        let text = run_help_text(&argv)?;
        print!("{text}");
        if !text.ends_with('\n') {
            println!();
        }
        return Ok(0);
    }

    // Default: for a --help-ai tool, parse the ROOT document, walk to this
    // capability's node by its invoke chain, and render the rich superset
    // fields (usage / args / output_schema). Parsing the root + walking — rather
    // than probing the subcommand directly — is deliberate: many tools emit the
    // full recursive document only at the root. Falls through to plain text when
    // the tool can't be probed / parsed / the node isn't found.
    if help.ai {
        if let Some(rich) = render_help_ai(cap) {
            print!("{rich}");
            if !rich.ends_with('\n') {
                println!();
            }
            return Ok(0);
        }
    }

    // Fallback: the tool's plain help text — terse by default, --full for the rest.
    let text = run_help_text(&argv)?;
    let lines: Vec<&str> = text.lines().collect();
    const LIMIT: usize = 16;
    if full || lines.len() <= LIMIT {
        print!("{text}");
        if !text.ends_with('\n') {
            println!();
        }
    } else {
        for l in &lines[..LIMIT] {
            println!("{l}");
        }
        println!(
            "\n… +{} more lines — run: t4c help {} --full",
            lines.len() - LIMIT,
            path
        );
    }
    Ok(0)
}

/// Run a help argv and return stdout (or stderr if stdout is empty).
fn run_help_text(argv: &[String]) -> Result<String> {
    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .with_context(|| format!("running help: {}", argv.join(" ")))?;
    Ok(if out.stdout.is_empty() {
        String::from_utf8_lossy(&out.stderr).into_owned()
    } else {
        String::from_utf8_lossy(&out.stdout).into_owned()
    })
}

/// Probe the root tool's `--help-ai --json`, walk to `cap`'s node by its invoke
/// chain, and render the node's rich superset fields. `None` (→ caller falls
/// back to plain help) when the tool can't be run, the JSON doesn't parse, or
/// the node isn't present in the document.
fn render_help_ai(cap: &Capability) -> Option<String> {
    let root_cmd = cap.invoke.first()?;
    let out = Command::new(root_cmd)
        .args(["--help-ai", "--json"])
        .output()
        .ok()?;
    let doc: crate::capability::HelpAiDoc = serde_json::from_slice(&out.stdout).ok()?;
    // The capability's invoke is [root, sub1, sub2, …]; walk the doc by the tail.
    let mut node = &doc;
    for seg in cap.invoke.iter().skip(1) {
        node = node.subcommands.iter().find(|s| &s.name == seg)?;
    }
    Some(format_help_ai_node(node))
}

/// Render one `--help-ai` node for a CLI caller: summary, usage, an args table,
/// a subcommands list, and a pretty-printed output_schema. Absent sections are
/// omitted (no empty headers).
fn format_help_ai_node(node: &crate::capability::HelpAiDoc) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    if !node.summary.is_empty() {
        let _ = writeln!(s, "{}", node.summary);
    }
    if let Some(u) = &node.usage {
        let _ = writeln!(s, "\nusage: {u}");
    }
    if !node.args.is_empty() {
        let _ = writeln!(s, "\narguments:");
        for a in &node.args {
            let flag = match (&a.long, &a.short) {
                (Some(l), Some(sh)) => format!("{sh}, {l}"),
                (Some(l), None) => l.clone(),
                (None, Some(sh)) => sh.clone(),
                (None, None) => a.name.clone(),
            };
            let val = if a.takes_value {
                format!(" <{}>", a.value_name.as_deref().unwrap_or("VALUE"))
            } else {
                String::new()
            };
            let mut meta = Vec::new();
            if a.required {
                meta.push("required".to_string());
            }
            if a.multiple {
                meta.push("repeatable".to_string());
            }
            if !a.possible_values.is_empty() {
                meta.push(format!("one of: {}", a.possible_values.join(", ")));
            }
            if !a.default.is_empty() {
                meta.push(format!("default: {}", a.default.join(", ")));
            }
            let meta = if meta.is_empty() {
                String::new()
            } else {
                format!(" [{}]", meta.join("; "))
            };
            let help = a.help.as_deref().unwrap_or("");
            let _ = writeln!(
                s,
                "  {flag}{val}{meta}{}{help}",
                if help.is_empty() { "" } else { "  " }
            );
        }
    }
    if !node.subcommands.is_empty() {
        let _ = writeln!(s, "\nsubcommands:");
        for sub in &node.subcommands {
            let _ = writeln!(s, "  {:<16} {}", sub.name, sub.summary);
        }
    }
    if let Some(schema) = &node.output_schema {
        let pretty = serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string());
        let _ = writeln!(s, "\noutput schema:\n{pretty}");
    }
    s
}

/// A path miss is not an error wall: rank the query and offer did-you-mean.
fn fuzzy_miss(tree: &Tree, query: &str, json: bool) -> Result<i32> {
    let caps: Vec<&Capability> = tree.all().collect();
    let ranked = LexicalRanker.rank(query, &caps);
    let suggestions: Vec<(String, String)> = ranked
        .iter()
        .filter(|r| r.score > 0.0)
        .take(3)
        .map(|r| (r.cap.path.to_string(), r.cap.summary.clone()))
        .collect();

    if json {
        #[derive(Serialize)]
        struct Miss {
            error: String,
            query: String,
            did_you_mean: Vec<String>,
        }
        let m = Miss {
            error: "no such path".to_string(),
            query: query.to_string(),
            did_you_mean: suggestions.iter().map(|(p, _)| p.clone()).collect(),
        };
        println!("{}", serde_json::to_string_pretty(&m)?);
        return Ok(2);
    }

    println!("no capability at {query:?}.");
    if suggestions.is_empty() {
        println!("try `t4c find {query}` or `t4c list`.");
    } else {
        println!("did you mean:");
        for (p, s) in &suggestions {
            println!("  {p}  ({s})");
        }
    }
    Ok(2)
}

fn do_run(tree: &Tree, path_str: &str, extra: &[String]) -> Result<i32> {
    let path = match CapPath::parse(path_str) {
        Ok(p) => p,
        Err(_) => return fuzzy_miss(tree, path_str, false),
    };
    let cap = match tree.get(&path) {
        Some(c) => c,
        None => {
            // exact miss: a prefix walks; otherwise fuzzy did-you-mean.
            if !tree.walk(&path).is_empty() {
                return do_walk(tree, Some(path_str), false);
            }
            return fuzzy_miss(tree, path_str, false);
        }
    };
    if cap.invoke.is_empty() {
        anyhow::bail!("{path} has no invocation argv");
    }
    let mut argv = cap.invoke.clone();
    argv.extend(extra.iter().cloned());

    // Flush "what I ran" BEFORE the child writes, so a piped reader isn't blind.
    println!("-> {path} : running {}", argv.join(" "));
    std::io::stdout().flush().ok();

    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .with_context(|| format!("running {}", argv.join(" ")))?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::StaticSource;

    fn tree() -> Tree {
        let mut reg = Registry::new();
        reg.add_source(Box::new(StaticSource::new("c", crate::catalog::curated())));
        reg.build().unwrap()
    }

    fn toks(s: &str) -> Vec<String> {
        s.split_whitespace().map(|t| t.to_string()).collect()
    }

    #[test]
    fn format_help_ai_node_renders_rich_and_omits_absent() {
        use crate::capability::{HelpAiArg, HelpAiDoc};
        let node = HelpAiDoc {
            name: "run".to_string(),
            summary: "run the thing".to_string(),
            usage: Some("tool run <target>".to_string()),
            args: vec![HelpAiArg {
                name: "top".to_string(),
                long: Some("--top".to_string()),
                takes_value: true,
                value_name: Some("N".to_string()),
                help: Some("max hits".to_string()),
                ..Default::default()
            }],
            output_schema: Some(serde_json::json!({"type":"array"})),
            ..Default::default()
        };
        let out = format_help_ai_node(&node);
        assert!(out.contains("run the thing"));
        assert!(out.contains("usage: tool run <target>"));
        assert!(out.contains("--top"));
        assert!(out.contains("max hits"));
        assert!(out.contains("output schema"));
        assert!(!out.contains("subcommands:"), "no subcommands => no header");

        // a bare node omits every rich section
        let bare = HelpAiDoc {
            name: "x".to_string(),
            summary: "just a summary".to_string(),
            ..Default::default()
        };
        let bo = format_help_ai_node(&bare);
        assert!(bo.contains("just a summary"));
        assert!(!bo.contains("usage:"));
        assert!(!bo.contains("arguments:"));
        assert!(!bo.contains("output schema"));
    }

    #[test]
    fn registry_document_carries_review_only_banner() {
        // The persisted registry is a generated export, never read back — the
        // banner must warn that edits are ignored + overwritten and point at the
        // override layer (pairs with self_config_path's "NOT hand-edited" doc).
        let doc = registry_document(&[]).unwrap();
        assert!(doc.starts_with("# t4c registry"), "banner leads the file");
        assert!(doc.contains("REVIEW ONLY"));
        assert!(doc.contains("OVERWRITTEN"));
        assert!(
            doc.contains("overrides.toml"),
            "points at the override layer"
        );
        // The banner is TOML comments, so the document still parses (and would
        // round-trip the caps) — a banner that broke parsing would corrupt the
        // export it heads.
        let back = TomlConfigSource::parse_str(&doc).expect("banner + toml parses");
        assert!(back.is_empty());
    }

    #[test]
    fn bare_exact_path_runs() {
        let t = tree();
        assert_eq!(
            route_bare(&t, &toks("bash.jq foo")),
            BareAction::Run {
                path: CapPath::parse("bash.jq").unwrap(),
                args: vec!["foo".to_string()],
            }
        );
    }

    #[test]
    fn bare_prefix_walks() {
        let t = tree();
        assert_eq!(
            route_bare(&t, &toks("bash")),
            BareAction::Walk {
                prefix: CapPath::parse("bash").unwrap(),
            }
        );
    }

    #[test]
    fn bare_help_and_schema_flags_route_to_help() {
        let t = tree();
        assert_eq!(
            route_bare(&t, &toks("mcp.code-index.recall --help-ai")),
            BareAction::Help {
                path: CapPath::parse("mcp.code-index.recall").unwrap(),
                schema: false,
            }
        );
        assert_eq!(
            route_bare(&t, &toks("bash.jq --schema")),
            BareAction::Help {
                path: CapPath::parse("bash.jq").unwrap(),
                schema: true,
            }
        );
    }

    #[test]
    fn unknown_path_falls_back_to_find() {
        let t = tree();
        match route_bare(&t, &toks("xyzzy nothing here")) {
            BareAction::Find { intent } => assert!(intent.contains("xyzzy")),
            other => panic!("expected find, got {other:?}"),
        }
    }

    #[test]
    fn dotted_miss_becomes_spaced_find_intent() {
        let t = tree();
        match route_bare(&t, &toks("bash.nope")) {
            BareAction::Find { intent } => assert_eq!(intent, "bash nope"),
            other => panic!("expected find, got {other:?}"),
        }
    }

    #[test]
    fn catalog_builds_into_tree() {
        let t = tree();
        assert!(t
            .get(&CapPath::parse("mcp.code-index.recall").unwrap())
            .is_some());
        assert!(t.walk(&CapPath::parse("bash").unwrap()).len() >= 4);
        assert!(t.get(&CapPath::parse("bash.jq").unwrap()).is_some());
    }

    // --- mu-2332: warm-path vector wiring (the gpt-5.5 review finding) --------
    // The bug the reviewer caught: warm_registry destructured `Fresh { caps, .. }`
    // and threw away the archived vectors, so semantic find silently fell back to
    // the JSON cache (or lexical). These tests lock the contract that the warm
    // path SURFACES the snapshot's vectors and rank_caps PREFERS them.

    /// A round-tripped snapshot carrying vectors yields a non-empty
    /// `VectorCache` via the same conversion `warm_registry` performs — i.e. the
    /// archived vectors are not dropped on the warm read.
    #[test]
    fn warm_path_surfaces_snapshot_vectors() {
        use crate::semantic::VectorCache;
        use std::collections::HashMap;

        let caps = crate::catalog::curated();
        let mut vectors = HashMap::new();
        vectors.insert("bash.jq".to_string(), vec![0.1f32, 0.2, 0.3]);
        let snap =
            crate::snapshot::Snapshot::build(&caps, &vectors, "mdl", "CAT".into(), "PATH".into());

        let dir = std::env::temp_dir().join(format!("t4c-warmvec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("snap.rkyv");
        snap.save(&path).unwrap();

        // Load + apply warm_registry's exact vectors->cache conversion.
        let outcome = crate::snapshot::load_valid(&path, "CAT", "PATH");
        let cache = match outcome {
            crate::snapshot::LoadOutcome::Fresh { vectors, model, .. } => {
                if vectors.is_empty() {
                    None
                } else {
                    Some(VectorCache {
                        model,
                        by_path: vectors,
                    })
                }
            }
            other => panic!("expected Fresh, got {other:?}"),
        };
        let cache = cache.expect("warm path must surface a cache when vectors archived");
        assert_eq!(cache.model, "mdl");
        assert_eq!(cache.by_path.get("bash.jq").unwrap(), &vec![0.1, 0.2, 0.3]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// rank_caps PREFERS the passed snapshot cache over the on-disk JSON path:
    /// with no embedder configured both branches fall through to lexical, so we
    /// assert the call is well-formed and total (no panic, returns a ranking)
    /// when handed an explicit cache — the wiring `do_find` now relies on.
    #[test]
    fn rank_caps_accepts_snapshot_cache() {
        use crate::semantic::VectorCache;
        use std::collections::HashMap;

        let t = tree();
        let caps: Vec<&Capability> = t.all().collect();
        let mut by_path = HashMap::new();
        by_path.insert("bash.jq".to_string(), vec![0.0f32; 4]);
        let cache = VectorCache {
            model: "m".into(),
            by_path,
        };
        // Passing the cache must not panic and must produce a full ranking.
        let ranked = rank_caps("query text", &caps, Some(&cache));
        assert_eq!(ranked.len(), caps.len());
    }
}
