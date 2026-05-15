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
    AskSessionRequest, AskSessionResponse, AuthDenialCode, AuthExchangeResponse,
    AuthInitiateRequest, AuthOfferRequest, AuthOfferResponse, CancelOutstandingRequest,
    CancelOutstandingResponse, CancelSessionRequest, CancelSessionResponse, CloseSessionRequest,
    CloseSessionResponse, CreateSessionRequest, CreateSessionResponse,
    DaemonOutstandingCallsRequest, DaemonOutstandingCallsResponse, DaemonStatsRequest,
    DaemonStatsResponse, DaemonUsageHistoryRequest, DaemonUsageHistoryResponse,
    DelegateSessionRequest, DelegateSessionResponse, MailboxConsumeRequest, MailboxConsumeResponse,
    MailboxListRequest, MailboxListResponse, MailboxMessageView, MailboxPostRequest,
    MailboxPostResponse, PeerHelloRequest, PeerHelloResponse, PingRequest, PingResponse,
    ProviderSelector, ProviderStatusKind, Request, RespondToInputRequiredRequest,
    RespondToInputRequiredResponse, Response, ScheduleWakeupRequest, SessionEventsRequest,
    SessionEventsResponse, SessionListRequest, SessionListResponse, SessionStatsRequest,
    SessionStatsResponse, SessionStatusSummary, StartAutonomousRequest, StartAutonomousResponse,
};
use mu_core::transport::{codes, err_response, ok_response, NotificationWriter};
use mu_core::usage_history::{aggregate_into_rows, extract_per_session_metrics};

use super::auth::{AuthRegistry, AuthState, AuthStateHandle, AuthStepOutcome};
use super::daemon_info::DaemonInfo;
use super::discovery::{derive_status, derive_status_from_events, SessionDiscovery};
use super::factory::ProviderFactory;
use super::forwarder::forward_events;
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
        DaemonOutstandingCallsRequest::METHOD => handle_outstanding_calls(request, sessions),
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

    match build_and_register_session(BuildSessionRequest {
        selector: &params.provider,
        system_prompt: params.system_prompt, // mu-n48
        parent_session_id: None,             // no parent — this is a root session
        branched_at_parent_event_id: None,
        capability: Capability::root(), // root session: unrestricted
        notif,
        sessions,
        factory,
        tools,
        daemon_info: &daemon_info,
    }) {
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

    match build_and_register_session(BuildSessionRequest {
        selector: &params.provider,
        system_prompt: None, // mu-n48: delegate sessions inherit (no override yet)
        parent_session_id: Some(params.parent_session_id.clone()),
        branched_at_parent_event_id: params.branched_at_parent_event_id,
        capability: child_capability,
        notif,
        sessions,
        factory,
        tools,
        daemon_info: &daemon_info,
    }) {
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

/// Input bundle for [`build_and_register_session`]. Groups the
/// request shape (selector / system prompt / parent linkage /
/// capability) and the daemon's runtime dependencies (notification
/// writer / sessions registry / provider factory / tools / daemon
/// info) into one struct so the call site reads cleanly.
struct BuildSessionRequest<'a> {
    // request shape
    selector: &'a ProviderSelector,
    system_prompt: Option<String>,
    parent_session_id: Option<String>,
    branched_at_parent_event_id: Option<u64>,
    capability: Capability,
    // runtime deps (daemon-global)
    notif: NotificationWriter,
    sessions: Sessions,
    factory: ProviderFactory,
    tools: Arc<Vec<Arc<dyn Tool>>>,
    daemon_info: &'a DaemonInfo,
}

/// Shared session-creation logic for both `create_session` (root) and
/// `session.delegate` (child). Returns the new session_id on success
/// or a human-readable error on provider-construction failure.
fn build_and_register_session(req: BuildSessionRequest<'_>) -> Result<String, String> {
    let BuildSessionRequest {
        selector,
        system_prompt,
        parent_session_id,
        branched_at_parent_event_id,
        capability,
        notif,
        sessions,
        factory,
        tools,
        daemon_info,
    } = req;
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
    let provider_status = Arc::new(Mutex::new(
        super::provider_status::ProviderStatusTracker::new(),
    ));
    let mailbox = Arc::new(super::mailbox::MailboxState::new());
    let (events_tx, events_rx) = tokio::sync::mpsc::channel(64);
    let session_tools: Vec<Arc<dyn Tool>> = (*tools).clone();
    let agent = AgentLoop::spawn(
        provider,
        session_tools,
        AgentConfig {
            system_prompt,
            ..AgentConfig::default()
        },
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
        provider_status.clone(),
    ));

    sessions.insert(
        session_id.clone(),
        crate::serve::sessions::NewSession {
            input_tx,
            forwarder: forwarder_handle,
            agent: agent_handle,
            event_log,
            pending_approvals,
            parent_session_id,
            capability: capability_handle,
            provider_status,
            mailbox,
        },
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

async fn handle_cancel_outstanding(request: Request<Value>, sessions: Sessions) -> Response<Value> {
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
            // mu-035 Phase D: snapshot the live tracker BEFORE
            // dispatching the cancel. The tracker is updated
            // write-through by the forwarder on every ProviderStatus
            // event, so this is the best approximation of "state at
            // the moment of the cancel" we can compute server-side.
            // None means no call was outstanding — was_in = Idle and
            // canceled = false even if the send succeeds (the loop
            // is between asks and will drop the input on receipt).
            let snapshot = sessions.provider_status_snapshot(&params.session_id);
            let was_in = snapshot
                .as_ref()
                .map(|s| s.kind)
                .unwrap_or(ProviderStatusKind::Idle);
            let had_outstanding = snapshot.is_some();

            let reason = params.reason.unwrap_or_else(|| "client request".into());
            // best-effort — if the loop already terminated, send fails
            // silently and we report canceled=false.
            let send_ok = tx
                .send(AgentInput::CancelOutstanding {
                    reason: reason.clone(),
                })
                .await
                .is_ok();
            let resp = CancelOutstandingResponse {
                canceled: send_ok && had_outstanding,
                was_in,
            };
            ok_response(request.id, to_value_or_null(resp))
        }
    }
}

