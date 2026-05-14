//! Daemon-level identity + counters (mu-038).
//!
//! `DaemonInfo` carries the per-process stable ID and start time that
//! the `daemon.stats` RPC and the discovery layer both need. Cheap
//! to clone (Arc-backed); shared by dispatch and the discovery
//! backend.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct DaemonInfo {
    inner: Arc<DaemonInfoInner>,
}

#[derive(Debug, Clone)]
struct DaemonInfoInner {
    /// Stable identifier for this daemon's lifetime. New on every
    /// process start. Generated as a hex-encoded random 64-bit
    /// number; no UUID dep.
    daemon_id: String,
    version: String,
    started_at_unix_ms: u64,
    /// On-disk events directory (mu-upb). When Some, the daemon
    /// attaches a per-session JSONL writer at
    /// `<events_dir>/<daemon_id>/<session_id>.jsonl`. None disables
    /// disk persistence — used by tests to avoid writing into
    /// `~/.local/share/mu/events/`.
    events_dir: Option<PathBuf>,
}

impl DaemonInfo {
    /// Create a fresh daemon info with a random id. Called once per
    /// `mu serve` process at startup. `events_dir` is None by default
    /// (disk persistence off); callers set it via `with_events_dir`
    /// after construction.
    pub fn new(version: impl Into<String>) -> Self {
        let raw: u64 = rand::random();
        Self {
            inner: Arc::new(DaemonInfoInner {
                daemon_id: format!("{raw:016x}"),
                version: version.into(),
                started_at_unix_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                events_dir: None,
            }),
        }
    }

    /// Builder-style setter for the on-disk events directory.
    /// `with_events_dir(None)` disables disk persistence; tests use
    /// this. Production binary passes `default_events_dir()`.
    pub fn with_events_dir(self, events_dir: Option<PathBuf>) -> Self {
        let inner = (*self.inner).clone();
        Self {
            inner: Arc::new(DaemonInfoInner {
                events_dir,
                ..inner
            }),
        }
    }

    pub fn events_dir(&self) -> Option<&std::path::Path> {
        self.inner.events_dir.as_deref()
    }

    /// Test helper: deterministic id, no events_dir.
    #[cfg(test)]
    pub fn test_with_id(id: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(DaemonInfoInner {
                daemon_id: id.into(),
                version: version.into(),
                started_at_unix_ms: 0,
                events_dir: None,
            }),
        }
    }

    pub fn daemon_id(&self) -> &str {
        &self.inner.daemon_id
    }

    pub fn version(&self) -> &str {
        &self.inner.version
    }

    pub fn started_at_unix_ms(&self) -> u64 {
        self.inner.started_at_unix_ms
    }

    pub fn uptime_ms(&self) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        now.saturating_sub(self.inner.started_at_unix_ms)
    }
}
