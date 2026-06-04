//! Subprocess-backed [`RecallProvider`] — shells out to the operator's
//! `agent` CLI to fetch session-start memory context.
//!
//! mu-phl v0 / bead `mu-3j32`. Wraps `agent memory context --cwd <cwd>`'s
//! markdown output as a single [`RecalledItem`] with
//! [`RecallSource::Memory`]. The output is sub-10ms in practice (per the
//! Phase 1 exploration measurement) so we use a blocking
//! `Command::output()` call without an explicit watchdog timeout; if a
//! pathological case surfaces, a `wait-timeout`-style escape hatch can
//! land as a follow-up.
//!
//! Behavior:
//!
//! - Binary absent (e.g. `~/.local/bin/agent` not on this machine):
//!   returns empty vec, logs once at `warn` level. No panic.
//! - Binary present but exits non-zero: returns empty vec, logs the
//!   stderr at `warn` level.
//! - Binary present, exits zero, stdout empty: returns empty vec.
//! - Binary present, exits zero, stdout non-empty: wraps as one item.
//!
//! v1's `EventLogRecallProvider` (full mu-phl event-pointer architecture)
//! replaces this; the trait shape stays the same.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::capability::Capability;
use crate::context::rope::SpanText;

use super::{RecallProvider, RecallSource, RecalledItem};

/// Shells out to the operator's `agent` CLI for session-start memory
/// recall. Construct with [`Self::default`] for the standard
/// `~/.local/bin/agent` path; tests can use [`Self::with_binary`] to
/// point at a stub.
#[derive(Debug)]
pub struct SubprocessRecallProvider {
    binary_path: PathBuf,
    /// mu-zk2i: injection tier passed as `--tier <this>` to the CLI.
    /// `"identity"` (the [`Default`]) requests the small kernel —
    /// user-first identity rows + identity-tagged rules, with task
    /// detail demoted to the `memory_recall` tool; `"full"` requests
    /// the classic four-section wall. The CLI owns tier semantics and
    /// ordering (user-first is rendered there — mu-42x8 lever a); mu
    /// passes the dial through verbatim from `[recall].tier`.
    tier: String,
    /// AtomicBool so the "binary not found" warning logs only once per
    /// session even though `recall()` may be called multiple times. The
    /// recall trait is `Send + Sync`, so plain mutation isn't an option.
    warned_about_missing_binary: Arc<AtomicBool>,
}

impl SubprocessRecallProvider {
    /// Construct pointing at a non-default binary path. Primary use
    /// case: tests pointing at a stub script.
    pub fn with_binary(binary_path: impl Into<PathBuf>) -> Self {
        Self {
            binary_path: binary_path.into(),
            tier: "identity".to_string(),
            warned_about_missing_binary: Arc::new(AtomicBool::new(false)),
        }
    }

    /// mu-zk2i: override the injection tier (from `[recall].tier`).
    pub fn with_tier(mut self, tier: impl Into<String>) -> Self {
        self.tier = tier.into();
        self
    }
}

impl Default for SubprocessRecallProvider {
    /// Default binary path: `~/.local/bin/agent`. Falls back to bare
    /// `agent` if `$HOME` is unset (rare). Tier defaults to
    /// `"identity"` (the small kernel).
    fn default() -> Self {
        let path = dirs::home_dir()
            .map(|h| h.join(".local").join("bin").join("agent"))
            .unwrap_or_else(|| PathBuf::from("agent"));
        Self::with_binary(path)
    }
}

impl RecallProvider for SubprocessRecallProvider {
    fn recall(&self, cwd: &Path, _capability: &Capability) -> Vec<RecalledItem> {
        let output = match Command::new(&self.binary_path)
            .arg("memory")
            .arg("context")
            .arg("--cwd")
            .arg(cwd)
            .arg("--tier")
            .arg(&self.tier)
            .output()
        {
            Ok(o) => o,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if !self
                    .warned_about_missing_binary
                    .swap(true, Ordering::Relaxed)
                {
                    tracing::warn!(
                        binary = %self.binary_path.display(),
                        cwd = %cwd.display(),
                        "SubprocessRecallProvider: agent CLI not found; session-start \
                         memory recall disabled. install ~/.local/bin/agent or supply \
                         a custom binary path to silence.",
                    );
                }
                return Vec::new();
            }
            Err(e) => {
                tracing::warn!(
                    binary = %self.binary_path.display(),
                    cwd = %cwd.display(),
                    error = %e,
                    "SubprocessRecallProvider: failed to spawn agent CLI",
                );
                return Vec::new();
            }
        };

        if !output.status.success() {
            let stderr_excerpt: String = String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(200)
                .collect();
            tracing::warn!(
                binary = %self.binary_path.display(),
                cwd = %cwd.display(),
                status = ?output.status.code(),
                stderr = %stderr_excerpt,
                "SubprocessRecallProvider: agent CLI exited non-zero",
            );
            return Vec::new();
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            return Vec::new();
        }

