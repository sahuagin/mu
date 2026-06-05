//! Session-domain request handlers (session.*, autonomy-related).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use std::path::PathBuf;

use mu_core::agent::{AgentConfig, AgentInput, AgentLoop, AgentMessage, SpawnArgs, Tool};
use mu_core::capability::Capability;
use mu_core::context::rope::SpanText;
use mu_core::context::CacheTtl;
use mu_core::context::{ProjectContext, RecalledItem};
use mu_core::event_log::{EventActor, EventPayload, SessionEventLog};
use mu_core::protocol::{
    AskSessionRequest, AskSessionResponse, CancelOutstandingRequest, CancelOutstandingResponse,
    CancelSessionRequest, CancelSessionResponse, CloseSessionRequest, CloseSessionResponse,
    CreateSessionRequest, CreateSessionResponse, DelegateSessionRequest, DelegateSessionResponse,
    PingResponse, ProviderSelector, Request, RespondToInputRequiredRequest,
    RespondToInputRequiredResponse, Response, ScheduleWakeupRequest, SessionEventsRequest,
    SessionEventsResponse, SessionListRequest, SessionListResponse, SessionStatsRequest,
    SessionStatsResponse, SetRouteRequest, SetRouteResponse, SpawnWorkerRequest,
    SpawnWorkerResponse, StartAutonomousRequest, StartAutonomousResponse,
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
        cwd: params.cwd,                     // mu-phl v0 / mu-045
        parent_session_id: None,             // no parent — this is a root session
        branched_at_parent_event_id: None,
        capability: Capability::root(), // root session: unrestricted
        cache_ttl: params.cache_ttl.unwrap_or_default(), // mu-f1a0
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
        cwd: params.cwd,     // mu-phl v0 / mu-045
        parent_session_id: Some(params.parent_session_id.clone()),
        branched_at_parent_event_id: params.branched_at_parent_event_id,
        capability: child_capability,
        // mu-f1a0: delegated workers are PINNED to the 5m tier
        // regardless of the parent's — they run gap-free tool loops,
        // so the 1h tier's 2x write premium is pure cost (operator
        // requirement, bead body).
        cache_ttl: CacheTtl::FiveMinutes,
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
    /// mu-phl v0 / mu-045: operator's cwd at session creation time.
    /// Used to scope the recall providers attached via
    /// [`DaemonInfo::recall_providers`]. None → daemon falls back to
    /// `std::env::current_dir()` (back-compat).
    cwd: Option<PathBuf>,
    parent_session_id: Option<String>,
    branched_at_parent_event_id: Option<u64>,
    capability: Capability,
    /// mu-f1a0: prompt-cache TTL tier for this session's provider.
    cache_ttl: CacheTtl,
    // runtime deps (daemon-global)
    notif: NotificationWriter,
    sessions: Sessions,
    factory: ProviderFactory,
    tools: Arc<Vec<Arc<dyn Tool>>>,
    daemon_info: &'a DaemonInfo,
}

/// Per-session tool list: the daemon's base tools plus a `spawn_worker`
/// tool scoped to THIS session, so a worker's results route back to this
/// session's mailbox (waking it) rather than the dead "supervisor" ghost.
/// The spawn_worker tool is only added in production (events_dir set) —
/// tests have no pot infrastructure. (mu-slat)
fn session_spawn_tools(
    base: &[Arc<dyn Tool>],
    sessions: &Sessions,
    daemon_info: &DaemonInfo,
    session_id: &str,
) -> Vec<Arc<dyn Tool>> {
    let mut tools = base.to_vec();
    if daemon_info.events_dir().is_some() {
        tools.push(Arc::new(crate::tools::SpawnWorkerTool::new(
            // mu-qc08: a WEAK handle — a strong clone here deadlocks
            // shutdown (the tool lives in this session's own tool list).
            sessions.downgrade(),
            daemon_info.clone(),
            Some(session_id.to_string()),
        )));
    }
    tools
}

