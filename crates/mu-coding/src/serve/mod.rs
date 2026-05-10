//! `mu serve` mode — JSON-RPC daemon over stdio (or generic
//! reader/writer for tests).

use std::sync::Arc;

use tokio::io::{AsyncBufRead, AsyncWrite, BufReader};

use mu_core::agent::Provider;

mod dispatch;
mod forwarder;
mod sessions;

pub use sessions::Sessions;

/// Production entry point — serve over the process's stdin/stdout.
pub async fn run(provider: Arc<dyn Provider>) -> anyhow::Result<()> {
    let stdin = BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    serve_with_io(stdin, stdout, provider).await
}

/// Test/integration hook — serve over generic reader/writer.
pub async fn serve_with_io<R, W>(
    reader: R,
    writer: W,
    provider: Arc<dyn Provider>,
) -> anyhow::Result<()>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let sessions = Sessions::new();
    mu_core::transport::serve(reader, writer, move |req, notif| {
        let sessions = sessions.clone();
        let provider = provider.clone();
        async move { dispatch::dispatch(req, notif, sessions, provider).await }
    })
    .await
    .map_err(Into::into)
}
