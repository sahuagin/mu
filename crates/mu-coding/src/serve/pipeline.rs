//! Ingest pipeline — the daemon's real border (spec mu-046, WP3).
//!
//! The named pattern: disruptor + event sourcing, with the core
//! treated like a matching engine. Adapters at the edges (stdio
//! JSON-RPC today; MCP in WP5), a sequenced durable journal in the
//! middle, a single-writer consumer processing in seq order, receipts
//! out. Every inbound request becomes a journaled command — fsync'd
//! per policy — BEFORE anything processes it (INV-1); a command that
//! cannot be made durable is rejected with `JOURNAL_UNAVAILABLE` and
//! never enqueued (INV-2, fail closed).
//!
//! Flow per command:
//!
//! 1. [`ingest`] — extract `session_id`, classify the method
//!    (daemon- vs session-scoped), redact secret-bearing params
//!    (INV-6), append `CommandReceived` to the daemon journal, enqueue
//!    into the control-plane queue. Journal-append + enqueue happen
//!    under one lock so journal seq order == queue order (INV-3).
//! 2. The control-plane consumer (single writer, INV-3) dequeues in
//!    order: auth gate first ([`super::dispatch::auth_gate`] — a
//!    rejection is journaled as `CommandRejected { stage: AuthGate }`,
//!    a receipt too), then routes through
//!    [`super::dispatch::dispatch_inner`]. Daemon-scoped commands run
//!    inline, preserving control-plane ordering; session-scoped
//!    commands are spawned so a slow session cannot stall the control
//!    plane (concurrency exists only across pipelines).
//! 3. On completion a receipt wrapping the original command (INV-5,
//!    [`CommandEcho`]) is journaled — `CommandSucceeded` /
//!    `CommandFailed` / `CommandRejected` — and the response leaves
//!    through the tagged outbound stream (INV-8). A receipt-append
//!    failure is logged and the response still goes out: the command
//!    is already durable, and the orphaned `CommandReceived` IS the
//!    legible marker (INV-4).

use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::Value;
use tokio::sync::mpsc;

use mu_core::agent::Tool;
use mu_core::command_journal::{
    AuthSnapshot, CommandEcho, CommandJournal, JournalPayload, Origin, RejectStage,
};
use mu_core::protocol::{
    AskSessionRequest, AuthInitiateRequest, CancelOutstandingRequest, CancelSessionRequest,
    CloseSessionRequest, Request, RespondToInputRequiredRequest, Response, ScheduleWakeupRequest,
    SessionEventsRequest, SessionStatsRequest, SetRouteRequest, SpawnWorkerRequest,
    StartAutonomousRequest,
};
use mu_core::skill::loader::LoadedSkill;
use mu_core::transport::{
    codes, err_response, NotificationWriter, Outbound, OutboundEnvelope, OutboundStream,
};

use super::auth::{AuthRegistry, AuthState, AuthStateHandle};
use super::daemon_info::DaemonInfo;
use super::discovery::SessionDiscovery;
use super::dispatch::{self, DispatchCtx};
use super::factory::ProviderFactory;
use super::sessions::Sessions;

/// Crash-injection seam for the spec mu-046 crash test (INV-1/INV-4):
/// a session-scoped, test-only method whose execution panics AFTER
/// ingest and before any receipt — leaving exactly one
/// `CommandReceived` on disk and an orphan on replay. Debug builds
/// only; release builds route it to `METHOD_NOT_FOUND` like any other
/// unknown method.
const TEST_PANIC_METHOD: &str = "mu.test.panic";

/// Which pipeline a method belongs to.
///
/// Interim (WP3): both scopes journal into the DAEMON journal and ride
/// the control-plane queue; the scope decides inline-vs-spawned
/// execution. WP4 moves session-scoped commands onto their session's
/// own log + queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// Control-plane: processed inline by the single-writer consumer.
    Daemon,
    /// Session-addressed: spawned so a session's input channel can
    /// never block the control plane.
    Session,
}

/// Route by method. Session-scoped methods are the session-addressed
/// verbs; everything else — including unknown methods, which the
/// router rejects with `METHOD_NOT_FOUND` → `CommandRejected{Routing}`
/// — is control-plane.
fn classify(method: &str) -> Scope {
    match method {
        m if m == AskSessionRequest::METHOD
            || m == CancelSessionRequest::METHOD
            || m == CancelOutstandingRequest::METHOD
            || m == CloseSessionRequest::METHOD
            || m == SessionStatsRequest::METHOD
            || m == SessionEventsRequest::METHOD
            || m == StartAutonomousRequest::METHOD
            || m == ScheduleWakeupRequest::METHOD
            || m == RespondToInputRequiredRequest::METHOD
            || m == SetRouteRequest::METHOD
            || m == SpawnWorkerRequest::METHOD
            || m == TEST_PANIC_METHOD =>
        {
            Scope::Session
        }
        _ => Scope::Daemon,
    }
}

