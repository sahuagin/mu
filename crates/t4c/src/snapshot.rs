//! The warm-start snapshot (mu-2332 part 2).
//!
//! Discovery is expensive: probing the PATH for every catalogued tool (`which`
//! per entry), then embedding the catalog over the network. Today every `t4c
//! find` re-probes the environment via [`crate::catalog::EnvCatalogSource`] and
//! re-loads a JSON vector cache. This module replaces that with ONE archived
//! artifact: the whole post-discovery state — present capabilities, their
//! captured help, and their embedding vectors — serialized with rkyv and loaded
//! **zero-copy via mmap**. A warm start does no probing and no embedding; it
//! maps the file and runs.
//!
//! ## The snapshot is CACHE, never source of truth
//!
//! The archive carries a 3-part validity header: a [`SCHEMA_VERSION`] const, a
//! hash of the catalog file content, and a hash of the PATH-set. On load, any
//! mismatch means the snapshot describes a world that no longer exists — so it
//! is treated as ABSENT and discovery regenerates it. We never migrate an old
//! archive. This is what makes rkyv's schema-evolution brittleness acceptable
//! by design: an incompatible layout is indistinguishable from a stale one, and
//! both self-heal by rediscovery (mu-2332 ACCEPTANCE).
//!
//! f32 embedding vectors are rkyv's best case — a plain `Vec<f32>` archives to a
//! contiguous block that maps with no per-element work, which is the whole point
//! of reaching for rkyv over the existing serde_json path.

