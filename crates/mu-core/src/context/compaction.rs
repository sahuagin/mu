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
pub mod fidelity;
pub mod fixture;
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
        Some(b) => spans.iter().map(|s| bpe_span_tokens(b, s)).sum(),
        None => spans.iter().map(|s| s.content.chars().count()).sum(),
    }
}

/// Per-span BPE estimate with a size guard
/// (mu-mu-solo-loop-terminate-5ek5): tiktoken's cl100k regex is
/// measured QUADRATIC on long uniform runs (400K spaces took 150s on
/// the dev host; the 2026-06-07 incident's 1.88 GB span would never
/// return), and the rank vector for a multi-GB span is itself
/// GB-scale. Since this can run INLINE in the agent loop task
/// (sync compaction policies), spans over the guard get the chars/4
/// approximation instead — compaction thresholds are coarse; a
/// megabyte-plus span is over any sane per-span budget on either
/// ruler.
const BPE_SPAN_GUARD_BYTES: usize = 1024 * 1024;

fn bpe_span_tokens(bpe: &tiktoken_rs::CoreBPE, span: &Span) -> usize {
    if span.content.len() > BPE_SPAN_GUARD_BYTES {
        return span.content.chars().count() / 4;
    }
    bpe.encode_with_special_tokens(&span.content).len()
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

    /// mu-kgu.8: opt into the background-worker compaction path.
    ///
    /// Default `false` — the agent loop runs `compact()` synchronously
    /// inline, matching mu-kgu.4 behavior. Policies that perform
    /// network I/O (e.g. [`hash_summary::HashAndSummaryPolicy`] with a
    /// `ProviderJudge`) SHOULD override to `true`: the agent loop
    /// wraps the call in `tokio::spawn`, continues the foreground
    /// turn with the un-compacted rope, and applies the result on a
    /// subsequent turn.
    ///
    /// Tradeoff: turns 1-2 after threshold-cross see the un-compacted
    /// rope. Set thresholds at least 2-3 turns' worth of growth
    /// below the model's hard context limit when enabling this path.
    fn is_async(&self) -> bool {
        false
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

// ============================================================================
// mu-kgu.8: background-worker compaction
// ============================================================================
//
// Bounds how often background compaction can fire and how long any one
// attempt is allowed to run. Inspired by pi_agent_rust's
// CompactionWorker quota shape (~/src/flywheel/pi_agent_rust/src/compaction_worker.rs).

/// Per-session quota for background compaction attempts.
#[derive(Debug, Clone, Copy)]
pub struct CompactionQuota {
    /// Minimum elapsed time between successive `start` calls.
    /// Prevents runaway: even if the policy reports `is_async = true`
    /// on every turn, we won't fire more often than this.
    pub cooldown: std::time::Duration,
    /// Wall-clock cap on a single in-flight background compaction.
    /// Reached → the agent loop abandons the result and clears the
    /// pending handle (fail-closed: no compaction this round).
    pub timeout: std::time::Duration,
    /// Hard cap on total successful starts per session. Catches a
    /// degenerate "compact forever" loop without a kill-switch.
    pub max_attempts_per_session: u32,
}

impl Default for CompactionQuota {
    fn default() -> Self {
        // Matches pi_agent_rust defaults; both daemons hit Anthropic
        // or OpenRouter-class endpoints with similar latency budgets.
        Self {
            cooldown: std::time::Duration::from_secs(60),
            timeout: std::time::Duration::from_secs(120),
            max_attempts_per_session: 100,
        }
    }
}

/// Per-session state machine for the background-compaction path.
///
/// The agent loop calls [`Self::can_start`] before deciding whether
/// to invoke an async policy synchronously (fallback) or spawn it on
/// the background path. After a successful spawn via
/// [`Self::start`], subsequent turns call [`Self::try_take`] to
/// non-blockingly poll for completion. The result is then applied to
/// the next render via [`crate::context::append_messages_to_baseline`].
///
/// Designed for in-process per-session use. NOT cross-session; the
/// daemon-wide tracker mu-iwq Phase D introduced (in mu-coding) is a
/// separate concern.
pub struct BackgroundCompactionState {
    pending: Option<PendingCompaction>,
    quota: CompactionQuota,
    last_start: Option<std::time::Instant>,
    attempt_count: u32,
}

struct PendingCompaction {
    handle: tokio::task::JoinHandle<CompactionResult>,
    started_at: std::time::Instant,
    /// Snapshot of `messages.len()` at spawn time — the rope
    /// baseline produced by this compaction covers the first N
    /// messages; the agent loop appends spans for messages added
    /// since then via [`append_messages_to_baseline`].
    messages_at_spawn: usize,
}

impl std::fmt::Debug for BackgroundCompactionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackgroundCompactionState")
            .field("pending", &self.pending.is_some())
            .field("quota", &self.quota)
            .field("attempt_count", &self.attempt_count)
            .finish()
    }
}

