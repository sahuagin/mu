//! Hash-and-summary compaction policy — single judge-model call,
//! output is `{ keep: [hash], summary: string }`.
//!
//! **Stub.** The `compact()` body is `todo!()` pending mu-kgu.3. The
//! foundation (mu-kgu.1) locks in the trait surface; mu-kgu.3 fills
//! in the judge-call wiring, JSON-mode schema validation, and the
//! atomic-surgery step that replaces non-kept spans with a single
//! summary span.
//!
//! See `mu-kgu.3`'s bead body for the operator-designed differentiator:
//! - input: span sequence with stable 8-char hex hashes + content
//! - judge output: `{ keep: [hash...], summary: "..." }`
//! - surgery: kept spans verbatim + one new `CompactionSummary` span
//! - fail-closed: any judge error returns the original rope unchanged
//!   plus a `CompactionDecision` recording the failure
//!
//! The judge-provider field (and any other fields — judge model name,
//! preview length, hash truncation policy) is deferred to mu-kgu.3.
//! Kept as a unit struct here so the gatekeeper does not lock in a
//! shape Phase 2 has not yet committed to.

use super::{CompactionPolicy, CompactionResult};
use crate::context::rope::RetainedRope;

/// Hash-and-summary compaction policy.
///
/// **Phase-2 stub** — see module docs.
#[derive(Debug, Default, Clone, Copy)]
pub struct HashAndSummaryPolicy;

impl HashAndSummaryPolicy {
    pub fn new() -> Self {
        Self
    }
}

impl CompactionPolicy for HashAndSummaryPolicy {
    fn compact(&self, _rope: &RetainedRope, _target_tokens: usize) -> CompactionResult {
        todo!("mu-kgu.3 — implement HashAndSummaryPolicy::compact (single judge-call: keep-list + summary)")
    }
}
