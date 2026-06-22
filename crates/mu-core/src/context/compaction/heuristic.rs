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
//!    cluster first. Adjacency is the *eviction-ordering* heuristic;
//!    it is no longer the correctness rule. mu-4n8u: a final
//!    [`reconcile_tool_pairs`] pass closes every drop set to whole
//!    exchange units keyed by `call_id` (recoverable from the
//!    assistant span's `blocks` and the `ToolResult` span id), so
//!    parallel / non-contiguous / reordered results can never leave an
//!    orphaned `tool_use` or `tool_result` — the invalid shape that
//!    makes a provider reject the next request (OpenAI "No tool output
//!    found for function call"; Anthropic tool_use/tool_result
//!    mismatch).
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
//! mu-tlri: additionally, ANY span whose retention is `Startup` or
//! `Pinned` is never an eviction candidate, regardless of kind — the
//! standing zones (project-context CLAUDE.md/AGENTS.md file-loads,
//! operator pins, skill bodies). See [`evictable`] for the incident
//! rationale and the deliberate exclusion of `Hot` from the guard.
//! A pass may therefore finish above `target_tokens`; over budget
//! beats self-lobotomy.
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

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use super::{CompactionDecision, CompactionPolicy, CompactionResult};
use crate::agent::types::ContentBlock;
use crate::context::assembly::extract_call_id_from_span_id;
use crate::context::rope::{RetainedRope, RetentionClass, Span, SpanKind};

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

/// mu-tlri: standing spans are never eviction candidates.
///
/// `Startup` and `Pinned` are the session's standing zones — system
/// prompt, project-context file-loads (CLAUDE.md/AGENTS.md), memory
/// injections, operator pins, skill bodies. They sit at the front of
/// the rope, i.e., inside the stable prefix the cache strategy marks;
/// the incident behind this bead (session c76f6949) showed dropping
/// them is wrong THREE ways at once: the agent loses its operating
/// rules mid-session, the prefix cache invalidates at position ~0
/// (ejecting ~6K of rules cost ~20× what retaining them would have),
/// and "oldest first" is exactly backwards for cache discipline —
/// oldest = front-of-rope = most stable = cheapest to keep.
///
/// Deliberately NOT `!retention().is_stable()`: `Hot` is "stable" for
/// cache-prefix purposes but conversational — tiers 2/3 must still
/// evict old Hot turns. The guard is about *standing* content, not
/// stability.
///
/// Consequence: a policy pass may finish above `target_tokens` when
/// only standing spans remain. That is the intended trade — better
/// over budget than self-lobotomized (no-self-lobotomy guard,
/// decision #1 in the compaction design).
fn evictable(span: &Span) -> bool {
    !matches!(
        span.retention(),
        RetentionClass::Startup | RetentionClass::Pinned
    )
}

