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
use mu_core::capability::Capability;
use mu_core::event_log::SessionEventLog;
use mu_core::protocol::{ApprovalDecision, OutstandingCall};

#[cfg(test)]
use super::mailbox::MailboxState;
use super::mailbox::MailboxStateHandle;
use super::provider_status::{ProviderCallState, ProviderStatusTracker};

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
    /// What this session is allowed to do (mu-033). Wrapped in a
    /// Mutex so dispatch can decrement the tool-call budget per
    /// invocation. None on the type isn't possible — we always
    /// instantiate a Capability (root() for root sessions, attenuated
    /// for delegates). But check_allow returns a structured result
    /// so the agent loop can refuse with a specific reason.
    capability: Arc<Mutex<Capability>>,
    /// Live provider-call state (mu-035 Phase D). The forwarder
    /// mirrors `AgentEvent::ProviderStatus` into this on every emit
    /// and clears it on Done/Error. Dispatch reads it to assemble
    /// `daemon.outstanding_calls` and to fill in the `was_in` field
    /// of `session.cancel_outstanding` replies.
    provider_status: Arc<Mutex<ProviderStatusTracker>>,
    /// mu-lho (mu-037 Phase 1): per-session mailbox + peer-handle
    /// state. Holds the monotonic seq counter for new posts and the
    /// peer-handle registry for handles this session has issued.
    /// Mailbox messages themselves live in the session's event log;
    /// this struct is the coordination state around them.
    mailbox: MailboxStateHandle,
}

/// In-memory session registry. Cheap to clone (Arc-backed).
#[derive(Clone)]
pub struct Sessions {
    inner: Arc<Mutex<HashMap<String, SessionState>>>,
}

