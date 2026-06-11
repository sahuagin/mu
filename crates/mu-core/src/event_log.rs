//! Per-session append-only event log.
//!
//! The durable record of what happened in a session. Sessions append
//! significant events (user message arrived, assistant message
//! completed, tool called, tool result returned, ask round-trip
//! finished, error). Streaming-only events (text deltas, lifecycle
//! ticks) do NOT go in the log — they're projection details for the
//! wire layer.
//!
//! v1 was in-memory only; JSONL persistence, `ContextAssembly`
//! events (mu-032), and `CompactionAssembly` events (mu-za92) have
//! since landed. Remaining future work (per architecture doc
//! `specs/architecture/event-sourced-context.md`): `MemoryWrite`
//! events, branching/lineage via `parent_event_ids`.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::{AssistantMessage, StopReason, Usage};
use crate::command_journal::{AuthSnapshot, CommandEcho, Origin, RejectStage};

/// A single event in a session's durable log.
///
/// Envelope is shared across all kinds; payload is a tagged enum so
/// each kind keeps its own typed shape. See architecture doc for
/// the full design rationale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEvent {
    /// Monotonic per-session id, starting at 1. NOT globally unique.
    pub id: u64,
    pub session_id: String,
    /// Causal links to prior events (e.g. ToolResult points back to
    /// the ToolCall it answers, AssistantMessage points to the
    /// UserMessage it replies to). v1 leaves these mostly empty;
    /// future work populates them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parent_event_ids: Vec<u64>,
    /// Unix milliseconds at append time.
    pub timestamp_unix_ms: u64,
    pub actor: EventActor,
    pub payload: EventPayload,
}

/// Who/what produced the event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventActor {
    User,
    Agent,
    Tool { name: String },
    Provider { name: String },
    System,
}

