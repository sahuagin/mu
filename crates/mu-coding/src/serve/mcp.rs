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
//!
//! ## Adapter #2 (spec mu-046 WP5 — INV-7, no side doors)
//!
//! Tool invocations no longer call handlers directly. Each one is
//! parsed into a JSON-RPC command and fed through the SAME
//! [`pipeline::ingest`] the stdio adapter uses: journaled
//! `CommandReceived` (fsync per policy) BEFORE anything processes it,
//! fail-closed `JOURNAL_UNAVAILABLE` when it can't be made durable,
//! the pipeline's auth gate applied by the consumer, execution via
//! `dispatch_inner`'s mcp.* tool table (which invokes the unchanged
//! pre-WP5 handler bodies), exactly one receipt wrapping the original
//! command, and the response returning through the tagged outbound
//! stream (INV-8) filtered by this connection's
//! `Origin { transport: "mcp", connection_id }`.
//!
//! **Naming rule:** every tool journals as method `mcp.<tool_name>` —
//! the spec's stated default namespace. No tool maps onto a native
//! wire method name, even where the shapes are close (`mu_peer_hello`
//! ≈ `peer.hello`): one uniform rule keeps the journal legible about
//! WHICH border a command crossed, and the consumer-side translation
//! table (`dispatch.rs::dispatch_mcp_tool`) owns the native-shape
//! mapping. The journaled params are the RAW MCP tool arguments — the
//! faithful border record.
//!
//! **Scope table** (mirrors the underlying handler's pipeline scope,
//! see `pipeline::classify`):
//!
//! | MCP method               | scope   | mirrors        |
//! |--------------------------|---------|----------------|
//! | `mcp.mu_daemon_info`     | daemon  | (daemon read)  |
//! | `mcp.mu_peer_hello`      | daemon  | `peer.hello`   |
//! | `mcp.mu_mailbox_post`    | session | `mailbox.post` |
//! | `mcp.mu_mailbox_list`    | daemon  | `mailbox.list` |
//! | `mcp.mu_mailbox_read`    | daemon  | `mailbox.read` |
//! | `mcp.mu_mailbox_consume` | daemon  | `mailbox.consume` |
//!
//! **Response correlation:** rmcp may interleave tool calls on one
//! connection, so each invocation mints a daemon-unique synthetic
//! request id, subscribes its own outbound receiver BEFORE ingesting,
//! and awaits the envelope matching (origin, request_id). Like
//! `transport::write_loop`, a subscriber lagged more than the stream
//! capacity is lossy — a documented MVP tradeoff.
//!
//! **Auth:** pre-WP5 the MCP surface had no auth at all. Each accepted
//! connection now gets a fresh per-connection `AuthState` with the
//! same posture as stdio ([`auth::initial_connection_state`],
//! mu-ddua): pre-authenticated root when no `[auth]` mechanism
//! enforces (the default — the MVP open-gate posture), and
//! `Unauthenticated` when bearer tokens are configured — in which case
//! every tool call is rejected `AUTH_REQUIRED` (-32001), because the
//! MCP surface offers no auth handshake yet. The gate applies
//! uniformly; an MCP-side handshake is future work.
//!
//! Resources (status reads + subscriptions) are not commands and stay
//! direct reads of session state — WP5 covers the tool surface.

// rmcp's ServerHandler trait wants `impl Future` return types in several
// places, so these can't all become `async fn` without fighting the SDK shape.
#![allow(clippy::manual_async_fn)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rmcp::model::*;
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt};
use serde_json::{json, Map as JsonMap, Value};
use tokio::net::UnixListener;
use tokio::sync::{broadcast, Mutex};
use tracing::{debug, info};

use mu_core::command_journal::Origin;
use mu_core::protocol::{Request, Response, JSONRPC_VERSION};
use mu_core::session_status::{ProviderSnapshot, SessionStatus, StatusInputs};
use mu_core::transport::OutboundStream;

use super::auth::{self, AuthRegistry, AuthStateHandle};
use super::daemon_info::DaemonInfo;
use super::discovery::now_unix_ms;
use super::dispatch::MCP_METHOD_PREFIX;
use super::pipeline::{self, ControlPlane};
use super::sessions::Sessions;

/// Default socket path: $MU_STATE_DIR/mcp.sock or ~/.local/share/mu/mcp.sock
pub fn default_mcp_socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("MU_STATE_DIR") {
        return PathBuf::from(dir).join("mcp.sock");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".local/share/mu").join("mcp.sock")
}