use crate::capability::{Capability, HelpSpec};
use crate::path::CapPath;
use anyhow::{Context, Result};
use rkyv::{Archive, Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Bump on ANY change to the archived layout below. A loader seeing a different
/// version treats the snapshot as absent and rediscovers — it never migrates.
/// (The catalog-hash and PATH-hash guard *content* drift; this const guards
/// *shape* drift.)
pub const SCHEMA_VERSION: u32 = 2;

/// One capability as it lives in the archive: flat, all-owned, no newtypes.
/// rkyv archives `String`/`Vec` directly; we reconstruct the [`Capability`]
/// (with its [`CapPath`]) on load. `effects`/`help` collapse to optional plain
/// fields so the archived form has no enum-discriminant surprises.
#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
pub struct ArchCapability {
    /// Dotted path string, e.g. `bash.jj.status`.
    pub path: String,
    pub summary: String,
    pub keywords: Vec<String>,
    pub invoke: Vec<String>,
    /// Help argv, if known (probed or curated). Empty vec => no help spec.
    pub help_argv: Vec<String>,
    /// Whether the help argv speaks `--help-ai --json`.
    pub help_ai: bool,
    /// The capability's embedding vector. Empty => not embedded (no live
    /// embedder at discover time); the ranker then degrades to lexical.
    pub vector: Vec<f32>,
    /// Capability gates this requires (the permission surface).
    pub requires: Vec<String>,
}

/// The archived post-discovery state. Header first (cheap to validate), body
/// after. `model` records which embedder produced the vectors so a query-time
/// embedder mismatch can fall back rather than cosine-comparing incompatible
/// spaces.
#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
pub struct Snapshot {
    /// Layout version — must equal [`SCHEMA_VERSION`] or the snapshot is stale.
    pub schema_version: u32,
    /// blake3 of the catalog file content at discover time. Changes when a tool
    /// is added/edited in the TOML — invalidating the snapshot.
    pub catalog_hash: String,
    /// blake3 of the sorted PATH entries at discover time. Changes when the
    /// environment's tool set could differ — invalidating the snapshot.
    pub path_hash: String,
    /// blake3 of the resolved binaries the present-set depends on (path/len/
    /// mtime each). Catches a tool added/removed/replaced WITHIN an unchanged
    /// PATH directory — the residual `path_hash` can't see. See
    /// [`tool_fingerprint`].
    pub tool_hash: String,
    /// Embedder model id that produced the vectors (empty if none).
    pub model: String,
    /// The present (installed) capabilities, with captured help + vectors.
    pub present: Vec<ArchCapability>,
}

impl Snapshot {
    /// Where the snapshot lives: `$T4C_SNAPSHOT` or `~/.cache/t4c/snapshot.rkyv`.
    pub fn default_path() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("T4C_SNAPSHOT") {
            return Some(PathBuf::from(p));
        }
        std::env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(".cache/t4c/snapshot.rkyv"))
    }

    /// Build a snapshot from the present capabilities and a path -> vector map.
    /// `catalog_hash`/`path_hash` are the current-world fingerprints (see
    /// [`catalog_content_hash`], [`path_set_hash`]).
    pub fn build(
        present: &[Capability],
        vectors: &HashMap<String, Vec<f32>>,
        model: &str,
        catalog_hash: String,
        path_hash: String,
    ) -> Self {
        // The binaries the present-set actually invokes — fingerprinted so the
        // snapshot invalidates when one of them changes in place (see
        // tool_fingerprint). invoke[0] is the command each capability runs.
        let invoke_cmds: Vec<String> = present
            .iter()
            .filter_map(|c| c.invoke.first().cloned())
            .collect();
        let tool_hash = tool_fingerprint(&invoke_cmds);
        let present = present
            .iter()
            .map(|c| {
                let (help_argv, help_ai) = match &c.help {
                    Some(h) => (h.argv.clone(), h.ai),
                    None => (Vec::new(), false),
                };
                ArchCapability {
                    path: c.path.to_string(),
                    summary: c.summary.clone(),
                    keywords: c.keywords.clone(),
                    invoke: c.invoke.clone(),
                    help_argv,
                    help_ai,
                    vector: vectors
                        .get(&c.path.to_string())
                        .cloned()
                        .unwrap_or_default(),
                    requires: c.requires.clone(),
                }
            })
            .collect();
        Self {
            schema_version: SCHEMA_VERSION,
            catalog_hash,
            path_hash,
            tool_hash,
            model: model.to_string(),
            present,
        }
    }

    /// Serialize and write the snapshot atomically (temp + rename) so a crashed
    /// write never leaves a half-archive that the loader would treat as
    /// corrupt-but-present.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating snapshot dir {}", parent.display()))?;
        }
        let bytes =
            rkyv::to_bytes::<rkyv::rancor::Error>(self).context("rkyv-serializing snapshot")?;
        // Unique temp name per writer so concurrent savers targeting the same
        // snapshot don't clobber a shared temp file — each writes its own temp,
        // then atomically renames into place. The rename is the commit point;
        // last writer wins cleanly, no torn file (reviewer finding: a fixed
        // `.rkyv.tmp` raced under concurrency). Uniqueness is STRUCTURAL, not
        // probabilistic: pid separates processes, the atomic counter separates
        // threads. The previous pid+SystemTime-nanos nonce collided for
        // same-process threads on coarse CI clocks — two writers shared a temp,
        // the first rename took it, the second failed ENOENT (flaked
        // concurrent_saves_produce_a_valid_archive three times on 2026-06-11).
        static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_extension(format!("rkyv.tmp.{}.{nonce}", std::process::id()));
        std::fs::write(&tmp, &bytes)
            .with_context(|| format!("writing snapshot tmp {}", tmp.display()))?;
        // Best-effort cleanup of our own temp if the rename fails (e.g. cross-dev).
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e)
                .with_context(|| format!("renaming snapshot into place {}", path.display()));
        }
        Ok(())
    }

    /// Reconstruct owned [`Capability`] values plus the path->vector map for the
    /// semantic ranker. This is the deserialized (owned) path; the zero-copy
    /// validation happens first in [`load_valid`]. Capabilities whose archived
    /// path fails to parse are skipped (defensive: a corrupt entry can't take
    /// down the whole load).
    fn reconstruct(&self) -> (Vec<Capability>, HashMap<String, Vec<f32>>) {
        let mut caps = Vec::with_capacity(self.present.len());
        let mut vectors = HashMap::with_capacity(self.present.len());
        for a in &self.present {
            let Ok(path) = CapPath::parse(&a.path) else {
                continue;
            };
            let help = if a.help_argv.is_empty() {
                None
            } else {
                Some(HelpSpec {
                    argv: a.help_argv.clone(),
                    ai: a.help_ai,
                })
            };
            if !a.vector.is_empty() {
                vectors.insert(a.path.clone(), a.vector.clone());
            }
            caps.push(Capability {
                path,
                summary: a.summary.clone(),
                keywords: a.keywords.clone(),
                invoke: a.invoke.clone(),
                help,
                requires: a.requires.clone(),
                effects: None,
            });
        }
        (caps, vectors)
    }
}

