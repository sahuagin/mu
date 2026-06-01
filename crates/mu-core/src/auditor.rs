//! Process-layer auditors (bead mu-pr6r / mu-pr6r.1): deterministic
//! invariant checks over a worker/session typed event stream.
//!
//! Each check is a pure function `&[SessionEvent] -> Vec<AuditFinding>` —
//! narrow, deterministic, runnable independently, findings consolidated.
//! This is the seam-auditor pattern (one narrow checklist per concern)
//! applied at the PROCESS layer, parallel to the artifact-layer diff
//! auditors in goal-protocol.
//!
//! Governing principle (mu-pr6r): invariants survive by being CHECKED,
//! not trusted — the check fires whether or not the worker remembers.
//!
//! v1 (this slice) ships the first deterministic check
//! (`repeated_identical_tool_call`) plus the offline `mu audit` entry
//! point. The remaining deterministic checks (loop-without-state-change,
//! tool-success-rate-dropping, stop-fired-but-no-stop,
//! claim-without-verifying-call, output/exit-mismatch) are the rest of
//! mu-pr6r.1; the triage + realtime layers are mu-pr6r.3. Offline first
//! (read the JSONL) — there is no live event-subscribe seam yet, and a
//! batch pass over existing logs is already useful.

use serde_json::Value;

use crate::event_log::{EventPayload, SessionEvent};

/// How loudly a finding asks for attention. The triage layer (mu-pr6r.3)
/// will key escalation off this; for now it orders and labels output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Near-certain process fault; wants attention now.
    High,
    /// Suspicious pattern; worth a look.
    Medium,
    /// Informational signal.
    Low,
}

/// A single process-invariant violation found in an event stream.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditFinding {
    /// The event id at which the pattern became conclusive — points the
    /// operator at the right place in the log.
    pub event_id: u64,
    pub severity: Severity,
    /// Stable machine name of the invariant (e.g.
    /// `"repeated_identical_tool_call"`) — for grouping/correlation.
    pub invariant: &'static str,
    /// Human-readable specifics.
    pub detail: String,
}

/// Default run length at which an identical-tool-call repeat is flagged.
pub const DEFAULT_REPEAT_THRESHOLD: usize = 3;

/// Run every deterministic auditor over the stream and consolidate the
/// findings. The single entry point used by `mu audit`.
pub fn audit_session(events: &[SessionEvent]) -> Vec<AuditFinding> {
    let mut findings = Vec::new();
    findings.extend(check_repeated_identical_tool_call(
        events,
        DEFAULT_REPEAT_THRESHOLD,
    ));
    findings
}