/// Process-wide connection counter so every accepted MCP connection
/// gets a unique [`Origin`] (mirrors the stdio transport's counter in
/// `mu_core::transport`).
static MCP_CONNECTION_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Allocate one MCP connection's border identity at accept time.
fn next_mcp_origin() -> Origin {
    let id = MCP_CONNECTION_COUNTER.fetch_add(1, Ordering::Relaxed);
    Origin {
        transport: "mcp".into(),
        connection_id: Some(id.to_string()),
    }
}

/// Start the MCP unix socket listener. Runs until the listener is dropped.
/// Spawns a task per connection.
///
/// spec mu-046 WP5: the listener holds producer handles into the
/// ingest pipeline (`control`) and the daemon-wide outbound stream —
/// every tool invocation on every connection crosses
/// [`pipeline::ingest`] like any stdio request. Each accepted
/// connection gets its own [`Origin`] and a fresh per-connection auth
/// state derived from `auth_registry` (module doc, "Auth").
pub(crate) async fn serve_mcp_socket(
    socket_path: PathBuf,
    sessions: Sessions,
    daemon_info: DaemonInfo,
    control: Arc<ControlPlane>,
    outbound: OutboundStream,
    auth_registry: Arc<AuthRegistry>,
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
        let control = control.clone();
        let outbound = outbound.clone();
        let auth_state: AuthStateHandle = Arc::new(std::sync::Mutex::new(
            auth::initial_connection_state(&auth_registry),
        ));
        tokio::spawn(async move {
            let handler = MuMcpHandler::new(
                sessions,
                daemon_info,
                control,
                outbound,
                next_mcp_origin(),
                auth_state,
            );
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
    /// Producer handle into the ingest pipeline (spec mu-046 WP5):
    /// every tool invocation becomes a journaled `mcp.<tool>` command
    /// through [`pipeline::ingest`] — adapter #2, no side doors.
    control: Arc<ControlPlane>,
    /// The daemon-wide outbound stream (INV-8); tool responses come
    /// back as envelopes tagged with this connection's origin.
    outbound: OutboundStream,
    /// This connection's border identity (`transport: "mcp"`).
    origin: Origin,
    /// This connection's auth state — the pipeline's gate reads it at
    /// processing time (module doc, "Auth").
    auth_state: AuthStateHandle,
    /// Active subscription tasks keyed by resource URI. Each entry
    /// holds a JoinHandle that watches the session's status channel
    /// and pushes notifications to the peer. Dropped on unsubscribe.
    watch_tasks: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
}

impl MuMcpHandler {
    fn new(
        sessions: Sessions,
        daemon_info: DaemonInfo,
        control: Arc<ControlPlane>,
        outbound: OutboundStream,
        origin: Origin,
        auth_state: AuthStateHandle,
    ) -> Self {
        Self {
            sessions,
            daemon_info,
            control,
            outbound,
            origin,
            auth_state,
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
        async move {
            let arguments = Value::Object(request.arguments.unwrap_or_default());
            match self.dispatch_tool(&request.name, arguments).await {
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

// ─── Tool dispatch → ingest pipeline (spec mu-046 WP5) ──────────────

/// Daemon-unique synthetic JSON-RPC ids for MCP-originated commands.
/// MCP tool invocations carry no client-chosen JSON-RPC id, so the
/// adapter mints one to correlate the response envelope (and the
/// journal record).
static MCP_REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

fn next_mcp_request_id() -> Value {
    let n = MCP_REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    Value::String(format!("mcp-{n}"))
}

impl MuMcpHandler {
    /// One tool invocation through the border: build the
    /// `mcp.<tool_name>` command (params = the raw MCP arguments),
    /// cross [`pipeline::ingest`] — journaled before processing,
    /// fail-closed on journal error — then await this command's
    /// response envelope on the outbound stream and translate it back
    /// into the MCP tool-result shape (`Ok(result)` → success content,
    /// `Err("code: message")` → error content, exactly the pre-WP5
    /// strings for handler outcomes).
    async fn dispatch_tool(&self, name: &str, arguments: Value) -> Result<Value, String> {
        let request_id = next_mcp_request_id();
        let request = Request {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: request_id.clone(),
            method: format!("{MCP_METHOD_PREFIX}{name}"),
            params: arguments,
        };
        // Subscribe BEFORE ingesting so the response envelope cannot
        // slip past between enqueue and subscribe.
        let mut rx = self.outbound.subscribe();
        if let Some(response) = pipeline::ingest(
            &self.control,
            request,
            self.origin.clone(),
            &self.auth_state,
        ) {
            // Immediate reject (journal unavailable / daemon shutting
            // down): nothing was enqueued, nothing will arrive on the
            // stream — fail closed to the MCP caller.
            return extract_result(response);
        }
        loop {
            match rx.recv().await {
                Ok(envelope) => {
                    if envelope.origin.as_ref() != Some(&self.origin) {
                        continue;
                    }
                    if envelope.request_id.as_ref() != Some(&request_id) {
                        continue;
                    }
                    let response: Response<Value> = serde_json::from_value(envelope.item.0)
                        .map_err(|e| format!("malformed response envelope: {e}"))?;
                    return extract_result(response);
                }
                // Same documented lossy-under-extreme-lag tradeoff as
                // transport::write_loop: skipped envelopes are gone. If
                // this command's response was among them the await would
                // never resolve, so surface the lag as an error instead
                // of risking a wedged MCP connection.
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    return Err(format!(
                        "outbound stream lagged ({skipped} envelopes dropped); \
                         tool result lost — retry the call"
                    ));
                }
                // Every sender dropped — daemon shutting down.
                Err(broadcast::error::RecvError::Closed) => {
                    return Err("daemon outbound stream closed (shutting down)".to_string());
                }
            }
        }
    }
}

fn extract_result(response: Response<Value>) -> Result<Value, String> {
    match response {
        Response::Ok { result, .. } => Ok(result),
        Response::Err { error, .. } => Err(format!("{}: {}", error.code, error.message)),
    }
}

// ─── Tests (spec mu-046 WP5) ────────────────────────────────────────
//
// Real rmcp round trips: a server handler over a tempdir unix socket
// wired to a real control plane + journal, driven by an rmcp client —
// the same client shape `mcp_client` uses (`()` is the trivial
// ClientHandler).

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as StdHashMap;
    use std::path::Path;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    use serde_json::json;
    use tokio::sync::mpsc;

    use mu_core::agent::AgentInput;
    use mu_core::capability::Capability;
    use mu_core::command_journal::{
        CommandJournal, FsyncPolicy, JournalPayload, JournalRecord, RejectStage,
    };
    use mu_core::context::CacheTtl;
    use mu_core::event_log::{EventPayload, SessionEventLog};

    use super::super::discovery::SessionDiscovery;
    use super::super::factory::ProviderFactory;
    use super::super::LocalRegistryBackend;

    struct Harness {
        /// Owns the socket + journal paths; dropped last.
        _dir: tempfile::TempDir,
        journal_path: std::path::PathBuf,
        sessions: Sessions,
        daemon_info: DaemonInfo,
        control: Arc<ControlPlane>,
        client: rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
        _server: tokio::task::JoinHandle<()>,
    }

    /// Default-config registry: BEARER with an empty allowlist never
    /// enforces, so MCP connections start pre-authenticated (mu-ddua —
    /// the MVP open-gate posture).
    fn open_registry() -> Arc<AuthRegistry> {
        Arc::new(auth::registry_from_config(
            &mu_core::config::Config::default().auth,
        ))
    }

    /// Enforcing registry: with tokens configured the gate is live and
    /// fresh MCP connections start `Unauthenticated`.
    fn enforcing_registry() -> Arc<AuthRegistry> {
        Arc::new(auth::registry_from_config(
            &mu_core::config::AuthConfig::Bearer {
                tokens: vec!["mcp-test-token".into()],
            },
        ))
    }

    /// Full adapter stack: journal (tempdir, [journal].dir pattern) →
    /// control plane → MCP socket → connected rmcp client.
    async fn spawn_harness(auth_registry: Arc<AuthRegistry>) -> Harness {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal_path = dir.path().join("daemon.jsonl");
        let journal = Arc::new(
            CommandJournal::open(&journal_path, "d-mcp-test", FsyncPolicy::Never)
                .expect("open journal"),
        );
        let sessions = Sessions::new();
        let factory: ProviderFactory =
            Arc::new(|_selector, _cache_ttl| Err(anyhow::anyhow!("no provider in mcp unit tests")));
        let daemon_info = DaemonInfo::new("test");
        let discovery: Arc<dyn SessionDiscovery> = Arc::new(LocalRegistryBackend::new(
            sessions.clone(),
            daemon_info.daemon_id().to_string(),
        ));
        let outbound = OutboundStream::new();
        let control = Arc::new(pipeline::spawn_control_plane(
            journal,
            pipeline::PipelineCtx {
                sessions: sessions.clone(),
                factory,
                tools: Arc::new(Vec::new()),
                skills: Arc::new(Vec::new()),
                daemon_info: daemon_info.clone(),
                discovery,
                auth_registry: auth_registry.clone(),
            },
            outbound.clone(),
        ));
        let socket_path = dir.path().join("mcp.sock");
        let server = {
            let socket_path = socket_path.clone();
            let sessions = sessions.clone();
            let daemon_info = daemon_info.clone();
            let control = control.clone();
            tokio::spawn(async move {
                let _ = serve_mcp_socket(
                    socket_path,
                    sessions,
                    daemon_info,
                    control,
                    outbound,
                    auth_registry,
                )
                .await;
            })
        };
        // Bind creates the socket file; existence means accept is live.
        for _ in 0..500 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let stream = tokio::net::UnixStream::connect(&socket_path)
            .await
            .expect("connect mcp socket");
        let client = ().serve(stream.into_split()).await.expect("mcp client handshake");
        Harness {
            _dir: dir,
            journal_path,
            sessions,
            daemon_info,
            control,
            client,
            _server: server,
        }
    }

    /// Register a fake live session: a real input channel (the test
    /// keeps the receiver) and a disk-backed event log under `dir` —
    /// the WP4 session-pipeline shape (same helper as pipeline tests).
    fn insert_session(
        sessions: &Sessions,
        id: &str,
        input_tx: mpsc::Sender<AgentInput>,
        disk_dir: &Path,
    ) -> Arc<SessionEventLog> {
        let log = Arc::new(SessionEventLog::new(id.to_string()));
        log.attach_disk_writer(&disk_dir.join(format!("{id}.jsonl")))
            .expect("attach disk writer");
        sessions.insert(
            id.to_string(),
            super::super::sessions::NewSession {
                input_tx,
                forwarder: tokio::spawn(async {}),
                agent: tokio::spawn(async {}),
                event_log: log.clone(),
                pending_approvals: Arc::new(StdMutex::new(StdHashMap::new())),
                parent_session_id: None,
                capability: Arc::new(StdMutex::new(Capability::root())),
                cache_ttl: CacheTtl::default(),
                provider_status: Arc::new(StdMutex::new(
                    super::super::provider_status::ProviderStatusTracker::new(),
                )),
                mailbox: Arc::new(super::super::mailbox::MailboxState::new()),
                status_watch: None,
            },
        );
        log
    }

    async fn call_tool(harness: &Harness, name: &str, args: Value) -> CallToolResult {
        let mut params = CallToolRequestParams::new(name.to_string());
        if let Value::Object(map) = args {
            params = params.with_arguments(map);
        }
        harness
            .client
            .call_tool(params)
            .await
            .expect("call_tool transport error")
    }

    fn result_text(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn journal_records(path: &Path) -> Vec<JournalRecord> {
        let (records, malformed) = CommandJournal::replay(path).expect("replay journal");
        assert_eq!(malformed, 0, "journal has malformed records");
        records
    }

    /// Obtain a peer handle for `from_session` against `to_session`
    /// via the mu_peer_hello tool (itself through the pipeline).
    async fn peer_handle(harness: &Harness, to_session: &str, from_session: &str) -> String {
        let result = call_tool(
            harness,
            "mu_peer_hello",
            json!({
                "to_session_id": to_session,
                "from_daemon_id": harness.daemon_info.daemon_id(),
                "from_session_id": from_session,
            }),
        )
        .await;
        assert_ne!(result.is_error, Some(true), "{}", result_text(&result));
        let v: Value = serde_json::from_str(&result_text(&result)).expect("hello json");
        assert_eq!(v["outcome"], "accepted", "{v}");
        v["peer_handle"].as_str().expect("peer_handle").to_string()
    }

    /// Round trip through the pipeline (INV-7/INV-8): mu_daemon_info
    /// reaches the MCP caller via the outbound stream, and the daemon
    /// journal carries `CommandReceived { method: "mcp.mu_daemon_info",
    /// origin.transport: "mcp" }` plus exactly one success receipt
    /// wrapping the original (INV-4/INV-5).
    #[tokio::test]
    async fn daemon_info_round_trips_and_is_journaled_with_receipt() {
        let harness = spawn_harness(open_registry()).await;
        let result = call_tool(&harness, "mu_daemon_info", json!({})).await;
        assert_ne!(result.is_error, Some(true), "{}", result_text(&result));
        let v: Value = serde_json::from_str(&result_text(&result)).expect("result json");
        assert_eq!(v["version"], "test");
        assert_eq!(v["session_count"], 0);
        assert_eq!(v["daemon_id"], harness.daemon_info.daemon_id());

        // Receipts are appended before responses are emitted, so the
        // observed result means the records are durable.
        let records = journal_records(&harness.journal_path);
        let (seq, origin) = records
            .iter()
            .find_map(|r| match &r.payload {
                JournalPayload::CommandReceived { method, origin, .. }
                    if method == "mcp.mu_daemon_info" =>
                {
                    Some((r.seq, origin.clone()))
                }
                _ => None,
            })
            .expect("CommandReceived for the MCP tool");
        assert_eq!(origin.transport, "mcp");
        assert!(origin.connection_id.is_some(), "per-connection identity");
        let receipts: Vec<_> = records
            .iter()
            .filter_map(|r| match &r.payload {
                JournalPayload::CommandSucceeded {
                    command_seq,
                    command,
                    ..
                } if *command_seq == seq => Some(command.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(receipts.len(), 1, "exactly one receipt: {records:?}");
        assert_eq!(receipts[0].method, "mcp.mu_daemon_info");
    }

    /// The border ordering test: a mailbox post via MCP journals
    /// `CommandReceived` into the TARGET SESSION's own log (session
    /// scope, mirroring `mailbox.post`) BEFORE the
    /// `MailboxMessagePosted` effect, with the RAW MCP arguments as
    /// params and exactly one receipt pairing the command (INV-1/4/5).
    #[tokio::test]
    async fn mailbox_post_journals_in_session_log_before_effect() {
        let harness = spawn_harness(open_registry()).await;
        let (input_tx, _input_rx) = mpsc::channel::<AgentInput>(4);
        let log = insert_session(&harness.sessions, "s-mcp", input_tx, harness._dir.path());
        let handle = peer_handle(&harness, "s-mcp", "s-peer").await;

        let result = call_tool(
            &harness,
            "mu_mailbox_post",
            json!({
                "to_session_id": "s-mcp",
                "peer_handle": handle,
                "from_daemon_id": harness.daemon_info.daemon_id(),
                "from_session_id": "s-peer",
                "kind": "note",
                "subject": "hello over mcp",
                "body": {"x": 1},
            }),
        )
        .await;
        assert_ne!(result.is_error, Some(true), "{}", result_text(&result));
        let v: Value = serde_json::from_str(&result_text(&result)).expect("post json");
        assert_eq!(v["posted"], true);

        let events = log.snapshot();
        let (received_id, params, origin) = events
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::CommandReceived {
                    method,
                    params,
                    origin,
                    ..
                } if method == "mcp.mu_mailbox_post" => {
                    Some((e.id, params.clone(), origin.clone()))
                }
                _ => None,
            })
            .expect("CommandReceived in the session log");
        assert_eq!(origin.transport, "mcp");
        // The journaled params are the RAW MCP arguments.
        assert_eq!(params["to_session_id"], "s-mcp");
        assert_eq!(params["subject"], "hello over mcp");
        let posted_id = events
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::MailboxMessagePosted { .. } => Some(e.id),
                _ => None,
            })
            .expect("MailboxMessagePosted in the session log");
        assert!(
            received_id < posted_id,
            "CommandReceived (id {received_id}) must precede the effect (id {posted_id})"
        );
        // Exactly one receipt, pairing the command, wrapping the original.
        let receipts: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.payload {
                EventPayload::CommandSucceeded {
                    command_event_id,
                    command,
                    ..
                } => Some((*command_event_id, command.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(receipts.len(), 1, "exactly one receipt: {events:?}");
        assert_eq!(receipts[0].0, received_id);
        assert_eq!(receipts[0].1.method, "mcp.mu_mailbox_post");
        // The daemon journal carries the (daemon-scoped) hello but NOT
        // the session-scoped post.
        let records = journal_records(&harness.journal_path);
        assert!(
            records.iter().any(|r| matches!(&r.payload,
                JournalPayload::CommandReceived { method, .. } if method == "mcp.mu_peer_hello")),
            "hello is a control-plane command"
        );
        assert!(
            !records.iter().any(|r| matches!(&r.payload,
                JournalPayload::CommandReceived { method, .. } if method == "mcp.mu_mailbox_post")),
            "the post belongs to the session pipeline"
        );
    }

    /// Fail closed (INV-2) at the MCP border: with the ingest seam
    /// broken, the tool call returns an error to the MCP caller, no
    /// effect happens, and nothing was journaled for it.
    #[tokio::test]
    async fn broken_journal_fails_closed_with_no_effect() {
        let harness = spawn_harness(open_registry()).await;
        let (input_tx, _input_rx) = mpsc::channel::<AgentInput>(4);
        let log = insert_session(&harness.sessions, "s-mcp", input_tx, harness._dir.path());
        let handle = peer_handle(&harness, "s-mcp", "s-peer").await;

        harness.control.poison_ingest_seam_for_tests();

        let result = call_tool(
            &harness,
            "mu_mailbox_post",
            json!({
                "to_session_id": "s-mcp",
                "peer_handle": handle,
                "from_daemon_id": harness.daemon_info.daemon_id(),
                "from_session_id": "s-peer",
                "kind": "note",
                "subject": "should never land",
                "body": null,
            }),
        )
        .await;
        assert_eq!(result.is_error, Some(true), "{}", result_text(&result));
        assert!(
            result_text(&result).contains("journal unavailable"),
            "error names the journal: {}",
            result_text(&result)
        );
        // No effect, and fail-closed means not even a CommandReceived.
        let events = log.snapshot();
        assert!(
            !events
                .iter()
                .any(|e| matches!(&e.payload, EventPayload::MailboxMessagePosted { .. })),
            "no effect may happen: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(&e.payload, EventPayload::CommandReceived { .. })),
            "rejected before any append: {events:?}"
        );
        let records = journal_records(&harness.journal_path);
        assert!(
            !records.iter().any(|r| matches!(&r.payload,
                JournalPayload::CommandReceived { method, .. } if method == "mcp.mu_mailbox_post")),
            "daemon journal must not carry the rejected post"
        );
    }

    /// With `[auth]` tokens configured the pipeline's gate applies to
    /// MCP connections too (pre-WP5 they bypassed auth entirely): the
    /// call is rejected `AUTH_REQUIRED` and the rejection is a
    /// journaled receipt (`CommandRejected { stage: AuthGate }`).
    #[tokio::test]
    async fn auth_gate_applies_to_mcp_tools_when_enforcing() {
        let harness = spawn_harness(enforcing_registry()).await;
        let result = call_tool(&harness, "mu_daemon_info", json!({})).await;
        assert_eq!(result.is_error, Some(true), "{}", result_text(&result));
        assert!(
            result_text(&result).contains("-32001"),
            "AUTH_REQUIRED surfaces to the MCP caller: {}",
            result_text(&result)
        );
        let records = journal_records(&harness.journal_path);
        let seq = records
            .iter()
            .find_map(|r| match &r.payload {
                JournalPayload::CommandReceived { method, .. }
                    if method == "mcp.mu_daemon_info" =>
                {
                    Some(r.seq)
                }
                _ => None,
            })
            .expect("rejected command is still journaled (border record)");
        assert!(
            records.iter().any(|r| matches!(&r.payload,
                JournalPayload::CommandRejected { command_seq, stage: RejectStage::AuthGate, .. }
                    if *command_seq == seq)),
            "auth rejection is a receipt: {records:?}"
        );
    }

    /// Unknown tools route through the pipeline too: METHOD_NOT_FOUND
    /// to the caller, `CommandRejected { stage: Routing }` on disk.
    #[tokio::test]
    async fn unknown_tool_rejected_with_routing_receipt() {
        let harness = spawn_harness(open_registry()).await;
        let result = call_tool(&harness, "no_such_tool", json!({})).await;
        assert_eq!(result.is_error, Some(true));
        assert!(
            result_text(&result).contains("unknown tool: no_such_tool"),
            "{}",
            result_text(&result)
        );
        let records = journal_records(&harness.journal_path);
        let seq = records
            .iter()
            .find_map(|r| match &r.payload {
                JournalPayload::CommandReceived { method, .. } if method == "mcp.no_such_tool" => {
                    Some(r.seq)
                }
                _ => None,
            })
            .expect("unknown tool still crosses the border journaled");
        assert!(
            records.iter().any(|r| matches!(&r.payload,
                JournalPayload::CommandRejected { command_seq, stage: RejectStage::Routing, .. }
                    if *command_seq == seq)),
            "routing rejection is a receipt: {records:?}"
        );
    }
}
