//! Ingest pipeline — the daemon's real border (spec mu-046, WP3–WP5).
//!
//! The named pattern: disruptor + event sourcing, with the core
//! treated like a matching engine. Adapters at the edges (stdio
//! JSON-RPC and the MCP socket — see `serve/mcp.rs`, adapter #2 since
//! WP5), a sequenced durable journal in the
//! middle, a single-writer consumer processing in seq order, receipts
//! out. Every inbound request becomes a journaled command — fsync'd
//! per policy — BEFORE anything processes it (INV-1); a command that
//! cannot be made durable is rejected with `JOURNAL_UNAVAILABLE` and
//! never enqueued (INV-2, fail closed).
//!
//! Flow per command:
//!
//! 1. [`ingest`] — extract the addressed session id, classify the
//!    method (daemon- vs session-scoped), redact secret-bearing
//!    params (INV-6), then journal `CommandReceived` into the
//!    command's own pipeline journal:
//!    - **Session-scoped** commands journal into the addressed
//!      session's OWN event log via the strict
//!      [`SessionEventLog::append_command`] (fsync'd, errors
//!      propagate) — WP4. The session-log event id is the command id
//!      receipts correlate by (`command_event_id`).
//!    - **Daemon-scoped** commands — and the two documented
//!      session-scoped FALLBACK cases below — journal into the daemon
//!      control-plane journal (WP3).
//!
//!    Journal-append + enqueue happen under one lock so journal seq
//!    order == queue order (INV-3).
//!
//! 2. The control-plane consumer (single writer, INV-3) dequeues in
//!    order: auth gate first ([`super::dispatch::auth_gate`] — a
//!    rejection is journaled as `CommandRejected { stage: AuthGate }`
//!    into the same journal slot, a receipt too), then routes through
//!    [`super::dispatch::dispatch_inner`]. Daemon-scoped commands run
//!    inline, preserving control-plane ordering; session-scoped
//!    commands are spawned so a slow session cannot stall the control
//!    plane (concurrency exists only across pipelines).
//! 3. On completion a receipt wrapping the original command (INV-5,
//!    [`CommandEcho`]) is journaled into the command's slot —
//!    `CommandSucceeded` / `CommandFailed` / `CommandRejected` — and
//!    the response leaves through the tagged outbound stream (INV-8).
//!    A receipt-append failure is logged and the response still goes
//!    out: the command is already durable, and the orphaned
//!    `CommandReceived` IS the legible marker (INV-4).
//!
//! ## Session-log routing fallbacks (WP4, documented rule)
//!
//! A session-scoped command journals into the daemon journal instead
//! of the session's log when:
//!
//! - **The session is unresolvable** (no `session_id` param, or no
//!   in-memory session under that id). The border record must always
//!   exist somewhere; the daemon journal carries both the
//!   `CommandReceived` and the eventual `CommandRejected`/`Failed`
//!   (typically "session not found"). The lookup is in-memory only —
//!   the border does not lazily resurrect read-only ghosts from disk
//!   just to address them (same posture as `close_session`).
//! - **The session's log has no disk writer attached** (e.g.
//!   `persist_events_to_disk = false` configs, plain in-memory test
//!   sessions). `append_command` on such a log errors `Unsupported`
//!   by design — an in-memory log cannot make a command durable — so
//!   the pipeline checks [`SessionEventLog::has_disk_writer`] and
//!   routes these to the daemon journal EXPLICITLY: border compliance
//!   is preserved; session-log strictness needs disk.
//!
//! In both fallback cases the command keeps the WP3 receipt shape
//! (receipt in the daemon journal at handler completion — including
//! the immediate `accepted: true` receipt for `ask_session`, since
//! there is no session log for a Done-time receipt to land in).
//!
//! ## `ask_session` receipt deferral (WP4, spec "Receipt semantics")
//!
//! For an `ask_session` journaled in the session's log, the wire
//! response (`accepted: true`) stays immediate, but the receipt
//! records the PROCESSING outcome: the pipeline mints a
//! [`CommandTicket`] (command_event_id + echo), threads it through
//! `dispatch_inner` → `handle_ask_session` → `AgentInput::UserMessage`
//! into the agent loop, and the forwarder writes `CommandSucceeded`
//! at the turn's `Done` / `CommandFailed` on `Error`-or-`Aborted`
//! (see `super::forwarder`). Only an ACCEPTED ask defers: if the
//! handler returns an error (session not found mid-flight, input
//! channel closed — delivery failure is an outcome), the pipeline
//! writes the failure receipt itself at handler completion.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::Value;
use tokio::sync::mpsc;

