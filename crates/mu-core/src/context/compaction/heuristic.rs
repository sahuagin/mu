//! Heuristic compaction policy — drops spans by `SpanKind` priority,
//! zero model calls.
//!
//! mu-kgu.2 — replaces the Phase 1 stub. The trait surface is locked
//! by mu-kgu.1; this module fills in the priority-ordered eviction
//! logic.
//!
//! ## Drop priority (lowest → highest survival)
//!
//! Per the mu-kgu.2 bead body:
//!
//! 1. **Stale `FileLoad` spans** — oldest first. v1 proxy: span
//!    position (earlier = older). Future: mtime-based staleness.
//! 2. **Old `ToolCall`/`ToolResult` clusters** — adjacent runs of
//!    tool-call/tool-result spans are dropped as a unit, oldest
//!    cluster first. v1 proxy for "paired by `call_id`": the rope
//!    layer carries no `call_id` field, so adjacency is the
//!    structural pairing rule.
//! 3. **Old `Assistant` turns** — older than the [`KEEP_RECENT_ASSISTANT`]
//!    most-recent assistant spans.
//! 4. **`SkillActivation`** — oldest first. v1 proxy: in the current
//!    rope substrate [`super::super::rope::RetainedRope::deactivate_skill`]
//!    already removes deactivated spans from the active set, so this
//!    tier is mostly a no-op until provenance-driven staleness lands.
//!
//! Preserved (never dropped by v1): `System`, `ToolSchema`, the
//! [`KEEP_RECENT_ASSISTANT`] most-recent `Assistant` spans,
//! `MemoryInjection`, `User`, `Compaction`.
//!
//! ## Token measurement (v1)
//!
//! No tokenizer is wired into mu-core today; the policy uses
//! `content.chars().count()` as the per-span size proxy. Swapping in
//! a real tokenizer later is a one-function change ([`span_size`]).
//!
//! ## Provenance
//!
//! The compacted rope is built via
//! [`super::super::rope::RetainedRope::from_spans`], which drops the
//! event log. v1 accepts this loss — the parent bead documents
//! compaction correctness as fuzzy, not byte-for-byte, and surviving
//! spans were originated by events outside the compacted rope's
//! lifetime anyway. A future bead can extend rope.rs with a
//! span-filter primitive that preserves provenance.

use std::collections::HashSet;
use std::time::Instant;

use super::{CompactionDecision, CompactionPolicy, CompactionResult};
use crate::context::rope::{RetainedRope, Span, SpanKind};

/// Number of most-recent `Assistant` spans preserved across a
/// compaction pass. Hardcoded for v1 per the bead's "Out" list
/// ("Configurable priority. v1 hardcodes priority; future bead can
/// add config.").
const KEEP_RECENT_ASSISTANT: usize = 2;

/// Drop spans by `SpanKind` priority. Zero model calls.
///
/// See module docs for the priority table.
#[derive(Debug, Default, Clone, Copy)]
pub struct SpanFamilyDropPolicy;

impl SpanFamilyDropPolicy {
    pub fn new() -> Self {
        Self
    }
}

/// v1 token estimate: char-count — matches the shared
/// [`super::estimate_tokens`] semantics so cross-policy benchmarks
/// compare like-for-like. Real tokenizer swaps in here when one lands.
fn span_size(span: &Span) -> usize {
    span.content.chars().count()
}

/// Mark `idx` as dropped with `reason`. Idempotent.
fn record_drop(
    idx: usize,
    reason: &str,
    spans: &[Span],
    sizes: &[usize],
    dropped: &mut [bool],
    tokens_after: &mut usize,
    decisions: &mut Vec<CompactionDecision>,
) {
    if dropped[idx] {
        return;
    }
    dropped[idx] = true;
    *tokens_after = tokens_after.saturating_sub(sizes[idx]);
    decisions.push(CompactionDecision::Dropped {
        span_id: spans[idx].id.clone(),
        reason: reason.to_string(),
    });
}

/// Group consecutive `ToolCall` / `ToolResult` spans into clusters.
/// Each cluster is a `Vec<usize>` of indices in iteration order.
fn tool_clusters(spans: &[Span]) -> Vec<Vec<usize>> {
    let mut clusters = Vec::new();
    let mut i = 0;
    while i < spans.len() {
        if matches!(spans[i].kind, SpanKind::ToolCall | SpanKind::ToolResult) {
            let mut cluster = Vec::new();
            while i < spans.len()
                && matches!(spans[i].kind, SpanKind::ToolCall | SpanKind::ToolResult)
            {
                cluster.push(i);
                i += 1;
            }
            clusters.push(cluster);
        } else {
            i += 1;
        }
    }
    clusters
}

