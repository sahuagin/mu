//! `mu serve` mode — JSON-RPC daemon over stdio (or generic
//! reader/writer for tests).

use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncBufRead, AsyncWrite, BufReader};

use mu_core::agent::Tool;

pub mod daemon_info;
pub mod discovery;
mod dispatch;
pub mod factory;
mod forwarder;
mod sessions;

pub use daemon_info::DaemonInfo;
pub use discovery::{FileBackend, LocalRegistryBackend, SessionDiscovery};
pub use factory::{
    build_provider_from_selector, build_tools, make_provider_factory, parse_tools_csv,
    selector_from_cli, BashSettings, ProviderFactory,
};
pub use sessions::Sessions;

/// Default on-disk events directory used by the production binary
/// (mu-upb). `None` means "don't write events to disk." Tests
/// explicitly pass `None` to avoid polluting the developer's
/// `~/.local/share/mu/events/` with test fixtures.
pub fn default_events_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".local/share/mu/events"))
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
pub async fn run(factory: ProviderFactory, tools: Vec<Arc<dyn Tool>>) -> anyhow::Result<()> {
    let config = mu_core::config::Config::load_default();
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
    let daemon_info = DaemonInfo::new(env!("CARGO_PKG_VERSION"))
        .with_events_dir(events_dir)
        .with_config(config);
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
    // Wrap tools in Arc so cloning per request is a single pointer
    // copy regardless of tools list size.
    let tools = Arc::new(tools);
    mu_core::transport::serve(reader, writer, move |req, notif| {
        let sessions = sessions.clone();
        let factory = factory.clone();
        let tools = tools.clone();
        let daemon_info = daemon_info.clone();
        let discovery = discovery.clone();
        async move {
            dispatch::dispatch(req, notif, sessions, factory, tools, daemon_info, discovery).await
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
