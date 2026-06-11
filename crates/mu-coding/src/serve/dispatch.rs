//! JSON-RPC method dispatch router for `mu serve`.
//!
//! spec mu-046 (WP3): requests no longer arrive here straight off the
//! transport — they cross the ingest pipeline first
//! ([`super::pipeline`]: journaled, sequenced, single-writer). The
//! pipeline's control-plane consumer applies [`auth_gate`] and then
//! routes via [`dispatch_inner`], whose match arms are the unchanged
//! per-method handlers.

use std::sync::Arc;

use serde_json::Value;

use mu_core::agent::Tool;
use mu_core::protocol::{
    AskSessionRequest, AuthInitiateRequest, AuthOfferRequest, CancelOutstandingRequest,
    CancelSessionRequest, CapabilitiesDiscoverRequest, CloseSessionRequest, CreateSessionRequest,
    DaemonListRoutesRequest, DaemonOutstandingCallsRequest, DaemonStatsRequest,
    DaemonUsageHistoryRequest, DelegateSessionRequest, MailboxConsumeRequest, MailboxListRequest,
    MailboxPostRequest, MailboxReadRequest, PeerHelloRequest, PingRequest, Request,
    RespondToInputRequiredRequest, Response, ResumeSessionRequest, ScheduleWakeupRequest,
    SessionEventsRequest, SessionListRequest, SessionStatsRequest, SetRouteRequest,
    SpawnWorkerRequest, StartAutonomousRequest,
};
use mu_core::skill::loader::LoadedSkill;
use mu_core::transport::{codes, err_response, NotificationWriter};

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

/// Methods callable without an `Authenticated` `AuthState`. Anything
/// outside this list requires the gate to pass.
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
pub(crate) async fn dispatch_inner(
    request: Request<Value>,
    notif: NotificationWriter,
    ctx: DispatchCtx,
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
        AskSessionRequest::METHOD => handle_ask_session(request, sessions).await,
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
        other => err_response(
            request.id,
            codes::METHOD_NOT_FOUND,
            format!("unknown method: {other}"),
        ),
    }
}
