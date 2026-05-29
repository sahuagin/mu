//! MCP server surface — rmcp SDK implementation.
//!
//! Exposes mu's session status as subscribable resources and mailbox
//! operations as tools, over a Unix socket. Replaces the hand-rolled
//! MCP framing with rmcp's `ServerHandler` trait.
//!
//! Resources:
//!   mu://session/{id}/status  — SessionStatus (subscribable)
//!   mu://daemon/status        — daemon-wide metrics
//!
//! Tools (migrated from hand-rolled):
//!   mu_daemon_info, mu_peer_hello, mu_mailbox_post,
//!   mu_mailbox_list, mu_mailbox_read, mu_mailbox_consume
//!
//! Custom notifications:
//!   mu/session_status — pushes full SessionStatus inline on change
//!     (subscribers don't need to re-read after resource_updated)

// rmcp's ServerHandler trait wants `impl Future` return types in several
// places, so these can't all become `async fn` without fighting the SDK shape.
#![allow(clippy::manual_async_fn)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::model::*;
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt};
use serde_json::{json, Map as JsonMap, Value};
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{debug, info};

use mu_core::protocol::{Request, Response, JSONRPC_VERSION};
use mu_core::session_status::{ProviderSnapshot, SessionStatus, StatusInputs};
use mu_core::transport::NotificationWriter;

use super::daemon_info::DaemonInfo;
use super::discovery::now_unix_ms;
use super::handlers::mailbox::*;
use super::sessions::Sessions;

/// Default socket path: $MU_STATE_DIR/mcp.sock or ~/.local/share/mu/mcp.sock
pub fn default_mcp_socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("MU_STATE_DIR") {
        return PathBuf::from(dir).join("mcp.sock");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".local/share/mu").join("mcp.sock")
}

/// Start the MCP unix socket listener. Runs until the listener is dropped.
/// Spawns a task per connection.
pub async fn serve_mcp_socket(
    socket_path: PathBuf,
    sessions: Sessions,
    daemon_info: DaemonInfo,
) -> anyhow::Result<()> {
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
    }
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = UnixListener::bind(&socket_path)?;
    info!("MCP server listening on {}", socket_path.display());

    loop {
        let (stream, _addr) = listener.accept().await?;
        let sessions = sessions.clone();
        let daemon_info = daemon_info.clone();
        tokio::spawn(async move {
            let handler = MuMcpHandler::new(sessions, daemon_info);
            let (reader, writer) = stream.into_split();
            match handler.serve((reader, writer)).await {
                Ok(running) => {
                    debug!("MCP connection established");
                    let _ = running.waiting().await;
                    debug!("MCP connection closed");
                }
                Err(e) => {
                    debug!("MCP connection failed: {e:#}");
                }
            }
        });
    }
}

// ─── Handler ────────────────────────────────────────────────────────

#[derive(Clone)]
struct MuMcpHandler {
    sessions: Sessions,
    daemon_info: DaemonInfo,
    /// Active subscription tasks keyed by resource URI. Each entry
    /// holds a JoinHandle that watches the session's status channel
    /// and pushes notifications to the peer. Dropped on unsubscribe.
    watch_tasks: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
}

impl MuMcpHandler {
    fn new(sessions: Sessions, daemon_info: DaemonInfo) -> Self {
        Self {
            sessions,
            daemon_info,
            watch_tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn compute_session_status(&self, session_id: &str) -> Option<SessionStatus> {
        let log = self.sessions.event_log(session_id)?;
        let (provider_kind, model) = log.provider_info().unwrap_or_default();
        let usage = log.cumulative_usage();
        let provider_status = self
            .sessions
            .provider_status_snapshot(session_id)
            .map(|snap| ProviderSnapshot {
                kind: snap.kind,
                started_at_unix_ms: snap.started_at_unix_ms,
                now_unix_ms: now_unix_ms(),
            });

        Some(SessionStatus::compute(StatusInputs {
            session_id,
            daemon_id: self.daemon_info.daemon_id(),
            provider_kind: &provider_kind,
            model: &model,
            cumulative_usage: usage.as_ref(),
            ask_count: log.ask_count(),
            tool_call_count: log.tool_call_count(),
            elapsed_total_ms: log.elapsed_total_ms(),
            provider_status,
        }))
    }
}

impl ServerHandler for MuMcpHandler {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_resources_subscribe()
                .build(),
        )
        .with_server_info(Implementation::new("mu", env!("CARGO_PKG_VERSION")))
        .with_instructions("mu daemon — session status resources + mailbox tools")
    }

