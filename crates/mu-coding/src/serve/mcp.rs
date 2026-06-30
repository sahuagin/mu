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
//! router (INV-8) on the lane registered for this connection's
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
//! **Response correlation (WP9):** rmcp may interleave tool calls on
//! one connection, so each invocation mints a daemon-unique synthetic
//! request id. The connection registers ONE outbound lane at accept
//! (spec mu-046 INV-11 — per-connection egress queues; no shared
//! broadcast ring, so other sessions' traffic can never evict this
//! connection's responses); a connection-level demux task
//! ([`demux_loop`]) drains the lane and routes Response envelopes by
//! request id to per-invocation oneshot waiters, registered BEFORE
//! ingest and removed at delivery. Notifications arriving on an MCP
//! lane — including broadcast (origin-less) envelopes, which reach
//! every lane — are dropped at the demux with a trace: the MCP tool
//! surface has no notification channel today. Failure modes: lane
//! closed (daemon shutting down) and lane poisoned (this connection
//! was disconnected as a slow consumer; the command journal and its
//! receipts are the source of truth).
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use rmcp::model::*;
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt};
use serde_json::{json, Map as JsonMap, Value};
use t4c::{HelpAiDoc, HelpAiProbeSource};
use tokio::net::UnixListener;
use tokio::sync::{oneshot, Mutex};
use tracing::{debug, info};

use mu_core::command_journal::Origin;
use mu_core::protocol::{Request, Response, JSONRPC_VERSION};
use mu_core::session_status::{ProviderSnapshot, SessionStatus, StatusInputs};
use mu_core::transport::{ConnectionLane, LaneTerminated, Router};

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
    outbound: Router,
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
            let origin = next_mcp_origin();
            // spec mu-046 WP9: ONE outbound lane per MCP connection,
            // registered at accept; the demux task drains it for the
            // connection's whole life, routing responses to their
            // invocation's waiter (module doc, "Response correlation").
            let lane = outbound.register(origin.clone());
            let demux = Arc::new(McpDemux::default());
            let demux_task = tokio::spawn(demux_loop(lane, Arc::clone(&demux)));
            let demux_for_cleanup = Arc::clone(&demux);
            let handler =
                MuMcpHandler::new(sessions, daemon_info, control, origin, auth_state, demux);
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
            // Connection over: stop the demux; dropping its lane
            // unregisters it from the router. abort() bypasses the
            // loop's own terminal-set + waiter-clear, so repeat that
            // cleanup here — any in-flight invocation future rmcp left
            // alive would otherwise await a waiter nobody will resolve.
            // Same order rule as demux_loop: reason first, then drop
            // the waiters.
            demux_task.abort();
            McpDemux::lock(&demux_for_cleanup.terminal)
                .get_or_insert_with(|| "MCP connection closed".to_string());
            McpDemux::lock(&demux_for_cleanup.waiters).clear();
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
    /// This connection's response demultiplexer (spec mu-046 WP9):
    /// the connection's single outbound lane is drained by
    /// [`demux_loop`]; each invocation registers a waiter here and
    /// receives its response by request id.
    demux: Arc<McpDemux>,
    /// This connection's border identity (`transport: "mcp"`).
    origin: Origin,
    /// This connection's auth state — the pipeline's gate reads it at
    /// processing time (module doc, "Auth").
    auth_state: AuthStateHandle,
    /// Whether THIS connection negotiated `experimental.mu.aiHelp` at
    /// `initialize` (the client advertised it; we mirrored it back). Set once
    /// during the handshake and read by [`Self::on_custom_request`] to gate
    /// `mu/aiHelp`: a client that never negotiated the feature sees the method
    /// as `METHOD_NOT_FOUND`, exactly as if it were unimplemented. Per
    /// connection (`Arc<AtomicBool>`), not process-wide.
    ai_help_negotiated: Arc<AtomicBool>,
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
        origin: Origin,
        auth_state: AuthStateHandle,
        demux: Arc<McpDemux>,
    ) -> Self {
        Self {
            sessions,
            daemon_info,
            control,
            demux,
            origin,
            auth_state,
            ai_help_negotiated: Arc::new(AtomicBool::new(false)),
            watch_tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn compute_session_status(&self, session_id: &str) -> Option<SessionStatus> {
        let log = self.sessions.event_log(session_id)?;
        let (provider_kind, model) = log.provider_info().unwrap_or_default();
        let usage = log.cumulative_usage();
        // mu-context-limits-wire: this pull path used to leave the context
        // fields unset, so it disagreed with the forwarder's push path.
        // Both now read the same recorded soft/hard limits and report the
        // fill (last call input) so a freshly-read resource matches the
        // last pushed status. See `mu_core::session_status` for the terms.
        let (context_soft_limit, context_hard_limit) = log
            .context_limits()
            .map_or((None, None), |(soft, hard, _max_output)| (Some(soft), hard));
        let (_, context_used_tokens) = log.live_usage();
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
            context_soft_limit,
            context_hard_limit,
            context_used_tokens,
        }))
    }

    /// The `initialize` response for this connection: the base [`Self::get_info`]
    /// capabilities, plus `experimental.mu.aiHelp: true` ONLY when `negotiated`
    /// (the client asked for it). Built by injecting into the base rather than
    /// duplicating it, so the server-info/instructions never drift.
    fn info_with_ai_help(&self, negotiated: bool) -> InitializeResult {
        let mut info = self.get_info();
        if negotiated {
            let mut mu = JsonMap::new();
            mu.insert("aiHelp".to_string(), Value::Bool(true));
            info.capabilities
                .experimental
                .get_or_insert_with(Default::default)
                .insert("mu".to_string(), mu);
        }
        info
    }
}