/// Param fields that carry secrets, by method (spec mu-046 INV-6):
/// redacted before the params reach the journal — both the
/// `CommandReceived` record and the [`CommandEcho`] inside receipts.
/// Same posture as `config::SECRET_KEY_DENYLIST`: grow this with every
/// new secret-bearing method.
const SECRET_PARAM_FIELDS: &[(&str, &[&str])] =
    &[(AuthInitiateRequest::METHOD, &["initial_response"])];

/// Clone `params` with secret-bearing fields replaced by
/// `"[REDACTED]"`. Handlers still receive the original request — only
/// the journal sees the redacted copy.
fn redact_params(method: &str, params: &Value) -> Value {
    let mut params = params.clone();
    if let Some((_, fields)) = SECRET_PARAM_FIELDS.iter().find(|(m, _)| *m == method) {
        if let Some(obj) = params.as_object_mut() {
            for field in *fields {
                if let Some(v) = obj.get_mut(*field) {
                    *v = Value::String("[REDACTED]".to_string());
                }
            }
        }
    }
    params
}

/// The connection's auth state at the moment the command crossed the
/// border, projected into the journal's [`AuthSnapshot`]. A poisoned
/// lock snapshots as `Denied` — consistent with the gate's
/// fail-closed posture.
fn snapshot_auth(auth_state: &AuthStateHandle) -> AuthSnapshot {
    match auth_state.lock() {
        Ok(s) => match &*s {
            AuthState::Authenticated { .. } => AuthSnapshot::Authenticated,
            AuthState::Unauthenticated => AuthSnapshot::Unauthenticated,
            AuthState::Denied { .. } => AuthSnapshot::Denied,
        },
        Err(_poisoned) => AuthSnapshot::Denied,
    }
}

/// A journaled command in flight: the parsed inbound request plus its
/// border identity — origin, journal seq (THE command id), the
/// redacted params snapshot reused for receipt echoes, and the
/// connection's live auth handle for the consumer's gate.
pub(crate) struct Command {
    /// Journal seq of this command's `CommandReceived` (INV-3).
    seq: u64,
    request: Request<Value>,
    origin: Origin,
    /// Secret-redacted params (INV-6) — what receipts echo (INV-5).
    redacted_params: Value,
    scope: Scope,
    /// Live per-connection auth handle. The gate reads it at
    /// PROCESSING time, so a queued `peer.auth_initiate` authenticates
    /// the commands pipelined behind it (the journal's `AuthSnapshot`
    /// records the at-ingest state).
    auth_state: AuthStateHandle,
}

/// Producer-side handle on the control plane, held by every adapter
/// (stdio today, MCP in WP5). Dropping every handle closes the queue
/// and lets the consumer exit — the shutdown cascade's first domino.
pub(crate) struct ControlPlane {
    /// Journal-append + enqueue happen under this lock so journal seq
    /// order == queue order (INV-3) no matter how many adapters
    /// produce concurrently.
    seam: Mutex<IngestSeam>,
}

struct IngestSeam {
    journal: Arc<CommandJournal>,
    tx: mpsc::UnboundedSender<Command>,
}

/// Daemon-wide handles the consumer needs to build a [`DispatchCtx`]
/// per command (all cheap clones). The per-connection `auth_state`
/// rides each [`Command`] instead.
pub(crate) struct PipelineCtx {
    pub sessions: Sessions,
    pub factory: ProviderFactory,
    pub tools: Arc<Vec<Arc<dyn Tool>>>,
    pub skills: Arc<Vec<LoadedSkill>>,
    pub daemon_info: DaemonInfo,
    pub discovery: Arc<dyn SessionDiscovery>,
    pub auth_registry: Arc<AuthRegistry>,
}

impl PipelineCtx {
    fn dispatch_ctx(&self, auth_state: AuthStateHandle) -> DispatchCtx {
        DispatchCtx {
            sessions: self.sessions.clone(),
            factory: self.factory.clone(),
            tools: self.tools.clone(),
            skills: self.skills.clone(),
            daemon_info: self.daemon_info.clone(),
            discovery: self.discovery.clone(),
            auth_registry: self.auth_registry.clone(),
            auth_state,
        }
    }
}

