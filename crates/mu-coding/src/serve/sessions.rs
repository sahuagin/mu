//! In-memory session registry for `mu serve`.
//!
//! All map mutations happen under a `std::sync::Mutex`. The lock is
//! NEVER held across an `await` — see `input_sender` for the
//! lock-then-clone-then-drop pattern.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use mu_core::agent::AgentInput;
use mu_core::event_log::SessionEventLog;
use mu_core::protocol::ApprovalDecision;

/// Per-session state held by the daemon.
struct SessionState {
    input_tx: mpsc::Sender<AgentInput>,
    /// Forwarder task handle. Stored to keep it conceptually owned by
    /// the session; tokio spawned tasks run regardless of whether
    /// JoinHandle is held, but storage documents lifetime intent.
    _forwarder: JoinHandle<()>,
    /// Wrapper around the agent loop's JoinHandle. Same intent.
    _agent: JoinHandle<()>,
    /// Per-session durable-ish event log. v1 is in-memory; future
    /// work persists. The forwarder appends; readers (cumulative
    /// usage queries, future replay) snapshot via the log's own
    /// methods.
    event_log: Arc<SessionEventLog>,
    /// Outstanding `session.input_required` prompts (mu-029). Keyed
    /// by `request_id`. When the client responds via
    /// `session.respond_to_input_required`, dispatch::handle_…
    /// pulls the matching oneshot out and sends the decision; the
    /// agent loop receives it and continues.
    pending_approvals: Arc<Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>>,
    /// Parent session if this is a delegate (mu-031). None for root
    /// sessions. Used by tree queries and future subtree rollup
    /// computations.
    #[allow(dead_code)] // Read by future tree-rollup queries.
    parent_session_id: Option<String>,
}

/// In-memory session registry. Cheap to clone (Arc-backed).
#[derive(Clone)]
pub struct Sessions {
    inner: Arc<Mutex<HashMap<String, SessionState>>>,
}

impl Sessions {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Generate a unique session id. Counter-based; no UUID dep.
    pub fn next_id() -> String {
        static C: AtomicU64 = AtomicU64::new(1);
        format!("session-{}", C.fetch_add(1, Ordering::Relaxed))
    }

    /// Insert a new session. Caller has already spawned the agent
    /// loop and forwarder; this just stores their handles + the
    /// session's event log + the pending-approvals registry +
    /// optional parent reference for delegated sessions.
    pub fn insert(
        &self,
        id: String,
        input_tx: mpsc::Sender<AgentInput>,
        forwarder: JoinHandle<()>,
        agent: JoinHandle<()>,
        event_log: Arc<SessionEventLog>,
        pending_approvals: Arc<Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>>,
        parent_session_id: Option<String>,
    ) {
        if let Ok(mut map) = self.inner.lock() {
            map.insert(
                id,
                SessionState {
                    input_tx,
                    _forwarder: forwarder,
                    _agent: agent,
                    event_log,
                    pending_approvals,
                    parent_session_id,
                },
            );
        }
        // Lock poisoning here means a panic happened while another
        // task held the lock. Continuing without inserting is the
        // safest behavior — the caller's create_session will return
        // a session_id but ask_session against it will return
        // "session not found." Better than crashing the daemon.
    }

    /// Clone a session's input sender, briefly locking the map.
    /// Returns None if the session doesn't exist.
    ///
    /// The lock is held only for the `.get` and `.clone` calls — both
    /// sync, both fast. No `await` runs while the lock is held.
    pub fn input_sender(&self, id: &str) -> Option<mpsc::Sender<AgentInput>> {
        self.inner
            .lock()
            .ok()?
            .get(id)
            .map(|s| s.input_tx.clone())
    }

    /// Look up a session's event log. Returns None if the session
    /// doesn't exist. Same lock-then-clone-then-drop pattern as
    /// `input_sender`.
    pub fn event_log(&self, id: &str) -> Option<Arc<SessionEventLog>> {
        self.inner
            .lock()
            .ok()?
            .get(id)
            .map(|s| s.event_log.clone())
    }

    /// Take a pending-approval oneshot off the session's registry
    /// for `request_id`. Returns None if the request_id isn't
    /// outstanding (already answered, expired, or never existed).
    /// The caller sends a decision on the returned channel.
    pub fn take_pending_approval(
        &self,
        session_id: &str,
        request_id: &str,
    ) -> Option<oneshot::Sender<ApprovalDecision>> {
        let map = self.inner.lock().ok()?;
        let state = map.get(session_id)?;
        let mut pending = state.pending_approvals.lock().ok()?;
        pending.remove(request_id)
    }

    /// Remove a session. Dropping its `SessionState` drops the
    /// `input_tx`; the agent loop sees its input channel close and
    /// terminates naturally on the next iteration.
    pub fn remove(&self, id: &str) -> bool {
        match self.inner.lock() {
            Ok(mut map) => map.remove(id).is_some(),
            Err(_) => false,
        }
    }
}

impl Default for Sessions {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_id_is_unique() {
        let a = Sessions::next_id();
        let b = Sessions::next_id();
        assert_ne!(a, b);
        assert!(a.starts_with("session-"));
    }

    #[tokio::test]
    async fn insert_get_remove_round_trip() {
        let sessions = Sessions::new();
        let id = "test-session".to_string();

        let (tx, _rx) = mpsc::channel::<AgentInput>(1);
        let forwarder = tokio::spawn(async {});
        let agent = tokio::spawn(async {});
        let log = Arc::new(SessionEventLog::new(id.clone()));

        let approvals = Arc::new(Mutex::new(HashMap::new()));
        sessions.insert(id.clone(), tx, forwarder, agent, log, approvals, None);
        assert!(sessions.input_sender(&id).is_some());
        assert!(sessions.event_log(&id).is_some());
        assert!(sessions.remove(&id));
        assert!(sessions.input_sender(&id).is_none());
        assert!(sessions.event_log(&id).is_none());
        assert!(!sessions.remove(&id));
    }

    #[tokio::test]
    async fn take_pending_approval_round_trips_via_oneshot() {
        let sessions = Sessions::new();
        let id = "session-pending".to_string();
        let (tx, _rx) = mpsc::channel::<AgentInput>(1);
        let log = Arc::new(SessionEventLog::new(id.clone()));
        let approvals = Arc::new(Mutex::new(HashMap::new()));

        // Pre-populate one pending approval before the session is
        // even fully wired — this mirrors what the agent loop does
        // (it inserts before emitting the notification).
        let (decision_tx, decision_rx) = oneshot::channel::<ApprovalDecision>();
        approvals
            .lock()
            .unwrap()
            .insert("req-1".to_string(), decision_tx);

        sessions.insert(
            id.clone(),
            tx,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
            log,
            approvals,
            None,
        );

        // Take the pending oneshot, simulating the dispatch handler.
        let sender = sessions
            .take_pending_approval(&id, "req-1")
            .expect("pending approval should be present");
        sender.send(ApprovalDecision::Approve).expect("send decision");

        // Verify the receiver got it.
        let got = decision_rx.await.expect("recv decision");
        assert_eq!(got, ApprovalDecision::Approve);

        // Second take returns None (already consumed).
        assert!(sessions.take_pending_approval(&id, "req-1").is_none());
        // Unknown request_id returns None.
        assert!(sessions
            .take_pending_approval(&id, "req-doesnt-exist")
            .is_none());
        // Unknown session returns None.
        assert!(sessions
            .take_pending_approval("session-nope", "req-1")
            .is_none());
    }
}
