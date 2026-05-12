//! Pluggable session discovery (mu-038).
//!
//! `SessionDiscovery` abstracts "what sessions are visible to this
//! daemon" so that:
//!
//!   - the in-process registry (this daemon's own `Sessions` map) is
//!     the default zero-config backend (LocalRegistryBackend),
//!   - a future file-based backend can announce sessions cross-daemon
//!     on the same machine (FileBackend),
//!   - a future etcd-based backend can announce sessions cross-machine
//!     in a cluster (EtcdBackend, paired with a cluster-service per
//!     memory `d32898bd`),
//!
//! all behind the same trait. The `session.list` dispatch handler
//! reads through this trait — it never speaks to the in-memory map
//! directly — so adding a new backend is a code-add, not a code-edit.
//!
//! v1 ships only LocalRegistryBackend. FileBackend and EtcdBackend
//! are sketched in `specs/mu-038-projection-queries-and-discovery.md`
//! and will be implemented in follow-up changes.

use std::sync::Arc;

pub mod file_backend;
pub mod local_registry;

pub use file_backend::FileBackend;
pub use local_registry::LocalRegistryBackend;

use async_trait::async_trait;

use mu_core::event_log::{EventActor, EventPayload, SessionEvent, SessionEventLog};
use mu_core::protocol::{SessionInfo, SessionListFilter, SessionStatusSummary};

/// Info a daemon publishes to the discovery layer for one of its
/// local sessions. Backends that federate (file, etcd) serialise
/// this to their substrate; LocalRegistryBackend ignores it (the
/// in-memory `Sessions` map is the authoritative source).
#[derive(Debug, Clone)]
pub struct LocalSessionInfo {
    pub session_id: String,
    pub daemon_id: String,
    pub parent_session_id: Option<String>,
    pub event_log: Arc<SessionEventLog>,
}

/// Errors a discovery backend can surface. Most are best-effort:
/// `list` MAY return a `PartialFailure` carrying any local results
/// plus the names of peer backends that failed (INV-2 in mu-038).
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("backend error: {0}")]
    Backend(String),
    /// Returned by federating backends when some peers were
    /// unreachable. `local` is the slice of results we DID get.
    #[error("partial failure (peers down: {failed_peers:?})")]
    PartialFailure {
        local: Vec<SessionInfo>,
        failed_peers: Vec<String>,
    },
}

#[async_trait]
pub trait SessionDiscovery: Send + Sync {
    /// Enumerate sessions visible to this daemon. LocalRegistry
    /// returns only this daemon's sessions; federating backends
    /// (File, Etcd) include peers when `filter.include_remote` is
    /// set.
    async fn list(
        &self,
        filter: &SessionListFilter,
    ) -> Result<Vec<SessionInfo>, DiscoveryError>;

    /// Announce that a local session exists. Best-effort: failures
    /// are logged but do not propagate. LocalRegistry is a no-op
    /// (the in-memory map is the source of truth); federating
    /// backends write the entry to their substrate.
    async fn announce(&self, _info: LocalSessionInfo) -> Result<(), DiscoveryError> {
        Ok(())
    }

    /// Note a session's departure. Same best-effort semantics.
    async fn withdraw(&self, _session_id: &str) -> Result<(), DiscoveryError> {
        Ok(())
    }
}

// ── Free helper: derive SessionStatusSummary from a log ─────────────

/// How fresh "last text activity" must be for `Streaming` to win
/// over `Done`. Anything older than this falls through to derived-
/// from-last-event-kind logic.
const STREAMING_RECENCY_MS: u64 = 5_000;

/// Compute the session's status purely from its event log.
///
/// Post-mu-035, the live `ProviderStatusTracker` is the authoritative
/// source for local sessions; this remains the source for sessions in
/// peer daemons whose tracker isn't reachable. Both must agree at
/// rest (after Done events have landed in the log).
pub fn derive_status(log: &SessionEventLog) -> SessionStatusSummary {
    let events = log.snapshot();
    derive_status_from_events(&events, now_unix_ms())
}

