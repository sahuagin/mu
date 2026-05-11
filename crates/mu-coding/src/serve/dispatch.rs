//! JSON-RPC method dispatch for `mu serve`.
//!
//! The handler closure passed to `mu_core::transport::serve` calls
//! `dispatch::dispatch` for every incoming request. The five method
//! arms map mu-001's `*Request` types to operations on the session
//! manager.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use mu_core::agent::{AgentConfig, AgentInput, AgentLoop, AgentMessage, Tool};
use mu_core::capability::Capability;
use mu_core::event_log::{EventActor, EventPayload, SessionEventLog};
use mu_core::protocol::{
    AskSessionRequest, AskSessionResponse, CancelOutstandingRequest, CancelOutstandingResponse,
    CancelSessionRequest, CancelSessionResponse, CloseSessionRequest, CloseSessionResponse,
    CreateSessionRequest, CreateSessionResponse, DaemonStatsRequest, DaemonStatsResponse,
    DaemonUsageHistoryRequest, DaemonUsageHistoryResponse, DelegateSessionRequest,
    DelegateSessionResponse, PingRequest, PingResponse, ProviderSelector, ProviderStatusKind,
    Request, RespondToInputRequiredRequest, RespondToInputRequiredResponse, Response,
    SessionEventsRequest, SessionEventsResponse, SessionListRequest, SessionListResponse,
    SessionStatsRequest, SessionStatsResponse, SessionStatusSummary,
};
use mu_core::usage_history::{aggregate_into_rows, extract_per_session_metrics};
use mu_core::transport::{codes, err_response, ok_response, NotificationWriter};

use super::daemon_info::DaemonInfo;
use super::discovery::{derive_status, derive_status_from_events, SessionDiscovery};
use super::factory::ProviderFactory;
use super::forwarder::forward_events;
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
        CreateSessionRequest::METHOD => {
            handle_create_session(request, notif, sessions, factory, tools, daemon_info.clone())
        }
        DelegateSessionRequest::METHOD => {
            handle_delegate_session(request, notif, sessions, factory, tools, daemon_info.clone())
        }
        AskSessionRequest::METHOD => handle_ask_session(request, sessions).await,
        CancelSessionRequest::METHOD => handle_cancel_session(request, sessions).await,
        CancelOutstandingRequest::METHOD => {
            handle_cancel_outstanding(request, sessions).await
        }
        CloseSessionRequest::METHOD => handle_close_session(request, sessions),
        SessionStatsRequest::METHOD => handle_session_stats(request, sessions),
        SessionListRequest::METHOD => handle_session_list(request, discovery).await,
        SessionEventsRequest::METHOD => handle_session_events(request, sessions),
        DaemonStatsRequest::METHOD => handle_daemon_stats(request, sessions, daemon_info),
        DaemonUsageHistoryRequest::METHOD => handle_daemon_usage_history(request, sessions),
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
    daemon_info: DaemonInfo,
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

    match build_and_register_session(
        &params.provider,
        None, // no parent — this is a root session
        None,
        Capability::root(), // root session: unrestricted
        notif,
        sessions,
        factory,
        tools,
        &daemon_info,
    ) {
        Ok(session_id) => {
            let resp = CreateSessionResponse { session_id };
            ok_response(request.id, to_value_or_null(resp))
        }
        Err(e) => err_response(
            request.id,
            codes::INVALID_PARAMS,
            format!("create_session: {e}"),
        ),
    }
}

fn handle_delegate_session(
    request: Request<Value>,
    notif: NotificationWriter,
    sessions: Sessions,
    factory: ProviderFactory,
    tools: Arc<Vec<Arc<dyn Tool>>>,
    daemon_info: DaemonInfo,
) -> Response<Value> {
    let params: DelegateSessionRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("session.delegate: invalid params: {e}"),
            );
        }
    };

    // Verify the parent session exists, and snapshot its current
    // capability so we can attenuate it for the child (mu-033).
    let parent_cap_handle = match sessions.capability(&params.parent_session_id) {
        Some(c) => c,
        None => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!(
                    "session.delegate: parent session not found: {}",
                    params.parent_session_id
                ),
            );
        }
    };

    // Compute child capability = parent ∩ requested attenuations.
    // If the request didn't supply attenuations, the child inherits
    // the parent's capability unchanged.
    let child_capability = {
        let parent_cap = parent_cap_handle
            .lock()
            .map(|c| c.clone())
            .unwrap_or_else(|_| Capability::root());
        match &params.attenuations {
            Some(attn) => parent_cap.attenuate(attn),
            None => parent_cap,
        }
    };

    match build_and_register_session(
        &params.provider,
        Some(params.parent_session_id.clone()),
        params.branched_at_parent_event_id,
        child_capability,
        notif,
        sessions,
        factory,
        tools,
        &daemon_info,
    ) {
        Ok(child_session_id) => {
            let resp = DelegateSessionResponse { child_session_id };
            ok_response(request.id, to_value_or_null(resp))
        }
        Err(e) => err_response(
            request.id,
            codes::INVALID_PARAMS,
            format!("session.delegate: {e}"),
        ),
    }
}