/// Per-span token estimate via the shared [`super::estimate_tokens`]
/// (tiktoken-rs cl100k_base, post mu-kgu.10). Keeps internal eviction
/// accounting on the same ruler as the reported `CompactionResult`
/// metrics — heuristic's local sums equal the post-compact rope's
/// reported total minus the dropped sizes.
fn span_size(span: &Span) -> usize {
    super::estimate_tokens(std::slice::from_ref(span))
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
        span_id: spans[idx].id.to_string(),
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

/// mu-4n8u: the `call_id`s an assistant span PRODUCES via its
/// `tool_use` blocks. Empty for assistant turns with no tool calls and
/// for spans whose `blocks` were never populated (synthetic/test spans,
/// or pre-`mu-yqeq.3` ropes) — those fall back to the adjacency tiers.
fn assistant_call_ids(span: &Span) -> Vec<&str> {
    span.blocks()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolCall(tc) => Some(tc.id.as_str()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// mu-4n8u: reverse a [`record_drop`] — restore the span, give its
/// tokens back, and remove its drop decision. Used by
/// [`reconcile_tool_pairs`] when an exchange unit must be kept whole
/// because one member is non-evictable (standing). Idempotent.
fn undrop(
    idx: usize,
    spans: &[Span],
    sizes: &[usize],
    dropped: &mut [bool],
    tokens_after: &mut usize,
    decisions: &mut Vec<CompactionDecision>,
) {
    if !dropped[idx] {
        return;
    }
    dropped[idx] = false;
    *tokens_after = tokens_after.saturating_add(sizes[idx]);
    let id = spans[idx].id();
    decisions
        .retain(|d| !matches!(d, CompactionDecision::Dropped { span_id, .. } if span_id == id));
}

/// mu-4n8u: enforce tool_use/tool_result pairing by `call_id`, not
/// position. The tier heuristics pair an assistant `tool_use` with its
/// results by adjacency, which is correct only for the tidy
/// `assistant → contiguous results` shape. Parallel calls whose results
/// interleave, results separated by an intervening span, or two
/// assistants whose results land adjacent all break that assumption and
/// can leave a `tool_use` with no matching `tool_result` (or the
/// reverse) — an invalid conversation the next provider request rejects.
///
/// This pass runs after every tier and closes the drop set to whole
/// exchange units: an assistant's `tool_use` and ALL its results share
/// drop-status. A unit is dropped whole only when it is *fully*
/// droppable — every member is evictable AND its assistant is not one
/// of the [`KEEP_RECENT_ASSISTANT`] preserved turns (`preserved`).
/// Otherwise the whole unit is *kept*: any already-dropped member is
/// undropped. A standing (`Startup`/`Pinned`) or recent member must
/// survive, and an orphaned half is invalid, so the rest of the unit
/// comes back with it. Over-budget beats an invalid request (the
/// no-self-lobotomy guard), and keeping the recent unit honors the
/// `KEEP_RECENT_ASSISTANT` invariant the adjacency tiers track only by
/// position — without this the cascade could drop a preserved assistant
/// just because one of its old results was selected for eviction.
///
/// No-op for ropes without recoverable `call_id`s (the assistant span
/// has no `tool_use` blocks, or the result span id isn't the
/// `…-tool-result:{call_id}` shape), so the adjacency tiers remain the
/// sole mechanism there.
fn reconcile_tool_pairs(
    spans: &[Span],
    preserved: &HashSet<usize>,
    sizes: &[usize],
    dropped: &mut [bool],
    tokens_after: &mut usize,
    decisions: &mut Vec<CompactionDecision>,
) {
    // call_id → indices of the ToolResult spans answering it.
    let mut results_by_call: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, s) in spans.iter().enumerate() {
        if *s.kind() == SpanKind::ToolResult {
            if let Some(cid) = extract_call_id_from_span_id(s.id()) {
                results_by_call.entry(cid).or_default().push(i);
            }
        }
    }
    if results_by_call.is_empty() {
        return;
    }

    // One exchange unit per assistant tool_use turn: the assistant span
    // plus every result span answering one of its calls. Units are
    // disjoint (a call_id has exactly one producing tool_use), so order
    // doesn't matter.
    for (i, s) in spans.iter().enumerate() {
        if *s.kind() != SpanKind::Assistant {
            continue;
        }
        let call_ids = assistant_call_ids(s);
        if call_ids.is_empty() {
            continue;
        }
        let mut members = vec![i];
        for cid in &call_ids {
            if let Some(idxs) = results_by_call.get(cid) {
                members.extend(idxs.iter().copied());
            }
        }
        if !members.iter().any(|&m| dropped[m]) {
            continue; // whole unit survives — nothing to reconcile
        }
        // Drop the unit whole only if it is fully droppable: every
        // member evictable AND the assistant not a preserved-recent
        // turn. Otherwise keep it whole (undrop), so the cascade can
        // never drop a standing or recent member to close an orphan.
        let droppable = !preserved.contains(&i) && members.iter().all(|&m| evictable(&spans[m]));
        if droppable {
            for &m in &members {
                record_drop(
                    m,
                    "tool-pair reconciliation (call_id): closed orphaned exchange unit",
                    spans,
                    sizes,
                    dropped,
                    tokens_after,
                    decisions,
                );
            }
        } else {
            for &m in &members {
                undrop(m, spans, sizes, dropped, tokens_after, decisions);
            }
        }
    }
}

impl CompactionPolicy for SpanFamilyDropPolicy {
    /// Stable policy label for `AgentEvent::CompactionAssembly`. Without
    /// this override the heuristic fell through to the trait default
    /// "compaction-policy", making it indistinguishable from any other
    /// non-overriding policy in event logs and the by-policy stats views
    /// (found while wiring hash-and-summary's label for mu-8bkf).
    fn policy_label(&self) -> &'static str {
        "span-family-drop"
    }

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
        // mu-tlri: project-context file-loads (CLAUDE.md/AGENTS.md)
        // are Startup — standing, not stale — and skip this tier.
        // Mid-session file reads (Hot/Warm) remain candidates.
        for i in 0..n {
            if tokens_after <= target_tokens {
                break;
            }
            if spans[i].kind == SpanKind::FileLoad && evictable(&spans[i]) {
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
        // CRITICAL: also drop the Assistant span immediately preceding each
        // cluster — it contains the tool_use block that references the call
        // IDs in the cluster. Leaving it orphaned produces an invalid
        // conversation (model sees tool_use with no matching tool_result).
        let clusters = tool_clusters(spans);
        for cluster in &clusters {
            if tokens_after <= target_tokens {
                break;
            }
            // mu-tlri: the assistant+cluster unit drops together or
            // not at all — a standing member anywhere in the unit
            // (theoretical today; tool spans are Hot) would otherwise
            // leave an orphaned tool_use/tool_result pairing.
            let preceding_standing = cluster
                .first()
                .filter(|&&first| first > 0 && spans[first - 1].kind == SpanKind::Assistant)
                .is_some_and(|&first| !evictable(&spans[first - 1]));
            if preceding_standing || cluster.iter().any(|&idx| !evictable(&spans[idx])) {
                continue;
            }
            // Find the Assistant span preceding this cluster.
            if let Some(&first_idx) = cluster.first() {
                if first_idx > 0 && spans[first_idx - 1].kind == SpanKind::Assistant {
                    record_drop(
                        first_idx - 1,
                        "assistant with orphaned tool_use (evicted with tool cluster)",
                        spans,
                        &sizes,
                        &mut dropped,
                        &mut tokens_after,
                        &mut decisions,
                    );
                }
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
        // When dropping an Assistant span that has tool_use content (the
        // next span is ToolCall/ToolResult), also drop the tool cluster
        // to avoid orphaned tool_results in the conversation.
        for i in 0..n {
            if tokens_after <= target_tokens {
                break;
            }
            if spans[i].kind == SpanKind::Assistant
                && !preserved_assistants.contains(&i)
                && evictable(&spans[i])
            {
                // mu-tlri: assistant + trailing tool cluster drop as a
                // unit or not at all (mirrors tier 2's pairing rule) —
                // a standing cluster member would otherwise be
                // orphaned by the assistant drop.
                let cluster_end = (i + 1..n)
                    .take_while(|&j| {
                        matches!(spans[j].kind, SpanKind::ToolCall | SpanKind::ToolResult)
                    })
                    .last()
                    .map_or(i, |j| j);
                if (i + 1..=cluster_end).any(|j| !evictable(&spans[j])) {
                    continue;
                }
                record_drop(
                    i,
                    "old assistant turn",
                    spans,
                    &sizes,
                    &mut dropped,
                    &mut tokens_after,
                    &mut decisions,
                );
                // Drop trailing tool cluster if present (empty range
                // when cluster_end == i, i.e., no trailing cluster).
                for j in i + 1..=cluster_end {
                    record_drop(
                        j,
                        "tool cluster orphaned by assistant drop",
                        spans,
                        &sizes,
                        &mut dropped,
                        &mut tokens_after,
                        &mut decisions,
                    );
                }
            }
        }

        // Tier 4: SkillActivation, oldest first.
        for i in 0..n {
            if tokens_after <= target_tokens {
                break;
            }
            // mu-tlri: live skill bodies are Pinned (skill/loader.rs),
            // so this tier only reaches non-standing activations —
            // consistent with the module doc's "mostly a no-op until
            // provenance-driven staleness lands".
            if spans[i].kind == SpanKind::SkillActivation && evictable(&spans[i]) {
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

        // mu-4n8u: tool_use/tool_result pairing safety net. The tiers
        // above pair by adjacency; reconcile by call_id so no orphaned
        // pair can survive into the next provider request (the
        // compaction→400 failure class). Runs last, on the final drop
        // set, regardless of which tier touched a given span.
        reconcile_tool_pairs(
            spans,
            &preserved_assistants,
            &sizes,
            &mut dropped,
            &mut tokens_after,
            &mut decisions,
        );

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
        result.rope.spans().iter().map(|s| s.id()).collect()
    }

    /// mu-kgu.10: helper for computing tokenizer-relative targets so
    /// tests don't depend on specific tokenizer magic numbers.
    /// Returns a target slightly above the rope's own tokenized size
    /// for "under target" scenarios, or a fraction of it to force drops.
    fn target_from_rope(rope: &RetainedRope, fraction: f32) -> usize {
        let total = super::super::estimate_tokens(rope.spans());
        ((total as f32) * fraction) as usize
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
        // Two small spans. Target 1_000 — well above any tokenizer's count.
        let rope = RetainedRope::from_spans(vec![
            span("sys", SpanKind::System, "you"),
            span("u1", SpanKind::User, "hi"),
        ]);
        let r = SpanFamilyDropPolicy::new().compact(&rope, 1_000);
        assert_eq!(surviving_ids(&r), vec!["sys", "u1"]);
        assert!(r.decisions.is_empty(), "no drops below target");
        // mu-kgu.10: relational rather than magic-number. Under target →
        // before == after; rope unchanged.
        assert_eq!(r.tokens_before, r.tokens_after);
        assert!(r.tokens_before > 0, "non-empty rope must measure non-zero");
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
        // 4 spans (2 droppable FileLoads + 2 protected System/ToolSchema).
        // Target = 50% of total → must drop ~half; only FileLoad is droppable.
        let rope = RetainedRope::from_spans(vec![
            span("sys", SpanKind::System, "0123456789"),
            span("ts", SpanKind::ToolSchema, "0123456789"),
            span("f1", SpanKind::FileLoad, "0123456789"),
            span("f2", SpanKind::FileLoad, "0123456789"),
        ]);
        let target = target_from_rope(&rope, 0.6);
        let r = SpanFamilyDropPolicy::new().compact(&rope, target);
        let survivors = surviving_ids(&r);
        assert!(survivors.contains(&"sys"), "System must survive");
        assert!(survivors.contains(&"ts"), "ToolSchema must survive");
        // At least one FileLoad dropped; oldest goes first.
        let drops = dropped_ids(&r);
        assert!(drops.contains(&"f1"), "oldest FileLoad dropped first");
    }

    #[test]
    fn file_loads_dropped_before_tool_results() {
        // Two FileLoads + a ToolCall/Result pair. Target = 60% of total →
        // forces ~40% drop. FileLoad tier (50% of rope) drains first;
        // tool cluster only touched if necessary.
        let rope = RetainedRope::from_spans(vec![
            span("f1", SpanKind::FileLoad, "0123456789"),
            span("tc1", SpanKind::ToolCall, "0123456789"),
            span("tr1", SpanKind::ToolResult, "0123456789"),
            span("f2", SpanKind::FileLoad, "0123456789"),
        ]);
        let target = target_from_rope(&rope, 0.6);
        let r = SpanFamilyDropPolicy::new().compact(&rope, target);
        let drops = dropped_ids(&r);
        // Both FileLoads should drop before any tool span is touched.
        assert!(drops.contains(&"f1"), "f1 dropped in tier 1");
        assert!(drops.contains(&"f2"), "f2 dropped in tier 1");
        // Tool cluster preserved (FileLoad drops covered the gap).
        let survivors = surviving_ids(&r);
        assert!(survivors.contains(&"tc1"));
        assert!(survivors.contains(&"tr1"));
    }

    #[test]
    fn tool_call_and_result_drop_as_a_cluster() {
        // Tool cluster preceded by its assistant (which has tool_use).
        // a2/a3 are the two most recent assistants → preserved.
        // Target forces eviction through tier 2.
        let rope = RetainedRope::from_spans(vec![
            span("sys", SpanKind::System, "sys"),
            span("a_tooluse", SpanKind::Assistant, "[tool_call:read({})]"),
            span("tc1", SpanKind::ToolCall, "0123456789"),
            span("tr1", SpanKind::ToolResult, "0123456789"),
            span("a2", SpanKind::Assistant, "0123456789"),
            span("a3", SpanKind::Assistant, "0123456789"),
        ]);
        let target = target_from_rope(&rope, 0.6);
        let r = SpanFamilyDropPolicy::new().compact(&rope, target);
        let drops = dropped_ids(&r);
        assert!(drops.contains(&"tc1"), "tc1 dropped");
        assert!(drops.contains(&"tr1"), "tr1 dropped");
        assert!(
            drops.contains(&"a_tooluse"),
            "assistant with tool_use must be dropped with its cluster"
        );
        let survivors = surviving_ids(&r);
        assert_eq!(survivors, vec!["sys", "a2", "a3"]);
    }

    #[test]
    fn tool_cluster_drop_never_orphans_assistant_tool_use() {
        // Reproducer for the compaction→400 bug: dropping a tool cluster
        // without its assistant leaves an orphaned tool_use block in the
        // conversation, causing the model to 400 with "No tool output
        // found for function call."
        let rope = RetainedRope::from_spans(vec![
            span("sys", SpanKind::System, "system prompt"),
            span("u1", SpanKind::User, "hello"),
            span(
                "a1_tools",
                SpanKind::Assistant,
                "[tool_call:bash({cmd:pwd})]",
            ),
            span("tc1", SpanKind::ToolCall, "bash pwd"),
            span("tr1", SpanKind::ToolResult, "/home/user"),
            span("a2_text", SpanKind::Assistant, "You are in /home/user"),
            span("u2", SpanKind::User, "do more stuff"),
            span(
                "a3_tools",
                SpanKind::Assistant,
                "[tool_call:read({path:f})]",
            ),
            span("tc2", SpanKind::ToolCall, "read f"),
            span("tr2", SpanKind::ToolResult, "file contents here"),
            span("a4_text", SpanKind::Assistant, "The file contains..."),
        ]);
        // Aggressive target to force tier 2 drops.
        let target = target_from_rope(&rope, 0.4);
        let r = SpanFamilyDropPolicy::new().compact(&rope, target);
        let survivors = surviving_ids(&r);
        // Key invariant: no surviving Assistant span should have a
        // tool_use block whose tool cluster was dropped.
        for (i, s) in r.rope.spans().iter().enumerate() {
            if s.kind == SpanKind::Assistant && s.content().contains("[tool_call:") {
                // The next span must be a ToolCall/ToolResult cluster.
                let next = r.rope.spans().get(i + 1);
                assert!(
                    next.is_some_and(|n| matches!(
                        n.kind,
                        SpanKind::ToolCall | SpanKind::ToolResult
                    )),
                    "Assistant span {:?} has tool_use but no following tool cluster — \
                     this would cause a provider 400 error. Survivors: {:?}",
                    s.id(),
                    survivors,
                );
            }
        }
    }

    #[test]
    fn preserves_two_most_recent_assistant_turns() {
        // Five Assistant turns; only a4/a5 (most recent 2) survive.
        // Target = 50% → drop ~half, forcing into Assistant tier.
        let rope = RetainedRope::from_spans(vec![
            span("a1", SpanKind::Assistant, "0123456789"),
            span("a2", SpanKind::Assistant, "0123456789"),
            span("a3", SpanKind::Assistant, "0123456789"),
            span("a4", SpanKind::Assistant, "0123456789"),
            span("a5", SpanKind::Assistant, "0123456789"),
        ]);
        let target = target_from_rope(&rope, 0.5);
        let r = SpanFamilyDropPolicy::new().compact(&rope, target);
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
        // The tool cluster (tc/tr) is preceded by an assistant that acts
        // as the tool_use originator. Three more assistants so
        // KEEP_RECENT_ASSISTANT=2 preserves a_mid + a_keep.
        let rope = RetainedRope::from_spans(vec![
            span("f", SpanKind::FileLoad, "xxxxx"),
            span("a_tool", SpanKind::Assistant, "[tool_call:x({})]"),
            span("tc", SpanKind::ToolCall, "xxxxx"),
            span("tr", SpanKind::ToolResult, "xxxxx"),
            span("a_old", SpanKind::Assistant, "xxxxx"),
            span("a_mid", SpanKind::Assistant, "xxxxx"),
            span("a_keep", SpanKind::Assistant, "xxxxx"),
            span("sk", SpanKind::SkillActivation, "xxxxx"),
        ]);
        let r = SpanFamilyDropPolicy::new().compact(&rope, 0);
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
        assert!(joined.contains("a_tool=assistant with orphaned tool_use"));
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
        // Aggressive target forces f1 to drop; sys (System) is protected.
        let r = SpanFamilyDropPolicy::new().compact(&rope, 1);
        // mu-kgu.10: assert relationally — tokens_after equals the
        // surviving rope's tokenized size, which is sys's size alone.
        let sys_only = super::super::estimate_tokens(&[span("sys", SpanKind::System, "abc")]);
        assert_eq!(
            r.tokens_after, sys_only,
            "tokens_after must equal the surviving rope's tokenized size"
        );
        assert!(r.tokens_before > r.tokens_after, "drop reduced tokens");
        assert_eq!(surviving_ids(&r), vec!["sys"]);
    }

    #[test]
    fn keep_most_recent_assistant_when_only_one_exists() {
        // KEEP_RECENT_ASSISTANT=2 but only one Assistant present —
        // it's preserved; the FileLoad takes the drop.
        let rope = RetainedRope::from_spans(vec![
            span("a1", SpanKind::Assistant, "0123456789"),
            span("f1", SpanKind::FileLoad, "0123456789"),
        ]);
        let target = target_from_rope(&rope, 0.5);
        let r = SpanFamilyDropPolicy::new().compact(&rope, target);
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

    // ── mu-tlri: standing spans are not eviction candidates ──────

    /// The incident shape (session c76f6949): project-context
    /// file-loads (Startup, front of rope) were dropped as 'stale
    /// file-load' while the agent ran on, ruleless. They must
    /// survive every pressure level; mid-session file reads
    /// (non-standing) remain tier-1 candidates.
    #[test]
    fn standing_file_loads_survive_while_stale_ones_drop() {
        let rope = RetainedRope::from_spans(vec![
            Span::new(
                "sys",
                SpanKind::System,
                "you are mu",
                RetentionClass::Startup,
            ),
            Span::new(
                "project-file:/repo/CLAUDE.md",
                SpanKind::FileLoad,
                "operating rules operating rules",
                RetentionClass::Startup,
            ),
            Span::new(
                "project-file:/home/AGENTS.md",
                SpanKind::FileLoad,
                "more standing rules here too",
                RetentionClass::Startup,
            ),
            span("u1", SpanKind::User, "hi"),
            span(
                "file:/tmp/scratch.txt",
                SpanKind::FileLoad,
                "a mid-session read, droppable",
            ),
            span("a1", SpanKind::Assistant, "hello"),
        ]);
        // Target 1 forces maximum pressure — every tier drains.
        let result = SpanFamilyDropPolicy::new().compact(&rope, 1);

        let survivors: Vec<&str> = result.rope.spans().iter().map(|s| s.id()).collect();
        assert!(
            survivors.contains(&"project-file:/repo/CLAUDE.md"),
            "standing file-load must survive max pressure; got {survivors:?}"
        );
        assert!(survivors.contains(&"project-file:/home/AGENTS.md"));
        let dropped_ids: Vec<&str> = result
            .decisions
            .iter()
            .filter_map(|d| match d {
                CompactionDecision::Dropped { span_id, .. } => Some(span_id.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            dropped_ids.contains(&"file:/tmp/scratch.txt"),
            "non-standing file-load stays a tier-1 candidate; dropped: {dropped_ids:?}"
        );
        assert!(
            !dropped_ids.iter().any(|id| id.starts_with("project-file:")),
            "no decision may name a standing span; dropped: {dropped_ids:?}"
        );
    }

    /// Pinned skill bodies (skill/loader.rs pins them) survive
    /// tier 4; a hypothetical non-standing activation still drops.
    #[test]
    fn pinned_skill_activation_survives_tier_4() {
        let rope = RetainedRope::from_spans(vec![
            Span::new(
                "skill:rust:SKILL.md",
                SpanKind::SkillActivation,
                "pinned skill body",
                RetentionClass::Pinned,
            ),
            span("sk-old", SpanKind::SkillActivation, "stale warm activation"),
            span("u1", SpanKind::User, "hi"),
        ]);
        let result = SpanFamilyDropPolicy::new().compact(&rope, 1);
        let survivors: Vec<&str> = result.rope.spans().iter().map(|s| s.id()).collect();
        assert!(survivors.contains(&"skill:rust:SKILL.md"));
        assert!(!survivors.contains(&"sk-old"));
    }

    /// Over-budget beats self-lobotomy: when only standing spans
    /// remain, the pass returns above target rather than dropping
    /// them.
    #[test]
    fn all_standing_rope_compacts_over_target_without_drops() {
        let rope = RetainedRope::from_spans(vec![
            Span::new(
                "sys",
                SpanKind::System,
                "you are mu",
                RetentionClass::Startup,
            ),
            Span::new(
                "project-file:/repo/CLAUDE.md",
                SpanKind::FileLoad,
                "rules rules rules rules rules",
                RetentionClass::Startup,
            ),
        ]);
        let result = SpanFamilyDropPolicy::new().compact(&rope, 1);
        assert_eq!(
            result.rope.spans().len(),
            2,
            "nothing evictable, nothing dropped"
        );
        assert!(
            result.tokens_after > 1,
            "pass ends over budget by design (no-self-lobotomy)"
        );
        assert!(result
            .decisions
            .iter()
            .all(|d| !matches!(d, CompactionDecision::Dropped { .. })));
    }

    /// Tier-2/3 pairing: a standing member anywhere in an
    /// assistant+cluster unit keeps the WHOLE unit (no orphaned
    /// tool_use/tool_result halves).
    #[test]
    fn standing_cluster_member_keeps_the_whole_unit() {
        let rope = RetainedRope::from_spans(vec![
            span("a1", SpanKind::Assistant, "calls a tool"),
            Span::new(
                "t1",
                SpanKind::ToolResult,
                "pinned tool result",
                RetentionClass::Pinned,
            ),
            span("u1", SpanKind::User, "hi"),
            span("a2", SpanKind::Assistant, "x"),
            span("a3", SpanKind::Assistant, "y"),
            span("a4", SpanKind::Assistant, "z"),
        ]);
        let result = SpanFamilyDropPolicy::new().compact(&rope, 1);
        let survivors: Vec<&str> = result.rope.spans().iter().map(|s| s.id()).collect();
        assert!(
            survivors.contains(&"a1") && survivors.contains(&"t1"),
            "assistant+cluster unit with a standing member survives intact; got {survivors:?}"
        );
    }

    // ── mu-4n8u: call_id pairing (realistic spans) ──────────────
    //
    // These build spans the way `assembly::message_to_span` does:
    // assistant turns carry `tool_use` blocks (real call_ids), and
    // ToolResult span ids are `msg-{idx}-tool-result:{call_id}`. The
    // synthetic spans used by the tests above carry no recoverable
    // call_id, so they exercise only the adjacency tiers; these
    // exercise the `reconcile_tool_pairs` safety net.

    fn tool_args() -> crate::agent::types::ToolArgs {
        crate::agent::types::ToolArgs::new(serde_json::json!({})).unwrap()
    }

    /// Assistant span with real `tool_use` blocks, as assembly emits.
    fn assistant_calls(id: &str, calls: &[&str]) -> Span {
        let blocks: Vec<ContentBlock> = calls
            .iter()
            .map(|c| {
                ContentBlock::ToolCall(crate::agent::types::ToolCall {
                    id: (*c).to_owned(),
                    name: "tool".to_owned(),
                    arguments: tool_args(),
                })
            })
            .collect();
        Span::new(id, SpanKind::Assistant, "0123456789", RetentionClass::Hot).with_blocks(blocks)
    }

    /// ToolResult span whose id encodes its call_id (assembly shape).
    fn tool_result(call_id: &str, content: &str) -> Span {
        Span::new(
            format!("msg-0-tool-result:{call_id}"),
            SpanKind::ToolResult,
            content,
            RetentionClass::Warm,
        )
    }

    /// The correctness invariant: every surviving tool_use has a
    /// surviving result and vice versa (paired by call_id).
    fn assert_no_orphans(result: &CompactionResult) {
        let spans = result.rope.spans();
        let ids: Vec<&str> = spans.iter().map(|s| s.id()).collect();
        let surviving_results: HashSet<&str> = spans
            .iter()
            .filter(|s| *s.kind() == SpanKind::ToolResult)
            .filter_map(|s| extract_call_id_from_span_id(s.id()))
            .collect();
        let surviving_calls: HashSet<&str> = spans.iter().flat_map(assistant_call_ids).collect();
        for c in &surviving_calls {
            assert!(
                surviving_results.contains(c),
                "orphaned tool_use {c}: survives with no matching tool_result. survivors={ids:?}"
            );
        }
        for r in &surviving_results {
            assert!(
                surviving_calls.contains(r),
                "orphaned tool_result {r}: survives with no matching tool_use. survivors={ids:?}"
            );
        }
    }

    #[test]
    fn parallel_calls_results_drop_as_one_unit() {
        // One assistant turn fans out to three calls; results are
        // contiguous. Aggressive target forces the unit out. Two recent
        // assistants are preserved so the old unit is the drop target.
        let rope = RetainedRope::from_spans(vec![
            assistant_calls("a_old", &["c1", "c2", "c3"]),
            tool_result("c1", "r1 0123456789"),
            tool_result("c2", "r2 0123456789"),
            tool_result("c3", "r3 0123456789"),
            span("a_keep1", SpanKind::Assistant, "recent one"),
            span("a_keep2", SpanKind::Assistant, "recent two"),
        ]);
        let target = target_from_rope(&rope, 0.4);
        let r = SpanFamilyDropPolicy::new().compact(&rope, target);
        assert_no_orphans(&r);
    }

    #[test]
    fn preserved_tooluse_with_dropped_nonadjacent_result() {
        // The hard case the adjacency tiers can't see: a recent
        // (preserved) assistant's tool_use, whose result is separated
        // from it (e.g. by a prior compaction's reordering). Tier 2
        // drops the lone result cluster (its preceding span is a User,
        // not the assistant), and tier 3 won't drop the assistant
        // because it's preserved — so the tool_use is left orphaned.
        // call_id pairing drops the whole unit instead.
        let rope = RetainedRope::from_spans(vec![
            span("a_old", SpanKind::Assistant, "old turn 0123456789"),
            assistant_calls("a_tool", &["c1"]), // 2nd-most-recent → preserved
            span("u_mid", SpanKind::User, "responding 0123456789"),
            tool_result("c1", "r1 0123456789"), // non-adjacent to a_tool
            span("a_last", SpanKind::Assistant, "most recent 0123456789"),
        ]);
        let target = target_from_rope(&rope, 0.3);
        let r = SpanFamilyDropPolicy::new().compact(&rope, target);
        assert_no_orphans(&r);
        // a_tool is one of the two most-recent assistants — preserved.
        // Reconciliation must keep the WHOLE unit (undrop its result),
        // never drop the preserved assistant to close the orphan.
        let survivors = surviving_ids(&r);
        assert!(
            survivors.contains(&"a_tool") && survivors.contains(&"msg-0-tool-result:c1"),
            "preserved tool_use keeps its result (cascade-keep), not dropped; got {survivors:?}"
        );
    }

    #[test]
    fn adjacent_results_from_two_turns_no_cross_orphan() {
        // Pathological layout (e.g. post-compaction reordering): two
        // assistants' results land in one adjacency run. The cluster's
        // preceding span is the WRONG assistant (aB), so adjacency drops
        // aB+both results and orphans aA's tool_use. call_id pairing
        // keeps each (assistant, result) consistent.
        let rope = RetainedRope::from_spans(vec![
            assistant_calls("aA", &["cA"]),
            assistant_calls("aB", &["cB"]),
            tool_result("cA", "rA 0123456789"),
            tool_result("cB", "rB 0123456789"),
            span("a_keep1", SpanKind::Assistant, "recent one"),
            span("a_keep2", SpanKind::Assistant, "recent two"),
        ]);
        let target = target_from_rope(&rope, 0.4);
        let r = SpanFamilyDropPolicy::new().compact(&rope, target);
        assert_no_orphans(&r);
    }

    #[test]
    fn dropping_old_assistant_drops_its_result() {
        // Tier 3 drops an old assistant turn; its result must go too,
        // or the surviving result is orphaned.
        let rope = RetainedRope::from_spans(vec![
            assistant_calls("a_old", &["c1"]),
            tool_result("c1", "r1 0123456789"),
            span("a_keep1", SpanKind::Assistant, "recent one 0123456789"),
            span("a_keep2", SpanKind::Assistant, "recent two 0123456789"),
        ]);
        let r = SpanFamilyDropPolicy::new().compact(&rope, 1);
        assert_no_orphans(&r);
    }

    #[test]
    fn standing_result_keeps_the_whole_unit_by_call_id() {
        // A non-adjacent, Pinned result: tier 3 would drop the (Hot, old)
        // assistant tool_use and leave the standing result orphaned.
        // Reconciliation cascades the other way — keep the whole unit.
        let rope = RetainedRope::from_spans(vec![
            assistant_calls("a_old", &["c1"]),
            span("u_mid", SpanKind::User, "interjecting 0123456789"),
            Span::new(
                "msg-0-tool-result:c1",
                SpanKind::ToolResult,
                "pinned result 0123456789",
                RetentionClass::Pinned,
            ),
            span("a_keep1", SpanKind::Assistant, "recent one"),
            span("a_keep2", SpanKind::Assistant, "recent two"),
        ]);
        let r = SpanFamilyDropPolicy::new().compact(&rope, 1);
        assert_no_orphans(&r);
        let survivors = surviving_ids(&r);
        assert!(
            survivors.contains(&"a_old") && survivors.contains(&"msg-0-tool-result:c1"),
            "standing member keeps the whole unit; got {survivors:?}"
        );
    }

    #[test]
    fn aggressive_compaction_preserves_pairing_invariant() {
        // Mixed sequential + parallel exchanges, compacted hard. The
        // invariant must hold across the whole rope.
        let rope = RetainedRope::from_spans(vec![
            span("sys", SpanKind::System, "system"),
            assistant_calls("a1", &["c1"]),
            tool_result("c1", "r1 0123456789"),
            span("u1", SpanKind::User, "next 0123456789"),
            assistant_calls("a2", &["c2", "c3"]),
            tool_result("c2", "r2 0123456789"),
            tool_result("c3", "r3 0123456789"),
            assistant_calls("a3", &["c4"]),
            tool_result("c4", "r4 0123456789"),
            span("a4", SpanKind::Assistant, "recent text one"),
            span("a5", SpanKind::Assistant, "recent text two"),
        ]);
        let target = target_from_rope(&rope, 0.3);
        let r = SpanFamilyDropPolicy::new().compact(&rope, target);
        assert_no_orphans(&r);
    }
}
