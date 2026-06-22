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
//! This slice ships the deterministic checks `repeated_identical_tool_call`
//! and `output_exit_mismatch` plus the offline `mu audit` entry point. The
//! remaining deterministic checks (loop-without-state-change,
//! tool-success-rate-dropping, stop-fired-but-no-stop,
//! claim-without-verifying-call) are the rest of mu-pr6r.1; the triage +
//! realtime layers are mu-pr6r.3. Offline first (read the JSONL) — there
//! is no live event-subscribe seam yet, and a batch pass over existing
//! logs is already useful.

use serde_json::Value;

use crate::agent::{AssistantMessage, ContentBlock};
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
    findings.extend(check_output_exit_mismatch(events));
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

/// INVARIANT: a worker that produced substantive output must not then
/// terminate abnormally. This is the mu-qc08 signature — a worker did real
/// work (emitted assistant text, or completed an `ask_session`
/// round-trip) and *then* was killed, timed out, or exited non-zero. When
/// that happens the spawn layer's success/failure signal and the work the
/// worker actually did disagree: exactly the mismatch that silently
/// reported "every worker failed" for ~5 days while workers were in fact
/// producing results (the deadlock fixed in mu-qc08, PR #150).
///
/// "Substantive output" is deliberately narrow and deterministic: a `Done`
/// event (an `ask_session` round-trip completed) OR an
/// `AssistantMessageEvent` carrying at least one non-empty `Text` block.
/// Thinking-only or tool-call-only assistant turns are "in progress," not
/// delivered output, and do NOT count — counting them would flag any
/// worker that merely *started* before dying, a different and noisier
/// signal. Output must precede the abnormal terminal event (events are
/// ordered) — "produced output THEN got killed."
///
/// Abnormal terminal events and their severities:
/// - `WorkerFailed` / `WorkerTimeout` → High. Abrupt termination after
///   producing output is the loss-of-signal shape: the result likely
///   never reached the parent's mailbox.
/// - `WorkerExited { exit_code != 0 }` → Medium. A clean exit reporting a
///   non-zero code after doing work is more plausibly a real error the
///   worker chose to surface, so it is suspicious rather than near-certain.
/// - `WorkerExited { exit_code: 0 }` is the healthy path and never flags.
///
/// One worker's lifecycle lives in its own log (mu-slat), so a stream
/// normally holds at most one terminal event; the check nonetheless emits
/// one finding per qualifying terminal event so a merged/multi-worker
/// stream is handled correctly. The `event_id` points at the terminal
/// event (where the mismatch becomes conclusive); the detail back-
/// references the first output event.
pub fn check_output_exit_mismatch(events: &[SessionEvent]) -> Vec<AuditFinding> {
    let mut findings = Vec::new();
    // Id of the first substantive-output event seen so far (None until one
    // appears). A terminal event only mismatches if output preceded it.
    let mut first_output_id: Option<u64> = None;

    for ev in events {
        match &ev.payload {
            EventPayload::Done { .. } => {
                first_output_id.get_or_insert(ev.id);
            }
            EventPayload::AssistantMessageEvent { message } if has_substantive_text(message) => {
                first_output_id.get_or_insert(ev.id);
            }
            EventPayload::WorkerFailed { reason } => {
                if let Some(out_id) = first_output_id {
                    findings.push(AuditFinding {
                        event_id: ev.id,
                        severity: Severity::High,
                        invariant: "output_exit_mismatch",
                        detail: format!(
                            "worker produced output (first at event {out_id}) but then \
                             FAILED: {reason} — the spawn-layer failure signal disagrees \
                             with the work the worker actually did (mu-qc08 signature)"
                        ),
                    });
                }
            }
            EventPayload::WorkerTimeout { elapsed_ms } => {
                if let Some(out_id) = first_output_id {
                    findings.push(AuditFinding {
                        event_id: ev.id,
                        severity: Severity::High,
                        invariant: "output_exit_mismatch",
                        detail: format!(
                            "worker produced output (first at event {out_id}) but then \
                             TIMED OUT after {elapsed_ms} ms — output risks being lost \
                             despite real work (mu-qc08 signature)"
                        ),
                    });
                }
            }
            EventPayload::WorkerExited {
                exit_code,
                elapsed_ms,
            } if *exit_code != 0 => {
                if let Some(out_id) = first_output_id {
                    findings.push(AuditFinding {
                        event_id: ev.id,
                        severity: Severity::Medium,
                        invariant: "output_exit_mismatch",
                        detail: format!(
                            "worker produced output (first at event {out_id}) but then \
                             exited non-zero (code {exit_code}, after {elapsed_ms} ms) — \
                             a result was generated yet the exit reports failure"
                        ),
                    });
                }
            }
            _ => {}
        }
    }
    findings
}

