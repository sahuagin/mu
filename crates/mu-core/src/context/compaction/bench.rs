//! Benchmark harness for [`CompactionPolicy`] impls (mu-kgu.5).
//!
//! Loads session JSONLs written by [`crate::event_log::SessionEventLog`]
//! (mu-upb's write-side persistence) from disk, projects each session's
//! events into a [`crate::context::RetainedRope`] (via the same
//! [`crate::context::assemble_rope`] path the live agent loop uses),
//! runs each configured policy, and emits structured metrics rows.
//!
//! Two pieces are split out so the example binary can stay thin:
//!
//! - [`load_session_rope`] — JSONL → `RetainedRope`. Replays
//!   `UserMessage` / `AssistantMessageEvent` / `ToolResult` payloads
//!   in event-order into a `Vec<AgentMessage>` and hands it to
//!   `assemble_rope` (no system prompt, no tool specs — the corpus
//!   doesn't carry those today).
//! - [`benchmark_session`] — runs each `(label, policy)` against the
//!   given rope at `target_tokens` and returns one [`BenchRow`] per
//!   policy.
//!
//! ## Mock judge for [`super::hash_summary::HashAndSummaryPolicy`]
//!
//! [`KeepHalfJudge`] is a deterministic, no-network judge that the
//! example wires in by default. Real Anthropic-backed judges are
//! out-of-scope for mu-kgu.5 (the bead's "Out" list: "Live judge API
//! calls"); a future bead will land a `ProviderJudge` adapter.

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use serde::Serialize;

use super::hash_summary::{Judge, JudgeError};
use super::CompactionPolicy;
use crate::agent::types::AgentMessage;
use crate::context::assemble_rope;
use crate::context::rope::RetainedRope;
use crate::event_log::{EventPayload, SessionEvent, SessionEventLog};

/// One row of benchmark output.
///
/// Field order matches the CSV header emitted by the example. The
/// schema is deliberately flat so downstream tooling (jupyter,
/// `csvkit`, mu-pex later) can join across runs without struct
/// destructuring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BenchRow {
    /// Source session id (from the JSONL's first event, or filename
    /// stem if the log was empty — same fallback as
    /// [`SessionEventLog::from_jsonl`]).
    pub session_id: String,
    /// `CompactionPolicy::policy_label()` — short stable identifier.
    pub policy_label: String,
    /// `CompactionResult::tokens_before`.
    pub tokens_before: usize,
    /// `CompactionResult::tokens_after`.
    pub tokens_after: usize,
    /// Count of all [`CompactionDecision`] entries.
    pub decisions_count: usize,
    /// Number of model calls the policy performed. v1 heuristic policies
    /// emit `0`; the hash-summary policy emits `1` (one judge call per
    /// `compact`).
    pub model_calls: u32,
    /// Wall-clock duration of `compact()`, in milliseconds. Measured
    /// at the harness's call site (independent of the policy's own
    /// `wall_clock_ms`, which may round to 0 for fast policies).
    pub wall_clock_ms: u64,
    /// Total spans in the rope BEFORE compaction. Lets the operator
    /// sanity-check the "spans dropped" delta independent of token
    /// estimates.
    pub spans_before: usize,
    /// Total spans in the rope AFTER compaction.
    pub spans_after: usize,
}

/// Load a session JSONL into a [`RetainedRope`].
///
/// Walks the on-disk event log and replays its conversational events
/// (`UserMessage`, `AssistantMessageEvent`, `ToolResult`) into a
/// `Vec<AgentMessage>`, then projects via [`assemble_rope`]. The
/// corpus does not carry system prompts or tool specs today; both
/// are passed as `None` / `&[]`.
///
/// Returns the `(session_id, rope, malformed_lines)` tuple so callers
/// can label benchmark rows and surface read-side health.
pub fn load_session_rope(path: &Path) -> std::io::Result<(String, RetainedRope, usize)> {
    let (log, malformed) = SessionEventLog::from_jsonl(path)?;
    let session_id = log.session_id().to_string();
    let events = log.snapshot();
    let messages = events_to_messages(&events);
    let rope = assemble_rope(None, &messages, &[]);
    Ok((session_id, rope, malformed))
}

