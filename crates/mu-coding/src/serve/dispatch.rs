//! JSON-RPC method dispatch router for `mu serve`.
//!
//! spec mu-046 (WP3): requests no longer arrive here straight off the
//! transport — they cross the ingest pipeline first
//! ([`super::pipeline`]: journaled, sequenced, single-writer). The
//! pipeline's control-plane consumer applies [`auth_gate`] and then
//! routes via [`dispatch_inner`], whose match arms are the unchanged
//! per-method handlers.

use std::sync::Arc;

use serde_json::{json, Value};

use mu_core::agent::Tool;
use mu_core::protocol::{
    AskSessionRequest, AuthInitiateRequest, AuthOfferRequest, CancelOutstandingRequest,
    CancelSessionRequest, CapabilitiesDiscoverRequest, CloseSessionRequest, CreateSessionRequest,
    DaemonListRoutesRequest, DaemonOutstandingCallsRequest, DaemonStatsRequest,
    DaemonUsageHistoryRequest, DelegateSessionRequest, MailboxConsumeRequest, MailboxListRequest,
    MailboxPostRequest, MailboxReadRequest, PeerHelloRequest, PingRequest, Request,
    RespondToInputRequiredRequest, Response, ResumeSessionRequest, ScheduleWakeupRequest,
    SessionEventsRequest, SessionListRequest, SessionStatsRequest, SetRouteRequest,
    SpawnWorkerRequest, StartAutonomousRequest, JSONRPC_VERSION,
};
use mu_core::skill::loader::LoadedSkill;
use mu_core::transport::{codes, err_response, ok_response, NotificationWriter};

use super::auth::{AuthRegistry, AuthState, AuthStateHandle};
use super::daemon_info::DaemonInfo;
use super::discovery::SessionDiscovery;
use super::factory::ProviderFactory;
use super::handlers::auth::{handle_auth_initiate, handle_auth_offer};
use super::handlers::capabilities::handle_capabilities_discover;
use super::handlers::{daemon::*, mailbox::*, session::*};
use super::sessions::Sessions;

// mu-7rk (mu-yox): dispatch carries two extra daemon-wide handles:
// a shared `AuthRegistry` (constructed once at serve start from
// `[auth]` config) and a per-connection `AuthStateHandle`, both
// bundled into `DispatchCtx`.
//
// mu-fnn (mu-7rk-c): the connect-time auth gate. Methods are split
// into a pre-auth allowlist (`peer.auth_*`) and the protected
// remainder. The gate enforces:
//
//   - `AuthState::Authenticated { .. }` → all methods proceed.
//   - `AuthState::Unauthenticated` → only pre-auth methods proceed;
//     everything else is rejected with `auth_required`.
//   - `AuthState::Denied { .. }` → terminal; ALL methods (including
//     pre-auth retries) are rejected with `auth_denied`. The transport
//     close on denial lands in mu-1p6 (mu-7rk-d), separate.
//
// `peer.auth_response` is reserved (mu-vha) but not yet routed; it is
// listed in the pre-auth allowlist for future-proofing — the
// dispatcher still returns `METHOD_NOT_FOUND` for it until mu-oeo
// (mu-7rk-g) wires up multi-step state. The order matters: gate first,
// route second — otherwise an unauthenticated `METHOD_NOT_FOUND`
// reveals routing surface.

/// spec mu-046 WP5: namespace prefix under which MCP tool invocations
/// enter the pipeline — tool `<name>` becomes wire method
/// `mcp.<name>`. The spec's stated default; no MCP tool maps onto a
/// native wire method name, even where the shapes are close (one rule,
/// uniformly applied — see the `serve/mcp.rs` module doc).
pub(crate) const MCP_METHOD_PREFIX: &str = "mcp.";

/// `mcp.mu_mailbox_post` — the one session-scoped MCP method (it
/// addresses a target session via `to_session_id`, mirroring
/// `mailbox.post`). Referenced by `pipeline::classify`, which keeps the
/// mcp.* scope table aligned with the native methods'.
pub(crate) const MCP_MAILBOX_POST_METHOD: &str = "mcp.mu_mailbox_post";

/// Methods callable without an `Authenticated` `AuthState`. Anything
/// outside this list requires the gate to pass. Note `mcp.*` methods
/// are deliberately absent: the MCP surface is gated exactly like the
/// native one (spec mu-046 INV-7 — no side doors).
const PRE_AUTH_METHODS: &[&str] = &[
    AuthOfferRequest::METHOD,
    AuthInitiateRequest::METHOD,
    // peer.auth_response is reserved in the protocol (mu-vha) but the
    // dispatcher doesn't route it until mu-oeo (mu-7rk-g). Listed here
    // so when routing is added, callers don't need to re-auth first.
    "peer.auth_response",
];