/// Spawn the control-plane consumer (single writer, INV-3) and return
/// the producer handle. The consumer owns the daemon's session map et
/// al. via `ctx`; it exits — releasing them — when the last producer
/// handle drops.
pub(crate) fn spawn_control_plane(
    journal: Arc<CommandJournal>,
    ctx: PipelineCtx,
    stream: OutboundStream,
) -> ControlPlane {
    let (tx, mut rx) = mpsc::unbounded_channel::<Command>();
    let consumer_journal = journal.clone();
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            process_command(cmd, &ctx, &consumer_journal, &stream).await;
        }
    });
    ControlPlane {
        seam: Mutex::new(IngestSeam { journal, tx }),
    }
}

/// The border crossing (spec mu-046 INV-1/INV-2). Journal
/// `CommandReceived` — fsync'd per policy — then enqueue; under one
/// lock so seq order == queue order (INV-3).
///
/// Returns `Some(response)` only for immediate rejects (journal
/// unavailable, daemon shutting down) — the transport envelopes and
/// sends those. `None` means the command was accepted: its response
/// arrives via the outbound stream once the pipeline processes it.
pub(crate) fn ingest(
    control: &ControlPlane,
    request: Request<Value>,
    origin: Origin,
    auth_state: &AuthStateHandle,
) -> Option<Response<Value>> {
    let session_id = request
        .params
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let scope = classify(&request.method);
    let redacted_params = redact_params(&request.method, &request.params);
    let auth = snapshot_auth(auth_state);

    let seam = match control.seam.lock() {
        Ok(seam) => seam,
        // A poisoned seam means a panic mid-append: durability is no
        // longer certain, so fail closed (INV-2).
        Err(_poisoned) => {
            return Some(err_response(
                request.id,
                codes::JOURNAL_UNAVAILABLE,
                "command journal unavailable: ingest seam poisoned",
            ));
        }
    };
    // mu-046 WP4: session-scoped commands (`scope == Scope::Session`)
    // will journal into their session's own event log via
    // `SessionEventLog::append_command` here instead; interim, ALL
    // commands journal into the daemon control-plane journal.
    let appended = seam.journal.append(JournalPayload::CommandReceived {
        request_id: request.id.clone(),
        method: request.method.clone(),
        params: redacted_params.clone(),
        session_id,
        auth,
        origin: origin.clone(),
    });
    let seq = match appended {
        Ok(seq) => seq,
        Err(err) => {
            // INV-2 (fail closed): not durable ⇒ never enqueued, never
            // processed.
            tracing::error!(
                %err,
                method = %request.method,
                "command journal append failed; rejecting command"
            );
            return Some(err_response(
                request.id,
                codes::JOURNAL_UNAVAILABLE,
                format!("command journal unavailable: {err}"),
            ));
        }
    };
    let command = Command {
        seq,
        request,
        origin,
        redacted_params,
        scope,
        auth_state: auth_state.clone(),
    };
    if let Err(send_err) = seam.tx.send(command) {
        // Consumer gone — daemon shutting down. The command is durable
        // (journaled, no receipt: a legible orphan) but won't run.
        return Some(err_response(
            send_err.0.request.id,
            codes::INTERNAL_ERROR,
            "control plane unavailable (daemon shutting down)",
        ));
    }
    None
}

/// One consumer tick: gate, route, receipt, respond.
async fn process_command(
    cmd: Command,
    ctx: &PipelineCtx,
    journal: &Arc<CommandJournal>,
    stream: &OutboundStream,
) {
    // Gate first, route second (mu-fnn) — an unauthenticated
    // METHOD_NOT_FOUND would reveal routing surface. The rejection is
    // a receipt too (spec mu-046 receipt semantics): journaled even
    // though no handler ran.
    if let Err((code, message)) = dispatch::auth_gate(&cmd.auth_state, &cmd.request.method) {
        let receipt = JournalPayload::CommandRejected {
            command_seq: cmd.seq,
            command: command_echo(&cmd),
            code,
            message: message.clone(),
            stage: RejectStage::AuthGate,
        };
        append_receipt(journal, cmd.seq, receipt);
        emit_response(
            stream,
            &cmd.origin,
            cmd.seq,
            err_response(cmd.request.id, code, message),
        );
        return;
    }
    let notif = NotificationWriter::for_origin(stream.clone(), cmd.origin.clone());
    let dctx = ctx.dispatch_ctx(cmd.auth_state.clone());
    match cmd.scope {
        // Daemon-scoped: inline, preserving control-plane ordering
        // (INV-3: seq order == processing order).
        Scope::Daemon => {
            execute_and_receipt(cmd, notif, dctx, journal.clone(), stream.clone()).await
        }
        // Session-scoped: spawned — the control plane must never block
        // on a session's input channel. Ordering holds within the
        // control plane; concurrency exists only across pipelines.
        Scope::Session => {
            tokio::spawn(execute_and_receipt(
                cmd,
                notif,
                dctx,
                journal.clone(),
                stream.clone(),
            ));
        }
    }
}

