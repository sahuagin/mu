use serde::{Deserialize, Serialize};
use serde_json::Value;

// mu-6a8: extracted submodules. Re-exported below so external callers
// (`use mu_core::protocol::{X};`) see no API change. The remaining
// in-file sections (jsonrpc envelope, session, stats, autonomy, mu-035
// provider-status, events) are extraction targets for follow-up phases.
mod auth;
mod mailbox;
mod stats;
pub use auth::*;
pub use mailbox::*;
pub use stats::*;

// ===== JSON-RPC 2.0 envelope =====

pub const JSONRPC_VERSION: &str = "2.0";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request<P> {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    pub params: P,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Response<R> {
    Ok {
        jsonrpc: String,
        id: Value,
        result: R,
    },
    Err {
        jsonrpc: String,
        id: Value,
        error: ErrorObject,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification<P> {
    pub jsonrpc: String,
    pub method: String,
    pub params: P,
}

// ===== Methods =====

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PingRequest;

impl PingRequest {
    pub const METHOD: &'static str = "ping";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PingResponse {
    pub pong: bool,
    pub server_version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    pub provider: ProviderSelector,
    /// Optional system prompt override. None → daemon default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

impl CreateSessionRequest {
    pub const METHOD: &'static str = "create_session";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateSessionResponse {
    pub session_id: String,
}

/// Provider selection at session-create time. Tagged enum so the wire
/// format is `{ "kind": "anthropic_api", "model": "claude-..." }`.
///
/// As of mu-019, `openai_codex` is the canonical name for OAuth-based
/// access to OpenAI via the Codex backend. Earlier protocol drafts
/// used `openai_oauth`; the rename happened when mu started talking
/// to `chatgpt.com/backend-api/codex/responses` directly instead of
/// shelling out to pi.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderSelector {
    AnthropicApi { model: String },
    AnthropicOauth { model: String },
    OpenaiApi { model: String },
    OpenaiCodex { model: String },
    Openrouter { model: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AskSessionRequest {
    pub session_id: String,
    pub user_message: String,
}

impl AskSessionRequest {
    pub const METHOD: &'static str = "ask_session";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AskSessionResponse {
    /// Acknowledgement that the request was accepted; the actual content
    /// is delivered via `session.*` notifications. Final terminator is
    /// the `session.done` notification.
    pub accepted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelSessionRequest {
    pub session_id: String,
}

impl CancelSessionRequest {
    pub const METHOD: &'static str = "cancel_session";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelSessionResponse {
    pub cancelled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloseSessionRequest {
    pub session_id: String,
}

impl CloseSessionRequest {
    pub const METHOD: &'static str = "close_session";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloseSessionResponse {
    pub closed: bool,
}

/// Query a session's running totals (mu-027). The result is a
/// snapshot, derived from the session's durable event log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatsRequest {
    pub session_id: String,
}

impl SessionStatsRequest {
    pub const METHOD: &'static str = "session.stats";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatsResponse {
    pub session_id: String,
    /// Provider kind from the wire protocol (e.g. "openai_codex").
    /// None if no SessionCreated event has been recorded (shouldn't
    /// happen in normal use; defensive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Unix ms of the first event (typically SessionCreated).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at_unix_ms: Option<u64>,
    /// Unix ms of the most recent event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity_unix_ms: Option<u64>,
    /// Total event count in the log.
    pub event_count: u32,
    /// Number of completed ask_session round-trips.
    pub ask_count: u32,
    /// Sum of Done.turn_count across all asks.
    pub total_turn_count: u32,
    /// Number of tool invocations.
    pub tool_call_count: u32,
    /// Sum of Done.elapsed_ms across all asks.
    pub elapsed_total_ms: u64,
    /// Aggregated usage across all asks. None if no Done event
    /// reported usage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::agent::Usage>,
}

// ===== mu-038: projection queries (session.list, session.events, daemon.stats) =====

/// Filter for `session.list`. All fields optional; default = "all
/// local, no limit." Forward-compat additive: new fields added in
/// future revisions can be ignored by older daemons.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionListFilter {
    /// Include sessions from peer daemons (requires a federating
    /// SessionDiscovery backend like FileBackend or EtcdBackend).
    /// LocalRegistryBackend ignores this flag — it only sees local.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_remote: bool,
    /// Only sessions whose parent_session_id matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Only sessions in the given status. Default = any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<SessionStatusSummary>,
    /// Only sessions with last_activity_unix_ms >= this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_since_unix_ms: Option<u64>,
    /// Cap response size. 0 or None ⇒ no limit (use cautiously).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Derived summary of where a session is in its lifecycle. Computed
/// from the session's event log (post-mu-035, the live
/// ProviderStatusTracker is authoritative for local sessions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatusSummary {
    /// No ask in flight; last event was Done/SessionClosed or the
    /// log is empty.
    Idle,
    /// User message arrived; model call may or may not have started.
    Asking,
    /// Model is producing text (text_delta-style activity within the
    /// last ~5s).
    Streaming,
    /// A tool call is in flight (started but not yet completed).
    ToolExecuting,
    /// A session.input_required notification is outstanding; the
    /// session is blocked on a client approve/deny.
    AwaitingInputRequired,
    /// Last completed ask ended cleanly.
    Done,
    /// Last event was Error.
    Errored,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    /// Stable per-daemon identifier (UUID generated at startup). Used
    /// by federating discovery backends to disambiguate sessions
    /// across daemons.
    pub daemon_id: String,
    /// True iff this session is in a peer daemon (only ever true with
    /// include_remote + a federating backend).
    pub is_remote: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    pub provider_kind: String,
    pub model: String,
    pub status: SessionStatusSummary,
    pub started_at_unix_ms: u64,
    pub last_activity_unix_ms: u64,
    pub ask_count: u32,
    pub tool_call_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cumulative_usage: Option<crate::agent::Usage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionListRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<SessionListFilter>,
}

impl SessionListRequest {
    pub const METHOD: &'static str = "session.list";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionListResponse {
    pub sessions: Vec<SessionInfo>,
    pub snapshot_at_unix_ms: u64,
    /// Set when `include_remote=true` and one or more peer daemons
    /// failed to respond. Local results are still included.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_peers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEventsRequest {
    pub session_id: String,
    /// Resume cursor from a prior page. Returns events with id > this
    /// value. Omit to start from the beginning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_event_id: Option<u64>,
    /// Cap response size. None or 0 ⇒ a sensible default (200).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Restrict to specific payload kinds (e.g. ["text_delta",
    /// "tool_call"]). Empty/omitted ⇒ all kinds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds_filter: Vec<String>,
}

impl SessionEventsRequest {
    pub const METHOD: &'static str = "session.events";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEventsResponse {
    /// Already-serialised SessionEvent values (see event_log.rs for
    /// the shape). Returned as serde_json::Value so wire consumers
    /// can decode lazily without depending on mu-core types.
    pub events: Vec<serde_json::Value>,
    /// Cursor for the next page. None when end_of_log is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_event_id: Option<u64>,
    pub end_of_log: bool,
}

// ===== mu-036: autonomous session loop ============================
//
// Two new RPCs and a small constellation of typed events for the
// "session runs without a human in the loop" primitive. See
// specs/mu-036-session-autonomous-loop.md for design intent.
//
// Phase A (this slice): the wire surface. Dispatch handlers return
// a "not-yet-implemented" error until Phase B (mu-3ao) lands the
// agent-loop integration. The types are stable enough for clients
// to start coding against.

/// Request to put `session_id` into autonomous mode with `goal` and
/// bounds in `options`. The daemon validates the session's
/// capability includes `AutonomyCapability::Allowed` (mu-036 INV-1);
/// if not, returns an error. The *real* bounds enforcement uses the
/// capability's values — not these options — so a delegate cannot
/// widen its autonomy by passing bigger numbers (INV-2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartAutonomousRequest {
    pub session_id: String,
    pub goal: String,
    pub options: AutonomyOptions,
}

impl StartAutonomousRequest {
    pub const METHOD: &'static str = "session.start_autonomous";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartAutonomousResponse {
    pub accepted: bool,
}

/// Per-call autonomy preferences. The capability is the bound; these
/// options refine within it. All are optional because the capability's
/// values are the authoritative ceiling.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutonomyOptions {
    /// Soft cap on iterations. Daemon also enforces the
    /// `Capability::autonomy::max_iterations` ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<u32>,
    /// How often to run the goal-check. 1 = every iteration.
    /// Higher numbers trade responsiveness for cost (especially
    /// for `DelegateGrader`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_check_interval: Option<u32>,
    /// How the loop decides it's done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_check_method: Option<GoalCheckMethod>,
    /// If no progress (no tool call, no streaming) for this long,
    /// emit `session.input_required` to ask the human for guidance.
    /// None ⇒ no escalation timer (loop runs until a bound trips).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalate_on_idle_after_ms: Option<u64>,
}

/// How an autonomous loop decides whether the goal is met.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tag", rename_all = "snake_case")]
pub enum GoalCheckMethod {
    /// Agent emits `session.callout { kind: "goal_status", body:
    /// { satisfied: bool, reason: String } }` at end of each
    /// iteration; loop terminates when satisfied: true.
    SelfReport,
    /// Between iterations, ask a sibling/delegate session to grade.
    /// Constrains the grader's response shape via the prompt template.
    DelegateGrader {
        grader_session_id: String,
        grader_prompt_template: String,
    },
    /// Wait for a `session.external_signal` notification with
    /// matching `signal_name`. Useful for "stop when CI passes."
    ExternalSignal { signal_name: String },
}

/// Outcome of one autonomous iteration, recorded in the event log
/// and surfaced via `session.autonomous_iteration_completed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tag", rename_all = "snake_case")]
pub enum AutonomousIterationOutcome {
    /// Goal not yet met; loop continues to the next iteration.
    Continue,
    /// Goal met; loop terminates with `AutonomousTerminated { reason:
    /// GoalMet }` next.
    GoalMet { detail: String },
    /// Iteration failed (e.g. tool error, grader timeout). Loop
    /// terminates with `AutonomousTerminated { reason: IterationError }`.
    IterationError { message: String },
    /// Escalation tripped — the loop emitted `session.input_required`
    /// and is awaiting a human response.
    EscalatingToHuman,
}

/// Why the autonomous loop terminated. Always the final event for
/// this autonomous run (INV-7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tag", rename_all = "snake_case")]
pub enum AutonomousTerminationReason {
    GoalMet {
        detail: String,
    },
    IterationCap,
    WallClockExpired,
    ToolCallCapExhausted,
    EscalationTimedOut,
    GraderRejected {
        detail: String,
    },
    /// Externally cancelled via session.cancel_outstanding or
    /// session.cancel_session.
    Cancelled,
    /// Provider or tool error mid-loop that wasn't recoverable.
    Errored {
        message: String,
    },
}

/// Request to park the session for `sleep_for_ms` (or until
/// `wake_at_unix_ms`). Exactly one of the two must be set.
/// While sleeping, the session does not consume model budget
/// (INV-5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleWakeupRequest {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sleep_for_ms: Option<u64>,
    /// Free-form reason. Recorded in the event log and surfaced as
    /// the next iteration's `motivation` field after wake.
    pub reason: String,
}

impl ScheduleWakeupRequest {
    pub const METHOD: &'static str = "session.schedule_wakeup";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleWakeupResponse {
    pub accepted: bool,
    pub scheduled_for_unix_ms: u64,
}

/// `session.autonomous_iteration_started` notification (mu-036). Emitted
/// at the top of every autonomous iteration. `motivation` is the
/// model-reported one-sentence "what I'm doing this turn and why"
/// (after a `schedule_wakeup`, this is the wake reason).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutonomousIterationStartedEvent {
    pub session_id: String,
    pub iteration: u32,
    pub motivation: String,
}

impl AutonomousIterationStartedEvent {
    pub const METHOD: &'static str = "session.autonomous_iteration_started";
}

/// `session.autonomous_iteration_completed` notification (mu-036).
/// Emitted at the end of every autonomous iteration with the outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutonomousIterationCompletedEvent {
    pub session_id: String,
    pub iteration: u32,
    pub outcome: AutonomousIterationOutcome,
}