/// Daemon-wide handles threaded to every dispatched request — bundled so
/// `dispatch_inner` takes the per-request `(request, notif)` plus one context
/// struct rather than ten positional args. Built per command by the pipeline
/// consumer (the handles are cheap `Arc`/clone-able).
pub struct DispatchCtx {
    pub sessions: Sessions,
    pub factory: ProviderFactory,
    pub tools: Arc<Vec<Arc<dyn Tool>>>,
    pub skills: Arc<Vec<LoadedSkill>>,
    pub daemon_info: DaemonInfo,
    pub discovery: Arc<dyn SessionDiscovery>,
    pub auth_registry: Arc<AuthRegistry>,
    pub auth_state: AuthStateHandle,
}

/// The mu-fnn enforcement gate, extracted (spec mu-046 WP3) so the
/// pipeline's control-plane consumer can apply it before routing —
/// gate first, route second — and journal the rejection as a
/// `CommandRejected { stage: AuthGate }` receipt. `Err((code,
/// message))` is the rejection the caller turns into both the receipt
/// and the wire error response.
///
/// Snapshot the AuthState (lock + clone + drop) so nothing downstream
/// holds a Mutex across .await points. A poisoned lock fails closed:
/// snapshot becomes a synthetic `Denied { MalformedExchange }` and
/// every method is rejected.
pub(crate) fn auth_gate(auth_state: &AuthStateHandle, method: &str) -> Result<(), (i32, String)> {
    let state_snapshot: AuthState = match auth_state.lock() {
        Ok(s) => s.clone(),
        Err(_poisoned) => AuthState::Denied {
            code: mu_core::protocol::AuthDenialCode::MalformedExchange,
        },
    };
    match &state_snapshot {
        AuthState::Authenticated { .. } => Ok(()),
        AuthState::Unauthenticated => {
            if PRE_AUTH_METHODS.contains(&method) {
                Ok(())
            } else {
                Err((
                    codes::AUTH_REQUIRED,
                    format!("method `{method}` requires an authenticated connection"),
                ))
            }
        }
        AuthState::Denied { code } => {
            // Denied is terminal — every method (including auth
            // retries) is rejected until reconnect.
            Err((
                codes::AUTH_DENIED,
                format!("connection auth denied (code={code:?}); reconnect required"),
            ))
        }
    }
}