/// Shared session-creation logic for both `create_session` (root) and
/// `session.delegate` (child). Returns the new session_id on success
/// or a human-readable error on provider-construction failure.
fn build_and_register_session(req: BuildSessionRequest<'_>) -> Result<String, String> {
    let BuildSessionRequest {
        selector,
        system_prompt,
        cwd,
        parent_session_id,
        branched_at_parent_event_id,
        capability,
        notif,
        sessions,
        factory,
        tools,
        daemon_info,
        cache_ttl,
    } = req;
    let provider =
        factory(selector, cache_ttl).map_err(|e| format!("could not build provider: {e}"))?;

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
    let kind_arc: Arc<str> = Arc::from(kind_str.as_str());
    let model_arc: Arc<str> = Arc::from(model_str.as_str());
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
            // mu-rf9x: register the provider's token-accounting
            // convention so log readers can interpret every usage
            // record in this session without provider arithmetic.
            usage_semantics: Some(provider.capabilities().usage_semantics),
        },
    );

    let pending_approvals = Arc::new(Mutex::new(HashMap::new()));
    // mu-phl v0 / mu-0bxv: build the new session's project context by
    // iterating the daemon's recall providers (set up at daemon startup
    // via DaemonInfo::with_recall_providers) against the effective cwd
    // and the session's capability. Computed BEFORE capability is moved
    // into capability_handle so we can borrow it.
    let project_context = build_project_context(daemon_info, cwd.as_deref(), &capability);
    let capability_handle = Arc::new(Mutex::new(capability));
    let provider_status = Arc::new(Mutex::new(
        super::super::provider_status::ProviderStatusTracker::new(),
    ));
    let mailbox = Arc::new(super::super::mailbox::MailboxState::new());
    let (events_tx, events_rx) = tokio::sync::mpsc::channel(64);
    let mut session_tools =
        session_spawn_tools(tools.as_slice(), &sessions, daemon_info, &session_id);
    // mu-onq8: always-on in-loop capability discovery. Ranks the session's
    // sibling tools (attenuated by this session's capability) against a
    // free-text intent, so the agent can find the right tool in-loop instead
    // of shelling out to the allowlist-blocked bash path. Built over a
    // snapshot of the siblings (excludes itself). Skills are not yet threaded
    // here (tools-only v1); the daemon's discovered skills join in the mu-onq8
    // follow-up (pairs with mu-re0s).
    let discover_siblings = Arc::new(session_tools.clone());
    session_tools.push(Arc::new(crate::tools::DiscoverTool::new(
        discover_siblings,
        Arc::new(Vec::<mu_core::skill::loader::LoadedSkill>::new()),
        capability_handle.clone(),
        // mu-kex4.6.3: semantic ranking is opt-in via [index].semantic_discover.
        daemon_info.config().index.semantic_discover,
    )));
    let compaction_cfg = &daemon_info.config().compaction;
    let compaction_policy_override = resolve_compaction_policy(compaction_cfg);
    // mu-k011: discovery-bootstrap default. When session-start recall is
    // disabled (MU_NO_RECALL / `[recall].enabled = false`), an uninstructed
    // model declines instead of discovering; inject a short bootstrap so it
    // searches memory + calls the native `discover` tool (mu-onq8) on demand.
    // Conservative: applies only when the operator supplied no system prompt
    // of their own — see compose_system_prompt for the design rationale.
    let recall_enabled = daemon_info.config().recall_enabled();
    let effective_system_prompt =
        super::super::discovery_bootstrap::compose_system_prompt(system_prompt, recall_enabled);
    let agent = AgentLoop::spawn(SpawnArgs {
        provider,
        provider_kind: kind_arc,
        model: model_arc,
        tools: session_tools,
        config: AgentConfig {
            system_prompt: effective_system_prompt.map(SpanText::from),
            max_turns,
            project_context,
            compaction_threshold: Some(compaction_cfg.trigger_threshold_tokens),
            compaction_policy_override,
        },
        events: events_tx,
        pending_approvals: pending_approvals.clone(),
        capability: capability_handle.clone(),
    });
    let input_tx = agent.sender();
    let agent_handle = tokio::spawn(async move {
        let _ = agent.join().await;
    });
    let (status_tx, status_rx) = tokio::sync::watch::channel(None);
    let forwarder_handle = tokio::spawn(forward_events(
        session_id.clone(),
        events_rx,
        notif.clone(),
        event_log.clone(),
        provider_status.clone(),
        daemon_info.daemon_id().to_string(),
        Some(status_tx),
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
            cache_ttl,
            provider_status,
            mailbox,
            status_watch: Some(status_rx),
        },
    );

    Ok(session_id)
}