/// Shared session-creation logic for both `create_session` (root) and
/// `session.delegate` (child). Returns the new session_id on success
/// or a human-readable error on provider-construction failure.
fn build_and_register_session(
    selector: &ProviderSelector,
    parent_session_id: Option<String>,
    branched_at_parent_event_id: Option<u64>,
    capability: Capability,
    notif: NotificationWriter,
    sessions: Sessions,
    factory: ProviderFactory,
    tools: Arc<Vec<Arc<dyn Tool>>>,
    daemon_info: &DaemonInfo,
) -> Result<String, String> {
    let provider = factory(selector).map_err(|e| format!("could not build provider: {e}"))?;

    let session_id = Sessions::next_id();
    let event_log = Arc::new(SessionEventLog::new(session_id.clone()));

    // mu-upb: attach a per-session JSONL writer at
    // <events_dir>/<daemon_id>/<session_id>.jsonl.
    // Best-effort — failures are logged but don't block session
    // creation. When daemon_info.events_dir() is None (tests),
    // skip entirely — no disk write happens. Production sets
    // events_dir to ~/.local/share/mu/events.
    if let Some(events_dir) = daemon_info.events_dir() {
        let path = events_dir
            .join(daemon_info.daemon_id())
            .join(format!("{}.jsonl", session_id));
        if let Err(e) = event_log.attach_disk_writer(&path) {
            tracing::warn!(
                session_id = %session_id,
                path = %path.display(),
                error = %e,
                "could not attach disk writer; continuing in-memory only",
            );
        }
    }

    let (kind_str, model_str) = describe_selector(selector);
    event_log.append(
        EventActor::System,
        EventPayload::SessionCreated {
            provider_kind: kind_str,
            model: model_str,
            parent_session_id: parent_session_id.clone(),
            branched_at_parent_event_id,
        },
    );

    let pending_approvals = Arc::new(Mutex::new(HashMap::new()));
    let capability_handle = Arc::new(Mutex::new(capability));
    let (events_tx, events_rx) = tokio::sync::mpsc::channel(64);
    let session_tools: Vec<Arc<dyn Tool>> = (*tools).clone();
    let agent = AgentLoop::spawn(
        provider,
        session_tools,
        AgentConfig::default(),
        events_tx,
        pending_approvals.clone(),
        capability_handle.clone(),
    );
    let input_tx = agent.sender();
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
        pending_approvals,
        parent_session_id,
        capability_handle,
    );

    Ok(session_id)
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

async fn handle_cancel_outstanding(
    request: Request<Value>,
    sessions: Sessions,
) -> Response<Value> {
    // mu-035 Phase C: narrow-cancel of the current provider call.
    // Sends AgentInput::CancelOutstanding through the session's input
    // channel; the agent loop aborts the in-flight stream / tool and
    // emits Done(Aborted), then continues to wait for the next ask.
    // Session stays alive.
    let params: CancelOutstandingRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("cancel_outstanding: invalid params: {e}"),
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
            let reason = params.reason.unwrap_or_else(|| "client request".into());
            // best-effort — if the loop already terminated, send fails
            // silently and we report canceled=false.
            let send_ok = tx
                .send(AgentInput::CancelOutstanding {
                    reason: reason.clone(),
                })
                .await
                .is_ok();
            // We can't synchronously observe what state the loop was
            // in at the moment we sent (it's racy by nature). v1
            // reports a best-effort "canceled if we managed to send,
            // and we don't know was_in." A future slice could plumb
            // a per-session ProviderStatusTracker into Sessions and
            // read its current state here — but that's mu-iwq Phase D
            // territory (daemon.outstanding_calls), so we keep this
            // handler narrow for now.
            let resp = CancelOutstandingResponse {
                canceled: send_ok,
                was_in: ProviderStatusKind::AwaitingFirstToken, // placeholder; see comment above
            };
            ok_response(request.id, to_value_or_null(resp))
        }
    }
}

fn handle_session_stats(request: Request<Value>, sessions: Sessions) -> Response<Value> {
    let params: SessionStatsRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("session.stats: invalid params: {e}"),
            );
        }
    };

    let log = match sessions.event_log(&params.session_id) {
        Some(l) => l,
        None => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("session not found: {}", params.session_id),
            );
        }
    };

    let (provider_kind, model) = match log.provider_info() {
        Some((k, m)) => (Some(k), Some(m)),
        None => (None, None),
    };

    let resp = SessionStatsResponse {
        session_id: params.session_id,
        provider_kind,
        model,
        started_at_unix_ms: log.started_at_unix_ms(),
        last_activity_unix_ms: log.last_activity_unix_ms(),
        event_count: log.len() as u32,
        ask_count: log.ask_count(),
        total_turn_count: log.total_turn_count(),
        tool_call_count: log.tool_call_count(),
        elapsed_total_ms: log.elapsed_total_ms(),
        usage: log.cumulative_usage(),
    };
    ok_response(request.id, to_value_or_null(resp))
}

