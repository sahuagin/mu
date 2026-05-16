//! JSON-RPC method dispatch router for `mu serve`.
//!
//! The handler closure passed to `mu_core::transport::serve` calls
//! `dispatch::dispatch` for every incoming request. The match statement
//! routes based on method name to handler modules.

use std::sync::Arc;

use serde_json::Value;

use mu_core::agent::Tool;
use mu_core::protocol::{
    AskSessionRequest, CancelOutstandingRequest, CancelSessionRequest, CloseSessionRequest,
    CreateSessionRequest, DaemonOutstandingCallsRequest, DaemonStatsRequest,
    DaemonUsageHistoryRequest, DelegateSessionRequest, MailboxConsumeRequest, MailboxListRequest,
    MailboxPostRequest, PeerHelloRequest, PingRequest, Request, RespondToInputRequiredRequest,
    Response, ScheduleWakeupRequest, SessionEventsRequest, SessionListRequest, SessionStatsRequest,
    StartAutonomousRequest,
};
use mu_core::transport::{codes, err_response, NotificationWriter};

use super::daemon_info::DaemonInfo;
use super::discovery::SessionDiscovery;
use super::factory::ProviderFactory;
use super::handlers::{daemon::*, mailbox::*, session::*};
use super::sessions::Sessions;

pub async fn dispatch(
    request: Request<Value>,
    notif: NotificationWriter,
    sessions: Sessions,
    factory: ProviderFactory,
    tools: Arc<Vec<Arc<dyn Tool>>>,
    daemon_info: DaemonInfo,
    discovery: Arc<dyn SessionDiscovery>,
) -> Response<Value> {
    match request.method.as_str() {
        PingRequest::METHOD => handle_ping(request),
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
