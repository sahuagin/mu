//! Project-file [`RecallProvider`] — reads the canonical CLAUDE.md /
//! AGENTS.md hierarchy at session start and wraps each present file as
//! one [`RecalledItem`] with [`RecallSource::ProjectFile`].
//!
//! mu-phl v0 / bead `mu-zj4e`. The file set is mu-native: project-local
//! files first (so they can override globals), then the operator's
//! mu global under `~/.config/mu/`.
//!
//! mu-native migration (bead `mu-mu-native-config-sources-98j7`): the
//! defaults no longer borrow the operator's claude-code / pi-rust config
//! (`~/.claude-personal/CLAUDE.md`, `~/CLAUDE.md`, `~/.pi/agent/AGENTS.md`).
//! mu reads its OWN files under `~/.config/mu/` — the same root the
//! layered [`crate::config::Config`] loads `config.toml` from. A
//! deployment that still wants those files can pass them explicitly via
//! [`ProjectFileRecallProvider::with_files`].
//!
//! Behavior:
//!
//! - Missing files: skipped silently (no warn, no panic). Most installs
//!   won't have all of them.
//! - Non-readable files: logged at `warn`, skipped.
//! - Duplicate canonical paths (e.g., a symlink resolving to the same
//!   target as an absolute entry): emitted ONCE, in the order of first
//!   appearance.
//! - Each present file → exactly one [`RecalledItem`]. v0 doesn't parse
//!   content; whole-file goes in.
//!
//! v1's mu-phl ingest pipeline replaces this with idempotent
//! `MemoryIngest` events (content-hash dedup, `supersedes` edges, etc.);
//! the trait shape stays the same.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::capability::Capability;
use crate::context::rope::SpanText;

use super::{RecallProvider, RecallSource, RecalledItem};

/// Canonical file hierarchy for v0. Read in order; all present files
/// are included. Leading `./` resolves against the session's `cwd`;
/// leading `~/` resolves against the operator's home directory
/// (`$HOME` / `dirs::home_dir()`).
pub const DEFAULT_FILES_IN_ORDER: &[&str] = &[
    "./CLAUDE.md",            // project root (overrides the global)
    "./AGENTS.md",            // project root
    "~/.config/mu/CLAUDE.md", // operator's mu-native global
    "~/.config/mu/AGENTS.md", // operator's mu-native global
];

/// Reads a fixed hierarchy of project-context files at session start.
/// Construct with [`Self::default`] for the canonical set above; tests
/// (and future per-deployment overrides) can use [`Self::with_files`]
/// to substitute a different list.
#[derive(Debug)]
pub struct ProjectFileRecallProvider {
    /// Raw template paths — each entry may contain leading `./` or
    /// `~/` to be resolved per-call against the session's `cwd` and
    /// the operator's `$HOME`.
    files: Vec<String>,
}

impl ProjectFileRecallProvider {
    /// Construct with an explicit list of template paths. Each entry
    /// follows the same `./` / `~/` resolution rules as
    /// [`DEFAULT_FILES_IN_ORDER`].
    pub fn with_files<I, S>(files: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            files: files.into_iter().map(Into::into).collect(),
        }
    }
}

impl Default for ProjectFileRecallProvider {
    fn default() -> Self {
        Self::with_files(DEFAULT_FILES_IN_ORDER.iter().copied())
    }
}

impl RecallProvider for ProjectFileRecallProvider {
    fn recall(&self, cwd: &Path, _capability: &Capability) -> Vec<RecalledItem> {
        let home = dirs::home_dir();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut out: Vec<RecalledItem> = Vec::with_capacity(self.files.len());

        for template in &self.files {
            let Some(raw_path) = resolve_template(template, cwd, home.as_deref()) else {
                continue;
            };

            // Canonicalize so symlinks resolve to the same key for dedup.
            // If canonicalize fails (typically: file doesn't exist), skip.
            let canonical = match raw_path.canonicalize() {
                Ok(p) => p,
                Err(_) => continue,
            };

            // Don't read directories — only files.
            if !canonical.is_file() {
                continue;
            }

            if !seen.insert(canonical.clone()) {
                continue;
            }

            let content = match std::fs::read_to_string(&canonical) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        path = %canonical.display(),
                        error = %e,
                        "ProjectFileRecallProvider: skipping unreadable file",
                    );
                    continue;
                }
            };

            // Skip empty files — they'd produce empty spans for no value.
            if content.trim().is_empty() {
                continue;
            }

            // Stable id: blake3 hash of the canonical-path bytes. Same
            // file in the same location → same id across sessions
            // (rope dedup); a moved or renamed file gets a new id.
            // Truncate to 12 hex chars (~48 bits, plenty for a session-
            // scoped id space).
            let path_hash = blake3::hash(canonical.to_string_lossy().as_bytes());
            let hash_hex = path_hash.to_hex();
            let stable_id: SpanText = format!("file-{}", &hash_hex.as_str()[..12]).into();

            out.push(RecalledItem {
                source: RecallSource::ProjectFile {
                    path: canonical.clone(),
                },
                content: content.into(),
                stable_id,
            });
        }

        out
    }
}

