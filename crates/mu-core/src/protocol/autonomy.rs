//! mu-036 autonomous-session-loop wire types: the two RPCs
//! (`session.start_autonomous`, `session.schedule_wakeup`) and the
//! `session.autonomous_*` notification events emitted across a run.
//!
//! Phase A surface for mu-036 — the dispatch handlers return
//! "not-yet-implemented" until Phase B (mu-3ao) lands the agent-loop
//! integration. The types here are stable enough for clients to start
//! coding against.
//!
//! Extracted from `protocol.rs` per mu-6a8 phase 4 (2026-05-18); re-exported
//! by `protocol::*` so external callers see no API change.

use serde::{Deserialize, Serialize};

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

/// `session.autonomous_scheduled_wakeup` notification. Emitted when an
/// autonomous run parks itself with `session.schedule_wakeup`. The event is
/// also durable (`EventPayload::AutonomousScheduledWakeup`); the wire form
/// exists so frontends can notify an away operator immediately instead of
/// waiting for the eventual wake iteration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutonomousScheduledWakeupEvent {
    pub session_id: String,
    pub wake_at_unix_ms: u64,
    pub reason: String,
}

impl AutonomousScheduledWakeupEvent {
    pub const METHOD: &'static str = "session.autonomous_scheduled_wakeup";
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
