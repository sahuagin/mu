//! JSON-RPC method dispatch router for `mu serve`.
//!
//! The handler closure passed to `mu_core::transport::serve` calls
//! `dispatch::dispatch` for every incoming request. The match statement
//! routes based on method name to handler modules.

use std::sync::Arc;

use serde_json::Value;

use mu_core::agent::Tool;
use mu_core::protocol::{
    AskSessionRequest, AuthInitiateRequest, AuthOfferRequest, CancelOutstandingRequest,
    CancelSessionRequest, CloseSessionRequest, CreateSessionRequest, DaemonOutstandingCallsRequest,
    DaemonStatsRequest, DaemonUsageHistoryRequest, DelegateSessionRequest, MailboxConsumeRequest,
    MailboxListRequest, MailboxPostRequest, PeerHelloRequest, PingRequest, Request,
    RespondToInputRequiredRequest, Response, ScheduleWakeupRequest, SessionEventsRequest,
    SessionListRequest, SessionStatsRequest, StartAutonomousRequest,
};
use mu_core::transport::{codes, err_response, NotificationWriter};

use super::auth::{AuthRegistry, AuthStateHandle};
use super::daemon_info::DaemonInfo;
use super::discovery::SessionDiscovery;
use super::factory::ProviderFactory;
use super::handlers::auth::{handle_auth_initiate, handle_auth_offer};
use super::handlers::{daemon::*, mailbox::*, session::*};
use super::sessions::Sessions;

// mu-7rk (mu-yox): `dispatch` now carries two extra daemon-wide
// handles: a shared `AuthRegistry` (constructed once at serve start
// from `[auth]` config) and a per-connection `AuthStateHandle`. The
// two new arms (`peer.auth_offer`, `peer.auth_initiate`) drive the
// handshake. **No other arm consumes the resulting `AuthState`** —
// enforcement is mu-fnn (mu-7rk-c) and the clippy "too many arguments"
// lint stays silenced; bundling these into a struct would only push
// the same fields into a builder.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch(
    request: Request<Value>,
    notif: NotificationWriter,
    sessions: Sessions,
    factory: ProviderFactory,
    tools: Arc<Vec<Arc<dyn Tool>>>,
    daemon_info: DaemonInfo,
    discovery: Arc<dyn SessionDiscovery>,
    auth_registry: Arc<AuthRegistry>,
    auth_state: AuthStateHandle,
) -> Response<Value> {
    match request.method.as_str() {
        PingRequest::METHOD => handle_ping(request),
        // mu-7rk (mu-yox): connect-time SASL-shaped auth handshake.
        AuthOfferRequest::METHOD => handle_auth_offer(request, &auth_registry),
        AuthInitiateRequest::METHOD => handle_auth_initiate(request, &auth_registry, &auth_state),
        CreateSessionRequest::METHOD => handle_create_session(
            request,
            notif,
            sessions,
            factory,
            tools,
            daemon_info.clone(),
        ),
        DelegateSessionRequest::METHOD => handle_delegate_session(
            request,
            notif,
            sessions,
            factory,
            tools,
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
        MailboxConsumeRequest::METHOD => handle_mailbox_consume(request, sessions),
        // mu-036 Phase A.2: wire surface ready, dispatch stubs return
        // a structured "not yet implemented" until Phase B (mu-3ao /
        // mu-7zn / mu-pv9) lands the agent-loop integration.
        StartAutonomousRequest::METHOD => handle_start_autonomous(request, sessions).await,
        ScheduleWakeupRequest::METHOD => handle_schedule_wakeup(request, sessions),
        RespondToInputRequiredRequest::METHOD => {
            handle_respond_to_input_required(request, sessions)
        }
        other => err_response(
            request.id,
            codes::METHOD_NOT_FOUND,
            format!("unknown method: {other}"),
        ),
    }
}
