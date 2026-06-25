//! In-memory session registry for `mu serve`.
//!
//! All map mutations happen under a `std::sync::Mutex`. The lock is
//! NEVER held across an `await` — see `input_sender` for the
//! lock-then-clone-then-drop pattern.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use mu_core::agent::AgentInput;
use mu_core::capability::Capability;
use mu_core::context::CacheTtl;
use mu_core::event_log::SessionEventLog;
use mu_core::protocol::{ApprovalDecision, OutstandingCall, WorkerStatus};
use mu_core::session_status::SessionStatus;

use super::dialogue_poller::DialoguePollerHandle;
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
    /// mu-f1a0: the prompt-cache TTL tier this session was created
    /// with. Read by session.set_route so a live provider/model swap
    /// preserves the tier instead of silently downgrading to 5m.
    cache_ttl: CacheTtl,
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
    /// MCP status subscription: watch receiver for live SessionStatus
    /// updates. The forwarder sends on status-changing events; MCP
    /// subscribers clone this receiver and watch for changes.
    status_watch: Option<tokio::sync::watch::Receiver<Option<SessionStatus>>>,
    /// mu-context-limits-wire phase 2: the live context soft limit (=
    /// compaction trigger) in tokens, shared with the running agent loop.
    /// `session.set_config` stores the new value here; the loop reads it
    /// at each compaction check. `0` ⇒ unset (loop uses its config /
    /// default fallback). Wrapped so the daemon and loop share one cell.
    live_context_soft_limit: Arc<AtomicU64>,
    /// mu-dialogue-inbound-wakeup: handle to this session's dialogue
    /// poller, when one was spawned (only for sessions whose tool list
    /// carries a `dialogue_poll` tool — i.e. the dialogue MCP server was
    /// imported). `None` otherwise. [`Sessions::remove_with_teardown`]
    /// takes it out and awaits the task so close is deterministic; the
    /// poller also self-terminates if the loop's input receiver drops
    /// first.
    dialogue_poller: Option<DialoguePollerHandle>,
}

/// A session loaded from disk at daemon startup (mu-u1ld). The
/// session is read-only — no input channel, no agent loop. Only the
/// event log + parent reference are retained, just enough to satisfy
/// `session.list`, `session.events`, and `session.stats` queries.
///
/// New asks against a rehydrated session ID return the standard
/// "session not found" error — see [`Sessions::input_sender`], which
/// only consults the live map.
struct RehydratedSession {
    event_log: Arc<SessionEventLog>,
    parent_session_id: Option<String>,
    mailbox: MailboxStateHandle,
}

/// mu-slat: a spawned worker subprocess session. Has an event log and
/// mailbox (can participate in peer.hello and receive messages) but no
/// in-process agent loop/provider — the agent runs as a child process
/// (`mu ask` or `claude -p`). The supervisor monitors the child and
/// records lifecycle events.
#[allow(dead_code)] // Fields used by worker.rs (Task 4).
pub(crate) struct SubprocessSession {
    pub event_log: Arc<SessionEventLog>,
    pub mailbox: MailboxStateHandle,
    pub parent_session_id: Option<String>,
    pub pot_name: String,
    pub status: Mutex<WorkerStatus>,
    pub started_at_unix_ms: u64,
    pub child_handle: Option<JoinHandle<()>>,
}

/// In-memory session registry. Cheap to clone (Arc-backed).
///
/// Three parallel maps:
/// - `inner` — fully live sessions (agent loop running, input channel
///   open). Created by `insert`.
/// - `workers` (mu-slat) — spawned subprocess sessions. No agent loop;
///   monitored by a supervisor task. Created by `insert_worker`.
/// - `rehydrated` (mu-u1ld) — read-only ghost sessions loaded from the
///   on-disk event log at daemon startup. Created by `insert_rehydrated`.
///
/// Listing and event-log queries see all three maps (inner > workers >
/// rehydrated on ID collision); live-state queries (input sender,
/// capability, provider status) see only `inner`. Mailbox sees inner +
/// workers + rehydrated. Removal hits all three.
#[derive(Clone)]
pub struct Sessions {
    inner: Arc<Mutex<HashMap<String, SessionState>>>,
    workers: Arc<Mutex<HashMap<String, SubprocessSession>>>,
    rehydrated: Arc<Mutex<HashMap<String, RehydratedSession>>>,
    /// On-disk events root. Not hardcoded here — it's resolved elsewhere
    /// and handed in at construction: `serve::resolve_events_dir(config)`
    /// derives it from `[session].state_dir` (overridable) and
    /// `[session].persist_events_to_disk`, falling back to
    /// `serve::default_events_dir()`. Enables [`event_log`](Self::event_log)'s
    /// lazy find-by-id fallback: a past session is loaded from disk (and
    /// cached) the first time it's addressed, rather than the daemon
    /// bulk-rehydrating every log at startup (mu-lazy-session-rehydration-bh4f).
    /// `None` when persistence is off (tests / ephemeral) → no disk fallback.
    events_dir: Option<PathBuf>,
}