/// mu-035 Phase D: `daemon.outstanding_calls` — fleet view of every
/// in-flight provider call across all sessions on this daemon. Used
/// by the TUI command-centre view. Each session's tracker is updated
/// write-through by the forwarder; this handler just snapshots the
/// registry and computes per-call `elapsed_ms` against a single
/// `now_unix_ms` so all rows in one response are consistent.
fn handle_outstanding_calls(request: Request<Value>, sessions: Sessions) -> Response<Value> {
    let now = super::discovery::now_unix_ms();
    let calls = sessions.snapshot_outstanding_calls(now);
    let resp = DaemonOutstandingCallsResponse {
        calls,
        snapshot_at_unix_ms: now,
    };
    ok_response(request.id, to_value_or_null(resp))
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
    let params: RespondToInputRequiredRequest = match serde_json::from_value(request.params.clone())
    {
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
    let params: SessionListRequest = match serde_json::from_value(request.params.clone()) {
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

fn handle_session_events(request: Request<Value>, sessions: Sessions) -> Response<Value> {
    let params: SessionEventsRequest = match serde_json::from_value(request.params.clone()) {
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
    let _params: DaemonStatsRequest = match serde_json::from_value(request.params.clone()) {
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
            total_input_tokens = total_input_tokens.saturating_add(u.input_tokens);
            total_output_tokens = total_output_tokens.saturating_add(u.output_tokens);
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
fn handle_daemon_usage_history(request: Request<Value>, sessions: Sessions) -> Response<Value> {
    let params: DaemonUsageHistoryRequest = match serde_json::from_value(request.params.clone()) {
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

/// mu-036 Phase B: session.start_autonomous handler.
///
/// Validates the session exists and that its capability includes
/// `AutonomyCapability::Allowed` (INV-1 enforcement). On pass, sends
/// `AgentInput::StartAutonomous { goal, options }` into the session's
/// input channel; the agent loop transitions into `RunMode::Autonomous`
/// and drives the iteration cycle. Bounds (`max_iterations`,
/// `max_wall_clock_ms`, `max_total_tool_calls_in_autonomy`) are read
/// from the session's `Capability` at every iteration boundary, NOT
/// from `options` — INV-2 (options can narrow but never widen).
async fn handle_start_autonomous(request: Request<Value>, sessions: Sessions) -> Response<Value> {
    use mu_core::capability::AutonomyCapability;

    let params: StartAutonomousRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("session.start_autonomous: invalid params: {e}"),
            );
        }
    };

    let cap_handle = match sessions.capability(&params.session_id) {
        Some(c) => c,
        None => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!(
                    "session.start_autonomous: session not found: {}",
                    params.session_id
                ),
            );
        }
    };
    let cap_snapshot = cap_handle
        .lock()
        .map(|c| c.clone())
        .unwrap_or_else(|_| Default::default());
    if matches!(cap_snapshot.autonomy, AutonomyCapability::Disallowed) {
        return err_response(
            request.id,
            codes::INVALID_PARAMS,
            "session.start_autonomous: session capability has \
             autonomy: Disallowed (INV-1; default for sessions \
             created via create_session)"
                .to_string(),
        );
    }

    let sender = sessions.input_sender(&params.session_id);
    match sender {
        None => err_response(
            request.id,
            codes::INVALID_PARAMS,
            format!(
                "session.start_autonomous: session not found: {}",
                params.session_id
            ),
        ),
        Some(tx) => {
            match tx
                .send(AgentInput::StartAutonomous {
                    goal: params.goal,
                    options: params.options,
                })
                .await
            {
                Ok(_) => {
                    let resp = StartAutonomousResponse { accepted: true };
                    ok_response(request.id, to_value_or_null(resp))
                }
                Err(_) => err_response(
                    request.id,
                    codes::INTERNAL_ERROR,
                    "session.start_autonomous: session loop has terminated",
                ),
            }
        }
    }
}

/// mu-036 Phase A.2: stub for session.schedule_wakeup. Same shape
/// as handle_start_autonomous — wire surface is complete, agent-
/// loop wiring is Phase C (mu-7zn).
fn handle_schedule_wakeup(request: Request<Value>, sessions: Sessions) -> Response<Value> {
    use mu_core::capability::AutonomyCapability;

    let params: ScheduleWakeupRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("session.schedule_wakeup: invalid params: {e}"),
            );
        }
    };
    if params.wake_at_unix_ms.is_some() == params.sleep_for_ms.is_some() {
        return err_response(
            request.id,
            codes::INVALID_PARAMS,
            "session.schedule_wakeup: exactly one of wake_at_unix_ms / \
             sleep_for_ms must be set"
                .to_string(),
        );
    }
    let cap_handle = match sessions.capability(&params.session_id) {
        Some(c) => c,
        None => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!(
                    "session.schedule_wakeup: session not found: {}",
                    params.session_id
                ),
            );
        }
    };
    let cap_snapshot = cap_handle
        .lock()
        .map(|c| c.clone())
        .unwrap_or_else(|_| Default::default());
    let allowed_wakeup = match cap_snapshot.autonomy {
        AutonomyCapability::Allowed {
            allow_schedule_wakeup,
            ..
        } => allow_schedule_wakeup,
        AutonomyCapability::Disallowed => false,
    };
    if !allowed_wakeup {
        return err_response(
            request.id,
            codes::INVALID_PARAMS,
            "session.schedule_wakeup: session capability does not permit \
             schedule_wakeup (AutonomyCapability::Disallowed, or Allowed \
             with allow_schedule_wakeup: false)"
                .to_string(),
        );
    }
    err_response(
        request.id,
        codes::INTERNAL_ERROR,
        "session.schedule_wakeup: wire surface ready (Phase A.2); \
         agent-loop integration is Phase C (bead mu-7zn)."
            .to_string(),
    )
}