/// INVARIANT: the same tool call — identical `name` AND `arguments` —
/// should not repeat back-to-back many times. A long run of identical
/// calls is a worker spinning on an unchanging action instead of making
/// progress (the retry-loop / stuck shape).
///
/// Counts *consecutive* identical `ToolCall`s, keyed on `(name,
/// arguments)` — NOT `call_id`, which is unique per call. Non-`ToolCall`
/// events between two identical calls (e.g. the `ToolResult` answering
/// the first) do NOT reset the run — `call → result → identical call`
/// is exactly the loop we want to catch. A *different* `ToolCall`
/// resets it. Emits one finding per run, when it first crosses
/// `threshold`.
pub fn check_repeated_identical_tool_call(
    events: &[SessionEvent],
    threshold: usize,
) -> Vec<AuditFinding> {
    let mut findings = Vec::new();
    if threshold == 0 {
        return findings;
    }
    let mut run: Option<(&str, &Value)> = None;
    let mut run_len: usize = 0;
    let mut run_start_id: u64 = 0;
    let mut flagged = false;

    for ev in events {
        let EventPayload::ToolCall {
            name, arguments, ..
        } = &ev.payload
        else {
            continue;
        };
        match run {
            Some((rn, ra)) if rn == name.as_str() && ra == arguments => {
                run_len += 1;
            }
            _ => {
                run = Some((name.as_str(), arguments));
                run_len = 1;
                run_start_id = ev.id;
                flagged = false;
            }
        }
        if run_len >= threshold && !flagged {
            findings.push(AuditFinding {
                event_id: ev.id,
                severity: Severity::High,
                invariant: "repeated_identical_tool_call",
                detail: format!(
                    "tool `{name}` called {run_len}x with identical arguments \
                     (run began at event {run_start_id}) — worker appears stuck \
                     repeating an unchanging action"
                ),
            });
            flagged = true;
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::EventActor;
    use serde_json::json;

    fn tool_call(id: u64, name: &str, args: Value) -> SessionEvent {
        SessionEvent {
            id,
            session_id: "s".into(),
            parent_event_ids: vec![],
            timestamp_unix_ms: id,
            actor: EventActor::Agent,
            payload: EventPayload::ToolCall {
                call_id: format!("c{id}"),
                name: name.into(),
                arguments: args,
            },
        }
    }

    fn tool_result(id: u64, call_id: &str) -> SessionEvent {
        SessionEvent {
            id,
            session_id: "s".into(),
            parent_event_ids: vec![],
            timestamp_unix_ms: id,
            actor: EventActor::Tool { name: "x".into() },
            payload: EventPayload::ToolResult {
                call_id: call_id.into(),
                content: "ok".into(),
                is_error: false,
            },
        }
    }

    #[test]
    fn flags_three_identical_calls() {
        let events = vec![
            tool_call(1, "read", json!({"path": "a"})),
            tool_call(2, "read", json!({"path": "a"})),
            tool_call(3, "read", json!({"path": "a"})),
        ];
        let f = check_repeated_identical_tool_call(&events, 3);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].invariant, "repeated_identical_tool_call");
        assert_eq!(f[0].event_id, 3);
        assert_eq!(f[0].severity, Severity::High);
    }

    #[test]
    fn two_identical_calls_below_threshold_are_fine() {
        let events = vec![
            tool_call(1, "read", json!({"path": "a"})),
            tool_call(2, "read", json!({"path": "a"})),
        ];
        assert!(check_repeated_identical_tool_call(&events, 3).is_empty());
    }

    #[test]
    fn interleaved_tool_results_do_not_reset_the_run() {
        // call → result → identical call → result → identical call: still a stuck loop.
        let events = vec![
            tool_call(1, "grep", json!({"q": "foo"})),
            tool_result(2, "c1"),
            tool_call(3, "grep", json!({"q": "foo"})),
            tool_result(4, "c3"),
            tool_call(5, "grep", json!({"q": "foo"})),
        ];
        let f = check_repeated_identical_tool_call(&events, 3);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].event_id, 5);
    }

    #[test]
    fn different_arguments_reset_the_run() {
        let events = vec![
            tool_call(1, "read", json!({"path": "a"})),
            tool_call(2, "read", json!({"path": "b"})), // different args
            tool_call(3, "read", json!({"path": "a"})),
        ];
        assert!(check_repeated_identical_tool_call(&events, 3).is_empty());
    }

    #[test]
    fn flags_each_distinct_stuck_run_once() {
        let events = vec![
            tool_call(1, "read", json!({"p": "a"})),
            tool_call(2, "read", json!({"p": "a"})),
            tool_call(3, "read", json!({"p": "a"})),
            tool_call(4, "read", json!({"p": "a"})), // still run 1, already flagged
            tool_call(5, "ls", json!({})),           // resets
            tool_call(6, "read", json!({"p": "a"})),
            tool_call(7, "read", json!({"p": "a"})),
            tool_call(8, "read", json!({"p": "a"})), // run 2 crosses threshold
        ];
        let f = check_repeated_identical_tool_call(&events, 3);
        assert_eq!(f.len(), 2, "one finding per distinct stuck run");
        assert_eq!(f[0].event_id, 3);
        assert_eq!(f[1].event_id, 8);
    }

    #[test]
    fn audit_session_runs_the_registered_checks() {
        let events = vec![
            tool_call(1, "read", json!({"p": "a"})),
            tool_call(2, "read", json!({"p": "a"})),
            tool_call(3, "read", json!({"p": "a"})),
        ];
        assert_eq!(audit_session(&events).len(), 1);
    }

    #[test]
    fn empty_stream_has_no_findings() {
        assert!(audit_session(&[]).is_empty());
    }
}
