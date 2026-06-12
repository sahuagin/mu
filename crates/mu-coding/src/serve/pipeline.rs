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
//!    commands are handed — still in consumer/seq order — to their
//!    session's FIFO dispatcher (next section) so a slow session
//!    cannot stall the control plane and commands to the SAME session
//!    cannot reorder. Concurrency exists only across pipelines, never
//!    within one.
//! 3. On completion a receipt wrapping the original command (INV-5,
//!    [`CommandEcho`]) is journaled into the command's slot —
//!    `CommandSucceeded` / `CommandFailed` / `CommandRejected` — and
//!    the response leaves through the tagged outbound stream (INV-8).
//!    A receipt-append failure is logged and the response still goes
//!    out: the command is already durable, and the orphaned
//!    `CommandReceived` IS the legible marker (INV-4).
//!
//! ## Per-session FIFO dispatch (WP8 — the session pipeline's intake)
//!
//! The consumer routes session-scoped commands (post-journal, in seq
//! order) into a per-session dispatcher: one task per addressed
//! session id, processing that session's commands strictly in arrival
//! order — execute handler → receipt → emit, exactly one command at a
//! time. This is what makes INV-3 true for session pipelines: journal
//! order == queue order == processing order WITHIN a session (a
//! pipelined `ask_session` then `cancel_session` reach the session's
//! input channel in journal order); concurrency exists only ACROSS
//! sessions and the control plane. A dispatcher wedged on a full
//! input channel wedges only its own session's lane — never the
//! control plane, never another session.
//!
//! Lifecycle: a dispatcher is created lazily on the first command
//! addressed to a session id, and torn down after processing a
//! command once the session is no longer live (closed — or never
//! existed: the unresolvable fallback) AND its queue is empty.
//! Emptiness is checked under the same lock the consumer enqueues
//! under, so teardown cannot race an incoming command. If a
//! dispatcher task dies without removing its entry (a handler
//! panic), the next command for that session sweeps the dead entry
//! and spawns a fresh dispatcher; commands queued in the dead lane
//! are lost to processing and stay journaled-but-receiptless — the
//! INV-4 orphan marker, the same blast shape a panicked pre-WP8
//! spawn had. Ordering is keyed by the ADDRESSED session id, not the
//! journal slot: session-scoped commands that fell back to the
//! daemon journal (below) still ride their session's dispatcher.
//! Only a session-scoped command carrying no session id at all is
//! spawned free (defensive — validation rejects those; there is no
//! lane to order it against).
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

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

use serde_json::{json, Value};
use tokio::sync::mpsc;

use mu_core::agent::Tool;
use mu_core::command_journal::{
    AuthSnapshot, CommandEcho, CommandJournal, CommandTicket, JournalPayload, Origin, RejectStage,
};
use mu_core::event_log::{EventActor, EventPayload, SessionEventLog};
use mu_core::protocol::{
    AskSessionRequest, AuthInitiateRequest, CancelOutstandingRequest, CancelSessionRequest,
    CapabilitiesDiscoverRequest, CloseSessionRequest, DaemonListRoutesRequest,
    DaemonOutstandingCallsRequest, DaemonStatsRequest, DaemonUsageHistoryRequest,
    MailboxListRequest, MailboxPostRequest, MailboxReadRequest, PingRequest, Request,
    RespondToInputRequiredRequest, Response, ScheduleWakeupRequest, SessionEventsRequest,
    SessionListRequest, SessionStatsRequest, SetRouteRequest, SpawnWorkerRequest,
    StartAutonomousRequest,
};
use mu_core::skill::loader::LoadedSkill;
use mu_core::transport::{
    codes, err_response, NotificationWriter, Outbound, OutboundEnvelope, Router,
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
    /// (WP4; daemon-journal fallback per the module doc) and rides
    /// the session's FIFO dispatcher (WP8, module doc) so a session's
    /// input channel can never block the control plane — and commands
    /// to the same session can never reorder.
    Session,
}

/// How a method's addressed session id is encoded in params.
///
/// Today the ingest seam preserves the historical broad behavior:
/// either `session_id` or `to_session_id` is accepted for every
/// method, including unknown/daemon-scoped methods. The metadata
/// table still owns the policy so the special case is explicit and
/// future methods can narrow it deliberately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionIdParam {
    EitherSessionOrToSession,
}

impl SessionIdParam {
    fn extract(self, params: &Value) -> Option<String> {
        match self {
            SessionIdParam::EitherSessionOrToSession => params
                .get("session_id")
                .or_else(|| params.get("to_session_id"))
                .and_then(|v| v.as_str())
                .map(str::to_string),
        }
    }
}

/// Ingest-time metadata for one JSON-RPC method.
///
/// Keep routing scope, read/query behavior, session-address extraction,
/// and journal redaction in ONE place. PR #275 introduced these as
/// parallel truth tables; centralizing them prevents future method
/// additions from updating dispatch while forgetting journaling,
/// redaction, or query-elision policy.
#[derive(Debug, Clone, Copy)]
struct MethodMeta {
    scope: Scope,
    query: bool,
    session_id_param: SessionIdParam,
    secret_fields: &'static [&'static str],
}