/// mu-phl v0 (mu-0bxv): iterate the daemon's recall providers against
/// the effective cwd + session capability, collecting all returned
/// items into a single [`ProjectContext`].
///
/// Returns `None` when no items were produced (either no providers
/// configured — the test default — or all providers returned empty).
/// `None` is the back-compat case: downstream
/// [`assemble_rope_with_context`] takes `Option<&ProjectContext>` and
/// no-ops on `None`, producing the pre-mu-phl rope layout.
///
/// `cwd` resolution: caller's `cwd` parameter is honored when `Some`;
/// `None` falls back to `std::env::current_dir()`. If both fail
/// (extremely unlikely), uses `/` as a defensible last resort — the
/// recall providers will produce empty results, which is correct.
fn build_project_context(
    daemon_info: &DaemonInfo,
    cwd: Option<&std::path::Path>,
    capability: &Capability,
) -> Option<ProjectContext> {
    let providers = daemon_info.recall_providers();
    if providers.is_empty() {
        return None;
    }

    let resolved_cwd: PathBuf = cwd
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));

    let mut items: Vec<RecalledItem> = Vec::new();
    for provider in providers {
        items.extend(provider.recall(&resolved_cwd, capability));
    }

    if items.is_empty() {
        None
    } else {
        Some(ProjectContext { items })
    }
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
        ProviderSelector::Ollama { model } => ("ollama".into(), model.clone()),
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

/// mu-k56u: swap the provider+model on a live session between turns.
pub async fn handle_set_route(
    request: Request<Value>,
    sessions: Sessions,
    factory: ProviderFactory,
    daemon_info: DaemonInfo,
) -> Response<Value> {
    let params: SetRouteRequest = match serde_json::from_value(request.params) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("invalid set_route params: {e}"),
            )
        }
    };

    let (kind_str, model_str) = describe_selector(&params.provider);

    let catalog = daemon_info.route_catalog();
    if catalog.find(&kind_str, &model_str).is_none() {
        let available: Vec<String> = catalog
            .configured_entries()
            .filter(|e| e.provider_kind.as_ref() == kind_str)
            .map(|e| e.model.to_string())
            .collect();
        let suggestion = if available.is_empty() {
            format!("no configured models for provider {kind_str}")
        } else {
            format!("available models for {kind_str}: {}", available.join(", "))
        };
        return err_response(
            request.id,
            codes::INVALID_PARAMS,
            format!("unknown route {kind_str}/{model_str}. {suggestion}"),
        );
    }

    // mu-f1a0: a live route swap must preserve the session's cache
    // TTL tier — silently dropping an interactive session to 5m on a
    // model switch would re-introduce the expiry re-pays the tier
    // exists to prevent.
    let route_ttl = sessions.cache_ttl(&params.session_id).unwrap_or_default();
    let provider = match factory(&params.provider, route_ttl) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INTERNAL_ERROR,
                format!("could not build provider: {e}"),
            )
        }
    };

    let input_tx = match sessions.input_sender(&params.session_id) {
        Some(tx) => tx,
        None => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("session not found: {}", params.session_id),
            )
        }
    };

    let input = AgentInput::SwitchProvider {
        provider,
        provider_kind: Arc::from(kind_str.as_str()),
        model: Arc::from(model_str.as_str()),
    };

    if input_tx.send(input).await.is_err() {
        return err_response(
            request.id,
            codes::INTERNAL_ERROR,
            "session agent loop has terminated".to_string(),
        );
    }

    ok_response(
        request.id,
        serde_json::to_value(SetRouteResponse {
            provider_kind: kind_str,
            model: model_str,
        })
        .unwrap_or_default(),
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
        EventPayload::CompactionAssembly { .. } => "compaction_assembly",
        EventPayload::ProviderStatusUpdate { .. } => "provider_status_update",
        EventPayload::AutonomousIterationStarted { .. } => "autonomous_iteration_started",
        EventPayload::AutonomousIterationCompleted { .. } => "autonomous_iteration_completed",
        EventPayload::AutonomousScheduledWakeup { .. } => "autonomous_scheduled_wakeup",
        EventPayload::AutonomousTerminated { .. } => "autonomous_terminated",
        EventPayload::MailboxMessagePosted { .. } => "mailbox_message_posted",
        EventPayload::MailboxMessageConsumed { .. } => "mailbox_message_consumed",
        EventPayload::TaskTelemetry { .. } => "task_telemetry",
        EventPayload::ErrorInvalidMessage { .. } => "error_invalid_message",
        EventPayload::ProviderSwitched { .. } => "provider_switched",
        EventPayload::WorkerSpawned { .. } => "worker_spawned",
        EventPayload::WorkerExited { .. } => "worker_exited",
        EventPayload::WorkerFailed { .. } => "worker_failed",
        EventPayload::WorkerTimeout { .. } => "worker_timeout",
    }
}