// ───────────────────────── mu-lho (mu-037 Phase 1) ─────────────────────────

/// `peer.hello` — A asks B for a peer handle. v1 policy: accept any
/// same-daemon peer whose `want.method` is `"mailbox.post"`. The
/// target session issues a fresh opaque token with
/// `allowed_methods = {mailbox.post}` and no expiry. Future iterations
/// make this policy programmable per-target-session.
fn handle_peer_hello(
    request: Request<Value>,
    sessions: Sessions,
    _daemon_info: DaemonInfo,
) -> Response<Value> {
    let params: PeerHelloRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("peer.hello: invalid params: {e}"),
            );
        }
    };

    // Target must exist.
    let mailbox = match sessions.mailbox(&params.to_session_id) {
        Some(m) => m,
        None => {
            return ok_response(
                request.id,
                to_value_or_null(PeerHelloResponse::Denied {
                    reason: format!("unknown target session: {}", params.to_session_id),
                }),
            );
        }
    };

    // v1 default policy: accept only `mailbox.post`.
    let response = if params.want.method == MailboxPostRequest::METHOD {
        let allowed: std::collections::HashSet<String> =
            std::iter::once(MailboxPostRequest::METHOD.to_owned()).collect();
        let token = mailbox.issue_handle(
            params.from.session_id.clone(),
            allowed.clone(),
            None, // no expiry in Phase 1
            None, // no per-handle call budget in Phase 1
        );
        PeerHelloResponse::Accepted {
            peer_handle: token,
            allowed_methods: allowed.into_iter().collect(),
            expires_at_unix_ms: None,
        }
    } else {
        PeerHelloResponse::Denied {
            reason: format!(
                "v1 policy refuses method `{}`; only `mailbox.post` is offered",
                params.want.method,
            ),
        }
    };

    ok_response(request.id, to_value_or_null(response))
}