impl CompactionPolicy for SpanFamilyDropPolicy {
    fn compact(&self, rope: &RetainedRope, target_tokens: usize) -> CompactionResult {
        let start = Instant::now();
        let spans = rope.spans();
        let n = spans.len();
        let sizes: Vec<usize> = spans.iter().map(span_size).collect();
        let tokens_before: usize = sizes.iter().sum();

        if n == 0 || tokens_before <= target_tokens {
            return CompactionResult {
                rope: rope.clone(),
                decisions: Vec::new(),
                tokens_before,
                tokens_after: tokens_before,
                wall_clock_us: start.elapsed().as_micros() as u64,
            };
        }

        let mut dropped = vec![false; n];
        let mut decisions: Vec<CompactionDecision> = Vec::new();
        let mut tokens_after = tokens_before;

        let assistant_indices: Vec<usize> = spans
            .iter()
            .enumerate()
            .filter(|(_, s)| s.kind == SpanKind::Assistant)
            .map(|(i, _)| i)
            .collect();
        let preserved_assistants: HashSet<usize> = assistant_indices
            .iter()
            .rev()
            .take(KEEP_RECENT_ASSISTANT)
            .copied()
            .collect();

        // Tier 1: stale FileLoad, oldest (smallest index) first.
        for i in 0..n {
            if tokens_after <= target_tokens {
                break;
            }
            if spans[i].kind == SpanKind::FileLoad {
                record_drop(
                    i,
                    "stale file-load (v1: oldest first)",
                    spans,
                    &sizes,
                    &mut dropped,
                    &mut tokens_after,
                    &mut decisions,
                );
            }
        }

        // Tier 2: ToolCall/ToolResult clusters (adjacent), oldest cluster first.
        let clusters = tool_clusters(spans);
        for cluster in &clusters {
            if tokens_after <= target_tokens {
                break;
            }
            for &idx in cluster {
                record_drop(
                    idx,
                    "old tool call/result cluster",
                    spans,
                    &sizes,
                    &mut dropped,
                    &mut tokens_after,
                    &mut decisions,
                );
            }
        }

        // Tier 3: old Assistant turns (not in preserved-recent set).
        for i in 0..n {
            if tokens_after <= target_tokens {
                break;
            }
            if spans[i].kind == SpanKind::Assistant && !preserved_assistants.contains(&i) {
                record_drop(
                    i,
                    "old assistant turn",
                    spans,
                    &sizes,
                    &mut dropped,
                    &mut tokens_after,
                    &mut decisions,
                );
            }
        }

        // Tier 4: SkillActivation, oldest first.
        for i in 0..n {
            if tokens_after <= target_tokens {
                break;
            }
            if spans[i].kind == SpanKind::SkillActivation {
                record_drop(
                    i,
                    "stale skill activation (v1: oldest first)",
                    spans,
                    &sizes,
                    &mut dropped,
                    &mut tokens_after,
                    &mut decisions,
                );
            }
        }

        let survivors: Vec<Span> = spans
            .iter()
            .enumerate()
            .filter(|(i, _)| !dropped[*i])
            .map(|(_, s)| s.clone())
            .collect();

        CompactionResult {
            rope: RetainedRope::from_spans(survivors),
            decisions,
            tokens_before,
            tokens_after,
            wall_clock_us: start.elapsed().as_micros() as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::rope::{RetentionClass, Span, SpanKind};

    fn span(id: &str, kind: SpanKind, content: &str) -> Span {
        Span::new(id, kind, content, RetentionClass::Warm)
    }

    fn dropped_ids(result: &CompactionResult) -> Vec<&str> {
        result
            .decisions
            .iter()
            .filter_map(|d| match d {
                CompactionDecision::Dropped { span_id, .. } => Some(span_id.as_str()),
                _ => None,
            })
            .collect()
    }

    fn surviving_ids(result: &CompactionResult) -> Vec<&str> {
        result.rope.spans().iter().map(|s| s.id.as_str()).collect()
    }

    #[test]
    fn empty_rope_is_identity() {
        let rope = RetainedRope::new();
        let r = SpanFamilyDropPolicy::new().compact(&rope, 0);
        assert!(r.rope.is_empty());
        assert!(r.decisions.is_empty());
        assert_eq!(r.tokens_before, 0);
        assert_eq!(r.tokens_after, 0);
    }

    #[test]
    fn under_target_returns_rope_unchanged_with_metrics() {
        // Two small spans, total ~5 chars. Target 1_000.
        let rope = RetainedRope::from_spans(vec![
            span("sys", SpanKind::System, "you"),
            span("u1", SpanKind::User, "hi"),
        ]);
        let r = SpanFamilyDropPolicy::new().compact(&rope, 1_000);
        assert_eq!(surviving_ids(&r), vec!["sys", "u1"]);
        assert!(r.decisions.is_empty(), "no drops below target");
        assert_eq!(r.tokens_before, 5);
        assert_eq!(r.tokens_after, 5);
    }

    #[test]
    fn idempotent_when_already_under_target() {
        let rope = RetainedRope::from_spans(vec![span("sys", SpanKind::System, "y")]);
        let r1 = SpanFamilyDropPolicy::new().compact(&rope, 100);
        let r2 = SpanFamilyDropPolicy::new().compact(&r1.rope, 100);
        assert_eq!(r1.rope.spans(), r2.rope.spans());
    }

    #[test]
    fn preserves_system_and_tool_schema_when_dropping() {
        // 4 spans each of size 10. Target 25 → must drop 2.
        // FileLoad is the only droppable tier present.
        let rope = RetainedRope::from_spans(vec![
            span("sys", SpanKind::System, "0123456789"),
            span("ts", SpanKind::ToolSchema, "0123456789"),
            span("f1", SpanKind::FileLoad, "0123456789"),
            span("f2", SpanKind::FileLoad, "0123456789"),
        ]);
        let r = SpanFamilyDropPolicy::new().compact(&rope, 25);
        let survivors = surviving_ids(&r);
        assert!(survivors.contains(&"sys"), "System must survive");
        assert!(survivors.contains(&"ts"), "ToolSchema must survive");
        // At least one FileLoad dropped; oldest goes first.
        let drops = dropped_ids(&r);
        assert!(drops.contains(&"f1"), "oldest FileLoad dropped first");
    }

    #[test]
    fn file_loads_dropped_before_tool_results() {
        // Two FileLoads + a ToolCall/Result pair, each size 10.
        // Target 25 → 15 chars of drops needed. FileLoad tier exhausted
        // first; tool cluster only touched if necessary.
        let rope = RetainedRope::from_spans(vec![
            span("f1", SpanKind::FileLoad, "0123456789"),
            span("tc1", SpanKind::ToolCall, "0123456789"),
            span("tr1", SpanKind::ToolResult, "0123456789"),
            span("f2", SpanKind::FileLoad, "0123456789"),
        ]);
        let r = SpanFamilyDropPolicy::new().compact(&rope, 25);
        let drops = dropped_ids(&r);
        // Both FileLoads should drop before any tool span is touched.
        assert!(drops.contains(&"f1"), "f1 dropped in tier 1");
        assert!(drops.contains(&"f2"), "f2 dropped in tier 1");
        // Tool cluster preserved (15 chars of FileLoad drops were enough).
        let survivors = surviving_ids(&r);
        assert!(survivors.contains(&"tc1"));
        assert!(survivors.contains(&"tr1"));
    }

    #[test]
    fn tool_call_and_result_drop_as_a_cluster() {
        // No FileLoad to drain; tool cluster is the only droppable tier.
        // Sizes: sys=3, tc1=10, tr1=10, a1=10, a2=10. Total=43. Target=35.
        // a1/a2 are the two most recent assistants → preserved.
        // Cluster (tc1,tr1)=20 drops together.
        let rope = RetainedRope::from_spans(vec![
            span("sys", SpanKind::System, "sys"),
            span("tc1", SpanKind::ToolCall, "0123456789"),
            span("tr1", SpanKind::ToolResult, "0123456789"),
            span("a1", SpanKind::Assistant, "0123456789"),
            span("a2", SpanKind::Assistant, "0123456789"),
        ]);
        let r = SpanFamilyDropPolicy::new().compact(&rope, 35);
        let drops = dropped_ids(&r);
        assert!(drops.contains(&"tc1"), "tc1 dropped");
        assert!(drops.contains(&"tr1"), "tr1 dropped");
        let survivors = surviving_ids(&r);
        assert_eq!(survivors, vec!["sys", "a1", "a2"]);
    }

    #[test]
    fn preserves_two_most_recent_assistant_turns() {
        // Five Assistant turns; only a4/a5 (most recent 2) survive.
        // Sizes 10 each → 50 total. Target 25 → drop 25+ chars
        // of Assistant tier.
        let rope = RetainedRope::from_spans(vec![
            span("a1", SpanKind::Assistant, "0123456789"),
            span("a2", SpanKind::Assistant, "0123456789"),
            span("a3", SpanKind::Assistant, "0123456789"),
            span("a4", SpanKind::Assistant, "0123456789"),
            span("a5", SpanKind::Assistant, "0123456789"),
        ]);
        let r = SpanFamilyDropPolicy::new().compact(&rope, 25);
        let survivors = surviving_ids(&r);
        assert!(survivors.contains(&"a4"));
        assert!(survivors.contains(&"a5"));
        let drops = dropped_ids(&r);
        // a1 is oldest → first to drop.
        assert!(drops.contains(&"a1"), "oldest Assistant dropped first");
    }

    #[test]
    fn drops_recorded_with_specific_reasons() {
        // One of each droppable kind; tiny target forces multi-tier drops.
        // Three Assistants so KEEP_RECENT_ASSISTANT=2 preserves a_mid +
        // a_keep and a_old is exposed for the Tier 3 reason check.
        let rope = RetainedRope::from_spans(vec![
            span("f", SpanKind::FileLoad, "xxxxx"),
            span("tc", SpanKind::ToolCall, "xxxxx"),
            span("tr", SpanKind::ToolResult, "xxxxx"),
            span("a_old", SpanKind::Assistant, "xxxxx"),
            span("a_mid", SpanKind::Assistant, "xxxxx"),
            span("a_keep", SpanKind::Assistant, "xxxxx"),
            span("sk", SpanKind::SkillActivation, "xxxxx"),
        ]);
        let r = SpanFamilyDropPolicy::new().compact(&rope, 0);
        // Every reason string contains the tier marker phrase.
        let reasons: Vec<String> = r
            .decisions
            .iter()
            .filter_map(|d| match d {
                CompactionDecision::Dropped { span_id, reason } => {
                    Some(format!("{span_id}={reason}"))
                }
                _ => None,
            })
            .collect();
        let joined = reasons.join("|");
        assert!(joined.contains("f=stale file-load"));
        assert!(joined.contains("tc=old tool call/result cluster"));
        assert!(joined.contains("tr=old tool call/result cluster"));
        assert!(joined.contains("a_old=old assistant turn"));
        assert!(joined.contains("sk=stale skill activation"));
    }

    #[test]
    fn tokens_after_reflects_dropped_size() {
        let rope = RetainedRope::from_spans(vec![
            span("sys", SpanKind::System, "abc"),
            span("f1", SpanKind::FileLoad, "0123456789"),
        ]);
        let r = SpanFamilyDropPolicy::new().compact(&rope, 3);
        assert_eq!(r.tokens_before, 13);
        assert_eq!(r.tokens_after, 3, "f1 (10 chars) dropped; sys (3) remains");
    }

    #[test]
    fn keep_most_recent_assistant_when_only_one_exists() {
        // KEEP_RECENT_ASSISTANT=2 but only one Assistant present —
        // it's preserved; the FileLoad takes the drop.
        let rope = RetainedRope::from_spans(vec![
            span("a1", SpanKind::Assistant, "0123456789"),
            span("f1", SpanKind::FileLoad, "0123456789"),
        ]);
        let r = SpanFamilyDropPolicy::new().compact(&rope, 10);
        let survivors = surviving_ids(&r);
        assert!(survivors.contains(&"a1"));
        assert!(!survivors.contains(&"f1"));
    }

    #[test]
    fn order_preserved_in_survivors() {
        // Drops must not reorder survivors.
        let rope = RetainedRope::from_spans(vec![
            span("sys", SpanKind::System, "y"),
            span("f1", SpanKind::FileLoad, "0123456789"),
            span("u1", SpanKind::User, "z"),
            span("a1", SpanKind::Assistant, "q"),
        ]);
        let r = SpanFamilyDropPolicy::new().compact(&rope, 4);
        // f1 drops; sys/u1/a1 keep insertion order.
        assert_eq!(surviving_ids(&r), vec!["sys", "u1", "a1"]);
    }

    #[test]
    fn arc_dyn_trait_object_works() {
        use std::sync::Arc;
        let policy: Arc<dyn CompactionPolicy> = Arc::new(SpanFamilyDropPolicy::new());
        let rope = RetainedRope::from_spans(vec![span("sys", SpanKind::System, "y")]);
        let r = policy.compact(&rope, 1_000);
        assert_eq!(r.rope.spans().len(), 1);
    }
}