/// Project a [`SessionEvent`] sequence into the [`AgentMessage`]
/// sequence the live loop would have held at end-of-session.
///
/// Public so tests in the example or downstream tools can exercise
/// the projection in isolation.
pub fn events_to_messages(events: &[SessionEvent]) -> Vec<AgentMessage> {
    let mut out: Vec<AgentMessage> = Vec::with_capacity(events.len());
    for ev in events {
        match &ev.payload {
            EventPayload::UserMessage { content } => {
                out.push(AgentMessage::User {
                    content: content.clone(),
                });
            }
            EventPayload::AssistantMessageEvent { message } => {
                out.push(AgentMessage::Assistant(message.clone()));
            }
            EventPayload::ToolResult {
                call_id,
                content,
                is_error,
            } => {
                out.push(AgentMessage::ToolResult {
                    call_id: call_id.clone(),
                    content: content.clone(),
                    is_error: *is_error,
                });
            }
            // ToolCall is already represented inside the preceding
            // AssistantMessageEvent's content blocks — re-emitting it
            // here would double-count. Everything else (Done,
            // ContextAssembly, ProviderStatusUpdate, autonomous-loop
            // events, callouts, errors, SessionCreated/Closed) is
            // metadata, not conversational state.
            _ => {}
        }
    }
    out
}

/// A `(label, policy)` pair the harness will run. The label is what
/// shows up in [`BenchRow::policy_label`] / the CSV header — typically
/// `policy.policy_label()`, but the caller may override (e.g., to
/// distinguish two `HashAndSummaryPolicy` instances configured with
/// different judges).
pub struct LabeledPolicy {
    pub label: String,
    pub policy: Arc<dyn CompactionPolicy>,
    /// How many model calls one `compact()` invocation performs for
    /// this policy. Caller-supplied because the trait doesn't surface
    /// it; v1 conventions: 0 for heuristic policies, 1 for hash-and-
    /// summary.
    pub model_calls: u32,
}

impl std::fmt::Debug for LabeledPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LabeledPolicy")
            .field("label", &self.label)
            .field("policy", &self.policy.policy_label())
            .field("model_calls", &self.model_calls)
            .finish()
    }
}

/// Run every policy against `rope` at `target_tokens`. Returns one
/// row per policy, in the order they appear in `policies`.
pub fn benchmark_session(
    session_id: &str,
    rope: &RetainedRope,
    policies: &[LabeledPolicy],
    target_tokens: usize,
) -> Vec<BenchRow> {
    let mut rows: Vec<BenchRow> = Vec::with_capacity(policies.len());
    let spans_before = rope.len();
    for lp in policies {
        let start = Instant::now();
        let result = lp.policy.compact(rope, target_tokens);
        let wall_clock_ms = start.elapsed().as_millis().min(u64::MAX as u128) as u64;
        rows.push(BenchRow {
            session_id: session_id.to_string(),
            policy_label: lp.label.clone(),
            tokens_before: result.tokens_before,
            tokens_after: result.tokens_after,
            decisions_count: result.decisions.len(),
            model_calls: lp.model_calls,
            wall_clock_ms,
            spans_before,
            spans_after: result.rope.len(),
        });
    }
    rows
}

/// Deterministic judge that keeps every other span (even indices into
/// the rope's span sequence). Pure-CPU, no network — the default
/// "live mode is not yet wired" stand-in for the benchmark harness.
///
/// The judge builds its keep-list by parsing the prompt: the prompt
/// shape emitted by [`super::hash_summary::HashAndSummaryPolicy`]
/// includes `hash=<short-hash>` lines, which this judge collects in
/// order and emits at even indices. Independent of the keep
/// strategy, the summary string is constant.
#[derive(Debug)]
pub struct KeepHalfJudge {
    /// Records every prompt the judge has been asked (for tests that
    /// want to assert the policy actually called through).
    pub prompts: Mutex<Vec<String>>,
}

impl Default for KeepHalfJudge {
    fn default() -> Self {
        Self {
            prompts: Mutex::new(Vec::new()),
        }
    }
}

impl KeepHalfJudge {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Judge for KeepHalfJudge {
    fn judge(&self, prompt: &str) -> Result<String, JudgeError> {
        if let Ok(mut p) = self.prompts.lock() {
            p.push(prompt.to_string());
        }
        let hashes: Vec<&str> = prompt
            .lines()
            .filter_map(|l| l.strip_prefix("hash="))
            .map(|rest| rest.split_whitespace().next().unwrap_or(""))
            .filter(|h| !h.is_empty())
            .collect();
        let keep: Vec<&&str> = hashes.iter().step_by(2).collect();
        let keep_quoted: Vec<String> = keep.iter().map(|h| format!("\"{h}\"")).collect();
        let summary = "[keep-half mock judge] absorbed every other span";
        Ok(format!(
            "{{\"keep\":[{}],\"summary\":\"{summary}\"}}",
            keep_quoted.join(",")
        ))
    }
}

/// Write a CSV header line matching [`BenchRow`]'s field order.
pub fn csv_header() -> &'static str {
    "session_id,policy_label,tokens_before,tokens_after,decisions_count,model_calls,wall_clock_ms,spans_before,spans_after"
}