    // ─── Resources ──────────────────────────────────────────────────

    fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListResourcesResult, McpError>> + Send + '_ {
        async move {
            let snapshot = self.sessions.snapshot_for_listing();
            let mut resources = Vec::with_capacity(snapshot.len() + 1);

            resources.push(Resource::new(
                RawResource::new("mu://daemon/status", "daemon-status")
                    .with_description("Daemon-wide status and metrics"),
                None,
            ));

            for (sid, _log, _parent) in &snapshot {
                resources.push(Resource::new(
                    RawResource::new(
                        format!("mu://session/{sid}/status"),
                        format!("session-{sid}-status"),
                    )
                    .with_description(format!("Session {sid} status")),
                    None,
                ));
            }

            Ok(ListResourcesResult {
                resources,
                ..Default::default()
            })
        }
    }

    fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListResourceTemplatesResult, McpError>> + Send + '_
    {
        async move {
            Ok(ListResourceTemplatesResult {
                resource_templates: vec![ResourceTemplate::new(
                    RawResourceTemplate::new("mu://session/{session_id}/status", "session-status")
                        .with_description(
                            "Live session metrics: phase, tokens, cost, context pressure",
                        ),
                    None,
                )],
                ..Default::default()
            })
        }
    }

    fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ReadResourceResult, McpError>> + Send + '_ {
        async move {
            let uri = request.uri.as_str();

            if uri == "mu://daemon/status" {
                let stats = self.sessions.snapshot_for_listing();
                let daemon_status = json!({
                    "daemon_id": self.daemon_info.daemon_id(),
                    "version": self.daemon_info.version(),
                    "uptime_ms": self.daemon_info.uptime_ms(),
                    "session_count": stats.len(),
                });
                return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                    serde_json::to_string_pretty(&daemon_status).unwrap_or_default(),
                    uri,
                )]));
            }

            if let Some(session_id) = uri
                .strip_prefix("mu://session/")
                .and_then(|rest| rest.strip_suffix("/status"))
            {
                let status = self.compute_session_status(session_id).ok_or_else(|| {
                    McpError::resource_not_found(
                        format!("session not found: {session_id}"),
                        Some(json!({"uri": uri})),
                    )
                })?;
                return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                    serde_json::to_string_pretty(&status).unwrap_or_default(),
                    uri,
                )]));
            }

            Err(McpError::resource_not_found(
                format!("unknown resource: {uri}"),
                None,
            ))
        }
    }

    fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), McpError>> + Send + '_ {
        async move {
            let uri = request.uri.as_str().to_string();
            info!(uri = %uri, "MCP subscribe");

            // Extract session_id from URI and get the watch channel.
            let session_id = uri
                .strip_prefix("mu://session/")
                .and_then(|rest| rest.strip_suffix("/status"))
                .map(|s| s.to_string());

            if let Some(ref sid) = session_id {
                if let Some(mut rx) = self.sessions.status_watch(sid) {
                    let peer = context.peer.clone();
                    let uri_clone = uri.clone();
                    let task = tokio::spawn(async move {
                        while rx.changed().await.is_ok() {
                            let status = rx.borrow().clone();
                            if let Some(ref status) = status {
                                // Push the standard resource_updated notification
                                let _ = peer
                                    .send_notification(
                                        ServerNotification::ResourceUpdatedNotification(
                                            ResourceUpdatedNotification::new(
                                                ResourceUpdatedNotificationParam {
                                                    uri: uri_clone.clone(),
                                                },
                                            ),
                                        ),
                                    )
                                    .await;
                                // Push our custom inline notification with full payload
                                if let Ok(payload) = serde_json::to_value(status) {
                                    let _ = peer
                                        .send_notification(ServerNotification::CustomNotification(
                                            CustomNotification::new(
                                                "mu/session_status",
                                                Some(payload),
                                            ),
                                        ))
                                        .await;
                                }
                            }
                        }
                    });
                    let mut tasks = self.watch_tasks.lock().await;
                    if let Some(old) = tasks.insert(uri, task) {
                        old.abort();
                    }
                    return Ok(());
                }
            }

            // For URIs we can't subscribe to, accept silently (no-op).
            Ok(())
        }
    }

    fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), McpError>> + Send + '_ {
        async move {
            let uri = request.uri.as_str().to_string();
            info!(uri = %uri, "MCP unsubscribe");
            let mut tasks = self.watch_tasks.lock().await;
            if let Some(task) = tasks.remove(&uri) {
                task.abort();
            }
            Ok(())
        }
    }

    // ─── Tools ──────────────────────────────────────────────────────

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        async move {
            Ok(ListToolsResult {
                tools: tools_list(),
                ..Default::default()
            })
        }
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResult, McpError>> + Send + '_ {
        let sessions = self.sessions.clone();
        let daemon_info = self.daemon_info.clone();
        async move {
            let arguments = Value::Object(request.arguments.unwrap_or_default());
            match dispatch_tool(&request.name, arguments, &sessions, &daemon_info).await {
                Ok(v) => Ok(CallToolResult::success(vec![Content::new(
                    RawContent::text(v.to_string()),
                    None,
                )])),
                Err(msg) => Ok(CallToolResult::error(vec![Content::new(
                    RawContent::text(msg),
                    None,
                )])),
            }
        }
    }
}