/// The outcome of a load attempt. `Stale` carries WHY so `discover`/`find` can
/// log the self-heal cause; `Missing` means no file. Both lead to the same
/// action (rediscover) — the distinction is only for observability.
#[derive(Debug)]
pub enum LoadOutcome {
    /// A valid, current snapshot: reconstructed capabilities + vectors + model.
    Fresh {
        caps: Vec<Capability>,
        vectors: HashMap<String, Vec<f32>>,
        model: String,
    },
    /// File exists but is stale or unreadable (reason for logging). Self-heal.
    Stale(String),
    /// No snapshot file. Cold start.
    Missing,
}

/// Load the snapshot at `path`, validating the 3-part header against the
/// current world (`catalog_hash` / `path_hash`). Returns [`LoadOutcome::Fresh`]
/// only when schema version AND both hashes match; otherwise a self-heal signal.
///
/// The read is mmap-backed: we map the file and run rkyv's validating access
/// over the mapped bytes (zero-copy structural check) before deserializing.
/// On any validation failure the snapshot is `Stale`, never a hard error — a
/// corrupt cache must degrade to rediscovery, not abort the program.
pub fn load_valid(path: &Path, catalog_hash: &str, path_hash: &str) -> LoadOutcome {
    if !path.exists() {
        return LoadOutcome::Missing;
    }
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return LoadOutcome::Stale(format!("open failed: {e}")),
    };
    // SAFETY: we only ever read the mapping, and treat any inconsistency
    // (including a file mutated under us) as a validation failure -> Stale.
    let mmap = match unsafe { memmap2::Mmap::map(&file) } {
        Ok(m) => m,
        Err(e) => return LoadOutcome::Stale(format!("mmap failed: {e}")),
    };
    // Zero-copy validating access: confirms the bytes are a well-formed
    // ArchivedSnapshot before we trust any field.
    let archived = match rkyv::access::<ArchivedSnapshot, rkyv::rancor::Error>(&mmap) {
        Ok(a) => a,
        Err(e) => return LoadOutcome::Stale(format!("archive invalid (treat as stale): {e}")),
    };
    // Cheap header checks straight off the archived view — no full deserialize
    // until the header proves the body is worth trusting.
    if archived.schema_version != SCHEMA_VERSION {
        return LoadOutcome::Stale(format!(
            "schema {} != current {SCHEMA_VERSION}",
            archived.schema_version
        ));
    }
    if archived.catalog_hash.as_ref() != catalog_hash {
        return LoadOutcome::Stale("catalog changed since discover".to_string());
    }
    if archived.path_hash.as_ref() != path_hash {
        return LoadOutcome::Stale("PATH set changed since discover".to_string());
    }
    // Tool-binary fingerprint: recompute the live fingerprint over the SAME
    // binaries the snapshot committed to (its archived invoke commands) and
    // compare to the stored hash. This is what catches a tool deleted/replaced
    // inside an unchanged PATH dir — the case path_hash is blind to. It stats
    // only the present-set's binaries (a dozen), never a full catalog re-probe.
    let archived_invoke_cmds: Vec<String> = archived
        .present
        .iter()
        .filter_map(|c| c.invoke.first().map(|s| s.as_ref().to_string()))
        .collect();
    let live_tool_hash = tool_fingerprint(&archived_invoke_cmds);
    if archived.tool_hash.as_ref() != live_tool_hash {
        return LoadOutcome::Stale("a depended-on tool binary changed since discover".to_string());
    }
    // Header is good: deserialize to owned and reconstruct.
    let snap: Snapshot = match rkyv::deserialize::<Snapshot, rkyv::rancor::Error>(archived) {
        Ok(s) => s,
        Err(e) => return LoadOutcome::Stale(format!("deserialize failed: {e}")),
    };
    let (caps, vectors) = snap.reconstruct();
    LoadOutcome::Fresh {
        caps,
        vectors,
        model: snap.model,
    }
}