/// Output of [`BackgroundCompactionState::try_take`]: a completed
/// compaction is paired with the snapshot of `messages.len()` taken at
/// spawn time so the agent loop knows which suffix to append.
#[derive(Debug)]
pub struct CompletedBackgroundCompaction {
    pub result: CompactionResult,
    pub messages_at_spawn: usize,
}

impl BackgroundCompactionState {
    pub fn new(quota: CompactionQuota) -> Self {
        Self {
            pending: None,
            quota,
            last_start: None,
            attempt_count: 0,
        }
    }

    /// Whether a new background compaction is allowed to start now.
    /// False if another is in flight, the cooldown hasn't elapsed,
    /// or we've hit the per-session attempt cap.
    pub fn can_start(&self) -> bool {
        if self.pending.is_some() {
            return false;
        }
        if self.attempt_count >= self.quota.max_attempts_per_session {
            return false;
        }
        if let Some(last) = self.last_start {
            if last.elapsed() < self.quota.cooldown {
                return false;
            }
        }
        true
    }

    /// Spawn the policy on a tokio task. Caller must already have
    /// confirmed [`Self::can_start`]; debug_assert in case it didn't.
    ///
    /// `policy` is cloned into the task. `rope_snapshot` is the
    /// baseline rope the task will compact; it's intentionally owned,
    /// not borrowed, because the task outlives the caller's stack.
    /// `target_tokens` is the post-compaction target. `messages_len`
    /// is the snapshot of `messages.len()` at spawn time — recorded
    /// so the agent loop can append later-added message spans on
    /// apply (see [`crate::context::append_messages_to_baseline`]).
    pub fn start(
        &mut self,
        policy: std::sync::Arc<dyn CompactionPolicy>,
        rope_snapshot: RetainedRope,
        target_tokens: usize,
        messages_len: usize,
    ) {
        debug_assert!(
            self.can_start(),
            "start() called while can_start() is false",
        );
        let handle = tokio::spawn(async move {
            // The policy's compact() is synchronous; the task simply
            // wraps it. For Provider-backed judges, ProviderJudge
            // does its own dedicated-thread runtime spawn internally
            // (see mu-kgu.11's provider_judge module), so we don't
            // need block_in_place here.
            policy.compact(&rope_snapshot, target_tokens)
        });
        let now = std::time::Instant::now();
        self.pending = Some(PendingCompaction {
            handle,
            started_at: now,
            messages_at_spawn: messages_len,
        });
        self.last_start = Some(now);
        self.attempt_count = self.attempt_count.saturating_add(1);
    }

    /// Non-blocking poll. Returns:
    /// - `Some(Some(completion))` when a pending compaction has
    ///   finished and the agent loop should adopt its result.
    /// - `Some(None)` when a pending compaction exceeded the quota
    ///   timeout and was abandoned. The agent loop should record that
    ///   no compaction was applied (fail-closed); subsequent turns
    ///   are free to attempt again once the cooldown elapses.
    /// - `None` when nothing is pending OR the in-flight task is
    ///   still running and within the timeout.
    pub async fn try_take(&mut self) -> Option<Option<CompletedBackgroundCompaction>> {
        let pending = self.pending.as_ref()?;
        if pending.started_at.elapsed() > self.quota.timeout {
            if let Some(p) = self.pending.take() {
                p.handle.abort();
            }
            return Some(None);
        }
        if !pending.handle.is_finished() {
            return None;
        }
        let pending = self.pending.take()?;
        match pending.handle.await {
            Ok(result) => Some(Some(CompletedBackgroundCompaction {
                result,
                messages_at_spawn: pending.messages_at_spawn,
            })),
            // Task panicked or was aborted — surface as "no compaction
            // this round" rather than crashing the session.
            Err(_) => Some(None),
        }
    }