use mu_core::agent::Tool;
use mu_core::command_journal::{
    AuthSnapshot, CommandEcho, CommandJournal, CommandTicket, JournalPayload, Origin, RejectStage,
};
use mu_core::event_log::{EventActor, EventPayload, SessionEventLog};
use mu_core::protocol::{
    AskSessionRequest, AuthInitiateRequest, CancelOutstandingRequest, CancelSessionRequest,
    CloseSessionRequest, MailboxPostRequest, Request, RespondToInputRequiredRequest, Response,
    ScheduleWakeupRequest, SessionEventsRequest, SessionStatsRequest, SetRouteRequest,
    SpawnWorkerRequest, StartAutonomousRequest,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// Control-plane: processed inline by the single-writer consumer.
    Daemon,
    /// Session-addressed: journals into the session's own event log
    /// (WP4; daemon-journal fallback per the module doc) and is
    /// spawned so a session's input channel can never block the
    /// control plane.
    Session,
}

/// Route by method. Session-scoped methods are the session-addressed
/// verbs — including `mailbox.post`, which is addressed to a target
/// session (`to_session_id`) — everything else, including unknown
/// methods (which the router rejects with `METHOD_NOT_FOUND` →
/// `CommandRejected{Routing}`), is control-plane.
///
/// `mcp.*` methods (spec mu-046 WP5) mirror their underlying handler's
/// scope: `mcp.mu_mailbox_post` is session-scoped exactly like
/// `mailbox.post` (the raw MCP arguments carry the same top-level
/// `to_session_id` that [`addressed_session_id`] reads); every other
/// MCP tool — `mu_daemon_info`, `mu_peer_hello`, and the
/// `mu_mailbox_list`/`read`/`consume` reads — mirrors a control-plane
/// method and falls through to `Scope::Daemon` like its native twin
/// (full table in the `serve/mcp.rs` module doc).
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
            || m == MailboxPostRequest::METHOD
            || m == dispatch::MCP_MAILBOX_POST_METHOD
            || m == TEST_PANIC_METHOD =>
        {
            Scope::Session
        }
        _ => Scope::Daemon,
    }
}

/// The session a command addresses, by param. Session-scoped verbs
/// carry `session_id`; `mailbox.post` addresses its target via
/// `to_session_id` (the wire mixes the two shapes — the protocol
/// request types are authoritative).
fn addressed_session_id(request: &Request<Value>) -> Option<String> {
    request
        .params
        .get("session_id")
        .or_else(|| request.params.get("to_session_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
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

/// Where a command's `CommandReceived` was journaled — and therefore
/// where its receipt must land (one journal per pipeline, INV-3/4/5).
enum JournalSlot {
    /// Daemon control-plane journal (WP3 path; also the documented
    /// fallback for session-scoped commands — see module doc).
    Daemon { seq: u64 },
    /// The addressed session's own event log (WP4). `event_id` is the
    /// session-log id of the `CommandReceived` — the
    /// `command_event_id` receipts correlate by.
    Session {
        log: Arc<SessionEventLog>,
        event_id: u64,
    },
}

impl JournalSlot {
    /// The command id within its pipeline, for outbound correlation.
    fn command_id(&self) -> u64 {
        match self {
            JournalSlot::Daemon { seq } => *seq,
            JournalSlot::Session { event_id, .. } => *event_id,
        }
    }
}

/// A journaled command in flight: the parsed inbound request plus its
/// border identity — origin, journal slot (THE command id), the
/// redacted params snapshot reused for receipt echoes, and the
/// connection's live auth handle for the consumer's gate.
pub(crate) struct Command {
    /// Where this command's `CommandReceived` landed (INV-3).
    slot: JournalSlot,
    request: Request<Value>,
    origin: Origin,
    /// Secret-redacted params (INV-6) — what receipts echo (INV-5).
    redacted_params: Value,
    scope: Scope,
    /// Unix ms at ingest — receipts compute `elapsed_ms` from this.
    received_at_unix_ms: u64,
    /// Live per-connection auth handle. The gate reads it at
    /// PROCESSING time, so a queued `peer.auth_initiate` authenticates
    /// the commands pipelined behind it (the journal's `AuthSnapshot`
    /// records the at-ingest state).
    auth_state: AuthStateHandle,
}

/// Producer-side handle on the control plane, held by every adapter
/// (stdio and MCP). Dropping every handle closes the queue and lets
/// the consumer exit — the shutdown cascade's first domino.
pub(crate) struct ControlPlane {
    /// Session registry, consulted at ingest/route time to resolve a
    /// session-scoped command's own event log (WP4). A registry clone
    /// here is shutdown-safe: the ControlPlane is owned by the
    /// transport closure and drops on EOF, before the consumer's own
    /// `PipelineCtx` clone needs to be the last one standing.
    sessions: Sessions,
    /// Journal-append + enqueue happen under this lock so journal seq
    /// order == queue order (INV-3) no matter how many adapters
    /// produce concurrently.
    seam: Mutex<IngestSeam>,
}

struct IngestSeam {
    journal: Arc<CommandJournal>,
    tx: mpsc::UnboundedSender<Command>,
}

#[cfg(test)]
impl ControlPlane {
    /// Test seam for the INV-2 fail-closed path: poison the ingest
    /// seam (a thread panics while holding it) so every subsequent
    /// [`ingest`] rejects with `JOURNAL_UNAVAILABLE` before any append
    /// or enqueue. Used by the in-crate pipeline and MCP adapter tests.
    pub(crate) fn poison_ingest_seam_for_tests(self: &Arc<Self>) {
        let poisoner = Arc::clone(self);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.seam.lock().unwrap();
            panic!("poison the ingest seam (test)");
        })
        .join();
    }
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
    let sessions = ctx.sessions.clone();
    let consumer_journal = journal.clone();
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            process_command(cmd, &ctx, &consumer_journal, &stream).await;
        }
    });
    ControlPlane {
        sessions,
        seam: Mutex::new(IngestSeam { journal, tx }),
    }
}