// ─── Tool definitions (migrated from hand-rolled) ───────────────────

fn schema(v: Value) -> Arc<JsonMap<String, Value>> {
    match v {
        Value::Object(m) => Arc::new(m),
        _ => Arc::new(JsonMap::new()),
    }
}

fn tools_list() -> Vec<Tool> {
    vec![
        Tool::new(
            "mu_daemon_info",
            "Get daemon info: daemon_id, session count, uptime.",
            schema(json!({ "type": "object", "properties": {}, "required": [] })),
        ),
        Tool::new(
            "mu_peer_hello",
            "Request a mailbox peer handle from a target session.",
            schema(json!({
                "type": "object",
                "properties": {
                    "to_session_id": {"type": "string"},
                    "from_daemon_id": {"type": "string"},
                    "from_session_id": {"type": "string"},
                    "want_method": {"type": "string"}
                },
                "required": ["to_session_id", "from_daemon_id", "from_session_id"]
            })),
        ),
        Tool::new(
            "mu_mailbox_post",
            "Post a message to a session's mailbox.",
            schema(json!({
                "type": "object",
                "properties": {
                    "to_session_id": {"type": "string"},
                    "peer_handle": {"type": "string"},
                    "from_daemon_id": {"type": "string"},
                    "from_session_id": {"type": "string"},
                    "kind": {"type": "string"},
                    "subject": {"type": "string"},
                    "body": {}
                },
                "required": ["to_session_id", "peer_handle", "from_daemon_id", "from_session_id", "kind", "subject", "body"]
            })),
        ),
        Tool::new(
            "mu_mailbox_list",
            "List messages in a session's mailbox.",
            schema(json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "since_seq": {"type": "number"},
                    "include_consumed": {"type": "boolean"}
                },
                "required": ["session_id"]
            })),
        ),
        Tool::new(
            "mu_mailbox_read",
            "Read the full body of a single mailbox message.",
            schema(json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "seq": {"type": "number"}
                },
                "required": ["session_id", "seq"]
            })),
        ),
        Tool::new(
            "mu_mailbox_consume",
            "Mark messages as consumed.",
            schema(json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "seqs": {"type": "array", "items": {"type": "number"}}
                },
                "required": ["session_id", "seqs"]
            })),
        ),
    ]
}