/// Method-routing core: the per-method match, with NO auth gate — the
/// caller (the pipeline consumer) has already run [`auth_gate`]. The
/// arms are the pre-mu-046 `dispatch()` arms, byte-identical; only the
/// entry path around them changed.
///
/// `ask_ticket` (spec mu-046 WP4) is the receipt ticket the pipeline
/// minted when this command's `CommandReceived` landed in the
/// session's own event log. Only the `ask_session` arm consumes it —
/// the handler threads it into `AgentInput::UserMessage` so the
/// turn's terminal receipt pairs with the right `CommandReceived`.
/// `Some` only when the pipeline routed the command to a session log
/// AND the method is accept-async (`ask_session`); `None` otherwise.
pub(crate) async fn dispatch_inner(
    request: Request<Value>,
    notif: NotificationWriter,
    ctx: DispatchCtx,
    ask_ticket: Option<mu_core::command_journal::CommandTicket>,
) -> Response<Value> {
    let DispatchCtx {
        sessions,
        factory,
        tools,
        skills,
        daemon_info,
        discovery,
        auth_registry,
        auth_state,
    } = ctx;
    let method = request.method.as_str();
    match method {
        PingRequest::METHOD => handle_ping(request),
        // mu-kex4.6.4: in-process Layer-1 `t4c find` over RPC — rank the
        // session's permission-attenuated manifest (tools + skills) by intent.
        CapabilitiesDiscoverRequest::METHOD => {
            handle_capabilities_discover(request, sessions, tools, skills)
        }
        // mu-7rk (mu-yox): connect-time SASL-shaped auth handshake.
        AuthOfferRequest::METHOD => handle_auth_offer(request, &auth_registry),
        AuthInitiateRequest::METHOD => handle_auth_initiate(request, &auth_registry, &auth_state),
        CreateSessionRequest::METHOD => handle_create_session(
            request,
            notif,
            sessions,
            factory,
            tools,
            skills,
            daemon_info.clone(),
        ),
        DelegateSessionRequest::METHOD => handle_delegate_session(
            request,
            notif,
            sessions,
            factory,
            tools,
            skills,
            daemon_info.clone(),
        ),
        // mu-mh4: strict fork-at-tail resume.
        ResumeSessionRequest::METHOD => handle_resume_session(
            request,
            notif,
            sessions,
            factory,
            tools,
            skills,
            daemon_info.clone(),
        ),
        AskSessionRequest::METHOD => handle_ask_session(request, sessions, ask_ticket).await,
        CancelSessionRequest::METHOD => handle_cancel_session(request, sessions).await,
        CancelOutstandingRequest::METHOD => handle_cancel_outstanding(request, sessions).await,
        CloseSessionRequest::METHOD => handle_close_session(request, sessions),
        SessionStatsRequest::METHOD => handle_session_stats(request, sessions),
        SessionListRequest::METHOD => handle_session_list(request, discovery).await,
        SessionEventsRequest::METHOD => handle_session_events(request, sessions),
        DaemonStatsRequest::METHOD => handle_daemon_stats(request, sessions, daemon_info),
        DaemonUsageHistoryRequest::METHOD => handle_daemon_usage_history(request, sessions),
        DaemonOutstandingCallsRequest::METHOD => handle_daemon_outstanding_calls(request, sessions),
        // mu-lho (mu-037 Phase 1): peer-discovery + mailbox.
        PeerHelloRequest::METHOD => handle_peer_hello(request, sessions, daemon_info.clone()),
        MailboxPostRequest::METHOD => {
            handle_mailbox_post(request, sessions, notif.clone(), daemon_info.clone()).await
        }
        MailboxListRequest::METHOD => handle_mailbox_list(request, sessions),
        MailboxReadRequest::METHOD => handle_mailbox_read(request, sessions),
        MailboxConsumeRequest::METHOD => handle_mailbox_consume(request, sessions),
        // mu-036: session.start_autonomous (Phase B, mu-3ao) and
        // session.schedule_wakeup (Phase C, mu-7zn) are wired into the
        // agent loop. Both enqueue an AgentInput into the session's
        // input channel.
        StartAutonomousRequest::METHOD => handle_start_autonomous(request, sessions).await,
        ScheduleWakeupRequest::METHOD => handle_schedule_wakeup(request, sessions).await,
        RespondToInputRequiredRequest::METHOD => {
            handle_respond_to_input_required(request, sessions)
        }
        SetRouteRequest::METHOD => {
            handle_set_route(request, sessions, factory, daemon_info.clone()).await
        }
        DaemonListRoutesRequest::METHOD => handle_list_routes(request, daemon_info),
        SpawnWorkerRequest::METHOD => {
            handle_spawn_worker(request, sessions, daemon_info.clone()).await
        }
        other => {
            // spec mu-046 WP5: `mcp.<tool>` commands produced by the MCP
            // adapter route to the tool table below — same consumer, same
            // gate, same receipts as native methods.
            let mcp_tool = other.strip_prefix(MCP_METHOD_PREFIX).map(str::to_string);
            match mcp_tool {
                Some(tool) => dispatch_mcp_tool(&tool, request, notif, sessions, daemon_info).await,
                None => err_response(
                    request.id,
                    codes::METHOD_NOT_FOUND,
                    format!("unknown method: {}", request.method),
                ),
            }
        }
    }
}

// ─── spec mu-046 WP5: MCP tool table ────────────────────────────────
//
// The MCP adapter (`serve/mcp.rs`) journals each tool invocation's RAW
// MCP arguments as the command params — the faithful border record —
// and the consumer routes it here, where the arguments are translated
// into the native wire shapes and the SAME handlers the native methods
// route to are invoked. These bodies are the pre-WP5
// `serve/mcp.rs::dispatch_tool` arms, moved verbatim — handler logic
// unchanged, only the entry path. Translation failures (missing or
// mistyped fields) are `INVALID_PARAMS`, which the pipeline receipts as
// `CommandRejected { Validation }`; unknown tools are
// `METHOD_NOT_FOUND` → `CommandRejected { Routing }`.

