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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mu_core::agent::{PermissionLevel, SideEffects, Tool};
use mu_core::config::McpServerConfig;
use mu_core::protocol::{McpImportedToolStatus, McpServerConnectionState, McpServerStatus};
use rmcp::model::{
    ClientCapabilities, ClientInfo, ClientRequest, CustomRequest, ExperimentalCapabilities,
    Implementation, ServerResult,
};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::ServiceExt;
use serde_json::Value;

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

/// Outbound MCP client handler. Replaces the trivial `()` `ClientHandler`:
/// it still needs none of the client-offered features (sampling / roots /
/// elicitation), but it advertises `experimental.mu.aiHelp` so a mu-aware peer
/// offers its negotiated `mu/aiHelp` surface, and it carries a per-connection
/// cache for the scoped help nodes fetched lazily from that peer.
#[derive(Clone, Default)]
pub(crate) struct MuMcpClient {
    /// Scoped AI-help nodes fetched on demand from the peer, keyed by their
    /// scope path. PARTIAL at rest: only the paths a caller actually asked for
    /// are materialized — the tree is never fetched whole, and nothing is
    /// fetched during import/handshake. Read only through [`fetch_ai_help`],
    /// which the live agent loop does not yet drive (navigation wiring is out
    /// of this change's scope), so it is dead outside tests.
    #[cfg_attr(not(test), allow(dead_code))]
    help_cache: Arc<Mutex<HashMap<Vec<String>, Value>>>,
}

impl rmcp::ClientHandler for MuMcpClient {
    fn get_info(&self) -> ClientInfo {
        // Advertise the experimental feature flag the mu server negotiates on:
        // `experimental.mu.aiHelp: true`. A non-mu server simply ignores an
        // experimental key it doesn't recognize.
        let mut mu = serde_json::Map::new();
        mu.insert("aiHelp".to_string(), Value::Bool(true));
        let mut experimental = ExperimentalCapabilities::new();
        experimental.insert("mu".to_string(), mu);
        ClientInfo::new(
            ClientCapabilities::builder()
                .enable_experimental_with(experimental)
                .build(),
            Implementation::new("mu", env!("CARGO_PKG_VERSION")),
        )
    }
}

/// The long-lived client handle for one remote server. One connection, shared
/// (Arc) by every `RemoteMcpTool` and by [`fetch_ai_help`].
pub(crate) type McpPeer = rmcp::service::RunningService<rmcp::service::RoleClient, MuMcpClient>;

/// Lazily fetch the AI-help node for `path` from a peer, caching it on the
/// connection. The first call for a path issues the `mu/aiHelp` custom request
/// and stores the result; later calls for the same path return the cached node
/// with no round trip. Only the requested scope is ever materialized — this is
/// the "partial at rest" ingestion: navigation pulls one node at a time, on
/// demand, NEVER during import. A peer that did not negotiate the feature
/// answers `METHOD_NOT_FOUND`, surfaced here as an error.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn fetch_ai_help(peer: &McpPeer, path: &[String]) -> anyhow::Result<Value> {
    fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }
    if let Some(hit) = lock(&peer.service().help_cache).get(path).cloned() {
        return Ok(hit);
    }
    let request = ClientRequest::CustomRequest(CustomRequest::new(
        "mu/aiHelp",
        Some(serde_json::json!({ "path": path })),
    ));
    let node = match peer.send_request(request).await {
        Ok(ServerResult::CustomResult(custom)) => custom.0,
        Ok(other) => anyhow::bail!("mu/aiHelp returned an unexpected result: {other:?}"),
        Err(e) => anyhow::bail!("mu/aiHelp request failed: {e}"),
    };
    lock(&peer.service().help_cache).insert(path.to_vec(), node.clone());
    Ok(node)
}

pub struct ImportedMcpTool {
    pub tool: Arc<dyn Tool>,
    pub status_server_index: usize,
    pub status_tool_index: usize,
}

pub struct ImportedMcpTools {
    pub tools: Vec<ImportedMcpTool>,
    pub status: Vec<McpServerStatus>,
}