    pub fn attempt_count(&self) -> u32 {
        self.attempt_count
    }
}

impl Drop for BackgroundCompactionState {
    fn drop(&mut self) {
        if let Some(p) = self.pending.take() {
            p.handle.abort();
        }
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

    // ── mu-kgu.8 ────────────────────────────────────────────────

    /// Mock async policy: reports `is_async = true` and shrinks the
    /// rope to its first span. Used to drive BackgroundCompactionState
    /// tests deterministically.
    #[derive(Debug, Default)]
    struct AsyncShrinkPolicy;

    impl CompactionPolicy for AsyncShrinkPolicy {
        fn compact(&self, rope: &RetainedRope, _target_tokens: usize) -> CompactionResult {
            let kept: Vec<Span> = rope.spans().iter().take(1).cloned().collect();
            CompactionResult {
                rope: RetainedRope::from_spans(kept),
                decisions: vec![],
                tokens_before: 1000,
                tokens_after: 100,
                wall_clock_us: 1,
            }
        }
        fn policy_label(&self) -> &'static str {
            "async-shrink-test"
        }
        fn is_async(&self) -> bool {
            true
        }
    }

    #[test]
    fn default_quota_matches_pi_shape() {
        let q = CompactionQuota::default();
        assert_eq!(q.cooldown, std::time::Duration::from_secs(60));
        assert_eq!(q.timeout, std::time::Duration::from_secs(120));
        assert_eq!(q.max_attempts_per_session, 100);
    }

    #[test]
    fn fresh_bg_state_can_start() {
        let s = BackgroundCompactionState::new(CompactionQuota::default());
        assert!(s.can_start());
        assert_eq!(s.attempt_count(), 0);
    }

    #[tokio::test]
    async fn bg_state_start_then_try_take_yields_result_on_next_poll() {
        let mut s = BackgroundCompactionState::new(CompactionQuota::default());
        let rope = sample_rope();
        let policy: std::sync::Arc<dyn CompactionPolicy> = std::sync::Arc::new(AsyncShrinkPolicy);
        s.start(policy, rope.clone(), 100, 5);
        assert!(!s.can_start(), "should be blocked while pending");
        // Let the spawned task run.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let outcome = s.try_take().await.expect("a completion outcome");
        let complete = outcome.expect("not a timeout");
        assert_eq!(complete.messages_at_spawn, 5);
        // Policy keeps just the first span.
        assert_eq!(complete.result.rope.spans().len(), 1);
    }

    #[tokio::test]
    async fn bg_state_cooldown_blocks_start() {
        let quota = CompactionQuota {
            cooldown: std::time::Duration::from_secs(3600),
            ..CompactionQuota::default()
        };
        let mut s = BackgroundCompactionState::new(quota);
        let rope = sample_rope();
        let policy: std::sync::Arc<dyn CompactionPolicy> = std::sync::Arc::new(AsyncShrinkPolicy);
        s.start(policy, rope.clone(), 100, 0);
        // Drain the result so pending is None.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let _ = s.try_take().await;
        // Now pending is gone but cooldown is enormous.
        assert!(
            !s.can_start(),
            "long cooldown should prevent immediate re-start"
        );
    }

    #[tokio::test]
    async fn bg_state_max_attempts_blocks_start() {
        let quota = CompactionQuota {
            cooldown: std::time::Duration::from_millis(0),
            max_attempts_per_session: 1,
            ..CompactionQuota::default()
        };
        let mut s = BackgroundCompactionState::new(quota);
        let rope = sample_rope();
        let policy: std::sync::Arc<dyn CompactionPolicy> = std::sync::Arc::new(AsyncShrinkPolicy);
        s.start(policy.clone(), rope.clone(), 100, 0);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let _ = s.try_take().await;
        assert!(!s.can_start(), "max_attempts_per_session=1 should block");
    }
}
