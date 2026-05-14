//! Context-compaction policies — the [`CompactionPolicy`] trait surface
//! and a foundational [`NoCompactionPolicy`] no-op impl.
//!
//! Per `specs/architecture/event-sourced-context.md` and the mu-kgu
//! design (parent bead), compaction is a sibling pluggable surface to
//! [`CacheStrategy`](super::cache::CacheStrategy) and
//! [`ProviderRenderer`](super::renderer::ProviderRenderer): the rope
//! is the controlled variable, the policy is per-session-config, and
//! the agent loop dispatches the policy when a token threshold is
//! crossed.
//!
//! ## Phase 1 of mu-kgu (this module, mu-kgu.1)
//!
//! IN:
//! - [`CompactionPolicy`] trait
//! - [`CompactionResult`] — return shape (new rope + audit log + metrics)
//! - [`CompactionDecision`] — per-span audit entry (`#[non_exhaustive]`
//!   so Phase 2 can extend without breaking existing matches)
//! - [`NoCompactionPolicy`] — identity policy; the v1 default
//! - Stub policies at [`heuristic`] and [`hash_summary`] with
//!   `todo!()` `compact()` bodies — Phase 2 workers fill them in
//!
//! ## Phase 2 (deferred, parallel beads)
//!
//! - `mu-kgu.2` — fills in [`heuristic::SpanFamilyDropPolicy::compact`]
//! - `mu-kgu.3` — fills in [`hash_summary::HashAndSummaryPolicy::compact`]
//! - `mu-kgu.4` — wires `provider.compaction_policy().compact(...)` into
//!   the agent loop on threshold-cross
//!
//! ## Synchronous-by-design
//!
//! The trait is intentionally synchronous, mirroring `CacheStrategy` /
//! `ProviderRenderer`. Impls that need async work (e.g.
//! [`hash_summary::HashAndSummaryPolicy`] making a judge-model call)
//! block on a runtime inside `compact()` rather than async-leak the
//! whole trait into every loop-site call. The agent loop dispatches
//! compaction between turns, not during streaming, so a synchronous
//! API matches the call site.

pub mod bench;
pub mod hash_summary;
pub mod heuristic;
pub mod provider_judge;

use serde::{Deserialize, Serialize};

use super::rope::{RetainedRope, Span};

/// Shared cross-policy token estimator: real-tokenizer count via
/// tiktoken-rs's `cl100k_base` encoding (the public BPE that's the
/// closest approximation of Anthropic's undocumented tokenizer; both
/// are BPE schemes with similar compression ratios within ~5%).
///
/// Both [`heuristic::SpanFamilyDropPolicy`] and
/// [`hash_summary::HashAndSummaryPolicy`] route through this so
/// cross-policy benchmarks (mu-kgu.5) compare like-for-like AND can
/// be cited next to Anthropic's reported `usage.iterations[].input_tokens`
/// from the auto-compaction API.
///
/// mu-kgu.10: pre-swap this was `chars().count()`, which under-counted
/// real tokens by ~50% on natural-language content (calibration vs
/// Anthropic at 124k tokens showed mu's reported 81k via chars/4).
/// Headline-comparable numbers require a real tokenizer; cl100k_base
/// is the practical pick for a public Rust crate.
pub fn estimate_tokens(spans: &[Span]) -> usize {
    use std::sync::OnceLock;
    static BPE: OnceLock<Option<tiktoken_rs::CoreBPE>> = OnceLock::new();
    // One-time encoder construction per process via OnceLock. The
    // wrapped Option lets us fall back to chars().count() if cl100k_base
    // ever fails to load (downloaded BPE files, etc.). Never panics in
    // the agent loop's hot path.
    let bpe = BPE.get_or_init(|| tiktoken_rs::cl100k_base().ok());
    match bpe {
        Some(b) => spans
            .iter()
            .map(|s| b.encode_with_special_tokens(&s.content).len())
            .sum(),
        None => spans.iter().map(|s| s.content.chars().count()).sum(),
    }
}