// ─── Experimental mu/aiHelp surface ─────────────────────────────────
//
// A negotiated, gated custom request that serves t4c-style `--help-ai`
// documentation over MCP. The wire feature flag rides in the experimental
// capabilities map as `experimental.mu.aiHelp` on BOTH sides of the
// handshake; the request itself is the custom JSON-RPC method `mu/aiHelp`
// with params `{ "path": [...] }`. The response reuses the `HelpAiDoc`
// superset fields for the addressed node, with its immediate `subcommands`
// trimmed to `children: [{name, summary}]` (no recursive grandchildren) so a
// consumer navigates the tree one level at a time.

/// The custom JSON-RPC method name for the AI-help surface.
const AI_HELP_METHOD: &str = "mu/aiHelp";

/// One registered external `--help-ai` producer the surface can probe. The
/// registry is static and small (a curated set of mu-adjacent agent tools);
/// `command` is the executable resolved on `PATH`.
struct AiHelpProducer {
    /// Path segment / display name (e.g. `agent`).
    name: &'static str,
    /// One-line, discovery-facing summary for the registry listing and for the
    /// shallow fallback node when the producer can't be probed.
    summary: &'static str,
    /// The executable to invoke for `<command> <scope...> --help-ai --json`.
    command: &'static str,
}

/// The registered producers. Kept deliberately minimal; `agent` is the
/// canonical mu-adjacent tool that already speaks `--help-ai --json`.
static AI_HELP_PRODUCERS: &[AiHelpProducer] = &[AiHelpProducer {
    name: "agent",
    summary: "agent memory / tasks / metrics CLI (agent memory, agent task, …)",
    command: "agent",
}];