// ── mu-slat: spawn_worker ────────────────────────────────────────────

pub async fn handle_spawn_worker(
    request: Request<Value>,
    sessions: Sessions,
    daemon_info: DaemonInfo,
) -> Response<Value> {
    let req: SpawnWorkerRequest = match serde_json::from_value(request.params) {
        Ok(r) => r,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("bad SpawnWorkerRequest: {e}"),
            );
        }
    };

    let config = crate::serve::worker::SpawnWorkerConfig {
        prompt: req.prompt.clone(),
        model: req.model,
        pot_name: req.pot_name,
        timeout_secs: req.timeout_secs,
        parent_session_id: req.parent_session_id,
    };

    match crate::serve::worker::spawn_worker(config, sessions, daemon_info).await {
        Ok(result) => {
            let resp = SpawnWorkerResponse {
                session_id: result.session_id,
                pot_name: result.pot_name,
            };
            ok_response(request.id, to_value_or_null(resp))
        }
        Err(e) => err_response(request.id, codes::INTERNAL_ERROR, e),
    }
}

/// Resolve the per-session compaction policy from config, with legible
/// diagnostics. Closes mu-8bkf: the previous inline match wired only
/// `"heuristic"` and silently fell through to a no-op for every other
/// value — including the documented `"hash-and-summary"` — so a configured
/// `trigger_threshold_tokens` produced no compaction with no signal.
fn resolve_compaction_policy(
    cfg: &mu_core::config::CompactionConfig,
) -> Option<Arc<dyn mu_core::context::compaction::CompactionPolicy>> {
    use mu_core::context::compaction::heuristic::SpanFamilyDropPolicy;
    let heuristic = || -> Arc<dyn mu_core::context::compaction::CompactionPolicy> {
        Arc::new(SpanFamilyDropPolicy::new())
    };
    match cfg.default_policy.as_str() {
        "heuristic" => {
            tracing::info!(
                threshold = cfg.trigger_threshold_tokens,
                "compaction: heuristic span-family drop active"
            );
            Some(heuristic())
        }
        "hash-and-summary" | "hash_summary" => Some(resolve_hash_and_summary_policy(cfg)),
        other => {
            if !matches!(other, "no-compaction" | "none" | "") {
                tracing::warn!(
                    default_policy = %other,
                    "compaction: unknown default_policy; running with NO compaction \
                     (valid: heuristic, hash-and-summary, no-compaction)"
                );
            }
            if cfg.trigger_threshold_tokens > 0 && matches!(other, "no-compaction" | "none" | "") {
                tracing::warn!(
                    threshold = cfg.trigger_threshold_tokens,
                    default_policy = %other,
                    "compaction: trigger_threshold_tokens is set but default_policy is \
                     explicitly \"no-compaction\" — context will NOT be compacted. \
                     Remove the explicit no-compaction override to use the default \
                     heuristic policy, or set [compaction].default_policy = \
                     \"hash-and-summary\" for judge-backed compaction (mu-8bkf)."
                );
            }
            None
        }
    }
}