impl AutonomousIterationCompletedEvent {
    pub const METHOD: &'static str = "session.autonomous_iteration_completed";
}

/// `session.autonomous_terminated` notification (mu-036). Always the
/// final autonomy event for a run (INV-7). Session returns to
/// RunMode::Idle and is addressable via ask_session again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutonomousTerminatedEvent {
    pub session_id: String,
    pub reason: AutonomousTerminationReason,
}

impl AutonomousTerminatedEvent {
    pub const METHOD: &'static str = "session.autonomous_terminated";
}

// ===== mu-035: session.provider_status + session.cancel_outstanding =====

/// Provider-call lifecycle states surfaced to clients (mu-035). Tags
/// stable; future additions are additive. Serialized snake_case to
/// match the rest of the wire surface.
///
/// State transitions roughly:
///   AwaitingFirstToken → Streaming  (first content token)
///   AwaitingFirstToken → Thinking   (provider opens stream but stays quiet)
///   Streaming → Thinking            (gap > idle_threshold_ms mid-stream)
///   Streaming → ToolExecuting       (model decides to call a tool)
///   ToolExecuting → AwaitingToolResult (tool dispatched, awaiting result)
///   AwaitingToolResult → Streaming  (next assistant turn begins)
///   * → Idle                        (session.done landed)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStatusKind {
    Idle,
    AwaitingFirstToken,
    Streaming,
    Thinking,
    ToolExecuting,
    AwaitingToolResult,
}

