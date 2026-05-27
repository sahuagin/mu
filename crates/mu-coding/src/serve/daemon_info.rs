//! Daemon-level identity + counters (mu-038).
//!
//! `DaemonInfo` carries the per-process stable ID and start time that
//! the `daemon.stats` RPC and the discovery layer both need. Cheap
//! to clone (Arc-backed); shared by dispatch and the discovery
//! backend.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use mu_core::config::Config;
use mu_core::context::RecallProvider;
use mu_core::route_catalog::RouteCatalog;

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
    /// mu-l1z: parsed config loaded at daemon startup. Held as Arc
    /// so dispatch handlers and other consumers can read it without
    /// going back to disk. Tests pass `Config::default()` to avoid
    /// reading from the developer's `~/.config/mu/config.toml`.
    config: Arc<Config>,
    /// mu-phl v0 (mu-0bxv): session-start recall providers. The
    /// session handler iterates these on `create_session` /
    /// `session.delegate`, collects [`RecalledItem`]s, and bundles
    /// them into the new session's `AgentConfig.project_context`.
    /// Default: empty Vec (tests pass; no recall happens). Production
    /// wires up [`SubprocessRecallProvider`] +
    /// [`ProjectFileRecallProvider`] via [`with_recall_providers`].
    recall_providers: Arc<Vec<Arc<dyn RecallProvider>>>,
    route_catalog: Arc<RouteCatalog>,
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
                config: Arc::new(Config::default()),
                recall_providers: Arc::new(Vec::new()),
                route_catalog: Arc::new(RouteCatalog::from_env()),
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

    /// mu-l1z: builder-style setter for the parsed config. Production
    /// `serve::run` calls this with `Config::load_default()`; tests
    /// pass `Config::default()` for hermetic behavior.
    pub fn with_config(self, config: Config) -> Self {
        let inner = (*self.inner).clone();
        Self {
            inner: Arc::new(DaemonInfoInner {
                config: Arc::new(config),
                ..inner
            }),
        }
    }

    /// mu-phl v0 (mu-0bxv): builder-style setter for the session-start
    /// recall provider chain. Production wires up
    /// `vec![Arc::new(SubprocessRecallProvider::default()),
    ///       Arc::new(ProjectFileRecallProvider::default())]`;
    /// tests pass an empty vec (the default) to skip recall, or a
    /// custom Vec containing stub providers for deterministic
    /// recall-content tests.
    pub fn with_recall_providers(self, providers: Vec<Arc<dyn RecallProvider>>) -> Self {
        let inner = (*self.inner).clone();
        Self {
            inner: Arc::new(DaemonInfoInner {
                recall_providers: Arc::new(providers),
                ..inner
            }),
        }
    }

    pub fn events_dir(&self) -> Option<&std::path::Path> {
        self.inner.events_dir.as_deref()
    }

    /// mu-l1z: read access to the loaded config. Dispatch handlers
    /// and other daemon-side consumers (compaction judge selection
    /// in mu-kgu.11) call this to resolve operator preferences.
    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// mu-phl v0 (mu-0bxv): read access to the recall provider chain.
    /// The session handler iterates this on every `create_session` /
    /// `session.delegate` to build the new session's `ProjectContext`.
    /// Empty in tests and (by default) in `DaemonInfo::new` — production
    /// wires up via [`with_recall_providers`].
    pub fn recall_providers(&self) -> &[Arc<dyn RecallProvider>] {
        &self.inner.recall_providers
    }

    pub fn route_catalog(&self) -> &RouteCatalog {
        &self.inner.route_catalog
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
                config: Arc::new(Config::default()),
                recall_providers: Arc::new(Vec::new()),
                route_catalog: Arc::new(RouteCatalog::from_env()),
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