/// The border crossing (spec mu-046 INV-1/INV-2). Journal
/// `CommandReceived` — fsync'd per policy — then enqueue; under one
/// lock so seq order == queue order (INV-3). Session-scoped commands
/// journal into their session's own event log (WP4); the daemon
/// journal is the documented fallback (module doc).
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
    let session_id = addressed_session_id(&request);
    let scope = classify(&request.method);
    let redacted_params = redact_params(&request.method, &request.params);
    let auth = snapshot_auth(auth_state);
    let received_at_unix_ms = now_unix_ms();

    // WP4 routing: a session-scoped command journals into the
    // addressed session's own log IFF that session is in memory AND
    // its log can take the strict append (disk writer attached).
    // Resolved BEFORE the seam lock — the registry has its own locks
    // and nesting them under the seam invites ordering hazards.
    let session_log: Option<Arc<SessionEventLog>> = match scope {
        Scope::Session => session_id
            .as_deref()
            .and_then(|id| control.sessions.event_log_in_memory(id))
            .filter(|log| log.has_disk_writer()),
        Scope::Daemon => None,
    };

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
    let slot = match session_log {
        // Session pipeline (WP4): strict fsync'd append into the
        // session's own log, BEFORE the session's input queue can see
        // the command (INV-1). The command crossed the border from
        // the client, so the record's actor is `User`; receipts are
        // written by the daemon and carry `System`.
        Some(log) => {
            let appended = log.append_command(
                EventActor::User,
                EventPayload::CommandReceived {
                    request_id: request.id.clone(),
                    method: request.method.clone(),
                    params: redacted_params.clone(),
                    auth,
                    origin: origin.clone(),
                },
            );
            match appended {
                Ok(event_id) => JournalSlot::Session { log, event_id },
                Err(err) => {
                    // INV-2 (fail closed): not durable ⇒ never
                    // enqueued, never processed. (`Unsupported` can't
                    // reach here — has_disk_writer gated above — so
                    // this is a real IO failure.)
                    tracing::error!(
                        %err,
                        method = %request.method,
                        session_id = ?session_id,
                        "session event-log command append failed; rejecting command"
                    );
                    return Some(err_response(
                        request.id,
                        codes::JOURNAL_UNAVAILABLE,
                        format!("command journal unavailable: {err}"),
                    ));
                }
            }
        }
        // Daemon control plane — daemon-scoped commands plus the
        // session-scoped fallback cases (module doc).
        None => {
            let appended = seam.journal.append(JournalPayload::CommandReceived {
                request_id: request.id.clone(),
                method: request.method.clone(),
                params: redacted_params.clone(),
                session_id,
                auth,
                origin: origin.clone(),
            });
            match appended {
                Ok(seq) => JournalSlot::Daemon { seq },
                Err(err) => {
                    // INV-2 (fail closed): not durable ⇒ never
                    // enqueued, never processed.
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
            }
        }
    };
    let command = Command {
        slot,
        request,
        origin,
        redacted_params,
        scope,
        received_at_unix_ms,
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
    // though no handler ran — into the same slot the CommandReceived
    // landed in.
    if let Err((code, message)) = dispatch::auth_gate(&cmd.auth_state, &cmd.request.method) {
        append_receipt(
            journal,
            &cmd.slot,
            command_echo(&cmd),
            ReceiptBody::Rejected {
                code,
                message: message.clone(),
                stage: RejectStage::AuthGate,
            },
        );
        emit_response(
            stream,
            &cmd.origin,
            cmd.slot.command_id(),
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
/// command (INV-5) into the command's slot, emit the enveloped
/// response (INV-8). The one deliberate non-receipt: an ACCEPTED
/// `ask_session` on a session slot — its receipt records the
/// processing outcome and is written by the forwarder at the turn's
/// `Done`/`Error`, correlated via the [`CommandTicket`] threaded
/// through the handler (module doc, "ask_session receipt deferral").
async fn execute_and_receipt(
    cmd: Command,
    notif: NotificationWriter,
    dctx: DispatchCtx,
    journal: Arc<CommandJournal>,
    stream: OutboundStream,
) {
    let echo = command_echo(&cmd);
    let Command {
        slot,
        request,
        origin,
        received_at_unix_ms,
        ..
    } = cmd;
    if cfg!(debug_assertions) && request.method == TEST_PANIC_METHOD {
        // See TEST_PANIC_METHOD: dies after ingest, before any receipt.
        panic!("{TEST_PANIC_METHOD}: injected post-ingest crash (spec mu-046 crash test)");
    }
    // Mint the deferral ticket for session-slot asks (WP4). The ticket
    // carries the receipt correlation EXPLICITLY into the agent loop;
    // the pipeline keeps its own `echo` for the failure path.
    let ask_ticket: Option<CommandTicket> = match &slot {
        JournalSlot::Session { event_id, .. } if request.method == AskSessionRequest::METHOD => {
            Some(CommandTicket {
                command_event_id: *event_id,
                echo: echo.clone(),
                received_at_unix_ms,
            })
        }
        _ => None,
    };
    let deferred = ask_ticket.is_some();
    let started = Instant::now();
    let response = dispatch::dispatch_inner(request, notif, dctx, ask_ticket).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match (&response, deferred) {
        // Accepted session-slot ask: the ticket is in the agent
        // loop's input queue; the forwarder writes this command's
        // receipt at the turn's terminal Done/Error.
        (Response::Ok { .. }, true) => {}
        // Everything else — including a REJECTED/FAILED ask (the
        // ticket died with the undelivered input) — receipts here.
        _ => append_receipt(
            &journal,
            &slot,
            echo,
            receipt_body_for(&response, elapsed_ms),
        ),
    }
    emit_response(&stream, &origin, slot.command_id(), response);
}

fn command_echo(cmd: &Command) -> CommandEcho {
    CommandEcho {
        request_id: cmd.request.id.clone(),
        method: cmd.request.method.clone(),
        params: cmd.redacted_params.clone(),
    }
}

/// Journal-agnostic receipt shape: projected into [`JournalPayload`]
/// (daemon slot) or [`EventPayload`] (session slot) by
/// [`append_receipt`].
enum ReceiptBody {
    Succeeded {
        result: Value,
        elapsed_ms: u64,
    },
    Failed {
        code: i32,
        message: String,
        elapsed_ms: u64,
    },
    Rejected {
        code: i32,
        message: String,
        stage: RejectStage,
    },
}

/// Classify a handler outcome into its receipt. `INVALID_PARAMS` /
/// `METHOD_NOT_FOUND` are pre-handler-effect refusals —
/// `CommandRejected { Validation | Routing }`; other errors are
/// processing failures (`CommandFailed`).
fn receipt_body_for(response: &Response<Value>, elapsed_ms: u64) -> ReceiptBody {
    match response {
        Response::Ok { result, .. } => ReceiptBody::Succeeded {
            result: result.clone(),
            elapsed_ms,
        },
        Response::Err { error, .. } => match error.code {
            codes::INVALID_PARAMS => ReceiptBody::Rejected {
                code: error.code,
                message: error.message.clone(),
                stage: RejectStage::Validation,
            },
            codes::METHOD_NOT_FOUND => ReceiptBody::Rejected {
                code: error.code,
                message: error.message.clone(),
                stage: RejectStage::Routing,
            },
            _ => ReceiptBody::Failed {
                code: error.code,
                message: error.message.clone(),
                elapsed_ms,
            },
        },
    }
}

/// Append a receipt to the command's journal slot. Receipts are
/// outcomes, not intake, so a failure is logged, never fatal: the
/// command is already durable, and the orphaned `CommandReceived` IS
/// the legible marker (INV-4). The response still goes out. Session
/// slots use the strict `append_command` (the log has a disk writer
/// by construction — preferred over the best-effort `append` so a
/// receipt is fsync'd-durable before the response leaves), with
/// errors landing here as the logged orphan.
fn append_receipt(
    journal: &Arc<CommandJournal>,
    slot: &JournalSlot,
    echo: CommandEcho,
    body: ReceiptBody,
) {
    match slot {
        JournalSlot::Daemon { seq } => {
            let payload = match body {
                ReceiptBody::Succeeded { result, elapsed_ms } => JournalPayload::CommandSucceeded {
                    command_seq: *seq,
                    command: echo,
                    result,
                    elapsed_ms,
                },
                ReceiptBody::Failed {
                    code,
                    message,
                    elapsed_ms,
                } => JournalPayload::CommandFailed {
                    command_seq: *seq,
                    command: echo,
                    code,
                    message,
                    elapsed_ms,
                },
                ReceiptBody::Rejected {
                    code,
                    message,
                    stage,
                } => JournalPayload::CommandRejected {
                    command_seq: *seq,
                    command: echo,
                    code,
                    message,
                    stage,
                },
            };
            if let Err(err) = journal.append(payload) {
                tracing::error!(
                    %err,
                    command_seq = seq,
                    "receipt append failed; command stays an orphan in the journal"
                );
            }
        }
        JournalSlot::Session { log, event_id } => {
            let payload = match body {
                ReceiptBody::Succeeded { result, elapsed_ms } => EventPayload::CommandSucceeded {
                    command_event_id: *event_id,
                    command: echo,
                    result,
                    elapsed_ms,
                },
                ReceiptBody::Failed {
                    code,
                    message,
                    elapsed_ms,
                } => EventPayload::CommandFailed {
                    command_event_id: *event_id,
                    command: echo,
                    code,
                    message,
                    elapsed_ms,
                },
                ReceiptBody::Rejected {
                    code,
                    message,
                    stage,
                } => EventPayload::CommandRejected {
                    command_event_id: *event_id,
                    command: echo,
                    code,
                    message,
                    stage,
                },
            };
            if let Err(err) = log.append_command(EventActor::System, payload) {
                tracing::error!(
                    %err,
                    command_event_id = event_id,
                    session_id = %log.session_id(),
                    "receipt append failed; command stays an orphan in the session log"
                );
            }
        }
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

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_core::agent::AgentInput;
    use mu_core::capability::Capability;
    use mu_core::command_journal::FsyncPolicy;
    use mu_core::config::Config;
    use mu_core::context::CacheTtl;
    use mu_core::protocol::JSONRPC_VERSION;
    use serde_json::json;
    use std::collections::HashMap;
    use std::time::Duration;

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

    fn test_origin() -> Origin {
        Origin {
            transport: "test".into(),
            connection_id: None,
        }
    }

    /// Register a fake live session in the registry: a real input
    /// channel (the test controls the receiver) and an event log,
    /// optionally disk-backed under `dir`.
    fn insert_session(
        sessions: &Sessions,
        id: &str,
        input_tx: mpsc::Sender<AgentInput>,
        disk_dir: Option<&std::path::Path>,
    ) -> Arc<SessionEventLog> {
        let log = Arc::new(SessionEventLog::new(id.to_string()));
        if let Some(dir) = disk_dir {
            log.attach_disk_writer(&dir.join(format!("{id}.jsonl")))
                .expect("attach disk writer");
        }
        sessions.insert(
            id.to_string(),
            super::super::sessions::NewSession {
                input_tx,
                forwarder: tokio::spawn(async {}),
                agent: tokio::spawn(async {}),
                event_log: log.clone(),
                pending_approvals: Arc::new(Mutex::new(HashMap::new())),
                parent_session_id: None,
                capability: Arc::new(Mutex::new(Capability::root())),
                cache_ttl: CacheTtl::default(),
                provider_status: Arc::new(Mutex::new(
                    super::super::provider_status::ProviderStatusTracker::new(),
                )),
                mailbox: Arc::new(super::super::mailbox::MailboxState::new()),
                status_watch: None,
            },
        );
        log
    }

    /// Poll the session log until `pred` over its snapshot holds, or
    /// time out.
    async fn wait_for_log(
        log: &Arc<SessionEventLog>,
        pred: impl Fn(&[mu_core::event_log::SessionEvent]) -> bool,
    ) {
        for _ in 0..1000 {
            if pred(&log.snapshot()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition not reached within 10s: {:?}", log.snapshot());
    }

    fn daemon_journal_methods(path: &std::path::Path) -> Vec<String> {
        let (records, _) = CommandJournal::replay(path).expect("replay");
        records
            .iter()
            .filter_map(|r| match &r.payload {
                JournalPayload::CommandReceived { method, .. } => Some(method.clone()),
                _ => None,
            })
            .collect()
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
        control.poison_ingest_seam_for_tests();

        let req = request(
            mu_core::protocol::CreateSessionRequest::METHOD,
            json!({ "provider": { "kind": "anthropic_api", "model": "x" } }),
        );
        let auth = authed_state();
        let response = ingest(&control, req, test_origin(), &auth)
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
    /// plane — including `mailbox.post`, session-addressed via
    /// `to_session_id` (WP4) — everything else (incl. unknown
    /// methods) stays on it.
    #[test]
    fn classify_routes_session_verbs_to_session_scope() {
        assert_eq!(classify(AskSessionRequest::METHOD), Scope::Session);
        assert_eq!(classify(CloseSessionRequest::METHOD), Scope::Session);
        assert_eq!(classify(SpawnWorkerRequest::METHOD), Scope::Session);
        assert_eq!(classify(MailboxPostRequest::METHOD), Scope::Session);
        assert_eq!(classify("ping"), Scope::Daemon);
        assert_eq!(classify("create_session"), Scope::Daemon);
        assert_eq!(classify("peer.auth_initiate"), Scope::Daemon);
        assert_eq!(classify("no.such.method"), Scope::Daemon);
        // mcp.* mirrors the underlying handler's scope (WP5): the post
        // is session-addressed; everything else is control-plane like
        // its native twin.
        assert_eq!(classify("mcp.mu_mailbox_post"), Scope::Session);
        assert_eq!(classify("mcp.mu_daemon_info"), Scope::Daemon);
        assert_eq!(classify("mcp.mu_peer_hello"), Scope::Daemon);
        assert_eq!(classify("mcp.mu_mailbox_list"), Scope::Daemon);
        assert_eq!(classify("mcp.mu_mailbox_read"), Scope::Daemon);
        assert_eq!(classify("mcp.mu_mailbox_consume"), Scope::Daemon);
    }

    /// `mailbox.post` addresses its session via `to_session_id`.
    #[test]
    fn addressed_session_id_reads_both_param_shapes() {
        let ask = request(AskSessionRequest::METHOD, json!({ "session_id": "s-1" }));
        assert_eq!(addressed_session_id(&ask).as_deref(), Some("s-1"));
        let post = request(
            MailboxPostRequest::METHOD,
            json!({ "to_session_id": "s-2" }),
        );
        assert_eq!(addressed_session_id(&post).as_deref(), Some("s-2"));
        let ping = request("ping", json!(null));
        assert_eq!(addressed_session_id(&ping), None);
    }

    /// Wedged-loop, channel-FULL case (spec mu-046 WP4, INV-1): an
    /// ask to a session whose input channel is saturated is still
    /// durable in the SESSION's log before any delivery attempt — the
    /// spawned handler blocks on the send, no receipt yet, and the
    /// daemon journal carries nothing for the ask (it lives in the
    /// session pipeline).
    #[tokio::test]
    async fn wedged_full_channel_ask_is_durable_in_session_log_inv1() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal_path = dir.path().join("d.jsonl");
        let journal = Arc::new(
            CommandJournal::open(&journal_path, "d", FsyncPolicy::Never).expect("open journal"),
        );
        let ctx = test_ctx();
        let sessions = ctx.sessions.clone();
        let stream = OutboundStream::new();
        let control = spawn_control_plane(journal, ctx, stream);

        // Capacity-1 channel, pre-filled, receiver alive but never
        // draining: the wedged loop.
        let (input_tx, _input_rx) = mpsc::channel::<AgentInput>(1);
        input_tx
            .try_send(AgentInput::Cancel)
            .expect("pre-fill the channel");
        let log = insert_session(&sessions, "s-wedged", input_tx, Some(dir.path()));

        let req = request(
            AskSessionRequest::METHOD,
            json!({ "session_id": "s-wedged", "user_message": "hello?" }),
        );
        let auth = authed_state();
        assert!(
            ingest(&control, req, test_origin(), &auth).is_none(),
            "accepted asks respond via the outbound stream"
        );

        // Durable before processed: CommandReceived is in the session
        // log (memory mirrors a durable write — append_command goes
        // disk-first)...
        wait_for_log(&log, |events| {
            events
                .iter()
                .any(|e| matches!(&e.payload, EventPayload::CommandReceived { method, .. } if method == AskSessionRequest::METHOD))
        })
        .await;
        // ...and on the raw bytes too.
        let raw = std::fs::read_to_string(dir.path().join("s-wedged.jsonl")).expect("read log");
        assert!(raw.contains("command_received"), "raw: {raw}");

        // Delivery is wedged: no receipt yet (the receipt is an
        // outcome; this command has none).
        assert!(
            !log.snapshot().iter().any(|e| matches!(
                &e.payload,
                EventPayload::CommandSucceeded { .. }
                    | EventPayload::CommandFailed { .. }
                    | EventPayload::CommandRejected { .. }
            )),
            "wedged ask must have no receipt yet: {:?}",
            log.snapshot()
        );

        // The daemon journal no longer carries the session-scoped ask.
        assert!(
            daemon_journal_methods(&journal_path).is_empty(),
            "daemon journal must not carry session-scoped commands"
        );
    }

    /// Wedged-loop, channel-CLOSED case (spec mu-046 WP4): the
    /// CommandReceived is durable in the session log even though
    /// delivery fails — and the delivery failure IS an outcome, so the
    /// pipeline writes a `CommandFailed` receipt with the matching
    /// `command_event_id`.
    #[tokio::test]
    async fn closed_channel_ask_gets_command_failed_receipt_in_session_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal_path = dir.path().join("d.jsonl");
        let journal = Arc::new(
            CommandJournal::open(&journal_path, "d", FsyncPolicy::Never).expect("open journal"),
        );
        let ctx = test_ctx();
        let sessions = ctx.sessions.clone();
        let stream = OutboundStream::new();
        let control = spawn_control_plane(journal, ctx, stream);

        // Channel whose receiver is already gone: the dead loop.
        let (input_tx, input_rx) = mpsc::channel::<AgentInput>(1);
        drop(input_rx);
        let log = insert_session(&sessions, "s-dead", input_tx, Some(dir.path()));

        let req = request(
            AskSessionRequest::METHOD,
            json!({ "session_id": "s-dead", "user_message": "anyone home?" }),
        );
        let auth = authed_state();
        assert!(ingest(&control, req, test_origin(), &auth).is_none());

        wait_for_log(&log, |events| {
            events
                .iter()
                .any(|e| matches!(&e.payload, EventPayload::CommandFailed { .. }))
        })
        .await;
        let events = log.snapshot();
        let received_id = events
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::CommandReceived { method, .. }
                    if method == AskSessionRequest::METHOD =>
                {
                    Some(e.id)
                }
                _ => None,
            })
            .expect("CommandReceived in session log");
        let (failed_ref, echo_method) = events
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::CommandFailed {
                    command_event_id,
                    command,
                    ..
                } => Some((*command_event_id, command.method.clone())),
                _ => None,
            })
            .expect("CommandFailed receipt in session log");
        assert_eq!(failed_ref, received_id, "receipt pairs with its command");
        assert_eq!(echo_method, AskSessionRequest::METHOD);
        assert!(
            daemon_journal_methods(&journal_path).is_empty(),
            "daemon journal must not carry session-scoped commands"
        );
    }

    /// In-memory-only session logs cannot make a command durable
    /// (append_command is Unsupported by design), so the pipeline
    /// EXPLICITLY falls back to the daemon journal — border compliance
    /// preserved — and the command keeps the WP3 receipt shape there.
    #[tokio::test]
    async fn in_memory_session_log_falls_back_to_daemon_journal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal_path = dir.path().join("d.jsonl");
        let journal = Arc::new(
            CommandJournal::open(&journal_path, "d", FsyncPolicy::Never).expect("open journal"),
        );
        let ctx = test_ctx();
        let sessions = ctx.sessions.clone();
        let stream = OutboundStream::new();
        let control = spawn_control_plane(journal, ctx, stream);

        // Live session, but its log has NO disk writer
        // (persist_events_to_disk=false shape).
        let (input_tx, mut input_rx) = mpsc::channel::<AgentInput>(4);
        let log = insert_session(&sessions, "s-mem", input_tx, None);
        assert!(!log.has_disk_writer());

        let req = request(
            AskSessionRequest::METHOD,
            json!({ "session_id": "s-mem", "user_message": "hi" }),
        );
        let auth = authed_state();
        assert!(ingest(&control, req, test_origin(), &auth).is_none());

        // The handler delivers the input (no ticket — daemon slot)...
        let delivered = tokio::time::timeout(Duration::from_secs(2), input_rx.recv())
            .await
            .expect("input delivered")
            .expect("channel open");
        match delivered {
            AgentInput::UserMessage(_, ticket) => {
                assert!(ticket.is_none(), "daemon-slot asks carry no ticket");
            }
            other => panic!("expected UserMessage, got {other:?}"),
        }
        // ...and the border record + WP3-shaped receipt live in the
        // DAEMON journal; the in-memory session log saw no command
        // rows.
        for _ in 0..1000 {
            let (records, _) = CommandJournal::replay(&journal_path).expect("replay");
            if records.iter().any(|r| {
                matches!(&r.payload, JournalPayload::CommandSucceeded { command, .. }
                    if command.method == AskSessionRequest::METHOD)
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            daemon_journal_methods(&journal_path),
            vec![AskSessionRequest::METHOD.to_string()],
            "fallback border record lands in the daemon journal"
        );
        assert!(
            log.snapshot()
                .iter()
                .all(|e| !matches!(&e.payload, EventPayload::CommandReceived { .. })),
            "in-memory session log must not receive command rows"
        );
    }

    /// Unresolvable session (no such id): the border record always
    /// exists — CommandReceived + the "session not found" rejection
    /// both land in the DAEMON journal (documented fallback rule).
    #[tokio::test]
    async fn unresolvable_session_falls_back_to_daemon_journal_with_rejection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal_path = dir.path().join("d.jsonl");
        let journal = Arc::new(
            CommandJournal::open(&journal_path, "d", FsyncPolicy::Never).expect("open journal"),
        );
        let ctx = test_ctx();
        let stream = OutboundStream::new();
        let control = spawn_control_plane(journal, ctx, stream);

        let req = request(
            AskSessionRequest::METHOD,
            json!({ "session_id": "never-existed", "user_message": "hi" }),
        );
        let auth = authed_state();
        assert!(ingest(&control, req, test_origin(), &auth).is_none());

        // "session not found" is INVALID_PARAMS → Rejected{Validation}.
        for _ in 0..1000 {
            let (records, _) = CommandJournal::replay(&journal_path).expect("replay");
            if records
                .iter()
                .any(|r| matches!(&r.payload, JournalPayload::CommandRejected { .. }))
            {
                let received = records.iter().any(|r| {
                    matches!(&r.payload, JournalPayload::CommandReceived { method, session_id, .. }
                        if method == AskSessionRequest::METHOD
                            && session_id.as_deref() == Some("never-existed"))
                });
                assert!(received, "border record present: {records:?}");
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no rejection receipt landed in the daemon journal");
    }

    /// A session-slot ask that the handler ACCEPTS defers its receipt
    /// to the turn's Done: the ticket rides into the input queue with
    /// the matching command_event_id, and the pipeline writes NO
    /// receipt at handler completion.
    #[tokio::test]
    async fn accepted_session_ask_defers_receipt_and_threads_ticket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal_path = dir.path().join("d.jsonl");
        let journal = Arc::new(
            CommandJournal::open(&journal_path, "d", FsyncPolicy::Never).expect("open journal"),
        );
        let ctx = test_ctx();
        let sessions = ctx.sessions.clone();
        let stream = OutboundStream::new();
        let control = spawn_control_plane(journal, ctx, stream);

        let (input_tx, mut input_rx) = mpsc::channel::<AgentInput>(4);
        let log = insert_session(&sessions, "s-live", input_tx, Some(dir.path()));

        let req = request(
            AskSessionRequest::METHOD,
            json!({ "session_id": "s-live", "user_message": "do a thing" }),
        );
        let auth = authed_state();
        assert!(ingest(&control, req, test_origin(), &auth).is_none());

        let delivered = tokio::time::timeout(Duration::from_secs(2), input_rx.recv())
            .await
            .expect("input delivered")
            .expect("channel open");
        let ticket = match delivered {
            AgentInput::UserMessage(_, Some(t)) => t,
            other => panic!("expected ticketed UserMessage, got {other:?}"),
        };
        let received_id = log
            .snapshot()
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::CommandReceived { .. } => Some(e.id),
                _ => None,
            })
            .expect("CommandReceived in session log");
        assert_eq!(
            ticket.command_event_id, received_id,
            "ticket pairs with the journaled CommandReceived"
        );
        assert_eq!(ticket.echo.method, AskSessionRequest::METHOD);
        // Give the spawned handler a beat to finish, then confirm no
        // premature receipt: the ask's receipt belongs to its Done.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !log.snapshot().iter().any(|e| matches!(
                &e.payload,
                EventPayload::CommandSucceeded { .. }
                    | EventPayload::CommandFailed { .. }
                    | EventPayload::CommandRejected { .. }
            )),
            "accepted ask must not be receipted at handler completion: {:?}",
            log.snapshot()
        );
    }
}
