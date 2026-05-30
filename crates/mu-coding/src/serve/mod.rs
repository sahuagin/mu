//! `mu serve` mode — JSON-RPC daemon over stdio (or generic
//! reader/writer for tests).

use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncBufRead, AsyncWrite, BufReader};

use mu_core::agent::Tool;
use mu_core::event_log::SessionEventLog;

pub mod auth;
pub mod daemon_info;
pub mod discovery;
mod dispatch;
pub mod factory;
mod forwarder;
mod handlers;
mod mailbox;
pub mod mcp;
mod provider_status;
pub(crate) mod pty_spawn;
mod sessions;
pub(crate) mod worker;

pub use daemon_info::DaemonInfo;
pub use discovery::{FileBackend, LocalRegistryBackend, SessionDiscovery};
pub use factory::{
    build_provider_from_selector, build_tools, make_provider_factory, parse_tools_csv,
    selector_from_cli, BashSettings, ProviderFactory,
};
pub use sessions::Sessions;

struct AbortOnDrop(std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(h) = self.0.lock().unwrap().take() {
            h.abort();
        }
    }
}

/// Default on-disk events directory used by the production binary
/// (mu-upb). `None` means "don't write events to disk." Tests
/// explicitly pass `None` to avoid polluting the developer's
/// `~/.local/share/mu/events/` with test fixtures.
pub fn default_events_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".local/share/mu/events"))
}

/// mu-u1ld: scan `events_dir` for past-run session JSONLs and
/// register them in `sessions` as read-only rehydrated entries.
/// Called once at daemon startup. Returns the number of sessions
/// rehydrated (zero on any I/O error — rehydration is best-effort,
/// never aborts startup).
///
/// Scan structure: `events_dir/<daemon_id>/<session_id>.jsonl`. The
/// new daemon's own `<daemon_id>` directory doesn't exist yet at this
/// point (no sessions have been created), so every visible subdir
/// represents some prior or concurrent daemon. We load every file we
/// find. Concurrent peers' active sessions are also rehydrated as
/// read-only — `FileBackend::list` continues to expose them as remote
/// for `session.list --include-remote`, but local read-only queries
/// (`session.events`, `session.stats`) work too.
///
/// Per the mu-u1ld P1 design (2026-05-18): MVP is read-only and
/// no-cap. Garbage collection and writable rehydration are deferred to
/// follow-up beads.
pub fn rehydrate_sessions(events_dir: &std::path::Path, sessions: &Sessions) -> usize {
    use mu_core::event_log::{EventPayload, SessionEventLog};
    let outer = match std::fs::read_dir(events_dir) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    let mut count = 0usize;
    for daemon_entry in outer.flatten() {
        let daemon_path = daemon_entry.path();
        if !daemon_path.is_dir() {
            continue;
        }
        let session_files = match std::fs::read_dir(&daemon_path) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for f in session_files.flatten() {
            let session_path = f.path();
            if session_path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let (log, _malformed) = match SessionEventLog::from_jsonl(&session_path) {
                Ok(loaded) => loaded,
                Err(_) => continue,
            };
            // Pull parent_session_id from the SessionCreated event,
            // matching FileBackend's discovery pattern (mu-031 tree
            // queries traverse parents across the boundary).
            let parent_session_id = log.snapshot().iter().find_map(|e| match &e.payload {
                EventPayload::SessionCreated {
                    parent_session_id, ..
                } => parent_session_id.clone(),
                _ => None,
            });
            let session_id = log.session_id().to_string();
            sessions.insert_rehydrated(session_id, Arc::new(log), parent_session_id);
            count += 1;
        }
    }
    count
}

/// mu-l1z: resolve the events directory from a loaded
/// [`mu_core::config::Config`].
///
/// - If the operator opted out of disk persistence via
///   `[session] persist_events_to_disk = false`, returns `None`.
/// - Otherwise, if `[session] state_dir` is set, returns
///   `<state_dir>/events`.
/// - Otherwise, falls back to [`default_events_dir`] (the legacy
///   `~/.local/share/mu/events` path).
pub fn resolve_events_dir(config: &mu_core::config::Config) -> Option<PathBuf> {
    if !config.session.persist_events_to_disk {
        return None;
    }
    config
        .session
        .state_dir
        .as_ref()
        .map(|s| s.join("events"))
        .or_else(default_events_dir)
}

