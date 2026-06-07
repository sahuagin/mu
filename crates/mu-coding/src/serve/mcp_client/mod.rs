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

use mu_core::agent::{SideEffects, Tool};
use mu_core::config::McpServerConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::ServiceExt;

/// mu-cvm5 (mu-n25a Phase 4): resolve the side-effects class for one imported
/// MCP tool, fail-safe. MCP carries no side-effects metadata, so there is no
/// honest source the runtime can trust. Precedence:
///   1. per-tool override (`tool_side_effects[remote_name]`),
///   2. server-wide operator floor (`side_effects`),
///   3. the fail-safe default `Execute` — the MOST restrictive class.
///
/// An UNCLASSIFIED tool (no operator config) therefore imports as `Execute`
/// and is refused by any session with a `max_side_effects` ceiling below
/// `Execute` (e.g. a read-only operator posture). Classifying a server is a
/// deliberate, auditable operator act — never something the remote asserts.
///
/// The second tuple element is `classified`: whether an operator
/// classification existed (per-tool or server floor) or the fail-safe fired.
/// The importer uses it to pick the permission level — a classified tool is
/// the operator's trust statement and runs `Allow`; an unclassified one
/// keeps `Ask` as a second gate behind the side-effects ceiling. Without
/// this, the Phase-5 fail-closed `ToolPolicy::default()` (`Ask`) would make
/// every MCP call prompt — and wedge headless serve sessions, which have no
/// approver (observed live: serve_smoke's MCP import test hung forever).
fn resolve_side_effects(cfg: &McpServerConfig, remote_name: &str) -> (SideEffects, bool) {
    match cfg
        .tool_side_effects
        .get(remote_name)
        .copied()
        .or(cfg.side_effects)
    {
        Some(se) => (se, true),
        None => (SideEffects::Execute, false),
    }
}

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
        let (side_effects, classified) = resolve_side_effects(cfg, def.name.as_ref());
        tools.push(Arc::new(RemoteMcpTool::new(
            &cfg.name,
            cfg.prefix.as_deref(),
            &def,
            side_effects,
            classified,
            peer.clone(),
        )));
    }
    Ok(tools)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(side_effects: Option<SideEffects>, per_tool: &[(&str, SideEffects)]) -> McpServerConfig {
        McpServerConfig {
            name: "srv".to_owned(),
            url: "http://localhost/mcp".to_owned(),
            tools: None,
            prefix: None,
            side_effects,
            tool_side_effects: per_tool
                .iter()
                .map(|(n, se)| (n.to_string(), *se))
                .collect::<HashMap<_, _>>(),
        }
    }

    #[test]
    fn unclassified_server_fails_safe_to_execute() {
        // mu-cvm5 ACCEPTANCE: adding an MCP server with no classification
        // fails safe — every imported tool is the most restrictive class.
        let c = cfg(None, &[]);
        assert_eq!(
            resolve_side_effects(&c, "anything"),
            (SideEffects::Execute, false)
        );
        assert_eq!(
            resolve_side_effects(&c, "delete_everything"),
            (SideEffects::Execute, false)
        );
    }

    #[test]
    fn server_wide_floor_applies_to_all_tools() {
        let c = cfg(Some(SideEffects::ReadOnly), &[]);
        assert_eq!(
            resolve_side_effects(&c, "code_recall"),
            (SideEffects::ReadOnly, true)
        );
        assert_eq!(
            resolve_side_effects(&c, "code_status"),
            (SideEffects::ReadOnly, true)
        );
    }

    #[test]
    fn per_tool_override_beats_server_floor() {
        // Server trusted as read_only, but one tool pinned higher.
        let c = cfg(
            Some(SideEffects::ReadOnly),
            &[("run_query", SideEffects::External)],
        );
        assert_eq!(
            resolve_side_effects(&c, "code_recall"),
            (SideEffects::ReadOnly, true)
        );
        assert_eq!(
            resolve_side_effects(&c, "run_query"),
            (SideEffects::External, true)
        );
    }

    #[test]
    fn per_tool_override_applies_even_without_server_floor() {
        // No server-wide floor: unlisted tools fail safe to Execute, but a
        // per-tool classification still takes effect for the named tool.
        let c = cfg(None, &[("code_recall", SideEffects::ReadOnly)]);
        assert_eq!(
            resolve_side_effects(&c, "code_recall"),
            (SideEffects::ReadOnly, true)
        );
        assert_eq!(
            resolve_side_effects(&c, "other"),
            (SideEffects::Execute, false)
        );
    }
}