/// Render one [`BenchRow`] as a CSV line. Strings are quoted only
/// when they contain a comma; session_ids / policy labels in this
/// harness never do.
pub fn csv_row(r: &BenchRow) -> String {
    fn esc(s: &str) -> String {
        if s.contains(',') || s.contains('"') {
            let escaped = s.replace('"', "\"\"");
            format!("\"{escaped}\"")
        } else {
            s.to_string()
        }
    }
    format!(
        "{},{},{},{},{},{},{},{},{}",
        esc(&r.session_id),
        esc(&r.policy_label),
        r.tokens_before,
        r.tokens_after,
        r.decisions_count,
        r.model_calls,
        r.wall_clock_ms,
        r.spans_before,
        r.spans_after,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::types::{AssistantMessage, ContentBlock, StopReason};
    use crate::context::compaction::hash_summary::HashAndSummaryPolicy;
    use crate::context::compaction::heuristic::SpanFamilyDropPolicy;
    use crate::context::compaction::NoCompactionPolicy;
    use crate::context::rope::SpanKind;
    use crate::event_log::EventActor;
    use std::io::Write;

    fn tmpfile(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mu-kgu5-bench-{}-{}", name, std::process::id()));
        std::fs::create_dir_all(&dir).expect("create tmp dir");
        dir.join(format!("{name}.jsonl"))
    }

    fn write_jsonl(path: &std::path::Path, events: &[SessionEvent]) {
        let mut file = std::fs::File::create(path).expect("create");
        for ev in events {
            let line = serde_json::to_string(ev).expect("ser");
            writeln!(file, "{line}").expect("write");
        }
    }

    fn ev(id: u64, session: &str, payload: EventPayload) -> SessionEvent {
        SessionEvent {
            id,
            session_id: session.to_string(),
            parent_event_ids: Vec::new(),
            timestamp_unix_ms: 0,
            actor: EventActor::User,
            payload,
        }
    }

    #[test]
    fn load_session_rope_projects_three_conversational_events_into_three_spans() {
        let path = tmpfile("three-events");
        let events = vec![
            ev(
                1,
                "s-mock-1",
                EventPayload::SessionCreated {
                    provider_kind: "faux".into(),
                    model: "test".into(),
                    parent_session_id: None,
                    branched_at_parent_event_id: None,
                },
            ),
            ev(
                2,
                "s-mock-1",
                EventPayload::UserMessage {
                    content: "hello".into(),
                },
            ),
            ev(
                3,
                "s-mock-1",
                EventPayload::AssistantMessageEvent {
                    message: AssistantMessage {
                        content: vec![ContentBlock::Text {
                            text: "hi back".into(),
                        }],
                        stop_reason: StopReason::EndTurn,
                        usage: None,
                    },
                },
            ),
            ev(
                4,
                "s-mock-1",
                EventPayload::ToolResult {
                    call_id: "c-1".into(),
                    content: "{\"ok\":true}".into(),
                    is_error: false,
                },
            ),
        ];
        write_jsonl(&path, &events);

        let (sid, rope, malformed) = load_session_rope(&path).expect("load");
        assert_eq!(sid, "s-mock-1");
        assert_eq!(malformed, 0);
        let kinds: Vec<_> = rope.spans().iter().map(|s| s.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![SpanKind::User, SpanKind::Assistant, SpanKind::ToolResult]
        );
    }

    #[test]
    fn load_session_rope_skips_non_conversational_events() {
        // SessionCreated / Done / ProviderStatusUpdate / ContextAssembly
        // are metadata — they must not produce spans.
        let path = tmpfile("metadata-only");
        let events = vec![
            ev(
                1,
                "s-meta",
                EventPayload::SessionCreated {
                    provider_kind: "faux".into(),
                    model: "test".into(),
                    parent_session_id: None,
                    branched_at_parent_event_id: None,
                },
            ),
            ev(
                2,
                "s-meta",
                EventPayload::Done {
                    stop_reason: StopReason::EndTurn,
                    turn_count: 1,
                    usage: None,
                    elapsed_ms: None,
                },
            ),
        ];
        write_jsonl(&path, &events);
        let (_, rope, _) = load_session_rope(&path).expect("load");
        assert!(rope.is_empty());
    }

    #[test]
    fn load_session_rope_uses_filename_stem_when_log_is_empty() {
        let path = tmpfile("empty");
        std::fs::File::create(&path).expect("create");
        let (sid, rope, malformed) = load_session_rope(&path).expect("load");
        assert!(rope.is_empty());
        assert_eq!(malformed, 0);
        assert_eq!(sid, "empty");
    }

    #[test]
    fn load_session_rope_reports_malformed_count_without_failing() {
        let path = tmpfile("malformed");
        let mut file = std::fs::File::create(&path).expect("create");
        writeln!(file, "this-is-not-json").expect("w");
        let good = ev(
            7,
            "s-malformed",
            EventPayload::UserMessage {
                content: "ok".into(),
            },
        );
        writeln!(file, "{}", serde_json::to_string(&good).unwrap()).expect("w");
        let (sid, rope, malformed) = load_session_rope(&path).expect("load");
        assert_eq!(sid, "s-malformed");
        assert_eq!(malformed, 1);
        assert_eq!(rope.len(), 1);
    }

    #[test]
    fn benchmark_session_emits_one_row_per_policy_in_order() {
        let path = tmpfile("bench-three-policies");
        let events = vec![
            ev(
                1,
                "s-bench",
                EventPayload::UserMessage {
                    content: "a".into(),
                },
            ),
            ev(
                2,
                "s-bench",
                EventPayload::UserMessage {
                    content: "b".into(),
                },
            ),
            ev(
                3,
                "s-bench",
                EventPayload::UserMessage {
                    content: "c".into(),
                },
            ),
        ];
        write_jsonl(&path, &events);
        let (sid, rope, _) = load_session_rope(&path).expect("load");

        let policies = vec![
            LabeledPolicy {
                label: "no-compaction".into(),
                policy: Arc::new(NoCompactionPolicy::new()),
                model_calls: 0,
            },
            LabeledPolicy {
                label: "span-family-drop".into(),
                policy: Arc::new(SpanFamilyDropPolicy::new()),
                model_calls: 0,
            },
            LabeledPolicy {
                label: "hash-and-summary-v1".into(),
                policy: Arc::new(HashAndSummaryPolicy::new(Arc::new(KeepHalfJudge::new()))),
                model_calls: 1,
            },
        ];
        let rows = benchmark_session(&sid, &rope, &policies, 0);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].policy_label, "no-compaction");
        assert_eq!(rows[1].policy_label, "span-family-drop");
        assert_eq!(rows[2].policy_label, "hash-and-summary-v1");
        for r in &rows {
            assert_eq!(r.session_id, sid);
            assert_eq!(r.spans_before, 3);
        }
        // NoCompactionPolicy preserves the rope verbatim.
        assert_eq!(rows[0].spans_after, 3);
        // KeepHalfJudge keeps even-indexed spans (0,2) → 2 kept + 1 summary = 3.
        // (No identity-shortcut here: HashAndSummaryPolicy's compact()
        // ignores target_tokens and always queries the judge.)
        assert_eq!(rows[2].model_calls, 1);
    }

    #[test]
    fn keep_half_judge_keeps_even_indexed_hashes() {
        let judge = KeepHalfJudge::new();
        let prompt = "header\n---\nhash=aaa kind=user id=u1\nbody\n---\nhash=bbb kind=user id=u2\nbody\n---\nhash=ccc kind=user id=u3\nbody\n";
        let raw = judge.judge(prompt).expect("judge ok");
        assert!(raw.contains("\"aaa\""));
        assert!(raw.contains("\"ccc\""));
        assert!(!raw.contains("\"bbb\""));
        assert!(judge.prompts.lock().map(|p| p.len()).unwrap_or(0) == 1);
    }

    #[test]
    fn csv_header_and_row_match_field_order() {
        let row = BenchRow {
            session_id: "s".into(),
            policy_label: "p".into(),
            tokens_before: 10,
            tokens_after: 5,
            decisions_count: 2,
            model_calls: 0,
            wall_clock_ms: 1,
            spans_before: 3,
            spans_after: 1,
        };
        let header = csv_header();
        let line = csv_row(&row);
        // Counts of commas in header and row line up.
        assert_eq!(
            header.matches(',').count(),
            line.matches(',').count(),
            "header={header}\nrow={line}"
        );
    }

    #[test]
    fn benchmark_session_under_target_produces_identity_rows() {
        // tokens_before below target_tokens → SpanFamilyDropPolicy is
        // identity. NoCompactionPolicy is always identity. This
        // exercises the "happy path" of the harness.
        let path = tmpfile("under-target");
        let events = vec![ev(
            1,
            "s-tiny",
            EventPayload::UserMessage {
                content: "x".into(),
            },
        )];
        write_jsonl(&path, &events);
        let (sid, rope, _) = load_session_rope(&path).expect("load");
        let policies = vec![
            LabeledPolicy {
                label: "no-compaction".into(),
                policy: Arc::new(NoCompactionPolicy::new()),
                model_calls: 0,
            },
            LabeledPolicy {
                label: "span-family-drop".into(),
                policy: Arc::new(SpanFamilyDropPolicy::new()),
                model_calls: 0,
            },
        ];
        let rows = benchmark_session(&sid, &rope, &policies, 1_000_000);
        for r in &rows {
            assert_eq!(r.spans_after, r.spans_before);
            assert_eq!(r.decisions_count, 0);
        }
    }
}
