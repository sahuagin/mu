//! Session-domain request handlers (session.*, autonomy-related).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
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
    ConfigApplied, ConfigRejected, CreateSessionRequest, CreateSessionResponse,
    DelegateSessionRequest, DelegateSessionResponse, GetConfigRequest, GetConfigResponse,
    PingResponse, ProviderSelector, Request, RespondToInputRequiredRequest,
    RespondToInputRequiredResponse, Response, ScheduleWakeupRequest, ScheduleWakeupResponse,
    SessionEventsRequest, SessionEventsResponse, SessionListRequest, SessionListResponse,
    SessionStatsRequest, SessionStatsResponse, SetConfigRequest, SetConfigResponse,
    SetRouteRequest, SetRouteResponse, SpawnWorkerRequest, SpawnWorkerResponse,
    StartAutonomousRequest, StartAutonomousResponse,
};
use mu_core::transport::{codes, err_response, ok_response, NotificationWriter};

use crate::serve::daemon_info::DaemonInfo;
use crate::serve::discovery::SessionDiscovery;
use crate::serve::factory::ProviderFactory;
use crate::serve::forwarder::forward_events;
use crate::serve::sessions::Sessions;

use super::{ok_or_respond, some_or_respond, to_value_or_null};

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
    skills: Arc<Vec<mu_core::skill::loader::LoadedSkill>>,
    daemon_info: DaemonInfo,
) -> Response<Value> {
    let params: CreateSessionRequest = ok_or_respond!(
        serde_json::from_value(request.params.clone()),
        request.id,
        codes::INVALID_PARAMS,
        "create_session: invalid params"
    );

    // mu-7e21: root capability, with autonomy granted IFF the client
    // asked at creation. INV-1 holds: `Capability::root()` itself stays
    // autonomy-Disallowed; the grant rides the operator's create call,
    // never anything the model can reach (attenuation is intersect-only
    // and no agent-facing surface writes capability).
    let mut capability = Capability::root();
    if let Some(autonomy) = params.autonomy {
        capability.autonomy = autonomy;
    }
    // mu-n25a: side-effects ceiling, same plumbing as the autonomy grant.
    // None → root stays unrestricted (back-compat); Some(x) caps the
    // session so any tool declaring side-effects above `x` is refused at
    // the dispatch choke point regardless of its permission level.
    if let Some(max_side_effects) = params.max_side_effects {
        capability.max_side_effects = Some(max_side_effects);
    }

    match build_and_register_session(BuildSessionRequest {
        selector: &params.provider,
        system_prompt: params.system_prompt, // mu-n48
        cwd: params.cwd,                     // mu-phl v0 / mu-045
        parent_session_id: None,             // no parent — this is a root session
        branched_at_parent_event_id: None,
        capability,                // root: unrestricted, autonomy per mu-7e21 grant above
        seed_messages: Vec::new(), // mu-mh4: fresh session starts empty
        seed_events: Vec::new(),   // mu-mh4: fresh session has no seed events
        cache_ttl: params.cache_ttl.unwrap_or_default(), // mu-f1a0
        max_turns: params.max_turns, // mu-779s: per-session cap override
        effort: params.effort,     // mu-vcbm: launch-time effort default
        notif,
        sessions,
        factory,
        tools,
        skills,
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
    skills: Arc<Vec<mu_core::skill::loader::LoadedSkill>>,
    daemon_info: DaemonInfo,
) -> Response<Value> {
    let params: DelegateSessionRequest = ok_or_respond!(
        serde_json::from_value(request.params.clone()),
        request.id,
        codes::INVALID_PARAMS,
        "session.delegate: invalid params"
    );

    // Verify the parent session exists, and snapshot its current
    // capability so we can attenuate it for the child (mu-033).
    let parent_cap_handle = some_or_respond!(
        sessions.capability(&params.parent_session_id),
        request.id,
        codes::INVALID_PARAMS,
        format!(
            "session.delegate: parent session not found: {}",
            params.parent_session_id
        )
    );

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
        // mu-mh4: delegate sessions still start empty (the branch id is
        // recorded for audit). session.resume is the path that seeds a
        // continuation history; delegate-with-seed is future work.
        seed_messages: Vec::new(),
        seed_events: Vec::new(), // mu-mh4: delegate sessions have no seed events
        // mu-f1a0: delegated workers are PINNED to the 5m tier
        // regardless of the parent's — they run gap-free tool loops,
        // so the 1h tier's 2x write premium is pure cost (operator
        // requirement, bead body).
        cache_ttl: CacheTtl::FiveMinutes,
        max_turns: None, // delegate sessions inherit the cap from the parent
        effort: None,    // mu-vcbm: delegates use the provider default
        notif,
        sessions,
        factory,
        tools,
        skills,
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

/// mu-mh4: `session.resume` — STRICT fork-at-tail resume.
///
/// Resolves the predecessor session's event log (`Sessions::event_log`
/// lazily find-by-ids and parses the one matching on-disk log on demand —
/// across daemon dirs — so a cross-daemon predecessor is addressable here
/// without the old startup bulk-rehydration; mu-lazy-session-rehydration-bh4f),
/// projects it to its last clean boundary via
/// [`mu_core::agent::continuation::project_strict`], and — only if the
/// log is CLEAN — births a fresh live session parented on the dead one,
/// seeded with the continuation history. A ragged log is REFUSED with a
/// precise diagnosis and a `mu --recover` hint (git-style); it is never
/// silently truncated.
///
/// The resumed session's capability is the predecessor's ∩ any requested
/// attenuations (intersection-only — resume can only narrow). When the
/// predecessor's live capability is gone (a cold rehydrated session has
/// no capability handle — the NORMAL resume case), it FAILS CLOSED to the
/// most-restrictive baseline ([`Capability::read_only`]) and then applies
/// the attenuations — so a resume can never WIDEN privileges past a
/// read-only floor (mu-mh4; capability persistence is the real fix —
/// mu-nqn5).
pub fn handle_resume_session(
    request: Request<Value>,
    notif: NotificationWriter,
    sessions: Sessions,
    factory: ProviderFactory,
    tools: Arc<Vec<Arc<dyn Tool>>>,
    skills: Arc<Vec<mu_core::skill::loader::LoadedSkill>>,
    daemon_info: DaemonInfo,
) -> Response<Value> {
    use mu_core::protocol::{ResumeSessionRequest, ResumeSessionResponse, SessionRef};

    let params: ResumeSessionRequest = ok_or_respond!(
        serde_json::from_value(request.params.clone()),
        request.id,
        codes::INVALID_PARAMS,
        "session.resume: invalid params"
    );

    // Parse the session ref (daemon:session or mu:<daemon>/<session>).
    let parsed = ok_or_respond!(
        SessionRef::parse(&params.session_ref),
        request.id,
        codes::INVALID_PARAMS,
        "session.resume"
    );

    // Resolve the predecessor's event log from the Sessions map (the
    // session id is the addressable key; rehydration loaded all daemons'
    // logs at startup).
    let predecessor_log = some_or_respond!(
        sessions.event_log(&parsed.session),
        request.id,
        codes::INVALID_PARAMS,
        format!(
            "session.resume: predecessor session not found: {} \
             (looked up `{}`; is the daemon's events dir the one that holds it?)",
            parsed.to_canonical(),
            parsed.session
        )
    );

    // STRICT continuation projection. A ragged log is refused with a
    // diagnosis naming the damage + a --recover hint.
    let events = predecessor_log.snapshot();
    let continuation = match mu_core::agent::continuation::project_strict(&events) {
        Ok(c) => c,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!(
                    "session.resume refused: {e}. \
                     The log is not cleanly resumable; run `mu --recover {}` to \
                     tombstone the broken record(s) and resume from the last prompt.",
                    params.session_ref
                ),
            );
        }
    };

    // Capability baseline (mu-mh4 fail-closed; mu-nqn5 is the real fix).
    //
    // When the predecessor's live capability handle is still present
    // (warm same-daemon resume) we start from it — the resume can only
    // intersect it down with the requested attenuations.
    //
    // When it is GONE — the NORMAL cold/rehydrated case, because a dead
    // session has no in-memory capability and we do not yet persist it
    // (mu-nqn5) — we FAIL CLOSED to the most-restrictive reasonable
    // baseline (`Capability::read_only`): no tools, ReadOnly side-effects
    // ceiling, autonomy disallowed. Falling back to `root()` here was the
    // panel-flagged attenuation-only-narrows violation: resume of a
    // restricted session would have WIDENED privileges, since
    // attenuate(root, attn) ⊇ attenuate(restricted_predecessor, attn).
    // The operator's `attenuations` can only narrow further from this
    // floor, never widen past it; explicit re-grants are out of scope
    // until capability persistence (mu-nqn5) lands.
    let base_cap = sessions
        .capability(&parsed.session)
        .and_then(|h| h.lock().ok().map(|c| c.clone()))
        .unwrap_or_else(Capability::read_only);
    let resumed_capability = match &params.attenuations {
        Some(attn) => base_cap.attenuate(attn),
        None => base_cap,
    };

    let seeded_message_count = continuation.messages.len();
    // mu-mh4 (panel finding 3): the actor is CALLER-SUPPLIED and
    // UNVERIFIED — there is no connection-derived identity threaded into
    // this handler (the serve layer authenticates the connection with a
    // trust-on-spawn bearer token, not a per-actor identity). Record it
    // as a CLAIMED identity so every projection of the HeadAttached event
    // knows the attribution is unverified and a model calling
    // session.resume cannot be mistaken for the operator. mu-nqn5 (and a
    // future identity layer) can stamp a verified identity alongside.
    let claimed_actor = params
        .actor
        .clone()
        .unwrap_or_else(|| "operator".to_string());

    // mu-mh4 (panel finding 4): NO authz check on who may resume the
    // predecessor. This handler has no requester-identity primitive to
    // check against — the serve layer authenticates the *connection*
    // with a trust-on-spawn bearer token (every authenticated connection
    // is daemon-local and equally trusted), and `create_session` itself
    // applies no per-actor capability gate either, so there is nothing to
    // mirror at this layer. Resume is therefore guarded by daemon-local
    // trust only; a real "may this actor resume this session" check waits
    // on the identity layer (mu-nqn5 follow-up).

    // The attach itself is a HeadAttached event on the new (live)
    // session's log — session identity continuous across serving daemons
    // (mu-mh4 design). Passed as a SEED EVENT so it is appended before the
    // session is registered (no audit-continuity gap — panel finding 4).
    let head_attached = EventPayload::HeadAttached {
        daemon_id: daemon_info.daemon_id().to_string(),
        claimed_actor,
        predecessor_session_id: parsed.session.clone(),
        branched_at_event_id: continuation.fork_event_id,
    };

    let new_session_id = build_and_register_session(BuildSessionRequest {
        selector: &params.provider,
        system_prompt: None,
        cwd: params.cwd,
        // The fork-at-tail lineage: the new live session is parented on
        // the dead one, branched at its last clean boundary.
        parent_session_id: Some(parsed.session.clone()),
        branched_at_parent_event_id: continuation.fork_event_id,
        capability: resumed_capability,
        seed_messages: continuation.messages,
        seed_events: vec![head_attached],
        cache_ttl: CacheTtl::default(),
        max_turns: None, // resume sessions inherit the cap from the predecessor
        effort: None,    // mu-vcbm: resumed sessions use the provider default
        notif,
        sessions: sessions.clone(),
        factory,
        tools,
        skills,
        daemon_info: &daemon_info,
    });
    let new_session_id = ok_or_respond!(
        new_session_id,
        request.id,
        codes::INVALID_PARAMS,
        "session.resume"
    );

    let resp = ResumeSessionResponse {
        session_id: new_session_id,
        predecessor_session_id: parsed.session,
        branched_at_event_id: continuation.fork_event_id,
        seeded_message_count,
    };
    ok_response(request.id, to_value_or_null(resp))
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
    /// mu-mh4: pre-seeded conversation history for a resumed/forked
    /// session (continuation projection of the predecessor's log).
    /// Empty for fresh and (current) delegate sessions.
    seed_messages: Vec<AgentMessage>,
    /// mu-mh4 (panel finding 4): system events to append to the new
    /// session's log immediately after `SessionCreated` and BEFORE the
    /// session is registered in the Sessions map. Appending here (rather
    /// than after registration) closes the audit-continuity race: a
    /// reader that observes the session in the registry is guaranteed to
    /// also see these seed events, because they are already durable on
    /// the log before the session becomes observable. Used by
    /// `session.resume` to seed `HeadAttached`. Empty otherwise.
    seed_events: Vec<EventPayload>,
    /// mu-f1a0: prompt-cache TTL tier for this session's provider.
    cache_ttl: CacheTtl,
    /// mu-779s: cap on assistant-message turns. `None` → use the
    /// provider-aware default (20 for Anthropic, 35 for OpenAI, etc.).
    /// `Some(n)` → cap at `n` turns. `Some(0)` → disable entirely.
    /// Forwarded as `AgentConfig::max_turns` to the agent loop.
    max_turns: Option<u32>,
    /// mu-vcbm: launch-time reasoning-effort default. Forwarded as
    /// `AgentConfig::effort`. `None` → provider's own default.
    effort: Option<String>,
    // runtime deps (daemon-global)
    notif: NotificationWriter,
    sessions: Sessions,
    factory: ProviderFactory,
    tools: Arc<Vec<Arc<dyn Tool>>>,
    skills: Arc<Vec<mu_core::skill::loader::LoadedSkill>>,
    daemon_info: &'a DaemonInfo,
}

/// Per-session tool list: the daemon's base tools plus a `spawn_worker`
/// tool scoped to THIS session, so a worker's results route back to this
/// session's mailbox (waking it) rather than the dead "supervisor" ghost.
/// The spawn_worker tool is only added in production (events_dir set) —
/// tests have no pot infrastructure. (mu-slat)
///
/// mu-7e21: autonomy tools are injected IFF the session's capability
/// grants them — gated on capability, NOT events_dir (they need no pot
/// infrastructure, only the session's own input channel). The tool
/// list is therefore capability-honest: a session that can't enter
/// autonomous mode never sees `start_autonomous`; one whose grant has
/// `allow_schedule_wakeup: false` sees `start_autonomous` but not
/// `schedule_wakeup`.
fn session_spawn_tools(
    base: &[Arc<dyn Tool>],
    sessions: &Sessions,
    daemon_info: &DaemonInfo,
    session_id: &str,
    autonomy: &mu_core::capability::AutonomyCapability,
) -> Vec<Arc<dyn Tool>> {
    use mu_core::capability::AutonomyCapability;

    // mu-dialogue per-session identity: the dialogue MCP connection is
    // daemon-shared (one pipe for all sessions, handshake fires once at
    // startup), so a session's peer id can only be bound at the tool layer —
    // the same idiom as spawn_worker/watch below. dialogue_say.from is forced
    // to this session's id; dialogue_poll.to defaults to it (so polling your
    // own inbox needs no argument). Tools the daemon didn't import (dialogue
    // server absent/unreachable) simply aren't present and pass through.
    let dialogue_identity = format!("mu:{}:{}", daemon_info.daemon_id(), session_id);
    let mut tools: Vec<Arc<dyn Tool>> = base
        .iter()
        .map(|t| match t.spec().name.as_str() {
            "dialogue_say" => Arc::new(crate::tools::SessionDialogueTool::new(
                t.clone(),
                dialogue_identity.clone(),
                "from",
                crate::tools::DialogueBind::Force,
            )) as Arc<dyn Tool>,
            "dialogue_poll" => Arc::new(crate::tools::SessionDialogueTool::new(
                t.clone(),
                dialogue_identity.clone(),
                "to",
                crate::tools::DialogueBind::Default,
            )) as Arc<dyn Tool>,
            _ => t.clone(),
        })
        .collect();
    if daemon_info.events_dir().is_some() {
        tools.push(Arc::new(crate::tools::SpawnWorkerTool::new(
            // mu-qc08: a WEAK handle — a strong clone here deadlocks
            // shutdown (the tool lives in this session's own tool list).
            sessions.downgrade(),
            daemon_info.clone(),
            Some(session_id.to_string()),
        )));
        // mu-watch-tool-wakeup-o03p: the `watch` tool — spawn a command,
        // wake THIS session when it exits. Same WEAK-handle discipline as
        // spawn_worker (it lives in this session's own tool list), and
        // scoped to this session_id so the wakeup routes back here.
        tools.push(Arc::new(crate::tools::WatchTool::new(
            sessions.downgrade(),
            session_id.to_string(),
            // mu-qnag: watch gates every command through the daemon's bash
            // policy. A session with no `--bash-*` flags resolves to strict
            // (read-only allowlist), so a read-only reviewer's
            // watch("cargo test") is rejected by the SAME gate bash uses;
            // a `--bash-yolo` worker keeps watch("cargo build") unchanged.
            daemon_info.bash_settings().resolve_mode(),
        )));
    }
    if let AutonomyCapability::Allowed {
        allow_schedule_wakeup,
        ..
    } = autonomy
    {
        tools.push(Arc::new(crate::tools::StartAutonomousTool::new(
            sessions.downgrade(),
            session_id.to_string(),
        )));
        if *allow_schedule_wakeup {
            tools.push(Arc::new(crate::tools::ScheduleWakeupTool::new(
                sessions.downgrade(),
                session_id.to_string(),
            )));
        }
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
        seed_messages,
        seed_events,
        notif,
        sessions,
        factory,
        tools,
        skills,
        daemon_info,
        cache_ttl,
        max_turns,
        effort,
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
    // mu-779s: per-session max_turns. The resolution order is:
    // 1. Request-supplied value (params.max_turns) — overrides all
    //    Some(0) means "disable cap entirely"
    // 2. Config's default_max_turns (if Some(n)) — operator default
    // 3. Provider-aware default (20 Anthropic, 35 OpenAI, etc.)
    let max_turns = max_turns
        .or_else(|| daemon_info.config().session.default_max_turns)
        .or_else(|| Some(mu_core::agent::loop_::default_max_turns_for(&kind_str)));
    // Resolve this session's context limits once, here, where both the
    // route catalog and the daemon config are reachable, then record
    // them on the log so the status projections (forwarder/mcp) read the
    // effective soft/hard limits straight off the event stream rather
    // than re-deriving them. See `mu_core::session_status` for the
    // soft-limit / hard-limit / fill vocabulary.
    let (context_soft_limit, context_hard_limit, max_output_tokens) =
        resolve_context_limits(daemon_info, &kind_str, &model_str);
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
    // Only record a snapshot when we actually have a soft limit — a route
    // the catalog doesn't know yields no limits, and a missing event is
    // truer than a fabricated number (the meter simply stays blank).
    if let Some(soft) = context_soft_limit {
        event_log.append(
            EventActor::System,
            EventPayload::SessionConfigResolved {
                context_soft_limit: soft,
                context_hard_limit,
                // mu-a79g: record the output budget the compaction
                // trigger reserves against, so the effective compaction
                // point is reconstructable from the event stream alone.
                max_output_tokens,
            },
        );
    }

    // mu-mh4 (panel finding 4): append any seed events (e.g. resume's
    // HeadAttached) AFTER SessionCreated but BEFORE the session is
    // registered below. This closes the audit-continuity race: the
    // session only becomes observable in the Sessions map once these
    // events are already durable on the log, so no reader can see the
    // session without also seeing them.
    for payload in seed_events {
        event_log.append(EventActor::System, payload);
    }

    let pending_approvals = Arc::new(Mutex::new(HashMap::new()));
    // mu-phl v0 / mu-0bxv: build the new session's project context by
    // iterating the daemon's recall providers (set up at daemon startup
    // via DaemonInfo::with_recall_providers) against the effective cwd
    // and the session's capability. Computed BEFORE capability is moved
    // into capability_handle so we can borrow it.
    let project_context = build_project_context(daemon_info, cwd.as_deref(), &capability);
    // mu-recall-provenance-audit-vnc9.1 (P0): record the recall
    // injection set as provenance refs — {source, content-hash,
    // tokens}, never the text. Appended before `sessions.insert`
    // below, so (same as the mu-mh4 seed-event guarantee) no reader
    // can observe the session without the provenance event already
    // durable on its log. Skipped when recall produced nothing.
    if let Some(ctx) = &project_context {
        event_log.append(
            EventActor::System,
            mu_core::context::recall::recall_provenance_payload(ctx),
        );
    }
    // mu-7e21: snapshot the autonomy grant before `capability` moves
    // into its handle — the tool list is built from it (injection is
    // capability-gated; see session_spawn_tools).
    let autonomy = capability.autonomy.clone();
    let capability_handle = Arc::new(Mutex::new(capability));
    let provider_status = Arc::new(Mutex::new(
        super::super::provider_status::ProviderStatusTracker::new(),
    ));
    let mailbox = Arc::new(super::super::mailbox::MailboxState::new());
    let (events_tx, events_rx) = tokio::sync::mpsc::channel(64);
    let mut session_tools = session_spawn_tools(
        tools.as_slice(),
        &sessions,
        daemon_info,
        &session_id,
        &autonomy,
    );
    // mu-onq8: always-on in-loop capability discovery. Ranks the session's
    // sibling tools (attenuated by this session's capability) plus the
    // daemon-discovered skills against a free-text intent, so the agent can
    // find the right capability in-loop instead of shelling out to the
    // allowlist-blocked bash path.
    let discover_siblings = Arc::new(session_tools.clone());
    session_tools.push(Arc::new(crate::tools::DiscoverTool::new(
        discover_siblings,
        skills.clone(),
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
    // mu-mu-bare-flag-fxc8: bare sessions suppress the bootstrap too —
    // hermetic means mu injects nothing the operator didn't supply.
    let bare = daemon_info.config().recall.bare;
    let effective_system_prompt = super::super::discovery_bootstrap::compose_system_prompt(
        system_prompt,
        recall_enabled,
        bare,
    );
    // mu-uz0n: implicit capability discovery — per-turn hint injection,
    // ranked by the same lexical engine as the `discover` tool. Bare
    // sessions stay hermetic (no injection mu didn't get told to make).
    let index_cfg = &daemon_info.config().index;
    let discover_hints = (index_cfg.discover_injection && !bare).then(|| {
        mu_core::context::capability_hints::DiscoverHints {
            skills: skills.clone(),
            limit: index_cfg.discover_injection_limit,
        }
    });
    // mu-context-limits-wire phase 2: shared live soft limit. Seeded with
    // the resolved soft limit (0 = unknown ⇒ loop uses its config/default
    // fallback). session.set_config writes this atomic and the loop reads
    // it at each compaction check — no restart needed.
    let live_context_soft_limit = Arc::new(AtomicU64::new(context_soft_limit.unwrap_or(0)));
    let agent = AgentLoop::spawn(SpawnArgs {
        provider,
        provider_kind: kind_arc,
        model: model_arc,
        tools: session_tools,
        live_context_soft_limit: live_context_soft_limit.clone(),
        config: AgentConfig {
            system_prompt: effective_system_prompt.map(SpanText::from),
            max_turns,
            project_context,
            // The compaction trigger IS the soft limit (resolved above and
            // recorded as SessionConfigResolved), so the point mu compacts
            // and the denominator the status meter shows are one number.
            // None (unknown route) ⇒ the loop falls back to its own
            // default; the daemon never injects a magic constant here.
            compaction_threshold: context_soft_limit.map(|s| s as usize),
            // mu-ub6q: reserve the model's output budget below the soft
            // limit so compaction fires with room for the response, not
            // only once the input alone reaches the window. None route ⇒
            // 0 ⇒ no reservation (unchanged behavior).
            max_output_tokens: max_output_tokens.unwrap_or(0) as usize,
            compaction_policy_override,
            // mu-mh4: seed the loop with the continuation history when
            // this session is a resume/fork-at-tail; empty otherwise.
            seed_messages,
            discover_hints,
            // mu-vcbm: launch-time effort default → loop's standing effort.
            effort: effort.map(|e| Arc::from(e.as_str())),
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
            live_context_soft_limit,
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
        ProviderSelector::Vllm { model } => ("vllm".into(), model.clone()),
        ProviderSelector::Ollama { model } => ("ollama".into(), model.clone()),
    }
}

/// `ticket` (spec mu-046 WP4): when the ingest pipeline journaled this
/// ask's `CommandReceived` into the session's own event log, the
/// ticket rides into `AgentInput::UserMessage` here so the agent loop
/// can carry it back out on the turn's terminal `Done` — the forwarder
/// then writes the `CommandSucceeded`/`CommandFailed` receipt with the
/// correct pairing. If the send below FAILS, the ticket dies with the
/// input; the pipeline (which kept its own echo) writes the failure
/// receipt at handler completion instead — delivery failure is an
/// outcome.
pub async fn handle_ask_session(
    request: Request<Value>,
    sessions: Sessions,
    ticket: Option<mu_core::command_journal::CommandTicket>,
) -> Response<Value> {
    let params: AskSessionRequest = ok_or_respond!(
        serde_json::from_value(request.params.clone()),
        request.id,
        codes::INVALID_PARAMS,
        "ask_session: invalid params"
    );

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
            // mu-vcbm: a per-turn `/effort` selection rides in with the
            // ask and updates the session's standing effort stickily.
            let effort = params.effort.map(|e| Arc::from(e.as_str()));
            // Boxed to keep AgentInput's variant size small (clippy
            // large_enum_variant) — tickets ride rarely-hot paths.
            match tx
                .send(AgentInput::UserMessage(msg, ticket.map(Box::new), effort))
                .await
            {
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
    let params: CancelSessionRequest = ok_or_respond!(
        serde_json::from_value(request.params.clone()),
        request.id,
        codes::INVALID_PARAMS,
        "cancel_session: invalid params"
    );

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
    let params: CancelOutstandingRequest = ok_or_respond!(
        serde_json::from_value(request.params.clone()),
        request.id,
        codes::INVALID_PARAMS,
        "cancel_outstanding: invalid params"
    );

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
    let params: SessionStatsRequest = ok_or_respond!(
        serde_json::from_value(request.params.clone()),
        request.id,
        codes::INVALID_PARAMS,
        "session.stats: invalid params"
    );

    let log = some_or_respond!(
        sessions.event_log(&params.session_id),
        request.id,
        codes::INVALID_PARAMS,
        format!("session not found: {}", params.session_id)
    );

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
    let params: RespondToInputRequiredRequest = ok_or_respond!(
        serde_json::from_value(request.params.clone()),
        request.id,
        codes::INVALID_PARAMS,
        "respond_to_input_required: invalid params"
    );

    // Look up the pending oneshot; if found, send the decision.
    let sender_opt = sessions.take_pending_approval(&params.session_id, &params.request_id);
    let accepted = match sender_opt {
        Some(sender) => sender.send(params.decision).is_ok(),
        None => false,
    };
    let resp = RespondToInputRequiredResponse { accepted };
    ok_response(request.id, to_value_or_null(resp))
}

pub async fn handle_close_session(request: Request<Value>, sessions: Sessions) -> Response<Value> {
    let params: CloseSessionRequest = ok_or_respond!(
        serde_json::from_value(request.params.clone()),
        request.id,
        codes::INVALID_PARAMS,
        "close_session: invalid params"
    );

    // Emit SessionClosed into the log BEFORE removing the session
    // from the registry — once removed, the log handle is dropped.
    // In-memory ONLY (mu-lazy-session-rehydration-bh4f, gpt-5.5 review):
    // close must not lazily resurrect a read-only ghost from disk just to
    // append a no-op SessionClosed to it. An unloaded past session has
    // nothing to close — `remove` below returns false, which is correct.
    if let Some(log) = sessions.event_log_in_memory(&params.session_id) {
        log.append(EventActor::System, EventPayload::SessionClosed);
    }

    // mu-dialogue-inbound-wakeup: tear the session down deterministically —
    // signal its dialogue poller (if any) and await the task — instead of
    // dropping it and relying on the loop's input channel closing. The
    // sessions mutex is never held across the await (see `remove_with_teardown`).
    let removed = sessions.remove_with_teardown(&params.session_id).await;
    let resp = CloseSessionResponse { closed: removed };
    ok_response(request.id, to_value_or_null(resp))
}

/// Lists sessions known to the live daemon: in-memory live/worker sessions,
/// any past session already lazily cached (via `event_log`), plus peers when
/// `include_remote` is set. By design (mu-lazy-session-rehydration-bh4f) this
/// does NOT bulk-scan disk — the daemon no longer rehydrates every log at
/// startup, so a past session won't appear here until it's been addressed.
/// Cheap offline enumeration of ALL on-disk sessions (after a restart, to find
/// an id to `resume`/`recover`) is the standalone `mu list-sessions` command
/// (`crate::sessions_index::scan_session_index`), not this RPC. If a future
/// consumer needs past sessions surfaced through this RPC, wire that scan in
/// here behind a filter flag rather than reviving the startup bulk-load.
pub async fn handle_session_list(
    request: Request<Value>,
    discovery: Arc<dyn SessionDiscovery>,
) -> Response<Value> {
    let params: SessionListRequest = ok_or_respond!(
        serde_json::from_value(request.params.clone()),
        request.id,
        codes::INVALID_PARAMS,
        "session.list: invalid params"
    );
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
    let params: SessionEventsRequest = ok_or_respond!(
        serde_json::from_value(request.params.clone()),
        request.id,
        codes::INVALID_PARAMS,
        "session.events: invalid params"
    );
    let log = some_or_respond!(
        sessions.event_log(&params.session_id),
        request.id,
        codes::INVALID_PARAMS,
        format!("session not found: {}", params.session_id)
    );

    let limit = params.limit.unwrap_or(200).clamp(1, 5000) as usize;
    let after = params.after_event_id.unwrap_or(0);
    let kinds_filter: std::collections::HashSet<String> =
        params.kinds_filter.iter().cloned().collect();

    let all = log.snapshot();
    let mut events_json: Vec<Value> = Vec::with_capacity(limit);
    let mut last_emitted: Option<u64> = None;
    let mut end_of_log = true;

    for ev in all.iter().filter(|e| e.id > after) {
        let payload_kind = ev.payload.kind_str();
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

    let params: StartAutonomousRequest = ok_or_respond!(
        serde_json::from_value(request.params.clone()),
        request.id,
        codes::INVALID_PARAMS,
        "session.start_autonomous: invalid params"
    );

    let cap_handle = some_or_respond!(
        sessions.capability(&params.session_id),
        request.id,
        codes::INVALID_PARAMS,
        format!(
            "session.start_autonomous: session not found: {}",
            params.session_id
        )
    );
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

/// mu-036 Phase C (mu-7zn): session.schedule_wakeup handler.
///
/// Validates that exactly one of `wake_at_unix_ms` / `sleep_for_ms`
/// is set and that the session's capability grants
/// `allow_schedule_wakeup` (INV-1), resolves the relative
/// `sleep_for_ms` to an absolute wall-clock instant, then sends
/// `AgentInput::ScheduleWakeup` into the session's input channel.
/// The agent loop parks itself in `RunMode::Sleeping` and resumes the
/// autonomous run at iteration N+1 on wake (INV-5: no model/tool
/// budget consumed while parked).
pub async fn handle_schedule_wakeup(
    request: Request<Value>,
    sessions: Sessions,
) -> Response<Value> {
    use mu_core::capability::AutonomyCapability;

    let params: ScheduleWakeupRequest = ok_or_respond!(
        serde_json::from_value(request.params.clone()),
        request.id,
        codes::INVALID_PARAMS,
        "session.schedule_wakeup: invalid params"
    );
    if params.wake_at_unix_ms.is_some() == params.sleep_for_ms.is_some() {
        return err_response(
            request.id,
            codes::INVALID_PARAMS,
            "session.schedule_wakeup: exactly one of wake_at_unix_ms / \
             sleep_for_ms must be set"
                .to_string(),
        );
    }
    let cap_handle = some_or_respond!(
        sessions.capability(&params.session_id),
        request.id,
        codes::INVALID_PARAMS,
        format!(
            "session.schedule_wakeup: session not found: {}",
            params.session_id
        )
    );
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

    // Resolve to an absolute wall-clock wake time. `sleep_for_ms` is
    // relative to now; `wake_at_unix_ms` is already absolute. The
    // exactly-one invariant above guarantees one branch is taken.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let scheduled_for_unix_ms = match (params.wake_at_unix_ms, params.sleep_for_ms) {
        (Some(at), None) => at,
        (None, Some(sleep)) => now_ms.saturating_add(sleep),
        // Unreachable given the exactly-one check above.
        _ => now_ms,
    };

    let sender = sessions.input_sender(&params.session_id);
    match sender {
        None => err_response(
            request.id,
            codes::INVALID_PARAMS,
            format!(
                "session.schedule_wakeup: session not found: {}",
                params.session_id
            ),
        ),
        Some(tx) => {
            match tx
                .send(AgentInput::ScheduleWakeup {
                    wake_at_unix_ms: scheduled_for_unix_ms,
                    reason: params.reason,
                })
                .await
            {
                Ok(_) => {
                    let resp = ScheduleWakeupResponse {
                        accepted: true,
                        scheduled_for_unix_ms,
                    };
                    ok_response(request.id, to_value_or_null(resp))
                }
                Err(_) => err_response(
                    request.id,
                    codes::INTERNAL_ERROR,
                    "session.schedule_wakeup: session loop has terminated",
                ),
            }
        }
    }
}

/// mu-k56u: swap the provider+model on a live session between turns.
pub async fn handle_set_route(
    request: Request<Value>,
    sessions: Sessions,
    factory: ProviderFactory,
    daemon_info: DaemonInfo,
) -> Response<Value> {
    let params: SetRouteRequest = ok_or_respond!(
        serde_json::from_value(request.params),
        request.id,
        codes::INVALID_PARAMS,
        "invalid set_route params"
    );

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
    let provider = ok_or_respond!(
        factory(&params.provider, route_ttl),
        request.id,
        codes::INTERNAL_ERROR,
        "could not build provider"
    );

    let input_tx = some_or_respond!(
        sessions.input_sender(&params.session_id),
        request.id,
        codes::INVALID_PARAMS,
        format!("session not found: {}", params.session_id)
    );

    // Resolve the NEW model's context limits before the switch so we can
    // re-record them; the catalog lookup above already proved the route
    // exists.
    // mu-ub6q: both route-derived compaction budgets (soft limit and
    // output reservation) ride on the switch input below so the loop
    // applies them together for the model now in force.
    let (context_soft_limit, context_hard_limit, max_output_tokens) =
        resolve_context_limits(&daemon_info, &kind_str, &model_str);

    // mu-ub6q: carry BOTH route-derived compaction budgets on the switch
    // input so the loop applies them in one handler — no window where a
    // turn sees the new reservation paired with the old soft limit.
    let input = AgentInput::SwitchProvider {
        provider,
        provider_kind: Arc::from(kind_str.as_str()),
        model: Arc::from(model_str.as_str()),
        max_output_tokens: max_output_tokens.unwrap_or(0) as usize,
        context_soft_limit: context_soft_limit.unwrap_or(0),
    };

    if input_tx.send(input).await.is_err() {
        return err_response(
            request.id,
            codes::INTERNAL_ERROR,
            "session agent loop has terminated".to_string(),
        );
    }

    // Re-record the resolved config for the new model so `context_limits()`
    // (and thus the status meter + compaction trigger) tracks the switch.
    // A backward scan means this shadows the creation-time snapshot. The
    // loop separately logs ProviderSwitched for provider/model identity;
    // this carries the limits that event's reserved fields don't yet.
    // Skip when the new route has no soft limit (nothing truthful to say).
    //
    // mu-a79g — ordering rationale (ci-aipr panel raised this, conceded on
    // convergence): this append intentionally FOLLOWS `input_tx.send` above
    // and is guarded on it — we record the snapshot only for a switch the
    // loop actually accepted (a send failure returns early, recording
    // nothing). A reviewer may flag that a config effect can precede its
    // durable snapshot (the loop could emit a CompactionAssembly using the
    // new `max_output` reserve before this event lands). That is NOT a
    // source-of-truth gap: CompactionAssembly is self-describing — it
    // carries the predicted/threshold/output_reserve it fired on, so
    // reconstructing a compaction never depends on correlating it with this
    // snapshot. This snapshot serves the status/config seam (the model's
    // current budget), not per-compaction interpretation. The inverse order
    // (append-first) would be worse: it would record config for a switch
    // that may never be delivered.
    if let (Some(soft), Some(log)) = (context_soft_limit, sessions.event_log(&params.session_id)) {
        log.append(
            EventActor::System,
            EventPayload::SessionConfigResolved {
                context_soft_limit: soft,
                context_hard_limit,
                // mu-a79g: the new model's output budget tracks the
                // switch, mirroring the reservation the SwitchProvider
                // handler just applied above.
                max_output_tokens,
            },
        );
    }

    // mu-ub6q: the LIVE soft-limit atomic is updated by the agent loop's
    // SwitchProvider handler (carrying context_soft_limit above), in the
    // same step it applies the output reservation — so the two halves of
    // the effective compaction trigger move together and a queued turn
    // can't observe a half-updated pair. (set_config still updates the
    // atomic daemon-side; this is the switch path.)

    ok_response(
        request.id,
        serde_json::to_value(SetRouteResponse {
            provider_kind: kind_str,
            model: model_str,
        })
        .unwrap_or_default(),
    )
}

// ── generic config-plane handlers (mu-context-limits-wire phase 2) ───
//
// `session.get_config` / `session.set_config` are generic key→value
// messages; this is the daemon-side REGISTRY of addressable keys. The
// wire types never change as keys are added — only this module does.
// Both are gated on the session's `ConfigCapability` axis.

/// The config keys this daemon understands. A finite, explicit set —
/// adding one is a match arm in [`read_config_key`]/[`apply_config_entry`],
/// never a new wire message (that's the "don't hardcode the threshold as
/// THE update event" requirement: the message is generic, the keys are
/// data).
mod config_keys {
    /// The context soft limit (= compaction trigger). Read **and** write.
    pub const SOFT_LIMIT: &str = "context.soft_limit";
    /// The model's hard context ceiling. Read-only at session scope
    /// (tune per-model in models.toml).
    pub const HARD_LIMIT: &str = "context.hard_limit";
    /// Current context fill (last call's input tokens). Read-only.
    pub const USED_TOKENS: &str = "context.used_tokens";
    /// Every key returned for the explicit `"*"` whole-config request.
    pub const ALL_READABLE: &[&str] = &[SOFT_LIMIT, HARD_LIMIT, USED_TOKENS];
}

/// Read one key's current value. `None` ⇒ unknown key (omitted from the
/// response — the daemon never volunteers a value that wasn't asked for).
/// `Some(Value::Null)` ⇒ a known key with no value yet.
fn read_config_key(log: &SessionEventLog, key: &str) -> Option<Value> {
    let (soft, hard) = log
        .context_limits()
        .map_or((None, None), |(s, h, _max_output)| (Some(s), h));
    match key {
        config_keys::SOFT_LIMIT => Some(soft.map_or(Value::Null, Value::from)),
        config_keys::HARD_LIMIT => Some(hard.map_or(Value::Null, Value::from)),
        config_keys::USED_TOKENS => Some(log.live_usage().1.map_or(Value::Null, Value::from)),
        _ => None,
    }
}

/// `session.get_config` — read named keys. Gated on `ConfigCapability`
/// (needs ≥ ReadOnly). Returns ONLY the requested keys; the sentinel
/// `"*"` (explicit) expands to every readable key.
pub async fn handle_get_config(request: Request<Value>, sessions: Sessions) -> Response<Value> {
    let params: GetConfigRequest = ok_or_respond!(
        serde_json::from_value(request.params),
        request.id,
        codes::INVALID_PARAMS,
        "invalid get_config params"
    );
    let cap = some_or_respond!(
        sessions.capability(&params.session_id),
        request.id,
        codes::INVALID_PARAMS,
        format!(
            "session.get_config: session not found: {}",
            params.session_id
        )
    );
    let can_read = cap.lock().map(|c| c.config.can_read()).unwrap_or(false);
    if !can_read {
        return err_response(
            request.id,
            codes::AUTH_DENIED,
            "session.get_config: capability denies config read (config = none)".to_string(),
        );
    }
    let log = some_or_respond!(
        sessions.event_log(&params.session_id),
        request.id,
        codes::INVALID_PARAMS,
        format!(
            "session.get_config: session not found: {}",
            params.session_id
        )
    );

    let keys: Vec<String> = if params.keys.iter().any(|k| k == GetConfigRequest::ALL) {
        config_keys::ALL_READABLE
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        params.keys.clone()
    };
    let mut values = std::collections::BTreeMap::new();
    for key in keys {
        if let Some(v) = read_config_key(&log, &key) {
            values.insert(key, v);
        }
        // Unknown keys are silently omitted: never return more than asked.
    }
    ok_response(request.id, to_value_or_null(GetConfigResponse { values }))
}

/// `session.set_config` — write named keys. Gated on `ConfigCapability`
/// (needs ReadWrite). Each entry validated + applied independently; the
/// response reports per-key success/failure. The `context.soft_limit`
/// key records a `SessionConfigResolved` event (so the status meter
/// reflects it) AND pushes `AgentInput::SetContextSoftLimit` to the live
/// loop (so compaction uses it) — one change, both halves, via the event
/// path.
pub async fn handle_set_config(request: Request<Value>, sessions: Sessions) -> Response<Value> {
    let params: SetConfigRequest = ok_or_respond!(
        serde_json::from_value(request.params),
        request.id,
        codes::INVALID_PARAMS,
        "invalid set_config params"
    );
    let cap = some_or_respond!(
        sessions.capability(&params.session_id),
        request.id,
        codes::INVALID_PARAMS,
        format!(
            "session.set_config: session not found: {}",
            params.session_id
        )
    );
    let can_write = cap.lock().map(|c| c.config.can_write()).unwrap_or(false);
    if !can_write {
        return err_response(
            request.id,
            codes::AUTH_DENIED,
            "session.set_config: capability denies config write (config != read_write)".to_string(),
        );
    }
    let log = some_or_respond!(
        sessions.event_log(&params.session_id),
        request.id,
        codes::INVALID_PARAMS,
        format!(
            "session.set_config: session not found: {}",
            params.session_id
        )
    );
    let live_soft_limit = some_or_respond!(
        sessions.live_context_soft_limit(&params.session_id),
        request.id,
        codes::INVALID_PARAMS,
        format!(
            "session.set_config: session not found: {}",
            params.session_id
        )
    );

    let mut applied = Vec::new();
    let mut rejected = Vec::new();
    for entry in params.entries {
        let key = entry.key;
        let value = entry.value;
        let result: Result<Value, String> = match key.as_str() {
            config_keys::SOFT_LIMIT => match value.as_u64() {
                Some(tokens) if tokens > 0 => {
                    // Two halves of one change: (1) update the live cell
                    // the agent loop reads at its next compaction check
                    // (no restart); (2) record a SessionConfigResolved
                    // event so the status meter reflects the same value.
                    // Both flow from this one write.
                    live_soft_limit.store(tokens, Ordering::Relaxed);
                    // mu-a79g: this seam has no route catalog, so carry
                    // BOTH the hard limit and the output budget forward
                    // from the latest recorded snapshot — otherwise a
                    // soft-limit change would silently drop max_output and
                    // the effective compaction point would stop being
                    // reconstructable from the event stream.
                    let (hard, max_output) = log
                        .context_limits()
                        .map_or((None, None), |(_, h, m)| (h, m));
                    log.append(
                        EventActor::System,
                        EventPayload::SessionConfigResolved {
                            context_soft_limit: tokens,
                            context_hard_limit: hard,
                            max_output_tokens: max_output,
                        },
                    );
                    Ok(Value::from(tokens))
                }
                _ => Err("context.soft_limit expects a positive integer token count".to_string()),
            },
            config_keys::HARD_LIMIT | config_keys::USED_TOKENS => Err("read-only key".to_string()),
            _ => Err("unknown config key".to_string()),
        };
        match result {
            Ok(value) => applied.push(ConfigApplied { key, value }),
            Err(reason) => rejected.push(ConfigRejected { key, reason }),
        }
    }
    ok_response(
        request.id,
        to_value_or_null(SetConfigResponse { applied, rejected }),
    )
}

// ── mu-slat: spawn_worker ────────────────────────────────────────────

pub async fn handle_spawn_worker(
    request: Request<Value>,
    sessions: Sessions,
    daemon_info: DaemonInfo,
) -> Response<Value> {
    let req: SpawnWorkerRequest = ok_or_respond!(
        serde_json::from_value(request.params),
        request.id,
        codes::INVALID_PARAMS,
        "bad SpawnWorkerRequest"
    );

    let config = crate::serve::worker::SpawnWorkerConfig {
        prompt: req.prompt.clone(),
        provider: req.provider,
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

/// Resolve a session's effective context limits in tokens — the single
/// place that turns config + route catalog into the
/// `(soft, hard, max_output)` triple — `(soft, hard)` recorded as
/// [`EventPayload::SessionConfigResolved`], `max_output` threaded to the
/// agent loop as compaction headroom (mu-ub6q). See
/// [`mu_core::session_status`] for what soft/hard mean.
///
/// - **hard**: the model's `context_hard_limit` from the route catalog
///   (`None` when the catalog has none — informational only).
/// - **soft** precedence (no magic constant): the global override
///   `[compaction] context_soft_limit` if set, else the model's per-model
///   `context_soft_limit`, else — as a last resort when a model declares
///   no soft budget — its `context_hard_limit`. `None` only for a route
///   the catalog doesn't know at all (e.g. a hand-built test log); then
///   there is no meter denominator and no compaction trigger, which is
///   the honest answer rather than a guessed number.
fn resolve_context_limits(
    daemon_info: &DaemonInfo,
    provider_kind: &str,
    model: &str,
) -> (Option<u64>, Option<u64>, Option<u32>) {
    let route = daemon_info.route_catalog().find(provider_kind, model);
    let model_soft = route.and_then(|r| r.context_soft_limit);
    let hard = route.and_then(|r| r.context_hard_limit);
    // mu-ub6q: the model's output budget, reserved as compaction
    // headroom by the agent loop (AgentConfig::max_output_tokens) so a
    // soft limit set at the window still leaves room for the output.
    let max_output = route.and_then(|r| r.max_output_tokens);
    let soft = daemon_info
        .config()
        .compaction
        .context_soft_limit
        .map(|v| v as u64)
        .or(model_soft)
        .or(hard);
    (soft, hard, max_output)
}

/// Resolve the per-session compaction policy from config, with legible
/// diagnostics. Closes mu-8bkf: the previous inline match wired only
/// `"heuristic"` and silently fell through to a no-op for every other
/// value — including the documented `"hash-and-summary"` — so a configured
/// soft-limit override produced no compaction with no signal.
fn resolve_compaction_policy(
    cfg: &mu_core::config::CompactionConfig,
) -> Option<Arc<dyn mu_core::context::compaction::CompactionPolicy>> {
    use mu_core::context::compaction::heuristic::SpanFamilyDropPolicy;
    let heuristic = || -> Arc<dyn mu_core::context::compaction::CompactionPolicy> {
        Arc::new(SpanFamilyDropPolicy::new())
    };
    match cfg.default_policy.as_str() {
        "heuristic" => {
            // `[compaction] context_soft_limit` is an optional GLOBAL
            // soft-limit override; None means the effective budget is each
            // model's per-model context_soft_limit (resolved at creation).
            tracing::info!(
                soft_limit_override = ?cfg.context_soft_limit,
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
            if matches!(other, "no-compaction" | "none" | "") {
                // Every session has an effective soft limit (per-model or
                // the global override), so "no-compaction" means that
                // budget is observed by the meter but never acted on.
                tracing::warn!(
                    soft_limit_override = ?cfg.context_soft_limit,
                    default_policy = %other,
                    "compaction: default_policy is explicitly \"no-compaction\" — the \
                     context soft limit is shown in status but context will NOT be \
                     compacted when it is crossed. Remove the explicit no-compaction \
                     override to use the default heuristic policy, or set \
                     [compaction].default_policy = \"hash-and-summary\" for judge-backed \
                     compaction (mu-8bkf)."
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
/// ## Empty ranking vs all-unavailable
///
/// EMPTY `ranking` is the deliberate zero-config path: fall back to the
/// canned judge ([`mu_core::context::compaction::bench::KeepHalfJudge`]),
/// per the documented contract in `CompactionJudgeConfig` ("falls back to
/// its hard-coded canned judge (mu-kgu.3 behavior)") — `hash-and-summary`
/// works out-of-the-box with no model spend.
///
/// A NON-EMPTY ranking where every entry fails to construct is
/// configured-intent-failed: warn and degrade to the heuristic
/// span-family drop policy instead (the smarter no-model policy), never
/// a silent no-op. This function never fails — every path yields a
/// working policy.
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
        "hash_keep" | "hash" => KeepListMode::HashKeep,
        // Unset/unknown defaults to IndexKeep (mu-0fla): HashKeep makes
        // the judge transcribe N opaque hashes verbatim, which fails
        // fail-closed on large ropes (observed live on a 658-span log).
        // Reaching the transcription-prone mode now requires an explicit
        // "hash_keep". (The type-level KeepListMode::default() stays
        // HashKeep for back-compat; this production fallback overrides it.)
        _ => KeepListMode::IndexKeep,
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
            // No provider available. Two distinct cases (mu-8bkf judge
            // round): EMPTY ranking is the deliberate zero-config path —
            // the config contract says fall back to the canned judge
            // (KeepHalfJudge: deterministic, no-network, keeps every
            // other span; mu-kgu.3 behavior). A NON-EMPTY ranking that
            // produced no judge is configured-intent-failed — degrade to
            // the heuristic span-family drop (the smarter no-model
            // policy), not the bench mock, and say so loudly.
            let ranking_count = cfg.judge.ranking.len();
            if ranking_count == 0 {
                tracing::info!(
                    output_mode = %cfg.judge.output_mode,
                    "compaction: hash-and-summary active with canned judge (no ranking \
                     configured; zero model spend)"
                );
                return Arc::new(
                    HashAndSummaryPolicy::new(Arc::new(KeepHalfJudge::new()))
                        .with_output_mode(output_mode),
                );
            }
            tracing::warn!(
                ranking_count,
                "compaction: all judge ranking entries unavailable; falling back to \
                 heuristic span-family drop (configure a constructible \
                 [compaction.judge] ranking entry to enable the judge)"
            );
            return Arc::new(mu_core::context::compaction::heuristic::SpanFamilyDropPolicy::new());
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
    use mu_core::capability::ConfigCapability;
    use mu_core::event_log::SessionEventLog;
    use mu_core::protocol::JSONRPC_VERSION;
    use serde_json::json;

    // ── mu-context-limits-wire phase 2: config-plane handlers ────────

    /// Insert a minimal LIVE session (real input channel + event log +
    /// shared live soft-limit cell) so the config handlers have a target.
    fn insert_live_session(
        sessions: &Sessions,
        id: &str,
        cap: Capability,
    ) -> (Arc<SessionEventLog>, Arc<AtomicU64>) {
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel(8);
        let log = Arc::new(SessionEventLog::new(id.to_string()));
        let live = Arc::new(AtomicU64::new(0));
        sessions.insert(
            id.to_string(),
            crate::serve::sessions::NewSession {
                input_tx,
                forwarder: tokio::spawn(async {}),
                agent: tokio::spawn(async {}),
                event_log: log.clone(),
                pending_approvals: Arc::new(Mutex::new(HashMap::new())),
                parent_session_id: None,
                capability: Arc::new(Mutex::new(cap)),
                cache_ttl: CacheTtl::default(),
                provider_status: Arc::new(Mutex::new(
                    crate::serve::provider_status::ProviderStatusTracker::new(),
                )),
                mailbox: Arc::new(crate::serve::mailbox::MailboxState::new()),
                status_watch: None,
                live_context_soft_limit: live.clone(),
            },
        );
        (log, live)
    }

    fn cfg_req(method: &str, params: Value) -> Request<Value> {
        Request {
            jsonrpc: JSONRPC_VERSION.into(),
            id: json!(1),
            method: method.into(),
            params,
        }
    }

    #[tokio::test]
    async fn set_config_soft_limit_updates_live_and_meter_then_gate_denies() {
        let sessions = Sessions::new();
        let (log, live) = insert_live_session(&sessions, "s1", Capability::root());
        // Seed a resolved snapshot so the hard limit (and mu-a79g output
        // budget) are known — the latter must survive the set_config
        // soft-limit change below (carry-forward, no route catalog here).
        log.append(
            EventActor::System,
            EventPayload::SessionConfigResolved {
                context_soft_limit: 200_000,
                context_hard_limit: Some(1_000_000),
                max_output_tokens: Some(32_000),
            },
        );
        live.store(200_000, Ordering::Relaxed);

        // get returns ONLY the requested keys.
        let resp = handle_get_config(
            cfg_req(
                GetConfigRequest::METHOD,
                json!({"session_id": "s1", "keys": ["context.soft_limit", "context.hard_limit"]}),
            ),
            sessions.clone(),
        )
        .await;
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["result"]["values"]["context.soft_limit"], json!(200_000));
        assert_eq!(
            v["result"]["values"]["context.hard_limit"],
            json!(1_000_000)
        );
        assert!(
            v["result"]["values"].get("context.used_tokens").is_none(),
            "an unrequested key must never appear in the response"
        );

        // set soft limit → applied; live cell AND the meter (event) update.
        let resp = handle_set_config(
            cfg_req(
                SetConfigRequest::METHOD,
                json!({"session_id": "s1", "entries": [{"key": "context.soft_limit", "value": 120_000}]}),
            ),
            sessions.clone(),
        )
        .await;
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            v["result"]["applied"][0]["key"],
            json!("context.soft_limit")
        );
        assert!(v["result"]["rejected"].as_array().unwrap().is_empty());
        assert_eq!(live.load(Ordering::Relaxed), 120_000);
        // mu-a79g: soft limit updated, hard limit + output budget carried
        // forward from the seeded snapshot (set_config has no catalog).
        assert_eq!(
            log.context_limits(),
            Some((120_000, Some(1_000_000), Some(32_000)))
        );

        // read-only key + unknown key are both rejected (others still apply).
        let resp = handle_set_config(
            cfg_req(
                SetConfigRequest::METHOD,
                json!({"session_id": "s1", "entries": [
                    {"key": "context.hard_limit", "value": 5},
                    {"key": "nope.key", "value": 1}
                ]}),
            ),
            sessions.clone(),
        )
        .await;
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["result"]["rejected"].as_array().unwrap().len(), 2);

        // capability gate: downgrade to ReadOnly → set denied, value unchanged.
        if let Some(c) = sessions.capability("s1") {
            c.lock().unwrap().config = ConfigCapability::ReadOnly;
        }
        let resp = handle_set_config(
            cfg_req(
                SetConfigRequest::METHOD,
                json!({"session_id": "s1", "entries": [{"key": "context.soft_limit", "value": 99_000}]}),
            ),
            sessions.clone(),
        )
        .await;
        let v = serde_json::to_value(&resp).unwrap();
        assert!(
            v["error"].is_object(),
            "set_config must be denied when config capability is ReadOnly"
        );
        assert_eq!(
            live.load(Ordering::Relaxed),
            120_000,
            "a denied set must not change the live value"
        );
        // ReadOnly can still read.
        let resp = handle_get_config(
            cfg_req(
                GetConfigRequest::METHOD,
                json!({"session_id": "s1", "keys": ["context.soft_limit"]}),
            ),
            sessions.clone(),
        )
        .await;
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["result"]["values"]["context.soft_limit"], json!(120_000));
    }

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
            context_soft_limit: Some(150_000),
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

    // mu-8bkf judge round: a NON-EMPTY ranking where no entry is
    // constructible is configured-intent-failed and must degrade to the
    // heuristic span-family drop — not the canned bench judge, which is
    // reserved for the deliberate zero-config (empty-ranking) path.
    #[test]
    fn all_unavailable_ranking_falls_back_to_heuristic_not_canned() {
        use mu_core::config::{CompactionConfig, CompactionJudgeConfig, JudgeRankingEntry};
        use mu_core::context::compaction::hash_summary::DEFAULT_POLICY_ID;

        let cfg = CompactionConfig {
            default_policy: "hash-and-summary".to_string(),
            context_soft_limit: Some(150_000),
            judge: CompactionJudgeConfig {
                ranking: vec![JudgeRankingEntry {
                    provider: "not-a-real-provider".to_string(),
                    model: "irrelevant".to_string(),
                    auth: "api_key".to_string(),
                }],
                ..Default::default()
            },
        };
        let policy = resolve_compaction_policy(&cfg)
            .expect("hash-and-summary with failed ranking must still resolve to Some");
        assert_eq!(
            policy.policy_label(),
            "span-family-drop",
            "configured-but-unconstructible ranking must degrade to heuristic"
        );

        // Contrast: the deliberate empty-ranking path stays hash-and-summary.
        let zero_config = CompactionConfig {
            default_policy: "hash-and-summary".to_string(),
            context_soft_limit: Some(150_000),
            ..Default::default()
        };
        assert_eq!(
            resolve_compaction_policy(&zero_config)
                .unwrap()
                .policy_label(),
            DEFAULT_POLICY_ID,
            "empty ranking keeps the canned-judge hash-and-summary path"
        );
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
        let tools = session_spawn_tools(
            &base,
            &sessions,
            &di,
            "session-42",
            &mu_core::capability::AutonomyCapability::Disallowed,
        );
        assert!(
            tools.iter().any(|t| t.spec().name == "spawn_worker"),
            "production session should get a spawn_worker tool",
        );
    }

    // mu-watch-tool-wakeup-o03p: the watch tool is injected per-session
    // alongside spawn_worker (production only), scoped to the session id
    // so a finished watch wakes the caller.
    #[test]
    fn session_spawn_tools_injects_watch_in_production() {
        let base: Vec<Arc<dyn Tool>> = vec![];
        let sessions = Sessions::new();
        let di = DaemonInfo::new("test")
            .with_events_dir(Some(std::path::PathBuf::from("/tmp/mu-test-events")));
        let tools = session_spawn_tools(
            &base,
            &sessions,
            &di,
            "session-42",
            &mu_core::capability::AutonomyCapability::Disallowed,
        );
        assert!(
            tools.iter().any(|t| t.spec().name == "watch"),
            "production session should get a watch tool",
        );
    }

    #[test]
    fn session_spawn_tools_omits_watch_without_events_dir() {
        let base: Vec<Arc<dyn Tool>> = vec![];
        let sessions = Sessions::new();
        let di = DaemonInfo::new("test"); // no events_dir (tests / ephemeral)
        let tools = session_spawn_tools(
            &base,
            &sessions,
            &di,
            "session-42",
            &mu_core::capability::AutonomyCapability::Disallowed,
        );
        assert!(
            !tools.iter().any(|t| t.spec().name == "watch"),
            "no events_dir => no watch tool",
        );
    }

    #[test]
    fn session_spawn_tools_omits_spawn_worker_without_events_dir() {
        let base: Vec<Arc<dyn Tool>> = vec![];
        let sessions = Sessions::new();
        let di = DaemonInfo::new("test"); // no events_dir (tests / ephemeral)
        let tools = session_spawn_tools(
            &base,
            &sessions,
            &di,
            "session-42",
            &mu_core::capability::AutonomyCapability::Disallowed,
        );
        assert!(
            !tools.iter().any(|t| t.spec().name == "spawn_worker"),
            "no events_dir => no spawn_worker tool",
        );
    }

    // mu-7e21: autonomy tools are capability-gated, independent of
    // events_dir — the tool list must be honest in both directions.
    #[test]
    fn session_spawn_tools_injects_autonomy_tools_when_granted() {
        use mu_core::capability::AutonomyCapability;
        let base: Vec<Arc<dyn Tool>> = vec![];
        let sessions = Sessions::new();
        let di = DaemonInfo::new("test"); // no events_dir — gate is capability, not pots
        let granted = AutonomyCapability::Allowed {
            max_iterations: 10,
            max_wall_clock_ms: 60_000,
            max_total_tool_calls_in_autonomy: 100,
            allow_schedule_wakeup: true,
            allow_delegate_grader: false,
        };
        let tools = session_spawn_tools(&base, &sessions, &di, "session-42", &granted);
        assert!(
            tools.iter().any(|t| t.spec().name == "start_autonomous"),
            "autonomy grant => start_autonomous tool present",
        );
        assert!(
            tools.iter().any(|t| t.spec().name == "schedule_wakeup"),
            "allow_schedule_wakeup => schedule_wakeup tool present",
        );
    }

    #[test]
    fn session_spawn_tools_omits_schedule_wakeup_when_not_allowed() {
        use mu_core::capability::AutonomyCapability;
        let base: Vec<Arc<dyn Tool>> = vec![];
        let sessions = Sessions::new();
        let di = DaemonInfo::new("test");
        let granted = AutonomyCapability::Allowed {
            max_iterations: 10,
            max_wall_clock_ms: 60_000,
            max_total_tool_calls_in_autonomy: 100,
            allow_schedule_wakeup: false,
            allow_delegate_grader: false,
        };
        let tools = session_spawn_tools(&base, &sessions, &di, "session-42", &granted);
        assert!(
            tools.iter().any(|t| t.spec().name == "start_autonomous"),
            "autonomy grant => start_autonomous tool present",
        );
        assert!(
            !tools.iter().any(|t| t.spec().name == "schedule_wakeup"),
            "allow_schedule_wakeup: false => no schedule_wakeup tool",
        );
    }

    #[test]
    fn session_spawn_tools_omits_autonomy_tools_when_disallowed() {
        use mu_core::capability::AutonomyCapability;
        let base: Vec<Arc<dyn Tool>> = vec![];
        let sessions = Sessions::new();
        let di = DaemonInfo::new("test")
            .with_events_dir(Some(std::path::PathBuf::from("/tmp/mu-test-events")));
        let tools = session_spawn_tools(
            &base,
            &sessions,
            &di,
            "session-42",
            &AutonomyCapability::Disallowed,
        );
        assert!(
            !tools
                .iter()
                .any(|t| t.spec().name == "start_autonomous" || t.spec().name == "schedule_wakeup"),
            "INV-1: no autonomy grant => no autonomy tools, even in production",
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

    /// mu-mh4 (panel finding 1): resuming a COLD/rehydrated predecessor
    /// (no live capability handle — the normal resume case) must FAIL
    /// CLOSED. The resumed session's capability must be the
    /// most-restrictive `read_only()` baseline, NOT `root()`. Falling back
    /// to root would let resume WIDEN privileges (attenuation-only-narrows
    /// violation). This pins the fix until capability persistence
    /// (mu-nqn5) lets us recover the predecessor's actual capability.
    #[tokio::test]
    async fn resume_of_cold_session_does_not_yield_root_authority() {
        use mu_core::capability::Capability;

        let predecessor_id = "cold-predecessor";
        // rehydrated_session_with_events uses `insert_rehydrated`, which
        // registers the log WITHOUT a live capability handle — exactly the
        // cold case that previously fell back to root().
        let sessions = rehydrated_session_with_events(predecessor_id);
        assert!(
            sessions.capability(predecessor_id).is_none(),
            "precondition: rehydrated predecessor has no live capability handle",
        );

        let factory = crate::serve::factory::make_provider_factory(false, None);
        let tools: Arc<Vec<Arc<dyn Tool>>> = Arc::new(Vec::new());
        let di = DaemonInfo::new("test-daemon"); // no events_dir — in-memory only

        let req = Request {
            jsonrpc: JSONRPC_VERSION.into(),
            id: json!(1),
            method: "session.resume".into(),
            params: json!({
                "session_ref": format!("test-daemon:{predecessor_id}"),
                "provider": { "kind": "anthropic_api", "model": "faux" },
            }),
        };

        let resp = handle_resume_session(
            req,
            mu_core::transport::NotificationWriter::sink(),
            sessions.clone(),
            factory,
            tools,
            Arc::new(Vec::new()),
            di,
        );
        let value = serde_json::to_value(&resp).expect("serialize response");
        let result = value
            .get("result")
            .unwrap_or_else(|| panic!("resume must succeed, got {value}"));
        let new_id = result["session_id"]
            .as_str()
            .expect("session_id in result")
            .to_string();

        let cap_handle = sessions
            .capability(&new_id)
            .expect("resumed session has a live capability handle");
        let cap = cap_handle.lock().expect("lock capability").clone();

        assert_ne!(
            cap,
            Capability::root(),
            "FAIL-CLOSED: resuming a cold session must NOT yield root authority",
        );
        assert_eq!(
            cap,
            Capability::read_only(),
            "resumed cold session must get the read_only fail-closed baseline",
        );
        // Spell out the load-bearing axes so a regression is legible.
        assert_eq!(
            cap.allowed_tools,
            Some(std::collections::HashSet::new()),
            "fail-closed baseline allows no tools",
        );
        assert!(
            matches!(
                cap.autonomy,
                mu_core::capability::AutonomyCapability::Disallowed
            ),
            "fail-closed baseline disallows autonomy",
        );
        assert_eq!(
            cap.max_side_effects,
            Some(mu_core::agent::tool::SideEffects::ReadOnly),
            "fail-closed baseline pins the side-effects ceiling to ReadOnly",
        );
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
