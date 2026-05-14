//! Live provider-call status tracker (mu-035 Phase D).
//!
//! Each session in the [`Sessions`](super::sessions::Sessions) registry
//! carries an `Arc<Mutex<ProviderStatusTracker>>`. The forwarder writes
//! to it on every `AgentEvent::ProviderStatus` and clears it on
//! `AgentEvent::Done` / `AgentEvent::Error`. Dispatch handlers read it
//! to assemble `daemon.outstanding_calls` responses and to fill in the
//! `was_in` field of `session.cancel_outstanding` replies.
//!
//! The tracker is daemon-side state, not part of the agent loop's
//! contract — `mu_core::agent::AgentLoop` is unaware of it. This keeps
//! the cross-session visibility concern in the daemon's registry layer
//! and avoids leaking the registry handle into agent-core.

use mu_core::protocol::ProviderStatusKind;

/// Live snapshot of a session's current provider-call state. `None`
/// means the session is between asks — no outstanding call. Updated
/// write-through by the forwarder on every `ProviderStatus` event.
#[derive(Debug, Clone, Default)]
pub struct ProviderStatusTracker {
    current: Option<ProviderCallState>,
}

/// What `ProviderStatusTracker::snapshot` returns when a call is in
/// flight. Mirrors the wire-side `ProviderStatusEvent` minus the
/// session id (the registry knows that) and the bytes_received counter
/// (not needed for `daemon.outstanding_calls`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCallState {
    pub kind: ProviderStatusKind,
    pub started_at_unix_ms: u64,
    pub tool_call_id: Option<String>,
}

impl ProviderStatusTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record entry into a new provider-call state. Replaces any
    /// previous state — transitions are write-through.
    pub fn enter(&mut self, state: ProviderCallState) {
        self.current = Some(state);
    }

    /// Clear the tracker — the current ask completed (Done) or
    /// errored (Error). Subsequent `snapshot()` returns `None` until
    /// the next provider call emits its first `ProviderStatus` event.
    pub fn clear(&mut self) {
        self.current = None;
    }

    pub fn snapshot(&self) -> Option<ProviderCallState> {
        self.current.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_tracker_has_no_state() {
        let t = ProviderStatusTracker::new();
        assert!(t.snapshot().is_none());
    }

    #[test]
    fn enter_then_snapshot_returns_state() {
        let mut t = ProviderStatusTracker::new();
        t.enter(ProviderCallState {
            kind: ProviderStatusKind::AwaitingFirstToken,
            started_at_unix_ms: 1_000,
            tool_call_id: None,
        });
        let snap = t.snapshot().expect("state should be present");
        assert_eq!(snap.kind, ProviderStatusKind::AwaitingFirstToken);
        assert_eq!(snap.started_at_unix_ms, 1_000);
        assert!(snap.tool_call_id.is_none());
    }

    #[test]
    fn enter_replaces_previous_state() {
        let mut t = ProviderStatusTracker::new();
        t.enter(ProviderCallState {
            kind: ProviderStatusKind::AwaitingFirstToken,
            started_at_unix_ms: 1_000,
            tool_call_id: None,
        });
        t.enter(ProviderCallState {
            kind: ProviderStatusKind::Streaming,
            started_at_unix_ms: 2_000,
            tool_call_id: None,
        });
        let snap = t.snapshot().unwrap();
        assert_eq!(snap.kind, ProviderStatusKind::Streaming);
        assert_eq!(snap.started_at_unix_ms, 2_000);
    }

    #[test]
    fn clear_drops_state() {
        let mut t = ProviderStatusTracker::new();
        t.enter(ProviderCallState {
            kind: ProviderStatusKind::Streaming,
            started_at_unix_ms: 1_000,
            tool_call_id: None,
        });
        t.clear();
        assert!(t.snapshot().is_none());
    }
}
