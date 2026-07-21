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
pub mod discovery_bootstrap;
mod dispatch;
pub mod factory;
mod forwarder;
mod handlers;
mod mailbox;
pub mod mcp;
pub mod mcp_client;
mod mesh;
mod mesh_consume;
mod pipeline;
mod presence;
mod provider_status;
mod sessions;
pub(crate) mod worker;

pub use daemon_info::DaemonInfo;
pub use discovery::{FileBackend, LocalRegistryBackend, SessionDiscovery};
pub use factory::{
    build_provider_from_selector, build_tools, make_provider_factory, parse_tools_csv,
    resolve_launch_selection, selector_from_cli, BashSettings, ProviderFactory,
};
#[cfg(test)]
pub(crate) use mailbox::MailboxState;
#[cfg(test)]
pub(crate) use provider_status::ProviderStatusTracker;
pub(crate) use sessions::AutonomyClaimError;
#[cfg(test)]
pub(crate) use sessions::NewSession;
pub use sessions::{Sessions, WeakSessions};

struct AbortOnDrop(std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(h) = self.0.lock().unwrap().take() {
            h.abort();
        }
    }
}

fn mark_mcp_import_unregistered(
    status: &mut [mu_core::protocol::McpServerStatus],
    server_index: usize,
    tool_index: usize,
) {
    if let Some(server) = status.get_mut(server_index) {
        if let Some(tool_status) = server.imported_tools.get_mut(tool_index) {
            tool_status.registered = false;
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

// mu-lazy-session-rehydration-bh4f: the startup bulk-rehydration pass
// (mu-u1ld `rehydrate_sessions`) was removed. It parsed every on-disk
// session log into the registry before `mu serve` was usable — O(thousands
// of full JSONL parses) on a busy box. Rehydration is now request-driven:
// `Sessions::event_log` lazily loads one log by id on first access, and
// `sessions_index` enumerates cheaply (first record + mtime). See
// `serve_with_io_with_config` and `crate::sessions_index`.

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

/// spec mu-046 (WP3): resolve the daemon control-plane journal path.
///
/// `[journal].dir` overrides; otherwise the resolution mirrors
/// [`resolve_events_dir`] — `[session].state_dir` if set, else the
/// platform default — landing at `journal/`, a sibling of `events/`,
/// so the session-log scanners (`sessions_index`,
/// `discovery/file_backend`) never see it.
///
/// Unlike events, the journal has NO opt-out: a daemon that cannot
/// make commands durable does not serve (INV-2). Hermetic tests point
/// `[journal].dir` at a tempdir.
pub fn resolve_journal_path(config: &mu_core::config::Config, daemon_id: &str) -> PathBuf {
    let dir = config
        .journal
        .dir
        .clone()
        .or_else(|| config.session.state_dir.as_ref().map(|s| s.join("journal")))
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share/mu/journal")))
        // No home dir at all (stripped-down container): fall back to a
        // per-user-less location rather than refusing to resolve — the
        // open itself still fails closed if the path is unusable.
        .unwrap_or_else(|| std::env::temp_dir().join("mu-journal"));
    dir.join(format!("{daemon_id}.jsonl"))
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
///
/// mu-mu-bare-flag-fxc8: `bare = true` (the `--bare` CLI flag) forces a
/// hermetic daemon regardless of config file or env: recall providers
/// off AND the discovery bootstrap suppressed, by rewriting the loaded
/// `[recall]` section before anything downstream reads it.
///
/// mu-779s: `max_turns` is the default cap on assistant-message turns
/// for sessions created on this daemon. `None` → use provider-aware
/// default (20 for Anthropic, 35 for OpenAI). `Some(0)` → disable cap.
/// Forwarded as `CreateSessionRequest.max_turns` to session creation.
pub async fn run(
    factory: ProviderFactory,
    tools: Vec<Arc<dyn Tool>>,
    bare: bool,
    max_turns: Option<u32>,
    mcp_enabled_override: Option<bool>,
    bash_settings: BashSettings,
) -> anyhow::Result<()> {
    // spec mu-046 WP6: track config provenance — the file layers that
    // contributed, plus the consumer-side overrides applied right
    // here — so the journaled `ConfigLoaded` records where the
    // effective config came from (INV-9).
    let (mut config, mut config_sources) = mu_core::config::Config::load_default_with_sources();
    if let Ok(token) = std::env::var("MU_BEARER_TOKEN") {
        if !token.is_empty() {
            config.auth = mu_core::config::AuthConfig::Bearer {
                tokens: vec![token],
            };
            config_sources.push("env:MU_BEARER_TOKEN".to_string());
        }
    }
    if bare {
        config.recall.enabled = false;
        config.recall.bare = true;
        config_sources.push("cli:--bare".to_string());
    }
    if let Some(enabled) = mcp_enabled_override {
        config.mcp.enabled = enabled;
        config_sources.push(if enabled {
            "cli:--enable-mcp".to_string()
        } else {
            "cli:--disable-mcp".to_string()
        });
    }
    // mu-779s: if max_turns is Some, override the default. Note that
    // this is the daemon-wide default; per-session overrides still work
    // via CreateSessionRequest.max_turns.
    if let Some(n) = max_turns {
        // Store the daemon default in config for reference (not used yet).
        // The actual per-session default is handled in build_and_register_session.
        tracing::info!(max_turns = ?n, "daemon: max_turns cap set");
    }
    let events_dir = resolve_events_dir(&config);
    let stdin = BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    serve_with_io_with_config_sources(
        stdin,
        stdout,
        factory,
        tools,
        events_dir,
        config,
        config_sources,
        bash_settings,
    )
    .await
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
/// [`run`] loads `Config::load_default()` and calls
/// [`serve_with_io_with_config_sources`]. Tests pass
/// `Config::default()` (or a custom one if they're testing
/// config-driven behavior); the journaled `ConfigLoaded` provenance is
/// then the bare `["defaults"]` — an explicitly-passed config has no
/// file/env/CLI layers to attribute.
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
    serve_with_io_with_config_sources(
        reader,
        writer,
        factory,
        tools,
        events_dir,
        config,
        vec!["defaults".to_string()],
        // mu-qnag: test/default hook — strict bash policy (the safe floor).
        // Production `run` threads the real settings from the `--bash-*`
        // flags; tests that exercise watch's gate construct DaemonInfo
        // directly with the mode they want.
        BashSettings::default(),
    )
    .await
}

/// [`serve_with_io_with_config`] plus config source provenance (spec
/// mu-046 WP6): `config_sources` lists the layers that produced
/// `config` (see [`mu_core::config::Config::load_with_sources`]) and
/// is journaled verbatim in the boot-time `ConfigLoaded` record.
// The arg list is the daemon's full boot bundle (io, factory, tools,
// events_dir, config (+sources), and the bash/command policy — mu-qnag);
// threading a struct here would obscure the one production call site.
#[allow(clippy::too_many_arguments)]
pub async fn serve_with_io_with_config_sources<R, W>(
    reader: R,
    writer: W,
    factory: ProviderFactory,
    tools: Vec<Arc<dyn Tool>>,
    events_dir: Option<PathBuf>,
    config: mu_core::config::Config,
    config_sources: Vec<String>,
    bash_settings: BashSettings,
) -> anyhow::Result<()>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    // mu-lazy-session-rehydration-bh4f: the daemon no longer bulk-loads
    // and parses every on-disk session log at startup (the old mu-u1ld
    // `rehydrate_sessions` pass — O(thousands of full JSONL parses) before
    // `mu serve` was usable). Rehydration is now request-driven:
    //   - find-by-id (`resume` / `recover` / `session.events` /
    //     `session.stats`) lazily loads the one matching log on first
    //     access via `Sessions::event_log` — hence the events dir handed
    //     to the registry here.
    //   - enumeration (the standalone `mu list-sessions`) reads only each
    //     log's first record + mtime (`sessions_index::scan_session_index`).
    let sessions = Sessions::new_with_events_dir(events_dir.clone());
    // mu-7rk (mu-yox): build the connect-time auth registry from
    // `[auth]` config and allocate a fresh per-connection `AuthState`
    // handle. This `serve_with_io_with_config` call corresponds to one
    // connection — stdio in production, one duplex pipe in tests. The
    // handle is freshly allocated here so cross-connection auth state
    // never leaks.
    let auth_registry = Arc::new(auth::registry_from_config(&config.auth));
    // mu-ddua: auth is opt-in — when no configured mechanism actually
    // enforces, connections start pre-authenticated under root so a
    // default `mu serve` is usable out-of-box. The shared posture lives
    // in `auth::initial_connection_state` (the MCP adapter applies the
    // same rule per accepted connection, spec mu-046 WP5).
    let auth_state: auth::AuthStateHandle = Arc::new(std::sync::Mutex::new(
        auth::initial_connection_state(&auth_registry),
    ));
    // mu-phl v0 (mu-0bxv): wire up the canonical session-start recall
    // provider chain. Tests construct DaemonInfo without these (empty
    // vec) to skip recall; production runs the full chain.
    let recall_providers = build_recall_providers(&config);
    // mu-818c: best-effort ollama discovery → route catalog. Only in
    // production (events_dir set AND `[routes].ollama_discover`, default
    // true). The events_dir heuristic alone did NOT keep tests hermetic:
    // disk-backed test daemons tripped it, and on CI runners the
    // baked-in ollama base is an unroutable LAN address, so the probe's
    // bounded connect timeout stalled boot for its full duration —
    // hermetic tests set the knob false. A short timeout bounds
    // startup, so a down/absent ollama box can't stall the daemon; any
    // failure means "no ollama routes", logged at debug. `mu ask`
    // (ephemeral, no events_dir) skips the probe — it resolves ollama
    // via the selector, not the catalog.
    let route_catalog = {
        let mut catalog = mu_core::route_catalog::RouteCatalog::from_env();
        if events_dir.is_some() && config.routes.ollama_discover {
            let base = mu_ai::providers::ollama::base_from_env();
            match mu_ai::OllamaProvider::discover_models(&base, std::time::Duration::from_secs(2))
                .await
            {
                Ok(models) if !models.is_empty() => {
                    tracing::info!(
                        count = models.len(),
                        %base,
                        "ollama: discovered models for route catalog"
                    );
                    catalog = catalog.with_ollama_models(models);
                }
                Ok(_) => tracing::debug!(%base, "ollama: reachable but reported no models"),
                Err(e) => {
                    tracing::debug!(%base, error = %e, "ollama: discovery skipped (not reachable)")
                }
            }
        }
        catalog
    };
    let daemon_info = DaemonInfo::new(env!("CARGO_PKG_VERSION"))
        .with_events_dir(events_dir)
        .with_config(config)
        .with_recall_providers(recall_providers)
        .with_route_catalog(route_catalog)
        // mu-qnag: carry the daemon's command policy so the per-session
        // `watch` tool gates through the SAME BashMode as `bash`.
        .with_bash_settings(bash_settings);
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
    // Optional etcd-lease presence (push-mailbox spec, mu-daemon slice):
    // register this daemon + its live sessions on the dialogue channel under
    // one lease. Production-only (events_dir set - same hermeticity posture
    // as the supervisor mailbox above) AND config-gated: without
    // [dialogue.presence] enabled=true this spawns nothing and touches no
    // network, so a bare mu install needs no etcd.
    // mu-ad5x: the presence task loops forever and holds a Sessions clone,
    // so like the MCP listener below it MUST die with the transport handler
    // (AbortOnDrop captured by the closure) or the shutdown cascade wedges
    // and `mu serve` never exits after stdin closes. The abort is
    // crash-equivalent: the etcd lease expires at TTL and the peer keys
    // vanish with it.
    let presence_guard = if daemon_info.events_dir().is_some() {
        presence::from_config(daemon_info.config().dialogue.as_ref()).map(|pcfg| {
            tracing::info!(etcd = ?pcfg.etcd, "dialogue presence: enabled (etcd lease)");
            let handle =
                presence::spawn(pcfg, daemon_info.daemon_id().to_string(), sessions.clone());
            AbortOnDrop(std::sync::Mutex::new(Some(handle)))
        })
    } else {
        None
    };
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
    // mu-slat: the spawn_worker tool is injected per-session in
    // build_and_register_session (handlers/session.rs), not here — it
    // needs the calling session's id so worker results route back to
    // the right mailbox. A daemon-global instance can't know its caller.
    // mu-yc6: import tools from configured `[[mcp.servers]]` (outbound MCP
    // client). This is how code_recall reaches the in-loop agent — the
    // first-class path to symbol/concept search (the highest-value instance
    // of Friction B, "folkloric capabilities"); without it the agent falls
    // back to token-expensive grep. Best-effort connect at startup, mirroring
    // the recall-provider posture above: an unreachable server simply
    // contributes no tools (graceful degradation, never a startup failure).
    // Once registered they're base session tools, so the mu-onq8 `discover`
    // tool ranks them alongside everything else.
    let mut tools = tools;
    // mu-a0l6: mesh-consumed code_index tools register FIRST — enabling
    // consumption is an explicit operator choice, so it wins name collisions;
    // the MCP import below then warns+skips its duplicates. Best-effort like
    // the import: failure degrades to "no mesh tools", never a boot failure.
    if daemon_info.config().mesh.consume_code_index {
        match mesh_consume::mesh_code_index_tools(&daemon_info.config().mesh).await {
            Ok(mesh_tools) => {
                for tool in mesh_tools {
                    let name = tool.spec().name;
                    if tools.iter().any(|t| t.spec().name == name) {
                        tracing::warn!(tool = %name,
                            "mesh-consumed tool collides with an existing tool; skipping");
                    } else {
                        tools.push(tool);
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e,
                "mesh code_index consumption failed to start; continuing without it"),
        }
    }
    if daemon_info.config().mcp.enabled {
        let mut imported = mcp_client::import_remote_tools(&daemon_info.config().mcp.servers).await;
        for imported_tool in imported.tools {
            let name = imported_tool.tool.spec().name;
            if tools.iter().any(|t| t.spec().name == name) {
                tracing::warn!(
                    tool = %name,
                    "MCP-imported tool collides with an existing tool; skipping"
                );
                mark_mcp_import_unregistered(
                    &mut imported.status,
                    imported_tool.status_server_index,
                    imported_tool.status_tool_index,
                );
            } else {
                tools.push(imported_tool.tool);
            }
        }
        daemon_info.set_mcp_status(imported.status);
    } else {
        tracing::info!("MCP disabled; skipping outbound MCP imports");
        daemon_info.set_mcp_status(Vec::new());
    }
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
    // spec mu-046 INV-1/INV-2 (WP3): open the control-plane command
    // journal BEFORE accepting any traffic. Open failure ABORTS serve —
    // a daemon that cannot make commands durable does not serve.
    let journal_path = resolve_journal_path(daemon_info.config(), daemon_info.daemon_id());
    let journal = Arc::new(
        mu_core::command_journal::CommandJournal::open(
            &journal_path,
            daemon_info.daemon_id(),
            daemon_info.config().journal.fsync_policy(),
        )
        .map_err(|e| {
            anyhow::anyhow!(
                "cannot open command journal at {}: {e} (refusing to serve, spec mu-046 INV-2)",
                journal_path.display()
            )
        })?,
    );
    // spec mu-046 INV-8 (WP2) + INV-11 (WP9): the daemon-wide outbound
    // Router — the one way bytes leave the daemon. Each connection
    // registers its own ordered egress lane (the stdio connection's
    // inside serve_with_ingest; MCP connections at accept); tagged
    // envelopes route to their origin's lane, broadcasts to all. The
    // pipeline consumer holds a producer clone and drops it on
    // shutdown, closing the lanes.
    let outbound = mu_core::transport::Router::new();
    // spec mu-046 INV-3/INV-7 (WP3): the single-writer control-plane
    // consumer. It owns the daemon's session map et al. and exits —
    // releasing them, continuing the shutdown cascade — when the last
    // producer handle (held by the closure below) drops.
    let control = Arc::new(pipeline::spawn_control_plane(
        journal,
        pipeline::PipelineCtx {
            sessions: sessions.clone(),
            factory,
            tools,
            skills,
            daemon_info: daemon_info.clone(),
            discovery,
            auth_registry: auth_registry.clone(),
        },
        outbound.clone(),
    ));
    // spec mu-046 INV-9 (WP6): config is a message. The resolved
    // effective config — redacted (INV-6) — enters the control plane
    // as a journaled, sequenced `ConfigLoaded` HERE, after the journal
    // opened (so it is record 2, behind open()'s `JournalOpened`) and
    // strictly BEFORE any adapter exists (the MCP listener below, the
    // stdio read loop at the bottom): every adapter command's seq is
    // therefore greater than the config's. Append failure aborts
    // serve — boot-time fail-closed, same posture as journal open
    // failure (INV-2).
    {
        let mut effective = serde_json::to_value(daemon_info.config())
            .map_err(|e| anyhow::anyhow!("cannot serialize effective config: {e}"))?;
        mu_core::config::redact_config(&mut effective);
        control
            .inject_config_loaded(config_sources, effective)
            .map_err(|e| {
                anyhow::anyhow!(
                    "cannot journal ConfigLoaded: {e} (refusing to serve, spec mu-046 INV-2/INV-9)"
                )
            })?;
    }
    // mu-wxc4: NATS mesh adapter (adapter #3, INV-7). Config-gated: with
    // `[mesh] enabled=true` the daemon also serves its JSON-RPC surface over
    // the mesh via serve/mesh.rs — inbound crosses `ingest` (journaled),
    // outbound rides an outbound Router lane. Spawned AFTER the control plane
    // and outbound Router exist, like the MCP adapter below. The returned
    // handle aborts its tasks on drop, so capturing it in the transport
    // closure ties it to daemon shutdown (mu-ad5x lifetime contract).
    let mesh_guard = {
        let mesh_cfg = &daemon_info.config().mesh;
        // FAIL CLOSED (mu-iqo8): the mesh adapter serves ONE auth state for all
        // peers multiplexed on its subject, so per-connection auth cannot
        // isolate them — once any peer authenticates, every peer on the subject
        // is authorized. Rather than ship that bypass behind a doc, refuse to
        // expose the mesh when a peer-auth handshake is configured. The mesh's
        // real multi-peer auth is per-request biscuit capabilities (mesh-slice)
        // and lands with mu-iqo8; until then it runs ONLY where the daemon is
        // already pre-authenticated (no auth mechanism enforcing — e.g. a
        // single-operator / trusted-network deployment).
        let auth_required = matches!(
            auth::initial_connection_state(&auth_registry),
            auth::AuthState::Unauthenticated
        );
        if mesh_cfg.enabled && auth_required {
            tracing::error!(
                "[mesh].enabled but an auth mechanism requires a per-connection handshake; \
                 the mesh multiplexes peers on one subject and cannot yet isolate their auth \
                 (mu-iqo8) — refusing to expose protected commands over the mesh. Disable \
                 [mesh] or [auth], or wait for per-request capabilities."
            );
            None
        } else if mesh_cfg.enabled {
            let subject = if mesh_cfg.subject.is_empty() {
                format!("mu.daemon.{}.rpc", daemon_info.daemon_id())
            } else {
                mesh_cfg.subject.clone()
            };
            // Pre-authenticated (no enforcing mechanism): the mesh gets its own
            // connection auth state via `initial_connection_state`, the same
            // per-connection posture the MCP adapter applies (mu-046 WP5).
            let mesh_auth_state: auth::AuthStateHandle = Arc::new(std::sync::Mutex::new(
                auth::initial_connection_state(&auth_registry),
            ));
            match mesh::spawn_mesh_adapter(
                &mesh_cfg.nats_url,
                &subject,
                control.clone(),
                mesh_auth_state,
                outbound.clone(),
            )
            .await
            {
                Ok(handle) => Some(handle),
                Err(e) => {
                    tracing::warn!(error = %e,
                        "mesh adapter failed to start; daemon serves without the mesh");
                    None
                }
            }
        } else {
            None
        }
    };
    // mu-mb02: start MCP server on a unix socket if MU_MCP_SOCKET is
    // set or if the default socket path's parent exists. The MCP surface
    // shares Sessions + DaemonInfo with the primary JSON-RPC loop so
    // mailbox operations are consistent across both surfaces — and
    // since spec mu-046 WP5 it is adapter #2 (INV-7): it holds a
    // ControlPlane producer handle and the outbound stream, so it must
    // be spawned AFTER the control plane exists (hence this block sits
    // below `spawn_control_plane`, later in startup than pre-WP5).
    //
    // The MCP task holds Sessions + ControlPlane clones (the latter is
    // a pipeline producer handle). transport::serve's shutdown cascade
    // (drop handler → drop the control-plane sender → consumer exits →
    // sessions release → agent loops exit → NotificationWriters drop →
    // writer_task completes) deadlocks if any external clone keeps
    // those alive. AbortOnDrop is captured by the handler closure so
    // that dropping the handler aborts the MCP listener task first,
    // releasing its clones. (Per-connection MCP tasks keep their own
    // clones until the peer disconnects — pre-existing posture, now
    // also covering the ControlPlane handle.)
    let mcp_socket_path = std::env::var("MU_MCP_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|_| mcp::default_mcp_socket_path());
    let mcp_guard = if daemon_info.config().mcp.enabled
        && mcp_socket_path
            .parent()
            .map(|p| p.exists())
            .unwrap_or(false)
    {
        let mcp_sessions = sessions.clone();
        let mcp_daemon_info = daemon_info.clone();
        let mcp_control = control.clone();
        let mcp_outbound = outbound.clone();
        let mcp_auth_registry = auth_registry.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = mcp::serve_mcp_socket(
                mcp_socket_path,
                mcp_sessions,
                mcp_daemon_info,
                mcp_control,
                mcp_outbound,
                mcp_auth_registry,
            )
            .await
            {
                tracing::error!("MCP server exited: {e:#}");
            }
        });
        Some(AbortOnDrop(std::sync::Mutex::new(Some(handle))))
    } else {
        None
    };
    // The local Sessions handle must not outlive the transport closure:
    // a clone held across the final await below would keep every
    // SessionState alive and wedge the shutdown cascade documented in
    // transport::serve_with_ingest (writer_task would never observe
    // Closed). The pipeline consumer and the MCP guard own their own
    // clones with their own release paths.
    drop(sessions);
    // The stdio transport is adapter #1 (INV-7, no side doors): every
    // parsed request crosses pipeline::ingest — journaled before
    // anything processes it. `Some` = immediate reject the transport
    // sends; `None` = the pipeline owns the response.
    mu_core::transport::serve_with_ingest(reader, writer, outbound, move |req, _notif, origin| {
        let _ = &mcp_guard;
        // mu-ad5x: same lifetime contract as mcp_guard — dropping the
        // handler aborts the presence task, releasing its Sessions clone.
        let _ = &presence_guard;
        // mu-wxc4: same lifetime contract — dropping the handler aborts the
        // mesh adapter tasks, releasing their ControlPlane/Router clones.
        let _ = &mesh_guard;
        let control = control.clone();
        let auth_state = auth_state.clone();
        async move { pipeline::ingest(&control, req, origin, &auth_state) }
    })
    .await
    .map_err(Into::into)
}

/// Build the ordered session-start recall provider chain from config.
///
/// mu recall dial: `[recall].enabled = false` (or env `MU_NO_RECALL`) turns
/// off session-start context injection entirely — the agent discovers on
/// demand, the analog of claude-basic's `CLAUDE_BASIC_MEM`. Default keeps
/// the providers wired (existing behavior).
///
/// mu-recall-bootloader-flag-nxpo: when recall is enabled AND the bootloader
/// is on (`[recall].bootloader` or `MU_RECALL_BOOTLOADER=1`), a
/// [`BootloaderRecallProvider`] is pushed to the FRONT of the chain, so its
/// preamble is the first recall segment ahead of the memory + project-file
/// providers. With the bootloader off the chain is byte-identical to today
/// (the experiment's A condition); `--bare` forces `recall_enabled()` false,
/// so it suppresses the bootloader along with everything else.
fn build_recall_providers(
    config: &mu_core::config::Config,
) -> Vec<Arc<dyn mu_core::context::RecallProvider>> {
    use mu_core::context::recall::{
        BootloaderRecallProvider, ProjectFileRecallProvider, SubprocessRecallProvider,
    };

    if !config.recall_enabled() {
        tracing::info!(
            "recall disabled ([recall].enabled=false or MU_NO_RECALL) — \
             discover-on-demand mode; no session-start context injection"
        );
        return Vec::new();
    }

    let mut providers: Vec<Arc<dyn mu_core::context::RecallProvider>> = Vec::new();
    if config.bootloader_enabled() {
        // FIRST segment, before all providers: orientation-about-the-
        // orientation precedes the orientation.
        providers.push(Arc::new(BootloaderRecallProvider::new(
            config.bootloader_text().to_string(),
        )));
    }
    // mu-recall-operator-controls-5y6a: the agent-memory provider is now
    // independently toggleable (`[recall].memory`, default true) and its
    // binary is configurable (`[recall].memory_binary`, default
    // ~/.local/bin/agent). Skipping it leaves the project-file provider
    // (MU.md/AGENTS.md) below intact — file-only context with no
    // agent.sqlite injection — without disabling all recall.
    if config.recall.memory {
        // mu-zk2i: tier from `[recall].tier` — "identity" (default) injects
        // the small kernel; "full" restores the four-section wall.
        let memory_provider = match &config.recall.memory_binary {
            Some(path) => SubprocessRecallProvider::with_binary(path.clone()),
            None => SubprocessRecallProvider::default(),
        };
        providers.push(Arc::new(memory_provider.with_tier(&config.recall.tier)));
    }
    providers.push(Arc::new(ProjectFileRecallProvider::default()));
    providers
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_core::config::{Config, SessionConfig};

    #[test]
    fn mark_mcp_import_unregistered_targets_one_import_not_same_named_tools() {
        use mu_core::agent::{PermissionLevel, SideEffects};
        use mu_core::protocol::{McpImportedToolStatus, McpServerConnectionState, McpServerStatus};

        fn server(name: &str) -> McpServerStatus {
            McpServerStatus {
                name: name.to_string(),
                url: format!("http://{name}/mcp"),
                configured_tools: None,
                prefix: None,
                side_effects: Some(SideEffects::ReadOnly),
                tool_side_effects: std::collections::HashMap::new(),
                state: McpServerConnectionState::Connected,
                imported_tools: vec![McpImportedToolStatus {
                    remote_name: "same".to_string(),
                    local_name: "same".to_string(),
                    side_effects: SideEffects::ReadOnly,
                    permission: PermissionLevel::Allow,
                    classified: true,
                    registered: true,
                }],
                last_error: None,
                elapsed_ms: Some(1),
            }
        }

        let mut status = vec![server("first"), server("second")];
        mark_mcp_import_unregistered(&mut status, 1, 0);

        assert!(
            status[0].imported_tools[0].registered,
            "same local_name on an earlier registered server must stay registered"
        );
        assert!(
            !status[1].imported_tools[0].registered,
            "only the colliding import that was skipped should be marked skipped"
        );
    }

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

    #[test]
    fn resolve_journal_path_prefers_journal_dir_override() {
        let config = Config {
            journal: mu_core::config::JournalConfig {
                dir: Some(PathBuf::from("/tmp/mu-j")),
                ..Default::default()
            },
            session: SessionConfig {
                state_dir: Some(PathBuf::from("/var/lib/mu")),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            resolve_journal_path(&config, "d1"),
            PathBuf::from("/tmp/mu-j/d1.jsonl")
        );
    }

    #[test]
    fn resolve_journal_path_uses_state_dir_journal_sibling() {
        let config = Config {
            session: SessionConfig {
                state_dir: Some(PathBuf::from("/var/lib/mu")),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            resolve_journal_path(&config, "d1"),
            PathBuf::from("/var/lib/mu/journal/d1.jsonl")
        );
    }

    #[test]
    fn resolve_journal_path_defaults_next_to_events() {
        // No override, no state_dir: the platform default — a sibling
        // of events/ so the session-log scanners never see it. Unlike
        // events there is no opt-out: persist_events_to_disk=false
        // does NOT turn the journal off (spec mu-046 INV-2).
        let config = Config {
            session: SessionConfig {
                persist_events_to_disk: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let path = resolve_journal_path(&config, "d1");
        assert!(
            path.ends_with(".local/share/mu/journal/d1.jsonl")
                || path.ends_with("mu-journal/d1.jsonl"),
            "expected default journal path, got {path:?}",
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
                usage_semantics: None,
            },
        );
    }

    #[test]
    fn event_log_lazily_loads_past_session_from_disk() {
        // mu-lazy-session-rehydration-bh4f: no startup rehydration. A
        // past session is found-by-id and parsed on FIRST access, then
        // cached as a read-only ghost — replacing the old bulk
        // `rehydrate_sessions` pass.
        let events_dir = tempdir("lazy-load");
        write_session_jsonl(
            &events_dir,
            "daemon-aaa",
            "session-prev-1",
            "anthropic_api",
            "haiku",
            Some("session-parent-0"),
        );

        let sessions = Sessions::new_with_events_dir(Some(events_dir.clone()));
        // Nothing loaded up front — startup does no parsing.
        assert!(sessions.snapshot_for_listing().is_empty());

        // First access loads it from disk.
        let log = sessions.event_log("session-prev-1").expect("lazy load");
        assert_eq!(log.session_id(), "session-prev-1");

        // Now cached: visible in the in-memory listing (with the parent
        // ref carried from SessionCreated), and a second lookup returns
        // the SAME Arc rather than re-reading the file.
        let listing = sessions.snapshot_for_listing();
        assert_eq!(listing.len(), 1);
        assert_eq!(listing[0].2, Some("session-parent-0".to_string()));
        let again = sessions.event_log("session-prev-1").expect("cached");
        assert!(Arc::ptr_eq(&log, &again), "second lookup hits the cache");

        // Live-state queries stay None — it's a read-only ghost.
        assert!(sessions.input_sender("session-prev-1").is_none());

        let _ = std::fs::remove_dir_all(&events_dir);
    }

    #[test]
    fn event_log_unknown_id_returns_none_even_with_events_dir() {
        let events_dir = tempdir("lazy-miss");
        write_session_jsonl(
            &events_dir,
            "daemon-aaa",
            "session-real",
            "anthropic_api",
            "haiku",
            None,
        );
        let sessions = Sessions::new_with_events_dir(Some(events_dir.clone()));
        assert!(sessions.event_log("session-does-not-exist").is_none());
        let _ = std::fs::remove_dir_all(&events_dir);
    }

    #[test]
    fn event_log_no_events_dir_means_no_disk_fallback() {
        // Tests / ephemeral daemons construct Sessions without an events
        // dir; the lazy fallback is a no-op (no disk touched).
        let sessions = Sessions::new();
        assert!(sessions.event_log("anything").is_none());
    }

    #[test]
    fn event_log_in_memory_does_not_lazy_load() {
        // gpt-5.5 review: mutating callers (session.close) use the
        // in-memory lookup, which must NOT resurrect a past on-disk
        // session — otherwise close would append a no-op SessionClosed to
        // a ghost and report closed=true for something never live.
        let events_dir = tempdir("in-memory-no-disk");
        write_session_jsonl(
            &events_dir,
            "daemon-a",
            "session-disk",
            "anthropic_api",
            "haiku",
            None,
        );
        let sessions = Sessions::new_with_events_dir(Some(events_dir.clone()));

        // In-memory lookup never touches disk → miss for an uncached log.
        assert!(sessions.event_log_in_memory("session-disk").is_none());
        // The lazy accessor does find + cache it.
        assert!(sessions.event_log("session-disk").is_some());
        // Now cached, the in-memory lookup sees it.
        assert!(sessions.event_log_in_memory("session-disk").is_some());

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

    // mu-recall-bootloader-flag-nxpo: serialize the env-sensitive recall
    // tests — they all read the process-global `MU_RECALL_BOOTLOADER`, so
    // running them concurrently would let one test's set_var leak into
    // another's read. Lock + drop the guard around every such test.
    static BOOTLOADER_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_bootloader_env() -> std::sync::MutexGuard<'static, ()> {
        BOOTLOADER_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Run `f` with `MU_RECALL_BOOTLOADER` unset, restoring any prior value.
    fn with_bootloader_env_clear<T>(f: impl FnOnce() -> T) -> T {
        let _guard = lock_bootloader_env();
        let prev = std::env::var("MU_RECALL_BOOTLOADER").ok();
        std::env::remove_var("MU_RECALL_BOOTLOADER");
        let out = f();
        match prev {
            Some(v) => std::env::set_var("MU_RECALL_BOOTLOADER", v),
            None => std::env::remove_var("MU_RECALL_BOOTLOADER"),
        }
        out
    }

    // Whether `providers[0]` is the bootloader provider, by its `Debug`
    // type name — hermetic (no fs/subprocess), unlike running `recall()`.
    fn first_is_bootloader(providers: &[Arc<dyn mu_core::context::RecallProvider>]) -> bool {
        providers
            .first()
            .map(|p| format!("{p:?}").starts_with("BootloaderRecallProvider"))
            .unwrap_or(false)
    }

    #[test]
    fn build_recall_providers_default_omits_bootloader() {
        // Flag-off identity: default config => the exact pre-bootloader
        // chain (subprocess + project-file), bootloader NOT present.
        with_bootloader_env_clear(|| {
            let config = Config::default();
            assert!(!config.recall.bootloader);
            let providers = build_recall_providers(&config);
            assert_eq!(
                providers.len(),
                2,
                "default chain is subprocess + project-file"
            );
            assert!(!first_is_bootloader(&providers));
        });
    }

    #[test]
    fn build_recall_providers_memory_false_omits_memory_keeps_project_file() {
        // mu-recall-operator-controls-5y6a: [recall].memory=false drops the
        // agent-memory provider but KEEPS the project-file provider — file-only
        // context (MU.md/AGENTS.md) with no agent.sqlite injection, without
        // disabling all recall.
        with_bootloader_env_clear(|| {
            let mut config = Config::default();
            config.recall.memory = false;
            let providers = build_recall_providers(&config);
            assert_eq!(providers.len(), 1, "only the project-file provider remains");
            let debug = format!("{:?}", providers[0]);
            assert!(
                debug.starts_with("ProjectFileRecallProvider"),
                "sole provider should be project-file, got: {debug}"
            );
            assert!(
                !providers
                    .iter()
                    .any(|p| format!("{p:?}").starts_with("SubprocessRecallProvider")),
                "memory provider must be absent when [recall].memory=false"
            );
        });
    }

    #[test]
    fn build_recall_providers_memory_binary_reaches_provider() {
        // mu-recall-operator-controls-5y6a: [recall].memory_binary points the
        // agent-memory provider at a custom binary instead of the hardcoded
        // ~/.local/bin/agent default.
        with_bootloader_env_clear(|| {
            let mut config = Config::default();
            config.recall.memory_binary = Some(std::path::PathBuf::from("/custom/agent-bin"));
            let providers = build_recall_providers(&config);
            let memory_debug = providers
                .iter()
                .map(|p| format!("{p:?}"))
                .find(|d| d.starts_with("SubprocessRecallProvider"))
                .expect("memory provider present by default");
            assert!(
                memory_debug.contains("/custom/agent-bin"),
                "custom memory_binary should reach the provider, got: {memory_debug}"
            );
        });
    }

    #[test]
    fn build_recall_providers_flag_on_puts_bootloader_first() {
        with_bootloader_env_clear(|| {
            let mut config = Config::default();
            config.recall.bootloader = true;
            let providers = build_recall_providers(&config);
            assert_eq!(providers.len(), 3, "bootloader prepended to the chain");
            assert!(first_is_bootloader(&providers));
            // Terrain check: the first provider actually emits a Bootloader
            // item carrying the resolved (default) preamble text.
            let items = providers[0].recall(
                std::path::Path::new("/tmp"),
                &mu_core::capability::Capability::root(),
            );
            assert_eq!(items.len(), 1);
            assert!(matches!(
                items[0].source,
                mu_core::context::RecallSource::Bootloader
            ));
            assert_eq!(&*items[0].content, config.bootloader_text());
        });
    }

    #[test]
    fn build_recall_providers_env_override_beats_config_both_ways() {
        let _guard = lock_bootloader_env();
        // MU_RECALL_BOOTLOADER=1 forces the bootloader ON even though config
        // has it off (the experiment default).
        std::env::set_var("MU_RECALL_BOOTLOADER", "1");
        let mut off = Config::default();
        off.recall.bootloader = false;
        assert!(first_is_bootloader(&build_recall_providers(&off)));

        // MU_RECALL_BOOTLOADER=0 forces it OFF even though config has it on.
        std::env::set_var("MU_RECALL_BOOTLOADER", "0");
        let mut on = Config::default();
        on.recall.bootloader = true;
        let providers = build_recall_providers(&on);
        assert!(!first_is_bootloader(&providers));
        assert_eq!(providers.len(), 2);

        std::env::remove_var("MU_RECALL_BOOTLOADER");
    }

    #[test]
    fn build_recall_providers_bare_suppresses_bootloader() {
        // Bare suppresses everything incl. the bootloader: bare forces
        // recall_enabled() false, so the chain is empty even with the
        // bootloader flag (and env override) on.
        let _guard = lock_bootloader_env();
        std::env::set_var("MU_RECALL_BOOTLOADER", "1");
        let mut config = Config::default();
        config.recall.bare = true;
        config.recall.enabled = false; // bare implies recall off (mu ask/serve --bare)
        config.recall.bootloader = true;
        assert!(!config.recall_enabled());
        let providers = build_recall_providers(&config);
        assert!(providers.is_empty(), "bare => no providers at all");
        std::env::remove_var("MU_RECALL_BOOTLOADER");
    }
}