/// Resolve a template path like `./CLAUDE.md` or `~/foo/bar.md` against
/// the session's `cwd` and the operator's `home`. Returns `None` if a
/// `~/` template is given but `home` is `None` (unusual, but possible
/// in some test environments).
fn resolve_template(template: &str, cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(rest) = template.strip_prefix("~/") {
        home.map(|h| h.join(rest))
    } else if let Some(rest) = template.strip_prefix("./") {
        Some(cwd.join(rest))
    } else {
        Some(PathBuf::from(template))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique tempdir + return its path. The dir is leaked at end of
    /// test (process-scoped lifetime); each test gets its own.
    fn fresh_tempdir() -> PathBuf {
        let pid = std::process::id();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("mu-projfile-test-{pid}-{unique}"));
        std::fs::create_dir_all(&dir).expect("create tempdir");
        dir
    }

    #[test]
    fn no_files_present_returns_empty() {
        let dir = fresh_tempdir();
        // Templates pointing inside an empty dir → nothing.
        let provider = ProjectFileRecallProvider::with_files(["./CLAUDE.md", "./AGENTS.md"]);
        let items = provider.recall(&dir, &Capability::root());
        assert!(items.is_empty());
    }

    #[test]
    fn one_file_present_yields_one_item() {
        let dir = fresh_tempdir();
        std::fs::write(dir.join("CLAUDE.md"), "# project\nsome project context\n")
            .expect("write CLAUDE.md");

        let provider = ProjectFileRecallProvider::with_files(["./CLAUDE.md", "./AGENTS.md"]);
        let items = provider.recall(&dir, &Capability::root());

        assert_eq!(items.len(), 1);
        let item = &items[0];
        match &item.source {
            RecallSource::ProjectFile { path } => {
                assert!(path.ends_with("CLAUDE.md"));
            }
            other => panic!("expected ProjectFile source, got {other:?}"),
        }
        assert!(item.content.contains("some project context"));
        assert!(item.stable_id.starts_with("file-"));
        assert_eq!(item.stable_id.len(), "file-".len() + 12);
    }

    #[test]
    fn multiple_files_yield_items_in_declared_order() {
        let dir = fresh_tempdir();
        std::fs::write(dir.join("CLAUDE.md"), "project rules").expect("write CLAUDE");
        std::fs::write(dir.join("AGENTS.md"), "agent rules").expect("write AGENTS");

        let provider = ProjectFileRecallProvider::with_files(["./CLAUDE.md", "./AGENTS.md"]);
        let items = provider.recall(&dir, &Capability::root());

        assert_eq!(items.len(), 2);
        assert!(items[0].content.contains("project rules"));
        assert!(items[1].content.contains("agent rules"));
    }

    #[test]
    fn duplicate_canonical_paths_dedupe() {
        // Two templates resolving to the same canonical path (one direct,
        // one via symlink). Result should include the file only once.
        let dir = fresh_tempdir();
        let real = dir.join("CLAUDE.md");
        std::fs::write(&real, "content").expect("write real");
        let symlink = dir.join("CLAUDE-link.md");
        std::os::unix::fs::symlink(&real, &symlink).expect("symlink");

        let provider = ProjectFileRecallProvider::with_files(["./CLAUDE.md", "./CLAUDE-link.md"]);
        let items = provider.recall(&dir, &Capability::root());
        assert_eq!(items.len(), 1, "symlink dedup should leave 1 item");
    }

    #[test]
    fn empty_files_skipped() {
        let dir = fresh_tempdir();
        std::fs::write(dir.join("CLAUDE.md"), "").expect("write empty");
        std::fs::write(dir.join("AGENTS.md"), "real content").expect("write content");

        let provider = ProjectFileRecallProvider::with_files(["./CLAUDE.md", "./AGENTS.md"]);
        let items = provider.recall(&dir, &Capability::root());

        assert_eq!(items.len(), 1);
        assert!(items[0].content.contains("real content"));
    }

    #[test]
    fn missing_directory_does_not_panic() {
        // Templates that resolve to paths in a directory that doesn't
        // exist at all — should return empty, no panic.
        let provider = ProjectFileRecallProvider::with_files([
            "./does-not-exist/CLAUDE.md",
            "./also-not-there/AGENTS.md",
        ]);
        let nowhere = PathBuf::from("/this/path/does/not/exist");
        let items = provider.recall(&nowhere, &Capability::root());
        assert!(items.is_empty());
    }

    #[test]
    fn resolve_template_handles_tilde_dot_and_absolute() {
        let cwd = PathBuf::from("/home/user/project");
        let home = PathBuf::from("/home/user");

        assert_eq!(
            resolve_template("./CLAUDE.md", &cwd, Some(&home)),
            Some(PathBuf::from("/home/user/project/CLAUDE.md")),
        );
        assert_eq!(
            resolve_template("~/.config/mu/CLAUDE.md", &cwd, Some(&home)),
            Some(PathBuf::from("/home/user/.config/mu/CLAUDE.md")),
        );
        assert_eq!(
            resolve_template("/etc/global.md", &cwd, Some(&home)),
            Some(PathBuf::from("/etc/global.md")),
        );
        // tilde with no home → None
        assert_eq!(resolve_template("~/file.md", &cwd, None), None);
    }
}