fn handle_respond_to_input_required(
    request: Request<Value>,
    sessions: Sessions,
) -> Response<Value> {
    let params: RespondToInputRequiredRequest =
        match serde_json::from_value(request.params.clone()) {
            Ok(p) => p,
            Err(e) => {
                return err_response(
                    request.id,
                    codes::INVALID_PARAMS,
                    format!("respond_to_input_required: invalid params: {e}"),
                );
            }
        };

    // Look up the pending oneshot; if found, send the decision.
    let sender_opt = sessions.take_pending_approval(&params.session_id, &params.request_id);
    let accepted = match sender_opt {
        Some(sender) => sender.send(params.decision).is_ok(),
        None => false,
    };
    let resp = RespondToInputRequiredResponse { accepted };
    ok_response(request.id, to_value_or_null(resp))
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

// ── mu-038: projection-query handlers ──────────────────────────────

async fn handle_session_list(
    request: Request<Value>,
    discovery: Arc<dyn SessionDiscovery>,
) -> Response<Value> {
    let params: SessionListRequest =
        match serde_json::from_value(request.params.clone()) {
            Ok(p) => p,
            Err(e) => {
                return err_response(
                    request.id,
                    codes::INVALID_PARAMS,
                    format!("session.list: invalid params: {e}"),
                );
            }
        };
    let filter = params.filter.unwrap_or_default();
    let now_ms = super::discovery::now_unix_ms();
    match discovery.list(&filter).await {
        Ok(sessions) => {
            let resp = SessionListResponse {
                sessions,
                snapshot_at_unix_ms: now_ms,
                failed_peers: Vec::new(),
            };
            ok_response(request.id, to_value_or_null(resp))
        }
        Err(super::discovery::DiscoveryError::PartialFailure {
            local,
            failed_peers,
        }) => {
            // INV-2: local results survive a peer outage. Surface
            // failed_peers so the client can decide whether to retry
            // or warn.
            let resp = SessionListResponse {
                sessions: local,
                snapshot_at_unix_ms: now_ms,
                failed_peers,
            };
            ok_response(request.id, to_value_or_null(resp))
        }
        Err(super::discovery::DiscoveryError::Backend(msg)) => err_response(
            request.id,
            codes::INTERNAL_ERROR,
            format!("session.list: backend error: {msg}"),
        ),
    }
}

fn handle_session_events(
    request: Request<Value>,
    sessions: Sessions,
) -> Response<Value> {
    let params: SessionEventsRequest =
        match serde_json::from_value(request.params.clone()) {
            Ok(p) => p,
            Err(e) => {
                return err_response(
                    request.id,
                    codes::INVALID_PARAMS,
                    format!("session.events: invalid params: {e}"),
                );
            }
        };
    let log = match sessions.event_log(&params.session_id) {
        Some(l) => l,
        None => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("session not found: {}", params.session_id),
            );
        }
    };

    let limit = params.limit.unwrap_or(200).clamp(1, 5000) as usize;
    let after = params.after_event_id.unwrap_or(0);
    let kinds_filter: std::collections::HashSet<String> =
        params.kinds_filter.iter().cloned().collect();

    let all = log.snapshot();
    let mut events_json: Vec<Value> = Vec::with_capacity(limit);
    let mut last_emitted: Option<u64> = None;
    let mut end_of_log = true;

    for ev in all.iter().filter(|e| e.id > after) {
        let payload_kind = payload_kind_str(&ev.payload);
        if !kinds_filter.is_empty() && !kinds_filter.contains(payload_kind) {
            continue;
        }
        if events_json.len() >= limit {
            end_of_log = false;
            break;
        }
        match serde_json::to_value(ev) {
            Ok(v) => {
                last_emitted = Some(ev.id);
                events_json.push(v);
            }
            Err(_) => {
                // Best-effort: skip unserialisable events rather than
                // failing the whole page.
                continue;
            }
        }
    }

    let next_event_id = if end_of_log { None } else { last_emitted };
    let resp = SessionEventsResponse {
        events: events_json,
        next_event_id,
        end_of_log,
    };
    ok_response(request.id, to_value_or_null(resp))
}