/// Connect to every configured server and return the imported tools plus a
/// daemon-authoritative import report. Failures are per-server and non-fatal.
pub async fn import_remote_tools(servers: &[McpServerConfig]) -> ImportedMcpTools {
    /// Per-server budget for connect + initialize + tools/list. A hung or
    /// unresponsive server must degrade like an unreachable one — it cannot
    /// be allowed to stall daemon startup.
    const IMPORT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    let mut out: Vec<ImportedMcpTool> = Vec::new();
    let mut status: Vec<McpServerStatus> = Vec::new();
    for cfg in servers {
        let started = std::time::Instant::now();
        let imported = tokio::time::timeout(IMPORT_TIMEOUT, import_from_server(cfg))
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out after {IMPORT_TIMEOUT:?}")));
        let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        match imported {
            Ok((tools, imported_tools)) => {
                tracing::info!(
                    server = %cfg.name,
                    url = %cfg.url,
                    count = tools.len(),
                    "imported MCP tools"
                );
                let server_index = status.len();
                status.push(server_status(
                    cfg,
                    McpServerConnectionState::Connected,
                    imported_tools,
                    None,
                    Some(elapsed_ms),
                ));
                out.extend(
                    tools
                        .into_iter()
                        .enumerate()
                        .map(|(status_tool_index, tool)| ImportedMcpTool {
                            tool,
                            status_server_index: server_index,
                            status_tool_index,
                        }),
                );
            }
            Err(e) => {
                tracing::warn!(
                    server = %cfg.name,
                    url = %cfg.url,
                    error = %e,
                    "MCP server unreachable; no tools imported from it"
                );
                status.push(server_status(
                    cfg,
                    McpServerConnectionState::Unavailable,
                    Vec::new(),
                    Some(e.to_string()),
                    Some(elapsed_ms),
                ));
            }
        }
    }
    ImportedMcpTools { tools: out, status }
}

fn server_status(
    cfg: &McpServerConfig,
    state: McpServerConnectionState,
    imported_tools: Vec<McpImportedToolStatus>,
    last_error: Option<String>,
    elapsed_ms: Option<u64>,
) -> McpServerStatus {
    McpServerStatus {
        name: cfg.name.clone(),
        url: cfg.url.clone(),
        configured_tools: cfg.tools.clone(),
        prefix: cfg.prefix.clone().filter(|p| !p.is_empty()),
        side_effects: cfg.side_effects,
        tool_side_effects: cfg.tool_side_effects.clone(),
        state,
        imported_tools,
        last_error,
        elapsed_ms,
    }
}

fn local_tool_name(prefix: Option<&str>, remote_name: &str) -> String {
    match prefix {
        Some(p) if !p.is_empty() => format!("{p}{remote_name}"),
        _ => remote_name.to_string(),
    }
}

/// Handshake with one server and wrap its (allowlisted) tools.
async fn import_from_server(
    cfg: &McpServerConfig,
) -> anyhow::Result<(Vec<Arc<dyn Tool>>, Vec<McpImportedToolStatus>)> {
    let transport = StreamableHttpClientTransport::from_uri(cfg.url.as_str());
    // `initialize` handshake; the returned service owns the connection and
    // is shared (Arc) by every RemoteMcpTool imported from this server. The
    // handshake advertises `experimental.mu.aiHelp` but does NOT fetch any
    // help — scoped help is pulled lazily later via [`fetch_ai_help`].
    let peer: Arc<McpPeer> = Arc::new(MuMcpClient::default().serve(transport).await?);
    let remote = peer.list_all_tools().await?;
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    let mut imported_tools: Vec<McpImportedToolStatus> = Vec::new();
    for def in remote {
        if let Some(allow) = &cfg.tools {
            if !allow.iter().any(|a| a == def.name.as_ref()) {
                continue;
            }
        }
        let remote_name = def.name.to_string();
        let local_name = local_tool_name(cfg.prefix.as_deref(), &remote_name);
        let (side_effects, classified) = resolve_side_effects(cfg, &remote_name);
        let permission = if classified {
            PermissionLevel::Allow
        } else {
            PermissionLevel::Ask
        };
        imported_tools.push(McpImportedToolStatus {
            remote_name,
            local_name,
            side_effects,
            permission,
            classified,
            registered: true,
        });
        tools.push(Arc::new(RemoteMcpTool::new(
            &cfg.name,
            cfg.prefix.as_deref(),
            &def,
            side_effects,
            classified,
            peer.clone(),
        )));
    }
    Ok((tools, imported_tools))
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

    #[test]
    fn server_status_reports_configured_import_metadata() {
        let mut c = cfg(
            Some(SideEffects::ReadOnly),
            &[("run", SideEffects::External)],
        );
        c.tools = Some(vec!["code_status".to_string()]);
        c.prefix = Some("idx_".to_string());
        let status = server_status(
            &c,
            McpServerConnectionState::Connected,
            vec![McpImportedToolStatus {
                remote_name: "code_status".to_string(),
                local_name: local_tool_name(c.prefix.as_deref(), "code_status"),
                side_effects: SideEffects::ReadOnly,
                permission: PermissionLevel::Allow,
                classified: true,
                registered: true,
            }],
            None,
            Some(12),
        );

        assert_eq!(status.name, "srv");
        assert_eq!(
            status.configured_tools,
            Some(vec!["code_status".to_string()])
        );
        assert_eq!(status.prefix, Some("idx_".to_string()));
        assert_eq!(status.state, McpServerConnectionState::Connected);
        assert_eq!(status.elapsed_ms, Some(12));
        assert_eq!(status.imported_tools[0].local_name, "idx_code_status");
        assert_eq!(status.tool_side_effects["run"], SideEffects::External);
    }
}