/// A non-owning handle to the session registry (mu-qc08).
///
/// Long-lived per-session structures that need the registry only
/// *occasionally* — notably [`SpawnWorkerTool`](crate::tools::SpawnWorkerTool),
/// which lives inside a session's own tool list — must hold this
/// instead of a strong [`Sessions`] clone. A strong clone there is a
/// shutdown deadlock: the agent loop owns its tools, the tool would own
/// a strong ref to the map that holds the loop's *own* `input_tx`, so on
/// stdin-EOF the channel never closes, `input_rx.recv()` never returns
/// `None`, the loop never exits, and `transport::serve` hangs forever
/// (then `mu ask` SIGKILLs the daemon → non-zero exit). Holding a `Weak`
/// and upgrading only at point-of-use breaks the cycle.
#[derive(Clone)]
pub struct WeakSessions {
    inner: Weak<Mutex<HashMap<String, SessionState>>>,
    workers: Weak<Mutex<HashMap<String, SubprocessSession>>>,
    rehydrated: Weak<Mutex<HashMap<String, RehydratedSession>>>,
    events_dir: Option<PathBuf>,
}

impl WeakSessions {
    /// Upgrade to a strong [`Sessions`]. Returns `None` if the registry
    /// has already been dropped (daemon shutting down) — callers should
    /// surface that as a clean error rather than panicking.
    pub fn upgrade(&self) -> Option<Sessions> {
        Some(Sessions {
            inner: self.inner.upgrade()?,
            workers: self.workers.upgrade()?,
            rehydrated: self.rehydrated.upgrade()?,
            events_dir: self.events_dir.clone(),
        })
    }
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
    /// mu-f1a0: cache TTL tier the session was created with.
    pub cache_ttl: CacheTtl,
    pub provider_status: Arc<Mutex<ProviderStatusTracker>>,
    pub mailbox: MailboxStateHandle,
    pub status_watch: Option<tokio::sync::watch::Receiver<Option<SessionStatus>>>,
    /// mu-context-limits-wire phase 2: shared live context soft limit
    /// (= compaction trigger), the same `Arc` handed to the agent loop's
    /// `SpawnArgs`. `session.set_config` writes it via
    /// [`Sessions::live_context_soft_limit`].
    pub live_context_soft_limit: Arc<AtomicU64>,
    /// mu-dialogue-inbound-wakeup: handle to this session's dialogue poller
    /// (`Some` only when a `dialogue_poll` tool was available to drive it).
    /// Stored so [`Sessions::remove_with_teardown`] can signal + await it.
    /// `pub(crate)` (not `pub`) because [`DialoguePollerHandle`] is a
    /// crate-internal type — the registry is only constructed inside the
    /// crate anyway.
    pub(crate) dialogue_poller: Option<DialoguePollerHandle>,
}

impl Sessions {
    pub fn new() -> Self {
        Self::new_with_events_dir(None)
    }

