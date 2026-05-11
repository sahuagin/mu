//! JSON-RPC method dispatch for `mu serve`.
//!
//! The handler closure passed to `mu_core::transport::serve` calls
//! `dispatch::dispatch` for every incoming request. The five method
//! arms map mu-001's `*Request` types to operations on the session
//! manager.

use std::sync::Arc;

use serde_json::Value;

use mu_core::agent::{AgentConfig, AgentInput, AgentLoop, AgentMessage, Tool};
use mu_core::event_log::{EventActor, EventPayload, SessionEventLog};
use mu_core::protocol::{
    AskSessionRequest, AskSessionResponse, CancelSessionRequest, CancelSessionResponse,
    CloseSessionRequest, CloseSessionResponse, CreateSessionRequest, CreateSessionResponse,
    PingRequest, PingResponse, ProviderSelector, Request, Response,
};
use mu_core::transport::{codes, err_response, ok_response, NotificationWriter};

use super::factory::ProviderFactory;
use super::forwarder::forward_events;
use super::sessions::Sessions;

pub async fn dispatch(
    request: Request<Value>,
    notif: NotificationWriter,
    sessions: Sessions,
    factory: ProviderFactory,
    tools: Arc<Vec<Arc<dyn Tool>>>,
) -> Response<Value> {
    match request.method.as_str() {
        PingRequest::METHOD => handle_ping(request),
        CreateSessionRequest::METHOD => {
            handle_create_session(request, notif, sessions, factory, tools)
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
    factory: ProviderFactory,
    tools: Arc<Vec<Arc<dyn Tool>>>,
) -> Response<Value> {
    let params: CreateSessionRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("create_session: invalid params: {e}"),
            );
        }
    };

    // Per-session provider construction (mu-020). The factory closes
    // over daemon-startup flags (ephemeral, thinking); the selector
    // picks which provider + which model. Two sessions on the same
    // daemon can use different providers.
    let provider = match factory(&params.provider) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("create_session: could not build provider: {e}"),
            );
        }
    };

    let session_id = Sessions::next_id();

    // Per-session event log (mu-025). Records significant events
    // for the lifetime of the session. The forwarder appends as
    // events arrive; readers (cumulative usage, future replay)
    // snapshot via the log's methods.
    let event_log = Arc::new(SessionEventLog::new(session_id.clone()));
    // First event: provenance of the session itself.
    let (kind_str, model_str) = describe_selector(&params.provider);
    event_log.append(
        EventActor::System,
        EventPayload::SessionCreated {
            provider_kind: kind_str,
            model: model_str,
        },
    );

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
        event_log.clone(),
    ));

    sessions.insert(
        session_id.clone(),
        input_tx,
        forwarder_handle,
        agent_handle,
        event_log,
    );

    let resp = CreateSessionResponse { session_id };
    ok_response(request.id, to_value_or_null(resp))
}

/// Pull a (kind, model) pair out of a `ProviderSelector` for logging
/// purposes. The protocol-level enum is already snake_case on the
/// wire; we just want a flat (string, string) for the event payload.
fn describe_selector(selector: &ProviderSelector) -> (String, String) {
    match selector {
        ProviderSelector::AnthropicApi { model } => ("anthropic_api".into(), model.clone()),
        ProviderSelector::AnthropicOauth { model } => ("anthropic_oauth".into(), model.clone()),
        ProviderSelector::OpenaiApi { model } => ("openai_api".into(), model.clone()),
        ProviderSelector::OpenaiCodex { model } => ("openai_codex".into(), model.clone()),
        ProviderSelector::Openrouter { model } => ("openrouter".into(), model.clone()),
    }
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

    // Emit SessionClosed into the log BEFORE removing the session
    // from the registry — once removed, the log handle is dropped.
    if let Some(log) = sessions.event_log(&params.session_id) {
        log.append(EventActor::System, EventPayload::SessionClosed);
    }

    let removed = sessions.remove(&params.session_id);
    let resp = CloseSessionResponse { closed: removed };
    ok_response(request.id, to_value_or_null(resp))
}
