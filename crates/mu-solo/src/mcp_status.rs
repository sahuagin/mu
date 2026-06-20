//! Async MCP client for session status subscriptions.
//!
//! Connects to mu-serve's MCP Unix socket via rmcp SDK, subscribes to
//! a session's status resource, and forwards `mu/session_status`
//! custom notifications through a `tokio::sync::mpsc` channel.

use mu_core::session_status::SessionStatus;
use rmcp::model::*;
use rmcp::service::{NotificationContext, RoleClient};
use rmcp::{ClientHandler, ServiceExt};
use tokio::sync::mpsc;
use tracing::debug;

struct StatusHandler {
    tx: mpsc::UnboundedSender<SessionStatus>,
}

impl ClientHandler for StatusHandler {
    fn on_custom_notification(
        &self,
        notification: CustomNotification,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let method = notification.method.clone();
        let status = if method == "mu/session_status" {
            // Surface the outcome instead of silently swallowing it (was
            // `.ok().flatten()`). A deserialize failure here is the exact
            // silent drop that leaves the status meter blank: one field that
            // doesn't round-trip nukes the WHOLE status and the meter stays
            // empty with no signal. The warn survives `release`
            // (release_max_level_info keeps warn+); the success/empty lines
            // are debug! — run `debugrelease` + RUST_LOG=mu_solo=debug.
            match notification.params_as::<SessionStatus>() {
                Ok(Some(s)) => {
                    debug!(
                        provider_kind = %s.provider_kind,
                        model = %s.model,
                        context_soft_limit = ?s.context_soft_limit,
                        context_hard_limit = ?s.context_hard_limit,
                        context_used_tokens = ?s.context_used_tokens,
                        "mu/session_status applied"
                    );
                    Some(s)
                }
                Ok(None) => {
                    debug!("mu/session_status notification carried no params");
                    None
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "mu/session_status DESERIALIZE FAILED — status dropped; \
                         the meter will stay blank (a field did not round-trip)"
                    );
                    None
                }
            }
        } else {
            None
        };
        async move {
            if let Some(status) = status {
                let _ = self.tx.send(status);
            }
        }
    }

    fn get_info(&self) -> ClientInfo {
        ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("mu-solo", env!("CARGO_PKG_VERSION")),
        )
    }
}

fn mcp_socket_path() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("MU_STATE_DIR") {
        return std::path::PathBuf::from(dir).join("mcp.sock");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(home)
        .join(".local/share/mu")
        .join("mcp.sock")
}

/// Spawn an async task that connects to the MCP socket, subscribes to
/// the given session's status, and forwards SessionStatus updates
/// through the returned receiver.
///
/// Best-effort: if the socket doesn't exist or the connection fails,
/// the receiver will simply never produce values. The TUI falls back
/// to its inline accumulation.
pub fn spawn_status_subscriber(session_id: String) -> mpsc::UnboundedReceiver<SessionStatus> {
    let (tx, rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        if let Err(e) = run_subscriber(&session_id, tx).await {
            debug!("MCP status subscriber exited: {e:#}");
        }
    });

    rx
}

async fn run_subscriber(
    session_id: &str,
    tx: mpsc::UnboundedSender<SessionStatus>,
) -> anyhow::Result<()> {
    let sock_path = mcp_socket_path();

    let stream = {
        let mut attempts = 0;
        loop {
            match tokio::net::UnixStream::connect(&sock_path).await {
                Ok(s) => break s,
                Err(e) => {
                    attempts += 1;
                    if attempts >= 10 {
                        anyhow::bail!("connect to {}: {e}", sock_path.display());
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }
    };

    let (reader, writer) = stream.into_split();
    let handler = StatusHandler { tx };
    let running = handler.serve((reader, writer)).await?;

    running
        .subscribe(SubscribeRequestParams::new(format!(
            "mu://session/{session_id}/status"
        )))
        .await?;

    debug!(session_id, "MCP status subscription active");

    let _ = running.waiting().await;
    Ok(())
}
