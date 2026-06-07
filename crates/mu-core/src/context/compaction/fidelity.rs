//! Structural-fidelity metrics for compaction policies (mu-0fla, Layer 1).
//!
//! The metrics in [`super::bench`] answer "how much did the policy
//! shrink the rope?" — `tokens_after`, `spans_after`, `model_calls`,
//! `wall_clock_us`. They deliberately do **not** answer the question
//! that justifies a model-judge's per-compaction cost: "did the policy
//! keep the *load-bearing* content?" At a shared `target_tokens` the
//! heuristic and the judge keep *different* span sets (mu-0fla: 404 vs
//! 330 on the overnight rope) and nothing in a `BenchRow` says which set
//! retained the spans that mattered.
//!
//! This module is the first ("structural") of two fidelity layers:
//!
//! - **Layer 1 (this module) — structural retention.** Pure-CPU, no
//!   model call, fully deterministic. Classifies each pre-compaction
//!   span's *fate* (kept verbatim / absorbed into a summary / dropped
//!   wholesale) and rolls that up into regime-characterizing aggregates:
//!   did the policy keep the *goal* (the first user message)? what
//!   fraction of the *recent* tail survived verbatim? how does retention
//!   split by [`SpanKind`]? Together with the existing cost columns this
//!   gives a **fidelity-per-token** Pareto: `kept_tokens` (verbatim
//!   signal retained) against `tokens_after` (the cost of keeping it).
//!
//! - **Layer 2 (out of scope here) — downstream probe-question eval.**
//!   A model reads each compacted rope and answers hand-authored probe
//!   questions whose answers live in maybe-dropped spans; correctness is
//!   the fidelity signal. That layer needs a model and lives in the
//!   separate ollama-driven harness; it consumes the compacted ropes
//!   this Rust side produces.
//!
//! ## Why classify by rope-diff, not the decision log
//!
//! [`CompactionResult::decisions`] is a *SHOULD*-record audit log, not a
//! guarantee — [`super::NoCompactionPolicy`] records nothing, and a
//! fail-closed policy records a single `Failed` entry while keeping the
//! whole rope. So the authoritative source of "what survived" is the
//! **before/after span-id diff**: a span present in the after-rope was
//! kept verbatim; one absent was removed. We then consult
//! [`CompactionDecision::Summarized`] and any
//! [`SpanKind::CompactionSummary`] span's `absorbed_span_ids` only to
//! split the *removed* spans into "summarized" (retained compressed) vs
//! "dropped" (gone). This makes the fail-closed path read correctly as
//! "all kept" with no special-casing.

use std::collections::BTreeMap;
use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::{estimate_tokens, CompactionDecision, CompactionResult};
use crate::context::rope::{RetainedRope, Span, SpanKind};

/// Default recency windows (counts of most-recent spans) reported by
/// [`fidelity_report`]. The tail is where the heuristic claims its edge
/// (raw recent depth), so we sample shallow→deep to see the curve.
pub const DEFAULT_RECENCY_WINDOWS: &[usize] = &[10, 50, 100];

/// What happened to one pre-compaction span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanFate {
    /// Survived verbatim — its id is present in the post-compaction rope.
    Kept,
    /// Removed from the rope but absorbed into a summary span (its id
    /// appears in a `Summarized` decision or a `CompactionSummary`
    /// span's `absorbed_span_ids`). Retained compressed, not gone.
    Summarized,
    /// Removed wholesale — neither kept nor absorbed into any summary.
    Dropped,
}

impl SpanFate {
    /// Short stable identifier matching the serde snake_case wire name.
    pub fn label(self) -> &'static str {
        match self {
            SpanFate::Kept => "kept",
            SpanFate::Summarized => "summarized",
            SpanFate::Dropped => "dropped",
        }
    }
}

/// Retention split for one [`SpanKind`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindFidelity {
    pub kept: usize,
    pub summarized: usize,
    pub dropped: usize,
}

/// Verbatim retention over the most-recent `considered` spans of the
/// pre-compaction rope. `window` is the requested size; `considered` is
/// `min(window, rope_len)` (a small session's tail is the whole rope).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecencyPoint {
    /// The requested tail size (one of [`DEFAULT_RECENCY_WINDOWS`]).
    pub window: usize,
    /// Spans actually examined: `min(window, before_rope_len)`.
    pub considered: usize,
    /// How many of the `considered` tail spans were kept verbatim.
    pub kept: usize,
}