/// Production entry point — serve over the process's stdin/stdout.
///
/// `factory` is called once per session, given the client's
/// `create_session.provider` selector, to construct a fresh
/// `Arc<dyn Provider>`. Multiple sessions on the same daemon can use
/// different providers.
///
/// mu-l1z: loads `Config::load_default()` and consults
/// `[session].state_dir` / `persist_events_to_disk` to derive the
/// events directory. Config-less operators see no behavior change.
///
/// mu-fnn: when the `MU_BEARER_TOKEN` environment variable is set, it
/// overrides any bearer tokens from the config file with a single
/// process-scoped token. This is the trust-on-spawn handshake used by
/// `mu ask` (and any in-process parent that spawns `mu serve` as a
/// child): the parent generates a token, exports it to the child, and
/// presents the same token in `peer.auth_initiate`. The env override
/// is intentionally one-shot and not persisted to disk.
pub async fn run(factory: ProviderFactory, tools: Vec<Arc<dyn Tool>>) -> anyhow::Result<()> {
    let mut config = mu_core::config::Config::load_default();
    if let Ok(token) = std::env::var("MU_BEARER_TOKEN") {
        if !token.is_empty() {
            config.auth = mu_core::config::AuthConfig::Bearer {
                tokens: vec![token],
            };
        }
    }
    let events_dir = resolve_events_dir(&config);
    let stdin = BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    serve_with_io_with_config(stdin, stdout, factory, tools, events_dir, config).await
}

/// Test/integration hook — serve over generic reader/writer.
///
/// `events_dir` controls on-disk event log persistence (mu-upb).
/// Tests should pass `None` to avoid writing fixtures into the
/// developer's home directory; production passes
/// `default_events_dir()`.
///
/// mu-l1z: uses [`mu_core::config::Config::default`] for the
/// daemon's config. Tests that need a non-default config should
/// call [`serve_with_io_with_config`] directly.
pub async fn serve_with_io<R, W>(
    reader: R,
    writer: W,
    factory: ProviderFactory,
    tools: Vec<Arc<dyn Tool>>,
    events_dir: Option<PathBuf>,
) -> anyhow::Result<()>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    serve_with_io_with_config(
        reader,
        writer,
        factory,
        tools,
        events_dir,
        mu_core::config::Config::default(),
    )
    .await
}

