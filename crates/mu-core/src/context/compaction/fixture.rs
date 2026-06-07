//! JSON fixtures of compacted ropes for the downstream probe-question
//! eval (mu-0fla, Layer 2).
//!
//! Layer 1 ([`super::fidelity`]) answers *structurally* what each policy
//! kept. Layer 2 answers it *behaviorally*: a downstream model reads a
//! compacted rope and answers hand-authored probe questions whose
//! answers live in maybe-dropped spans; correctness is the fidelity
//! signal. That eval runs in a separate ollama-driven harness — but it
//! needs the actual **content** of each compacted rope, which the
//! metrics-only [`super::bench::BenchRow`] does not carry.
//!
//! This module emits that content as a serializable [`RopeFixture`]: the
//! post-compaction rope exactly as the model would see it
//! (`compacted_spans`), plus the pre-compaction spans that did *not*
//! survive verbatim (`removed_spans`, tagged Summarized/Dropped) so a
//! probe author knows what content is at risk and can target it. The
//! `fidelity` rollup rides along for reference.

use serde::{Deserialize, Serialize};

use super::fidelity::{
    classify_fates, fidelity_report, FidelityReport, SpanFate, DEFAULT_RECENCY_WINDOWS,
};
use super::CompactionResult;
use crate::context::rope::RetainedRope;

/// One span as it appears in a fixture — enough for the probe harness to
/// reconstruct the model-visible context, or to know what was lost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixtureSpan {
    /// Stable span id (within the source rope).
    pub id: String,
    /// `SpanKind::label` — `user`, `assistant`, `tool_result`,
    /// `compaction_summary`, etc.
    pub kind: String,
    /// The span's literal text content.
    pub content: String,
    /// Fate of this span. Omitted for `compacted_spans` (they are, by
    /// definition, present in the post-rope); set to `Summarized` or
    /// `Dropped` for `removed_spans`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fate: Option<SpanFate>,
}

/// A compacted rope plus what it lost, for one `(session, policy,
/// target)` — the unit the Layer-2 probe harness consumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RopeFixture {
    /// Source session id.
    pub session_id: String,
    /// Policy that produced this compaction (caller-supplied — the
    /// `CompactionResult` does not carry its own label).
    pub policy_label: String,
    /// `target_tokens` the policy was asked to hit.
    pub target_tokens: usize,
    /// Pre-compaction token count (policy-measured).
    pub tokens_before: usize,
    /// Post-compaction token count (policy-measured).
    pub tokens_after: usize,
    /// The post-compaction rope — exactly the context the downstream
    /// model would see. Probe questions are answered against THIS.
    pub compacted_spans: Vec<FixtureSpan>,
    /// Pre-compaction spans NOT kept verbatim (Summarized or Dropped) —
    /// the content a probe can target to test whether compaction lost
    /// something load-bearing.
    pub removed_spans: Vec<FixtureSpan>,
    /// Structural-fidelity rollup ([`super::fidelity`]) for reference.
    pub fidelity: FidelityReport,
}

