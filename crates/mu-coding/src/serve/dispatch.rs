//! JSON-RPC method dispatch for `mu serve`.
//!
//! The handler closure passed to `mu_core::transport::serve` calls
//! `dispatch::dispatch` for every incoming request. The five method
//! arms map mu-001's `*Request` types to operations on the session
//! manager.

use std::sync::Arc;

use serde_json::Value;

use mu_core::agent::{
    AgentConfig, AgentInput, AgentLoop, AgentMessage, Provider, Tool,
};
use mu_core::protocol::{
    AskSessionRequest, AskSessionResponse, CancelSessionRequest, CancelSessionResponse,
    CloseSessionRequest, CloseSessionResponse, CreateSessionRequest, CreateSessionResponse,
    PingRequest, PingResponse, Request, Response,
};
use mu_core::transport::{codes, err_response, ok_response, NotificationWriter};

use super::forwarder::forward_events;
use super::sessions::Sessions;

pub async fn dispatch(
    request: Request<Value>,
    notif: NotificationWriter,
    sessions: Sessions,
    provider: Arc<dyn Provider>,
    tools: Arc<Vec<Arc<dyn Tool>>>,
) -> Response<Value> {
    match request.method.as_str() {
        PingRequest::METHOD => handle_ping(request),
        CreateSessionRequest::METHOD => {
            handle_create_session(request, notif, sessions, provider, tools)
        }
        AskSessionRequest::METHOD => handle_ask_session(request, sessions).await,
        CancelSessionRequest::METHOD => handle_cancel_session(request, sessions).await,
        CloseSessionRequest::METHOD => handle_close_session(request, sessions),
        other => err_response(
            request.id,
            codes::METHOD_NOT_FOUND,
            format!("unknown method: {other}"),
        ),
    }
}

fn to_value_or_null<T: serde::Serialize>(value: T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn handle_ping(request: Request<Value>) -> Response<Value> {
    let resp = PingResponse {
        pong: true,
        server_version: env!("CARGO_PKG_VERSION").into(),
    };
    ok_response(request.id, to_value_or_null(resp))
}

fn handle_create_session(
    request: Request<Value>,
    notif: NotificationWriter,
    sessions: Sessions,
    provider: Arc<dyn Provider>,
    tools: Arc<Vec<Arc<dyn Tool>>>,
) -> Response<Value> {
    // v1 ignores request.params.provider — daemon-wide provider only.
    // We don't even need to parse the params for v1, but parsing
    // gives us a place to surface bad request shapes early.
    let _params: CreateSessionRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("create_session: invalid params: {e}"),
            );
        }
    };

    let session_id = Sessions::next_id();
    let (events_tx, events_rx) = tokio::sync::mpsc::channel(64);
    // Each session gets its own copy of the tools vec. Tools
    // themselves are Arc-wrapped so the actual Tool instances are
    // shared.
    let session_tools: Vec<Arc<dyn Tool>> = (*tools).clone();
    let agent = AgentLoop::spawn(provider, session_tools, AgentConfig::default(), events_tx);
    let input_tx = agent.sender();

    // Wrap AgentLoop::join into a JoinHandle<()> so it can sit in
    // SessionState alongside the forwarder's handle.
    let agent_handle = tokio::spawn(async move {
        let _ = agent.join().await;
    });

    let forwarder_handle = tokio::spawn(forward_events(
        session_id.clone(),
        events_rx,
        notif.clone(),
    ));

    sessions.insert(session_id.clone(), input_tx, forwarder_handle, agent_handle);

    let resp = CreateSessionResponse { session_id };
    ok_response(request.id, to_value_or_null(resp))
}

async fn handle_ask_session(request: Request<Value>, sessions: Sessions) -> Response<Value> {
    let params: AskSessionRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("ask_session: invalid params: {e}"),
            );
        }
    };

    // Brief sync lock to clone the sender; lock dropped before the
    // await below.
    let sender = sessions.input_sender(&params.session_id);

    match sender {
        None => err_response(
            request.id,
            codes::INVALID_PARAMS,
            format!("session not found: {}", params.session_id),
        ),
        Some(tx) => {
            let msg = AgentMessage::User {
                content: params.user_message,
            };
            match tx.send(AgentInput::UserMessage(msg)).await {
                Ok(_) => {
                    let resp = AskSessionResponse { accepted: true };
                    ok_response(request.id, to_value_or_null(resp))
                }
                Err(_) => err_response(
                    request.id,
                    codes::INTERNAL_ERROR,
                    "session loop has terminated",
                ),
            }
        }
    }
}

async fn handle_cancel_session(request: Request<Value>, sessions: Sessions) -> Response<Value> {
    let params: CancelSessionRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("cancel_session: invalid params: {e}"),
            );
        }
    };

    let sender = sessions.input_sender(&params.session_id);
    match sender {
        None => err_response(
            request.id,
            codes::INVALID_PARAMS,
            format!("session not found: {}", params.session_id),
        ),
        Some(tx) => {
            // best-effort cancel — if the loop already terminated,
            // the send fails silently, but we still report cancelled.
            let _ = tx.send(AgentInput::Cancel).await;
            let resp = CancelSessionResponse { cancelled: true };
            ok_response(request.id, to_value_or_null(resp))
        }
    }
}

fn handle_close_session(request: Request<Value>, sessions: Sessions) -> Response<Value> {
    let params: CloseSessionRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("close_session: invalid params: {e}"),
            );
        }
    };

    let removed = sessions.remove(&params.session_id);
    let resp = CloseSessionResponse { closed: removed };
    ok_response(request.id, to_value_or_null(resp))
}