/// Typed event payload. Common envelope, different shapes per kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventPayload {
    /// Session opened. Records provider+model selection. When the
    /// session is a delegate (born via `session.delegate`), carries
    /// the parent's id and the event in the parent's log this
    /// branched from. Both fields are None for root sessions.
    SessionCreated {
        provider_kind: String,
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branched_at_parent_event_id: Option<u64>,
        /// mu-rf9x: the provider's token-accounting convention,
        /// stamped at registration so log readers can interpret every
        /// subsequent usage record without provider-specific
        /// arithmetic. In force until a `ProviderSwitched` event
        /// restates it. `None` for pre-mu-rf9x logs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage_semantics: Option<crate::agent::capabilities::UsageSemantics>,
    },
    /// User-side input message arrived.
    UserMessage { content: String },
    /// Assistant turn completed. Carries text content, tool calls,
    /// stop reason, and per-turn usage (when the provider reports it).
    AssistantMessageEvent { message: AssistantMessage },
    /// A tool was invoked by the agent.
    ToolCall {
        call_id: String,
        name: String,
        arguments: Value,
    },
    /// The tool returned.
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
    /// One `ask_session` round-trip terminated. The aggregated usage
    /// and elapsed_ms here are what was sent on the wire's `session.done`
    /// notification. Summing across all Done events in a session
    /// gives session-cumulative usage.
    Done {
        stop_reason: StopReason,
        turn_count: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        elapsed_ms: Option<u64>,
    },
    /// Terminal error event.
    Error { message: String },
    /// Free-form agent observation (memory recall, status note, etc).
    /// Mirrors the wire-level `session.callout`.
    Callout {
        category: String,
        title: String,
        body: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        theme: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        context_refs: Vec<String>,
    },
    /// mu-gdwd: a provider message failed boundary validation
    /// (e.g., tool-call arguments contained NaN/Inf). The raw
    /// payload is preserved verbatim (capped at 64 KiB) for
    /// postmortem analysis. The turn is aborted.
    ErrorInvalidMessage {
        provider: String,
        raw_message: String,
        validation_error: String,
    },
    /// Provider/model switched mid-session (mu-k56u). Logged by the
    /// agent loop when it receives `AgentInput::SwitchProvider`.
    /// Properties are snapshotted from the route catalog at switch time.
    ProviderSwitched {
        old_provider_kind: Arc<str>,
        old_model: Arc<str>,
        new_provider_kind: Arc<str>,
        new_model: Arc<str>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_soft_limit: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_hard_limit: Option<u64>,
        /// mu-rf9x: the NEW provider's token-accounting convention —
        /// re-registration restates the interpretation in force for
        /// usage records from this point on. `None` for pre-mu-rf9x
        /// logs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage_semantics: Option<crate::agent::capabilities::UsageSemantics>,
    },
    /// Session closed (via `close_session` RPC or daemon shutdown).
    SessionClosed,
    /// Record of the prompt assembled for a provider call (mu-032).
    /// Emitted BEFORE `provider.stream()`. The agent loop records
    /// what was about to be sent so postmortem analysis can answer
    /// "what did the model see right before this?"
    ///
    /// v1 records counts + provider info. Per-message source-event
    /// mapping (the full source-map vision from
    /// specs/architecture/event-sourced-context.md) is reconstructable
    /// from the log by walking events of the relevant roles; an
    /// explicit per-segment field is reserved for v2.
    ContextAssembly {
        /// Monotonic counter, unique within this session. Links a
        /// ContextAssembly to the subsequent AssistantMessage/Done
        /// for this same model call.
        model_call_id: u32,
        message_count: u32,
        user_message_count: u32,
        assistant_message_count: u32,
        tool_result_count: u32,
        /// Number of tool specs in the request.
        tool_count: u32,
        /// Renderer-estimated token count of the assembled rope.
        /// `None` only for pre-mu-heqf sessions (v1 left this
        /// unwired; the live loop now populates it from
        /// `ProviderRenderer::context_sizes`). mu-wsgx: the live
        /// compaction trigger no longer compares this raw estimate —
        /// it predicts the prompt total from the previous call's
        /// provider-reported usage plus the estimate delta.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_count_estimate: Option<u64>,
        /// mu-heqf: per-section breakdown of `token_count_estimate`,
        /// keyed by `SpanKind::label()` (`"system"`, `"user"`,
        /// `"tool_result"`, `"file_load"`, …). Sections sum to the
        /// total by construction. Empty for pre-mu-heqf sessions.
        /// Answers "where is the context going?" straight from the
        /// JSONL — the instrumentation mu-u6hc's region map reads.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        token_breakdown: BTreeMap<String, u64>,
        /// Provider + model from the session's selector.
        provider_kind: String,
        model: String,
        /// mu-fb0: which `ProviderRenderer` rendered the rope for
        /// this call. `None` for pre-mu-fb0 sessions (durable log
        /// fixtures); `Some(...)` once the live loop projects through
        /// `Provider::renderer()`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        renderer: Option<String>,
        /// mu-fb0: which `CacheStrategy` was applied.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_strategy: Option<String>,
        /// mu-fb0: total span count in the projected rope (system +
        /// tool schemas + messages).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        span_count: Option<u32>,
        /// mu-fb0: number of cache boundaries the strategy placed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_boundary_count: Option<u32>,
        /// mu-fb0: first up-to-5 span ids of the rope (provenance
        /// breadcrumb without the full rope dump).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        first_span_ids: Vec<String>,
        /// mu-814o: blake3 digest (16 hex) of the rendered cacheable
        /// prefix (messages up to the last cache boundary). Diffing
        /// consecutive calls makes a mid-session full prefix-cache
        /// invalidation a one-grep diagnosis. `None` when no
        /// boundaries / pre-mu-814o sessions.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix_hash: Option<String>,
        /// mu-814o: per-span `"<id>=<blake3 8hex>"` rope-content
        /// digests over the same prefix range — names which span
        /// mutated under a stable id.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        prefix_span_hashes: Vec<String>,
    },
    /// mu-za92: a compaction policy ran (mu-kgu.4). Durable record of
    /// what was ejected and kept — pre-mu-za92 this lived only on the
    /// in-process event surface and the in-memory rope log, both of
    /// which vanish on process exit, leaving compaction invisible in
    /// the source-of-truth JSONL (the gap that made the
    /// mu-compaction-not-firing-self-hosted-ooil investigation
    /// require transcript archaeology instead of one grep).
    ///
    /// Emitted BEFORE the matching `ContextAssembly` for the same
    /// `model_call_id`: this event says what the policy did; that one
    /// says what was then rendered.
    CompactionAssembly {
        /// Joins with `ContextAssembly::model_call_id`.
        model_call_id: u32,
        /// `CompactionPolicy::policy_label()` of the policy that ran.
        policy_id: String,
        /// Renderer-estimated tokens before compaction — the value
        /// the threshold check saw.
        tokens_before: u64,
        /// Renderer-estimated tokens after. Best-effort; may exceed
        /// the policy's target.
        tokens_after: u64,
        /// Full per-span audit log: kept / dropped(reason) /
        /// summarized / failed(reason). Empty = identity result
        /// (fail-closed path) — the event still lands so an
        /// attempted-but-no-op compaction is visible.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        decisions: Vec<crate::context::CompactionDecision>,
        /// Wall-clock duration of `policy.compact()` in microseconds.
        wall_clock_us: u64,
    },
    /// mu-036: autonomous loop iteration began. `iteration` is
    /// 0-indexed across the run; `motivation` is the model-reported
    /// one-sentence "what I'm doing this turn and why" (after a
    /// schedule_wakeup, this is the wake reason).
    AutonomousIterationStarted { iteration: u32, motivation: String },
    /// mu-036: autonomous loop iteration ended. Outcome tells the
    /// caller whether the loop continues, exits, escalates, or errors.
    AutonomousIterationCompleted {
        iteration: u32,
        outcome: crate::protocol::AutonomousIterationOutcome,
    },
    /// mu-036: session has been parked via session.schedule_wakeup.
    /// While sleeping, no provider calls fire (INV-5). On wake, the
    /// next `AutonomousIterationStarted` carries `reason` as its
    /// motivation.
    AutonomousScheduledWakeup {
        wake_at_unix_ms: u64,
        reason: String,
    },
    /// mu-036: autonomous loop terminated. Always the final autonomy
    /// event for this run (INV-7); session returns to RunMode::Idle
    /// and is addressable via ask_session again.
    AutonomousTerminated {
        reason: crate::protocol::AutonomousTerminationReason,
    },
    /// Durable mirror of the wire-side `session.provider_status`
    /// notification (mu-035). Emitted on state transitions and on
    /// periodic ticks during non-streaming waits, both for live
    /// observability by streaming consumers and for post-hoc
    /// aggregation (mu-pex: TTFT = AwaitingFirstToken→Streaming gap,
    /// streaming_ms = time in Streaming per call). Field shape
    /// mirrors `crate::protocol::ProviderStatusEvent` minus
    /// `session_id` — the log already knows its session.
    ProviderStatusUpdate {
        state: crate::protocol::ProviderStatusKind,
        /// Unix milliseconds the session entered this state.
        started_at_unix_ms: u64,
        /// Milliseconds since `started_at_unix_ms` at emit time.
        /// Periodic ticks during a long wait re-emit with this
        /// value advancing; transitions emit with elapsed_ms = 0.
        elapsed_ms: u64,
        /// Cumulative bytes from the provider's SSE stream so far.
        /// None when not meaningful (Idle, pre-first-byte
        /// AwaitingFirstToken, providers that don't surface counts).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bytes_received: Option<u64>,
        /// Set only when `state` is ToolExecuting or AwaitingToolResult.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
    },
    /// mu-lho (mu-037 Phase 1): a peer posted a mailbox message to
    /// this session. Append-only — consumption is recorded as a
    /// separate `MailboxMessageConsumed` event referring back by
    /// `seq`. `mailbox.list` projects from these two variants:
    /// `posted_set ∖ consumed_set`. v1 retains all message bodies
    /// in the event log; a future retention/compaction pass can
    /// summarize old posted-and-consumed pairs (mu-mh4 territory).
    MailboxMessagePosted {
        /// Per-session monotonic sequence number. Assigned at
        /// `mailbox.post` dispatch time via the session's
        /// `mailbox_next_seq` atomic counter; carried on the wire
        /// in the `MailboxPostResponse`.
        seq: u64,
        /// Originating daemon id. Same-daemon in Phase 1; meaningful
        /// in Phase 2+ cross-daemon scenarios.
        from_daemon_id: String,
        /// Originating session id within `from_daemon_id`.
        from_session_id: String,
        /// Message-kind discriminator. Free-form string for v1
        /// (typical values: `"callout" | "task" | "fyi" |
        /// "file_reference" | "grader_result"`). A future spec
        /// can lock the enum.
        ///
        /// Wire-level rename to `kind` would collide with the
        /// EventPayload enum's `#[serde(tag = "kind")]` outer tag,
        /// so the on-disk field is `message_kind`. The wire-level
        /// `MailboxMessageView` (a separate struct) uses `kind`.
        message_kind: String,
        /// Short subject line, like an email Subject header.
        subject: String,
        /// Shape varies by `message_kind`; v1 stores opaque JSON so
        /// receivers can interpret per their handler.
        body: Value,
        /// Optional wall-clock expiration. None = no expiry.
        /// Receivers MAY filter expired messages from `mailbox.list`
        /// projections.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at_unix_ms: Option<u64>,
    },
    /// mu-lho (mu-037 Phase 1): the recipient session marked a
    /// previously-posted mailbox message as consumed via
    /// `mailbox.consume`. Refers back by the post's `seq`. Append-
    /// only — un-consuming requires a new post.
    MailboxMessageConsumed {
        /// The `seq` of the `MailboxMessagePosted` being marked.
        seq: u64,
    },
    /// mu-5g7i / spec mu-040: per-task termination envelope for the
    /// forensics axis (mu-fvy0 cluster). Emitted exactly once per
    /// task end, alongside the existing `Done` / `Error` events.
    /// Carries the rich envelope downstream analytics need (provider,
    /// timing, usage, exit reason). Fields the forwarder cannot
    /// populate yet (tools_granted/called, budget, local time) emit
    /// as `None` / empty `Vec` rather than being skipped — the
    /// contract is "always emit, fill in over time," not "emit only
    /// when complete."
    TaskTelemetry {
        task_id: String,
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_task_id: Option<String>,
        provider_kind: String,
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_version: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        started_at_unix_ms: Option<u64>,
        ended_at_unix_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wall_clock_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        completion_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_read_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_write_tokens: Option<u64>,
        /// Ephemeral-5m cache write tokens (1.25× premium). Present only when
        /// the provider returns the per-tier breakdown. mu-cache-write-tier-split-umq6.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_write_5m_tokens: Option<u64>,
        /// Ephemeral-1h cache write tokens (2.0× premium). Present only when
        /// the provider returns the per-tier breakdown. mu-cache-write-tier-split-umq6.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_write_1h_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tools_granted: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tools_actually_called: Vec<(String, u32)>,
        exit_reason: TaskExitReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_budget_usd: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actual_spend_usd: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        local_hour: Option<u8>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        day_of_week: Option<u8>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tz: Option<String>,
    },
    /// mu-slat: a pot-hosted claude-code worker was spawned as a
    /// subprocess session. Emitted once by the supervisor when the
    /// worker process starts successfully.
    WorkerSpawned {
        pot_name: String,
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt_summary: Option<String>,
    },
    /// mu-slat: worker process exited normally.
    WorkerExited { exit_code: i32, elapsed_ms: u64 },
    /// mu-slat: worker process failed (spawn error, signal, etc).
    WorkerFailed { reason: String },
    /// mu-slat: worker killed by timeout.
    WorkerTimeout { elapsed_ms: u64 },
    /// mu-operator-mark-5mwr: operator-assigned session quality mark,
    /// captured from the console or the `mu mark` CLI. Append-only —
    /// re-marking appends a newer event and projections take the
    /// latest by event id. Exists so degraded sessions can be
    /// labeled at the moment of judgment and later queried as
    /// replay-probe fixtures (projection side: mu-mucm
    /// `session_marks` view — field names locked between them).
    OperatorMark {
        /// 1 (unusable) ..= 5 (excellent).
        rating: u8,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// mu-mh4: a live head was attached to this session's log — the
    /// "attach head" half of resume. A session that died on daemon A is
    /// resumed by daemon B forking a fresh live session at the dead
    /// one's last clean boundary; this event records the attach so the
    /// session's identity is continuous across serving daemons. Lands
    /// on the NEW (live) session's log, naming the daemon that attached
    /// and the actor who requested it; the new `SessionCreated`'s
    /// `parent_session_id` / `branched_at_parent_event_id` point back to
    /// the predecessor.
    HeadAttached {
        /// The daemon that attached the live head (the resuming daemon).
        daemon_id: String,
        /// Who CLAIMED to request the attach (operator / agent actor).
        /// mu-mh4 (panel finding 3): this is caller-supplied and
        /// UNVERIFIED — `session.resume` has no connection-derived
        /// identity to cross-check it against (the serve layer
        /// authenticates the connection with a trust-on-spawn bearer
        /// token, not a per-actor identity). Named `claimed_actor` so
        /// every projection of this event treats the attribution as a
        /// claim, not a verified fact. A future identity layer (mu-nqn5
        /// follow-up) can stamp a verified actor alongside.
        claimed_actor: String,
        /// The predecessor session this forked from.
        predecessor_session_id: String,
        /// The event id in the predecessor's log this forked at (the
        /// clean boundary).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branched_at_event_id: Option<u64>,
    },
    /// mu-mh4: an append-only compensating event that marks an earlier
    /// record as poisoned (broken / incomplete / malformed). The log
    /// is NEVER edited; a Tombstone is laid OVER the bad record so
    /// every projection can skip it via one rule (skip tombstoned
    /// event ids). Born for `mu --recover` (lay tombstones over the
    /// ragged tail of a session that died mid-iteration, then resume
    /// from the last clean prompt), but the kind generalizes to ANY
    /// poisoned record: degraded_eof partials, malformed tool results,
    /// etc. Attributed and reasoned so the scar is legible in every
    /// consumer (the console renders tombstoned spans as scar tissue).
    Tombstone {
        /// The `id` of the event this tombstone invalidates.
        target_event_id: u64,
        /// Human-readable reason, e.g. "incomplete record: tool call
        /// `c1` has no matching ToolResult".
        reason: String,
    },
    /// spec mu-046: a session-scoped command crossed the daemon's
    /// border. Journaled into this log — via the strict
    /// [`SessionEventLog::append_command`] path — BEFORE the command
    /// enters the session's input queue (INV-1). The append happens
    /// before the auth gate, so rejected commands are visible too.
    CommandReceived {
        /// JSON-RPC id (client-chosen, NOT unique).
        request_id: Value,
        method: String,
        /// Secret-redacted params (INV-6).
        params: Value,
        auth: AuthSnapshot,
        origin: Origin,
    },
    /// spec mu-046: receipt — the command completed. Wraps the
    /// original command (INV-5) so the receipt is self-contained
    /// evidence of what was asked and what came of it.
    CommandSucceeded {
        /// The event id of the `CommandReceived` this answers — same
        /// correlation scheme as the daemon journal's `seq`.
        command_event_id: u64,
        command: CommandEcho,
        result: Value,
        elapsed_ms: u64,
    },
    /// spec mu-046: receipt — the command failed in processing.
    CommandFailed {
        command_event_id: u64,
        command: CommandEcho,
        code: i32,
        message: String,
        elapsed_ms: u64,
    },
    /// spec mu-046: receipt — the command was rejected before any
    /// handler ran (auth gate, validation, routing). Rejections are
    /// receipts too (INV-4: every CommandReceived eventually gains
    /// exactly one receipt — or none, and the orphan IS the legible
    /// crash marker).
    CommandRejected {
        command_event_id: u64,
        command: CommandEcho,
        code: i32,
        message: String,
        stage: RejectStage,
    },
}