/// `session.provider_status` notification payload (mu-035). Emitted
/// periodically while the agent loop is in a non-streaming wait, and
/// on every state transition. Cumulative wall-clock per call is
/// computable by summing `elapsed_ms` across consecutive
/// ProviderStatusEvents for the same session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderStatusEvent {
    pub session_id: String,
    pub kind: ProviderStatusKind,
    /// Unix milliseconds when the session entered this kind.
    pub started_at_unix_ms: u64,
    /// Milliseconds since `started_at_unix_ms`. Re-emitted in periodic
    /// ticks (Phase B) so a watching client sees the value advance.
    pub elapsed_ms: u64,
    /// Bytes received from the provider's SSE stream so far (cumulative
    /// for this turn). None when not meaningful (Idle, AwaitingFirstToken
    /// before any bytes, or providers that don't surface byte counts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_received: Option<u64>,
    /// Set only when `kind` is ToolExecuting or AwaitingToolResult.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// Cancel the **outstanding provider call** for a session without
/// ending the session itself (mu-035). The agent loop aborts the
/// in-flight stream and surfaces a CancelOutstanding outcome to the
/// loop's outer driver, which decides what to do next (retry on the
/// same provider, fall over to a different one, surface to a human).
///
/// Distinct from `cancel_session`: that ends the session. This kills
/// just the current provider call; the session is still addressable
/// via `ask_session` immediately after.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelOutstandingRequest {
    pub session_id: String,
    /// Free-form reason for the cancel. Logged in the event log; not
    /// otherwise interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl CancelOutstandingRequest {
    pub const METHOD: &'static str = "session.cancel_outstanding";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelOutstandingResponse {
    /// True iff a provider call was actually in flight at the time of
    /// the request. False (with `was_in: Idle`) when the call is a
    /// no-op because nothing was outstanding.
    pub canceled: bool,
    pub was_in: ProviderStatusKind,
}

