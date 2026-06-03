//! Outbound MCP client (mu-yc6): import tools from external MCP servers.
//!
//! At daemon startup the serve loop calls [`import_remote_tools`] with the
//! `[[mcp.servers]]` config entries. For each server we open an rmcp
//! Streamable HTTP transport, run the `initialize` handshake, `tools/list`
//! the surface, and wrap every (allowlisted) remote tool in a
//! [`RemoteMcpTool`] — an `Arc<dyn Tool>` indistinguishable from a built-in
//! to the agent loop. One long-lived connection per server, shared by all
//! sessions (the shared-service tier: the server handles concurrency, e.g.
//! code-index serving many clients off one DB set).
//!
//! Best-effort by design, mirroring the daemon's other optional
//! integrations: an unreachable server logs a warning and contributes zero
//! tools; the daemon never fails to start over a missing sidecar.

mod remote_tool;
pub use remote_tool::RemoteMcpTool;

use std::sync::Arc;

use mu_core::agent::Tool;
use mu_core::config::McpServerConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::ServiceExt;

/// The long-lived client handle for one remote server. `()` is the trivial
/// rmcp `ClientHandler` — we consume server-offered tools and need none of
/// the client-offered features (sampling/roots/elicitation) yet.
pub(crate) type McpPeer = rmcp::service::RunningService<rmcp::service::RoleClient, ()>;

/// Connect to every configured server and return the imported tools.
/// Failures are per-server and non-fatal.
pub async fn import_remote_tools(servers: &[McpServerConfig]) -> Vec<Arc<dyn Tool>> {
    /// Per-server budget for connect + initialize + tools/list. A hung or
    /// unresponsive server must degrade like an unreachable one — it cannot
    /// be allowed to stall daemon startup.
    const IMPORT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    let mut out: Vec<Arc<dyn Tool>> = Vec::new();
    for cfg in servers {
        let imported = tokio::time::timeout(IMPORT_TIMEOUT, import_from_server(cfg))
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out after {IMPORT_TIMEOUT:?}")));
        match imported {
            Ok(tools) => {
                tracing::info!(
                    server = %cfg.name,
                    url = %cfg.url,
                    count = tools.len(),
                    "imported MCP tools"
                );
                out.extend(tools);
            }
            Err(e) => {
                tracing::warn!(
                    server = %cfg.name,
                    url = %cfg.url,
                    error = %e,
                    "MCP server unreachable; no tools imported from it"
                );
            }
        }
    }
    out
}

/// Handshake with one server and wrap its (allowlisted) tools.
async fn import_from_server(cfg: &McpServerConfig) -> anyhow::Result<Vec<Arc<dyn Tool>>> {
    let transport = StreamableHttpClientTransport::from_uri(cfg.url.as_str());
    // `initialize` handshake; the returned service owns the connection and
    // is shared (Arc) by every RemoteMcpTool imported from this server.
    let peer: Arc<McpPeer> = Arc::new(().serve(transport).await?);
    let remote = peer.list_all_tools().await?;
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    for def in remote {
        if let Some(allow) = &cfg.tools {
            if !allow.iter().any(|a| a == def.name.as_ref()) {
                continue;
            }
        }
        tools.push(Arc::new(RemoteMcpTool::new(
            &cfg.name,
            cfg.prefix.as_deref(),
            &def,
            peer.clone(),
        )));
    }
    Ok(tools)
}
