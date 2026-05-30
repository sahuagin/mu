//! JSON-RPC types for capability discovery (mu-kex4.6.4).
//!
//! `capabilities/discover` projects mu's live capability manifest (registered
//! tools + discovered skills, attenuated by the session's capability) and ranks
//! it against a free-text intent — the in-process Layer-1 `t4c find` exposed
//! over the daemon's RPC surface. The result rows are [`CapabilityView`]s, the
//! same borrow-free shape the in-process `t4c_source::discover_view` produces.

use crate::t4c_source::CapabilityView;
use serde::{Deserialize, Serialize};

/// `capabilities/discover` request: rank the calling session's manifest against
/// `intent`, best-first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilitiesDiscoverRequest {
    /// The session whose attenuated manifest to query — discovery tracks the
    /// session's permission (only tools its capability allows are projected).
    pub session_id: String,
    /// Free-text intent, e.g. "search file contents" or "track an issue".
    pub intent: String,
    /// Top-k cap on results. Absent => the handler's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl CapabilitiesDiscoverRequest {
    pub const METHOD: &'static str = "capabilities/discover";
}

/// `capabilities/discover` response: the ranked capability rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilitiesDiscoverResponse {
    pub results: Vec<CapabilityView>,
}
