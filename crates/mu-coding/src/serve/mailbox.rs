//! mu-lho (mu-037 Phase 1): per-session mailbox state + peer-handle
//! registry held in the [`super::sessions::Sessions`] registry.
//!
//! Mailbox messages themselves live in each session's
//! [`mu_core::event_log::SessionEventLog`] as
//! [`MailboxMessagePosted`](mu_core::event_log::EventPayload::MailboxMessagePosted)
//! and [`MailboxMessageConsumed`](mu_core::event_log::EventPayload::MailboxMessageConsumed)
//! variants — the event log is the source of truth. This module
//! holds only the *coordination state* that the event log can't
//! express directly:
//!
//! - The monotonic `seq` counter that the dispatch handler uses to
//!   assign a stable sequence number to each new post BEFORE
//!   appending the event. Atomic so multiple in-flight posts don't
//!   collide.
//! - The [`PeerHandle`] registry — opaque tokens this session has
//!   issued to other sessions via `peer.hello`, plus their bounds
//!   (allowed methods, expiry, remaining call budget).
//!
//! Both are per-session-mutable; both are accessed from dispatch
//! handlers under the standard lock-then-clone-then-drop pattern
//! (see [`super::sessions::Sessions::input_sender`] for the
//! canonical example).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// One peer handle issued by this session to a peer. Opaque (the
/// token is what binds the handle to the issuer); the fields here
/// are the bounds the issuer set at acceptance time.
#[derive(Debug, Clone)]
pub struct PeerHandle {
    /// The peer session id this handle was issued to. Cross-checked
    /// against `MailboxPostRequest.from.session_id` at dispatch
    /// time: a handle issued to session A may not be used by session
    /// B even if B somehow learned the token.
    pub peer_session_id: String,
    /// RPC methods the peer may invoke. v1 default policy: just
    /// `mailbox.post`.
    pub allowed_methods: HashSet<String>,
    /// Wall-clock expiration. None = no expiry. Checked at every
    /// authorization decision.
    pub expires_at_unix_ms: Option<u64>,
    /// Soft per-handle call budget. Decremented on each successful
    /// authorized call. None = unlimited. Reaching 0 expires the
    /// handle (treated as "no longer authorized").
    pub max_calls_remaining: Option<u32>,
}

impl PeerHandle {
    /// Whether this handle currently authorizes `method` from
    /// `caller_session_id`. Returns `false` for any of: wrong caller,
    /// disallowed method, past expiry, exhausted call budget.
    pub fn authorizes(&self, caller_session_id: &str, method: &str) -> bool {
        if caller_session_id != self.peer_session_id {
            return false;
        }
        if !self.allowed_methods.contains(method) {
            return false;
        }
        if let Some(exp) = self.expires_at_unix_ms {
            if now_unix_ms() >= exp {
                return false;
            }
        }
        if let Some(0) = self.max_calls_remaining {
            return false;
        }
        true
    }

    /// Decrement the per-handle call budget if one is set. Idempotent
    /// when budget is unlimited. Called AFTER authorization passes
    /// and the action is about to execute.
    pub fn consume_one_call(&mut self) {
        if let Some(n) = self.max_calls_remaining.as_mut() {
            *n = n.saturating_sub(1);
        }
    }
}

/// Per-session mailbox + peer-handle state. Held in the
/// [`super::sessions::Sessions`] registry as Arc<Mutex<>> so dispatch
/// handlers can mutate without going through the agent loop.
#[derive(Debug, Default)]
pub struct MailboxState {
    /// Monotonic counter for the next `MailboxMessagePosted.seq`.
    /// `AtomicU64` because the seq must be assigned BEFORE the event-
    /// log append, and we need ordering even with concurrent
    /// `mailbox.post` dispatch (e.g. two peers posting to the same
    /// session at once). The event log itself serializes appends, so
    /// the gap between "fetch seq" and "append event" is bounded.
    pub next_seq: AtomicU64,
    /// Peer handles this session has issued. Keyed by token. The
    /// dispatch layer takes a clone of the inner `PeerHandle` while
    /// holding the lock and drops the lock before doing anything
    /// else — same pattern as other Sessions accessors.
    pub peer_handles_issued: Mutex<HashMap<String, PeerHandle>>,
}

impl MailboxState {
    pub fn new() -> Self {
        Self {
            next_seq: AtomicU64::new(1),
            peer_handles_issued: Mutex::new(HashMap::new()),
        }
    }