/// Pluggable strategy for compacting a [`RetainedRope`] toward a token
/// target.
///
/// Per spec proposal (and the mu-kgu parent-bead body):
/// ```text
/// trait CompactionPolicy
///   fn compact(rope: &RetainedRope, target_tokens: usize) -> CompactionResult
/// ```
///
/// Idiomatic Rust takes `&self` so per-impl configuration (e.g., the
/// judge model for [`hash_summary::HashAndSummaryPolicy`], priority
/// ordering for [`heuristic::SpanFamilyDropPolicy`]) lives on the
/// policy value. The receiver does not change the spec's semantics.
///
/// `target_tokens` is the desired post-compaction size — NOT a hard
/// budget cap. Heuristic policies stop evicting when `tokens_after
/// <= target_tokens`; summarization policies may overshoot or
/// undershoot depending on judge output. Callers MUST tolerate
/// `tokens_after > target_tokens` — the policy did its best.
///
/// ## Failure mode contract
///
/// If compaction cannot proceed safely (judge error, malformed
/// response, exhausted attempts), the policy MUST return the original
/// rope unchanged. Surfacing the failure through [`CompactionResult`]
/// (e.g., empty `decisions` + `tokens_after == tokens_before`) lets
/// the agent loop continue with the un-compacted rope rather than
/// blocking a turn. See mu-kgu.3 / mu-kgu.4 for the call-site
/// invariant.
pub trait CompactionPolicy: Send + Sync {
    /// Compact `rope` toward `target_tokens`. Implementations return
    /// a new rope (substrate is immutable) plus a decision log and
    /// metrics. NoCompactionPolicy returns the rope verbatim.
    fn compact(&self, rope: &RetainedRope, target_tokens: usize) -> CompactionResult;

    /// mu-kgu.4: short stable identifier for this policy. Surfaces
    /// in `AgentEvent::CompactionAssembly` so the operator can tell
    /// which policy ran without parsing trait-object type names.
    /// Default `"compaction-policy"`; concrete impls SHOULD override.
    fn policy_label(&self) -> &'static str {
        "compaction-policy"
    }
}

/// The return type of [`CompactionPolicy::compact`].
///
/// Carries the new rope plus a structured audit log of what happened
/// to each span, the token counts before/after (policy-measured), and
/// wall-clock time. The audit log is what
/// `AgentEvent::CompactionAssembly` will project onto the operator
/// view (mu-kgu.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionResult {
    /// The compacted rope. For [`NoCompactionPolicy`] this is an
    /// identity clone of the input.
    pub rope: RetainedRope,
    /// Per-span audit entries. Implementations SHOULD record one
    /// entry per span that was touched (kept / dropped / summarized).
    /// `NoCompactionPolicy` returns an empty `Vec` — nothing was
    /// touched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<CompactionDecision>,
    /// Token count of `rope` BEFORE compaction, as measured by the
    /// policy. `0` is valid for policies that do not measure (e.g.,
    /// [`NoCompactionPolicy`]).
    pub tokens_before: usize,
    /// Token count of the returned `rope`, as measured by the policy.
    /// MAY differ from `target_tokens` — see the trait doc on the
    /// "best-effort" contract.
    pub tokens_after: usize,
    /// Wall-clock duration of the `compact()` call in microseconds.
    /// `0` is valid for policies that do not measure. Microsecond
    /// precision matters because heuristic policies routinely complete
    /// in under 1ms on real session ropes; rounding to ms loses the
    /// signal entirely (mu-kgu.5 benchmark output showed `0` for every
    /// row pre-cleanup).
    pub wall_clock_us: u64,
}

impl CompactionResult {
    /// Identity result: rope unchanged, no decisions, tokens measured
    /// via the shared [`estimate_tokens`] so cross-policy comparison
    /// stays apples-to-apples. Used by [`NoCompactionPolicy`] and as a
    /// convenient fail-closed return for any policy that hits an error.
    pub fn identity(rope: RetainedRope) -> Self {
        let tokens = estimate_tokens(rope.spans());
        Self {
            rope,
            decisions: Vec::new(),
            tokens_before: tokens,
            tokens_after: tokens,
            wall_clock_us: 0,
        }
    }
}