/// True iff `caps.experimental.mu.aiHelp == true` — the client opting into the
/// experimental surface during `initialize`. A bare `experimental.mu` without
/// `aiHelp` (or a non-`true` value) does NOT negotiate the feature.
fn client_requested_ai_help(caps: &ClientCapabilities) -> bool {
    caps.experimental
        .as_ref()
        .and_then(|e| e.get("mu"))
        .and_then(|mu| mu.get("aiHelp"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Parse the `mu/aiHelp` params into a scope path. Absent params or an absent
/// `path` resolve to the root (`[]`); a present `path` MUST be an array of
/// strings, else `INVALID_PARAMS`.
fn parse_ai_help_path(params: Option<&Value>) -> Result<Vec<String>, McpError> {
    let Some(path_val) = params.and_then(|p| p.get("path")) else {
        return Ok(Vec::new());
    };
    let arr = path_val
        .as_array()
        .ok_or_else(|| McpError::invalid_params("`path` must be an array of strings", None))?;
    arr.iter()
        .map(|seg| {
            seg.as_str()
                .map(str::to_string)
                .ok_or_else(|| McpError::invalid_params("`path` segments must be strings", None))
        })
        .collect()
}

/// Resolve a scope path to its trimmed help node.
///
/// - `[]` → a synthetic root node listing the registered producers as
///   `children: [{name, summary}]` (no invented own scalar content).
/// - `[producer, scope...]` → the producer's `<command> <scope...> --help-ai
///   --json` doc, trimmed one level. A registered producer that is absent or
///   emits non-conforming output degrades to a shallow `{name, summary}` node
///   rather than failing the request.
/// - an unregistered top-level producer → `INVALID_PARAMS`.
fn resolve_ai_help(path: &[String]) -> Result<Value, McpError> {
    let Some((producer_name, scope)) = path.split_first() else {
        return Ok(ai_help_root());
    };
    let Some(producer) = AI_HELP_PRODUCERS.iter().find(|p| p.name == producer_name) else {
        return Err(McpError::invalid_params(
            format!("unknown mu/aiHelp producer: {producer_name}"),
            None,
        ));
    };
    match HelpAiProbeSource::probe_help_ai(producer.command, scope) {
        Ok(doc) => Ok(trim_help_node(&doc)),
        Err(_) => Ok(json!({ "name": producer.name, "summary": producer.summary })),
    }
}

/// The synthetic root node: the registry surface itself. Carries only its own
/// identity plus the producer list — no fabricated args/usage/output_schema.
fn ai_help_root() -> Value {
    let children: Vec<Value> = AI_HELP_PRODUCERS
        .iter()
        .map(|p| json!({ "name": p.name, "summary": p.summary }))
        .collect();
    json!({
        "name": "mu",
        "summary": "mu MCP AI-help surface — children are registered --help-ai producers",
        "children": children,
    })
}

/// Trim one [`HelpAiDoc`] node for the wire: keep this node's OWN rich fields
/// (name, summary, args, usage, output_schema, …) but replace its recursive
/// `subcommands` with a shallow `children: [{name, summary}]` list, dropping
/// grandchildren. Navigation is one level per request.
fn trim_help_node(doc: &HelpAiDoc) -> Value {
    let mut v = serde_json::to_value(doc).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut v {
        map.remove("subcommands");
        if !doc.subcommands.is_empty() {
            let children: Vec<Value> = doc
                .subcommands
                .iter()
                .map(|s| json!({ "name": s.name, "summary": s.summary }))
                .collect();
            map.insert("children".to_string(), Value::Array(children));
        }
    }
    v
}

impl ServerHandler for MuMcpHandler {
    fn get_info(&self) -> InitializeResult {
        // The BASE, non-negotiated server capabilities. Deliberately carries NO
        // `experimental.mu.aiHelp`: a client that does not ask for the feature
        // gets no unilateral advertisement of it. The negotiated superset is
        // assembled in [`Self::initialize`] from this base — see
        // [`Self::info_with_ai_help`].
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

    /// Override the handshake so the experimental `mu/aiHelp` feature is
    /// *negotiated*, not asserted: we advertise it back ONLY when the client
    /// advertised `experimental.mu.aiHelp == true`. The negotiation state is
    /// remembered on this connection and gates [`Self::on_custom_request`].
    /// (Replicates rmcp's default `set_peer_info` step so peer info is still
    /// recorded.)
    fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<InitializeResult, McpError>> + Send + '_ {
        let negotiated = client_requested_ai_help(&request.capabilities);
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }
        self.ai_help_negotiated.store(negotiated, Ordering::Relaxed);
        let info = self.info_with_ai_help(negotiated);
        std::future::ready(Ok(info))
    }

    /// Handle the experimental `mu/aiHelp` custom request (and nothing else —
    /// any other method keeps rmcp's `METHOD_NOT_FOUND` default). Gated on this
    /// connection having negotiated the feature at `initialize`; an
    /// un-negotiated caller sees `METHOD_NOT_FOUND` (-32601) as if the method
    /// did not exist. Params are `{ "path": [...] }`; the resolver returns the
    /// trimmed help node for that scope (root registry for `[]`, a producer's
    /// scoped `--help-ai` doc otherwise).
    fn on_custom_request(
        &self,
        request: CustomRequest,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CustomResult, McpError>> + Send + '_ {
        async move {
            if request.method != AI_HELP_METHOD {
                return Err(McpError::new(
                    ErrorCode::METHOD_NOT_FOUND,
                    request.method,
                    None,
                ));
            }
            if !self.ai_help_negotiated.load(Ordering::Relaxed) {
                // Not negotiated on this connection: behave as unimplemented.
                return Err(McpError::new(
                    ErrorCode::METHOD_NOT_FOUND,
                    AI_HELP_METHOD,
                    None,
                ));
            }
            let path = parse_ai_help_path(request.params.as_ref())?;
            let node = resolve_ai_help(&path)?;
            Ok(CustomResult::new(node))
        }
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
    /// response from the connection's demux (spec mu-046 WP9) and
    /// translate it back into the MCP tool-result shape
    /// (`Ok(result)` → success content, `Err("code: message")` →
    /// error content, exactly the pre-WP5 strings for handler
    /// outcomes).
    async fn dispatch_tool(&self, name: &str, arguments: Value) -> Result<Value, String> {
        let request_id = next_mcp_request_id();
        let id_key = request_id
            .as_str()
            .expect("minted MCP request ids are strings")
            .to_string();
        let request = Request {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: request_id,
            method: format!("{MCP_METHOD_PREFIX}{name}"),
            params: arguments,
        };
        // Register the waiter BEFORE ingesting so the response cannot
        // slip past between enqueue and registration (the same race
        // the pre-WP9 subscribe-before-ingest closed).
        let (tx, rx) = oneshot::channel();
        self.demux.register_waiter(id_key.clone(), tx);
        // If the demux has already terminated (lane closed or
        // poisoned), nothing will ever deliver: bail with its reason.
        // A termination AFTER this check drops our waiter, resolving
        // `rx` below with the same reason.
        if let Some(reason) = self.demux.terminal_reason() {
            self.demux.remove_waiter(&id_key);
            return Err(reason);
        }
        if let Some(response) = pipeline::ingest(
            &self.control,
            request,
            self.origin.clone(),
            &self.auth_state,
        ) {
            // Immediate reject (journal unavailable / daemon shutting
            // down): nothing was enqueued, nothing will arrive on the
            // lane — fail closed to the MCP caller.
            self.demux.remove_waiter(&id_key);
            return extract_result(response);
        }
        match rx.await {
            Ok(response) => extract_result(response),
            // The demux exited — dropping every pending waiter —
            // before this command's response arrived: the lane closed
            // (daemon shutdown) or was poisoned (slow-consumer
            // disconnect). Either way the reason explains it; the
            // command was journaled and may have EXECUTED, so callers
            // must check state before retrying non-idempotent tools.
            Err(_) => Err(self.demux.terminal_reason().unwrap_or_else(|| {
                "MCP outbound demux terminated before the response arrived".to_string()
            })),
        }
    }
}

/// Per-connection response demultiplexer (spec mu-046 WP9): the
/// invocation-side half of the connection's single outbound lane.
/// [`demux_loop`] is the consumer half.
#[derive(Default)]
struct McpDemux {
    /// Synthetic request id → the invocation's response waiter.
    /// Registered before ingest; removed at delivery, or by the
    /// invocation itself on an immediate ingest reject.
    waiters: std::sync::Mutex<HashMap<String, oneshot::Sender<Response<Value>>>>,
    /// Why [`demux_loop`] exited — set BEFORE the pending waiters are
    /// dropped, so an invocation woken by its dropped waiter can read
    /// the reason.
    terminal: std::sync::Mutex<Option<String>>,
}

impl McpDemux {
    /// Lock recovering from poisoning: the guarded sections are
    /// straight-line map operations, and wedging the connection over
    /// a poisoned lock helps no one.
    fn lock<'a, T>(mutex: &'a std::sync::Mutex<T>) -> std::sync::MutexGuard<'a, T> {
        match mutex.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn register_waiter(&self, id: String, tx: oneshot::Sender<Response<Value>>) {
        Self::lock(&self.waiters).insert(id, tx);
    }

    fn remove_waiter(&self, id: &str) -> Option<oneshot::Sender<Response<Value>>> {
        Self::lock(&self.waiters).remove(id)
    }

    fn terminal_reason(&self) -> Option<String> {
        Self::lock(&self.terminal).clone()
    }
}

/// Drain one MCP connection's outbound lane for the connection's
/// whole life (spec mu-046 WP9): deliver Response envelopes to their
/// invocation's waiter by request id; DROP notifications at trace —
/// the MCP tool surface has no notification channel today, and
/// broadcast (origin-less) envelopes land on MCP lanes too (module
/// doc, "Response correlation"). On lane termination, record why and
/// drop every pending waiter so in-flight invocations resolve with
/// the reason instead of hanging.
async fn demux_loop(lane: ConnectionLane, demux: Arc<McpDemux>) {
    let reason = loop {
        match lane.recv().await {
            Ok(envelope) => {
                if envelope.item.0.get("method").is_some() {
                    tracing::trace!(
                        origin = ?lane.origin(),
                        method = ?envelope.item.0.get("method"),
                        "dropping notification at MCP demux (no notification channel \
                         on the tool surface)"
                    );
                    continue;
                }
                let Some(id) = envelope.request_id.as_ref().and_then(Value::as_str) else {
                    tracing::trace!(
                        origin = ?lane.origin(),
                        "response envelope without a string request id at MCP demux"
                    );
                    continue;
                };
                let Some(tx) = demux.remove_waiter(id) else {
                    tracing::trace!(
                        origin = ?lane.origin(),
                        request_id = %id,
                        "response with no registered waiter at MCP demux"
                    );
                    continue;
                };
                match serde_json::from_value::<Response<Value>>(envelope.item.0) {
                    // A send failure means the invocation gave up
                    // (rmcp connection torn down mid-call); fine.
                    Ok(response) => {
                        let _ = tx.send(response);
                    }
                    // Cannot happen — the pipeline serializes a real
                    // Response into the envelope. Dropping `tx`
                    // resolves the invocation with the generic
                    // demux-terminated error.
                    Err(err) => {
                        tracing::warn!(%err, "malformed response envelope at MCP demux");
                    }
                }
            }
            // Every Router producer dropped — daemon shutting down.
            Err(LaneTerminated::Closed) => {
                break "daemon outbound closed (shutting down)".to_string();
            }
            // Slow-consumer disconnect (spec mu-046 INV-11). Do NOT
            // advise a blind retry: pending commands were journaled
            // and may have EXECUTED — only their result envelopes are
            // gone.
            Err(LaneTerminated::SlowConsumer { dropped_ephemeral }) => {
                tracing::error!(
                    origin = ?lane.origin(),
                    dropped_ephemeral,
                    "MCP connection disconnected as a slow consumer (outbound lane \
                     overflowed; spec mu-046 INV-11)"
                );
                break "MCP connection disconnected as a slow consumer (outbound lane \
                       overflowed); any in-flight tool RESULT was lost, but the calls \
                       themselves may have executed — the command journal and its \
                       receipts are the source of truth; check daemon state before \
                       retrying anything non-idempotent"
                    .to_string();
            }
        }
    };
    // Order matters: reason first, then drop the waiters, so an
    // invocation woken by its dropped waiter reads Some(reason).
    *McpDemux::lock(&demux.terminal) = Some(reason);
    McpDemux::lock(&demux.waiters).clear();
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
        /// Producer handle on the daemon-wide outbound router (WP9) —
        /// lets tests inject broadcast envelopes at MCP lanes.
        outbound: Router,
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

    /// A running server over a tempdir unix socket, WITHOUT a connected
    /// client. The handshake-level tests connect their own clients (with a
    /// chosen [`ClientInfo`]) so they can inspect the negotiated
    /// `initialize` response; [`spawn_harness`] layers a `()` client on top
    /// for the WP5 tool round-trip tests.
    struct ServerHandle {
        _dir: tempfile::TempDir,
        journal_path: std::path::PathBuf,
        sessions: Sessions,
        daemon_info: DaemonInfo,
        control: Arc<ControlPlane>,
        outbound: Router,
        socket_path: std::path::PathBuf,
        _server: tokio::task::JoinHandle<()>,
    }

    fn short_socket_tempdir() -> tempfile::TempDir {
        // Unix-domain socket paths are capped by sockaddr_un.sun_path
        // (104 bytes on FreeBSD). Long jj workspace names can leak into
        // tempfile defaults on some runners, so force these MCP socket tests
        // under a short parent when available.
        tempfile::Builder::new()
            .prefix("mu-mcp-")
            .tempdir_in("/tmp")
            .or_else(|_| tempfile::Builder::new().prefix("mu-mcp-").tempdir())
            .expect("tempdir")
    }

    /// Stand up the adapter stack (journal → control plane → MCP socket) and
    /// wait until the socket accepts. No client is connected.
    async fn spawn_server_socket(auth_registry: Arc<AuthRegistry>) -> ServerHandle {
        let dir = short_socket_tempdir();
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
        let outbound = Router::new();
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
            let outbound = outbound.clone();
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
        ServerHandle {
            _dir: dir,
            journal_path,
            sessions,
            daemon_info,
            control,
            outbound,
            socket_path,
            _server: server,
        }
    }

    /// Full adapter stack: [`spawn_server_socket`] plus a connected `()`
    /// rmcp client — the same trivial client shape the WP5 round-trip tests
    /// drive.
    async fn spawn_harness(auth_registry: Arc<AuthRegistry>) -> Harness {
        let server = spawn_server_socket(auth_registry).await;
        let stream = tokio::net::UnixStream::connect(&server.socket_path)
            .await
            .expect("connect mcp socket");
        let client = ().serve(stream.into_split()).await.expect("mcp client handshake");
        Harness {
            _dir: server._dir,
            journal_path: server.journal_path,
            sessions: server.sessions,
            daemon_info: server.daemon_info,
            control: server.control,
            outbound: server.outbound,
            client,
            _server: server._server,
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
                live_context_soft_limit: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
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

    // ─── spec mu-046 WP9: per-connection lane + demux ───────────────

    /// Two interleaved invocations on ONE connection demux correctly:
    /// each registers its own waiter keyed by its synthetic request
    /// id, and each resolves with its own result.
    #[tokio::test]
    async fn interleaved_invocations_demux_by_request_id() {
        let harness = spawn_harness(open_registry()).await;
        let (first, second) = tokio::join!(
            call_tool(&harness, "mu_daemon_info", json!({})),
            call_tool(&harness, "mu_daemon_info", json!({})),
        );
        for result in [first, second] {
            assert_ne!(result.is_error, Some(true), "{}", result_text(&result));
            let v: Value = serde_json::from_str(&result_text(&result)).expect("result json");
            assert_eq!(v["daemon_id"], harness.daemon_info.daemon_id());
        }
    }

    /// Broadcast (origin-less) envelopes reach MCP lanes and are
    /// dropped at the demux — the tool surface has no notification
    /// channel — without confusing response correlation: a tool call
    /// issued after the broadcast still round-trips.
    #[tokio::test]
    async fn broadcast_notifications_dropped_at_demux_without_breaking_calls() {
        let harness = spawn_harness(open_registry()).await;
        let broadcast = mu_core::transport::NotificationWriter::broadcast(harness.outbound.clone());
        broadcast
            .emit("daemon.announce", json!({"msg": "hello"}))
            .await
            .expect("broadcast emit");
        let result = call_tool(&harness, "mu_daemon_info", json!({})).await;
        assert_ne!(result.is_error, Some(true), "{}", result_text(&result));
    }

    /// Slow-consumer disconnect at the demux (spec mu-046 INV-11): a
    /// poisoned lane terminates the demux loop, which records the
    /// reason and drops every pending waiter — in-flight invocations
    /// resolve with the journal-is-source-of-truth error instead of
    /// hanging.
    #[tokio::test]
    async fn poisoned_lane_fails_pending_invocations_with_slow_consumer_error() {
        use mu_core::transport::{Outbound, OutboundEnvelope, LANE_HARD_CAP};

        let router = Router::new();
        let origin = next_mcp_origin();
        let lane = router.register(origin.clone());
        let demux = Arc::new(McpDemux::default());
        let (tx, rx) = oneshot::channel();
        demux.register_waiter("mcp-pending".to_string(), tx);

        // Poison the (not-yet-consumed) lane with a durable-only
        // flood, THEN run the demux: its first recv observes the
        // poison and it exits through the slow-consumer arm.
        for n in 0..=LANE_HARD_CAP {
            router.send(OutboundEnvelope {
                origin: Some(origin.clone()),
                request_id: Some(json!(format!("other-{n}"))),
                command_seq: None,
                item: Outbound(json!({"jsonrpc": "2.0", "id": n, "result": {}})),
            });
        }
        demux_loop(lane, Arc::clone(&demux)).await;

        assert!(
            rx.await.is_err(),
            "pending waiter must be dropped when the demux exits"
        );
        let reason = demux.terminal_reason().expect("terminal reason recorded");
        assert!(
            reason.contains("slow consumer"),
            "reason names the policy: {reason}"
        );
        assert!(
            reason.contains("source of truth"),
            "reason points at the journal: {reason}"
        );
    }

    /// A negotiated client gets the `mu/aiHelp` surface: the handshake mirrors
    /// `experimental.mu.aiHelp` (the client advertises it), and `fetch_ai_help`
    /// round-trips a scoped node. Root (`[]`) lists the registered producers —
    /// `agent` is one. A second fetch of the same scope is served from the
    /// per-connection cache, exercising the client plumbing end to end.
    #[tokio::test]
    async fn ai_help_round_trips_for_a_negotiated_client() {
        use super::super::mcp_client::{fetch_ai_help, MuMcpClient};
        let server = spawn_server_socket(open_registry()).await;
        let stream = tokio::net::UnixStream::connect(&server.socket_path)
            .await
            .expect("connect mcp socket");
        let peer = MuMcpClient::default()
            .serve(stream.into_split())
            .await
            .expect("mu client handshake (negotiated)");

        let root = fetch_ai_help(&peer, &[]).await.expect("fetch root ai-help");
        let producers: Vec<&str> = root["children"]
            .as_array()
            .expect("root carries a children array")
            .iter()
            .filter_map(|c| c["name"].as_str())
            .collect();
        assert!(producers.contains(&"agent"), "root children: {root}");

        // Second fetch of the same scope: served from the connection cache.
        let cached = fetch_ai_help(&peer, &[]).await.expect("cached fetch");
        assert_eq!(root, cached, "cached node must match the first fetch");
    }

    /// A client that did NOT negotiate `experimental.mu.aiHelp` (the trivial
    /// `()` handler advertises nothing) sees `mu/aiHelp` as METHOD_NOT_FOUND
    /// (-32601) — indistinguishable from an unimplemented method. The feature
    /// is invisible to a vanilla client.
    #[tokio::test]
    async fn ai_help_is_method_not_found_without_negotiation() {
        use rmcp::model::{ClientRequest, CustomRequest};
        let server = spawn_server_socket(open_registry()).await;
        let stream = tokio::net::UnixStream::connect(&server.socket_path)
            .await
            .expect("connect mcp socket");
        let client = ().serve(stream.into_split()).await.expect("client handshake");
        let resp = client
            .send_request(ClientRequest::CustomRequest(CustomRequest::new(
                "mu/aiHelp",
                Some(json!({ "path": [] })),
            )))
            .await;
        let err = resp.expect_err("un-negotiated mu/aiHelp must be rejected");
        let shown = format!("{err}").to_lowercase();
        assert!(
            shown.contains("-32601") || shown.contains("method not found"),
            "expected METHOD_NOT_FOUND, got: {shown}"
        );
    }
}
