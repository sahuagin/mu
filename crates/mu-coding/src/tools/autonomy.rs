//! mu-7e21: agent-facing autonomy tools.
//!
//! mu-036 shipped the wire surface (`session.start_autonomous`,
//! `session.schedule_wakeup`) but no way for the in-session agent to
//! DISCOVER or USE it: a live session asked "can you work toward a
//! goal autonomously?" listed `spawn_worker` and `watch`, then
//! disclaimed the exact capability its daemon implemented (operator
//! terrain test, daemon 1edc4afb18fd54fe session-1). These tools close
//! that gap from the inside: they let the model accept a goal or park
//! itself in-band, by sending the SAME `AgentInput`s the RPC handlers
//! send — no parallel control path.
//!
//! INV-1 is preserved structurally: the tools are only INJECTED into a
//! session whose capability already grants autonomy (see
//! `session_spawn_tools` in handlers/session.rs), and the loop's
//! capability bounds — not the tool arguments — remain the enforcement
//! ceiling (INV-2). A session without the grant never sees these tools,
//! so the tool list itself answers the discoverability question
//! honestly in both directions.

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use mu_core::agent::{AgentInput, Tool, ToolResult, ToolSpec};
use mu_core::protocol::AutonomyOptions;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::serve::WeakSessions;

fn err(content: impl Into<String>) -> ToolResult {
    ToolResult {
        content: content.into(),
        is_error: true,
    }
}

fn ok(content: impl Into<String>) -> ToolResult {
    ToolResult {
        content: content.into(),
        is_error: false,
    }
}

/// Tool: enter autonomous mode on THIS session with a goal.
///
/// Sends `AgentInput::StartAutonomous` into the owning session's input
/// channel — identical to what `handle_start_autonomous` does after
/// its capability check. The input is consumed at the next iteration
/// boundary, so a turn that calls this finishes normally and the loop
/// transitions afterward.
pub struct StartAutonomousTool {
    /// Weak for the same reason as SpawnWorkerTool (mu-qc08): this
    /// tool lives in its owning session's tool list; a strong handle
    /// would keep the session's own `input_tx` alive and deadlock
    /// shutdown. Upgraded transiently in `execute`.
    sessions: WeakSessions,
    /// The owning session — autonomy tools only ever act on self.
    session_id: String,
}

impl StartAutonomousTool {
    pub fn new(sessions: WeakSessions, session_id: String) -> Self {
        Self {
            sessions,
            session_id,
        }
    }

    /// Parse the model's arguments into the input the loop consumes.
    /// Factored out of `execute` for unit testing.
    fn build_input(arguments: &Value) -> Result<AgentInput, String> {
        let goal = arguments
            .get("goal")
            .and_then(Value::as_str)
            .filter(|g| !g.trim().is_empty())
            .ok_or_else(|| "missing required argument: goal".to_string())?
            .to_string();
        // Options refine WITHIN the capability ceiling (INV-2); absent
        // fields fall back to capability values, so passing nothing is
        // always safe.
        let options: AutonomyOptions = match arguments.get("options") {
            Some(v) => {
                serde_json::from_value(v.clone()).map_err(|e| format!("invalid options: {e}"))?
            }
            None => AutonomyOptions::default(),
        };
        Ok(AgentInput::StartAutonomous { goal, options })
    }
}

#[async_trait]
impl Tool for StartAutonomousTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "start_autonomous",
            "Enter autonomous mode on this session: work toward `goal` across \
             multiple iterations without a human between turns, until the goal \
             is met or a capability bound trips (iterations, wall clock, tool \
             calls — enforced by the daemon, not by you). ONLY call this when \
             the operator gives you an actual goal to pursue to completion — \
             never for hypothetical or capability questions ('could you...?', \
             'do you have...?'): answer those in text. The current turn \
             finishes normally; autonomy begins at the next iteration. While \
             autonomous you may park yourself with `schedule_wakeup` (if \
             granted) and will be woken by mailbox messages and watch \
             completions.",
            json!({
                "type": "object",
                "properties": {
                    "goal": {
                        "type": "string",
                        "description": "The goal to work toward, stated so a \
                         goal-check can judge completion."
                    },
                    "options": {
                        "type": "object",
                        "description": "Optional refinements within the \
                         capability ceiling: max_iterations, \
                         goal_check_interval, goal_check_method, \
                         escalate_on_idle_after_ms."
                    }
                },
                "required": ["goal"]
            }),
        )
        // mu-cvm5: explicit read-only opt-in (default now fails closed).
        // Affects only this session's own control flow, not world state;
        // gated by AutonomyCapability at tool-presence + the loop input
        // handler, not the tool-policy gate (mu-036).
        .read_only()
    }

    async fn execute(&self, arguments: Value, _cancel_rx: oneshot::Receiver<()>) -> ToolResult {
        let input = match Self::build_input(&arguments) {
            Ok(i) => i,
            Err(e) => return err(e),
        };
        send_to_own_loop(&self.sessions, &self.session_id, input).await
    }
}

/// Tool: park THIS session until a wall-clock instant.
///
/// Mirrors `handle_schedule_wakeup`: exactly one of `sleep_for_ms` /
/// `wake_at_unix_ms`, relative resolved to absolute HERE (the loop
/// consumes only absolutes). INV-5 holds: a parked session consumes no
/// model or tool budget; the wall-clock bound still ticks.
pub struct ScheduleWakeupTool {
    sessions: WeakSessions,
    session_id: String,
}

impl ScheduleWakeupTool {
    pub fn new(sessions: WeakSessions, session_id: String) -> Self {
        Self {
            sessions,
            session_id,
        }
    }