/// Run the routed handler, journal the receipt wrapping the original
/// command (INV-5), emit the enveloped response (INV-8).
async fn execute_and_receipt(
    cmd: Command,
    notif: NotificationWriter,
    dctx: DispatchCtx,
    journal: Arc<CommandJournal>,
    stream: OutboundStream,
) {
    let echo = command_echo(&cmd);
    let Command {
        seq,
        request,
        origin,
        ..
    } = cmd;
    if cfg!(debug_assertions) && request.method == TEST_PANIC_METHOD {
        // See TEST_PANIC_METHOD: dies after ingest, before any receipt.
        panic!("{TEST_PANIC_METHOD}: injected post-ingest crash (spec mu-046 crash test)");
    }
    let started = Instant::now();
    let response = dispatch::dispatch_inner(request, notif, dctx).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    append_receipt(&journal, seq, receipt_for(seq, echo, &response, elapsed_ms));
    emit_response(&stream, &origin, seq, response);
}

fn command_echo(cmd: &Command) -> CommandEcho {
    CommandEcho {
        request_id: cmd.request.id.clone(),
        method: cmd.request.method.clone(),
        params: cmd.redacted_params.clone(),
    }
}

/// Classify a handler outcome into its receipt. `INVALID_PARAMS` /
/// `METHOD_NOT_FOUND` are pre-handler-effect refusals —
/// `CommandRejected { Validation | Routing }`; other errors are
/// processing failures (`CommandFailed`).
fn receipt_for(
    command_seq: u64,
    command: CommandEcho,
    response: &Response<Value>,
    elapsed_ms: u64,
) -> JournalPayload {
    match response {
        Response::Ok { result, .. } => JournalPayload::CommandSucceeded {
            command_seq,
            command,
            result: result.clone(),
            elapsed_ms,
        },
        Response::Err { error, .. } => match error.code {
            codes::INVALID_PARAMS => JournalPayload::CommandRejected {
                command_seq,
                command,
                code: error.code,
                message: error.message.clone(),
                stage: RejectStage::Validation,
            },
            codes::METHOD_NOT_FOUND => JournalPayload::CommandRejected {
                command_seq,
                command,
                code: error.code,
                message: error.message.clone(),
                stage: RejectStage::Routing,
            },
            _ => JournalPayload::CommandFailed {
                command_seq,
                command,
                code: error.code,
                message: error.message.clone(),
                elapsed_ms,
            },
        },
    }
}

/// Append a receipt. Failure is logged, never fatal: the command is
/// already durable, and the orphaned `CommandReceived` IS the legible
/// marker (INV-4). The response still goes out.
fn append_receipt(journal: &Arc<CommandJournal>, command_seq: u64, receipt: JournalPayload) {
    if let Err(err) = journal.append(receipt) {
        tracing::error!(
            %err,
            command_seq,
            "receipt append failed; command stays an orphan in the journal"
        );
    }
}