/// Build a [`RopeFixture`] from a `(before-rope, result)` pair.
///
/// `policy_label` is caller-supplied (mirrors
/// [`super::bench::LabeledPolicy::label`]). The compacted rope is taken
/// verbatim from `result.rope`; `removed_spans` are the pre-compaction
/// spans whose [`SpanFate`] is not `Kept`.
pub fn rope_fixture(
    session_id: &str,
    policy_label: &str,
    before: &RetainedRope,
    result: &CompactionResult,
    target_tokens: usize,
) -> RopeFixture {
    // We classify here for `removed_spans` and `fidelity_report`
    // re-classifies internally — one redundant O(spans) pass. Left as-is
    // deliberately: this is an offline fixture generator (cold path), and
    // threading precomputed fates into the public `fidelity_report`
    // signature would bloat it for no runtime benefit on the live loop.
    let fates = classify_fates(before, result);

    let removed_spans: Vec<FixtureSpan> = before
        .spans()
        .iter()
        .zip(&fates)
        .filter(|(_, f)| !matches!(f, SpanFate::Kept))
        .map(|(s, f)| FixtureSpan {
            id: s.id().to_string(),
            kind: s.kind().label().to_string(),
            content: s.content().to_string(),
            fate: Some(*f),
        })
        .collect();

    let compacted_spans: Vec<FixtureSpan> = result
        .rope
        .spans()
        .iter()
        .map(|s| FixtureSpan {
            id: s.id().to_string(),
            kind: s.kind().label().to_string(),
            content: s.content().to_string(),
            fate: None,
        })
        .collect();

    RopeFixture {
        session_id: session_id.to_string(),
        policy_label: policy_label.to_string(),
        target_tokens,
        tokens_before: result.tokens_before,
        tokens_after: result.tokens_after,
        compacted_spans,
        removed_spans,
        fidelity: fidelity_report(before, result, DEFAULT_RECENCY_WINDOWS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::compaction::heuristic::SpanFamilyDropPolicy;
    use crate::context::compaction::{CompactionPolicy, NoCompactionPolicy};
    use crate::context::rope::{RetainedRope, RetentionClass, Span, SpanKind};

    fn rope_of(specs: &[(SpanKind, &str, &str)]) -> RetainedRope {
        let spans: Vec<Span> = specs
            .iter()
            .map(|(kind, id, content)| Span::new(*id, kind.clone(), *content, RetentionClass::Cold))
            .collect();
        RetainedRope::from_spans(spans)
    }

    #[test]
    fn no_compaction_fixture_has_all_spans_and_no_removals() {
        let before = rope_of(&[
            (SpanKind::User, "u1", "the goal"),
            (SpanKind::Assistant, "a1", "ok"),
        ]);
        let result = NoCompactionPolicy::new().compact(&before, 1_000_000);
        let fx = rope_fixture("s1", "no-compaction", &before, &result, 1_000_000);

        assert_eq!(fx.session_id, "s1");
        assert_eq!(fx.policy_label, "no-compaction");
        assert_eq!(fx.compacted_spans.len(), 2);
        assert!(fx.removed_spans.is_empty());
        // Content preserved verbatim, in order.
        assert_eq!(fx.compacted_spans[0].content, "the goal");
        assert_eq!(fx.compacted_spans[0].kind, "user");
        // compacted spans carry no fate; round-trips through serde.
        assert!(fx.compacted_spans[0].fate.is_none());
        let json = serde_json::to_string(&fx).expect("serialize");
        let back: RopeFixture = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, fx);
    }

    #[test]
    fn heuristic_fixture_records_dropped_content() {
        // Chunky rope above a tiny target → the heuristic drops spans
        // wholesale; those must land in removed_spans tagged Dropped,
        // WITH their content (so a probe can target them).
        let mut spans: Vec<Span> = Vec::new();
        for i in 0..30 {
            spans.push(Span::new(
                format!("t{i}"),
                SpanKind::ToolResult,
                format!("unique-content-marker-{i} {}", "x".repeat(400)),
                RetentionClass::Cold,
            ));
        }
        let before = RetainedRope::from_spans(spans);
        let result = SpanFamilyDropPolicy::new().compact(&before, 100);
        let fx = rope_fixture("s2", "span-family-drop", &before, &result, 100);

        assert!(!fx.removed_spans.is_empty(), "expected drops at target=100");
        // Every removed span is Dropped (heuristic has no summary) and
        // carries its original content.
        for rs in &fx.removed_spans {
            assert_eq!(rs.fate, Some(SpanFate::Dropped));
            assert!(rs.content.contains("unique-content-marker-"));
        }
        // compacted + removed accounts for every pre-compaction span
        // (heuristic adds no summary span).
        assert_eq!(
            fx.compacted_spans.len() + fx.removed_spans.len(),
            before.len()
        );
        // Headline token counts mirror the result.
        assert_eq!(fx.tokens_before, result.tokens_before);
        assert_eq!(fx.tokens_after, result.tokens_after);
    }

    #[test]
    fn summary_span_lands_in_compacted_absorbed_lands_in_removed() {
        // The hash-and-summary case (the interesting one): a synthetic
        // CompactionSummary span is NOT a pre-compaction span, so it must
        // appear in `compacted_spans` (fate None) — never `removed_spans`;
        // and the spans it absorbed must appear in `removed_spans` tagged
        // Summarized, WITH their original content.
        use crate::context::compaction::CompactionDecision;
        use crate::context::compaction::CompactionResult;

        let before = rope_of(&[
            (SpanKind::User, "u1", "goal"),
            (SpanKind::ToolResult, "t1", "absorbed-content"),
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
        let fx = rope_fixture("s3", "hash-and-summary", &before, &result, 0);

        // Summary span is present in compacted_spans, with no fate.
        let summary = fx
            .compacted_spans
            .iter()
            .find(|s| s.id == "sum1")
            .expect("summary span in compacted_spans");
        assert_eq!(summary.kind, "compaction_summary");
        assert!(summary.fate.is_none());
        // ...and never in removed_spans.
        assert!(fx.removed_spans.iter().all(|s| s.id != "sum1"));

        // The absorbed span is in removed_spans, tagged Summarized, with
        // its original content (a probe can target it).
        assert_eq!(fx.removed_spans.len(), 1);
        let t1 = &fx.removed_spans[0];
        assert_eq!(t1.id, "t1");
        assert_eq!(t1.fate, Some(SpanFate::Summarized));
        assert_eq!(t1.content, "absorbed-content");
    }
}