    /// Validate + resolve arguments to the absolute-time input the
    /// loop consumes. `now_unix_ms` is a parameter for testability.
    fn build_input(arguments: &Value, now_unix_ms: u64) -> Result<AgentInput, String> {
        let sleep_for = arguments.get("sleep_for_ms").and_then(Value::as_u64);
        let wake_at = arguments.get("wake_at_unix_ms").and_then(Value::as_u64);
        let reason = arguments
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("(no reason given)")
            .to_string();
        let wake_at_unix_ms = match (sleep_for, wake_at) {
            (Some(ms), None) => now_unix_ms.saturating_add(ms),
            (None, Some(at)) => at,
            _ => {
                return Err("exactly one of sleep_for_ms / wake_at_unix_ms must be set".to_string())
            }
        };
        Ok(AgentInput::ScheduleWakeup {
            wake_at_unix_ms,
            reason,
        })
    }
}

#[async_trait]
impl Tool for ScheduleWakeupTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "schedule_wakeup",
            "Park this session until a wall-clock time, consuming no model or \
             tool budget while asleep (the wall-clock bound still applies). \
             Use while autonomous to wait out long-running external work \
             instead of polling. Exactly one of sleep_for_ms / wake_at_unix_ms. \
             You resume at the next iteration when the timer fires — or \
             earlier, if a mailbox message or watch completion wakes you.",
            json!({
                "type": "object",
                "properties": {
                    "sleep_for_ms": {
                        "type": "integer",
                        "description": "Sleep duration from now, milliseconds."
                    },
                    "wake_at_unix_ms": {
                        "type": "integer",
                        "description": "Absolute wake time, unix epoch ms."
                    },
                    "reason": {
                        "type": "string",
                        "description": "Why you're parking — recorded on the \
                         wakeup event for the operator."
                    }
                }
            }),
        )
        // mu-cvm5: explicit read-only opt-in (default now fails closed).
        // Affects only this session's own control flow (mu-036), gated by
        // AutonomyCapability, not the tool-policy gate.
        .read_only()
    }

    async fn execute(&self, arguments: Value, _cancel_rx: oneshot::Receiver<()>) -> ToolResult {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let input = match Self::build_input(&arguments, now) {
            Ok(i) => i,
            Err(e) => return err(e),
        };
        send_to_own_loop(&self.sessions, &self.session_id, input).await
    }
}

/// Shared tail: upgrade the weak registry handle, find our own input
/// channel, send. Failure modes are all "session is shutting down" —
/// reported as tool errors, never panics.
async fn send_to_own_loop(
    sessions: &WeakSessions,
    session_id: &str,
    input: AgentInput,
) -> ToolResult {
    let Some(sessions) = sessions.upgrade() else {
        return err("daemon is shutting down");
    };
    let Some(tx) = sessions.input_sender(session_id) else {
        return err(format!("session not found: {session_id}"));
    };
    let summary = match &input {
        AgentInput::StartAutonomous { goal, .. } => {
            format!("autonomous mode accepted; goal: {goal}")
        }
        AgentInput::ScheduleWakeup {
            wake_at_unix_ms, ..
        } => {
            format!("wakeup scheduled at unix_ms {wake_at_unix_ms}")
        }
        _ => "input queued".to_string(),
    };
    match tx.send(input).await {
        Ok(()) => ok(format!(
            "{summary} — takes effect at the next iteration boundary \
             (the current turn finishes normally)."
        )),
        Err(_) => err("session loop has terminated"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_autonomous_requires_goal() {
        let e = StartAutonomousTool::build_input(&json!({})).unwrap_err();
        assert!(e.contains("goal"));
        let e = StartAutonomousTool::build_input(&json!({"goal": "  "})).unwrap_err();
        assert!(e.contains("goal"));
    }

    #[test]
    fn start_autonomous_parses_goal_and_options() {
        let input = StartAutonomousTool::build_input(&json!({
            "goal": "close all P1 beads",
            "options": {"max_iterations": 5}
        }))
        .unwrap();
        match input {
            AgentInput::StartAutonomous { goal, options } => {
                assert_eq!(goal, "close all P1 beads");
                assert_eq!(options.max_iterations, Some(5));
            }
            other => panic!("wrong input: {other:?}"),
        }
    }

    #[test]
    fn schedule_wakeup_requires_exactly_one_time_form() {
        let e = ScheduleWakeupTool::build_input(&json!({}), 1_000).unwrap_err();
        assert!(e.contains("exactly one"));
        let e = ScheduleWakeupTool::build_input(
            &json!({"sleep_for_ms": 10, "wake_at_unix_ms": 99}),
            1_000,
        )
        .unwrap_err();
        assert!(e.contains("exactly one"));
    }

    #[test]
    fn schedule_wakeup_resolves_relative_to_absolute() {
        let input =
            ScheduleWakeupTool::build_input(&json!({"sleep_for_ms": 500, "reason": "ci"}), 1_000)
                .unwrap();
        match input {
            AgentInput::ScheduleWakeup {
                wake_at_unix_ms,
                reason,
            } => {
                assert_eq!(wake_at_unix_ms, 1_500);
                assert_eq!(reason, "ci");
            }
            other => panic!("wrong input: {other:?}"),
        }
    }

    #[test]
    fn schedule_wakeup_passes_absolute_through() {
        let input =
            ScheduleWakeupTool::build_input(&json!({"wake_at_unix_ms": 7_777}), 1_000).unwrap();
        match input {
            AgentInput::ScheduleWakeup {
                wake_at_unix_ms, ..
            } => assert_eq!(wake_at_unix_ms, 7_777),
            other => panic!("wrong input: {other:?}"),
        }
    }
}