impl MethodMeta {
    const fn daemon() -> Self {
        Self {
            scope: Scope::Daemon,
            query: false,
            session_id_param: SessionIdParam::EitherSessionOrToSession,
            secret_fields: &[],
        }
    }

    const fn daemon_query() -> Self {
        Self {
            scope: Scope::Daemon,
            query: true,
            ..Self::daemon()
        }
    }

    const fn daemon_secret(secret_fields: &'static [&'static str]) -> Self {
        Self {
            secret_fields,
            ..Self::daemon()
        }
    }

    const fn session() -> Self {
        Self {
            scope: Scope::Session,
            ..Self::daemon()
        }
    }

    const fn session_query() -> Self {
        Self {
            scope: Scope::Session,
            query: true,
            ..Self::session()
        }
    }

    fn addressed_session_id(self, params: &Value) -> Option<String> {
        self.session_id_param.extract(params)
    }
}

const AUTH_INITIATE_SECRET_FIELDS: &[&str] = &["initial_response"];

/// Route/query/redaction metadata by method.
///
/// Session-scoped methods are the session-addressed verbs — including
/// `mailbox.post`, which is addressed to a target session
/// (`to_session_id`) — everything else, including unknown methods
/// (which the router rejects with `METHOD_NOT_FOUND` →
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
///
/// Query methods are a closed allowlist (spec mu-046 WP6, the
/// `[journal].journal_queries` knob): daemon- and session-scoped READS
/// whose processing mutates nothing. When `journal_queries = false`
/// these skip the journal — no `CommandReceived`, no receipt — but
/// still cross the same ingest seam, auth gate, and consumer.
/// Deliberately not a complement: an unlisted (or future) method fails
/// SAFE by journaling. `mailbox.consume` is NOT a query — consuming
/// marks messages consumed. The `mcp.*` twins of these reads are also
/// absent (conservative: the foreign surface keeps its full paper
/// trail; revisit if an ephemeral daemon ever fronts MCP).
fn method_meta(method: &str) -> MethodMeta {
    match method {
        // Daemon/control-plane read queries.
        m if m == PingRequest::METHOD
            || m == SessionListRequest::METHOD
            || m == DaemonStatsRequest::METHOD
            || m == DaemonUsageHistoryRequest::METHOD
            || m == DaemonOutstandingCallsRequest::METHOD
            || m == DaemonListRoutesRequest::METHOD
            || m == CapabilitiesDiscoverRequest::METHOD
            || m == MailboxListRequest::METHOD
            || m == MailboxReadRequest::METHOD =>
        {
            MethodMeta::daemon_query()
        }

        // Session read queries.
        m if m == SessionEventsRequest::METHOD || m == SessionStatsRequest::METHOD => {
            MethodMeta::session_query()
        }

        // Session-addressed mutating commands.
        m if m == AskSessionRequest::METHOD
            || m == CancelSessionRequest::METHOD
            || m == CancelOutstandingRequest::METHOD
            || m == CloseSessionRequest::METHOD
            || m == StartAutonomousRequest::METHOD
            || m == ScheduleWakeupRequest::METHOD
            || m == RespondToInputRequiredRequest::METHOD
            || m == SetRouteRequest::METHOD
            || m == SpawnWorkerRequest::METHOD
            || m == MailboxPostRequest::METHOD
            || m == dispatch::MCP_MAILBOX_POST_METHOD
            || m == TEST_PANIC_METHOD =>
        {
            MethodMeta::session()
        }

        // Secret-bearing daemon commands.
        m if m == AuthInitiateRequest::METHOD => {
            MethodMeta::daemon_secret(AUTH_INITIATE_SECRET_FIELDS)
        }

        // Unknown/future methods fail safe: daemon-scoped, journaled,
        // no redaction unless explicitly registered above.
        _ => MethodMeta::daemon(),
    }
}

fn classify(method: &str) -> Scope {
    method_meta(method).scope
}

/// Whether `method` is a recognized read-only query (see
/// [`method_meta`]).
fn is_query(method: &str) -> bool {
    method_meta(method).query
}

/// The session a command addresses, by param. Current policy accepts
/// either `session_id` or `to_session_id` for every method to preserve
/// the pre-metadata ingest behavior; known session-scoped methods are
/// still what decide routing scope.
fn addressed_session_id(request: &Request<Value>) -> Option<String> {
    method_meta(&request.method).addressed_session_id(&request.params)
}