impl RecencyPoint {
    /// Fraction of the considered tail kept verbatim, in `[0.0, 1.0]`.
    /// Computed on demand so the stored struct stays `Eq` (no `f64`
    /// field — `BenchRow` derives `Eq`).
    pub fn kept_fraction(self) -> f64 {
        if self.considered == 0 {
            0.0
        } else {
            self.kept as f64 / self.considered as f64
        }
    }
}

/// Structural-fidelity rollup for one policy run against one rope.
///
/// All counts are over the **pre-compaction** rope's spans (the summary
/// span a policy *adds* is not a pre-compaction span and is not counted
/// here — it's visible in `spans_after`/`tokens_after` on the row).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FidelityReport {
    /// Fate of the *goal*: the first `User`-kind span in the
    /// pre-compaction rope (the task statement). `None` if the session
    /// has no user message.
    pub goal_fate: Option<SpanFate>,
    /// Spans kept verbatim.
    pub kept_spans: usize,
    /// Spans absorbed into a summary.
    pub summarized_spans: usize,
    /// Spans dropped wholesale.
    pub dropped_spans: usize,
    /// Estimated tokens across the verbatim-kept spans — the "raw signal
    /// retained" axis of the fidelity-per-token Pareto. Uses the shared
    /// [`estimate_tokens`] so it's comparable to `tokens_after`.
    pub kept_tokens: usize,
    /// Retention split per [`SpanKind::label`]. Kinds with no spans are
    /// absent.
    pub per_kind: BTreeMap<String, KindFidelity>,
    /// Verbatim retention over recent tails of the rope.
    pub recency: Vec<RecencyPoint>,
}

impl FidelityReport {
    /// Verbatim-kept fraction for the recency point with the requested
    /// `window`, or `None` if no such window was computed. Callers must
    /// handle the `None` explicitly rather than collapsing a *missing*
    /// window into a *kept-nothing* `0.0` — they are different facts.
    pub fn recency_kept_fraction(&self, window: usize) -> Option<f64> {
        self.recency
            .iter()
            .find(|p| p.window == window)
            .map(|p| p.kept_fraction())
    }
}

/// Classify every pre-compaction span's [`SpanFate`], aligned 1:1 with
/// `before.spans()` order.
///
/// Authoritative source is the before/after span-id diff (see module
/// docs); the decision log and summary spans only split *removed* spans
/// into summarized-vs-dropped.
pub fn classify_fates(before: &RetainedRope, result: &CompactionResult) -> Vec<SpanFate> {
    let after_ids: HashSet<&str> = result.rope.spans().iter().map(|s| s.id()).collect();

    // Collect ids absorbed into a summary, from both the decision log
    // and the CompactionSummary spans in the after-rope (a policy may
    // record either, both, or — for a terse impl — only the span).
    // Borrowed (`&str`) from `result`, which outlives this function — no
    // per-id allocation.
    let mut absorbed_ids: HashSet<&str> = HashSet::new();
    for d in &result.decisions {
        if let CompactionDecision::Summarized {
            absorbed_span_ids, ..
        } = d
        {
            absorbed_ids.extend(absorbed_span_ids.iter().map(String::as_str));
        }
    }
    for s in result.rope.spans() {
        if let SpanKind::CompactionSummary {
            absorbed_span_ids, ..
        } = s.kind()
        {
            absorbed_ids.extend(absorbed_span_ids.iter().map(AsRef::as_ref));
        }
    }

    before
        .spans()
        .iter()
        .map(|s| {
            if after_ids.contains(s.id()) {
                SpanFate::Kept
            } else if absorbed_ids.contains(s.id()) {
                SpanFate::Summarized
            } else {
                SpanFate::Dropped
            }
        })
        .collect()
}