/// Input bundle for [`Sessions::insert`]. Mirrors `SessionState`'s
/// fields without exposing the inner struct; the caller fills this
/// in and hands ownership off. The two `JoinHandle`s are conceptually
/// "owned by the session"; storage documents lifetime intent (tokio
/// tasks run regardless of whether the handle is held).
pub struct NewSession {
    pub input_tx: mpsc::Sender<AgentInput>,
    pub forwarder: JoinHandle<()>,
    pub agent: JoinHandle<()>,
    pub event_log: Arc<SessionEventLog>,
    pub pending_approvals: Arc<Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>>,
    pub parent_session_id: Option<String>,
    pub capability: Arc<Mutex<Capability>>,
    pub provider_status: Arc<Mutex<ProviderStatusTracker>>,
    pub mailbox: MailboxStateHandle,
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
    /// optional parent reference for delegated sessions + the
    /// capability the session is operating under.
    pub fn insert(&self, id: String, new: NewSession) {
        if let Ok(mut map) = self.inner.lock() {
            map.insert(
                id,
                SessionState {
                    input_tx: new.input_tx,
                    _forwarder: new.forwarder,
                    _agent: new.agent,
                    event_log: new.event_log,
                    pending_approvals: new.pending_approvals,
                    parent_session_id: new.parent_session_id,
                    capability: new.capability,
                    provider_status: new.provider_status,
                    mailbox: new.mailbox,
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
        self.inner.lock().ok()?.get(id).map(|s| s.input_tx.clone())
    }

    /// Snapshot of every session for the discovery layer. Returns
    /// `(session_id, event_log, parent_session_id)` triples. The
    /// caller derives `SessionInfo` from these. Same lock-then-clone-
    /// then-drop pattern as the other accessors.
    pub fn snapshot_for_listing(&self) -> Vec<(String, Arc<SessionEventLog>, Option<String>)> {
        self.inner
            .lock()
            .ok()
            .map(|map| {
                map.iter()
                    .map(|(sid, s)| {
                        (
                            sid.clone(),
                            s.event_log.clone(),
                            s.parent_session_id.clone(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Look up a session's event log. Returns None if the session
    /// doesn't exist. Same lock-then-clone-then-drop pattern as
    /// `input_sender`.
    pub fn event_log(&self, id: &str) -> Option<Arc<SessionEventLog>> {
        self.inner.lock().ok()?.get(id).map(|s| s.event_log.clone())
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

    /// Look up a session's capability handle. Returns None if the
    /// session doesn't exist. Used by:
    ///   - dispatch::handle_delegate_session to compute the child's
    ///     attenuated capability from the parent's
    ///   - dispatch-time tool-call checks (future, when the agent
    ///     loop is wired to consult capabilities at execute time)
    pub fn capability(&self, id: &str) -> Option<Arc<Mutex<Capability>>> {
        self.inner
            .lock()
            .ok()?
            .get(id)
            .map(|s| s.capability.clone())
    }

    /// mu-lho (mu-037 Phase 1): handle to a session's mailbox state
    /// (seq counter + issued peer handles). Returned by clone, so
    /// dispatch handlers can hold it across awaits without keeping
    /// the registry lock.
    pub fn mailbox(&self, id: &str) -> Option<MailboxStateHandle> {
        self.inner.lock().ok()?.get(id).map(|s| s.mailbox.clone())
    }

    /// Snapshot the session's live provider-call state (mu-035 Phase
    /// D). `None` when the session doesn't exist OR when no provider
    /// call is currently in flight. Used by `session.cancel_outstanding`
    /// to fill in the `was_in` field with the actual state at the
    /// moment of the request.
    pub fn provider_status_snapshot(&self, id: &str) -> Option<ProviderCallState> {
        let tracker = self
            .inner
            .lock()
            .ok()?
            .get(id)
            .map(|s| s.provider_status.clone())?;
        let snap = tracker.lock().ok()?.snapshot();
        snap
    }

    /// Snapshot every outstanding provider call across all sessions
    /// (mu-035 Phase D). Returns one [`OutstandingCall`] per session
    /// currently in a non-idle state. Sessions that are between asks
    /// (tracker cleared) are omitted. `now_unix_ms` is used to
    /// compute `elapsed_ms`; the caller passes a single value so all
    /// elapsed fields in a snapshot are consistent.
    ///
    /// Two-phase lock pattern: snapshot the registry's handles under
    /// the registry lock, drop it, then snapshot each per-session
    /// tracker under its own lock. Each lock is held only for the
    /// duration of a sync `.clone()` or `.snapshot()`. No `await`
    /// runs while any lock is held.
    pub fn snapshot_outstanding_calls(&self, now_unix_ms: u64) -> Vec<OutstandingCall> {
        let handles: Vec<(
            String,
            Arc<SessionEventLog>,
            Arc<Mutex<ProviderStatusTracker>>,
        )> = self
            .inner
            .lock()
            .ok()
            .map(|map| {
                map.iter()
                    .map(|(sid, s)| (sid.clone(), s.event_log.clone(), s.provider_status.clone()))
                    .collect()
            })
            .unwrap_or_default();

        handles
            .iter()
            .filter_map(|(sid, log, tracker)| {
                let state = tracker.lock().ok()?.snapshot()?;
                // provider_info comes from the session's first
                // SessionCreated event log row; absent only if the
                // log was somehow truncated. Skip such sessions
                // rather than reporting empty strings.
                let (provider_kind, model) = log.provider_info()?;
                Some(OutstandingCall {
                    session_id: sid.clone(),
                    kind: state.kind,
                    provider_kind,
                    model,
                    started_at_unix_ms: state.started_at_unix_ms,
                    elapsed_ms: now_unix_ms.saturating_sub(state.started_at_unix_ms),
                })
            })
            .collect()
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
        let cap = Arc::new(Mutex::new(Capability::root()));
        let tracker = Arc::new(Mutex::new(ProviderStatusTracker::new()));
        sessions.insert(
            id.clone(),
            NewSession {
                input_tx: tx,
                forwarder,
                agent,
                event_log: log,
                pending_approvals: approvals,
                parent_session_id: None,
                capability: cap,
                provider_status: tracker,
                mailbox: Arc::new(MailboxState::new()),
            },
        );
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

        let cap = Arc::new(Mutex::new(Capability::root()));
        let tracker = Arc::new(Mutex::new(ProviderStatusTracker::new()));
        sessions.insert(
            id.clone(),
            NewSession {
                input_tx: tx,
                forwarder: tokio::spawn(async {}),
                agent: tokio::spawn(async {}),
                event_log: log,
                pending_approvals: approvals,
                parent_session_id: None,
                capability: cap,
                provider_status: tracker,
                mailbox: Arc::new(MailboxState::new()),
            },
        );

        // Take the pending oneshot, simulating the dispatch handler.
        let sender = sessions
            .take_pending_approval(&id, "req-1")
            .expect("pending approval should be present");
        sender
            .send(ApprovalDecision::Approve)
            .expect("send decision");

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

    /// mu-035 Phase D test helper: build a session pre-populated with
    /// a SessionCreated event (so provider_info() resolves), a fresh
    /// (idle) tracker, and return its event log + tracker handle for
    /// driving from the test body.
    fn make_session_with_tracker(
        sessions: &Sessions,
        id: &str,
        provider_kind: &str,
        model: &str,
    ) -> (Arc<SessionEventLog>, Arc<Mutex<ProviderStatusTracker>>) {
        use mu_core::event_log::EventActor;
        use mu_core::event_log::EventPayload;
        let log = Arc::new(SessionEventLog::new(id.to_string()));
        log.append(
            EventActor::System,
            EventPayload::SessionCreated {
                provider_kind: provider_kind.into(),
                model: model.into(),
                parent_session_id: None,
                branched_at_parent_event_id: None,
            },
        );
        let (tx, _rx) = mpsc::channel(1);
        let tracker = Arc::new(Mutex::new(ProviderStatusTracker::new()));
        sessions.insert(
            id.to_string(),
            NewSession {
                input_tx: tx,
                forwarder: tokio::spawn(async {}),
                agent: tokio::spawn(async {}),
                event_log: log.clone(),
                pending_approvals: Arc::new(Mutex::new(HashMap::new())),
                parent_session_id: None,
                capability: Arc::new(Mutex::new(Capability::root())),
                provider_status: tracker.clone(),
                mailbox: Arc::new(MailboxState::new()),
            },
        );
        (log, tracker)
    }

    #[tokio::test]
    async fn provider_status_snapshot_returns_none_when_idle() {
        use mu_core::protocol::ProviderStatusKind;
        let sessions = Sessions::new();
        let (_log, tracker) = make_session_with_tracker(&sessions, "s-1", "anthropic_api", "x");

        // Fresh tracker: no outstanding call.
        assert!(sessions.provider_status_snapshot("s-1").is_none());

        // Populate, then snapshot.
        tracker.lock().unwrap().enter(super::ProviderCallState {
            kind: ProviderStatusKind::Streaming,
            started_at_unix_ms: 1_000,
            tool_call_id: None,
        });
        let snap = sessions
            .provider_status_snapshot("s-1")
            .expect("state present");
        assert_eq!(snap.kind, ProviderStatusKind::Streaming);
        assert_eq!(snap.started_at_unix_ms, 1_000);

        // Clear → None again.
        tracker.lock().unwrap().clear();
        assert!(sessions.provider_status_snapshot("s-1").is_none());

        // Unknown session → None.
        assert!(sessions.provider_status_snapshot("unknown").is_none());
    }

    #[tokio::test]
    async fn snapshot_outstanding_calls_omits_idle_sessions() {
        use mu_core::protocol::ProviderStatusKind;
        let sessions = Sessions::new();
        let (_log_a, tracker_a) =
            make_session_with_tracker(&sessions, "s-a", "anthropic_api", "claude-x");
        let (_log_b, _tracker_b) =
            make_session_with_tracker(&sessions, "s-b", "openai_api", "gpt-y");

        // Only s-a has a call in flight.
        tracker_a.lock().unwrap().enter(super::ProviderCallState {
            kind: ProviderStatusKind::Streaming,
            started_at_unix_ms: 1_000,
            tool_call_id: None,
        });

        let calls = sessions.snapshot_outstanding_calls(1_500);
        assert_eq!(calls.len(), 1, "only s-a should appear");
        let c = &calls[0];
        assert_eq!(c.session_id, "s-a");
        assert_eq!(c.kind, ProviderStatusKind::Streaming);
        assert_eq!(c.provider_kind, "anthropic_api");
        assert_eq!(c.model, "claude-x");
        assert_eq!(c.started_at_unix_ms, 1_000);
        assert_eq!(c.elapsed_ms, 500);
    }

    #[tokio::test]
    async fn snapshot_outstanding_calls_uses_single_now_for_all_rows() {
        use mu_core::protocol::ProviderStatusKind;
        let sessions = Sessions::new();
        let (_log_a, tracker_a) =
            make_session_with_tracker(&sessions, "s-a", "anthropic_api", "claude-x");
        let (_log_b, tracker_b) =
            make_session_with_tracker(&sessions, "s-b", "openai_api", "gpt-y");

        tracker_a.lock().unwrap().enter(super::ProviderCallState {
            kind: ProviderStatusKind::AwaitingFirstToken,
            started_at_unix_ms: 1_000,
            tool_call_id: None,
        });
        tracker_b.lock().unwrap().enter(super::ProviderCallState {
            kind: ProviderStatusKind::ToolExecuting,
            started_at_unix_ms: 1_200,
            tool_call_id: Some("call-1".into()),
        });

        let calls = sessions.snapshot_outstanding_calls(2_000);
        assert_eq!(calls.len(), 2);
        let by_id: std::collections::HashMap<_, _> =
            calls.iter().map(|c| (c.session_id.clone(), c)).collect();
        assert_eq!(by_id["s-a"].elapsed_ms, 1_000);
        assert_eq!(by_id["s-b"].elapsed_ms, 800);
    }
}