/// `mailbox.post` — peer A drops a message into B's mailbox. Requires
/// a valid peer handle previously obtained from `peer.hello`. Appends
/// a `MailboxMessagePosted` event to the target session's event log
/// and emits a `session.mailbox_message` wire notification.
async fn handle_mailbox_post(
    request: Request<Value>,
    sessions: Sessions,
    notif: NotificationWriter,
    daemon_info: DaemonInfo,
) -> Response<Value> {
    let params: MailboxPostRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("mailbox.post: invalid params: {e}"),
            );
        }
    };

    let target_mailbox = match sessions.mailbox(&params.to_session_id) {
        Some(m) => m,
        None => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("session not found: {}", params.to_session_id),
            );
        }
    };

    // Authorization: require a valid peer handle issued by target.
    // Note: same-daemon trust intentionally NOT applied here — even
    // when sender and recipient are in-process, the handshake must
    // happen first. This avoids carving a Phase-1-only shortcut.
    if target_mailbox
        .check_handle(
            &params.peer_handle,
            &params.from.session_id,
            MailboxPostRequest::METHOD,
        )
        .is_none()
    {
        return err_response(
            request.id,
            codes::INVALID_PARAMS,
            "mailbox.post: invalid or expired peer handle".to_string(),
        );
    }

    // Verify the claimed `from.daemon_id` matches this daemon. Phase
    // 1 is single-daemon; future Phase 2 cross-daemon will gate this
    // differently.
    if params.from.daemon_id != daemon_info.daemon_id() {
        return err_response(
            request.id,
            codes::INVALID_PARAMS,
            format!(
                "mailbox.post: from.daemon_id `{}` does not match this daemon",
                params.from.daemon_id
            ),
        );
    }

    let log = match sessions.event_log(&params.to_session_id) {
        Some(l) => l,
        None => {
            // Race: session vanished between `mailbox()` and now.
            // Treat as "session not found."
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                "mailbox.post: target session no longer exists".to_string(),
            );
        }
    };

    let seq = target_mailbox.allocate_seq();
    // EventActor for a peer-originated post: the daemon mediated the
    // append, so `System` is the closest available variant. Peer
    // identity is carried in the payload's `from_daemon_id` /
    // `from_session_id` fields. A future spec can add
    // `EventActor::Peer { daemon_id, session_id }` if needed.
    let posted_event_id = log.append(
        EventActor::System,
        EventPayload::MailboxMessagePosted {
            seq,
            from_daemon_id: params.from.daemon_id.clone(),
            from_session_id: params.from.session_id.clone(),
            message_kind: params.kind.clone(),
            subject: params.subject.clone(),
            body: params.body.clone(),
            expires_at_unix_ms: params.expires_at_unix_ms,
        },
    );
    let posted_at_unix_ms = log
        .snapshot()
        .iter()
        .find(|e| e.id == posted_event_id)
        .map(|e| e.timestamp_unix_ms)
        .unwrap_or(0);

    // Wire notification — Phase 4 TUI (F9 mailbox view) subscribes.
    let notif_payload = mu_core::protocol::MailboxMessageEvent {
        session_id: params.to_session_id.clone(),
        seq,
        from_daemon_id: params.from.daemon_id.clone(),
        from_session_id: params.from.session_id.clone(),
        kind: params.kind.clone(),
        subject: params.subject.clone(),
        body: params.body.clone(),
        posted_at_unix_ms,
        expires_at_unix_ms: params.expires_at_unix_ms,
    };
    if let Ok(value) = serde_json::to_value(&notif_payload) {
        let _ = notif
            .emit(mu_core::protocol::MailboxMessageEvent::METHOD, value)
            .await;
    }

    ok_response(
        request.id,
        to_value_or_null(MailboxPostResponse { posted: true, seq }),
    )
}