impl EventPayload {
    /// The serde `kind` tag of this payload, as a static string —
    /// THE single source for displaying an event's kind. Must stay
    /// in lockstep with the `#[serde(tag = "kind", rename_all =
    /// "snake_case")]` derive above; the
    /// `kind_str_matches_serde_tag_for_representative_variants` test
    /// pins a representative set.
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::SessionCreated { .. } => "session_created",
            Self::UserMessage { .. } => "user_message",
            Self::AssistantMessageEvent { .. } => "assistant_message_event",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolResult { .. } => "tool_result",
            Self::Done { .. } => "done",
            Self::Error { .. } => "error",
            Self::Callout { .. } => "callout",
            Self::ErrorInvalidMessage { .. } => "error_invalid_message",
            Self::ProviderSwitched { .. } => "provider_switched",
            Self::SessionClosed => "session_closed",
            Self::ContextAssembly { .. } => "context_assembly",
            Self::CompactionAssembly { .. } => "compaction_assembly",
            Self::AutonomousIterationStarted { .. } => "autonomous_iteration_started",
            Self::AutonomousIterationCompleted { .. } => "autonomous_iteration_completed",
            Self::AutonomousScheduledWakeup { .. } => "autonomous_scheduled_wakeup",
            Self::AutonomousTerminated { .. } => "autonomous_terminated",
            Self::ProviderStatusUpdate { .. } => "provider_status_update",
            Self::MailboxMessagePosted { .. } => "mailbox_message_posted",
            Self::MailboxMessageConsumed { .. } => "mailbox_message_consumed",
            Self::TaskTelemetry { .. } => "task_telemetry",
            Self::WorkerSpawned { .. } => "worker_spawned",
            Self::WorkerExited { .. } => "worker_exited",
            Self::WorkerFailed { .. } => "worker_failed",
            Self::WorkerTimeout { .. } => "worker_timeout",
            Self::OperatorMark { .. } => "operator_mark",
            Self::HeadAttached { .. } => "head_attached",
            Self::Tombstone { .. } => "tombstone",
            Self::CommandReceived { .. } => "command_received",
            Self::CommandSucceeded { .. } => "command_succeeded",
            Self::CommandFailed { .. } => "command_failed",
            Self::CommandRejected { .. } => "command_rejected",
        }
    }
}

/// Categorical exit reason for a task — what brought the task to its
/// terminal state. Companion to `EventPayload::TaskTelemetry`. mu-5g7i.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskExitReason {
    /// Provider returned a terminal stop_reason (EndTurn, MaxTokens, etc).
    Done,
    /// Loop emitted `AgentEvent::Error` (provider failure, tool failure
    /// surfaced as terminal, internal panic-equivalent).
    Error,
    /// Task was aborted via cancel_session or operator stop. Maps from
    /// `StopReason::Aborted` in the existing Done path.
    Cancelled,
    /// Budget ledger tripped a cap. Not yet emitted by the loop
    /// (budget enforcement is a separate axis under mu-fvy0).
    BudgetCap,
    /// Watchdog timeout. Not yet emitted by the loop.
    Timeout,
    /// Operator-issued stop distinct from Cancelled. Reserved.
    OperatorStopped,
}

/// Append-only per-session log.
///
/// Cheap to clone (Arc-wrapped internally). Safe to share across
/// the dispatch loop, forwarder, and tests.
#[derive(Debug)]
pub struct SessionEventLog {
    session_id: String,
    events: Mutex<Vec<SessionEvent>>,
    next_id: AtomicU64,
    /// Optional on-disk JSONL writer (mu-upb). When set, every
    /// append() also writes the encoded event as one line. IO
    /// failures are logged but never block the in-memory append —
    /// disk persistence is best-effort, not load-bearing.
    disk_writer: Mutex<Option<File>>,
}

