//! Autonomy mode — iteration state machine + bounds enforcement.

use std::time::Instant;

use crate::protocol::AutonomyOptions;

/// Session run mode. Tracks the current phase: Idle (awaiting
/// external user action), Asking (processing an ask_session),
/// Autonomous (running toward a goal with iteration bounds),
/// or Sleeping (parked by `session.schedule_wakeup` until a wake time).
#[derive(Debug)]
pub enum RunMode {
    Idle,
    Asking,
    Autonomous {
        iteration: u32,
        goal: String,
        options: AutonomyOptions,
        started_at: Instant,
        tool_calls_consumed: u32,
    },
    /// mu-036 Phase C (mu-7zn): `session.schedule_wakeup` parks the
    /// autonomous session here until `wake_at`. The suspended
    /// autonomous context (`iteration`, `goal`, `options`,
    /// `started_at`, `tool_calls_consumed`) rides along so the loop
    /// re-enters `Autonomous` at iteration N+1 on wake — `reason`
    /// becomes that iteration's motivation. INV-5: no provider or
    /// tool budget is consumed while sleeping; only wall-clock
    /// (`started_at`) keeps accruing against the autonomy bound.
    Sleeping {
        wake_at: Instant,
        reason: String,
        iteration: u32,
        goal: String,
        options: AutonomyOptions,
        started_at: Instant,
        tool_calls_consumed: u32,
    },
}
