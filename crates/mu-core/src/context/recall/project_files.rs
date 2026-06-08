//! Project-file [`RecallProvider`] — reads mu's MU.md / AGENTS.md
//! hierarchy at session start and wraps each present file as one
//! [`RecalledItem`] with [`RecallSource::ProjectFile`].
//!
//! mu-phl v0 / bead `mu-zj4e`. The set ([`default_files_in_order`]) is
//! each of [`DEFAULT_FILENAMES`] (`MU.md`, `AGENTS.md`) at the project
//! root first (so a project file overrides the global), then under the
//! operator's mu config dir (`dirs::config_dir()/mu`) — the same XDG-aware
//! root [`crate::config::Config`] loads `config.toml` from and skills load
//! from. A deployment that wants a different set passes it explicitly via
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

/// Filenames mu looks for, in priority order, at each searched base.
pub const DEFAULT_FILENAMES: &[&str] = &["MU.md", "AGENTS.md"];

/// The default template list: each [`DEFAULT_FILENAMES`] entry at the
/// project root (`./<name>`) first, then under the operator's mu config
/// dir (`<config>/mu/<name>`). The operator base is resolved via
/// [`dirs::config_dir`] — XDG-aware, the SAME root [`crate::config::Config`]
/// loads `config.toml` from and skills load from (so `XDG_CONFIG_HOME`
/// can't split them). Read in order; all present files are included.
/// Project-root entries stay relative (resolved against the session cwd
/// in [`resolve_template`]); operator entries are absolute. (A future
/// config override would prepend this list; not wired yet.)
pub fn default_files_in_order() -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(2 * DEFAULT_FILENAMES.len());
    for name in DEFAULT_FILENAMES {
        out.push(format!("./{name}"));
    }
    if let Some(mu_cfg) = dirs::config_dir().map(|c| c.join("mu")) {
        for name in DEFAULT_FILENAMES {
            out.push(mu_cfg.join(name).to_string_lossy().into_owned());
        }
    }
    out
}

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
    /// [`default_files_in_order`].
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
        Self::with_files(default_files_in_order())
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

/// Resolve a template path like `./MU.md` or `~/foo/bar.md` against
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
        let provider = ProjectFileRecallProvider::with_files(["./MU.md", "./AGENTS.md"]);
        let items = provider.recall(&dir, &Capability::root());
        assert!(items.is_empty());
    }

    #[test]
    fn one_file_present_yields_one_item() {
        let dir = fresh_tempdir();
        std::fs::write(dir.join("MU.md"), "# project\nsome project context\n")
            .expect("write MU.md");

        let provider = ProjectFileRecallProvider::with_files(["./MU.md", "./AGENTS.md"]);
        let items = provider.recall(&dir, &Capability::root());

        assert_eq!(items.len(), 1);
        let item = &items[0];
        match &item.source {
            RecallSource::ProjectFile { path } => {
                assert!(path.ends_with("MU.md"));
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
        std::fs::write(dir.join("MU.md"), "project rules").expect("write MU");
        std::fs::write(dir.join("AGENTS.md"), "agent rules").expect("write AGENTS");

        let provider = ProjectFileRecallProvider::with_files(["./MU.md", "./AGENTS.md"]);
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
        let real = dir.join("MU.md");
        std::fs::write(&real, "content").expect("write real");
        let symlink = dir.join("MU-link.md");
        std::os::unix::fs::symlink(&real, &symlink).expect("symlink");

        let provider = ProjectFileRecallProvider::with_files(["./MU.md", "./MU-link.md"]);
        let items = provider.recall(&dir, &Capability::root());
        assert_eq!(items.len(), 1, "symlink dedup should leave 1 item");
    }

    #[test]
    fn empty_files_skipped() {
        let dir = fresh_tempdir();
        std::fs::write(dir.join("MU.md"), "").expect("write empty");
        std::fs::write(dir.join("AGENTS.md"), "real content").expect("write content");

        let provider = ProjectFileRecallProvider::with_files(["./MU.md", "./AGENTS.md"]);
        let items = provider.recall(&dir, &Capability::root());

        assert_eq!(items.len(), 1);
        assert!(items[0].content.contains("real content"));
    }

    #[test]
    fn missing_directory_does_not_panic() {
        // Templates that resolve to paths in a directory that doesn't
        // exist at all — should return empty, no panic.
        let provider = ProjectFileRecallProvider::with_files([
            "./does-not-exist/MU.md",
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
            resolve_template("./MU.md", &cwd, Some(&home)),
            Some(PathBuf::from("/home/user/project/MU.md")),
        );
        assert_eq!(
            resolve_template("~/.config/mu/MU.md", &cwd, Some(&home)),
            Some(PathBuf::from("/home/user/.config/mu/MU.md")),
        );
        assert_eq!(
            resolve_template("/etc/global.md", &cwd, Some(&home)),
            Some(PathBuf::from("/etc/global.md")),
        );
        // tilde with no home → None
        assert_eq!(resolve_template("~/file.md", &cwd, None), None);
    }

    #[test]
    fn default_files_in_order_is_mu_native() {
        // mu reads its OWN files: project root first, then the mu config
        // dir. Project entries are exact; operator entries are absolute
        // under <config>/mu (XDG-aware via dirs::config_dir, so we assert
        // the suffix rather than a host-specific prefix), and never claude.
        let files = default_files_in_order();
        assert_eq!(files[0], "./MU.md");
        assert_eq!(files[1], "./AGENTS.md");
        if files.len() > 2 {
            assert!(files[2].ends_with("mu/MU.md"), "got {}", files[2]);
            assert!(files[3].ends_with("mu/AGENTS.md"), "got {}", files[3]);
            assert!(!files.iter().any(|f| f.contains("CLAUDE")));
        }
    }

    #[test]
    fn claude_md_is_not_read() {
        // Negative test: mu reads MU.md, not CLAUDE.md. With the default
        // project-root filenames, a CLAUDE.md in the dir is ignored while
        // MU.md is picked up. (Project-root templates only, so the test
        // stays hermetic and never touches the real ~/.config/mu.)
        let dir = fresh_tempdir();
        std::fs::write(dir.join("CLAUDE.md"), "claude-code's file, not mu's")
            .expect("write CLAUDE.md fixture");
        std::fs::write(dir.join("MU.md"), "mu's own file").expect("write MU.md");

        let templates: Vec<String> = DEFAULT_FILENAMES.iter().map(|n| format!("./{n}")).collect();
        let provider = ProjectFileRecallProvider::with_files(templates);
        let items = provider.recall(&dir, &Capability::root());

        assert_eq!(items.len(), 1, "only MU.md should be picked up");
        assert!(items[0].content.contains("mu's own file"));
        match &items[0].source {
            RecallSource::ProjectFile { path } => {
                assert!(path.ends_with("MU.md"));
                assert!(!path.ends_with("CLAUDE.md"));
            }
            other => panic!("expected ProjectFile source, got {other:?}"),
        }
    }
}