/// Route one `mcp.<tool>` command. `request.params` are the raw MCP
/// tool arguments; `request.id` is the adapter's synthetic id, echoed
/// on the response so the adapter can correlate the outbound envelope.
async fn dispatch_mcp_tool(
    tool: &str,
    request: Request<Value>,
    notif: NotificationWriter,
    sessions: Sessions,
    daemon_info: DaemonInfo,
) -> Response<Value> {
    let id = request.id.clone();
    let args = request.params;
    let outcome: Result<Response<Value>, String> = match tool {
        "mu_daemon_info" => Ok(ok_response(
            id.clone(),
            json!({
                "daemon_id": daemon_info.daemon_id(),
                "version": daemon_info.version(),
                "session_count": sessions.snapshot_for_listing().len(),
            }),
        )),
        "mu_peer_hello" => peer_hello_params(&args).map(|rpc_params| {
            handle_peer_hello(
                native_request(id.clone(), PeerHelloRequest::METHOD, rpc_params),
                sessions.clone(),
                daemon_info.clone(),
            )
        }),
        "mu_mailbox_post" => match mailbox_post_params(&args) {
            Ok(rpc_params) => Ok(handle_mailbox_post(
                native_request(id.clone(), MailboxPostRequest::METHOD, rpc_params),
                sessions.clone(),
                notif.clone(),
                daemon_info.clone(),
            )
            .await),
            Err(msg) => Err(msg),
        },
        "mu_mailbox_list" => mailbox_list_params(&args).map(|rpc_params| {
            handle_mailbox_list(
                native_request(id.clone(), MailboxListRequest::METHOD, rpc_params),
                sessions.clone(),
            )
        }),
        "mu_mailbox_read" => mailbox_read_params(&args).map(|rpc_params| {
            handle_mailbox_read(
                native_request(id.clone(), MailboxReadRequest::METHOD, rpc_params),
                sessions.clone(),
            )
        }),
        "mu_mailbox_consume" => mailbox_consume_params(&args).map(|rpc_params| {
            handle_mailbox_consume(
                native_request(id.clone(), MailboxConsumeRequest::METHOD, rpc_params),
                sessions.clone(),
            )
        }),
        other => {
            return err_response(
                id,
                codes::METHOD_NOT_FOUND,
                format!("unknown tool: {other}"),
            )
        }
    };
    match outcome {
        Ok(response) => response,
        Err(msg) => err_response(id, codes::INVALID_PARAMS, msg),
    }
}

/// Build the native-shaped request a handler parses. The method string
/// is the native one purely for handler-side legibility (handlers parse
/// params and echo `id`; they never read `method`) — the journaled
/// command keeps its `mcp.<tool>` name.
fn native_request(id: Value, method: &str, params: Value) -> Request<Value> {
    Request {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id,
        method: method.to_string(),
        params,
    }
}

fn str_field(args: &Value, field: &str) -> Result<String, String> {
    args.get(field)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing required field: {field}"))
}

fn peer_hello_params(args: &Value) -> Result<Value, String> {
    let to_session_id = str_field(args, "to_session_id")?;
    let from_daemon_id = str_field(args, "from_daemon_id")?;
    let from_session_id = str_field(args, "from_session_id")?;
    let want_method = args
        .get("want_method")
        .and_then(|v| v.as_str())
        .unwrap_or("mailbox.post");
    Ok(json!({
        "to_session_id": to_session_id,
        "from": {
            "daemon_id": from_daemon_id,
            "session_id": from_session_id,
            "advertised_capabilities": []
        },
        "want": { "method": want_method }
    }))
}

fn mailbox_post_params(args: &Value) -> Result<Value, String> {
    Ok(json!({
        "to_session_id": str_field(args, "to_session_id")?,
        "peer_handle": str_field(args, "peer_handle")?,
        "from": {
            "daemon_id": str_field(args, "from_daemon_id")?,
            "session_id": str_field(args, "from_session_id")?,
        },
        "kind": str_field(args, "kind")?,
        "subject": str_field(args, "subject")?,
        "body": args.get("body").cloned().unwrap_or(Value::Null),
    }))
}

fn mailbox_list_params(args: &Value) -> Result<Value, String> {
    Ok(json!({
        "session_id": str_field(args, "session_id")?,
        "since_seq": args.get("since_seq").cloned(),
        "include_consumed": args.get("include_consumed").and_then(|v| v.as_bool()).unwrap_or(false),
    }))
}

fn mailbox_read_params(args: &Value) -> Result<Value, String> {
    let seq = args
        .get("seq")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| "missing required field: seq".to_string())?;
    Ok(json!({
        "session_id": str_field(args, "session_id")?,
        "seq": seq,
    }))
}

fn mailbox_consume_params(args: &Value) -> Result<Value, String> {
    let seqs = args
        .get("seqs")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_u64()).collect::<Vec<_>>())
        .unwrap_or_default();
    Ok(json!({
        "session_id": str_field(args, "session_id")?,
        "seqs": seqs,
    }))
}