/// Create a new "child" session that's lineage-aware of `parent_session_id`
/// (mu-031). The child session is fully independent at the runtime
/// level — own agent loop, own event log, own pending-approvals
/// registry — but carries a reference to its parent for audit, and
/// optionally a narrowed `Capability` derived from the parent's
/// (mu-033). v1: the child starts with empty message history;
/// `branched_at_parent_event_id` is recorded for audit/replay but
/// doesn't affect runtime state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelegateSessionRequest {
    pub parent_session_id: String,
    /// Provider for the child. Independent of the parent's — a child
    /// can use a different provider/model than its parent.
    pub provider: ProviderSelector,
    /// Optional: which event in the parent's log this branched from.
    /// For audit; v1 doesn't act on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branched_at_parent_event_id: Option<u64>,
    /// Optional capability attenuations (mu-033). The child's
    /// effective capability is the intersection of the parent's
    /// capability with this. Any field omitted is "no further
    /// narrowing on this axis from this request." If absent
    /// entirely, the child inherits the parent's capability
    /// unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attenuations: Option<crate::capability::CapabilityAttenuations>,
}

impl DelegateSessionRequest {
    pub const METHOD: &'static str = "session.delegate";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelegateSessionResponse {
    pub child_session_id: String,
}

/// Respond to an outstanding `session.input_required` notification
/// (mu-029). The daemon blocks the corresponding tool call until
/// the client sends this back. `request_id` identifies which prompt
/// is being answered; `decision` is approve or deny.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RespondToInputRequiredRequest {
    pub session_id: String,
    pub request_id: String,
    pub decision: ApprovalDecision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approve,
    Deny,
}

impl RespondToInputRequiredRequest {
    pub const METHOD: &'static str = "session.respond_to_input_required";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RespondToInputRequiredResponse {
    /// True if the daemon found the pending request and relayed
    /// the decision. False if the request_id was unknown (already
    /// answered, timed out, or never existed).
    pub accepted: bool,
}

// ===== Event notifications (daemon → frontend) =====

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextDeltaEvent {
    pub session_id: String,
    pub delta: String,
}

impl TextDeltaEvent {
    pub const METHOD: &'static str = "session.text_delta";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantTextFinalizedEvent {
    pub session_id: String,
    /// The final assembled text from the assistant message. This text
    /// matches what will appear in the durable AssistantMessageEvent.
    /// Emitted when streaming completes, before session.done, allowing
    /// clients to swap from streaming-text to finalized text atomically.
    /// See mu-wk2.
    pub text: String,
}

impl AssistantTextFinalizedEvent {
    pub const METHOD: &'static str = "session.assistant_text_finalized";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallStartedEvent {
    pub session_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: Value,
}

impl ToolCallStartedEvent {
    pub const METHOD: &'static str = "session.tool_call_started";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallCompletedEvent {
    pub session_id: String,
    pub tool_call_id: String,
    /// `Ok(result)` or `Err(message)` — both shapes serialize as a
    /// tagged enum so the frontend can render them differently.
    pub outcome: ToolOutcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolOutcome {
    Ok { result: Value },
    Err { message: String },
}

impl ToolCallCompletedEvent {
    pub const METHOD: &'static str = "session.tool_call_completed";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoneEvent {
    pub session_id: String,
    /// Why the loop ended — EndTurn, ToolUse (shouldn't see this on
    /// wire — Done means the chain is over), MaxTokens, Error, Aborted.
    pub stop_reason: crate::agent::StopReason,
    /// Aggregated token usage across this ask_session's turns.
    /// None means no provider in the chain reported usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::agent::Usage>,
    /// Wall time from the first turn's start to this Done emit, in
    /// milliseconds. None for clean-shutdown Dones where no turns ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

impl DoneEvent {
    pub const METHOD: &'static str = "session.done";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorEvent {
    pub session_id: String,
    pub message: String,
    /// Optional structured detail; provider-specific.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

impl ErrorEvent {
    pub const METHOD: &'static str = "session.error";
}

/// Daemon→client: "the agent is about to call this tool; should it?"
/// Emitted when a tool's policy says `PermissionLevel::Ask` (or AskOnce
/// on its first invocation per session). The daemon blocks dispatch
/// until a matching `session.respond_to_input_required` arrives.
/// See spec mu-029.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputRequiredEvent {
    pub session_id: String,
    /// Token to match in the corresponding response. Unique per
    /// pending prompt; the daemon-side registry is keyed on this.
    pub request_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: Value,
    /// Why the agent is asking — typically just a short summary of
    /// the tool + arguments rendered for the human. Frontends are
    /// free to show their own UI; this is a fallback.
    pub summary: String,
}

impl InputRequiredEvent {
    pub const METHOD: &'static str = "session.input_required";
}

/// Catch-all "the agent has something notable to say" notification.
/// Free-form `kind` and optional `theme` let new categories be added
/// without protocol changes. See spec mu-016. Documented starter
/// `kind` set: `info`, `status`, `observation`, `hint`, `warning`,
/// `memory`, `peer_message`. Documented starter `theme` set: `info`,
/// `muted`, `warning`, `danger`, `success`. Frontends fall back to
/// defaults for unknown values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalloutEvent {
    pub session_id: String,
    pub kind: String,
    pub title: String,
    pub body: CalloutBody,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// References to durable artifacts (spec IDs, memory IDs,
    /// code-index paths, beads). Body should be terse; refs let
    /// consumers fetch full context.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_refs: Vec<String>,
}

/// `CalloutEvent.body` shape. `Text` for simple cases; `Structured`
/// for richer payloads frontends may render specially.
///
/// Untagged: a Text body encodes as a bare string, a Structured
/// body as a JSON object/array/etc. This means deserializing a
/// string-as-Structured is impossible — strings always come back as
/// Text. That's intentional.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CalloutBody {
    Text(String),
    Structured(Value),
}