/// True when an assistant turn delivered actual text — at least one
/// `Text` content block whose text is not all whitespace. Thinking blocks
/// and tool calls are excluded: they are work-in-progress, not delivered
/// output. See `check_output_exit_mismatch` for why this scoping matters.
fn has_substantive_text(message: &AssistantMessage) -> bool {
    message
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::Text { text } if !text.trim().is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::StopReason;
    use crate::event_log::EventActor;
    use serde_json::json;

    fn event(id: u64, actor: EventActor, payload: EventPayload) -> SessionEvent {
        SessionEvent {
            id,
            session_id: "s".into(),
            parent_event_ids: vec![],
            timestamp_unix_ms: id,
            actor,
            payload,
        }
    }

    fn assistant_text(id: u64, text: &str) -> SessionEvent {
        event(
            id,
            EventActor::Agent,
            EventPayload::AssistantMessageEvent {
                message: AssistantMessage {
                    content: vec![ContentBlock::Text { text: text.into() }],
                    stop_reason: StopReason::EndTurn,
                    usage: None,
                },
            },
        )
    }

    fn assistant_thinking_only(id: u64, text: &str) -> SessionEvent {
        event(
            id,
            EventActor::Agent,
            EventPayload::AssistantMessageEvent {
                message: AssistantMessage {
                    content: vec![ContentBlock::Thinking {
                        text: text.into(),
                        opaque: None,
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: None,
                },
            },
        )
    }

    fn done(id: u64) -> SessionEvent {
        event(
            id,
            EventActor::Agent,
            EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 1,
                usage: None,
                elapsed_ms: None,
            },
        )
    }

    fn worker_failed(id: u64, reason: &str) -> SessionEvent {
        event(
            id,
            EventActor::System,
            EventPayload::WorkerFailed {
                reason: reason.into(),
            },
        )
    }

    fn worker_timeout(id: u64, elapsed_ms: u64) -> SessionEvent {
        event(
            id,
            EventActor::System,
            EventPayload::WorkerTimeout { elapsed_ms },
        )
    }

    fn worker_exited(id: u64, exit_code: i32, elapsed_ms: u64) -> SessionEvent {
        event(
            id,
            EventActor::System,
            EventPayload::WorkerExited {
                exit_code,
                elapsed_ms,
            },
        )
    }

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

    #[test]
    fn output_then_worker_failed_is_high() {
        let events = vec![
            assistant_text(1, "did the work"),
            worker_failed(2, "monitor channel closed"),
        ];
        let f = check_output_exit_mismatch(&events);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].invariant, "output_exit_mismatch");
        assert_eq!(f[0].event_id, 2);
        assert_eq!(f[0].severity, Severity::High);
        // back-references the first output event
        assert!(f[0].detail.contains("event 1"));
    }

    #[test]
    fn done_then_worker_timeout_is_high() {
        let events = vec![done(1), worker_timeout(2, 30_000)];
        let f = check_output_exit_mismatch(&events);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].event_id, 2);
        assert_eq!(f[0].severity, Severity::High);
    }

    #[test]
    fn output_then_nonzero_exit_is_medium() {
        let events = vec![assistant_text(1, "result"), worker_exited(2, 1, 5_000)];
        let f = check_output_exit_mismatch(&events);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].event_id, 2);
        assert_eq!(f[0].severity, Severity::Medium);
    }

    #[test]
    fn clean_zero_exit_after_output_never_flags() {
        // The healthy path: produced output, exited 0.
        let events = vec![assistant_text(1, "result"), worker_exited(2, 0, 5_000)];
        assert!(check_output_exit_mismatch(&events).is_empty());
    }

    #[test]
    fn abnormal_exit_without_prior_output_never_flags() {
        // A worker that died before producing anything is a different
        // signal (crash-on-startup), not an output/exit mismatch.
        let events = vec![worker_failed(1, "spawn error: ENOENT")];
        assert!(check_output_exit_mismatch(&events).is_empty());
    }

    #[test]
    fn output_after_the_exit_does_not_count() {
        // Output must PRECEDE the terminal event. Ordering matters: a
        // terminal event with no output before it is clean.
        let events = vec![worker_timeout(1, 1_000), assistant_text(2, "late")];
        assert!(check_output_exit_mismatch(&events).is_empty());
    }

    #[test]
    fn thinking_or_tool_only_turn_is_not_substantive_output() {
        // A thinking-only assistant turn is work-in-progress, not
        // delivered output — a worker that only got that far before
        // timing out should not flag as a mismatch.
        let events = vec![
            assistant_thinking_only(1, "let me plan"),
            worker_timeout(2, 1_000),
        ];
        assert!(check_output_exit_mismatch(&events).is_empty());
    }

    #[test]
    fn whitespace_only_text_is_not_substantive_output() {
        let events = vec![assistant_text(1, "   \n  "), worker_failed(2, "killed")];
        assert!(check_output_exit_mismatch(&events).is_empty());
    }

    #[test]
    fn merged_stream_flags_each_qualifying_terminal_event() {
        // A merged/multi-worker stream: two distinct
        // output-then-abnormal-exit shapes both surface.
        let events = vec![
            assistant_text(1, "worker A output"),
            worker_failed(2, "A failed"),
            assistant_text(3, "worker B output"),
            worker_exited(4, 2, 100),
        ];
        let f = check_output_exit_mismatch(&events);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].event_id, 2);
        assert_eq!(f[0].severity, Severity::High);
        assert_eq!(f[1].event_id, 4);
        assert_eq!(f[1].severity, Severity::Medium);
    }

    #[test]
    fn audit_session_runs_output_exit_mismatch() {
        let events = vec![done(1), worker_failed(2, "boom")];
        let findings = audit_session(&events);
        assert!(findings
            .iter()
            .any(|f| f.invariant == "output_exit_mismatch"));
    }
}