/// One audit entry in a [`CompactionResult::decisions`] log.
///
/// `#[non_exhaustive]` so Phase 2 policies can add variants
/// (e.g., `Failed { reason }` for mu-kgu.3's fail-closed path) without
/// forcing every downstream match to change in lockstep with the
/// foundation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CompactionDecision {
    /// The span was preserved verbatim in the post-compaction rope.
    Kept {
        /// The id of the span that was preserved.
        span_id: String,
    },
    /// The span was removed from the rope. `reason` is a short
    /// human-readable string (policy-defined; e.g., "stale file-load",
    /// "old tool-result").
    Dropped {
        /// The id of the span that was removed.
        span_id: String,
        /// Short explanation tying the drop to the policy's rules.
        reason: String,
    },
    /// One or more spans were merged into a single summary span.
    /// `absorbed_span_ids` lists the ids that no longer appear in the
    /// rope; `summary_span_id` is the new span that replaces them.
    Summarized {
        /// Span ids that were absorbed (removed from the post-rope).
        absorbed_span_ids: Vec<String>,
        /// Id of the new summary span in the post-rope.
        summary_span_id: String,
    },
    /// Compaction could not proceed safely (judge error, malformed
    /// response, irrecoverable hash collision, etc.). The
    /// accompanying [`CompactionResult`] MUST contain the **original**
    /// rope unchanged — the spec's fail-closed contract. `reason`
    /// is a short human-readable string for the operator log /
    /// briefing.
    Failed {
        /// Short explanation of why compaction was abandoned.
        reason: String,
    },
}

/// No-op compaction policy.
///
/// `compact()` returns the input rope unchanged, an empty decision
/// log, and zero metrics. Correct as the v1 default — preserves
/// pre-mu-kgu agent-loop behavior — and a useful baseline for tests
/// of code that should work whether or not compaction is active.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoCompactionPolicy;

impl NoCompactionPolicy {
    pub fn new() -> Self {
        Self
    }
}

impl CompactionPolicy for NoCompactionPolicy {
    fn compact(&self, rope: &RetainedRope, _target_tokens: usize) -> CompactionResult {
        CompactionResult::identity(rope.clone())
    }

    fn policy_label(&self) -> &'static str {
        "no-compaction"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::rope::{RetainedRope, RetentionClass, Span, SpanKind};

    fn sample_rope() -> RetainedRope {
        RetainedRope::from_spans(vec![
            Span::new(
                "sys",
                SpanKind::System,
                "you are mu",
                RetentionClass::Startup,
            ),
            Span::new("u1", SpanKind::User, "hi", RetentionClass::Hot),
            Span::new("a1", SpanKind::Assistant, "hello", RetentionClass::Hot),
            Span::new(
                "t1",
                SpanKind::ToolResult,
                "{\"ok\":true}",
                RetentionClass::Warm,
            ),
        ])
    }

    #[test]
    fn no_compaction_policy_is_idempotent_on_rope() {
        let rope = sample_rope();
        let result = NoCompactionPolicy::new().compact(&rope, 10_000);
        assert_eq!(
            result.rope.spans(),
            rope.spans(),
            "NoCompactionPolicy must preserve every span verbatim"
        );
        assert!(
            result.decisions.is_empty(),
            "NoCompactionPolicy must produce no decisions"
        );
        // tokens_before/after now reflect the actual rope size via
        // estimate_tokens (post-metrics-cleanup). Identity policy
        // returns equal before/after — they describe the same rope.
        let expected_tokens = estimate_tokens(rope.spans());
        assert_eq!(result.tokens_before, expected_tokens);
        assert_eq!(result.tokens_after, expected_tokens);
        assert_eq!(result.wall_clock_us, 0);
    }

