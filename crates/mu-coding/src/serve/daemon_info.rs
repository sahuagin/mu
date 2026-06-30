//! Daemon-level identity + counters (mu-038).
//!
//! `DaemonInfo` carries the per-process stable ID and start time that
//! the `daemon.stats` RPC and the discovery layer both need. Cheap
//! to clone (Arc-backed); shared by dispatch and the discovery
//! backend.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use mu_core::config::Config;
use mu_core::context::RecallProvider;
use mu_core::protocol::McpServerStatus;
use mu_core::route_catalog::RouteCatalog;

use super::factory::BashSettings;

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
    /// Daemon-authoritative outbound MCP import snapshot. Populated once
    /// after startup import attempts complete; queried by `daemon.mcp_status`
    /// so frontends do not re-derive import truth from local config.
    mcp_status: Arc<Mutex<Vec<McpServerStatus>>>,
    /// mu-phl v0 (mu-0bxv): session-start recall providers. The
    /// session handler iterates these on `create_session` /
    /// `session.delegate`, collects [`RecalledItem`]s, and bundles
    /// them into the new session's `AgentConfig.project_context`.
    /// Default: empty Vec (tests pass; no recall happens). Production
    /// wires up [`SubprocessRecallProvider`] +
    /// [`ProjectFileRecallProvider`] via [`with_recall_providers`].
    recall_providers: Arc<Vec<Arc<dyn RecallProvider>>>,
    route_catalog: Arc<RouteCatalog>,
    /// mu-qnag: the daemon's resolved command-execution policy (from
    /// `--bash-yolo` / `--bash-allow` / `--bash-prompt`). Both the `bash`
    /// tool and the per-session `watch` tool gate commands through this,
    /// so a restricted session's `watch` cannot run what its `bash`
    /// couldn't. Default = strict, no extras (the safe floor) for tests
    /// and a bare `new`.
    bash_settings: BashSettings,
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
                mcp_status: Arc::new(Mutex::new(Vec::new())),
                recall_providers: Arc::new(Vec::new()),
                route_catalog: Arc::new(RouteCatalog::from_env()),
                bash_settings: BashSettings::default(),
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

    /// Builder-style setter for the route catalog. Production
    /// `serve::run` uses this to install a catalog augmented with
    /// dynamically-discovered ollama models (the best-effort startup
    /// probe); tests and the default path keep `RouteCatalog::from_env`
    /// as built in [`new`]. (bead mu-818c)
    pub fn with_route_catalog(self, route_catalog: RouteCatalog) -> Self {
        let inner = (*self.inner).clone();
        Self {
            inner: Arc::new(DaemonInfoInner {
                route_catalog: Arc::new(route_catalog),
                ..inner
            }),
        }
    }

    /// mu-qnag: builder-style setter for the daemon's bash/command
    /// policy. Production `serve::run` passes the settings resolved from
    /// the `--bash-*` flags; tests and the default path keep
    /// `BashSettings::default()` (strict, no extras).
    pub fn with_bash_settings(self, bash_settings: BashSettings) -> Self {
        let inner = (*self.inner).clone();
        Self {
            inner: Arc::new(DaemonInfoInner {
                bash_settings,
                ..inner
            }),
        }
    }

    pub fn events_dir(&self) -> Option<&std::path::Path> {
        self.inner.events_dir.as_deref()
    }

    /// mu-qnag: read access to the daemon's command-execution policy.
    /// `session_spawn_tools` resolves a `BashMode` from this for the
    /// per-session `watch` tool, so watch and bash share one gate.
    pub fn bash_settings(&self) -> &BashSettings {
        &self.inner.bash_settings
    }

    /// mu-l1z: read access to the loaded config. Dispatch handlers
    /// and other daemon-side consumers (compaction judge selection
    /// in mu-kgu.11) call this to resolve operator preferences.
    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// Daemon-authoritative snapshot of outbound MCP import attempts.
    pub fn set_mcp_status(&self, status: Vec<McpServerStatus>) {
        match self.inner.mcp_status.lock() {
            Ok(mut guard) => *guard = status,
            Err(poisoned) => *poisoned.into_inner() = status,
        }
    }

    pub fn mcp_status_snapshot(&self) -> Vec<McpServerStatus> {
        self.inner
            .mcp_status
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
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
                mcp_status: Arc::new(Mutex::new(Vec::new())),
                recall_providers: Arc::new(Vec::new()),
                route_catalog: Arc::new(RouteCatalog::from_env()),
                bash_settings: BashSettings::default(),
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