/// Read the archived capability paths REGARDLESS of validity — used by
/// `verify` to inspect what categories a (possibly stale) snapshot held, so it
/// can tell "world moved but every category still resolves" (stale-equivalent)
/// from "a whole category is gone" (stale-degraded). Distinct from
/// [`load_valid`], which discards the body the instant a hash mismatches because
/// the warm read MUST self-heal; here we deliberately look inside a stale
/// snapshot. Still structurally validates the archive (a corrupt/missing file
/// yields `None`), so it never trusts garbage — it just skips the freshness
/// gate. Returns paths only (not full caps): verify compares category presence,
/// not capability content.
pub fn archived_paths(path: &Path) -> Option<Vec<String>> {
    if !path.exists() {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    // SAFETY: read-only mapping; any inconsistency surfaces as a validation
    // failure below and yields None.
    let mmap = unsafe { memmap2::Mmap::map(&file) }.ok()?;
    let archived = rkyv::access::<ArchivedSnapshot, rkyv::rancor::Error>(&mmap).ok()?;
    // Structurally valid: collect the present-set paths off the archived view
    // (no full deserialize needed — we only want the path strings).
    Some(
        archived
            .present
            .iter()
            .map(|c| c.path.as_ref().to_string())
            .collect(),
    )
}

/// blake3 of the catalog file content — the fingerprint that invalidates the
/// snapshot when a tool is added or a calling convention changes in the TOML.
/// The embedded default is included so a binary upgrade that ships a new
/// default catalog also invalidates an old snapshot.
pub fn catalog_content_hash(extra_catalog_files: &[PathBuf]) -> String {
    let mut hasher = blake3::Hasher::new();
    // The baked-in default always contributes.
    hasher.update(crate::catalog::DEFAULT_CATALOG_TOML.as_bytes());
    for p in extra_catalog_files {
        if let Ok(bytes) = std::fs::read(p) {
            hasher.update(&bytes);
        }
    }
    hasher.finalize().to_hex().to_string()
}

/// blake3 of the PATH set: split, trimmed, sorted, deduplicated, newline-joined.
/// Sorting makes the hash order-independent (reordering PATH doesn't invalidate
/// the snapshot, but adding/removing a directory — where a tool could appear or
/// vanish — does).
///
/// NOTE: this catches PATH-*directory* drift, but NOT a tool being added or
/// replaced WITHIN an unchanged PATH directory (delete `/usr/bin/jq` while
/// `/usr/bin` stays on PATH). That residual is closed by [`tool_fingerprint`],
/// which stats the specific binaries the snapshot committed to.
pub fn path_set_hash() -> String {
    let raw = std::env::var("PATH").unwrap_or_default();
    let sep = if cfg!(windows) { ';' } else { ':' };
    hash_path_str(&raw, sep)
}

/// blake3 fingerprint of the *resolved binaries the snapshot actually depends
/// on* — closing the gap that [`path_set_hash`] cannot: a tool removed,
/// installed, or replaced in place inside an UNCHANGED PATH directory.
///
/// For each present capability we resolve its invoke command against PATH and
/// fold in (resolved-path, len, mtime-nanos) — or an explicit "absent" marker
/// if it no longer resolves. This is the honest way to keep the warm-start
/// present/absent invariant true WITHOUT re-probing the whole catalog: we stat
/// only the N binaries the snapshot committed to (~the present-set size, a
/// dozen), never the full catalog `which`-sweep that `discover` does. So a
/// warm load stays cheap (N stats, no process spawns) while still invalidating
/// precisely when a depended-on tool changes.
///
/// `invoke_cmds` are the first-argv tokens of the present capabilities (the
/// binary each one runs). Order-independent: sorted before hashing.
pub fn tool_fingerprint(invoke_cmds: &[String]) -> String {
    let mut entries: Vec<String> = invoke_cmds
        .iter()
        .map(|cmd| {
            let resolved = resolve_in_path(cmd);
            match resolved.and_then(|p| std::fs::metadata(&p).ok().map(|m| (p, m))) {
                Some((p, m)) => {
                    let mtime = m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos())
                        .unwrap_or(0);
                    format!("{cmd}={}\t{}\t{mtime}", p.display(), m.len())
                }
                None => format!("{cmd}=ABSENT"),
            }
        })
        .collect();
    entries.sort_unstable();
    entries.dedup();
    let mut hasher = blake3::Hasher::new();
    hasher.update(entries.join("\n").as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// Resolve a command to its absolute path against `$PATH` (absolute commands
/// returned as-is if they exist). Returns the first match — mirrors how the
/// shell and [`crate::catalog::which`] resolve, so the fingerprint stats the
/// SAME binary the capability would actually invoke.
fn resolve_in_path(cmd: &str) -> Option<PathBuf> {
    let p = Path::new(cmd);
    if p.is_absolute() {
        return p.is_file().then(|| p.to_path_buf());
    }
    let path = std::env::var("PATH").ok()?;
    let sep = if cfg!(windows) { ';' } else { ':' };
    path.split(sep)
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(cmd))
        .find(|c| c.is_file())
}

/// Pure core of [`path_set_hash`], split out so it's testable WITHOUT mutating
/// the process-global `PATH` (which races other concurrently-running tests that
/// read the real environment). Splits on `sep`, trims, drops empties, sorts +
/// dedups (order-independent), and blake3-hashes the canonical join.
fn hash_path_str(raw: &str, sep: char) -> String {
    let mut dirs: Vec<&str> = raw
        .split(sep)
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .collect();
    dirs.sort_unstable();
    dirs.dedup();
    let mut hasher = blake3::Hasher::new();
    hasher.update(dirs.join("\n").as_bytes());
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(path: &str, summary: &str, _vec: Vec<f32>) -> Capability {
        let p = CapPath::parse(path).unwrap();
        Capability {
            invoke: p.invoke_argv(),
            path: p,
            summary: summary.to_string(),
            keywords: vec!["kw".to_string()],
            help: Some(HelpSpec {
                argv: vec!["tool".to_string(), "--help".to_string()],
                ai: true,
            }),
            requires: vec![],
            effects: None,
        }
    }

    #[test]
    fn round_trips_through_mmap_when_hashes_match() {
        let dir = std::env::temp_dir().join(format!("t4c-snap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("snapshot.rkyv");

        let caps = vec![
            cap("bash.jj.status", "show working copy", vec![0.1, 0.2, 0.3]),
            cap(
                "mcp.code-index.recall",
                "semantic recall",
                vec![0.4, 0.5, 0.6],
            ),
        ];
        let mut vectors = HashMap::new();
        vectors.insert("bash.jj.status".to_string(), vec![0.1, 0.2, 0.3]);
        vectors.insert("mcp.code-index.recall".to_string(), vec![0.4, 0.5, 0.6]);

        let snap = Snapshot::build(
            &caps,
            &vectors,
            "test-model",
            "CATHASH".into(),
            "PATHHASH".into(),
        );
        snap.save(&path).unwrap();

        match load_valid(&path, "CATHASH", "PATHHASH") {
            LoadOutcome::Fresh {
                caps,
                vectors,
                model,
            } => {
                assert_eq!(model, "test-model");
                assert_eq!(caps.len(), 2);
                let s = caps
                    .iter()
                    .find(|c| c.path.to_string() == "bash.jj.status")
                    .unwrap();
                assert_eq!(s.summary, "show working copy");
                assert!(s.help.is_some());
                assert_eq!(vectors.get("bash.jj.status").unwrap(), &vec![0.1, 0.2, 0.3]);
            }
            other => panic!("expected Fresh, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn schema_or_hash_mismatch_is_stale() {
        let dir = std::env::temp_dir().join(format!("t4c-snap-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("snapshot.rkyv");
        let caps = vec![cap("bash.jj.status", "x", vec![1.0])];
        let mut vectors = HashMap::new();
        vectors.insert("bash.jj.status".to_string(), vec![1.0]);
        let snap = Snapshot::build(&caps, &vectors, "m", "CAT".into(), "PATH".into());
        snap.save(&path).unwrap();

        // Catalog hash drift => Stale.
        assert!(matches!(
            load_valid(&path, "DIFFERENT", "PATH"),
            LoadOutcome::Stale(_)
        ));
        // PATH hash drift => Stale.
        assert!(matches!(
            load_valid(&path, "CAT", "DIFFERENT"),
            LoadOutcome::Stale(_)
        ));
        // Matching => Fresh.
        assert!(matches!(
            load_valid(&path, "CAT", "PATH"),
            LoadOutcome::Fresh { .. }
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_is_missing_not_error() {
        let path = std::env::temp_dir().join("t4c-snap-does-not-exist-xyzzy.rkyv");
        let _ = std::fs::remove_file(&path);
        assert!(matches!(load_valid(&path, "a", "b"), LoadOutcome::Missing));
    }

    #[test]
    fn corrupt_bytes_are_stale_not_panic() {
        let dir = std::env::temp_dir().join(format!("t4c-snap-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("snapshot.rkyv");
        std::fs::write(&path, b"this is not a valid rkyv archive at all").unwrap();
        assert!(matches!(load_valid(&path, "a", "b"), LoadOutcome::Stale(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn path_set_hash_is_order_independent() {
        // Pure-function test: no global PATH mutation (would race other tests).
        // Same dirs, different order => same hash.
        let h1 = hash_path_str("/a:/b:/c", ':');
        let h2 = hash_path_str("/c:/a:/b", ':');
        assert_eq!(h1, h2);
        // Trailing/duplicate/empty segments don't change the canonical set.
        let h_dup = hash_path_str("/a:/b:/c:/a::", ':');
        assert_eq!(h1, h_dup);
        // Adding a distinct dir => different hash.
        let h3 = hash_path_str("/a:/b:/c:/d", ':');
        assert_ne!(h1, h3);
    }

    #[test]
    fn concurrent_saves_produce_a_valid_archive() {
        // Finding (gpt-5.5 panel): a fixed `.rkyv.tmp` raced under concurrent
        // writers. With per-writer unique temp names, N threads saving to the
        // same path must each commit via their own temp and the final file must
        // be a clean, loadable archive (never a torn half-write from a clobbered
        // shared temp).
        let dir = std::env::temp_dir().join(format!("t4c-concsave-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("snapshot.rkyv");

        let caps = vec![cap("bash.jj.status", "x", vec![1.0])];
        let vectors = HashMap::new();
        let snap = std::sync::Arc::new(Snapshot::build(
            &caps,
            &vectors,
            "m",
            "CAT".into(),
            "PATH".into(),
        ));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let snap = snap.clone();
                let path = path.clone();
                std::thread::spawn(move || snap.save(&path).is_ok())
            })
            .collect();
        for h in handles {
            assert!(h.join().unwrap(), "a concurrent save failed");
        }

        // The committed file loads cleanly (not torn) and no stray temp files
        // were left behind in the dir.
        assert!(matches!(
            load_valid(&path, "CAT", "PATH"),
            LoadOutcome::Fresh { .. }
        ));
        let strays: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(strays.is_empty(), "leftover temp files: {strays:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn archived_paths_reads_present_set_regardless_of_freshness() {
        // archived_paths must surface the snapshot's category set EVEN when the
        // snapshot would be judged stale — that's what lets `verify` tell a lost
        // category from a moved-but-whole world. We read with no hash context at
        // all (the function takes none), proving freshness is not gated.
        let dir = std::env::temp_dir().join(format!("t4c-arch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("snapshot.rkyv");
        let caps = vec![
            cap("bash.jj.status", "x", vec![1.0]),
            cap("bash.gh.pr", "y", vec![2.0]),
        ];
        let vectors = HashMap::new();
        // Hashes here are arbitrary — archived_paths ignores them by design.
        let snap = Snapshot::build(&caps, &vectors, "m", "WHATEVER".into(), "WHATEVER".into());
        snap.save(&path).unwrap();

        let mut got = archived_paths(&path).expect("readable archive");
        got.sort();
        assert_eq!(got, vec!["bash.gh.pr", "bash.jj.status"]);

        // Missing file => None (not a panic, not an empty vec masquerading as read).
        let _ = std::fs::remove_file(&path);
        assert!(archived_paths(&path).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