/// `mailbox.list` — read a session's mailbox. Projects from the event
/// log: posts minus consumed. Self-access (a session listing its own
/// mailbox) doesn't require a handle; cross-session listing does.
fn handle_mailbox_list(request: Request<Value>, sessions: Sessions) -> Response<Value> {
    let params: MailboxListRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("mailbox.list: invalid params: {e}"),
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

    let messages = project_mailbox(&log, params.since_seq, params.include_consumed);
    ok_response(
        request.id,
        to_value_or_null(MailboxListResponse { messages }),
    )
}

/// `mailbox.consume` — mark messages as consumed. Each unknown or
/// already-consumed seq is silently skipped; the response reports
/// how many transitioned.
fn handle_mailbox_consume(request: Request<Value>, sessions: Sessions) -> Response<Value> {
    let params: MailboxConsumeRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("mailbox.consume: invalid params: {e}"),
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

    // Compute current consumed-set and posted-set from the log to
    // skip duplicates / unknowns.
    let (posted_seqs, consumed_seqs) = posted_and_consumed_sets(&log);
    let mut consumed_count: u32 = 0;
    for seq in &params.seqs {
        if !posted_seqs.contains(seq) {
            continue; // unknown — skip
        }
        if consumed_seqs.contains(seq) {
            continue; // already consumed — skip
        }
        log.append(
            EventActor::System,
            EventPayload::MailboxMessageConsumed { seq: *seq },
        );
        consumed_count = consumed_count.saturating_add(1);
    }

    ok_response(
        request.id,
        to_value_or_null(MailboxConsumeResponse { consumed_count }),
    )
}

/// Project the mailbox view from a session's event log. Pure function;
/// no IO. Walks the log once gathering posted entries and a consumed
/// set, then composes the final `MailboxMessageView` list filtering
/// per `since_seq` and `include_consumed`. Order is by `seq` ascending
/// (which equals event-log append order since `seq` is monotonic).
fn project_mailbox(
    log: &SessionEventLog,
    since_seq: Option<u64>,
    include_consumed: bool,
) -> Vec<MailboxMessageView> {
    let events = log.snapshot();
    let mut consumed = std::collections::HashSet::<u64>::new();
    for ev in events.iter().rev() {
        if let EventPayload::MailboxMessageConsumed { seq } = &ev.payload {
            consumed.insert(*seq);
        }
    }
    let mut out: Vec<MailboxMessageView> = Vec::new();
    for ev in &events {
        if let EventPayload::MailboxMessagePosted {
            seq,
            from_daemon_id,
            from_session_id,
            message_kind,
            subject,
            body,
            expires_at_unix_ms,
        } = &ev.payload
        {
            if let Some(threshold) = since_seq {
                if *seq < threshold {
                    continue;
                }
            }
            let was_consumed = consumed.contains(seq);
            if was_consumed && !include_consumed {
                continue;
            }
            out.push(MailboxMessageView {
                seq: *seq,
                from_daemon_id: from_daemon_id.clone(),
                from_session_id: from_session_id.clone(),
                kind: message_kind.clone(),
                subject: subject.clone(),
                body: body.clone(),
                posted_at_unix_ms: ev.timestamp_unix_ms,
                consumed: was_consumed,
                expires_at_unix_ms: *expires_at_unix_ms,
            });
        }
    }
    out
}

