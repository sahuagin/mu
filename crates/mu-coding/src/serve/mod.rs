//! `mu serve` mode — JSON-RPC daemon over stdio (or generic
//! reader/writer for tests).

use std::sync::Arc;

use tokio::io::{AsyncBufRead, AsyncWrite, BufReader};

use mu_core::agent::{Provider, Tool};

mod dispatch;
pub mod factory;
mod forwarder;
mod sessions;

pub use factory::{build_provider, build_tools, parse_tools_csv};
pub use sessions::Sessions;

/// Production entry point — serve over the process's stdin/stdout.
pub async fn run(
    provider: Arc<dyn Provider>,
    tools: Vec<Arc<dyn Tool>>,
) -> anyhow::Result<()> {
    let stdin = BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    serve_with_io(stdin, stdout, provider, tools).await
}

/// Test/integration hook — serve over generic reader/writer.
pub async fn serve_with_io<R, W>(
    reader: R,
    writer: W,
    provider: Arc<dyn Provider>,
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
        let provider = provider.clone();
        let tools = tools.clone();
        async move { dispatch::dispatch(req, notif, sessions, provider, tools).await }
    })
    .await
    .map_err(Into::into)
}