fn handle_daemon_stats(
    request: Request<Value>,
    sessions: Sessions,
    daemon_info: DaemonInfo,
) -> Response<Value> {
    let _params: DaemonStatsRequest =
        match serde_json::from_value(request.params.clone()) {
            Ok(p) => p,
            Err(e) => {
                return err_response(
                    request.id,
                    codes::INVALID_PARAMS,
                    format!("daemon.stats: invalid params: {e}"),
                );
            }
        };

    let now_ms = super::discovery::now_unix_ms();
    let snapshot = sessions.snapshot_for_listing();
    let session_count = snapshot.len() as u32;
    let mut active_session_count: u32 = 0;
    let mut total_events: u64 = 0;
    let mut total_tool_calls: u64 = 0;
    let mut total_input_tokens: u64 = 0;
    let mut total_output_tokens: u64 = 0;
    let mut in_flight_calls_count: u32 = 0;

    for (_sid, log, _parent) in snapshot.iter() {
        let events = log.snapshot();
        total_events = total_events.saturating_add(events.len() as u64);
        total_tool_calls = total_tool_calls.saturating_add(log.tool_call_count() as u64);
        if let Some(u) = log.cumulative_usage() {
            total_input_tokens = total_input_tokens.saturating_add(u.input_tokens as u64);
            total_output_tokens =
                total_output_tokens.saturating_add(u.output_tokens as u64);
        }
        let status = derive_status_from_events(&events, now_ms);
        if matches!(
            status,
            SessionStatusSummary::Asking
                | SessionStatusSummary::Streaming
                | SessionStatusSummary::ToolExecuting
                | SessionStatusSummary::AwaitingInputRequired
        ) {
            active_session_count = active_session_count.saturating_add(1);
            if matches!(
                status,
                SessionStatusSummary::Asking
                    | SessionStatusSummary::Streaming
                    | SessionStatusSummary::ToolExecuting
            ) {
                in_flight_calls_count = in_flight_calls_count.saturating_add(1);
            }
        }
    }

    let _ = derive_status; // keep import live; status is computed via derive_status_from_events above
    let resp = DaemonStatsResponse {
        daemon_id: daemon_info.daemon_id().to_string(),
        version: daemon_info.version().to_string(),
        started_at_unix_ms: daemon_info.started_at_unix_ms(),
        uptime_ms: daemon_info.uptime_ms(),
        session_count,
        active_session_count,
        total_events,
        total_tool_calls,
        total_input_tokens,
        total_output_tokens,
        in_flight_calls_count,
    };
    ok_response(request.id, to_value_or_null(resp))
}

/// mu-pex Phase 1 — historical roll-up of timing and token usage
/// across in-memory sessions (live + retained-recently-closed),
/// grouped by (provider, model, time-bucket).
fn handle_daemon_usage_history(
    request: Request<Value>,
    sessions: Sessions,
) -> Response<Value> {
    let params: DaemonUsageHistoryRequest =
        match serde_json::from_value(request.params.clone()) {
            Ok(p) => p,
            Err(e) => {
                return err_response(
                    request.id,
                    codes::INVALID_PARAMS,
                    format!("daemon.usage_history: invalid params: {e}"),
                );
            }
        };

    let snapshot = sessions.snapshot_for_listing();
    let mut per_session = Vec::with_capacity(snapshot.len());
    let mut considered: u32 = 0;
    for (_sid, log, _parent) in snapshot.iter() {
        let events = log.snapshot();
        let Some(metrics) = extract_per_session_metrics(&events) else {
            continue;
        };
        considered = considered.saturating_add(1);
        if let Some(since) = params.since_unix_ms {
            if metrics.started_at_unix_ms < since {
                continue;
            }
        }
        if let Some(until) = params.until_unix_ms {
            if metrics.started_at_unix_ms >= until {
                continue;
            }
        }
        per_session.push(metrics);
    }

    let rows = aggregate_into_rows(per_session, params.time_bucket_ms);
    let resp = DaemonUsageHistoryResponse {
        rows,
        session_count_total: considered,
        snapshot_at_unix_ms: super::discovery::now_unix_ms(),
    };
    ok_response(request.id, to_value_or_null(resp))
}

/// Wire `payload_kind` string for `kinds_filter`. Matches serde's
/// rename_all snake_case on the EventPayload tag.
fn payload_kind_str(p: &EventPayload) -> &'static str {
    match p {
        EventPayload::SessionCreated { .. } => "session_created",
        EventPayload::UserMessage { .. } => "user_message",
        EventPayload::AssistantMessageEvent { .. } => "assistant_message_event",
        EventPayload::ToolCall { .. } => "tool_call",
        EventPayload::ToolResult { .. } => "tool_result",
        EventPayload::Done { .. } => "done",
        EventPayload::Error { .. } => "error",
        EventPayload::Callout { .. } => "callout",
        EventPayload::SessionClosed => "session_closed",
        EventPayload::ContextAssembly { .. } => "context_assembly",
        EventPayload::ProviderStatusUpdate { .. } => "provider_status_update",
    }
}