    /// Construct a registry that knows its on-disk events root, enabling
    /// [`event_log`](Self::event_log)'s lazy find-by-id fallback
    /// (mu-lazy-session-rehydration-bh4f). Production (`mu serve`) passes
    /// the resolved events dir; tests / ephemeral daemons pass `None`.
    ///
    /// NOTE: `events_dir` is a plain (immutable) field, not shared across
    /// clones, so it must be set at construction — before the registry is
    /// cloned into the discovery backends and handler tasks.
    pub fn new_with_events_dir(events_dir: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            workers: Arc::new(Mutex::new(HashMap::new())),
            rehydrated: Arc::new(Mutex::new(HashMap::new())),
            events_dir,
        }
    }

    /// Create a non-owning [`WeakSessions`] handle (mu-qc08). Use this
    /// for references stored inside per-session structures that must not
    /// keep the registry alive (see [`WeakSessions`]).
    pub fn downgrade(&self) -> WeakSessions {
        WeakSessions {
            inner: Arc::downgrade(&self.inner),
            workers: Arc::downgrade(&self.workers),
            rehydrated: Arc::downgrade(&self.rehydrated),
            events_dir: self.events_dir.clone(),
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
                    cache_ttl: new.cache_ttl,
                    provider_status: new.provider_status,
                    mailbox: new.mailbox,
                    status_watch: new.status_watch,
                    live_context_soft_limit: new.live_context_soft_limit,
                    dialogue_poller: new.dialogue_poller,
                },
            );
        }
        // Lock poisoning here means a panic happened while another
        // task held the lock. Continuing without inserting is the
        // safest behavior — the caller's create_session will return
        // a session_id but ask_session against it will return
        // "session not found." Better than crashing the daemon.
    }

    /// Register a read-only session rehydrated from the on-disk event
    /// log at daemon startup (mu-u1ld). The session is queryable via
    /// `session.list`, `session.events`, and `session.stats`, but
    /// `ask_session` / `session.cancel_outstanding` / etc. return
    /// "session not found" because no live state exists.
    ///
    /// Collisions: if a live session already exists with the same ID
    /// (e.g. the counter-based `next_id` produced a clash with a prior
    /// run), the rehydrated entry is still recorded — but the live one
    /// takes precedence in `snapshot_for_listing` and `event_log`. New
    /// inserts via `insert` similarly shadow rehydrated entries.
    pub fn insert_rehydrated(
        &self,
        id: String,
        event_log: Arc<SessionEventLog>,
        parent_session_id: Option<String>,
    ) {
        // Reconstruct mailbox state from the event log so rehydrated
        // sessions can participate in peer.hello and receive messages.
        let mailbox = {
            let ms = MailboxState::new();
            let mut max_seq: u64 = 0;
            for ev in event_log.snapshot().iter() {
                if let mu_core::event_log::EventPayload::MailboxMessagePosted { seq, .. } =
                    &ev.payload
                {
                    if *seq > max_seq {
                        max_seq = *seq;
                    }
                }
            }
            if max_seq > 0 {
                ms.next_seq
                    .store(max_seq + 1, std::sync::atomic::Ordering::SeqCst);
            }
            Arc::new(ms)
        };
        if let Ok(mut map) = self.rehydrated.lock() {
            map.insert(
                id,
                RehydratedSession {
                    event_log,
                    parent_session_id,
                    mailbox,
                },
            );
        }
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
    /// caller derives `SessionInfo` from these. Includes both live and
    /// rehydrated (mu-u1ld) sessions. On ID collision, the live entry
    /// shadows the rehydrated one. Same lock-then-clone-then-drop
    /// pattern as the other accessors.
    pub fn snapshot_for_listing(&self) -> Vec<(String, Arc<SessionEventLog>, Option<String>)> {
        let mut out: HashMap<String, (Arc<SessionEventLog>, Option<String>)> = HashMap::new();
        // Priority: rehydrated < workers < inner (last insert wins).
        if let Ok(map) = self.rehydrated.lock() {
            for (sid, s) in map.iter() {
                out.insert(
                    sid.clone(),
                    (s.event_log.clone(), s.parent_session_id.clone()),
                );
            }
        }
        if let Ok(map) = self.workers.lock() {
            for (sid, s) in map.iter() {
                out.insert(
                    sid.clone(),
                    (s.event_log.clone(), s.parent_session_id.clone()),
                );
            }
        }
        if let Ok(map) = self.inner.lock() {
            for (sid, s) in map.iter() {
                out.insert(
                    sid.clone(),
                    (s.event_log.clone(), s.parent_session_id.clone()),
                );
            }
        }
        out.into_iter()
            .map(|(sid, (log, parent))| (sid, log, parent))
            .collect()
    }

    /// Look up a session's event log in memory only — live wins on ID
    /// collision, then workers, then already-cached rehydrated entries.
    /// Does NOT touch disk. Mutating callers that must not resurrect a
    /// read-only ghost from disk just to act on it (e.g. `session.close`,
    /// which appends `SessionClosed` then removes) use THIS rather than
    /// [`event_log`](Self::event_log). Same lock-then-clone-then-drop
    /// pattern as `input_sender`. (mu-lazy-session-rehydration-bh4f)
    pub fn event_log_in_memory(&self, id: &str) -> Option<Arc<SessionEventLog>> {
        if let Ok(map) = self.inner.lock() {
            if let Some(s) = map.get(id) {
                return Some(s.event_log.clone());
            }
        }
        if let Ok(map) = self.workers.lock() {
            if let Some(s) = map.get(id) {
                return Some(s.event_log.clone());
            }
        }
        if let Ok(map) = self.rehydrated.lock() {
            if let Some(s) = map.get(id) {
                return Some(s.event_log.clone());
            }
        }
        None
    }

    /// Look up a session's event log, with a lazy find-by-id load from
    /// disk on a full in-memory miss (mu-lazy-session-rehydration-bh4f):
    /// the daemon no longer bulk-rehydrates every log at startup, so a
    /// past session is parsed (and cached for next time) the first time
    /// it's actually addressed — by the READ-ONLY `resume` / `recover` /
    /// `session.events` / `session.stats` paths, all of which already
    /// have the id. Returns None if the session exists nowhere. The disk
    /// read happens after all in-memory locks are dropped.
    pub fn event_log(&self, id: &str) -> Option<Arc<SessionEventLog>> {
        self.event_log_in_memory(id)
            .or_else(|| self.lazy_load_from_disk(id))
    }

    /// Find a past session's `<daemon>/<id>.jsonl` on disk, parse it once,
    /// cache it as a read-only rehydrated entry, and return it. `None`
    /// when no events dir is configured (tests / ephemeral) or no log
    /// with that id exists. The targeted find ([`find_session_path`])
    /// touches only daemon-dir metadata — it never enumerates or parses
    /// the other logs. (mu-lazy-session-rehydration-bh4f)
    ///
    /// [`find_session_path`]: crate::sessions_index::find_session_path
    fn lazy_load_from_disk(&self, id: &str) -> Option<Arc<SessionEventLog>> {
        let dir = self.events_dir.as_ref()?;
        let path = crate::sessions_index::find_session_path(dir, id)?;
        let (log, malformed) = match SessionEventLog::from_jsonl(&path) {
            Ok(loaded) => loaded,
            Err(e) => {
                // Don't swallow I/O failures silently — a present-but-
                // unreadable log surfacing as "session not found" hides a
                // real defect (e.g. a ragged log the resume path should
                // diagnose). mu-lazy-session-rehydration-bh4f.
                tracing::warn!(
                    session_id = id,
                    path = %path.display(),
                    error = %e,
                    "lazy session load: failed to read on-disk event log",
                );
                return None;
            }
        };
        if malformed > 0 {
            tracing::warn!(
                session_id = id,
                path = %path.display(),
                malformed,
                "lazy session load: skipped malformed event-log line(s)",
            );
        }
        let log = Arc::new(log);
        // A concurrent first-access may have parsed + cached the same id
        // while we were reading. Prefer the already-cached Arc so repeat
        // lookups stay pointer-stable and we don't double-insert (the two
        // parses are equal for a dead session, so this is purely to avoid
        // wasted work + an Arc-identity surprise). (qwen review #1)
        if let Some(existing) = self.event_log_in_memory(id) {
            return Some(existing);
        }
        // Pull parent_session_id from the SessionCreated record so cached
        // tree-queries match the old bulk-rehydration behavior. Cached
        // under the queried id (what every caller addresses it by), not
        // the log's internal session_id — so repeat lookups by that id
        // hit the cache even if a renamed file's content id drifted.
        let parent_session_id = log.snapshot().iter().find_map(|e| match &e.payload {
            mu_core::event_log::EventPayload::SessionCreated {
                parent_session_id, ..
            } => parent_session_id.clone(),
            _ => None,
        });
        self.insert_rehydrated(id.to_string(), log.clone(), parent_session_id);
        Some(log)
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

    /// mu-f1a0: the cache TTL tier the session was created with.
    /// None when the session doesn't exist (or is rehydrated-only).
    pub fn cache_ttl(&self, id: &str) -> Option<CacheTtl> {
        self.inner
            .lock()
            .ok()
            .and_then(|map| map.get(id).map(|s| s.cache_ttl))
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
        if let Some(m) = self.inner.lock().ok()?.get(id).map(|s| s.mailbox.clone()) {
            return Some(m);
        }
        if let Some(m) = self.workers.lock().ok()?.get(id).map(|s| s.mailbox.clone()) {
            return Some(m);
        }
        self.rehydrated
            .lock()
            .ok()?
            .get(id)
            .map(|s| s.mailbox.clone())
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

    /// Get a clone of the status watch receiver for MCP subscriptions.
    pub fn status_watch(
        &self,
        id: &str,
    ) -> Option<tokio::sync::watch::Receiver<Option<SessionStatus>>> {
        self.inner.lock().ok()?.get(id)?.status_watch.clone()
    }

    /// mu-context-limits-wire phase 2: the shared live context soft-limit
    /// cell for a session, so `session.set_config` can update the
    /// compaction trigger the running loop reads. `None` if no such live
    /// session exists.
    pub fn live_context_soft_limit(&self, id: &str) -> Option<Arc<AtomicU64>> {
        self.inner
            .lock()
            .ok()?
            .get(id)
            .map(|s| s.live_context_soft_limit.clone())
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
    /// terminates naturally on the next iteration. Also evicts any
    /// rehydrated entry (mu-u1ld) under the same ID — symmetry so the
    /// session disappears from `session.list` after explicit removal.
    /// Returns true if either map had an entry.
    pub fn remove(&self, id: &str) -> bool {
        let live = match self.inner.lock() {
            Ok(mut map) => map.remove(id).is_some(),
            Err(_) => false,
        };
        let worker = match self.workers.lock() {
            Ok(mut map) => map.remove(id).is_some(),
            Err(_) => false,
        };
        let ghost = match self.rehydrated.lock() {
            Ok(mut map) => map.remove(id).is_some(),
            Err(_) => false,
        };
        live || worker || ghost
    }

    /// Remove a session like [`remove`](Self::remove), but additionally
    /// tear down its dialogue poller deterministically (mu-dialogue-inbound-
    /// wakeup): signal cancellation and await the poller task. The lock is
    /// released before the await — the poller handle is taken out under the
    /// lock, the lock is dropped, and only then is the task joined, so no
    /// session-registry lock is ever held across an `await` (the invariant
    /// this module's other accessors keep too).
    ///
    /// `remove` (sync) drops the poller handle without joining it; the
    /// poller then self-terminates when the agent loop's input receiver
    /// closes. This variant gives the close path a deterministic join so a
    /// closed session leaves no lingering poll in flight.
    pub async fn remove_with_teardown(&self, id: &str) -> bool {
        // Phase 1 (locked, sync): remove the live entry and take its poller
        // handle out. `live` records whether a live entry existed at all
        // (independent of whether it carried a poller); the rest of the
        // removed SessionState drops here, closing its input_tx.
        let (live, poller) = match self.inner.lock() {
            Ok(mut map) => match map.remove(id) {
                Some(mut state) => (true, state.dialogue_poller.take()),
                None => (false, None),
            },
            Err(_) => (false, None),
        };
        let worker = match self.workers.lock() {
            Ok(mut map) => map.remove(id).is_some(),
            Err(_) => false,
        };
        let ghost = match self.rehydrated.lock() {
            Ok(mut map) => map.remove(id).is_some(),
            Err(_) => false,
        };
        // Phase 2 (unlocked): join the poller task. All registry locks above
        // are dropped, so no lock is held across this await.
        if let Some(poller) = poller {
            poller.shutdown_and_join().await;
        }
        live || worker || ghost
    }

    /// mu-slat: insert a spawned worker subprocess session.
    pub(crate) fn insert_worker(&self, id: String, session: SubprocessSession) {
        if let Ok(mut map) = self.workers.lock() {
            map.insert(id, session);
        }
    }

    /// mu-slat: look up a worker's status. Returns None if the session
    /// isn't a worker.
    pub fn worker_status(&self, id: &str) -> Option<WorkerStatus> {
        self.workers
            .lock()
            .ok()?
            .get(id)
            .and_then(|s| s.status.lock().ok().map(|st| st.clone()))
    }

    /// mu-slat: update a worker's registry status. No-op if the worker vanished.
    pub(crate) fn set_worker_status(&self, id: &str, status: WorkerStatus) {
        if let Ok(map) = self.workers.lock() {
            if let Some(session) = map.get(id) {
                if let Ok(mut current) = session.status.lock() {
                    *current = status;
                }
            }
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
                cache_ttl: CacheTtl::default(),
                provider_status: tracker,
                mailbox: Arc::new(MailboxState::new()),
                status_watch: None,
                live_context_soft_limit: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                dialogue_poller: None,
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
    async fn rehydrated_session_appears_in_listing_and_event_log() {
        // mu-u1ld: a rehydrated entry must be visible to the
        // discovery layer (snapshot_for_listing) and to event-log
        // lookups, but invisible to live-state queries.
        let sessions = Sessions::new();
        let id = "ghost-1".to_string();
        let log = Arc::new(SessionEventLog::new(id.clone()));
        sessions.insert_rehydrated(id.clone(), log, Some("parent-7".into()));

        let listing = sessions.snapshot_for_listing();
        assert_eq!(listing.len(), 1);
        assert_eq!(listing[0].0, id);
        assert_eq!(listing[0].2, Some("parent-7".into()));

        assert!(sessions.event_log(&id).is_some());

        // Agent-loop queries must NOT see rehydrated sessions —
        // ask_session against a ghost ID will fall through to the
        // standard "session not found" error.
        assert!(sessions.input_sender(&id).is_none());
        assert!(sessions.capability(&id).is_none());
        // Mailbox IS available for rehydrated sessions — they can
        // participate in peer.hello and receive messages without
        // needing a live agent loop.
        assert!(sessions.mailbox(&id).is_some());
    }

    #[tokio::test]
    async fn rehydrated_session_can_be_removed() {
        let sessions = Sessions::new();
        let id = "ghost-2".to_string();
        let log = Arc::new(SessionEventLog::new(id.clone()));
        sessions.insert_rehydrated(id.clone(), log, None);

        assert!(sessions.event_log(&id).is_some());
        assert!(sessions.remove(&id), "remove should return true for ghost");
        assert!(sessions.event_log(&id).is_none());
        assert!(!sessions.remove(&id), "removing twice should return false");
    }

    #[tokio::test]
    async fn live_session_shadows_rehydrated_on_id_collision() {
        // The counter-based `next_id` can collide with rehydrated
        // session IDs from a prior run. When both exist, the live
        // entry must take precedence in listings and event_log.
        let sessions = Sessions::new();
        let id = "session-1".to_string();

        // Rehydrated first.
        let ghost_log = Arc::new(SessionEventLog::new(id.clone()));
        sessions.insert_rehydrated(id.clone(), ghost_log.clone(), Some("ghost-parent".into()));

        // Now a live session under the same ID.
        let live_log = Arc::new(SessionEventLog::new(id.clone()));
        let (tx, _rx) = mpsc::channel::<AgentInput>(1);
        sessions.insert(
            id.clone(),
            NewSession {
                input_tx: tx,
                forwarder: tokio::spawn(async {}),
                agent: tokio::spawn(async {}),
                event_log: live_log.clone(),
                pending_approvals: Arc::new(Mutex::new(HashMap::new())),
                parent_session_id: Some("live-parent".into()),
                capability: Arc::new(Mutex::new(Capability::root())),
                cache_ttl: CacheTtl::default(),
                provider_status: Arc::new(Mutex::new(ProviderStatusTracker::new())),
                mailbox: Arc::new(MailboxState::new()),
                status_watch: None,
                live_context_soft_limit: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                dialogue_poller: None,
            },
        );

        let listing = sessions.snapshot_for_listing();
        assert_eq!(listing.len(), 1);
        assert_eq!(
            listing[0].2,
            Some("live-parent".into()),
            "live entry should shadow ghost in listing"
        );
        // event_log returns the live one (Arc::ptr_eq for identity).
        let got = sessions.event_log(&id).expect("event_log");
        assert!(
            Arc::ptr_eq(&got, &live_log),
            "event_log should return the live log when both exist"
        );
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
                cache_ttl: CacheTtl::default(),
                provider_status: tracker,
                mailbox: Arc::new(MailboxState::new()),
                status_watch: None,
                live_context_soft_limit: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                dialogue_poller: None,
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
                usage_semantics: None,
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
                cache_ttl: CacheTtl::default(),
                provider_status: tracker.clone(),
                mailbox: Arc::new(MailboxState::new()),
                status_watch: None,
                live_context_soft_limit: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                dialogue_poller: None,
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
