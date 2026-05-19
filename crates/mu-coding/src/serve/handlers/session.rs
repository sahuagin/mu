//! Session-domain request handlers (session.*, autonomy-related).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use mu_core::agent::{AgentConfig, AgentInput, AgentLoop, AgentMessage, Tool};
use mu_core::capability::Capability;
use mu_core::event_log::{EventActor, EventPayload, SessionEventLog};
use mu_core::protocol::{
    AskSessionRequest, AskSessionResponse, CancelOutstandingRequest, CancelOutstandingResponse,
    CancelSessionRequest, CancelSessionResponse, CloseSessionRequest, CloseSessionResponse,
    CreateSessionRequest, CreateSessionResponse, DelegateSessionRequest, DelegateSessionResponse,
    PingResponse, ProviderSelector, Request, RespondToInputRequiredRequest,
    RespondToInputRequiredResponse, Response, ScheduleWakeupRequest, SessionEventsRequest,
    SessionEventsResponse, SessionListRequest, SessionListResponse, SessionStatsRequest,
    SessionStatsResponse, StartAutonomousRequest, StartAutonomousResponse,
};
use mu_core::transport::{codes, err_response, ok_response, NotificationWriter};

use crate::serve::daemon_info::DaemonInfo;
use crate::serve::discovery::SessionDiscovery;
use crate::serve::factory::ProviderFactory;
use crate::serve::forwarder::forward_events;
use crate::serve::sessions::Sessions;

use super::to_value_or_null;

pub fn handle_ping(request: Request<Value>) -> Response<Value> {
    let resp = PingResponse {
        pong: true,
        server_version: env!("CARGO_PKG_VERSION").into(),
    };
    ok_response(request.id, to_value_or_null(resp))
}