/// Compute the structural [`FidelityReport`] for one policy run.
///
/// `recency_windows` are the requested tail sizes (pass
/// [`DEFAULT_RECENCY_WINDOWS`] for the standard sweep).
pub fn fidelity_report(
    before: &RetainedRope,
    result: &CompactionResult,
    recency_windows: &[usize],
) -> FidelityReport {
    let spans = before.spans();
    let fates = classify_fates(before, result);

    let mut report = FidelityReport {
        // Goal = first user-kind span's fate (None if no user message).
        goal_fate: spans
            .iter()
            .position(|s| matches!(s.kind(), SpanKind::User))
            .map(|i| fates[i]),
        ..FidelityReport::default()
    };

    for (span, fate) in spans.iter().zip(&fates) {
        let entry = report
            .per_kind
            .entry(span.kind().label().to_string())
            .or_default();
        match fate {
            SpanFate::Kept => {
                report.kept_spans += 1;
                entry.kept += 1;
            }
            SpanFate::Summarized => {
                report.summarized_spans += 1;
                entry.summarized += 1;
            }
            SpanFate::Dropped => {
                report.dropped_spans += 1;
                entry.dropped += 1;
            }
        }
    }

    // Token estimate over the verbatim-kept spans in ONE batched call —
    // estimate_tokens takes a slice precisely to amortize encoder setup
    // over many spans. Comparable to tokens_after (same estimator).
    let kept_spans: Vec<Span> = spans
        .iter()
        .zip(&fates)
        .filter(|(_, f)| matches!(f, SpanFate::Kept))
        .map(|(s, _)| s.clone())
        .collect();
    report.kept_tokens = estimate_tokens(&kept_spans);

    // Recency: how much of each recent tail survived verbatim.
    let len = spans.len();
    for &window in recency_windows {
        let considered = window.min(len);
        let kept = fates[len - considered..]
            .iter()
            .filter(|f| matches!(f, SpanFate::Kept))
            .count();
        report.recency.push(RecencyPoint {
            window,
            considered,
            kept,
        });
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::compaction::heuristic::SpanFamilyDropPolicy;
    use crate::context::compaction::{CompactionPolicy, NoCompactionPolicy};
    use crate::context::rope::{RetainedRope, RetentionClass, Span, SpanKind};

    /// Build a rope from `(kind, id, content)` triples in order.
    fn rope_of(specs: &[(SpanKind, &str, &str)]) -> RetainedRope {
        let spans: Vec<Span> = specs
            .iter()
            .map(|(kind, id, content)| Span::new(*id, kind.clone(), *content, RetentionClass::Cold))
            .collect();
        RetainedRope::from_spans(spans)
    }

    #[test]
    fn no_compaction_keeps_everything() {
        let before = rope_of(&[
            (SpanKind::User, "u1", "the goal"),
            (SpanKind::Assistant, "a1", "ok"),
            (SpanKind::ToolResult, "t1", "result"),
        ]);
        let result = NoCompactionPolicy::new().compact(&before, 1_000_000);
        let report = fidelity_report(&before, &result, DEFAULT_RECENCY_WINDOWS);

        assert_eq!(report.goal_fate, Some(SpanFate::Kept));
        assert_eq!(report.kept_spans, 3);
        assert_eq!(report.summarized_spans, 0);
        assert_eq!(report.dropped_spans, 0);
        assert!(report.kept_tokens > 0);
        // Every recency window is fully retained.
        for rp in &report.recency {
            assert_eq!(rp.kept, rp.considered);
            assert!((rp.kept_fraction() - 1.0).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn classify_splits_kept_summarized_dropped() {
        // before: u1, a1, t1, t2
        let before = rope_of(&[
            (SpanKind::User, "u1", "goal"),
            (SpanKind::Assistant, "a1", "keep me"),
            (SpanKind::ToolResult, "t1", "absorbed"),
            (SpanKind::ToolResult, "t2", "gone"),
        ]);
        // after: u1, a1 survive verbatim; a CompactionSummary absorbs t1;
        // t2 is dropped (absent, not absorbed).
        let after = RetainedRope::from_spans(vec![
            Span::new("u1", SpanKind::User, "goal", RetentionClass::Cold),
            Span::new("a1", SpanKind::Assistant, "keep me", RetentionClass::Cold),
            Span::new(
                "sum1",
                SpanKind::CompactionSummary {
                    absorbed_span_ids: vec!["t1".into()],
                    generated_at_unix_ms: 0,
                    policy_id: "test".into(),
                },
                "summary of t1",
                RetentionClass::Cold,
            ),
        ]);
        let result = CompactionResult {
            rope: after,
            decisions: vec![CompactionDecision::Summarized {
                absorbed_span_ids: vec!["t1".into()],
                summary_span_id: "sum1".into(),
            }],
            tokens_before: 0,
            tokens_after: 0,
            wall_clock_us: 0,
        };

        let fates = classify_fates(&before, &result);
        assert_eq!(
            fates,
            vec![
                SpanFate::Kept,
                SpanFate::Kept,
                SpanFate::Summarized,
                SpanFate::Dropped
            ]
        );

        let report = fidelity_report(&before, &result, &[2, 4]);
        assert_eq!(report.kept_spans, 2);
        assert_eq!(report.summarized_spans, 1);
        assert_eq!(report.dropped_spans, 1);
        assert_eq!(report.goal_fate, Some(SpanFate::Kept));

        // per-kind: user kept, assistant kept, tool_result 1 summarized + 1 dropped.
        let tr = report
            .per_kind
            .get("tool_result")
            .expect("tool_result kind");
        assert_eq!(tr.summarized, 1);
        assert_eq!(tr.dropped, 1);
        assert_eq!(tr.kept, 0);

        // recency window 2 = last two spans (t1 summarized, t2 dropped) → 0 kept.
        let r2 = report.recency.iter().find(|r| r.window == 2).unwrap();
        assert_eq!(r2.considered, 2);
        assert_eq!(r2.kept, 0);
        // window 4 = whole rope → 2 kept.
        let r4 = report.recency.iter().find(|r| r.window == 4).unwrap();
        assert_eq!(r4.kept, 2);
    }

    #[test]
    fn absorbed_recovered_from_summary_span_when_decision_log_empty() {
        // A terse policy that records NO decisions but DOES emit a
        // CompactionSummary span — fate must still resolve via the span.
        let before = rope_of(&[
            (SpanKind::User, "u1", "goal"),
            (SpanKind::ToolResult, "t1", "absorbed"),
        ]);
        let after = RetainedRope::from_spans(vec![
            Span::new("u1", SpanKind::User, "goal", RetentionClass::Cold),
            Span::new(
                "sum1",
                SpanKind::CompactionSummary {
                    absorbed_span_ids: vec!["t1".into()],
                    generated_at_unix_ms: 0,
                    policy_id: "test".into(),
                },
                "summary",
                RetentionClass::Cold,
            ),
        ]);
        let result = CompactionResult {
            rope: after,
            decisions: vec![], // intentionally empty
            tokens_before: 0,
            tokens_after: 0,
            wall_clock_us: 0,
        };
        let fates = classify_fates(&before, &result);
        assert_eq!(fates, vec![SpanFate::Kept, SpanFate::Summarized]);
    }

    #[test]
    fn no_user_span_yields_none_goal() {
        let before = rope_of(&[
            (SpanKind::System, "s1", "sys"),
            (SpanKind::Assistant, "a1", "hi"),
        ]);
        let result = NoCompactionPolicy::new().compact(&before, 1_000_000);
        let report = fidelity_report(&before, &result, DEFAULT_RECENCY_WINDOWS);
        assert_eq!(report.goal_fate, None);
    }

    #[test]
    fn heuristic_drop_marks_dropped_spans() {
        // Real policy: a rope above target should drop something, and
        // those drops must classify as Dropped (heuristic drops
        // wholesale — no summary span).
        let mut spans: Vec<Span> = Vec::new();
        for i in 0..40 {
            spans.push(Span::new(
                format!("t{i}"),
                SpanKind::ToolResult,
                "x".repeat(500), // chunky so we exceed a small target
                RetentionClass::Cold,
            ));
        }
        let before = RetainedRope::from_spans(spans);
        let result = SpanFamilyDropPolicy::new().compact(&before, 100);
        let report = fidelity_report(&before, &result, DEFAULT_RECENCY_WINDOWS);
        // Something was dropped, nothing summarized (heuristic has no summary).
        assert!(
            report.dropped_spans > 0,
            "expected some drops at target=100"
        );
        assert_eq!(report.summarized_spans, 0);
        assert_eq!(
            report.kept_spans + report.dropped_spans,
            before.len(),
            "every span is kept or dropped (no summary)"
        );
    }
}
