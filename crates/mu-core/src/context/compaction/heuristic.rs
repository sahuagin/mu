//! Heuristic compaction policy — drops spans by `SpanKind` priority,
//! zero model calls.
//!
//! **Stub.** The `compact()` body is `todo!()` pending mu-kgu.2. The
//! foundation (mu-kgu.1) locks in the trait surface; mu-kgu.2 fills
//! in the priority-ordered eviction logic.
//!
//! See the parent bead `mu-kgu` for the design and `mu-kgu.2`'s body
//! for the drop priority (stale FileLoad > old ToolCall/Result >
//! old AssistantText > stale SkillActivation > pinned kinds).

use super::{CompactionPolicy, CompactionResult};
use crate::context::rope::RetainedRope;

/// Drop spans by `SpanKind` priority. Zero model calls.
///
/// **Phase-2 stub** — see module docs. mu-kgu.2 will:
/// - Walk the rope's spans, classify each by `SpanKind` priority tier
/// - Drop tier-by-tier until `tokens_after <= target_tokens`
/// - Record each drop in `CompactionResult::decisions` with the
///   priority-tier `reason`
/// - Preserve `ToolSchema` / `System` / current `SkillActivation` /
///   recent `AssistantText` as load-bearing
///
/// Field design (priority table, tie-breakers, age threshold) is
/// deferred to mu-kgu.2 — left as a unit struct so that bead has
/// freedom to choose the shape.
#[derive(Debug, Default, Clone, Copy)]
pub struct SpanFamilyDropPolicy;

impl SpanFamilyDropPolicy {
    pub fn new() -> Self {
        Self
    }
}

impl CompactionPolicy for SpanFamilyDropPolicy {
    fn compact(&self, _rope: &RetainedRope, _target_tokens: usize) -> CompactionResult {
        todo!("mu-kgu.2 — implement SpanFamilyDropPolicy::compact (zero-model-call drop-by-span-kind heuristic)")
    }
}