pub fn handle_create_session(
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

pub fn handle_delegate_session(
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
    // mu-779s: per-provider max_turns default. OpenAI/openrouter models
    // are chattier than Anthropic on tool-heavy reads and routinely hit
    // the default 20-turn cap; bump them so the common case stays
    // productive. Operator can still pin per-session via `--max-iterations`.
    let max_turns = mu_core::agent::loop_::default_max_turns_for(&kind_str);
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
        super::super::provider_status::ProviderStatusTracker::new(),
    ));
    let mailbox = Arc::new(super::super::mailbox::MailboxState::new());
    let (events_tx, events_rx) = tokio::sync::mpsc::channel(64);
    let session_tools: Vec<Arc<dyn Tool>> = (*tools).clone();
    let agent = AgentLoop::spawn(
        provider,
        session_tools,
        AgentConfig {
            system_prompt,
            max_turns,
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

pub async fn handle_ask_session(request: Request<Value>, sessions: Sessions) -> Response<Value> {
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

pub async fn handle_cancel_session(request: Request<Value>, sessions: Sessions) -> Response<Value> {
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

pub async fn handle_cancel_outstanding(
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
                .unwrap_or(mu_core::protocol::ProviderStatusKind::Idle);
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

pub fn handle_session_stats(request: Request<Value>, sessions: Sessions) -> Response<Value> {
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

pub fn handle_respond_to_input_required(
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

pub fn handle_close_session(request: Request<Value>, sessions: Sessions) -> Response<Value> {
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

pub async fn handle_session_list(
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
    let now_ms = super::super::discovery::now_unix_ms();
    match discovery.list(&filter).await {
        Ok(sessions) => {
            let resp = SessionListResponse {
                sessions,
                snapshot_at_unix_ms: now_ms,
                failed_peers: Vec::new(),
            };
            ok_response(request.id, to_value_or_null(resp))
        }
        Err(crate::serve::discovery::DiscoveryError::PartialFailure {
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
        Err(crate::serve::discovery::DiscoveryError::Backend(msg)) => err_response(
            request.id,
            codes::INTERNAL_ERROR,
            format!("session.list: backend error: {msg}"),
        ),
    }
}

pub fn handle_session_events(request: Request<Value>, sessions: Sessions) -> Response<Value> {
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
pub async fn handle_start_autonomous(
    request: Request<Value>,
    sessions: Sessions,
) -> Response<Value> {
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
pub fn handle_schedule_wakeup(request: Request<Value>, sessions: Sessions) -> Response<Value> {
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
        EventPayload::TaskTelemetry { .. } => "task_telemetry",
    }
}

#[cfg(test)]
mod tests {
    //! mu-u1ld phase B: verify that read-only queries against
    //! rehydrated sessions route through `Sessions::event_log(id)` and
    //! produce the same response shape as live sessions.
    //!
    //! Phase A added `insert_rehydrated` and made `event_log` consult
    //! both maps; the handlers below (`handle_session_stats`,
    //! `handle_session_events`) already go through that path. These
    //! tests pin that contract.

    use super::*;
    use mu_core::event_log::SessionEventLog;
    use mu_core::protocol::JSONRPC_VERSION;
    use serde_json::json;

    fn rehydrated_session_with_events(session_id: &str) -> Sessions {
        let sessions = Sessions::new();
        let log = SessionEventLog::new(session_id.to_string());
        log.append(
            EventActor::System,
            EventPayload::SessionCreated {
                provider_kind: "anthropic_api".into(),
                model: "haiku".into(),
                parent_session_id: None,
                branched_at_parent_event_id: None,
            },
        );
        log.append(
            EventActor::User,
            EventPayload::UserMessage {
                content: "hello".into(),
            },
        );
        log.append(
            EventActor::System,
            EventPayload::Done {
                stop_reason: mu_core::agent::StopReason::EndTurn,
                usage: None,
                turn_count: 1,
                elapsed_ms: Some(42),
            },
        );
        sessions.insert_rehydrated(session_id.to_string(), Arc::new(log), None);
        sessions
    }

    #[test]
    fn session_stats_works_for_rehydrated_session() {
        let session_id = "ghost-stats";
        let sessions = rehydrated_session_with_events(session_id);

        let req = Request {
            jsonrpc: JSONRPC_VERSION.into(),
            id: json!(1),
            method: "session.stats".into(),
            params: json!({ "session_id": session_id }),
        };
        let resp = handle_session_stats(req, sessions);
        let value = serde_json::to_value(resp).expect("serialize response");
        let result = value
            .get("result")
            .expect("response must have a result, not an error");
        assert_eq!(result["session_id"], session_id);
        assert_eq!(result["provider_kind"], "anthropic_api");
        assert_eq!(result["model"], "haiku");
        assert_eq!(result["event_count"], 3);
        assert_eq!(result["ask_count"], 1);
        assert_eq!(result["elapsed_total_ms"], 42);
    }

    #[test]
    fn session_events_works_for_rehydrated_session() {
        let session_id = "ghost-events";
        let sessions = rehydrated_session_with_events(session_id);

        let req = Request {
            jsonrpc: JSONRPC_VERSION.into(),
            id: json!(2),
            method: "session.events".into(),
            params: json!({ "session_id": session_id }),
        };
        let resp = handle_session_events(req, sessions);
        let value = serde_json::to_value(resp).expect("serialize response");
        let result = value
            .get("result")
            .expect("response must have a result, not an error");
        let events = result["events"].as_array().expect("events array");
        assert_eq!(events.len(), 3);
        assert_eq!(events[0]["payload"]["kind"], "session_created");
        assert_eq!(events[2]["payload"]["kind"], "done");
        assert_eq!(result["end_of_log"], true);
    }

    #[test]
    fn session_stats_returns_not_found_for_unknown_session() {
        // Sanity check: nonexistent IDs still get the error shape;
        // rehydrated lookup doesn't accidentally fall through to a
        // synthetic-empty response.
        let sessions = Sessions::new();
        let req = Request {
            jsonrpc: JSONRPC_VERSION.into(),
            id: json!(3),
            method: "session.stats".into(),
            params: json!({ "session_id": "never-existed" }),
        };
        let resp = handle_session_stats(req, sessions);
        let value = serde_json::to_value(resp).expect("serialize response");
        assert!(
            value.get("error").is_some(),
            "expected an error response, got {value}"
        );
    }
}
