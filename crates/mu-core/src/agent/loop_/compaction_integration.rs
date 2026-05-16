//! Compaction integration — baseline tracking for async compaction resumption.

use crate::context::RetainedRope;

/// Snapshot of a completed compaction (sync or async). Tracks the
/// compacted rope and the message-count at compaction time so
/// subsequent turns can rebuild the effective rope via
/// append_messages_to_baseline.
pub(crate) struct CompactionBaseline {
    pub rope: RetainedRope,
    pub messages_at_spawn: usize,
}