        // Stable id: short blake3 hash over the recall payload so re-recalls
        // with identical output produce the same span id (rope dedup).
        // 12 hex chars ≈ 48 bits — collision-resistant enough for a
        // per-session id space.
        let hash = blake3::hash(stdout.as_bytes());
        let hash_hex = hash.to_hex();
        let stable_id: SpanText = format!("memory-{}", &hash_hex.as_str()[..12]).into();

        vec![RecalledItem {
            source: RecallSource::Memory,
            content: stdout.into_owned().into(),
            stable_id,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    /// Create a temp directory + write an executable shell script there.
    /// Returns the script path. Caller is responsible for cleanup (the
    /// path stays on disk until tempdir is dropped — we deliberately
    /// leak in tests since this runs once per process).
    fn write_stub_binary(name: &str, body: &str) -> PathBuf {
        let pid = std::process::id();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("mu-recall-test-{pid}-{unique}"));
        std::fs::create_dir_all(&dir).expect("create tempdir");
        let script = dir.join(name);
        std::fs::write(&script, body).expect("write stub binary");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub binary");
        script
    }

    #[test]
    fn missing_binary_returns_empty_no_panic() {
        let provider = SubprocessRecallProvider::with_binary(
            "/this/path/definitely/does/not/exist/agent-xyz-stub",
        );
        let items = provider.recall(Path::new("/tmp"), &Capability::root());
        assert!(items.is_empty());
    }

    #[test]
    fn missing_binary_warns_only_once_per_session() {
        let provider = SubprocessRecallProvider::with_binary(
            "/this/path/definitely/does/not/exist/agent-xyz-stub",
        );
        // Invoke twice. Both should return empty + no panic. The second
        // shouldn't re-spam the log (verified by inspecting the
        // warned_about_missing_binary AtomicBool flipping from false to
        // true on the first call).
        assert!(!provider.warned_about_missing_binary.load(Ordering::Relaxed));
        let _ = provider.recall(Path::new("/tmp"), &Capability::root());
        assert!(provider.warned_about_missing_binary.load(Ordering::Relaxed));
        let _ = provider.recall(Path::new("/tmp"), &Capability::root());
        assert!(provider.warned_about_missing_binary.load(Ordering::Relaxed));
    }

    #[test]
    fn non_zero_exit_returns_empty() {
        // /bin/false exits with status 1 and no output.
        let provider = SubprocessRecallProvider::with_binary("/bin/false");
        let items = provider.recall(Path::new("/tmp"), &Capability::root());
        assert!(items.is_empty());
    }

    #[test]
    fn successful_run_wraps_stdout_as_one_recalled_item() {
        // Stub binary that prints known content regardless of args.
        let script = write_stub_binary(
            "fake-agent",
            "#!/bin/sh\nprintf '## Fake recall\\n\\nbody here\\n'\n",
        );
        let provider = SubprocessRecallProvider::with_binary(&script);
        let items = provider.recall(Path::new("/tmp"), &Capability::root());

        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert!(matches!(item.source, RecallSource::Memory));
        assert!(item.content.contains("## Fake recall"));
        assert!(item.content.contains("body here"));
        // stable_id is "memory-<12-hex>" format.
        assert!(item.stable_id.starts_with("memory-"));
        assert_eq!(item.stable_id.len(), "memory-".len() + 12);
    }

    #[test]
    fn empty_stdout_returns_empty() {
        // Stub that exits zero with no output.
        let script = write_stub_binary("silent-agent", "#!/bin/sh\nexit 0\n");
        let provider = SubprocessRecallProvider::with_binary(&script);
        let items = provider.recall(Path::new("/tmp"), &Capability::root());
        assert!(items.is_empty());
    }

    // ── mu-zk2i: injection tier ───────────────────────────────────

    /// Stub that echoes its argv so the test can assert exactly what
    /// reached the CLI.
    fn argv_echo_provider(name: &str) -> SubprocessRecallProvider {
        let script = write_stub_binary(name, "#!/bin/sh\necho \"argv: $@\"\n");
        SubprocessRecallProvider::with_binary(&script)
    }

    #[test]
    fn default_tier_is_identity_and_reaches_argv() {
        let provider = argv_echo_provider("tier-default-agent");
        let items = provider.recall(Path::new("/tmp"), &Capability::root());
        assert_eq!(items.len(), 1);
        assert!(
            items[0]
                .content
                .contains("memory context --cwd /tmp --tier identity"),
            "small kernel is the default; got: {}",
            items[0].content
        );
    }

    #[test]
    fn with_tier_full_restores_the_wall() {
        let provider = argv_echo_provider("tier-full-agent").with_tier("full");
        let items = provider.recall(Path::new("/tmp"), &Capability::root());
        assert_eq!(items.len(), 1);
        assert!(
            items[0].content.contains("--tier full"),
            "[recall].tier = \"full\" must pass through verbatim; got: {}",
            items[0].content
        );
    }
}