impl SessionEventLog {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            events: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            disk_writer: Mutex::new(None),
        }
    }

    /// mu-935: rebuild a SessionEventLog from a JSONL file previously
    /// written by `attach_disk_writer` (mu-upb's path). Used by the
    /// FileBackend discovery layer to read peer daemons' sessions
    /// off disk, and will be used by mu-mh4 (session persistence
    /// across daemon restart) when that lands.
    ///
    /// The returned log is in-memory only — no disk writer attached.
    /// `next_id` is set to `max(existing_id) + 1` so future appends
    /// don't collide. Malformed lines are skipped with a counter
    /// returned so callers can surface "we recovered N events,
    /// skipped M malformed ones." `session_id` is taken from the
    /// first event; if the file has no events, falls back to the
    /// filename stem.
    pub fn from_jsonl(path: &std::path::Path) -> std::io::Result<(Self, usize)> {
        use std::io::BufRead;

        let file = std::fs::File::open(path)?;
        let reader = std::io::BufReader::new(file);
        let mut events: Vec<SessionEvent> = Vec::new();
        let mut malformed: usize = 0;
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => {
                    malformed = malformed.saturating_add(1);
                    continue;
                }
            };
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionEvent>(&line) {
                Ok(ev) => events.push(ev),
                Err(_) => malformed = malformed.saturating_add(1),
            }
        }
        let session_id = events
            .first()
            .map(|e| e.session_id.clone())
            .unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("recovered")
                    .to_string()
            });
        let next_id = events
            .iter()
            .map(|e| e.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        Ok((
            Self {
                session_id,
                events: Mutex::new(events),
                next_id: AtomicU64::new(next_id),
                disk_writer: Mutex::new(None),
            },
            malformed,
        ))
    }

    /// Attach an on-disk JSONL writer (mu-upb). Creates the parent
    /// directories if needed and opens the file in append mode.
    ///
    /// Returns the path that was opened on success (useful for
    /// logging "events going to /path/to/file.jsonl"). On error,
    /// the writer stays None and append() continues in-memory only.
    pub fn attach_disk_writer(&self, path: &std::path::Path) -> std::io::Result<PathBuf> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        // Fsync the parent directory so the just-created file's dirent
        // survives a crash (the file's own writes don't persist it).
        // BEST-EFFORT, matching this log's posture (append() swallows
        // IO errors; persistence here is not load-bearing) — contrast
        // `CommandJournal::open`, where the same sync propagates.
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            if let Err(e) = std::fs::File::open(parent).and_then(|d| d.sync_all()) {
                tracing::warn!(
                    session_id = %self.session_id,
                    path = %parent.display(),
                    error = %e,
                    "parent-directory fsync failed after attaching disk writer; continuing"
                );
            }
        }
        let mut guard = self
            .disk_writer
            .lock()
            .map_err(|_| std::io::Error::other("disk_writer mutex poisoned"))?;
        *guard = Some(file);
        Ok(path.to_path_buf())
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Append an event. Returns the assigned id. Lock poisoning is
    /// silently recovered (the daemon should not crash on a poisoned
    /// log mutex; missing one append is better than a crash).
    pub fn append(&self, actor: EventActor, payload: EventPayload) -> u64 {
        self.append_with_parents(actor, payload, Vec::new())
    }

    pub fn append_with_parents(
        &self,
        actor: EventActor,
        payload: EventPayload,
        parent_event_ids: Vec<u64>,
    ) -> u64 {
        // mu-kgpg: this mutex is also the append-order mutex.  Both
        // best-effort appends and strict command appends hold it from
        // id assignment through disk projection and in-memory push, so
        // file order, event id order, and snapshot order cannot drift
        // across the two append paths.
        let mut disk_guard = match self.disk_writer.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::warn!(
                    session_id = %self.session_id,
                    "disk writer mutex poisoned; recovering for best-effort append"
                );
                poisoned.into_inner()
            }
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let event = SessionEvent {
            id,
            session_id: self.session_id.clone(),
            parent_event_ids,
            timestamp_unix_ms: now_unix_ms(),
            actor,
            payload,
        };
        // mu-upb: best-effort JSONL write before the in-memory push
        // so the disk record is at least as complete as memory. IO
        // failures are logged and ignored — disk persistence is not
        // load-bearing for the running daemon.
        if let Some(file) = disk_guard.as_mut() {
            match serde_json::to_string(&event) {
                Ok(line) => {
                    if let Err(e) = writeln!(file, "{line}") {
                        tracing::warn!(
                            session_id = %self.session_id,
                            error = %e,
                            "disk write failed; continuing in-memory only"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = %self.session_id,
                        error = %e,
                        "event serialization failed for disk write"
                    );
                }
            }
        }
        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        } else {
            tracing::warn!(
                session_id = %self.session_id,
                "event log mutex poisoned; event dropped"
            );
        }
        id
    }

    /// spec mu-046: like [`append`](Self::append), but for commands —
    /// the strict path. Writes the JSONL line, `sync_data()`s, THEN
    /// pushes to memory; IO errors propagate so the caller can fail
    /// closed (reject with `JOURNAL_UNAVAILABLE`, never process —
    /// INV-2). The inverse of `append`'s swallow-and-continue:
    /// command durability is load-bearing, gateway-event durability
    /// is best-effort.
    ///
    /// Errors `Unsupported` when no disk writer is attached — an
    /// in-memory-only session cannot make a command durable, and
    /// silently succeeding would forge the write-ahead guarantee.
    pub fn append_command(&self, actor: EventActor, payload: EventPayload) -> std::io::Result<u64> {
        // Hold the append-order lock across assign-id → write → fsync
        // → memory push so disk order, event id order, and snapshot
        // order stay aligned against both command and non-command
        // appends.
        let mut guard = self
            .disk_writer
            .lock()
            .map_err(|_| std::io::Error::other("disk_writer mutex poisoned"))?;
        let Some(file) = guard.as_mut() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "append_command requires a disk writer: commands must be durable before processing (spec mu-046 INV-1)",
            ));
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let event = SessionEvent {
            id,
            session_id: self.session_id.clone(),
            parent_event_ids: Vec::new(),
            timestamp_unix_ms: now_unix_ms(),
            actor,
            payload,
        };
        let line = serde_json::to_string(&event).map_err(std::io::Error::other)?;
        writeln!(file, "{line}")?;
        file.sync_data()?;
        // Disk first, memory second: by the time the event is visible
        // in any projection it is already durable. A poisoned events
        // mutex after a durable write surfaces as an error — the
        // on-disk record then reads as an orphan, which is the legible
        // outcome (INV-4). Keep the append-order lock held until the
        // memory push completes so a best-effort append cannot overtake
        // a strict command append in snapshots.
        let mut events = self
            .events
            .lock()
            .map_err(|_| std::io::Error::other("event log mutex poisoned after durable write"))?;
        events.push(event);
        Ok(id)
    }

    /// spec mu-046 WP4: does this log have an on-disk JSONL writer
    /// attached? The ingest pipeline consults this to decide where a
    /// session-scoped command journals: a disk-backed session log can
    /// take the strict [`append_command`](Self::append_command) path;
    /// an in-memory-only log cannot make a command durable, so the
    /// pipeline explicitly falls back to the daemon control-plane
    /// journal instead (border compliance preserved; session-log
    /// strictness needs disk).
    pub fn has_disk_writer(&self) -> bool {
        self.disk_writer
            .lock()
            .map(|g| g.is_some())
            .unwrap_or(false)
    }

    /// Snapshot the log. Clones the inner vec — safe to read without
    /// holding the lock.
    pub fn snapshot(&self) -> Vec<SessionEvent> {
        self.events
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.events.lock().map(|g| g.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Sum usage across all `Done` events in the log. Returns None if
    /// no Done event ever reported usage (e.g. faux provider, or
    /// every ask hit an error before reporting).
    pub fn cumulative_usage(&self) -> Option<Usage> {
        let Ok(events) = self.events.lock() else {
            return None;
        };
        let mut acc: Option<Usage> = None;
        for ev in events.iter() {
            if let EventPayload::Done { usage: Some(u), .. } = &ev.payload {
                acc = Some(match acc {
                    Some(prev) => prev + *u,
                    None => *u,
                });
            }
        }
        acc
    }

    /// Sum usage across all `AssistantMessageEvent` events — gives
    /// real-time token totals that update per model call, not just per
    /// completed ask. Returns the last model call's prompt-total tokens
    /// separately (for context pressure calculation).
    ///
    /// mu-rf9x: the prompt total is interpreted under the
    /// [`UsageSemantics`](crate::agent::capabilities::UsageSemantics)
    /// registered by the log's `SessionCreated` event (and restated by
    /// any `ProviderSwitched`) — the convention in force is folded
    /// forward, so an OpenAI-shaped provider's cache reads are no
    /// longer double-counted (the 2026-06-03 "93k vs 55.6k" status-bar
    /// incident). Pre-mu-rf9x logs carry no declaration; they fall
    /// back to the legacy additive sum, which was correct for
    /// Anthropic logs and stays wrong-but-unchanged for old OpenAI
    /// logs (those age out).
    pub fn live_usage(&self) -> (Option<Usage>, Option<u64>) {
        let Ok(events) = self.events.lock() else {
            return (None, None);
        };
        let mut acc: Option<Usage> = None;
        let mut last_input: Option<u64> = None;
        let mut semantics: Option<crate::agent::capabilities::UsageSemantics> = None;
        for ev in events.iter() {
            match &ev.payload {
                EventPayload::SessionCreated {
                    usage_semantics, ..
                }
                | EventPayload::ProviderSwitched {
                    usage_semantics, ..
                } => {
                    if usage_semantics.is_some() {
                        semantics = *usage_semantics;
                    }
                }
                EventPayload::AssistantMessageEvent { message } => {
                    if let Some(u) = message.usage {
                        let legacy_sum = u.input_tokens
                            + u.cache_read_input_tokens.unwrap_or(0)
                            + u.cache_creation_input_tokens.unwrap_or(0);
                        last_input = Some(
                            semantics
                                .and_then(|s| s.prompt_total(&u))
                                .unwrap_or(legacy_sum),
                        );
                        acc = Some(match acc {
                            Some(prev) => prev + u,
                            None => u,
                        });
                    }
                }
                _ => {}
            }
        }
        (acc, last_input)
    }

    /// Total turns across all asks. Sums `Done.turn_count`.
    pub fn total_turn_count(&self) -> u32 {
        self.events
            .lock()
            .map(|events| {
                events
                    .iter()
                    .filter_map(|e| {
                        if let EventPayload::Done { turn_count, .. } = &e.payload {
                            Some(*turn_count)
                        } else {
                            None
                        }
                    })
                    .sum()
            })
            .unwrap_or(0)
    }

    /// Count of `ask_session` round-trips that terminated. Equals
    /// the number of `Done` events.
    pub fn ask_count(&self) -> u32 {
        self.events
            .lock()
            .map(|events| {
                events
                    .iter()
                    .filter(|e| matches!(e.payload, EventPayload::Done { .. }))
                    .count() as u32
            })
            .unwrap_or(0)
    }

    /// Sum of elapsed_ms across all asks. Excludes asks where the
    /// provider didn't report timing.
    pub fn elapsed_total_ms(&self) -> u64 {
        self.events
            .lock()
            .map(|events| {
                events
                    .iter()
                    .filter_map(|e| {
                        if let EventPayload::Done { elapsed_ms, .. } = &e.payload {
                            *elapsed_ms
                        } else {
                            None
                        }
                    })
                    .sum()
            })
            .unwrap_or(0)
    }

    /// Count of `ContextAssembly` events — equals the number of
    /// provider calls made during this session. Each model call has
    /// exactly one ContextAssembly (emitted before the call).
    pub fn context_assembly_count(&self) -> u32 {
        self.events
            .lock()
            .map(|events| {
                events
                    .iter()
                    .filter(|e| matches!(e.payload, EventPayload::ContextAssembly { .. }))
                    .count() as u32
            })
            .unwrap_or(0)
    }

    /// Count of tool invocations.
    pub fn tool_call_count(&self) -> u32 {
        self.events
            .lock()
            .map(|events| {
                events
                    .iter()
                    .filter(|e| matches!(e.payload, EventPayload::ToolCall { .. }))
                    .count() as u32
            })
            .unwrap_or(0)
    }

    /// mu-index-mark-column-auiv: the session's current operator mark —
    /// latest `OperatorMark` by event id (re-marking appends; latest
    /// wins). None if the session has never been marked.
    pub fn latest_operator_mark(&self) -> Option<(u8, Option<String>)> {
        let events = self.events.lock().ok()?;
        events.iter().rev().find_map(|ev| match &ev.payload {
            EventPayload::OperatorMark { rating, note } => Some((*rating, note.clone())),
            _ => None,
        })
    }

    /// Timestamp of the first event (typically `SessionCreated`).
    /// None if the log is empty.
    pub fn started_at_unix_ms(&self) -> Option<u64> {
        self.events
            .lock()
            .ok()
            .and_then(|events| events.first().map(|e| e.timestamp_unix_ms))
    }

    /// Timestamp of the most recent event. None if the log is empty.
    pub fn last_activity_unix_ms(&self) -> Option<u64> {
        self.events
            .lock()
            .ok()
            .and_then(|events| events.last().map(|e| e.timestamp_unix_ms))
    }

    /// mu-mh4: the set of event ids invalidated by `Tombstone` events
    /// in this log. Projections skip these ids (one rule, every
    /// consumer). Empty when no tombstones have been laid.
    pub fn tombstoned_ids(&self) -> std::collections::BTreeSet<u64> {
        self.events
            .lock()
            .map(|events| tombstoned_ids(&events))
            .unwrap_or_default()
    }

    /// Pull (provider_kind, model) out of the first SessionCreated
    /// event. None if no such event has been recorded (e.g. log was
    /// constructed manually without going through dispatch).
    pub fn provider_info(&self) -> Option<(String, String)> {
        let events = self.events.lock().ok()?;
        for ev in events.iter().rev() {
            match &ev.payload {
                EventPayload::ProviderSwitched {
                    new_provider_kind,
                    new_model,
                    ..
                } => return Some((new_provider_kind.to_string(), new_model.to_string())),
                EventPayload::SessionCreated {
                    provider_kind,
                    model,
                    ..
                } => return Some((provider_kind.clone(), model.clone())),
                _ => {}
            }
        }
        None
    }
}

/// mu-mh4: collect the set of event ids invalidated by `Tombstone`
/// events. Free function so both the live log and offline projections
/// (over a borrowed `&[SessionEvent]`) share one rule.
pub fn tombstoned_ids(events: &[SessionEvent]) -> std::collections::BTreeSet<u64> {
    events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::Tombstone {
                target_event_id, ..
            } => Some(*target_event_id),
            _ => None,
        })
        .collect()
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ContentBlock;
    use serde_json::json;

    fn sample_usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            cache_creation_5m_input_tokens: None,
            cache_creation_1h_input_tokens: None,
            reasoning_tokens: None,
        }
    }

    #[test]
    fn append_assigns_monotonic_ids() {
        let log = SessionEventLog::new("s1");
        let a = log.append(
            EventActor::User,
            EventPayload::UserMessage {
                content: "hi".into(),
            },
        );
        let b = log.append(
            EventActor::Agent,
            EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 1,
                usage: None,
                elapsed_ms: None,
            },
        );
        assert_eq!(a, 1);
        assert_eq!(b, 2);
        assert_eq!(log.len(), 2);
    }

    #[test]
    fn cumulative_usage_sums_done_events() {
        let log = SessionEventLog::new("s1");
        // First ask: 100 in, 50 out
        log.append(
            EventActor::Agent,
            EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 1,
                usage: Some(sample_usage(100, 50)),
                elapsed_ms: Some(500),
            },
        );
        // Second ask: 200 in, 75 out
        log.append(
            EventActor::Agent,
            EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 1,
                usage: Some(sample_usage(200, 75)),
                elapsed_ms: Some(800),
            },
        );
        // Third ask: no usage reported (e.g. provider hiccup)
        log.append(
            EventActor::Agent,
            EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 1,
                usage: None,
                elapsed_ms: Some(100),
            },
        );
        let cumulative = log.cumulative_usage().expect("at least one Done had usage");
        assert_eq!(cumulative.input_tokens, 300);
        assert_eq!(cumulative.output_tokens, 125);
        assert_eq!(log.ask_count(), 3);
        assert_eq!(log.elapsed_total_ms(), 500 + 800 + 100);
    }

    #[test]
    fn cumulative_usage_none_when_no_done_reported() {
        let log = SessionEventLog::new("s1");
        log.append(
            EventActor::User,
            EventPayload::UserMessage {
                content: "hi".into(),
            },
        );
        log.append(
            EventActor::Agent,
            EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 1,
                usage: None,
                elapsed_ms: None,
            },
        );
        assert!(log.cumulative_usage().is_none());
        assert_eq!(log.ask_count(), 1);
        assert_eq!(log.elapsed_total_ms(), 0);
    }

    #[test]
    fn context_assembly_count_filters_to_assembly_events() {
        let log = SessionEventLog::new("ca-count");
        log.append(
            EventActor::User,
            EventPayload::UserMessage {
                content: "hi".into(),
            },
        );
        log.append(
            EventActor::System,
            EventPayload::ContextAssembly {
                model_call_id: 1,
                message_count: 1,
                user_message_count: 1,
                assistant_message_count: 0,
                tool_result_count: 0,
                tool_count: 0,
                token_count_estimate: None,
                token_breakdown: Default::default(),
                provider_kind: "anthropic_api".into(),
                model: "x".into(),
                renderer: None,
                cache_strategy: None,
                span_count: None,
                cache_boundary_count: None,
                first_span_ids: Vec::new(),
                prefix_hash: None,
                prefix_span_hashes: Vec::new(),
            },
        );
        log.append(
            EventActor::Agent,
            EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 1,
                usage: None,
                elapsed_ms: Some(100),
            },
        );
        log.append(
            EventActor::System,
            EventPayload::ContextAssembly {
                model_call_id: 2,
                message_count: 3,
                user_message_count: 2,
                assistant_message_count: 1,
                tool_result_count: 0,
                tool_count: 0,
                token_count_estimate: None,
                token_breakdown: Default::default(),
                provider_kind: "anthropic_api".into(),
                model: "x".into(),
                renderer: None,
                cache_strategy: None,
                span_count: None,
                cache_boundary_count: None,
                first_span_ids: Vec::new(),
                prefix_hash: None,
                prefix_span_hashes: Vec::new(),
            },
        );
        assert_eq!(log.context_assembly_count(), 2);
        // Unaffected derivations.
        assert_eq!(log.ask_count(), 1);
    }

    #[test]
    fn context_assembly_payload_round_trips() -> Result<(), serde_json::Error> {
        let ev = SessionEvent {
            id: 1,
            session_id: "s".into(),
            parent_event_ids: vec![],
            timestamp_unix_ms: 0,
            actor: EventActor::System,
            payload: EventPayload::ContextAssembly {
                model_call_id: 7,
                message_count: 5,
                user_message_count: 2,
                assistant_message_count: 2,
                tool_result_count: 1,
                tool_count: 3,
                token_count_estimate: Some(2048),
                token_breakdown: Default::default(),
                provider_kind: "openai_codex".into(),
                model: "gpt-5.5".into(),
                renderer: None,
                cache_strategy: None,
                span_count: None,
                cache_boundary_count: None,
                first_span_ids: Vec::new(),
                prefix_hash: None,
                prefix_span_hashes: Vec::new(),
            },
        };
        let v = serde_json::to_value(&ev)?;
        assert_eq!(v["payload"]["kind"], "context_assembly");
        assert_eq!(v["payload"]["message_count"], 5);
        let decoded: SessionEvent = serde_json::from_value(v)?;
        assert_eq!(decoded, ev);
        Ok(())
    }

    #[test]
    fn tool_call_count_includes_only_tool_calls() {
        let log = SessionEventLog::new("s1");
        log.append(
            EventActor::User,
            EventPayload::UserMessage {
                content: "do it".into(),
            },
        );
        log.append(
            EventActor::Agent,
            EventPayload::ToolCall {
                call_id: "c1".into(),
                name: "read".into(),
                arguments: json!({"path": "/x"}),
            },
        );
        log.append(
            EventActor::Tool {
                name: "read".into(),
            },
            EventPayload::ToolResult {
                call_id: "c1".into(),
                content: "contents".into(),
                is_error: false,
            },
        );
        log.append(
            EventActor::Agent,
            EventPayload::ToolCall {
                call_id: "c2".into(),
                name: "edit".into(),
                arguments: json!({}),
            },
        );
        assert_eq!(log.tool_call_count(), 2);
    }

    #[test]
    fn total_turn_count_sums_done_turn_counts() {
        let log = SessionEventLog::new("s1");
        log.append(
            EventActor::Agent,
            EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 3,
                usage: None,
                elapsed_ms: None,
            },
        );
        log.append(
            EventActor::Agent,
            EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 1,
                usage: None,
                elapsed_ms: None,
            },
        );
        assert_eq!(log.total_turn_count(), 4);
    }

    #[test]
    fn session_event_round_trips() -> Result<(), serde_json::Error> {
        let samples = [
            SessionEvent {
                id: 1,
                session_id: "s1".into(),
                parent_event_ids: vec![],
                timestamp_unix_ms: 1_700_000_000_000,
                actor: EventActor::User,
                payload: EventPayload::UserMessage {
                    content: "hello".into(),
                },
            },
            SessionEvent {
                id: 2,
                session_id: "s1".into(),
                parent_event_ids: vec![1],
                timestamp_unix_ms: 1_700_000_000_500,
                actor: EventActor::Agent,
                payload: EventPayload::AssistantMessageEvent {
                    message: AssistantMessage {
                        content: vec![ContentBlock::Text {
                            text: "hi back".into(),
                        }],
                        stop_reason: StopReason::EndTurn,
                        usage: Some(sample_usage(42, 7)),
                    },
                },
            },
            SessionEvent {
                id: 3,
                session_id: "s1".into(),
                parent_event_ids: vec![],
                timestamp_unix_ms: 1_700_000_001_000,
                actor: EventActor::Tool {
                    name: "read".into(),
                },
                payload: EventPayload::ToolResult {
                    call_id: "c1".into(),
                    content: "file contents".into(),
                    is_error: false,
                },
            },
            SessionEvent {
                id: 4,
                session_id: "s1".into(),
                parent_event_ids: vec![],
                timestamp_unix_ms: 1_700_000_002_000,
                actor: EventActor::Agent,
                payload: EventPayload::Done {
                    stop_reason: StopReason::EndTurn,
                    turn_count: 2,
                    usage: Some(sample_usage(100, 50)),
                    elapsed_ms: Some(750),
                },
            },
            SessionEvent {
                id: 5,
                session_id: "s1".into(),
                parent_event_ids: vec![],
                timestamp_unix_ms: 1_700_000_003_000,
                actor: EventActor::System,
                payload: EventPayload::SessionClosed,
            },
        ];

        for ev in samples {
            let value = serde_json::to_value(&ev)?;
            let decoded: SessionEvent = serde_json::from_value(value)?;
            assert_eq!(decoded, ev);
        }
        Ok(())
    }

    #[test]
    fn payload_wire_tags_are_snake_case() -> Result<(), serde_json::Error> {
        let event = SessionEvent {
            id: 1,
            session_id: "s1".into(),
            parent_event_ids: vec![],
            timestamp_unix_ms: 0,
            actor: EventActor::Agent,
            payload: EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 1,
                usage: None,
                elapsed_ms: None,
            },
        };
        let v = serde_json::to_value(&event)?;
        assert_eq!(v["payload"]["kind"], "done");
        assert_eq!(v["actor"]["kind"], "agent");
        Ok(())
    }

    // ─── mu-rf9x: live_usage interprets under registered semantics ───

    use crate::agent::capabilities::UsageSemantics;

    fn append_session_created(log: &SessionEventLog, semantics: Option<UsageSemantics>) {
        log.append(
            EventActor::System,
            EventPayload::SessionCreated {
                provider_kind: "p".into(),
                model: "m".into(),
                parent_session_id: None,
                branched_at_parent_event_id: None,
                usage_semantics: semantics,
            },
        );
    }

    fn append_assistant_usage(log: &SessionEventLog, u: Usage) {
        log.append(
            EventActor::Agent,
            EventPayload::AssistantMessageEvent {
                message: crate::agent::AssistantMessage {
                    content: vec![ContentBlock::Text { text: "ok".into() }],
                    stop_reason: StopReason::EndTurn,
                    usage: Some(u),
                },
            },
        );
    }

    fn cached_usage(input: u64, cache_read: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: 10,
            cache_read_input_tokens: Some(cache_read),
            cache_creation_input_tokens: None,
            cache_creation_5m_input_tokens: None,
            cache_creation_1h_input_tokens: None,
            reasoning_tokens: None,
        }
    }

    /// The status-bar incident, end to end: codex semantics
    /// registered at SessionCreated → cache reads are NOT added on
    /// top of input_tokens. (Pre-fix this returned 93,209.)
    #[test]
    fn live_usage_openai_semantics_no_double_count() {
        let log = SessionEventLog::new("s1");
        append_session_created(&log, Some(UsageSemantics::openai_style()));
        append_assistant_usage(&log, cached_usage(55_577, 37_632));
        let (_, last_input) = log.live_usage();
        assert_eq!(last_input, Some(55_577));
    }

    #[test]
    fn live_usage_anthropic_semantics_sums_disjoint_buckets() {
        let log = SessionEventLog::new("s1");
        append_session_created(&log, Some(UsageSemantics::anthropic_style()));
        append_assistant_usage(&log, cached_usage(1_000, 20_000));
        let (_, last_input) = log.live_usage();
        assert_eq!(last_input, Some(21_000));
    }

    /// Pre-mu-rf9x logs (no declaration) keep the legacy additive
    /// sum — unchanged behavior for old sessions.
    #[test]
    fn live_usage_unstamped_log_falls_back_to_legacy_sum() {
        let log = SessionEventLog::new("s1");
        append_session_created(&log, None);
        append_assistant_usage(&log, cached_usage(55_577, 37_632));
        let (_, last_input) = log.live_usage();
        assert_eq!(last_input, Some(55_577 + 37_632));
    }

    /// A mid-session ProviderSwitched re-registers the convention:
    /// usage records after the switch are interpreted under the NEW
    /// provider's semantics.
    #[test]
    fn live_usage_provider_switch_changes_interpretation() {
        let log = SessionEventLog::new("s1");
        append_session_created(&log, Some(UsageSemantics::anthropic_style()));
        append_assistant_usage(&log, cached_usage(1_000, 20_000));
        assert_eq!(log.live_usage().1, Some(21_000), "anthropic: disjoint sum");

        log.append(
            EventActor::System,
            EventPayload::ProviderSwitched {
                old_provider_kind: "anthropic".into(),
                old_model: "m1".into(),
                new_provider_kind: "openai_codex".into(),
                new_model: "m2".into(),
                context_soft_limit: None,
                context_hard_limit: None,
                usage_semantics: Some(UsageSemantics::openai_style()),
            },
        );
        append_assistant_usage(&log, cached_usage(50_000, 40_000));
        assert_eq!(
            log.live_usage().1,
            Some(50_000),
            "post-switch: openai total, cache subset not re-added"
        );
    }

    // ─── mu-cache-write-tier-split-umq6: TaskTelemetry serde tests ───────────

    /// TaskTelemetry with tier fields present round-trips through JSON
    /// and the tier fields are serialized (non-None) while present.
    #[test]
    fn umq6_task_telemetry_tier_fields_roundtrip() {
        let payload = EventPayload::TaskTelemetry {
            task_id: "task-00000000000000000042".to_owned(),
            session_id: "s1".to_owned(),
            parent_task_id: None,
            provider_kind: "anthropic_api".to_owned(),
            model: "claude-opus-4-7".to_owned(),
            model_version: None,
            started_at_unix_ms: None,
            ended_at_unix_ms: 1_700_000_000_000,
            wall_clock_ms: Some(2000),
            prompt_tokens: Some(1000),
            completion_tokens: Some(50),
            cache_read_tokens: Some(100),
            cache_write_tokens: Some(800),
            cache_write_5m_tokens: Some(300),
            cache_write_1h_tokens: Some(500),
            tools_granted: vec![],
            tools_actually_called: vec![],
            exit_reason: TaskExitReason::Done,
            max_budget_usd: None,
            actual_spend_usd: None,
            local_hour: None,
            day_of_week: None,
            tz: None,
        };

        let json = serde_json::to_value(&payload).expect("TaskTelemetry serializes");

        // Tier fields appear in the JSON output.
        assert_eq!(json["cache_write_5m_tokens"], serde_json::json!(300));
        assert_eq!(json["cache_write_1h_tokens"], serde_json::json!(500));

        // Roundtrip: deserialize back and the fields survive.
        let rt: EventPayload = serde_json::from_value(json.clone()).expect("deserializes back");
        match rt {
            EventPayload::TaskTelemetry {
                cache_write_5m_tokens,
                cache_write_1h_tokens,
                ..
            } => {
                assert_eq!(cache_write_5m_tokens, Some(300));
                assert_eq!(cache_write_1h_tokens, Some(500));
            }
            other => panic!("expected TaskTelemetry, got {other:?}"),
        }
    }

    /// Old events without tier fields deserialize with None (serde default).
    /// Tests the backwards-compat contract — this is the "no migration" guarantee.
    #[test]
    fn umq6_task_telemetry_old_event_no_tier_fields_deserializes_as_none() {
        // JSON that represents a pre-umq6 TaskTelemetry (no tier fields).
        // EventPayload uses `tag = "kind"` in snake_case.
        let old_json = serde_json::json!({
            "kind": "task_telemetry",
            "task_id": "task-00000000000000000001",
            "session_id": "s1",
            "provider_kind": "anthropic_api",
            "model": "claude-opus-4-7",
            "ended_at_unix_ms": 1_700_000_000_000u64,
            "exit_reason": "done",
            "cache_write_tokens": 200
            // no cache_write_5m_tokens / cache_write_1h_tokens fields
        });
        let payload: EventPayload =
            serde_json::from_value(old_json).expect("old event deserializes");
        match payload {
            EventPayload::TaskTelemetry {
                cache_write_tokens,
                cache_write_5m_tokens,
                cache_write_1h_tokens,
                ..
            } => {
                assert_eq!(cache_write_tokens, Some(200), "flat total preserved");
                assert!(
                    cache_write_5m_tokens.is_none(),
                    "absent tier field defaults to None"
                );
                assert!(cache_write_1h_tokens.is_none());
            }
            other => panic!("expected TaskTelemetry, got {other:?}"),
        }
    }

    /// When tier fields are None, they are NOT serialized (skip_serializing_if
    /// = "Option::is_none"). This keeps old-style event JSON compact.
    #[test]
    fn umq6_task_telemetry_none_tier_fields_not_serialized() {
        let payload = EventPayload::TaskTelemetry {
            task_id: "task-00000000000000000001".to_owned(),
            session_id: "s1".to_owned(),
            parent_task_id: None,
            provider_kind: "anthropic_api".to_owned(),
            model: "claude-opus-4-7".to_owned(),
            model_version: None,
            started_at_unix_ms: None,
            ended_at_unix_ms: 1_700_000_000_000,
            wall_clock_ms: None,
            prompt_tokens: None,
            completion_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: Some(100),
            cache_write_5m_tokens: None,
            cache_write_1h_tokens: None,
            tools_granted: vec![],
            tools_actually_called: vec![],
            exit_reason: TaskExitReason::Done,
            max_budget_usd: None,
            actual_spend_usd: None,
            local_hour: None,
            day_of_week: None,
            tz: None,
        };
        let json = serde_json::to_string(&payload).expect("serializes");
        assert!(
            !json.contains("cache_write_5m_tokens"),
            "absent tier field must not appear in JSON: {json}"
        );
        assert!(
            !json.contains("cache_write_1h_tokens"),
            "absent tier field must not appear in JSON: {json}"
        );
    }

    /// mu-operator-mark-5mwr: the on-disk shape is the contract with
    /// the mu-mucm `session_marks` projection — kind tag and field
    /// names are load-bearing, and an absent note must not appear.
    #[test]
    fn operator_mark_roundtrip_and_wire_shape() {
        let payload = EventPayload::OperatorMark {
            rating: 2,
            note: Some("relitigated settled decisions".into()),
        };
        let json = serde_json::to_string(&payload).expect("serializes");
        assert!(json.contains(r#""kind":"operator_mark""#), "{json}");
        assert!(json.contains(r#""rating":2"#), "{json}");
        let back: EventPayload = serde_json::from_str(&json).expect("roundtrips");
        assert_eq!(back, payload);

        let bare = serde_json::to_string(&EventPayload::OperatorMark {
            rating: 5,
            note: None,
        })
        .expect("serializes");
        assert!(
            !bare.contains("note"),
            "absent note must be skipped: {bare}"
        );

        // Appends like any other event; latest-by-id is the re-mark rule.
        let log = SessionEventLog::new("s1");
        log.append(
            EventActor::User,
            EventPayload::OperatorMark {
                rating: 2,
                note: None,
            },
        );
        let id = log.append(
            EventActor::User,
            EventPayload::OperatorMark {
                rating: 3,
                note: Some("on reflection".into()),
            },
        );
        assert_eq!(id, 2);
    }

    // ─── spec mu-046: command/receipt variants + append_command ───

    use crate::command_journal::{AuthSnapshot, CommandEcho, Origin, RejectStage};

    fn sample_command_received() -> EventPayload {
        EventPayload::CommandReceived {
            request_id: json!(7),
            method: "session.ask".into(),
            params: json!({"prompt": "hi"}),
            auth: AuthSnapshot::Authenticated,
            origin: Origin {
                transport: "stdio".into(),
                connection_id: Some("c1".into()),
            },
        }
    }

    fn sample_echo() -> CommandEcho {
        CommandEcho {
            request_id: json!(7),
            method: "session.ask".into(),
            params: json!({"prompt": "hi"}),
        }
    }

    /// In-memory-only logs cannot make a command durable, and silently
    /// succeeding would forge the write-ahead guarantee — the policy
    /// is an explicit `Unsupported` error.
    #[test]
    fn append_command_errors_without_disk_writer() {
        let log = SessionEventLog::new("s1");
        let err = log
            .append_command(EventActor::User, sample_command_received())
            .expect_err("no disk writer must be an error, never a silent success");
        assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
        // Nothing reached memory and no id was consumed: the failed
        // command left no trace, matching fail-closed semantics.
        assert!(log.is_empty());
        assert_eq!(
            log.append(EventActor::System, EventPayload::SessionClosed),
            1
        );
    }

    /// The strict path writes to disk BEFORE the in-memory push: after
    /// append_command returns, the JSONL line exists and `from_jsonl`
    /// rebuilds the same event the snapshot holds.
    #[test]
    fn append_command_disk_write_precedes_memory_and_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("s1.jsonl");
        let log = SessionEventLog::new("s1");
        log.attach_disk_writer(&path).expect("attach");

        let received_id = log
            .append_command(EventActor::User, sample_command_received())
            .expect("append_command");
        assert_eq!(received_id, 1);
        let receipt_id = log
            .append_command(
                EventActor::System,
                EventPayload::CommandSucceeded {
                    command_event_id: received_id,
                    command: sample_echo(),
                    result: json!({"accepted": true}),
                    elapsed_ms: 3,
                },
            )
            .expect("append receipt");
        assert_eq!(receipt_id, 2);

        // Disk holds both lines (durable, fsync'd) …
        let (recovered, malformed) = SessionEventLog::from_jsonl(&path).expect("from_jsonl");
        assert_eq!(malformed, 0);
        assert_eq!(recovered.snapshot(), log.snapshot());
        // … and the recovered payloads are the command variants.
        assert_eq!(recovered.snapshot()[0].payload, sample_command_received());
    }

    /// mu-kgpg: strict command appends and normal best-effort appends
    /// share one append-order lock, so snapshots and JSONL replay stay
    /// sorted by event id even when both paths race.
    #[test]
    fn mixed_append_paths_keep_id_snapshot_and_disk_order_aligned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("s-mixed.jsonl");
        let log = std::sync::Arc::new(SessionEventLog::new("s-mixed"));
        log.attach_disk_writer(&path).expect("attach");

        let mut threads = Vec::new();
        for worker in 0..8 {
            let log = std::sync::Arc::clone(&log);
            threads.push(std::thread::spawn(move || {
                for n in 0..100 {
                    if (worker + n) % 2 == 0 {
                        let _ = log
                            .append_command(EventActor::User, sample_command_received())
                            .expect("strict command append");
                    } else {
                        log.append(
                            EventActor::System,
                            EventPayload::OperatorMark {
                                rating: ((worker % 5) + 1) as u8,
                                note: Some(format!("worker {worker} event {n}")),
                            },
                        );
                    }
                }
            }));
        }
        for thread in threads {
            thread.join().expect("append worker");
        }

        let snapshot = log.snapshot();
        assert_eq!(snapshot.len(), 800);
        for (idx, event) in snapshot.iter().enumerate() {
            assert_eq!(event.id, (idx + 1) as u64, "snapshot order drifted");
        }

        let (recovered, malformed) = SessionEventLog::from_jsonl(&path).expect("from_jsonl");
        assert_eq!(malformed, 0);
        let recovered = recovered.snapshot();
        assert_eq!(recovered.len(), snapshot.len());
        for (idx, event) in recovered.iter().enumerate() {
            assert_eq!(event.id, (idx + 1) as u64, "disk order drifted");
        }
    }

    /// The four mu-046 variants' wire shape, pinned: internally tagged
    /// `kind` in snake_case, shared border types in their snake_case
    /// forms.
    #[test]
    fn command_variants_jsonl_round_trip() -> Result<(), serde_json::Error> {
        let samples = [
            (sample_command_received(), "command_received"),
            (
                EventPayload::CommandSucceeded {
                    command_event_id: 1,
                    command: sample_echo(),
                    result: json!({"ok": true}),
                    elapsed_ms: 10,
                },
                "command_succeeded",
            ),
            (
                EventPayload::CommandFailed {
                    command_event_id: 1,
                    command: sample_echo(),
                    code: -32603,
                    message: "provider exploded".into(),
                    elapsed_ms: 10,
                },
                "command_failed",
            ),
            (
                EventPayload::CommandRejected {
                    command_event_id: 1,
                    command: sample_echo(),
                    code: -32600,
                    message: "not yours".into(),
                    stage: RejectStage::AuthGate,
                },
                "command_rejected",
            ),
        ];
        for (payload, kind) in samples {
            let v = serde_json::to_value(&payload)?;
            assert_eq!(v["kind"], kind);
            let decoded: EventPayload = serde_json::from_value(v)?;
            assert_eq!(decoded, payload);
        }
        // Shared border types ride along in snake_case.
        let v = serde_json::to_value(sample_command_received())?;
        assert_eq!(v["auth"], "authenticated");
        assert_eq!(v["origin"]["transport"], "stdio");
        Ok(())
    }

    /// `kind_str` must agree with serde's `kind` tag — it exists so
    /// consumers print ONE authoritative string instead of re-matching
    /// the enum. A representative set of variants is pinned here; the
    /// exhaustive match in `kind_str` covers the rest by construction.
    #[test]
    fn kind_str_matches_serde_tag_for_representative_variants() -> Result<(), serde_json::Error> {
        let samples = [
            EventPayload::SessionCreated {
                provider_kind: "mock".into(),
                model: "m1".into(),
                parent_session_id: None,
                branched_at_parent_event_id: None,
                usage_semantics: None,
            },
            EventPayload::UserMessage {
                content: "hi".into(),
            },
            EventPayload::ToolCall {
                call_id: "c1".into(),
                name: "read_file".into(),
                arguments: json!({"path": "/tmp/x"}),
            },
            EventPayload::ToolResult {
                call_id: "c1".into(),
                content: "ok".into(),
                is_error: false,
            },
            EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 1,
                usage: Some(sample_usage(1, 2)),
                elapsed_ms: Some(10),
            },
            EventPayload::Error {
                message: "boom".into(),
            },
            EventPayload::Tombstone {
                target_event_id: 1,
                reason: "incomplete record".into(),
            },
            sample_command_received(),
            EventPayload::CommandSucceeded {
                command_event_id: 1,
                command: sample_echo(),
                result: json!({"ok": true}),
                elapsed_ms: 10,
            },
            EventPayload::CommandFailed {
                command_event_id: 1,
                command: sample_echo(),
                code: -32603,
                message: "provider exploded".into(),
                elapsed_ms: 10,
            },
            EventPayload::CommandRejected {
                command_event_id: 1,
                command: sample_echo(),
                code: -32600,
                message: "not yours".into(),
                stage: RejectStage::AuthGate,
            },
            EventPayload::SessionClosed,
        ];
        for payload in samples {
            let v = serde_json::to_value(&payload)?;
            assert_eq!(
                v["kind"],
                payload.kind_str(),
                "kind_str drifted from the serde tag for {payload:?}"
            );
        }
        Ok(())
    }

    /// Existing best-effort append is byte-for-byte unchanged: it
    /// still returns a bare u64 and still swallows disk-less appends.
    #[test]
    fn plain_append_still_best_effort_for_command_variants() {
        let log = SessionEventLog::new("s1");
        // No disk writer: plain append happily records in memory only —
        // the strictness lives in append_command, not in the variant.
        let id = log.append(EventActor::User, sample_command_received());
        assert_eq!(id, 1);
        assert_eq!(log.len(), 1);
    }
}