impl CalloutEvent {
    pub const METHOD: &'static str = "session.callout";
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn round_trip_request() -> Result<(), serde_json::Error> {
        let request = Request {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: json!(1),
            method: PingRequest::METHOD.to_owned(),
            params: PingRequest,
        };

        let value = serde_json::to_value(&request)?;
        let decoded: Request<PingRequest> = serde_json::from_value(value)?;

        assert_eq!(decoded, request);
        Ok(())
    }

    #[test]
    fn round_trip_response_ok() -> Result<(), serde_json::Error> {
        let response = Response::Ok {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: json!("req-1"),
            result: PingResponse {
                pong: true,
                server_version: "0.1.0".to_owned(),
            },
        };

        let value = serde_json::to_value(&response)?;
        let decoded: Response<PingResponse> = serde_json::from_value(value)?;

        assert_eq!(decoded, response);
        Ok(())
    }

    #[test]
    fn round_trip_response_err() -> Result<(), serde_json::Error> {
        let response: Response<()> = Response::Err {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: json!("req-2"),
            error: ErrorObject {
                code: -32601,
                message: "method not found".to_owned(),
                data: Some(json!({ "method": "missing" })),
            },
        };

        let value = serde_json::to_value(&response)?;
        let decoded: Response<()> = serde_json::from_value(value)?;

        assert_eq!(decoded, response);
        Ok(())
    }

    #[test]
    fn round_trip_notification() -> Result<(), serde_json::Error> {
        let notification = Notification {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            method: TextDeltaEvent::METHOD.to_owned(),
            params: TextDeltaEvent {
                session_id: "session-1".to_owned(),
                delta: "hello".to_owned(),
            },
        };

        let value = serde_json::to_value(&notification)?;
        let decoded: Notification<TextDeltaEvent> = serde_json::from_value(value)?;

        assert_eq!(decoded, notification);
        Ok(())
    }

    #[test]
    fn encoded_jsonrpc_version_is_two_point_zero() -> Result<(), serde_json::Error> {
        let request = Request {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: json!(1),
            method: PingRequest::METHOD.to_owned(),
            params: PingRequest,
        };
        let notification = Notification {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            method: TextDeltaEvent::METHOD.to_owned(),
            params: TextDeltaEvent {
                session_id: "session-1".to_owned(),
                delta: "hello".to_owned(),
            },
        };

        let request_value = serde_json::to_value(request)?;
        let notification_value = serde_json::to_value(notification)?;

        assert_eq!(request_value.get("jsonrpc"), Some(&json!(JSONRPC_VERSION)));
        assert_eq!(
            notification_value.get("jsonrpc"),
            Some(&json!(JSONRPC_VERSION))
        );
        Ok(())
    }

    #[test]
    fn notification_encoding_has_no_id() -> Result<(), serde_json::Error> {
        let notification = Notification {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            method: TextDeltaEvent::METHOD.to_owned(),
            params: TextDeltaEvent {
                session_id: "session-1".to_owned(),
                delta: "hello".to_owned(),
            },
        };

        let value = serde_json::to_value(notification)?;

        assert!(value.get("id").is_none());
        Ok(())
    }

    #[test]
    fn request_id_preserves_number_and_string_shapes() -> Result<(), serde_json::Error> {
        for id in [json!(7), json!("a-uuid")] {
            let request = Request {
                jsonrpc: JSONRPC_VERSION.to_owned(),
                id: id.clone(),
                method: PingRequest::METHOD.to_owned(),
                params: PingRequest,
            };

            let value = serde_json::to_value(&request)?;
            let decoded: Request<PingRequest> = serde_json::from_value(value)?;

            assert_eq!(decoded.id, id);
            assert_eq!(decoded, request);
        }
        Ok(())
    }

    #[test]
    fn provider_selector_uses_tagged_snake_case_wire_format() -> Result<(), serde_json::Error> {
        let samples = [
            (
                ProviderSelector::AnthropicApi {
                    model: "x".to_owned(),
                },
                json!({ "kind": "anthropic_api", "model": "x" }),
            ),
            (
                ProviderSelector::AnthropicOauth {
                    model: "x".to_owned(),
                },
                json!({ "kind": "anthropic_oauth", "model": "x" }),
            ),
            (
                ProviderSelector::OpenaiApi {
                    model: "x".to_owned(),
                },
                json!({ "kind": "openai_api", "model": "x" }),
            ),
            (
                ProviderSelector::OpenaiCodex {
                    model: "x".to_owned(),
                },
                json!({ "kind": "openai_codex", "model": "x" }),
            ),
            (
                ProviderSelector::Openrouter {
                    model: "x".to_owned(),
                },
                json!({ "kind": "openrouter", "model": "x" }),
            ),
        ];

        for (selector, expected) in samples {
            let value = serde_json::to_value(&selector)?;
            let decoded: ProviderSelector = serde_json::from_value(value.clone())?;

            assert_eq!(value, expected);
            assert_eq!(decoded, selector);
        }
        Ok(())
    }