/// Build a [`HashAndSummaryPolicy`] from the `[compaction.judge]` section.
///
/// ## Judge selection (mu-kgu.11 walk)
///
/// Walk `cfg.judge.ranking` in order; the first entry whose `(provider, auth)`
/// can be constructed wins.  Construction goes through
/// [`crate::serve::factory::build_provider_from_selector`] — the same path
/// [`build_and_register_session`] uses — so there is no parallel provider-
/// construction surface.
///
/// ## Empty ranking / all-unavailable
///
/// When `ranking` is empty OR every entry fails to construct, fall back to
/// the bench canned judge ([`mu_core::context::compaction::bench::KeepHalfJudge`]).
/// This satisfies the documented contract in `CompactionJudgeConfig`: "falls
/// back to its hard-coded canned judge (mu-kgu.3 behavior)" and means
/// `hash-and-summary` works out-of-the-box with no model spend.
///
/// The canned judge fallback is always constructible (pure in-process struct),
/// so this function never fails and never falls back further to heuristic.
fn resolve_hash_and_summary_policy(
    cfg: &mu_core::config::CompactionConfig,
) -> Arc<dyn mu_core::context::compaction::CompactionPolicy> {
    use std::time::Duration;

    use mu_core::context::compaction::bench::KeepHalfJudge;
    use mu_core::context::compaction::hash_summary::{HashAndSummaryPolicy, KeepListMode};
    use mu_core::context::compaction::provider_judge::ProviderJudge;
    use mu_core::context::CacheTtl;

    use crate::serve::factory::build_provider_from_selector;

    // Walk the ranking list; first constructible entry wins.
    let judge_provider: Option<Arc<dyn mu_core::agent::Provider>> =
        cfg.judge.ranking.iter().find_map(|entry| {
            // Map the ranking entry to a ProviderSelector.  Only the
            // currently-implemented provider kinds are attempted; an
            // unrecognised provider string is logged and skipped.
            let selector = ranking_entry_to_selector(entry)?;
            match build_provider_from_selector(&selector, false, None, CacheTtl::default()) {
                Ok(p) => {
                    tracing::info!(
                        provider = %entry.provider,
                        model = %entry.model,
                        auth = %entry.auth,
                        "compaction: selected judge provider from ranking"
                    );
                    Some(p)
                }
                Err(e) => {
                    tracing::debug!(
                        provider = %entry.provider,
                        model = %entry.model,
                        auth = %entry.auth,
                        error = %e,
                        "compaction: ranking entry unavailable; trying next"
                    );
                    None
                }
            }
        });

    let output_mode = match cfg.judge.output_mode.as_str() {
        "index_keep" => KeepListMode::IndexKeep,
        _ => KeepListMode::HashKeep,
    };

    let policy = match judge_provider {
        Some(provider) => {
            // Build ProviderJudge from the winning provider.
            let mut pj = ProviderJudge::new(provider);
            if cfg.judge.timeout_secs > 0 {
                pj = pj.with_timeout(Duration::from_secs(cfg.judge.timeout_secs));
            }
            tracing::info!(
                timeout_secs = cfg.judge.timeout_secs,
                output_mode = %cfg.judge.output_mode,
                "compaction: hash-and-summary policy active with live judge"
            );
            HashAndSummaryPolicy::new(Arc::new(pj)).with_output_mode(output_mode)
        }
        None => {
            // No provider available — fall back to the bench canned judge
            // (KeepHalfJudge: deterministic, no-network, keeps every other span).
            // This satisfies the config contract "falls back to canned judge
            // (mu-kgu.3 behavior)" when ranking is empty or all unavailable.
            let ranking_count = cfg.judge.ranking.len();
            if ranking_count == 0 {
                tracing::info!(
                    output_mode = %cfg.judge.output_mode,
                    "compaction: hash-and-summary active with canned judge (no ranking \
                     configured; zero model spend)"
                );
            } else {
                tracing::warn!(
                    ranking_count,
                    output_mode = %cfg.judge.output_mode,
                    "compaction: all judge ranking entries unavailable; \
                     hash-and-summary falling back to canned judge (zero model spend)"
                );
            }
            HashAndSummaryPolicy::new(Arc::new(KeepHalfJudge::new())).with_output_mode(output_mode)
        }
    };

    Arc::new(policy)
}

