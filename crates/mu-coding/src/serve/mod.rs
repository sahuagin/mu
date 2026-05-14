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

/// Production entry point — serve over the process's stdin/stdout.
///
/// `factory` is called once per session, given the client's
/// `create_session.provider` selector, to construct a fresh
/// `Arc<dyn Provider>`. Multiple sessions on the same daemon can use
/// different providers.
pub async fn run(factory: ProviderFactory, tools: Vec<Arc<dyn Tool>>) -> anyhow::Result<()> {
    let stdin = BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    // Production: events go to the default ~/.local/share/mu/events
    // directory. Override via the CLI in the future once we have a
    // `--events-dir` flag.
    serve_with_io(stdin, stdout, factory, tools, default_events_dir()).await
}

/// Test/integration hook — serve over generic reader/writer.
///
/// `events_dir` controls on-disk event log persistence (mu-upb).
/// Tests should pass `None` to avoid writing fixtures into the
/// developer's home directory; production passes
/// `default_events_dir()`.
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
    let sessions = Sessions::new();
    let daemon_info = DaemonInfo::new(env!("CARGO_PKG_VERSION")).with_events_dir(events_dir);
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
