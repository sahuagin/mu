//! Daemon-level stats projections: `daemon.stats`, `daemon.usage_history`,
//! and `daemon.outstanding_calls` wire types.
//!
//! The stats here are read-only snapshots derived from the daemon's
//! durable event log + live session registry. Closed sessions remain
//! visible until they age out of retention; in-flight calls reflect
//! current `ProviderStatusTracker` state.
//!
//! Extracted from `protocol.rs` per mu-6a8 phase 2 (2026-05-18); re-exported
//! by `protocol::*` so external callers see no API change.

use serde::{Deserialize, Serialize};

use super::ProviderStatusKind;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DaemonStatsRequest {}

impl DaemonStatsRequest {
    pub const METHOD: &'static str = "daemon.stats";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonStatsResponse {
    pub daemon_id: String,
    pub version: String,
    pub started_at_unix_ms: u64,
    pub uptime_ms: u64,
    pub session_count: u32,
    pub active_session_count: u32,
    pub total_events: u64,
    pub total_tool_calls: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    /// Number of sessions whose derived status is one of {Asking,
    /// Streaming, ToolExecuting}. Post-mu-035 this should be replaced
    /// by the live ProviderStatusTracker count.
    pub in_flight_calls_count: u32,
}

// ===== mu-pex Phase 1: daemon.usage_history =====

/// Roll up per-call timing and token usage across sessions, grouped
/// by (provider, model, time-bucket). Backed by aggregation over the
/// durable event log of in-memory sessions (Phase 1). Closed sessions
/// remain visible until they age out of the daemon's retention.
///
/// In Phase 1, TTFT and streaming distributions are None because the
/// underlying state-transition signal (ProviderStatusUpdate) is not
/// yet a durable event-log payload. Phase 1.5 will add that variant
/// and populate the fields without changing the response shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DaemonUsageHistoryRequest {
    /// Lower bound on `SessionCreated.timestamp_unix_ms` (inclusive).
    /// None ⇒ no lower bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_unix_ms: Option<u64>,
    /// Upper bound on `SessionCreated.timestamp_unix_ms` (exclusive).
    /// None ⇒ no upper bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_unix_ms: Option<u64>,
    /// Bucket size in milliseconds. Sessions in the same
    /// floor(started/bucket)*bucket window land in the same row.
    /// None ⇒ single bucket per (provider, model) over the whole
    /// time range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_bucket_ms: Option<u64>,
}

impl DaemonUsageHistoryRequest {
    pub const METHOD: &'static str = "daemon.usage_history";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonUsageHistoryResponse {
    pub rows: Vec<UsageHistoryRow>,
    /// Number of sessions considered before grouping. Useful for
    /// confirming the query range actually covered data.
    pub session_count_total: u32,
    pub snapshot_at_unix_ms: u64,
}

/// One row of the usage-history projection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageHistoryRow {
    pub provider_kind: String,
    pub model: String,
    /// floor(started_at / time_bucket_ms) * time_bucket_ms. When the
    /// request has no `time_bucket_ms`, this is the floor of the
    /// earliest session's `started_at_unix_ms` in the group.
    pub bucket_start_unix_ms: u64,
    pub session_count: u32,
    /// Time-to-first-token distribution. Phase 1: None (signal not
    /// yet durable). Phase 1.5: populated from
    /// AwaitingFirstToken→Streaming transitions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<PercentileStats>,
    /// Streaming-state duration distribution. Phase 1: None. Phase
    /// 1.5: populated from time spent in Streaming per call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streaming_ms: Option<PercentileStats>,
    /// AssistantMessageEvent.ts − ContextAssembly.ts per model_call.
    /// Proxy for "model turn-around latency" — not pure TTFT.
    /// None when no ContextAssembly events recorded for the bucket
    /// (e.g. faux provider or pre-mu-032 sessions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_call_latency_ms: Option<PercentileStats>,
    pub tool_total_ms: PercentileStats,
    pub wall_ms: PercentileStats,
    pub input_tokens_sum: u64,
    pub output_tokens_sum: u64,
    pub cache_read_input_tokens_sum: u64,
    pub cache_creation_input_tokens_sum: u64,
    pub reasoning_tokens_sum: u64,
    pub tool_call_count_sum: u64,
    pub error_count: u32,
}

/// median + p95 of a sample of u64 millisecond values. `count` is the
/// sample size that fed the percentiles (≥ 1; rows with empty samples
/// land in the parent field as None rather than PercentileStats with
/// count=0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PercentileStats {
    pub median: u64,
    pub p95: u64,
    pub count: u32,
}

// ===== daemon.outstanding_calls (mu-035 Phase D) =====

/// One outstanding provider call across the daemon, as returned by
/// `daemon.outstanding_calls`. Element of a snapshot — values can
/// change between when the snapshot was taken and when the client
/// reads them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutstandingCall {
    pub session_id: String,
    pub kind: ProviderStatusKind,
    pub provider_kind: String,
    pub model: String,
    pub started_at_unix_ms: u64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DaemonOutstandingCallsRequest {}

impl DaemonOutstandingCallsRequest {
    pub const METHOD: &'static str = "daemon.outstanding_calls";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonOutstandingCallsResponse {
    pub calls: Vec<OutstandingCall>,
    pub snapshot_at_unix_ms: u64,
}