/// Clone `params` with secret-bearing fields replaced by
/// `"[REDACTED]"`. Handlers still receive the original request — only
/// the journal sees the redacted copy.
fn redact_params(method: &str, params: &Value) -> Value {
    let mut params = params.clone();
    let fields = method_meta(method).secret_fields;
    if !fields.is_empty() {
        if let Some(obj) = params.as_object_mut() {
            for field in fields {
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
    /// Not journaled at all (spec mu-046 WP6): a recognized read-only
    /// query on a daemon with `[journal].journal_queries = false`. No
    /// `CommandReceived`, no receipt — [`append_receipt`] is a no-op
    /// for this slot — and no command id for outbound correlation.
    /// The command still crossed the same seam, gate, and consumer.
    Unjournaled,
}

impl JournalSlot {
    /// The command id within its pipeline, for outbound correlation.
    /// `None` for unjournaled queries — there is no journal record to
    /// correlate to.
    fn command_id(&self) -> Option<u64> {
        match self {
            JournalSlot::Daemon { seq } => Some(*seq),
            JournalSlot::Session { event_id, .. } => Some(*event_id),
            JournalSlot::Unjournaled => None,
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
    /// The session this command addresses
    /// ([`addressed_session_id`]) — the per-session dispatcher's
    /// routing key (WP8). Kept on the command (independent of the
    /// journal slot) so fallback-journaled commands still order
    /// against their session.
    session_id: Option<String>,
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

/// What rides the control-plane queue. Adapters produce
/// [`Command`]s; the boot sequence produces exactly one
/// [`ConfigLoaded`](PipelineInput::ConfigLoaded) (spec mu-046 INV-9)
/// before any adapter exists. Both enter through the same seam lock,
/// so journal seq order == queue order == processing order (INV-3)
/// holds across message kinds.
enum PipelineInput {
    /// Boxed: a `Command` is ~240 bytes (request + params + echo
    /// snapshot) vs. the 8-byte `ConfigLoaded` (clippy
    /// large_enum_variant); commands are heap-allocated once at ingest.
    Command(Box<Command>),
    /// The resolved startup config entered the pipeline as a message.
    /// Already journaled (under the seam lock) at `seq`; the consumer
    /// "applies" it — a no-op today, see [`process_config_loaded`].
    ConfigLoaded { seq: u64 },
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
    /// `[journal].journal_queries` (spec mu-046 WP6): `false` lets
    /// recognized read-only queries ([`is_query`]) skip the journal.
    journal_queries: bool,
    /// Journal-append + enqueue happen under this lock so journal seq
    /// order == queue order (INV-3) no matter how many adapters
    /// produce concurrently.
    seam: Mutex<IngestSeam>,
}

struct IngestSeam {
    journal: Arc<CommandJournal>,
    tx: mpsc::UnboundedSender<PipelineInput>,
}

impl ControlPlane {
    /// spec mu-046 INV-9 (WP6): inject the resolved (already redacted
    /// — the caller runs [`mu_core::config::redact_config`], INV-6)
    /// effective config as a journaled, sequenced control-plane
    /// message: `ConfigLoaded { sources, config }` is appended and
    /// enqueued under the SAME seam lock every adapter command crosses,
    /// so it gets a seq and is processed by the single-writer consumer
    /// like everything else — it is a message, not a side write.
    ///
    /// This is a narrow internal path rather than a [`ingest`] call
    /// because `ingest` is request-shaped (JSON-RPC request + origin +
    /// connection auth state) and `ConfigLoaded` is not a request —
    /// there is no client, no request id, no response. It exists only
    /// for the boot sequence today; a future `config.set` /
    /// `ConfigAmended` (spec mu-046 "deferred") rides this exact seam:
    /// journal the config message, sequence it, apply it in the
    /// consumer.
    ///
    /// Errors propagate (journal append failure, consumer gone):
    /// boot-time fail-closed — a daemon that cannot make its config
    /// message durable does not serve (same posture as journal open
    /// failure, INV-2).
    pub(crate) fn inject_config_loaded(
        &self,
        sources: Vec<String>,
        redacted_config: Value,
    ) -> std::io::Result<u64> {
        let seam = self
            .seam
            .lock()
            .map_err(|_| std::io::Error::other("ingest seam poisoned"))?;
        let seq = seam.journal.append(JournalPayload::ConfigLoaded {
            sources,
            config: redacted_config,
        })?;
        seam.tx
            .send(PipelineInput::ConfigLoaded { seq })
            .map_err(|_| std::io::Error::other("control plane consumer unavailable"))?;
        Ok(seq)
    }
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
    stream: Router,
) -> ControlPlane {
    let (tx, mut rx) = mpsc::unbounded_channel::<PipelineInput>();
    let sessions = ctx.sessions.clone();
    let journal_queries = ctx.daemon_info.config().journal.journal_queries;
    let consumer_journal = journal.clone();
    tokio::spawn(async move {
        // Per-session FIFO dispatchers (WP8, module doc). The consumer
        // holds the only strong Arc: when it exits, the map drops, the
        // dispatcher channels close, and each dispatcher drains its
        // queue and exits — the same shutdown cascade order as the
        // consumer itself.
        let dispatchers = Arc::new(SessionDispatchers::default());
        while let Some(input) = rx.recv().await {
            match input {
                PipelineInput::Command(cmd) => {
                    process_command(cmd, &ctx, &consumer_journal, &stream, &dispatchers).await;
                }
                PipelineInput::ConfigLoaded { seq } => process_config_loaded(seq),
            }
        }
    });
    ControlPlane {
        sessions,
        journal_queries,
        seam: Mutex::new(IngestSeam { journal, tx }),
    }
}

/// Apply a sequenced `ConfigLoaded` message (spec mu-046 INV-9). A
/// no-op today by design: the startup config was already constructed
/// and threaded into every component before the pipeline existed, so
/// there is nothing to mutate — the point is the sequenced durable
/// record, processed in order BEFORE any adapter command. When
/// runtime-mutable config lands (`config.set` → `ConfigAmended`,
/// deferred by the spec), THIS is where the new config value takes
/// effect, with the journal seq as the total order over config
/// changes vs. commands.
fn process_config_loaded(seq: u64) {
    tracing::debug!(seq, "control plane: ConfigLoaded applied (startup no-op)");
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
    // WP6: with `[journal].journal_queries = false`, recognized
    // read-only queries skip the journal (and receipts) entirely —
    // they still cross the seam lock and the consumer below, so the
    // border stays single and ordered; it just stops writing for
    // reads. Mutating commands always journal.
    let unjournaled_query = !control.journal_queries && is_query(&request.method);

    // WP4 routing: a session-scoped command journals into the
    // addressed session's own log IFF that session is in memory AND
    // its log can take the strict append (disk writer attached).
    // Resolved BEFORE the seam lock — the registry has its own locks
    // and nesting them under the seam invites ordering hazards.
    let session_log: Option<Arc<SessionEventLog>> = match scope {
        Scope::Session if !unjournaled_query => session_id
            .as_deref()
            .and_then(|id| control.sessions.event_log_in_memory(id))
            .filter(|log| log.has_disk_writer()),
        _ => None,
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
    let slot = if unjournaled_query {
        JournalSlot::Unjournaled
    } else {
        match session_log {
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
                    session_id: session_id.clone(),
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
        }
    };
    let command = Command {
        slot,
        request,
        origin,
        session_id,
        redacted_params,
        scope,
        received_at_unix_ms,
        auth_state: auth_state.clone(),
    };
    if let Err(send_err) = seam.tx.send(PipelineInput::Command(Box::new(command))) {
        // Consumer gone — daemon shutting down. The command is durable
        // (journaled, no receipt: a legible orphan) but won't run.
        let request_id = match send_err.0 {
            PipelineInput::Command(cmd) => cmd.request.id,
            PipelineInput::ConfigLoaded { .. } => Value::Null,
        };
        return Some(err_response(
            request_id,
            codes::INTERNAL_ERROR,
            "control plane unavailable (daemon shutting down)",
        ));
    }
    None
}

/// One session-scoped command, prepared by the consumer (gate already
/// passed, per-command context built) and queued on its session's
/// dispatcher — the arguments of one [`execute_and_receipt`] call.
struct SessionWork {
    cmd: Box<Command>,
    notif: NotificationWriter,
    dctx: DispatchCtx,
}

/// Per-session FIFO intake (spec mu-046 WP8, module doc): one
/// dispatcher task per addressed session id, each processing its
/// session's commands strictly in arrival order. The consumer enqueues
/// under the map lock; teardown checks queue emptiness under the same
/// lock — so a command can never land in a lane that has decided to
/// exit.
#[derive(Default)]
struct SessionDispatchers {
    map: Mutex<HashMap<String, mpsc::UnboundedSender<SessionWork>>>,
}

impl SessionDispatchers {
    /// Lock the map, recovering from poisoning: the guarded sections
    /// are straight-line map operations (no panics mid-mutation can
    /// leave the map incoherent), and wedging EVERY session over one
    /// poisoned lane would invert the isolation this type exists for.
    fn lock_map(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, mpsc::UnboundedSender<SessionWork>>> {
        match self.map.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Route one session-scoped command — already journaled, arriving
    /// in consumer/seq order — onto its session's dispatcher, created
    /// lazily on first use. A dead entry (the dispatcher task panicked
    /// without removing itself) is swept here: replace it and re-send
    /// this command; whatever was queued in the dead lane stays
    /// journaled-but-receiptless (the INV-4 orphan marker).
    fn dispatch(
        self: &Arc<Self>,
        session_id: String,
        work: SessionWork,
        sessions: &Sessions,
        journal: &Arc<CommandJournal>,
        stream: &Router,
    ) {
        let mut map = self.lock_map();
        let tx = map.entry(session_id.clone()).or_insert_with(|| {
            spawn_session_dispatcher(
                session_id.clone(),
                Arc::downgrade(self),
                sessions.clone(),
                journal.clone(),
                stream.clone(),
            )
        });
        if let Err(dead) = tx.send(work) {
            tracing::warn!(
                session_id = %session_id,
                "session dispatcher died (handler panic?); sweeping and respawning"
            );
            let fresh = spawn_session_dispatcher(
                session_id.clone(),
                Arc::downgrade(self),
                sessions.clone(),
                journal.clone(),
                stream.clone(),
            );
            let _ = fresh.send(dead.0);
            map.insert(session_id, fresh);
        }
    }
}

/// Spawn one session's dispatcher: drain the lane strictly in order —
/// each command fully executes (handler → receipt → emit) before the
/// next starts. A send into a full session input channel therefore
/// blocks ONLY this lane (the wedged-loop posture, WP8).
///
/// Teardown (module doc): after each command, if the session is no
/// longer live in the registry — closed, or it never was (the
/// unresolvable/daemon-fallback cases) — and the queue is empty under
/// the map lock, remove our entry and exit. `registry` is Weak so a
/// lingering dispatcher cannot keep the map (and thus itself) alive
/// past the consumer.
fn spawn_session_dispatcher(
    session_id: String,
    registry: Weak<SessionDispatchers>,
    sessions: Sessions,
    journal: Arc<CommandJournal>,
    stream: Router,
) -> mpsc::UnboundedSender<SessionWork> {
    let (tx, mut rx) = mpsc::unbounded_channel::<SessionWork>();
    tokio::spawn(async move {
        while let Some(SessionWork { cmd, notif, dctx }) = rx.recv().await {
            execute_and_receipt(*cmd, notif, dctx, journal.clone(), stream.clone()).await;
            if sessions.input_sender(&session_id).is_some() {
                continue; // live session: the lane persists.
            }
            let Some(registry) = registry.upgrade() else {
                // Consumer gone (shutdown): no map to clean; keep
                // draining until the channel closes.
                continue;
            };
            let map = &mut *registry.lock_map();
            // Emptiness under the SAME lock the consumer sends under:
            // nothing can be enqueued between this check and the
            // removal, and a fresh dispatcher (spawned by the next
            // command, if any) only starts after we are gone.
            if rx.is_empty() {
                map.remove(&session_id);
                return;
            }
        }
    });
    tx
}

/// One consumer tick: gate, route, receipt, respond.
async fn process_command(
    cmd: Box<Command>,
    ctx: &PipelineCtx,
    journal: &Arc<CommandJournal>,
    stream: &Router,
    dispatchers: &Arc<SessionDispatchers>,
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
            execute_and_receipt(*cmd, notif, dctx, journal.clone(), stream.clone()).await
        }
        // Session-scoped: onto the session's FIFO dispatcher (WP8,
        // module doc) — the control plane never blocks on a session's
        // input channel, and commands to the same session process in
        // journal order. Concurrency exists only across pipelines.
        Scope::Session => match cmd.session_id.clone() {
            Some(session_id) => dispatchers.dispatch(
                session_id,
                SessionWork { cmd, notif, dctx },
                &ctx.sessions,
                journal,
                stream,
            ),
            // Defensive: a session-scoped method carrying no session
            // id at all (validation rejects it). There is no lane to
            // order it against, so the pre-WP8 spawn keeps the control
            // plane unblocked.
            None => {
                tokio::spawn(execute_and_receipt(
                    *cmd,
                    notif,
                    dctx,
                    journal.clone(),
                    stream.clone(),
                ));
            }
        },
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
    stream: Router,
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
        _ => {
            let body = receipt_body_for(&echo.method, &response, elapsed_ms);
            append_receipt(&journal, &slot, echo, body);
        }
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
/// processing failures (`CommandFailed`). Success results for
/// query-class methods are elided — see [`receipt_result`].
fn receipt_body_for(method: &str, response: &Response<Value>, elapsed_ms: u64) -> ReceiptBody {
    match response {
        Response::Ok { result, .. } => ReceiptBody::Succeeded {
            result: receipt_result(method, result),
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

/// The `result` a success receipt embeds (spec mu-046 WP8).
/// Query-class methods ([`is_query`]) get a compact elision marker —
/// `{"elided": true, "result_bytes": N}` — instead of the full
/// result: a `session.events` receipt embedding the very events it
/// just read back into the same log would compound log growth under
/// polling. The `CommandEcho` still wraps the full command; only the
/// RESULT is elided. Non-query receipts carry their real result, and
/// the wire response is untouched either way.
fn receipt_result(method: &str, result: &Value) -> Value {
    if is_query(method) {
        let result_bytes = serde_json::to_string(result).map(|s| s.len()).unwrap_or(0);
        json!({ "elided": true, "result_bytes": result_bytes })
    } else {
        result.clone()
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
        // WP6: unjournaled query — no CommandReceived was written, so
        // a receipt would dangle. Deliberate no-op.
        JournalSlot::Unjournaled => {}
    }
}

/// One way out (INV-8): envelope the response with the originating
/// connection + journal correlation and send it to the outbound
/// stream; the connection's write loop delivers it. `command_seq` is
/// `None` for unjournaled queries (WP6) — there is no journal record
/// to correlate to.
fn emit_response(
    stream: &Router,
    origin: &Origin,
    command_seq: Option<u64>,
    response: Response<Value>,
) {
    let request_id = match &response {
        Response::Ok { id, .. } | Response::Err { id, .. } => id.clone(),
    };
    match serde_json::to_value(response) {
        Ok(value) => stream.send(OutboundEnvelope {
            origin: Some(origin.clone()),
            request_id: Some(request_id),
            command_seq,
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
        let stream = Router::new();
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
    /// session's dispatcher blocks on the send (WP8: wedging only that
    /// session's lane), no receipt yet, and the daemon journal carries
    /// nothing for the ask (it lives in the session pipeline).
    #[tokio::test]
    async fn wedged_full_channel_ask_is_durable_in_session_log_inv1() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal_path = dir.path().join("d.jsonl");
        let journal = Arc::new(
            CommandJournal::open(&journal_path, "d", FsyncPolicy::Never).expect("open journal"),
        );
        let ctx = test_ctx();
        let sessions = ctx.sessions.clone();
        let stream = Router::new();
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
        let stream = Router::new();
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
        let stream = Router::new();
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
        let stream = Router::new();
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
        let stream = Router::new();
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

    // ─── spec mu-046 WP6 ────────────────────────────────────────────

    /// The query allowlist (WP6): the daemon- and session-scoped reads
    /// are queries; everything mutating — explicitly including
    /// `mailbox.consume`, which marks messages consumed — is not.
    #[test]
    fn is_query_recognizes_reads_and_excludes_mutations() {
        for m in [
            "ping",
            "session.list",
            "session.events",
            "session.stats",
            "daemon.stats",
            "daemon.usage_history",
            "daemon.outstanding_calls",
            "daemon.list_routes",
            "capabilities/discover",
            "mailbox.list",
            "mailbox.read",
        ] {
            assert!(is_query(m), "{m} must be a query");
        }
        for m in [
            "mailbox.consume",
            "mailbox.post",
            "create_session",
            "ask_session",
            "close_session",
            "peer.auth_initiate",
            "session.set_route",
            "mcp.mu_mailbox_list", // mcp.* twins stay journaled (doc'd)
            "no.such.method",      // unknown methods fail safe: journaled
        ] {
            assert!(!is_query(m), "{m} must NOT be a query");
        }
    }

    /// INV-9 (WP6): `inject_config_loaded` goes through the same seam
    /// as adapter commands — the ConfigLoaded record gets the next seq
    /// after open()'s JournalOpened, and a command ingested AFTER the
    /// injection gets a strictly greater seq.
    #[tokio::test]
    async fn inject_config_loaded_is_sequenced_before_later_commands() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal_path = dir.path().join("d.jsonl");
        let journal = Arc::new(
            CommandJournal::open(&journal_path, "d", FsyncPolicy::Never).expect("open journal"),
        );
        let stream = Router::new();
        let control = spawn_control_plane(journal, test_ctx(), stream);

        let config_seq = control
            .inject_config_loaded(
                vec!["defaults".to_string(), "cli:--bare".to_string()],
                json!({ "recall": { "bare": true } }),
            )
            .expect("inject ConfigLoaded");
        assert_eq!(config_seq, 2, "record 1 is JournalOpened");

        let auth = authed_state();
        assert!(ingest(&control, request("ping", json!(null)), test_origin(), &auth).is_none());

        let (records, _) = CommandJournal::replay(&journal_path).expect("replay");
        assert!(matches!(
            records[0].payload,
            JournalPayload::JournalOpened { .. }
        ));
        match &records[1].payload {
            JournalPayload::ConfigLoaded { sources, config } => {
                assert_eq!(records[1].seq, config_seq);
                assert_eq!(sources[0], "defaults");
                assert_eq!(sources[1], "cli:--bare");
                assert_eq!(config["recall"]["bare"], true);
            }
            other => panic!("record 2 must be ConfigLoaded, got {other:?}"),
        }
        let ping_seq = received_seq(
            &CommandJournal::replay(&journal_path).expect("replay").0,
            "ping",
        )[0];
        assert!(
            ping_seq > config_seq,
            "adapter commands sequence AFTER ConfigLoaded ({ping_seq} > {config_seq})"
        );
    }

    fn received_seq(records: &[mu_core::command_journal::JournalRecord], m: &str) -> Vec<u64> {
        records
            .iter()
            .filter_map(|r| match &r.payload {
                JournalPayload::CommandReceived { method, .. } if method == m => Some(r.seq),
                _ => None,
            })
            .collect()
    }

    /// `[journal].journal_queries = false` (WP6): a recognized query
    /// crosses the seam and the consumer — it still gets its response
    /// via the outbound stream — but leaves NO journal record and no
    /// receipt. A mutating command on the same daemon still journals.
    #[tokio::test]
    async fn journal_queries_false_skips_query_journaling_but_responds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal_path = dir.path().join("d.jsonl");
        let journal = Arc::new(
            CommandJournal::open(&journal_path, "d", FsyncPolicy::Never).expect("open journal"),
        );
        let mut config = Config::default();
        config.journal.journal_queries = false;
        let mut ctx = test_ctx();
        ctx.daemon_info = ctx.daemon_info.clone().with_config(config);
        let stream = Router::new();
        let outbound_lane = stream.register(test_origin());
        let control = spawn_control_plane(journal, ctx, stream);

        let auth = authed_state();
        assert!(
            ingest(&control, request("ping", json!(null)), test_origin(), &auth).is_none(),
            "queries still cross the pipeline; response rides outbound"
        );
        let envelope = tokio::time::timeout(Duration::from_secs(2), outbound_lane.recv())
            .await
            .expect("ping response within 2s")
            .expect("lane open");
        assert_eq!(
            envelope.command_seq, None,
            "unjournaled query has no journal correlation"
        );
        assert_eq!(envelope.item.0["result"]["pong"], true);

        // Nothing past JournalOpened: no CommandReceived, no receipt.
        let (records, _) = CommandJournal::replay(&journal_path).expect("replay");
        assert_eq!(
            records.len(),
            1,
            "query left journal records behind: {records:?}"
        );

        // A mutating command (mailbox.consume — the documented
        // NOT-a-query) still journals its border record.
        let req = request(
            mu_core::protocol::MailboxConsumeRequest::METHOD,
            json!({ "session_id": "nope", "seqs": [1] }),
        );
        assert!(ingest(&control, req, test_origin(), &auth).is_none());
        for _ in 0..200 {
            let (records, _) = CommandJournal::replay(&journal_path).expect("replay");
            if !received_seq(&records, mu_core::protocol::MailboxConsumeRequest::METHOD).is_empty()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("mutating command must still journal with journal_queries=false");
    }

    // ─── spec mu-046 WP8 ────────────────────────────────────────────

    /// INV-3 within a session pipeline (the WP8 blocker's regression
    /// test): pipelined `ask_session` + `cancel_session` to ONE
    /// session reach that session's input channel strictly in journal
    /// order. The asks carry a ~1 MiB message, so each ask's handler
    /// does megabytes of params clone+parse before its send while the
    /// cancel's handler parses a few bytes — under the pre-WP8
    /// spawn-per-command shape (independent tasks racing to the input
    /// channel) the cancel reliably overtakes its ask on a
    /// multi-thread runtime; the per-session FIFO dispatcher makes the
    /// order deterministic regardless of handler latency.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn session_commands_reach_input_channel_in_journal_order_inv3() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Arc::new(
            CommandJournal::open(&dir.path().join("d.jsonl"), "d", FsyncPolicy::Never)
                .expect("open journal"),
        );
        let ctx = test_ctx();
        let sessions = ctx.sessions.clone();
        let stream = Router::new();
        let control = spawn_control_plane(journal, ctx, stream);

        let (input_tx, mut input_rx) = mpsc::channel::<AgentInput>(1);
        insert_session(&sessions, "s-fifo", input_tx, Some(dir.path()));

        let auth = authed_state();
        let padding = "x".repeat(1 << 20);
        for i in 0..4 {
            let ask = request(
                AskSessionRequest::METHOD,
                json!({
                    "session_id": "s-fifo",
                    "user_message": format!("m{i}:{padding}"),
                }),
            );
            assert!(ingest(&control, ask, test_origin(), &auth).is_none());
            let cancel = request(
                CancelSessionRequest::METHOD,
                json!({ "session_id": "s-fifo" }),
            );
            assert!(ingest(&control, cancel, test_origin(), &auth).is_none());
        }

        // Drain: journal order was ask(m0), cancel, ask(m1), cancel, …
        // — the input channel must replay exactly that.
        for i in 0..4 {
            let delivered = tokio::time::timeout(Duration::from_secs(10), input_rx.recv())
                .await
                .unwrap_or_else(|_| panic!("input {i} (ask) not delivered"))
                .expect("channel open");
            match delivered {
                AgentInput::UserMessage(mu_core::agent::AgentMessage::User { content }, _) => {
                    assert!(
                        content.starts_with(&format!("m{i}:")),
                        "asks must arrive in journal order: expected m{i}, got {}…",
                        &content[..8.min(content.len())]
                    );
                }
                other => panic!("expected UserMessage m{i}, got {other:?}"),
            }
            let delivered = tokio::time::timeout(Duration::from_secs(10), input_rx.recv())
                .await
                .unwrap_or_else(|_| panic!("input {i} (cancel) not delivered"))
                .expect("channel open");
            assert!(
                matches!(delivered, AgentInput::Cancel),
                "cancel {i} must follow its ask: got {delivered:?}"
            );
        }
    }

    /// Cross-session concurrency survives WP8: a session whose
    /// dispatcher is wedged on a full input channel delays neither
    /// another session's commands nor the control plane.
    #[tokio::test]
    async fn wedged_session_does_not_delay_other_sessions_or_control_plane() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Arc::new(
            CommandJournal::open(&dir.path().join("d.jsonl"), "d", FsyncPolicy::Never)
                .expect("open journal"),
        );
        let ctx = test_ctx();
        let sessions = ctx.sessions.clone();
        let stream = Router::new();
        let outbound_lane = stream.register(test_origin());
        let control = spawn_control_plane(journal, ctx, stream);

        // Session A: saturated channel, receiver alive but never
        // draining — its dispatcher wedges on the first ask.
        let (a_tx, _a_rx) = mpsc::channel::<AgentInput>(1);
        a_tx.try_send(AgentInput::Cancel).expect("pre-fill");
        insert_session(&sessions, "s-wedged", a_tx, Some(dir.path()));
        // Session B: healthy.
        let (b_tx, mut b_rx) = mpsc::channel::<AgentInput>(4);
        insert_session(&sessions, "s-healthy", b_tx, Some(dir.path()));

        let auth = authed_state();
        let ask_a = request(
            AskSessionRequest::METHOD,
            json!({ "session_id": "s-wedged", "user_message": "stuck" }),
        );
        assert!(ingest(&control, ask_a, test_origin(), &auth).is_none());
        let ask_b = request(
            AskSessionRequest::METHOD,
            json!({ "session_id": "s-healthy", "user_message": "moving" }),
        );
        assert!(ingest(&control, ask_b, test_origin(), &auth).is_none());

        // B's command lands despite A's wedged dispatcher…
        let delivered = tokio::time::timeout(Duration::from_secs(2), b_rx.recv())
            .await
            .expect("session B must not be delayed by session A's wedge")
            .expect("channel open");
        assert!(matches!(delivered, AgentInput::UserMessage(_, _)));

        // …and the control plane still answers daemon-scoped commands.
        assert!(ingest(&control, request("ping", json!(null)), test_origin(), &auth).is_none());
        let pong = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let envelope = outbound_lane.recv().await.expect("lane open");
                if envelope.item.0["result"]["pong"] == true {
                    return;
                }
            }
        })
        .await;
        assert!(
            pong.is_ok(),
            "control plane must not block on a wedged session"
        );
    }

    /// Receipt-result elision (WP8): a query-class command's success
    /// receipt carries the compact elision marker — NOT the (possibly
    /// huge) result it would otherwise re-embed into the very session
    /// log it read — while a mutating command's receipt keeps its real
    /// result. CommandEcho is untouched either way.
    #[tokio::test]
    async fn query_receipt_elides_result_mutation_receipt_keeps_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Arc::new(
            CommandJournal::open(&dir.path().join("d.jsonl"), "d", FsyncPolicy::Never)
                .expect("open journal"),
        );
        let ctx = test_ctx();
        let sessions = ctx.sessions.clone();
        let stream = Router::new();
        let control = spawn_control_plane(journal, ctx, stream);

        let (input_tx, mut _input_rx) = mpsc::channel::<AgentInput>(4);
        let log = insert_session(&sessions, "s-q", input_tx, Some(dir.path()));
        // Seed content a non-elided session.events receipt would echo
        // back into the log.
        log.append(
            EventActor::User,
            EventPayload::UserMessage {
                content: "seed-event-content".to_string(),
            },
        );

        // Query: session.events (journal_queries defaults to true).
        let auth = authed_state();
        let req = request(SessionEventsRequest::METHOD, json!({ "session_id": "s-q" }));
        assert!(ingest(&control, req, test_origin(), &auth).is_none());
        wait_for_log(&log, |events| {
            events.iter().any(|e| {
                matches!(&e.payload, EventPayload::CommandSucceeded { command, .. }
                    if command.method == SessionEventsRequest::METHOD)
            })
        })
        .await;
        let (echo_method, result) = log
            .snapshot()
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::CommandSucceeded {
                    command, result, ..
                } if command.method == SessionEventsRequest::METHOD => {
                    Some((command.method.clone(), result.clone()))
                }
                _ => None,
            })
            .expect("session.events receipt");
        assert_eq!(
            echo_method,
            SessionEventsRequest::METHOD,
            "echo keeps the command"
        );
        assert_eq!(
            result["elided"], true,
            "query result must be elided: {result}"
        );
        assert!(
            result["result_bytes"].as_u64().unwrap_or(0) > 0,
            "marker records the elided size: {result}"
        );
        assert!(
            !serde_json::to_string(&result)
                .expect("serialize")
                .contains("seed-event-content"),
            "elided receipt must not embed event content: {result}"
        );

        // Mutation: cancel_session on the same session keeps its real
        // result.
        let req = request(CancelSessionRequest::METHOD, json!({ "session_id": "s-q" }));
        assert!(ingest(&control, req, test_origin(), &auth).is_none());
        wait_for_log(&log, |events| {
            events.iter().any(|e| {
                matches!(&e.payload, EventPayload::CommandSucceeded { command, .. }
                    if command.method == CancelSessionRequest::METHOD)
            })
        })
        .await;
        let cancel_result = log
            .snapshot()
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::CommandSucceeded {
                    command, result, ..
                } if command.method == CancelSessionRequest::METHOD => Some(result.clone()),
                _ => None,
            })
            .expect("cancel_session receipt");
        assert_eq!(
            cancel_result["cancelled"], true,
            "mutation receipts keep their real result: {cancel_result}"
        );
        assert!(
            cancel_result.get("elided").is_none(),
            "mutation receipts are never elided: {cancel_result}"
        );
    }
}
