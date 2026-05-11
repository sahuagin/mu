//! Daemon-level identity + counters (mu-038).
//!
//! `DaemonInfo` carries the per-process stable ID and start time that
//! the `daemon.stats` RPC and the discovery layer both need. Cheap
//! to clone (Arc-backed); shared by dispatch and the discovery
//! backend.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct DaemonInfo {
    inner: Arc<DaemonInfoInner>,
}

#[derive(Debug)]
struct DaemonInfoInner {
    /// Stable identifier for this daemon's lifetime. New on every
    /// process start. Generated as a hex-encoded random 64-bit
    /// number; no UUID dep.
    daemon_id: String,
    version: String,
    started_at_unix_ms: u64,
}

impl DaemonInfo {
    /// Create a fresh daemon info with a random id. Called once per
    /// `mu serve` process at startup.
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
            }),
        }
    }

    /// Test helper: deterministic id.
    #[cfg(test)]
    pub fn test_with_id(id: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(DaemonInfoInner {
                daemon_id: id.into(),
                version: version.into(),
                started_at_unix_ms: 0,
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