    #[test]
    fn error_response_optional_data_field_presence() -> Result<(), serde_json::Error> {
        let without_data: Response<()> = Response::Err {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: json!(1),
            error: ErrorObject {
                code: -32000,
                message: "no detail".to_owned(),
                data: None,
            },
        };
        let with_data: Response<()> = Response::Err {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: json!(2),
            error: ErrorObject {
                code: -32001,
                message: "has detail".to_owned(),
                data: Some(json!({ "reason": "example" })),
            },
        };

        let without_value = serde_json::to_value(without_data)?;
        let with_value = serde_json::to_value(with_data)?;

        assert_eq!(nested_error_data(&without_value), None);
        assert_eq!(
            nested_error_data(&with_value),
            Some(&json!({ "reason": "example" }))
        );
        Ok(())
    }

    #[test]
    fn method_constants_match_wire_names() {
        assert_eq!(PingRequest::METHOD, "ping");
        assert_eq!(CreateSessionRequest::METHOD, "create_session");
        assert_eq!(AskSessionRequest::METHOD, "ask_session");
        assert_eq!(CancelSessionRequest::METHOD, "cancel_session");
        assert_eq!(CloseSessionRequest::METHOD, "close_session");
        assert_eq!(TextDeltaEvent::METHOD, "session.text_delta");
        assert_eq!(ToolCallStartedEvent::METHOD, "session.tool_call_started");
        assert_eq!(
            ToolCallCompletedEvent::METHOD,
            "session.tool_call_completed"
        );
        assert_eq!(DoneEvent::METHOD, "session.done");
        assert_eq!(ErrorEvent::METHOD, "session.error");
        assert_eq!(CalloutEvent::METHOD, "session.callout");
    }

    #[test]
    fn callout_text_body_round_trip() -> Result<(), serde_json::Error> {
        let event = CalloutEvent {
            session_id: "s1".to_owned(),
            kind: "observation".to_owned(),
            title: "spotted typo".to_owned(),
            body: CalloutBody::Text("line 5".to_owned()),
            theme: Some("info".to_owned()),
            context_refs: vec!["spec:mu-016".to_owned()],
        };
        let value = serde_json::to_value(&event)?;
        let decoded: CalloutEvent = serde_json::from_value(value.clone())?;
        assert_eq!(decoded, event);
        // Untagged enum: body should encode as a bare string.
        assert_eq!(value["body"], json!("line 5"));
        Ok(())
    }

    #[test]
    fn callout_structured_body_round_trip() -> Result<(), serde_json::Error> {
        let event = CalloutEvent {
            session_id: "s1".to_owned(),
            kind: "memory".to_owned(),
            title: "recalled".to_owned(),
            body: CalloutBody::Structured(json!({"id": "abc123", "preview": "..."})),
            theme: None,
            context_refs: vec![],
        };
        let value = serde_json::to_value(&event)?;
        let decoded: CalloutEvent = serde_json::from_value(value.clone())?;
        assert_eq!(decoded, event);
        // Untagged enum: structured body encodes as the object.
        assert_eq!(value["body"]["id"], "abc123");
        Ok(())
    }

    #[test]
    fn callout_skips_empty_optionals_in_encoding() -> Result<(), serde_json::Error> {
        let event = CalloutEvent {
            session_id: "s1".to_owned(),
            kind: "info".to_owned(),
            title: "hi".to_owned(),
            body: CalloutBody::Text("body".to_owned()),
            theme: None,
            context_refs: vec![],
        };
        let value = serde_json::to_value(&event)?;
        let obj = value.as_object().expect("object");
        assert!(!obj.contains_key("theme"), "theme: None should be omitted");
        assert!(
            !obj.contains_key("context_refs"),
            "empty context_refs should be omitted"
        );
        Ok(())
    }

    fn nested_error_data(value: &Value) -> Option<&Value> {
        match value.get("error") {
            Some(Value::Object(error)) => error.get("data"),
            _ => None,
        }
    }

    // ===== mu-029 session.input_required round-trips =====

    #[test]
    fn input_required_event_round_trips() -> Result<(), serde_json::Error> {
        let event = InputRequiredEvent {
            session_id: "s1".into(),
            request_id: "req-42".into(),
            tool_call_id: "call_x".into(),
            tool_name: "bash".into(),
            arguments: json!({ "command": "rm -rf /tmp/scratch" }),
            summary: "bash: rm -rf /tmp/scratch".into(),
        };
        let value = serde_json::to_value(&event)?;
        let decoded: InputRequiredEvent = serde_json::from_value(value)?;
        assert_eq!(decoded, event);
        Ok(())
    }