    /// Allocate the next `seq` for a new posted message and bump the
    /// counter. Called by the `mailbox.post` dispatch handler.
    pub fn allocate_seq(&self) -> u64 {
        self.next_seq.fetch_add(1, Ordering::SeqCst)
    }

    /// Issue a new opaque peer handle to `peer_session_id` with the
    /// given bounds. Token is 32 lowercase hex chars derived from
    /// `rand::random::<u128>()` — collision probability is
    /// astronomically low for the same-daemon scope of Phase 1.
    /// Records the handle in this session's issued-handles map and
    /// returns the token.
    pub fn issue_handle(
        &self,
        peer_session_id: impl Into<String>,
        allowed_methods: HashSet<String>,
        expires_at_unix_ms: Option<u64>,
        max_calls_remaining: Option<u32>,
    ) -> String {
        let raw: u128 = rand::random();
        let token = format!("{raw:032x}");
        let handle = PeerHandle {
            peer_session_id: peer_session_id.into(),
            allowed_methods,
            expires_at_unix_ms,
            max_calls_remaining,
        };
        if let Ok(mut map) = self.peer_handles_issued.lock() {
            map.insert(token.clone(), handle);
        }
        token
    }

    /// Check whether `token` authorizes `caller_session_id` to invoke
    /// `method` against this session. Returns the cloned handle on
    /// success so the caller can decrement its budget after the
    /// action; returns `None` on any failure (unknown token, wrong
    /// caller, method not allowed, expired, exhausted).
    pub fn check_handle(
        &self,
        token: &str,
        caller_session_id: &str,
        method: &str,
    ) -> Option<PeerHandle> {
        let mut map = self.peer_handles_issued.lock().ok()?;
        let handle = map.get(token)?.clone();
        if !handle.authorizes(caller_session_id, method) {
            return None;
        }
        // Decrement in place under the lock.
        if let Some(h) = map.get_mut(token) {
            h.consume_one_call();
        }
        Some(handle)
    }
}

/// Shared handle type used by Sessions and dispatch handlers.
pub type MailboxStateHandle = Arc<MailboxState>;

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_state_allocates_starting_seq() {
        let s = MailboxState::new();
        assert_eq!(s.allocate_seq(), 1);
        assert_eq!(s.allocate_seq(), 2);
        assert_eq!(s.allocate_seq(), 3);
    }

    #[test]
    fn issue_handle_records_token_and_bounds() {
        let s = MailboxState::new();
        let methods: HashSet<String> = ["mailbox.post"].iter().map(|x| x.to_string()).collect();
        let token = s.issue_handle("session-A", methods.clone(), None, None);
        assert_eq!(token.len(), 32);
        let h = s
            .check_handle(&token, "session-A", "mailbox.post")
            .expect("handle authorizes");
        assert_eq!(h.peer_session_id, "session-A");
        assert!(h.allowed_methods.contains("mailbox.post"));
    }

    #[test]
    fn handle_rejects_wrong_caller() {
        let s = MailboxState::new();
        let methods: HashSet<String> = ["mailbox.post"].iter().map(|x| x.to_string()).collect();
        let token = s.issue_handle("session-A", methods, None, None);
        assert!(s
            .check_handle(&token, "session-B", "mailbox.post")
            .is_none());
    }

    #[test]
    fn handle_rejects_disallowed_method() {
        let s = MailboxState::new();
        let methods: HashSet<String> = ["mailbox.post"].iter().map(|x| x.to_string()).collect();
        let token = s.issue_handle("session-A", methods, None, None);
        assert!(s
            .check_handle(&token, "session-A", "mailbox.consume")
            .is_none());
    }

    #[test]
    fn handle_rejects_past_expiry() {
        let s = MailboxState::new();
        let methods: HashSet<String> = ["mailbox.post"].iter().map(|x| x.to_string()).collect();
        let token = s.issue_handle("session-A", methods, Some(1), None);
        assert!(s
            .check_handle(&token, "session-A", "mailbox.post")
            .is_none());
    }

    #[test]
    fn handle_budget_exhausts() {
        let s = MailboxState::new();
        let methods: HashSet<String> = ["mailbox.post"].iter().map(|x| x.to_string()).collect();
        let token = s.issue_handle("session-A", methods, None, Some(2));
        assert!(s
            .check_handle(&token, "session-A", "mailbox.post")
            .is_some());
        assert!(s
            .check_handle(&token, "session-A", "mailbox.post")
            .is_some());
        // Third call: budget = 0, no longer authorizes.
        assert!(s
            .check_handle(&token, "session-A", "mailbox.post")
            .is_none());
    }
}
