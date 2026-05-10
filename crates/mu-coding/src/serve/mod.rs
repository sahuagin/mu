//! `mu serve` mode — JSON-RPC daemon over stdio (or generic
//! reader/writer for tests).

use std::sync::Arc;

use tokio::io::{AsyncBufRead, AsyncWrite, BufReader};

use mu_core::agent::Tool;

mod dispatch;
pub mod factory;
mod forwarder;
mod sessions;

pub use factory::{
    build_provider_from_selector, build_tools, make_provider_factory, parse_tools_csv,
    selector_from_cli, ProviderFactory,
};
pub use sessions::Sessions;

/// Production entry point — serve over the process's stdin/stdout.
///
/// `factory` is called once per session, given the client's
/// `create_session.provider` selector, to construct a fresh
/// `Arc<dyn Provider>`. Multiple sessions on the same daemon can use
/// different providers.
pub async fn run(
    factory: ProviderFactory,
    tools: Vec<Arc<dyn Tool>>,
) -> anyhow::Result<()> {
    let stdin = BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    serve_with_io(stdin, stdout, factory, tools).await
}

/// Test/integration hook — serve over generic reader/writer.
pub async fn serve_with_io<R, W>(
    reader: R,
    writer: W,
    factory: ProviderFactory,
    tools: Vec<Arc<dyn Tool>>,
) -> anyhow::Result<()>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let sessions = Sessions::new();
    // Wrap tools in Arc so cloning per request is a single pointer
    // copy regardless of tools list size.
    let tools = Arc::new(tools);
    mu_core::transport::serve(reader, writer, move |req, notif| {
        let sessions = sessions.clone();
        let factory = factory.clone();
        let tools = tools.clone();
        async move { dispatch::dispatch(req, notif, sessions, factory, tools).await }
    })
    .await
    .map_err(Into::into)
}