    #[test]
    fn respond_to_input_required_round_trip_approve() -> Result<(), serde_json::Error> {
        let req = RespondToInputRequiredRequest {
            session_id: "s1".into(),
            request_id: "req-42".into(),
            decision: ApprovalDecision::Approve,
        };
        let value = serde_json::to_value(&req)?;
        assert_eq!(value["decision"], "approve");
        let decoded: RespondToInputRequiredRequest = serde_json::from_value(value)?;
        assert_eq!(decoded, req);
        Ok(())
    }

    #[test]
    fn respond_to_input_required_round_trip_deny() -> Result<(), serde_json::Error> {
        let req = RespondToInputRequiredRequest {
            session_id: "s1".into(),
            request_id: "req-42".into(),
            decision: ApprovalDecision::Deny,
        };
        let value = serde_json::to_value(&req)?;
        assert_eq!(value["decision"], "deny");
        let decoded: RespondToInputRequiredRequest = serde_json::from_value(value)?;
        assert_eq!(decoded, req);
        Ok(())
    }

    #[test]
    fn input_required_event_method_constant() {
        assert_eq!(InputRequiredEvent::METHOD, "session.input_required");
        assert_eq!(
            RespondToInputRequiredRequest::METHOD,
            "session.respond_to_input_required"
        );
    }

    // ===== mu-031 session.delegate round-trips =====

    #[test]
    fn delegate_session_request_round_trip() -> Result<(), serde_json::Error> {
        let req = DelegateSessionRequest {
            parent_session_id: "session-7".into(),
            provider: ProviderSelector::OpenaiCodex {
                model: "gpt-5.5".into(),
            },
            branched_at_parent_event_id: Some(42),
            attenuations: Some(crate::capability::CapabilityAttenuations {
                allowed_tools: Some(vec!["read".into(), "grep".into()]),
                expires_in_seconds: Some(300),
                max_tool_calls: Some(10),
                autonomy: crate::capability::AutonomyCapability::default(),
                aws: None,
            }),
        };
        let value = serde_json::to_value(&req)?;
        let decoded: DelegateSessionRequest = serde_json::from_value(value)?;
        assert_eq!(decoded, req);
        Ok(())
    }

    #[test]
    fn delegate_session_request_optional_branch_point_omitted_when_none(
    ) -> Result<(), serde_json::Error> {
        let req = DelegateSessionRequest {
            parent_session_id: "session-7".into(),
            provider: ProviderSelector::AnthropicApi { model: "x".into() },
            branched_at_parent_event_id: None,
            attenuations: None,
        };
        let value = serde_json::to_value(&req)?;
        let obj = value.as_object().unwrap();
        assert!(
            !obj.contains_key("branched_at_parent_event_id"),
            "None branch-point should be omitted from wire"
        );
        Ok(())
    }

    #[test]
    fn delegate_session_method_constant() {
        assert_eq!(DelegateSessionRequest::METHOD, "session.delegate");
    }

    // ===== mu-7rk auth handshake (mu-vha) =====

    #[test]
    fn bearer_serializes_as_lowercase() -> Result<(), serde_json::Error> {
        let v = serde_json::to_value(AuthMechanism::Bearer)?;
        assert_eq!(v, json!("bearer"));
        Ok(())
    }

    #[test]
    fn other_mechanism_deserializes_from_unknown_string() -> Result<(), serde_json::Error> {
        let m: AuthMechanism = serde_json::from_value(json!("gssapi"))?;
        assert_eq!(m, AuthMechanism::Other("gssapi".into()));
        Ok(())
    }

    #[test]
    fn roundtrip_other_mechanism() -> Result<(), serde_json::Error> {
        let original = AuthMechanism::Other("oauth_bearer".into());
        let v = serde_json::to_value(&original)?;
        assert_eq!(v, json!("oauth_bearer"));
        let back: AuthMechanism = serde_json::from_value(v)?;
        assert_eq!(back, original);
        Ok(())
    }

    #[test]
    fn auth_offer_rejects_unknown_field() {
        let result: Result<AuthOfferRequest, _> =
            serde_json::from_value(json!({ "extra": "nope" }));
        assert!(
            result.is_err(),
            "AuthOfferRequest must reject unknown fields"
        );
    }

    #[test]
    fn auth_initiate_rejects_unknown_field() {
        let result: Result<AuthInitiateRequest, _> = serde_json::from_value(json!({
            "mechanism": "bearer",
            "initial_response": "hunter2",
            "extra": "nope",
        }));
        assert!(
            result.is_err(),
            "AuthInitiateRequest must reject unknown fields"
        );
    }

    #[test]
    fn auth_response_rejects_unknown_field() {
        let result: Result<AuthResponseRequest, _> = serde_json::from_value(json!({
            "server_state_id": "state-1",
            "response": "cmVzcG9uc2U=",
            "extra": "nope",
        }));
        assert!(
            result.is_err(),
            "AuthResponseRequest must reject unknown fields"
        );
    }

    #[test]
    fn auth_denial_code_snake_case() -> Result<(), serde_json::Error> {
        assert_eq!(
            serde_json::to_value(AuthDenialCode::InvalidCredentials)?,
            json!("invalid_credentials")
        );
        assert_eq!(
            serde_json::to_value(AuthDenialCode::UnsupportedMechanism)?,
            json!("unsupported_mechanism")
        );
        assert_eq!(
            serde_json::to_value(AuthDenialCode::MalformedExchange)?,
            json!("malformed_exchange")
        );
        Ok(())
    }