// ─── Tool dispatch → existing mailbox handlers ──────────────────────

async fn dispatch_tool(
    name: &str,
    arguments: Value,
    sessions: &Sessions,
    daemon_info: &DaemonInfo,
) -> Result<Value, String> {
    match name {
        "mu_daemon_info" => Ok(json!({
            "daemon_id": daemon_info.daemon_id(),
            "version": daemon_info.version(),
            "session_count": sessions.snapshot_for_listing().len(),
        })),
        "mu_peer_hello" => {
            let to_session_id = str_field(&arguments, "to_session_id")?;
            let from_daemon_id = str_field(&arguments, "from_daemon_id")?;
            let from_session_id = str_field(&arguments, "from_session_id")?;
            let want_method = arguments
                .get("want_method")
                .and_then(|v| v.as_str())
                .unwrap_or("mailbox.post");
            let rpc_params = json!({
                "to_session_id": to_session_id,
                "from": {
                    "daemon_id": from_daemon_id,
                    "session_id": from_session_id,
                    "advertised_capabilities": []
                },
                "want": { "method": want_method }
            });
            let request = make_internal_request("peer.hello", rpc_params);
            let response = handle_peer_hello(request, sessions.clone(), daemon_info.clone());
            extract_result(response)
        }
        "mu_mailbox_post" => {
            let rpc_params = json!({
                "to_session_id": str_field(&arguments, "to_session_id")?,
                "peer_handle": str_field(&arguments, "peer_handle")?,
                "from": {
                    "daemon_id": str_field(&arguments, "from_daemon_id")?,
                    "session_id": str_field(&arguments, "from_session_id")?,
                },
                "kind": str_field(&arguments, "kind")?,
                "subject": str_field(&arguments, "subject")?,
                "body": arguments.get("body").cloned().unwrap_or(Value::Null),
            });
            let request = make_internal_request("mailbox.post", rpc_params);
            let notif = NotificationWriter::sink();
            let response =
                handle_mailbox_post(request, sessions.clone(), notif, daemon_info.clone()).await;
            extract_result(response)
        }
        "mu_mailbox_list" => {
            let rpc_params = json!({
                "session_id": str_field(&arguments, "session_id")?,
                "since_seq": arguments.get("since_seq").cloned(),
                "include_consumed": arguments.get("include_consumed").and_then(|v| v.as_bool()).unwrap_or(false),
            });
            let request = make_internal_request("mailbox.list", rpc_params);
            let response = handle_mailbox_list(request, sessions.clone());
            extract_result(response)
        }
        "mu_mailbox_read" => {
            let seq = arguments
                .get("seq")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| "missing required field: seq".to_string())?;
            let rpc_params = json!({
                "session_id": str_field(&arguments, "session_id")?,
                "seq": seq,
            });
            let request = make_internal_request("mailbox.read", rpc_params);
            let response = handle_mailbox_read(request, sessions.clone());
            extract_result(response)
        }
        "mu_mailbox_consume" => {
            let seqs = arguments
                .get("seqs")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_u64()).collect::<Vec<_>>())
                .unwrap_or_default();
            let rpc_params = json!({
                "session_id": str_field(&arguments, "session_id")?,
                "seqs": seqs,
            });
            let request = make_internal_request("mailbox.consume", rpc_params);
            let response = handle_mailbox_consume(request, sessions.clone());
            extract_result(response)
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

fn str_field(args: &Value, field: &str) -> Result<String, String> {
    args.get(field)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing required field: {field}"))
}

fn make_internal_request(method: &str, params: Value) -> Request<Value> {
    Request {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id: json!(1),
        method: method.to_string(),
        params,
    }
}

fn extract_result(response: Response<Value>) -> Result<Value, String> {
    match response {
        Response::Ok { result, .. } => Ok(result),
        Response::Err { error, .. } => Err(format!("{}: {}", error.code, error.message)),
    }
}
