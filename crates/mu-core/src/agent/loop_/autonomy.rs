//! Autonomy mode — iteration state machine + bounds enforcement.

use std::time::Instant;

use crate::protocol::AutonomyOptions;

/// Session run mode. Tracks the current phase: Idle (awaiting
/// external user action), Asking (processing an ask_session),
/// Autonomous (running toward a goal with iteration bounds),
/// or Sleeping (Phase C placeholder).
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
    /// Phase C placeholder — schedule_wakeup parks the session here.
    Sleeping {
        wake_at: Instant,
        reason: String,
    },
}