    #[test]
    fn auth_mechanism_display_matches_wire() -> Result<(), serde_json::Error> {
        for m in [
            AuthMechanism::Bearer,
            AuthMechanism::Other("gssapi".into()),
            AuthMechanism::Other("oauth_bearer".into()),
        ] {
            let wire = serde_json::to_value(&m)?;
            let wire_str = wire.as_str().expect("AuthMechanism serializes as string");
            assert_eq!(format!("{m}"), wire_str);
        }
        Ok(())
    }

    // ===== mu-bys: response-shape locking tests =====
    //
    // Lock the wire shape of the auth response types so a future
    // accidental change to internal/external tagging, field renaming,
    // or `deny_unknown_fields` removal surfaces as a test failure
    // rather than as a silent client breakage.

    #[test]
    fn auth_mechanism_bearer_deserializes_from_lowercase() -> Result<(), serde_json::Error> {
        let m: AuthMechanism = serde_json::from_value(json!("bearer"))?;
        assert_eq!(m, AuthMechanism::Bearer);
        Ok(())
    }

    #[test]
    fn auth_exchange_response_accepted_wire_shape() -> Result<(), serde_json::Error> {
        let resp = AuthExchangeResponse::Accepted {
            granted_capability: crate::capability::Capability::default(),
        };
        let v = serde_json::to_value(&resp)?;
        assert_eq!(v["outcome"], "accepted");
        assert!(
            v.get("granted_capability").is_some(),
            "Accepted variant must carry granted_capability"
        );
        let back: AuthExchangeResponse = serde_json::from_value(v)?;
        assert_eq!(back, resp);
        Ok(())
    }

    #[test]
    fn auth_exchange_response_denied_wire_shape() -> Result<(), serde_json::Error> {
        let resp = AuthExchangeResponse::Denied {
            code: AuthDenialCode::InvalidCredentials,
            reason: "token not in allowlist".into(),
        };
        let v = serde_json::to_value(&resp)?;
        assert_eq!(
            v,
            json!({
                "outcome": "denied",
                "code": "invalid_credentials",
                "reason": "token not in allowlist",
            })
        );
        let back: AuthExchangeResponse = serde_json::from_value(v)?;
        assert_eq!(back, resp);
        Ok(())
    }

    #[test]
    fn auth_exchange_response_continue_wire_shape() -> Result<(), serde_json::Error> {
        let resp = AuthExchangeResponse::Continue {
            server_state_id: "state-abc".into(),
            challenge: "Y2hhbGxlbmdl".into(),
        };
        let v = serde_json::to_value(&resp)?;
        assert_eq!(
            v,
            json!({
                "outcome": "continue",
                "server_state_id": "state-abc",
                "challenge": "Y2hhbGxlbmdl",
            })
        );
        let back: AuthExchangeResponse = serde_json::from_value(v)?;
        assert_eq!(back, resp);
        Ok(())
    }

    #[test]
    fn auth_exchange_response_accepted_rejects_unknown_field() {
        let v: Value = json!({
            "outcome": "accepted",
            "granted_capability": {"autonomy": {"kind": "disallowed"}},
            "extra": true,
        });
        let result: Result<AuthExchangeResponse, _> = serde_json::from_value(v);
        assert!(
            result.is_err(),
            "AuthExchangeResponse::Accepted must reject unknown fields"
        );
    }

    #[test]
    fn auth_exchange_response_denied_rejects_unknown_field() {
        let v: Value = json!({
            "outcome": "denied",
            "code": "invalid_credentials",
            "reason": "nope",
            "extra": true,
        });
        let result: Result<AuthExchangeResponse, _> = serde_json::from_value(v);
        assert!(
            result.is_err(),
            "AuthExchangeResponse::Denied must reject unknown fields"
        );
    }

    #[test]
    fn auth_exchange_response_continue_rejects_unknown_field() {
        let v: Value = json!({
            "outcome": "continue",
            "server_state_id": "state-1",
            "challenge": "Y2g=",
            "extra": true,
        });
        let result: Result<AuthExchangeResponse, _> = serde_json::from_value(v);
        assert!(
            result.is_err(),
            "AuthExchangeResponse::Continue must reject unknown fields"
        );
    }

    #[test]
    fn auth_offer_response_wire_shape_mixed_mechanisms() -> Result<(), serde_json::Error> {
        let resp = AuthOfferResponse {
            mechanisms: vec![AuthMechanism::Bearer, AuthMechanism::Other("gssapi".into())],
        };
        let v = serde_json::to_value(&resp)?;
        assert_eq!(v, json!({ "mechanisms": ["bearer", "gssapi"] }));
        let back: AuthOfferResponse = serde_json::from_value(v)?;
        assert_eq!(back, resp);
        Ok(())
    }

    #[test]
    fn auth_offer_response_rejects_unknown_field() {
        let result: Result<AuthOfferResponse, _> = serde_json::from_value(json!({
            "mechanisms": ["bearer"],
            "extra": true,
        }));
        assert!(
            result.is_err(),
            "AuthOfferResponse must reject unknown fields"
        );
    }
}