/// mu-l1z: test/integration hook with explicit `Config`. Production
/// [`run`] loads `Config::load_default()` and calls this. Tests pass
/// `Config::default()` (or a custom one if they're testing
/// config-driven behavior).
pub async fn serve_with_io_with_config<R, W>(
    reader: R,
    writer: W,
    factory: ProviderFactory,
    tools: Vec<Arc<dyn Tool>>,
    events_dir: Option<PathBuf>,
    config: mu_core::config::Config,
) -> anyhow::Result<()>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let sessions = Sessions::new();
    // mu-u1ld: rehydrate past sessions from on-disk event logs so
    // `session.list`, `session.events`, and `session.stats` queries
    // see them after a daemon restart. Read-only — no agent loop is
    // spawned. No-op when events_dir is None.
    if let Some(ref dir) = events_dir {
        let _rehydrated = rehydrate_sessions(dir, &sessions);
    }
    // mu-7rk (mu-yox): build the connect-time auth registry from
    // `[auth]` config and allocate a fresh per-connection `AuthState`
    // handle. This `serve_with_io_with_config` call corresponds to one
    // connection — stdio in production, one duplex pipe in tests. The
    // handle is freshly allocated here so cross-connection auth state
    // never leaks.
    let auth_registry = Arc::new(auth::registry_from_config(&config.auth));
    let auth_state: auth::AuthStateHandle =
        Arc::new(std::sync::Mutex::new(auth::AuthState::default()));
    // mu-phl v0 (mu-0bxv): wire up the canonical session-start recall
    // provider chain. Tests construct DaemonInfo without these (empty
    // vec) to skip recall; production runs both.
    let recall_providers: Vec<Arc<dyn mu_core::context::RecallProvider>> = vec![
        Arc::new(mu_core::context::recall::SubprocessRecallProvider::default()),
        Arc::new(mu_core::context::recall::ProjectFileRecallProvider::default()),
    ];
    let daemon_info = DaemonInfo::new(env!("CARGO_PKG_VERSION"))
        .with_events_dir(events_dir)
        .with_config(config)
        .with_recall_providers(recall_providers);
    // mu-slat: register the well-known "supervisor" session so workers
    // always have a stable mailbox target for posting results back.
    // Only in production (events_dir set) — tests don't spawn workers.
    if let Some(dir) = daemon_info.events_dir() {
        let sup_id = String::from("supervisor");
        let sup_log = Arc::new(SessionEventLog::new(sup_id.clone()));
        let path = dir.join(daemon_info.daemon_id()).join("supervisor.jsonl");
        if let Err(e) = sup_log.attach_disk_writer(&path) {
            tracing::warn!(error = %e, "supervisor: could not attach disk writer");
        }
        sessions.insert_rehydrated(sup_id, sup_log, None);
        tracing::info!(daemon_id = %daemon_info.daemon_id(), "registered supervisor mailbox");
    }
    // mu-935: when events_dir is configured (mu-upb's on-disk JSONL
    // path), wrap the local backend with FileBackend so session.list
    // with include_remote=true picks up peer daemons' sessions from
    // the same machine. When events_dir is None (tests, ephemeral
    // mode), the local backend alone is exactly the right behavior.
    let local: Arc<dyn SessionDiscovery> = Arc::new(LocalRegistryBackend::new(
        sessions.clone(),
        daemon_info.daemon_id().to_string(),
    ));
    let discovery: Arc<dyn SessionDiscovery> = match daemon_info.events_dir() {
        Some(dir) => Arc::new(FileBackend::new(
            local,
            dir.to_path_buf(),
            daemon_info.daemon_id().to_string(),
        )),
        None => local,
    };
    // mu-mb02: start MCP server on a unix socket if MU_MCP_SOCKET is
    // set or if the default socket path's parent exists. The MCP surface
    // shares Sessions + DaemonInfo with the primary JSON-RPC loop so
    // mailbox operations are consistent across both surfaces.
    //
    // The MCP task holds a Sessions clone. transport::serve's shutdown
    // cascade (drop handler → drop sessions → agent loops exit →
    // NotificationWriters drop → writer_task completes) deadlocks if
    // any external clone keeps sessions alive. AbortOnDrop is captured
    // by the handler closure so that dropping the handler aborts the
    // MCP task first, releasing its sessions clone.
    let mcp_socket_path = std::env::var("MU_MCP_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|_| mcp::default_mcp_socket_path());
    let mcp_guard = if mcp_socket_path
        .parent()
        .map(|p| p.exists())
        .unwrap_or(false)
    {
        let mcp_sessions = sessions.clone();
        let mcp_daemon_info = daemon_info.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) =
                mcp::serve_mcp_socket(mcp_socket_path, mcp_sessions, mcp_daemon_info).await
            {
                tracing::error!("MCP server exited: {e:#}");
            }
        });
        Some(AbortOnDrop(std::sync::Mutex::new(Some(handle))))
    } else {
        None
    };

    // mu-slat: the spawn_worker tool is injected per-session in
    // build_and_register_session (handlers/session.rs), not here — it
    // needs the calling session's id so worker results route back to
    // the right mailbox. A daemon-global instance can't know its caller.
    let tools = Arc::new(tools);
    // mu-kex4.6.4: discover skills once at startup so `capabilities/discover`
    // can project them alongside tools (the daemon previously knew only tools;
    // skills were the gap the cold-dogfood proved, mu-kex4.6.2). Search dirs:
    // project-local `.mu/skills` under cwd + `~/.config/mu/skills` + the
    // claude-personal compat dir, plus `$MU_SKILLS_DIR` if set.
    let skills = {
        let project_root = std::env::current_dir().ok();
        let mut dirs = mu_core::skill::loader::default_search_dirs(project_root.as_deref());
        if let Ok(extra) = std::env::var("MU_SKILLS_DIR") {
            if !extra.is_empty() {
                dirs.push(PathBuf::from(extra));
            }
        }
        Arc::new(mu_core::skill::loader::discover_skills(&dirs))
    };
    mu_core::transport::serve(reader, writer, move |req, notif| {
        let _ = &mcp_guard;
        let sessions = sessions.clone();
        let factory = factory.clone();
        let tools = tools.clone();
        let skills = skills.clone();
        let daemon_info = daemon_info.clone();
        let discovery = discovery.clone();
        let auth_registry = auth_registry.clone();
        let auth_state = auth_state.clone();
        async move {
            dispatch::dispatch(
                req,
                notif,
                sessions,
                factory,
                tools,
                skills,
                daemon_info,
                discovery,
                auth_registry,
                auth_state,
            )
            .await
        }
    })
    .await
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_core::config::{Config, SessionConfig};

    #[test]
    fn resolve_events_dir_returns_none_when_persist_disabled() {
        let config = Config {
            session: SessionConfig {
                persist_events_to_disk: false,
                state_dir: Some(PathBuf::from("/tmp/should-not-be-used")),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(resolve_events_dir(&config), None);
    }

    #[test]
    fn resolve_events_dir_uses_state_dir_when_set() {
        let config = Config {
            session: SessionConfig {
                persist_events_to_disk: true,
                state_dir: Some(PathBuf::from("/var/lib/mu")),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            resolve_events_dir(&config),
            Some(PathBuf::from("/var/lib/mu/events"))
        );
    }

    fn tempdir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("mu-u1ld-{name}-{}-{nanos}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_session_jsonl(
        events_dir: &std::path::Path,
        daemon_id: &str,
        session_id: &str,
        provider_kind: &str,
        model: &str,
        parent: Option<&str>,
    ) {
        use mu_core::event_log::{EventActor, EventPayload, SessionEventLog};
        let dir = events_dir.join(daemon_id);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join(format!("{session_id}.jsonl"));
        let log = SessionEventLog::new(session_id.to_string());
        log.attach_disk_writer(&path).expect("attach writer");
        log.append(
            EventActor::System,
            EventPayload::SessionCreated {
                provider_kind: provider_kind.into(),
                model: model.into(),
                parent_session_id: parent.map(|s| s.into()),
                branched_at_parent_event_id: None,
            },
        );
    }

    #[test]
    fn rehydrate_sessions_registers_jsonl_entries_as_read_only() {
        // mu-u1ld: drop two daemon-dir JSONLs into a temp events_dir
        // and verify rehydrate_sessions registers them both as
        // read-only ghosts.
        let events_dir = tempdir("rehydrate-two");
        write_session_jsonl(
            &events_dir,
            "daemon-aaa",
            "session-prev-1",
            "anthropic_api",
            "haiku",
            None,
        );
        write_session_jsonl(
            &events_dir,
            "daemon-bbb",
            "session-prev-2",
            "openai_codex",
            "gpt-5",
            Some("session-prev-1"),
        );

        let sessions = Sessions::new();
        let n = rehydrate_sessions(&events_dir, &sessions);
        assert_eq!(n, 2, "both files should rehydrate");

        let listing = sessions.snapshot_for_listing();
        assert_eq!(listing.len(), 2);
        // Parent reference comes through from SessionCreated.
        let by_id: std::collections::HashMap<_, _> = listing
            .iter()
            .map(|(id, _, p)| (id.clone(), p.clone()))
            .collect();
        assert_eq!(by_id["session-prev-1"], None);
        assert_eq!(by_id["session-prev-2"], Some("session-prev-1".to_string()));

        // Live-state queries return None — rehydrated sessions are
        // read-only.
        assert!(sessions.input_sender("session-prev-1").is_none());

        let _ = std::fs::remove_dir_all(&events_dir);
    }

    #[test]
    fn rehydrate_sessions_returns_zero_when_events_dir_missing() {
        let sessions = Sessions::new();
        let n = rehydrate_sessions(std::path::Path::new("/nonexistent/path/mu-u1ld"), &sessions);
        assert_eq!(n, 0);
    }

    #[test]
    fn rehydrate_sessions_skips_non_jsonl_files_and_loose_files_in_events_root() {
        // events_dir has an unrelated file at the top level (not a
        // subdir) and a non-jsonl file inside a daemon dir. Both
        // should be ignored without erroring.
        let events_dir = tempdir("rehydrate-skip");
        std::fs::write(events_dir.join("README.txt"), "not a daemon dir").unwrap();
        let daemon_dir = events_dir.join("daemon-xxx");
        std::fs::create_dir_all(&daemon_dir).unwrap();
        std::fs::write(daemon_dir.join("note.txt"), "not a session log").unwrap();
        write_session_jsonl(
            &events_dir,
            "daemon-xxx",
            "session-keepme",
            "anthropic_api",
            "haiku",
            None,
        );

        let sessions = Sessions::new();
        let n = rehydrate_sessions(&events_dir, &sessions);
        assert_eq!(n, 1);
        assert!(sessions.event_log("session-keepme").is_some());

        let _ = std::fs::remove_dir_all(&events_dir);
    }

    #[test]
    fn resolve_events_dir_falls_back_to_default_when_state_dir_unset() {
        // With persist=true and state_dir=None, we expect the
        // legacy default_events_dir() value — typically
        // ~/.local/share/mu/events. We assert "Some(_)" rather than
        // a specific path because dirs::home_dir() differs across
        // CI environments.
        let config = Config {
            session: SessionConfig {
                persist_events_to_disk: true,
                state_dir: None,
                ..Default::default()
            },
            ..Default::default()
        };
        let got = resolve_events_dir(&config);
        // Only assert Some/None; the exact path depends on $HOME.
        assert!(got.is_some());
        let path = got.unwrap();
        assert!(
            path.ends_with(".local/share/mu/events"),
            "expected default events dir, got {path:?}",
        );
    }
}