/// Convert a [`mu_core::config::JudgeRankingEntry`] to a
/// [`mu_core::protocol::ProviderSelector`] for the compaction judge path.
///
/// Only provider kinds that `build_provider_from_selector` can actually
/// construct are attempted; others return `None` (the walk skips them).
fn ranking_entry_to_selector(
    entry: &mu_core::config::JudgeRankingEntry,
) -> Option<mu_core::protocol::ProviderSelector> {
    use mu_core::protocol::ProviderSelector;
    match entry.provider.as_str() {
        "anthropic" | "anthropic_api" | "anthropic-api" => Some(ProviderSelector::AnthropicApi {
            model: entry.model.clone(),
        }),
        "openrouter" => Some(ProviderSelector::Openrouter {
            model: entry.model.clone(),
        }),
        "openai_codex" | "openai-codex" | "codex" => Some(ProviderSelector::OpenaiCodex {
            model: entry.model.clone(),
        }),
        "ollama" => Some(ProviderSelector::Ollama {
            model: entry.model.clone(),
        }),
        other => {
            tracing::debug!(
                provider = %other,
                "compaction: judge ranking entry uses unsupported provider kind; skipping"
            );
            None
        }
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

    // mu-8bkf: compaction policy resolution from config.  Pins that
    // all named policies resolve to Some (hash-and-summary now wires the
    // real policy or canned judge fallback) and that unknown/explicit-none
    // values still resolve to None.
    #[test]
    fn compaction_policy_resolves_per_config_value() {
        use mu_core::config::CompactionConfig;
        use mu_core::context::compaction::hash_summary::DEFAULT_POLICY_ID;
        let cfg = |p: &str| CompactionConfig {
            default_policy: p.to_string(),
            trigger_threshold_tokens: 150_000,
            ..Default::default()
        };
        // Heuristic resolves to Some(SpanFamilyDropPolicy).
        assert!(resolve_compaction_policy(&cfg("heuristic")).is_some());

        // hash-and-summary now wires the real policy (with canned judge
        // fallback when ranking is empty, as in the default config).
        let hns = resolve_compaction_policy(&cfg("hash-and-summary"));
        assert!(hns.is_some(), "hash-and-summary must resolve to Some");
        let hns = resolve_compaction_policy(&cfg("hash_summary"));
        assert!(hns.is_some(), "hash_summary alias must resolve to Some");
        // Policy label should match HashAndSummaryPolicy's DEFAULT_POLICY_ID.
        assert_eq!(
            resolve_compaction_policy(&cfg("hash-and-summary"))
                .unwrap()
                .policy_label(),
            DEFAULT_POLICY_ID,
            "hash-and-summary policy_label must be DEFAULT_POLICY_ID"
        );

        // Explicit no-op and unknown resolve to None.
        assert!(resolve_compaction_policy(&cfg("no-compaction")).is_none());
        assert!(resolve_compaction_policy(&cfg("none")).is_none());
        assert!(resolve_compaction_policy(&cfg("")).is_none());
        assert!(resolve_compaction_policy(&cfg("bogus")).is_none());
    }

    // mu-8bkf: ranking_entry_to_selector maps known provider strings
    // to the correct ProviderSelector variants (including aliases), and
    // returns None for unsupported provider strings.
    #[test]
    fn ranking_entry_to_selector_maps_known_providers() {
        use mu_core::config::JudgeRankingEntry;
        use mu_core::protocol::ProviderSelector;

        let entry = |p: &str, m: &str| JudgeRankingEntry {
            provider: p.to_string(),
            model: m.to_string(),
            auth: "api_key".to_string(),
        };

        // anthropic aliases
        let sel = ranking_entry_to_selector(&entry("anthropic", "claude-haiku-4-5"));
        assert!(
            matches!(sel, Some(ProviderSelector::AnthropicApi { model }) if model == "claude-haiku-4-5"),
            "anthropic → AnthropicApi"
        );
        let sel = ranking_entry_to_selector(&entry("anthropic_api", "haiku"));
        assert!(
            matches!(sel, Some(ProviderSelector::AnthropicApi { .. })),
            "anthropic_api → AnthropicApi"
        );
        let sel = ranking_entry_to_selector(&entry("anthropic-api", "haiku"));
        assert!(
            matches!(sel, Some(ProviderSelector::AnthropicApi { .. })),
            "anthropic-api → AnthropicApi"
        );

        // openrouter
        let sel = ranking_entry_to_selector(&entry("openrouter", "anthropic/claude-haiku-4.5"));
        assert!(
            matches!(sel, Some(ProviderSelector::Openrouter { .. })),
            "openrouter → Openrouter"
        );

        // codex aliases
        let sel = ranking_entry_to_selector(&entry("openai_codex", "gpt-4o"));
        assert!(
            matches!(sel, Some(ProviderSelector::OpenaiCodex { .. })),
            "openai_codex → OpenaiCodex"
        );
        let sel = ranking_entry_to_selector(&entry("codex", "gpt-4o"));
        assert!(
            matches!(sel, Some(ProviderSelector::OpenaiCodex { .. })),
            "codex → OpenaiCodex"
        );

        // ollama
        let sel = ranking_entry_to_selector(&entry("ollama", "qwen3"));
        assert!(
            matches!(sel, Some(ProviderSelector::Ollama { .. })),
            "ollama → Ollama"
        );

        // unknown → None
        assert!(
            ranking_entry_to_selector(&entry("magic-ai", "model")).is_none(),
            "unknown provider → None"
        );
    }

    // mu-slat: per-session injection of the spawn_worker tool. In
    // production every session gets one scoped to its own id (so worker
    // results wake the caller); in tests/ephemeral mode (no events_dir)
    // it must be absent.
    #[test]
    fn session_spawn_tools_injects_spawn_worker_in_production() {
        let base: Vec<Arc<dyn Tool>> = vec![];
        let sessions = Sessions::new();
        let di = DaemonInfo::new("test")
            .with_events_dir(Some(std::path::PathBuf::from("/tmp/mu-test-events")));
        let tools = session_spawn_tools(&base, &sessions, &di, "session-42");
        assert!(
            tools.iter().any(|t| t.spec().name == "spawn_worker"),
            "production session should get a spawn_worker tool",
        );
    }

    #[test]
    fn session_spawn_tools_omits_spawn_worker_without_events_dir() {
        let base: Vec<Arc<dyn Tool>> = vec![];
        let sessions = Sessions::new();
        let di = DaemonInfo::new("test"); // no events_dir (tests / ephemeral)
        let tools = session_spawn_tools(&base, &sessions, &di, "session-42");
        assert!(
            !tools.iter().any(|t| t.spec().name == "spawn_worker"),
            "no events_dir => no spawn_worker tool",
        );
    }

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
                usage_semantics: None,
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