/// Helper: gather the (posted_seqs, consumed_seqs) sets in one pass.
fn posted_and_consumed_sets(
    log: &SessionEventLog,
) -> (
    std::collections::HashSet<u64>,
    std::collections::HashSet<u64>,
) {
    let mut posted = std::collections::HashSet::<u64>::new();
    let mut consumed = std::collections::HashSet::<u64>::new();
    for ev in log.snapshot().iter() {
        match &ev.payload {
            EventPayload::MailboxMessagePosted { seq, .. } => {
                posted.insert(*seq);
            }
            EventPayload::MailboxMessageConsumed { seq } => {
                consumed.insert(*seq);
            }
            _ => {}
        }
    }
    (posted, consumed)
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
        EventPayload::AutonomousIterationStarted { .. } => "autonomous_iteration_started",
        EventPayload::AutonomousIterationCompleted { .. } => "autonomous_iteration_completed",
        EventPayload::AutonomousScheduledWakeup { .. } => "autonomous_scheduled_wakeup",
        EventPayload::AutonomousTerminated { .. } => "autonomous_terminated",
        EventPayload::MailboxMessagePosted { .. } => "mailbox_message_posted",
        EventPayload::MailboxMessageConsumed { .. } => "mailbox_message_consumed",
    }
}

// ───────────────────────── mu-7rk (mu-yox) ─────────────────────────
//
// Connect-time SASL-shaped auth handshake (handler + dispatcher half).
//
// Two RPCs:
//   peer.auth_offer    — server lists supported mechanisms
//   peer.auth_initiate — caller picks mechanism + submits initial creds
//
// `peer.auth_response` is reserved on the wire (mu-vha) but no
// multi-step state registry exists yet — that's mu-oeo (mu-7rk-g).
// Until then, the dispatcher does NOT route the method (it falls
// through to METHOD_NOT_FOUND), keeping the surface honest.
//
// On `Accepted`, the per-connection [`AuthState`] is updated to
// `Authenticated { capability }`. Nothing else in this dispatcher
// consumes that state — enforcement on session.\*/mailbox.\* RPCs is
// mu-fnn (mu-7rk-c).

fn handle_auth_offer(request: Request<Value>, auth_registry: &AuthRegistry) -> Response<Value> {
    let _params: AuthOfferRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("peer.auth_offer: invalid params: {e}"),
            );
        }
    };
    let resp = AuthOfferResponse {
        mechanisms: auth_registry.offered(),
    };
    ok_response(request.id, to_value_or_null(resp))
}

fn handle_auth_initiate(
    request: Request<Value>,
    auth_registry: &AuthRegistry,
    auth_state: &AuthStateHandle,
) -> Response<Value> {
    let params: AuthInitiateRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("peer.auth_initiate: invalid params: {e}"),
            );
        }
    };
    let handler = match auth_registry.get(&params.mechanism) {
        Some(h) => h,
        None => {
            let resp = AuthExchangeResponse::Denied {
                code: AuthDenialCode::UnsupportedMechanism,
                reason: format!("no handler registered for mechanism `{}`", params.mechanism,),
            };
            return ok_response(request.id, to_value_or_null(resp));
        }
    };
    let outcome = handler.step_initial(params.initial_response.as_deref());
    let resp = outcome_to_response(outcome, auth_state);
    ok_response(request.id, to_value_or_null(resp))
}