    #[test]
    fn no_compaction_policy_ignores_target_tokens() {
        let rope = sample_rope();
        // Target far below the rope's notional size — still identity.
        let result = NoCompactionPolicy::new().compact(&rope, 0);
        assert_eq!(result.rope.spans(), rope.spans());
    }

    #[test]
    fn no_compaction_policy_on_empty_rope_returns_empty() {
        let rope = RetainedRope::new();
        let result = NoCompactionPolicy::new().compact(&rope, 1_000);
        assert!(result.rope.is_empty());
        assert!(result.decisions.is_empty());
    }

    #[test]
    fn compaction_result_identity_constructor_matches_no_op_shape() {
        let rope = sample_rope();
        let identity = CompactionResult::identity(rope.clone());
        let policy = NoCompactionPolicy::new().compact(&rope, 10_000);
        assert_eq!(identity, policy);
    }

    #[test]
    fn compaction_result_round_trips_through_serde() -> Result<(), serde_json::Error> {
        let result = CompactionResult {
            rope: sample_rope(),
            decisions: vec![
                CompactionDecision::Kept {
                    span_id: "sys".to_string(),
                },
                CompactionDecision::Dropped {
                    span_id: "u1".to_string(),
                    reason: "old user turn".to_string(),
                },
                CompactionDecision::Summarized {
                    absorbed_span_ids: vec!["a1".to_string(), "t1".to_string()],
                    summary_span_id: "compaction:1".to_string(),
                },
            ],
            tokens_before: 1234,
            tokens_after: 567,
            wall_clock_us: 42,
        };
        let json = serde_json::to_string(&result)?;
        let decoded: CompactionResult = serde_json::from_str(&json)?;
        assert_eq!(decoded, result);
        Ok(())
    }

    #[test]
    fn compaction_decision_kept_serde_uses_snake_case_action_tag() -> Result<(), serde_json::Error>
    {
        let d = CompactionDecision::Kept {
            span_id: "sys".to_string(),
        };
        let json = serde_json::to_string(&d)?;
        // tag = "action", rename_all = "snake_case" → variant is "kept".
        assert!(
            json.contains("\"action\":\"kept\""),
            "expected snake_case 'kept' action tag; got {json}"
        );
        let decoded: CompactionDecision = serde_json::from_str(&json)?;
        assert_eq!(decoded, d);
        Ok(())
    }

    #[test]
    fn compaction_decision_dropped_carries_reason() -> Result<(), serde_json::Error> {
        let d = CompactionDecision::Dropped {
            span_id: "f1".to_string(),
            reason: "stale file-load".to_string(),
        };
        let json = serde_json::to_string(&d)?;
        let decoded: CompactionDecision = serde_json::from_str(&json)?;
        assert_eq!(decoded, d);
        Ok(())
    }

    #[test]
    fn compaction_decision_summarized_carries_absorbed_and_summary_ids(
    ) -> Result<(), serde_json::Error> {
        let d = CompactionDecision::Summarized {
            absorbed_span_ids: vec!["x".to_string(), "y".to_string()],
            summary_span_id: "s".to_string(),
        };
        let json = serde_json::to_string(&d)?;
        let decoded: CompactionDecision = serde_json::from_str(&json)?;
        assert_eq!(decoded, d);
        Ok(())
    }

    #[test]
    fn compaction_policy_trait_object_is_send_sync() {
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<dyn CompactionPolicy>();
    }

    #[test]
    fn no_compaction_policy_is_usable_as_arc_trait_object() {
        use std::sync::Arc;
        let policy: Arc<dyn CompactionPolicy> = Arc::new(NoCompactionPolicy::new());
        let rope = sample_rope();
        let result = policy.compact(&rope, 1_000);
        assert_eq!(result.rope.spans(), rope.spans());
    }

    // mu-kgu.1's stub-panic tests for SpanFamilyDropPolicy and
    // HashAndSummaryPolicy have both been removed: mu-kgu.2 landed
    // the real heuristic impl in [`super::heuristic`], and mu-kgu.3
    // landed the real hash+summary impl in [`super::hash_summary`].
    // Each policy's real-impl tests live alongside its module.
}