/// One way out (INV-8): envelope the response with the originating
/// connection + journal correlation and send it to the outbound
/// stream; the connection's write loop delivers it.
fn emit_response(
    stream: &OutboundStream,
    origin: &Origin,
    command_seq: u64,
    response: Response<Value>,
) {
    let request_id = match &response {
        Response::Ok { id, .. } | Response::Err { id, .. } => id.clone(),
    };
    match serde_json::to_value(response) {
        Ok(value) => stream.send(OutboundEnvelope {
            origin: Some(origin.clone()),
            request_id: Some(request_id),
            command_seq: Some(command_seq),
            item: Outbound(value),
        }),
        Err(err) => tracing::warn!(%err, "response serialization failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_core::command_journal::FsyncPolicy;
    use mu_core::config::Config;
    use mu_core::protocol::JSONRPC_VERSION;
    use serde_json::json;

    fn test_ctx() -> PipelineCtx {
        let sessions = Sessions::new();
        let factory: ProviderFactory = Arc::new(|_selector, _cache_ttl| {
            Err(anyhow::anyhow!("no provider in pipeline unit tests"))
        });
        let daemon_info = DaemonInfo::new("test");
        let discovery: Arc<dyn SessionDiscovery> =
            Arc::new(super::super::LocalRegistryBackend::new(
                sessions.clone(),
                daemon_info.daemon_id().to_string(),
            ));
        PipelineCtx {
            sessions,
            factory,
            tools: Arc::new(Vec::new()),
            skills: Arc::new(Vec::new()),
            daemon_info,
            discovery,
            auth_registry: Arc::new(super::super::auth::registry_from_config(
                &Config::default().auth,
            )),
        }
    }

    fn request(method: &str, params: Value) -> Request<Value> {
        Request {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: json!(1),
            method: method.to_string(),
            params,
        }
    }

    fn authed_state() -> AuthStateHandle {
        Arc::new(std::sync::Mutex::new(AuthState::Authenticated {
            capability: mu_core::capability::Capability::root(),
        }))
    }

    /// INV-2 (fail closed) at the ingest seam: when the journal cannot
    /// accept the append — here, the seam is poisoned by a panic
    /// mid-ingest — the command gets `JOURNAL_UNAVAILABLE` and NO
    /// handler runs. Observable: `create_session` against the broken
    /// journal creates no session.
    #[tokio::test]
    async fn poisoned_seam_fails_closed_with_journal_unavailable_inv2() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Arc::new(
            CommandJournal::open(&dir.path().join("d.jsonl"), "d", FsyncPolicy::Never)
                .expect("open journal"),
        );
        let ctx = test_ctx();
        let sessions = ctx.sessions.clone();
        let stream = OutboundStream::new();
        let control = Arc::new(spawn_control_plane(journal.clone(), ctx, stream));

        // Poison the ingest seam: a thread panics while holding it.
        let poisoner = Arc::clone(&control);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.seam.lock().unwrap();
            panic!("poison the ingest seam");
        })
        .join();

        let req = request(
            mu_core::protocol::CreateSessionRequest::METHOD,
            json!({ "provider": { "kind": "anthropic_api", "model": "x" } }),
        );
        let origin = Origin {
            transport: "test".into(),
            connection_id: None,
        };
        let auth = authed_state();
        let response = ingest(&control, req, origin, &auth)
            .expect("broken journal must reject immediately (INV-2)");
        match response {
            Response::Err { error, .. } => {
                assert_eq!(error.code, codes::JOURNAL_UNAVAILABLE);
            }
            Response::Ok { .. } => panic!("expected JOURNAL_UNAVAILABLE error"),
        }
        // No handler ran: the command was never enqueued, so no
        // session exists and nothing past JournalOpened hit the file.
        assert!(sessions.snapshot_for_listing().is_empty());
        let (records, _) = CommandJournal::replay(&dir.path().join("d.jsonl")).expect("replay");
        assert_eq!(records.len(), 1, "only JournalOpened: {records:?}");
    }

    /// Secret-bearing params are redacted before they reach the
    /// journal (INV-6); other methods' params pass through untouched.
    #[test]
    fn redact_params_strips_auth_initiate_secret() {
        let redacted = redact_params(
            AuthInitiateRequest::METHOD,
            &json!({ "mechanism": "bearer", "initial_response": "hunter2" }),
        );
        assert_eq!(redacted["initial_response"], "[REDACTED]");
        assert_eq!(redacted["mechanism"], "bearer");

        let untouched = redact_params("ping", &json!({ "initial_response": "not-a-secret-here" }));
        assert_eq!(untouched["initial_response"], "not-a-secret-here");
    }

    /// Route-by-scope: session-addressed verbs spawn off the control
    /// plane; everything else (incl. unknown methods) stays on it.
    #[test]
    fn classify_routes_session_verbs_to_session_scope() {
        assert_eq!(classify(AskSessionRequest::METHOD), Scope::Session);
        assert_eq!(classify(CloseSessionRequest::METHOD), Scope::Session);
        assert_eq!(classify(SpawnWorkerRequest::METHOD), Scope::Session);
        assert_eq!(classify("ping"), Scope::Daemon);
        assert_eq!(classify("create_session"), Scope::Daemon);
        assert_eq!(classify("mailbox.post"), Scope::Daemon);
        assert_eq!(classify("peer.auth_initiate"), Scope::Daemon);
        assert_eq!(classify("no.such.method"), Scope::Daemon);
    }
}