/// Convert a handler step outcome into the wire response. On
/// `Done(capability)`, the per-connection `AuthState` is updated to
/// `Authenticated { capability }`. Recording-only in this bead — no
/// other arm in this dispatcher reads `AuthState` yet (mu-fnn).
///
/// mu-m84: when the per-connection mutex is poisoned at the moment a
/// `Done(_)` outcome arrives, the connection's `AuthState` cannot be
/// safely transitioned to `Authenticated`. Answering `Accepted`
/// regardless (pre-fix behavior) is a lying response — the client
/// believes it is authenticated while the server's state stays at
/// `Unauthenticated`, so every subsequent protected RPC (once mu-fnn
/// lands enforcement) would surface `auth_required` and trigger a
/// retry loop. We instead surface the lock failure as
/// `Denied { MalformedExchange, .. }`. `MalformedExchange` is the
/// closest existing variant; adding a new `AuthDenialCode` for
/// "internal state error" is mu-fnn surface, not mu-m84's.
fn outcome_to_response(
    outcome: AuthStepOutcome,
    auth_state: &AuthStateHandle,
) -> AuthExchangeResponse {
    match outcome {
        AuthStepOutcome::Done(capability) => match auth_state.lock() {
            Ok(mut s) => {
                *s = AuthState::Authenticated {
                    capability: capability.clone(),
                };
                AuthExchangeResponse::Accepted {
                    granted_capability: capability,
                }
            }
            Err(_poisoned) => AuthExchangeResponse::Denied {
                code: AuthDenialCode::MalformedExchange,
                reason: "internal state error".into(),
            },
        },
        AuthStepOutcome::Denied { code, reason } => AuthExchangeResponse::Denied { code, reason },
        AuthStepOutcome::Challenge {
            server_state_id,
            server_data,
        } => AuthExchangeResponse::Continue {
            server_state_id,
            challenge: server_data.unwrap_or_default(),
        },
    }
}

#[cfg(test)]
mod tests {
    //! mu-m84: poisoned-mutex regression coverage for
    //! `outcome_to_response`. Lives inline because the function is
    //! private to this module; integration tests in
    //! `crates/mu-coding/tests/auth_smoke.rs` would require exposing
    //! it as `pub`, which is API-surface creep for a test-only need.

    use super::*;

    /// mu-m84: when the per-connection `AuthState` mutex is poisoned
    /// at the moment a `Done(_)` outcome arrives, the dispatcher must
    /// NOT answer `Accepted` (a lying success) — it must answer
    /// `Denied { MalformedExchange, .. }`. Pre-fix, the lock failure
    /// was silently swallowed by `if let Ok(...)` and `Accepted` was
    /// returned regardless, leaving the state at `Unauthenticated`
    /// while the client believed it was in.
    #[test]
    fn bearer_done_under_lock_poison_does_not_respond_accepted() {
        let handle: AuthStateHandle = Arc::new(Mutex::new(AuthState::Unauthenticated));

        // Poison the mutex by panicking a background thread while it
        // holds the lock. `.join()` returns `Err(_)` once the panic
        // unwinds; we ignore it — the side effect we care about is
        // the now-poisoned state of `handle`.
        let poison = Arc::clone(&handle);
        let join_result = std::thread::spawn(move || {
            let _g = poison
                .lock()
                .expect("test setup: acquire lock to intentionally poison");
            panic!("mu-m84 test setup: intentional poison");
        })
        .join();
        assert!(
            join_result.is_err(),
            "test setup: poisoner thread must have panicked",
        );
        assert!(
            handle.is_poisoned(),
            "test setup: mutex must be poisoned after the panicking holder",
        );

        let outcome = AuthStepOutcome::Done(Capability::root());
        let resp = outcome_to_response(outcome, &handle);

        match resp {
            AuthExchangeResponse::Denied { code, .. } => {
                assert_eq!(
                    code,
                    AuthDenialCode::MalformedExchange,
                    "poisoned-lock denial must reuse MalformedExchange, not a new variant",
                );
            }
            other => {
                panic!("expected Denied{{MalformedExchange}} under poisoned lock; got {other:?}",)
            }
        }

        // State must remain `Unauthenticated` — we surfaced the
        // failure to the client and did not unilaterally upgrade the
        // session.
        let guard = handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            matches!(*guard, AuthState::Unauthenticated),
            "AuthState must stay Unauthenticated after poisoned-lock denial; got {:?}",
            *guard,
        );
    }
}