pub fn derive_status_from_events(events: &[SessionEvent], now_unix_ms: u64) -> SessionStatusSummary {
    if events.is_empty() {
        return SessionStatusSummary::Idle;
    }

    // Walk in reverse for the most recent terminal/error marker.
    let mut last_kind: Option<&EventPayload> = None;
    let mut last_user_message_ts: Option<u64> = None;
    let mut last_assistant_message_ts: Option<u64> = None;
    let mut last_done_ts: Option<u64> = None;
    let mut pending_tool_call_call_id: Option<String> = None;
    let mut input_required_open = false;

    for ev in events {
        match &ev.payload {
            EventPayload::SessionCreated { .. } => {}
            EventPayload::UserMessage { .. } => {
                last_user_message_ts = Some(ev.timestamp_unix_ms);
                input_required_open = false;
            }
            EventPayload::AssistantMessageEvent { .. } => {
                last_assistant_message_ts = Some(ev.timestamp_unix_ms);
            }
            EventPayload::ToolCall { call_id, .. } => {
                pending_tool_call_call_id = Some(call_id.clone());
            }
            EventPayload::ToolResult { call_id, .. } => {
                if pending_tool_call_call_id.as_ref() == Some(call_id) {
                    pending_tool_call_call_id = None;
                }
            }
            EventPayload::Done { .. } => {
                last_done_ts = Some(ev.timestamp_unix_ms);
                pending_tool_call_call_id = None;
                input_required_open = false;
            }
            EventPayload::Error { .. } => {
                return SessionStatusSummary::Errored;
            }
            EventPayload::Callout { category, .. } => {
                // input_required round-trips are recorded as a callout
                // today; switch to a dedicated event if/when needed.
                if category == "input_required" || category == "approval" {
                    input_required_open = true;
                } else if category == "input_required_resolved"
                    || category == "approval_resolved"
                {
                    input_required_open = false;
                }
            }
            EventPayload::SessionClosed => return SessionStatusSummary::Idle,
            EventPayload::ContextAssembly { .. } => {}
            // ProviderStatusUpdate is a lifecycle marker (mu-pex
            // Phase 1.5), not a session-status-driving event. The
            // tracker has its own derivation via the live wire
            // notification; for derive_status_from_events we ignore.
            EventPayload::ProviderStatusUpdate { .. } => {}
        }
        last_kind = Some(&ev.payload);
    }

    if matches!(last_kind, Some(EventPayload::Error { .. })) {
        return SessionStatusSummary::Errored;
    }
    if matches!(last_kind, Some(EventPayload::SessionClosed)) {
        return SessionStatusSummary::Idle;
    }
    if input_required_open {
        return SessionStatusSummary::AwaitingInputRequired;
    }
    if pending_tool_call_call_id.is_some() {
        return SessionStatusSummary::ToolExecuting;
    }

    // The session might be mid-stream — agent loop has emitted text
    // deltas but no Done yet. We don't log text_delta events, but
    // an AssistantMessage arriving without a Done shortly after is
    // the marker. Use "most recent assistant message later than
    // most recent Done" as the streaming proxy.
    let recent_assistant = last_assistant_message_ts
        .map(|t| (now_unix_ms.saturating_sub(t)) < STREAMING_RECENCY_MS)
        .unwrap_or(false);
    if recent_assistant && last_assistant_message_ts > last_done_ts {
        return SessionStatusSummary::Streaming;
    }

    // User message but no assistant response yet → Asking.
    if last_user_message_ts > last_done_ts.or(last_assistant_message_ts) {
        return SessionStatusSummary::Asking;
    }

    if last_done_ts.is_some() {
        return SessionStatusSummary::Done;
    }
    SessionStatusSummary::Idle
}

/// Cheap helper — keeps `derive_status` independent of any agent
/// loop or session machinery.
pub fn now_unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── derive_session_info: shared between LocalRegistry and dispatch ──

/// Derive `SessionInfo` from a local session's `(session_id, log,
/// parent_session_id)` triple. `daemon_id` is the running daemon's
/// stable id; `is_remote` is `false` for sessions in this daemon
/// (federating backends overwrite when they federate).
pub fn derive_session_info(
    session_id: &str,
    log: &SessionEventLog,
    parent_session_id: Option<String>,
    daemon_id: &str,
) -> SessionInfo {
    let (provider_kind, model) = log
        .provider_info()
        .unwrap_or_else(|| ("unknown".into(), "unknown".into()));
    SessionInfo {
        session_id: session_id.to_string(),
        daemon_id: daemon_id.to_string(),
        is_remote: false,
        parent_session_id,
        provider_kind,
        model,
        status: derive_status(log),
        started_at_unix_ms: log.started_at_unix_ms().unwrap_or(0),
        last_activity_unix_ms: log.last_activity_unix_ms().unwrap_or(0),
        ask_count: log.ask_count(),
        tool_call_count: log.tool_call_count(),
        cumulative_usage: log.cumulative_usage(),
    }
}

// Convenience re-exports so callers can `use serve::discovery::*` and
// get the bits they need without reaching into protocol crate paths.
pub use mu_core::protocol::{SessionInfo as PublicSessionInfo};

#[allow(dead_code)] // referenced by test scaffolding only today
fn _typecheck(_a: &EventActor) {}
