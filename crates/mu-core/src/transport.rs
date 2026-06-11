use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use serde::Serialize;
use serde_json::{json, Value};
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Notify;

use crate::command_journal::Origin;
use crate::protocol::{
    ErrorObject, Notification, ProviderStatusEvent, Request, Response, TextDeltaEvent,
    JSONRPC_VERSION,
};

// ===== Public API =====

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    /// Outbound channel closed before all messages were flushed.
    #[error("outbound channel closed")]
    OutboundClosed,
    /// The connection's outbound lane overflowed with durable traffic
    /// it was not draining (spec mu-046 INV-11 slow-consumer policy):
    /// the lane was poisoned and the writer terminated, closing this
    /// connection's outbound. The command journal / session logs hold
    /// everything durable that did not reach the wire.
    #[error("slow consumer: outbound lane overflowed; connection outbound closed")]
    SlowConsumer,
}

/// Standard JSON-RPC 2.0 error codes.
pub mod codes {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;

    // mu-fnn (mu-7rk-c): application-defined codes for the connect-time
    // auth enforcement gate. JSON-RPC 2.0 reserves -32099..=-32000 for
    // server-defined application errors.
    /// The method requires an authenticated connection (per-connection
    /// `AuthState` is `Unauthenticated`).
    pub const AUTH_REQUIRED: i32 = -32001;
    /// The connection's `AuthState` is terminally `Denied` — including
    /// re-attempts of pre-auth methods are rejected until reconnect.
    pub const AUTH_DENIED: i32 = -32002;
    /// spec mu-046 INV-2 (fail closed): the command could not be made
    /// durable — the journal append errored — so it was rejected and
    /// never processed.
    pub const JOURNAL_UNAVAILABLE: i32 = -32003;
}

/// Build a successful Response<Value>. Caller has already serialized
/// the result type into a Value.
pub fn ok_response(id: Value, result: Value) -> Response<Value> {
    Response::Ok {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id,
        result,
    }
}

/// Build an error Response<Value>.
pub fn err_response(id: Value, code: i32, message: impl Into<String>) -> Response<Value> {
    Response::Err {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id,
        error: ErrorObject {
            code,
            message: message.into(),
            data: None,
        },
    }
}

// ===== Two-tier outbound (spec mu-046 INV-11, WP9) =====

/// Lane depth at which eviction starts (spec mu-046 INV-11): a push
/// into a lane already holding this many envelopes evicts the OLDEST
/// EPHEMERAL envelope still queued. A healthy consumer never sees a
/// queue anywhere near this deep.
pub const EPHEMERAL_PRESSURE_CAP: usize = 1024;

/// Lane depth at which a durable-only backlog poisons the lane (spec
/// mu-046 INV-11 slow-consumer DISCONNECT). Durable growth is normally
/// self-limiting — responses are 1:1 with commands the client itself
/// sent — so a lane this deep with nothing ephemeral left to shed is
/// wedged beyond rescue: the writer terminates, closing the
/// connection's outbound. Nothing is lost from the system of record;
/// every durable item is derivable from the command journal / session
/// logs, which are the recovery path on reconnect.
pub const LANE_HARD_CAP: usize = 65536;

/// Wire method of the one-shot pressure-clears notice: when a lane
/// evicted ephemeral envelopes and pressure has since cleared, the
/// next push is preceded by `connection.lagged { dropped: n }` so the
/// client KNOWS deltas were skipped (and can re-read state from the
/// journals if it cares). Enqueued durable — the notice itself is
/// never evicted.
pub const CONNECTION_LAGGED_METHOD: &str = "connection.lagged";

/// Delivery tier of an outbound envelope (spec mu-046 INV-11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutboundTier {
    /// Never evicted while the connection lives: responses (all of
    /// them) and lifecycle-significant notifications a client must
    /// not miss. The matching-engine framing: an execution report for
    /// YOUR order is delivered or the session is torn down — it is
    /// never silently skipped.
    Durable,
    /// High-volume live-feed ticks (text deltas, provider-status
    /// updates) that may be evicted under pressure: the consolidated
    /// state they stream toward is already durable in the session
    /// log, so a slow consumer loses only liveness, not truth.
    Ephemeral,
}

/// Notification methods classified [`OutboundTier::Ephemeral`] — the
/// high-volume live feeds. Deliberately a closed allowlist: a method
/// NOT here (including unknown/future ones) defaults DURABLE, failing
/// safe against loss. Everything lifecycle-significant —
/// `session.done`, `session.error`, `session.input_required`,
/// `session.tool_call_*`, `session.mailbox_message`, the autonomous
/// lifecycle events — therefore rides durable without being listed.
const EPHEMERAL_METHODS: &[&str] = &[TextDeltaEvent::METHOD, ProviderStatusEvent::METHOD];

/// A tagged item on the daemon-wide outbound router (spec mu-046
/// INV-8: all responses and notifications leave through this seam —
/// no writer bypasses it).
///
/// `origin: None` means broadcast — every connection's lane receives
/// it. `Some(o)` means only the lane whose [`Origin`] matches.
#[derive(Clone, Debug)]
pub struct OutboundEnvelope {
    /// Which connection this belongs to; `None` ⇒ broadcast.
    pub origin: Option<Origin>,
    /// JSON-RPC id for response correlation (`None` for notifications).
    pub request_id: Option<Value>,
    /// Command-journal correlation (spec mu-046). Tagged by the ingest
    /// pipeline once commands are journaled (WP3); `None` until then.
    pub command_seq: Option<u64>,
    /// The serialized Response or Notification.
    pub item: Outbound,
}

impl OutboundEnvelope {
    /// Classify this envelope's delivery tier (spec mu-046 INV-11).
    /// Responses (no `method` on the item) are always durable;
    /// notifications are durable unless their method is on the
    /// [`EPHEMERAL_METHODS`] allowlist — unknown methods fail safe.
    pub fn tier(&self) -> OutboundTier {
        match self.item.0.get("method").and_then(Value::as_str) {
            Some(method) if EPHEMERAL_METHODS.contains(&method) => OutboundTier::Ephemeral,
            _ => OutboundTier::Durable,
        }
    }
}

/// The daemon-wide outbound router (spec mu-046 INV-8: the one way
/// bytes leave the daemon; INV-11: two-tier delivery).
///
/// ## Why a router and not a broadcast
///
/// The previous implementation was one daemon-wide
/// `tokio::sync::broadcast` ring shared by every connection: a
/// subscriber that fell behind dropped envelopes — RESPONSES included
/// — and, worse, one connection stalled on its own socket had its
/// traffic evicted by OTHER sessions' token deltas advancing the
/// shared ring. Wrong semantics. This is the exchange answer instead:
/// **per-consumer egress queues with an explicit slow-consumer
/// policy.** Each connection registers one ordered lane; producers
/// route envelopes to the addressed lane (or all lanes for
/// broadcast); pressure on one lane is invisible to every other.
///
/// ## The two tiers and the slow-consumer policy
///
/// Within a lane, envelopes are [`OutboundTier`]-classified at push:
///
/// - **Durable** (responses, receipts, lifecycle notifications) is
///   never dropped while the connection lives.
/// - **Ephemeral** (live-feed ticks) may be evicted oldest-first once
///   the lane exceeds [`EPHEMERAL_PRESSURE_CAP`]; when pressure
///   clears, a durable [`CONNECTION_LAGGED_METHOD`] notice tells the
///   client how many ticks it missed.
/// - A lane that exceeds [`LANE_HARD_CAP`] with nothing ephemeral
///   left to shed is **poisoned**: the consumer (the connection's
///   writer) observes it, logs the drop counters, and terminates —
///   the slow consumer is disconnected. The command journal / session
///   logs remain the recovery path (spec mu-046 INV-11).
///
/// One queue per connection — not two — so per-connection wire
/// ordering is exactly emission ordering, as it was under the
/// broadcast. Producers NEVER block and never fail: `send` is a push
/// under a short mutex.
///
/// Cheap to clone; every clone is a producer handle. When the last
/// clone drops, all lanes close and their consumers exit after
/// draining — the same shutdown cascade the broadcast's
/// `RecvError::Closed` used to provide.
#[derive(Clone, Debug)]
pub struct Router {
    inner: Arc<RouterInner>,
}

#[derive(Debug, Default)]
struct RouterInner {
    /// Registered lanes. Weak: the consumer ([`ConnectionLane`]) owns
    /// the lane; a dead entry (consumer exited) is pruned on the next
    /// `send`.
    lanes: Mutex<Vec<LaneEntry>>,
    next_lane_id: AtomicU64,
}

#[derive(Debug)]
struct LaneEntry {
    id: u64,
    origin: Origin,
    lane: Weak<LaneShared>,
}

impl Drop for RouterInner {
    /// Last producer handle dropped — daemon (or test harness)
    /// shutting down. Close every lane so consumers drain what is
    /// queued and exit (the broadcast-`Closed` cascade, preserved).
    fn drop(&mut self) {
        let entries = match self.lanes.get_mut() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
        for entry in entries.drain(..) {
            if let Some(lane) = entry.lane.upgrade() {
                lane.close();
            }
        }
    }
}

impl Router {
    /// Create an empty router (no lanes registered yet).
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RouterInner::default()),
        }
    }

    /// Register a connection's lane. The returned [`ConnectionLane`]
    /// is the lane's single consumer handle — give it to the
    /// connection's writer (or demux) task; dropping it unregisters
    /// the lane. Only envelopes sent after this call are observed.
    pub fn register(&self, origin: Origin) -> ConnectionLane {
        let shared = Arc::new(LaneShared::default());
        let id = self.inner.next_lane_id.fetch_add(1, Ordering::Relaxed);
        lock_recovering(&self.inner.lanes).push(LaneEntry {
            id,
            origin: origin.clone(),
            lane: Arc::downgrade(&shared),
        });
        ConnectionLane {
            id,
            origin,
            shared,
            router: Arc::downgrade(&self.inner),
        }
    }

    /// Route an envelope: `origin: Some(o)` delivers to o's lane(s);
    /// `None` broadcasts to every lane. NON-BLOCKING always (push
    /// under a short mutex); a send with no matching lane is silently
    /// ignored — the daemon may emit before any connection attaches.
    /// Never panics, never fails.
    pub fn send(&self, envelope: OutboundEnvelope) {
        let mut lanes = lock_recovering(&self.inner.lanes);
        // Deliver while pruning entries whose consumer is gone.
        lanes.retain(|entry| {
            let Some(lane) = entry.lane.upgrade() else {
                return false;
            };
            let matches = match &envelope.origin {
                Some(origin) => *origin == entry.origin,
                None => true,
            };
            if matches {
                lane.push(&entry.origin, envelope.clone());
            }
            true
        });
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

/// Lock a mutex, recovering from poisoning: the guarded sections are
/// straight-line queue/registry operations that cannot leave the data
/// incoherent mid-panic, and wedging the daemon's entire outbound over
/// one poisoned lock would invert the isolation the Router exists for.
fn lock_recovering<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Why a lane stopped producing envelopes (see
/// [`ConnectionLane::recv`]).
#[derive(Debug)]
pub enum LaneTerminated {
    /// Every [`Router`] producer handle dropped and the queue is
    /// drained — clean shutdown.
    Closed,
    /// The slow-consumer policy tripped ([`LANE_HARD_CAP`], spec
    /// mu-046 INV-11): this connection must be disconnected. The
    /// counter reports ephemeral envelopes evicted over the lane's
    /// life (queued durable items were discarded at poison time; the
    /// journals are the recovery path).
    SlowConsumer { dropped_ephemeral: u64 },
}

#[derive(Debug, Default)]
struct LaneShared {
    state: Mutex<LaneState>,
    /// Wakes the lane's single consumer.
    notify: Notify,
}

#[derive(Debug, Default)]
struct LaneState {
    /// The connection's ONE ordered egress queue — tier rides
    /// alongside so the eviction scan need not re-classify.
    queue: VecDeque<(OutboundEnvelope, OutboundTier)>,
    /// Ephemeral envelopes currently queued (short-circuits the
    /// eviction scan when zero).
    ephemeral_queued: usize,
    /// Ephemeral envelopes evicted over the lane's life.
    dropped_ephemeral_total: u64,
    /// Evictions since the last `connection.lagged` notice.
    dropped_since_notice: u64,
    poisoned: bool,
    closed: bool,
}

impl LaneShared {
    /// Non-blocking push with the INV-11 pressure policy. See the
    /// [`Router`] doc for the full framing.
    fn push(&self, origin: &Origin, envelope: OutboundEnvelope) {
        let mut state = lock_recovering(&self.state);
        if state.closed || state.poisoned {
            // A dead lane drops everything: the consumer is exiting
            // (or gone) and the journals already hold the durable
            // record.
            return;
        }
        // Pressure cleared with evictions outstanding: enqueue the
        // one-shot lagged notice (durable, in-order, ahead of this
        // item) so the client learns what it missed.
        if state.dropped_since_notice > 0 && state.queue.len() < EPHEMERAL_PRESSURE_CAP {
            let dropped = std::mem::take(&mut state.dropped_since_notice);
            state
                .queue
                .push_back((lagged_envelope(origin, dropped), OutboundTier::Durable));
        }
        if state.queue.len() >= EPHEMERAL_PRESSURE_CAP {
            if state.ephemeral_queued > 0 {
                // Evict the OLDEST ephemeral still queued; durable
                // items are NEVER evicted.
                if let Some(pos) = state
                    .queue
                    .iter()
                    .position(|(_, tier)| *tier == OutboundTier::Ephemeral)
                {
                    state.queue.remove(pos);
                    state.ephemeral_queued -= 1;
                    state.dropped_ephemeral_total += 1;
                    state.dropped_since_notice += 1;
                }
            } else if state.queue.len() >= LANE_HARD_CAP {
                // Durable-only backlog past the hard cap: the
                // connection is wedged beyond rescue. Poison the lane
                // (slow-consumer DISCONNECT); the consumer logs and
                // terminates on its next recv. The queued items are
                // discarded — they are all derivable from the command
                // journal / session logs.
                state.poisoned = true;
                state.queue.clear();
                state.ephemeral_queued = 0;
                drop(state);
                self.notify.notify_one();
                return;
            }
        }
        let tier = envelope.tier();
        if tier == OutboundTier::Ephemeral {
            state.ephemeral_queued += 1;
        }
        state.queue.push_back((envelope, tier));
        drop(state);
        self.notify.notify_one();
    }

    /// Close the lane: the consumer drains what is queued, then
    /// observes [`LaneTerminated::Closed`].
    fn close(&self) {
        lock_recovering(&self.state).closed = true;
        self.notify.notify_one();
    }
}

/// Build the one-shot `connection.lagged { dropped }` notification
/// envelope (durable — the notice itself is never evicted). Built by
/// hand so the push path is infallible.
fn lagged_envelope(origin: &Origin, dropped: u64) -> OutboundEnvelope {
    OutboundEnvelope {
        origin: Some(origin.clone()),
        request_id: None,
        command_seq: None,
        item: Outbound(json!({
            "jsonrpc": JSONRPC_VERSION,
            "method": CONNECTION_LAGGED_METHOD,
            "params": { "dropped": dropped },
        })),
    }
}

/// A connection's egress lane — the single-consumer handle returned by
/// [`Router::register`]. Owned by the connection's writer (or demux)
/// task; dropping it unregisters the lane from the router.
#[derive(Debug)]
pub struct ConnectionLane {
    id: u64,
    origin: Origin,
    shared: Arc<LaneShared>,
    /// Weak: a lingering consumer must not keep the router (and thus
    /// the shutdown cascade) alive.
    router: Weak<RouterInner>,
}

impl ConnectionLane {
    /// Pop the next envelope in emission order; waits when the queue
    /// is empty. Terminates with [`LaneTerminated::Closed`] after the
    /// queue drains post-shutdown, or [`LaneTerminated::SlowConsumer`]
    /// immediately when the lane is poisoned.
    pub async fn recv(&self) -> Result<OutboundEnvelope, LaneTerminated> {
        loop {
            {
                let mut state = lock_recovering(&self.shared.state);
                if state.poisoned {
                    return Err(LaneTerminated::SlowConsumer {
                        dropped_ephemeral: state.dropped_ephemeral_total,
                    });
                }
                if let Some((envelope, tier)) = state.queue.pop_front() {
                    if tier == OutboundTier::Ephemeral {
                        state.ephemeral_queued -= 1;
                    }
                    return Ok(envelope);
                }
                if state.closed {
                    return Err(LaneTerminated::Closed);
                }
            }
            // Single consumer + `notify_one` (which stores a permit
            // when no waiter is registered): a push between the
            // unlock above and this await leaves a permit, so the
            // wakeup cannot be missed.
            self.shared.notify.notified().await;
        }
    }

    /// This lane's connection identity.
    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    /// Ephemeral envelopes evicted from this lane over its life
    /// (observability + tests).
    pub fn dropped_ephemeral(&self) -> u64 {
        lock_recovering(&self.shared.state).dropped_ephemeral_total
    }
}

impl Drop for ConnectionLane {
    /// Consumer gone — unregister so producers stop queueing into a
    /// lane nobody drains. (The registry holds a Weak, so a missed
    /// removal is pruned on the next send; this keeps the map clean.)
    fn drop(&mut self) {
        if let Some(router) = self.router.upgrade() {
            lock_recovering(&router.lanes).retain(|entry| entry.id != self.id);
        }
    }
}

/// Handle on the outbound [`Router`] for emitting notifications.
/// Cheap to clone. Pass into request handlers so they can emit
/// notifications mid-flight.
///
/// Carries an `Option<Origin>`: a writer created for a connection tags
/// its notifications with that connection's origin, so they deliver
/// only there (today's semantics — a session's notifications go to the
/// connection that spawned it). An origin-less writer broadcasts to
/// every connection.
#[derive(Clone, Debug)]
pub struct NotificationWriter {
    origin: Option<Origin>,
    router: Router,
}

impl NotificationWriter {
    /// Create a no-op writer whose notifications are silently dropped.
    /// Used by the MCP server surface where notifications don't need to
    /// be forwarded to the MCP client.
    pub fn sink() -> Self {
        // A private router with no lanes: every send is ignored.
        Self {
            origin: None,
            router: Router::new(),
        }
    }

    /// Origin-less writer: notifications fan out to every lane
    /// registered on `router`.
    pub fn broadcast(router: Router) -> Self {
        Self {
            origin: None,
            router,
        }
    }

    /// Writer whose notifications deliver only to the connection whose
    /// [`Origin`] matches `origin`.
    pub fn for_origin(router: Router, origin: Origin) -> Self {
        Self {
            origin: Some(origin),
            router,
        }
    }

    /// Emit a notification. Returns `Ok(())` even with no lanes —
    /// see §INV-5.
    pub async fn emit<P: Serialize>(&self, method: &str, params: P) -> Result<(), TransportError> {
        let params = serde_json::to_value(params)?;
        let notif = Notification {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.to_string(),
            params,
        };
        let value = serde_json::to_value(&notif)?;
        self.router.send(OutboundEnvelope {
            origin: self.origin.clone(),
            request_id: None,
            command_seq: None,
            item: Outbound(value),
        });
        Ok(())
    }
}

/// Convenience: serve over the process's actual stdin/stdout.
pub async fn serve_stdio<F, Fut>(handler: F) -> Result<(), TransportError>
where
    F: Fn(Request<Value>, NotificationWriter) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<Value>> + Send + 'static,
{
    let stdin = BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    serve(stdin, stdout, handler).await
}

/// Generic transport: read newline-delimited JSON requests from
/// `reader`, dispatch each to `handler` on `tokio::spawn`, write
/// responses and notifications back to `writer`.
///
/// Creates a private [`Router`] for this connection. Daemons that own
/// a daemon-wide router (spec mu-046 INV-8) should call
/// [`serve_with_router`] instead and pass it down.
pub async fn serve<R, W, F, Fut>(reader: R, writer: W, handler: F) -> Result<(), TransportError>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    F: Fn(Request<Value>, NotificationWriter) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<Value>> + Send + 'static,
{
    serve_with_router(reader, writer, Router::new(), handler).await
}

/// Process-wide connection counter so every connection served by this
/// daemon gets a unique [`Origin`] at accept time.
static CONNECTION_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Allocate this connection's identity at accept time.
fn next_stdio_origin() -> Origin {
    let id = CONNECTION_COUNTER.fetch_add(1, Ordering::Relaxed);
    Origin {
        transport: "stdio".into(),
        connection_id: Some(id.to_string()),
    }
}

/// [`serve`] over a caller-owned daemon-wide [`Router`] (spec mu-046
/// INV-8: one way out). This connection gets a fresh [`Origin`]; the
/// handler's responses are enveloped with it (plus the request id)
/// and routed, and the connection's writer drains its registered
/// lane.
///
/// Adapter shim over [`serve_with_ingest`] preserving the historical
/// handler-returns-`Response` contract: each request's handler future
/// is spawned (concurrent dispatch) and its response enveloped onto
/// the router when it resolves. The DAEMON does not use this — it
/// flows through `serve_with_ingest` so every command is journaled
/// before processing (spec mu-046 INV-7, no side doors); this stays
/// for transports/tests that don't carry a journal.
pub async fn serve_with_router<R, W, F, Fut>(
    reader: R,
    writer: W,
    router: Router,
    handler: F,
) -> Result<(), TransportError>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    F: Fn(Request<Value>, NotificationWriter) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<Value>> + Send + 'static,
{
    let handler = Arc::new(handler);
    let respond_router = router.clone();
    serve_with_ingest(reader, writer, router, move |request, notif, origin| {
        let handler = Arc::clone(&handler);
        let router = respond_router.clone();
        async move {
            let request_id = request.id.clone();
            let response_fut = handler(request, notif);
            // Spawned, not awaited inline: this shim keeps the
            // pre-ingest concurrent-dispatch semantics (a slow request
            // must not block the next line). The spawned task holds a
            // router producer clone, so lanes stay open — and the
            // writer drains every response — until it has sent.
            tokio::spawn(async move {
                let response = response_fut.await;
                match serde_json::to_value(response) {
                    Ok(value) => router.send(OutboundEnvelope {
                        origin: Some(origin),
                        request_id: Some(request_id),
                        command_seq: None,
                        item: Outbound(value),
                    }),
                    Err(err) => tracing::warn!(%err, "response serialization failed"),
                }
            });
            None
        }
    })
    .await
}

/// The transport seam of the ingest pipeline (spec mu-046 WP3). Reads
/// newline-delimited JSON requests and hands each parsed request — with
/// this connection's [`Origin`] — to `handler`, which is the
/// ingest/route step:
///
/// - `Some(response)` ⇒ immediate reject (parse-adjacent failure,
///   journal unavailable): the transport envelopes and sends it, as it
///   always did.
/// - `None` ⇒ the pipeline owns the response; it arrives via the
///   outbound router, whose lane [`write_loop`] already delivers
///   (INV-8).
///
/// The handler is awaited INLINE, not spawned: ingest must observe
/// commands in wire order so journal seq order == queue order (INV-3).
/// Keep ingest fast — journal append + enqueue; the heavy work belongs
/// to the pipeline consumer behind the queue.
pub async fn serve_with_ingest<R, W, F, Fut>(
    reader: R,
    writer: W,
    router: Router,
    handler: F,
) -> Result<(), TransportError>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    F: Fn(Request<Value>, NotificationWriter, Origin) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Option<Response<Value>>> + Send + 'static,
{
    let origin = next_stdio_origin();
    let notif = NotificationWriter::for_origin(router.clone(), origin.clone());
    let lane = router.register(origin.clone());
    let mut writer_task = tokio::spawn(write_loop(writer, lane));
    let mut lines = reader.lines();

    // Tracks a writer that died while the client was still sending
    // (slow-consumer poison or write IO failure). A connection with no
    // way to deliver results must not keep executing commands —
    // disconnect means disconnect, so writer death ends the read loop
    // too instead of leaving an execute-without-result half-connection.
    let mut writer_result: Option<Result<(), TransportError>> = None;

    loop {
        let line = tokio::select! {
            line = lines.next_line() => match line? {
                Some(line) => line,
                None => break,
            },
            res = &mut writer_task => {
                writer_result = Some(match res {
                    Ok(result) => result,
                    Err(err) => {
                        tracing::warn!(%err, "writer task failed");
                        Err(TransportError::OutboundClosed)
                    }
                });
                break;
            }
        };
        match parse_request_line(&line) {
            Ok(request) => {
                let request_id = request.id.clone();
                if let Some(response) = handler(request, notif.clone(), origin.clone()).await {
                    match serde_json::to_value(response) {
                        Ok(value) => router.send(OutboundEnvelope {
                            origin: Some(origin.clone()),
                            request_id: Some(request_id),
                            command_seq: None,
                            item: Outbound(value),
                        }),
                        Err(err) => tracing::warn!(%err, "response serialization failed"),
                    }
                }
            }
            Err(response) => {
                let value = serde_json::to_value(response)?;
                let request_id = value.get("id").cloned().unwrap_or(Value::Null);
                router.send(OutboundEnvelope {
                    origin: Some(origin.clone()),
                    request_id: Some(request_id),
                    command_seq: None,
                    item: Outbound(value),
                });
            }
        }
    }

    // CRITICAL for clean shutdown post mu-035 Phase A (multi-turn fix):
    // dropping `handler` here releases the closure that captures the
    // daemon's state. On the ingest path (mu-046) that closure holds
    // the control-plane queue sender: dropping it lets the pipeline
    // consumer exit on recv()==None, which releases the daemon's
    // `sessions` map. On the shim path above, the closure holds the
    // sessions-capturing handler directly. Either way, releasing
    // sessions drops every SessionState, which drops every agent-loop
    // input sender, which lets the per-session agent loops exit on
    // recv()==None, which drops their events senders, which lets the
    // per-session forwarders exit, which drops their
    // NotificationWriter clones, which finally lets `writer_task` see
    // every Router producer clone drop (RouterInner::drop closes the
    // lanes -> recv() drains then returns Closed) and exit. In-flight
    // spawned request tasks hold their own producer clones and extend
    // the writer's life exactly until their responses are sent.
    //
    // Pre-multi-turn this chain worked implicitly because the agent
    // loop returned after one Done — but with multi-turn the loop
    // now survives until its input channel actually closes, which
    // can only happen after sessions drops, which requires this
    // explicit drop.
    drop(handler);
    drop(notif);
    drop(router);

    match writer_result {
        // Writer died first (poison / IO failure) and ended the read
        // loop above; its result is the connection's outcome.
        Some(result) => result,
        None => match writer_task.await {
            Ok(result) => result,
            Err(err) => {
                tracing::warn!(%err, "writer task failed");
                Err(TransportError::OutboundClosed)
            }
        },
    }
}

/// Anything destined for the outbound router: a serialized Response
/// or Notification, already as a Value so it can be flushed without
/// re-borrowing the type. Public because it rides inside
/// [`OutboundEnvelope`] (spec mu-046 INV-8).
#[derive(Clone, Debug)]
pub struct Outbound(pub Value);

// ===== Internal =====

/// Per-connection delivery: drain this connection's [`ConnectionLane`]
/// as JSONL. The lane is pre-filtered (the router only queues
/// envelopes addressed to — or broadcast at — this connection) and
/// single-queue, so per-connection wire ordering is exactly emission
/// ordering.
///
/// Termination preserves the shutdown cascade: when the last [`Router`]
/// producer clone drops, the lane closes and `recv` drains the
/// remaining queue before returning `Closed`. A poisoned lane (spec
/// mu-046 INV-11 slow-consumer policy) terminates the writer with
/// [`TransportError::SlowConsumer`] after logging the drop counters —
/// this connection's outbound closes; the daemon and every other lane
/// are unaffected, and the journals remain the recovery path.
async fn write_loop<W>(mut writer: W, lane: ConnectionLane) -> Result<(), TransportError>
where
    W: AsyncWrite + Unpin,
{
    loop {
        let envelope = match lane.recv().await {
            Ok(envelope) => envelope,
            Err(LaneTerminated::Closed) => break,
            Err(LaneTerminated::SlowConsumer { dropped_ephemeral }) => {
                tracing::error!(
                    origin = ?lane.origin(),
                    dropped_ephemeral,
                    lane_hard_cap = LANE_HARD_CAP,
                    "outbound lane overflowed with durable traffic: disconnecting slow \
                     consumer (spec mu-046 INV-11); the command journal / session logs \
                     hold everything that did not reach the wire"
                );
                return Err(TransportError::SlowConsumer);
            }
        };
        let line = serde_json::to_string(&envelope.item.0)?;
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }
    Ok(())
}

fn parse_request_line(line: &str) -> Result<Request<Value>, Response<Value>> {
    let value = match serde_json::from_str::<Value>(line) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(%err, "malformed json-rpc line");
            return Err(err_response(Value::Null, codes::PARSE_ERROR, "parse error"));
        }
    };

    let id = value.get("id").cloned().unwrap_or(Value::Null);
    match serde_json::from_value::<Request<Value>>(value) {
        Ok(request) => Ok(request),
        Err(err) => {
            tracing::warn!(%err, "invalid json-rpc request");
            Err(err_response(id, codes::INVALID_REQUEST, "invalid request"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;
    use tokio::io::{duplex, AsyncWriteExt, BufReader};

    struct Harness {
        input: tokio::io::DuplexStream,
        output: tokio::io::Lines<BufReader<tokio::io::DuplexStream>>,
        serve_task: tokio::task::JoinHandle<Result<(), TransportError>>,
    }

    fn spawn_harness<F, Fut>(handler: F) -> Harness
    where
        F: Fn(Request<Value>, NotificationWriter) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Response<Value>> + Send + 'static,
    {
        spawn_harness_on_router(Router::new(), handler)
    }

    /// Like [`spawn_harness`] but the connection joins a caller-owned
    /// daemon-wide router — lets tests attach multiple connections to
    /// one router (spec mu-046 INV-8).
    fn spawn_harness_on_router<F, Fut>(router: Router, handler: F) -> Harness
    where
        F: Fn(Request<Value>, NotificationWriter) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Response<Value>> + Send + 'static,
    {
        let (input, server_reader) = duplex(64 * 1024);
        let (server_writer, output) = duplex(64 * 1024);
        let reader = BufReader::new(server_reader);
        let output = BufReader::new(output).lines();
        let serve_task = tokio::spawn(serve_with_router(reader, server_writer, router, handler));
        Harness {
            input,
            output,
            serve_task,
        }
    }

    async fn write_json_line(
        input: &mut tokio::io::DuplexStream,
        value: Value,
    ) -> Result<(), std::io::Error> {
        let line = format!("{value}\n");
        input.write_all(line.as_bytes()).await
    }

    async fn read_value(
        output: &mut tokio::io::Lines<BufReader<tokio::io::DuplexStream>>,
    ) -> Result<Value, Box<dyn std::error::Error>> {
        let line = output.next_line().await?.ok_or("output stream closed")?;
        Ok(serde_json::from_str(&line)?)
    }

    #[tokio::test]
    async fn line_framing_writes_exactly_one_lf() -> Result<(), Box<dyn std::error::Error>> {
        let mut harness =
            spawn_harness(|req, _| async move { ok_response(req.id, json!({"pong": true})) });
        write_json_line(
            &mut harness.input,
            json!({"jsonrpc":"2.0","id":1,"method":"ping","params":null}),
        )
        .await?;

        let raw = harness
            .output
            .next_line()
            .await?
            .ok_or("output stream closed")?;
        assert!(!raw.contains('\n'));
        assert!(!raw.contains('\r'));
        let mut serialized = raw.clone();
        serialized.push('\n');
        assert_eq!(serialized.matches('\n').count(), 1);
        let value: Value = serde_json::from_str(&raw)?;
        assert_eq!(value["id"], json!(1));
        Ok(())
    }

    #[tokio::test]
    async fn round_trip_request() -> Result<(), Box<dyn std::error::Error>> {
        let mut harness =
            spawn_harness(|req, _| async move { ok_response(req.id, json!({"pong": true})) });
        write_json_line(
            &mut harness.input,
            json!({"jsonrpc":"2.0","id":1,"method":"ping","params":null}),
        )
        .await?;

        let response = read_value(&mut harness.output).await?;
        assert_eq!(response["jsonrpc"], json!("2.0"));
        assert_eq!(response["id"], json!(1));
        assert_eq!(response["result"], json!({"pong": true}));
        Ok(())
    }

    #[tokio::test]
    async fn notification_emission_precedes_response() -> Result<(), Box<dyn std::error::Error>> {
        let mut harness = spawn_harness(|req, notif| async move {
            if let Err(err) = notif
                .emit("session.text_delta", json!({"session_id":"s","delta":"hi"}))
                .await
            {
                panic!("notification emit failed: {err}");
            }
            ok_response(req.id, json!({"accepted": true}))
        });
        write_json_line(
            &mut harness.input,
            json!({"jsonrpc":"2.0","id":1,"method":"ask_session","params":null}),
        )
        .await?;

        let notification = read_value(&mut harness.output).await?;
        let response = read_value(&mut harness.output).await?;
        assert_eq!(notification["method"], json!("session.text_delta"));
        assert_eq!(
            notification["params"],
            json!({"session_id":"s","delta":"hi"})
        );
        assert_eq!(response["id"], json!(1));
        assert_eq!(response["result"], json!({"accepted": true}));
        Ok(())
    }

    #[tokio::test]
    async fn malformed_and_invalid_requests_return_errors_and_transport_continues(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut harness =
            spawn_harness(|req, _| async move { ok_response(req.id, json!({"pong": true})) });
        harness.input.write_all(b"{not valid json\n").await?;
        write_json_line(
            &mut harness.input,
            json!({"jsonrpc":"2.0","id":2,"params":null}),
        )
        .await?;
        write_json_line(
            &mut harness.input,
            json!({"jsonrpc":"2.0","id":3,"method":"ping","params":null}),
        )
        .await?;

        let parse_error = read_value(&mut harness.output).await?;
        let invalid_request = read_value(&mut harness.output).await?;
        let valid_response = read_value(&mut harness.output).await?;
        assert_eq!(parse_error["id"], Value::Null);
        assert_eq!(parse_error["error"]["code"], json!(codes::PARSE_ERROR));
        assert_eq!(invalid_request["id"], json!(2));
        assert_eq!(
            invalid_request["error"]["code"],
            json!(codes::INVALID_REQUEST)
        );
        assert_eq!(valid_response["id"], json!(3));
        assert_eq!(valid_response["result"], json!({"pong": true}));
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_dispatch_returns_second_response_first(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut harness = spawn_harness(|req, _| async move {
            if req.id == json!(1) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            ok_response(req.id, json!({"done": true}))
        });
        write_json_line(
            &mut harness.input,
            json!({"jsonrpc":"2.0","id":1,"method":"slow","params":null}),
        )
        .await?;
        write_json_line(
            &mut harness.input,
            json!({"jsonrpc":"2.0","id":2,"method":"fast","params":null}),
        )
        .await?;

        let first = read_value(&mut harness.output).await?;
        let second = read_value(&mut harness.output).await?;
        assert_eq!(first["id"], json!(2));
        assert_eq!(second["id"], json!(1));
        Ok(())
    }

    #[tokio::test]
    async fn eof_terminates_serve_after_draining_response() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut harness =
            spawn_harness(|req, _| async move { ok_response(req.id, json!({"pong": true})) });
        write_json_line(
            &mut harness.input,
            json!({"jsonrpc":"2.0","id":1,"method":"ping","params":null}),
        )
        .await?;
        drop(harness.input);

        let response = read_value(&mut harness.output).await?;
        assert_eq!(response["id"], json!(1));
        let result = harness.serve_task.await?;
        assert!(result.is_ok());
        Ok(())
    }

    /// Two connections on one daemon-wide router: a response envelope
    /// tagged with connection A's origin is queued only on A's lane.
    /// B's first output line is its own response — if A's response had
    /// leaked into B's lane, queue ordering would have put it first
    /// (spec mu-046 INV-8 per-connection routing).
    #[tokio::test]
    async fn response_routes_only_to_originating_connection(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let router = Router::new();
        let handler =
            |req: Request<Value>, _| async move { ok_response(req.id, json!({"pong": true})) };
        let mut conn_a = spawn_harness_on_router(router.clone(), handler);
        let mut conn_b = spawn_harness_on_router(router.clone(), handler);

        write_json_line(
            &mut conn_a.input,
            json!({"jsonrpc":"2.0","id":"for-a","method":"ping","params":null}),
        )
        .await?;
        let response_a = read_value(&mut conn_a.output).await?;
        assert_eq!(response_a["id"], json!("for-a"));

        // A's response has already been routed; B's lane was
        // registered before it was sent, so if the router failed to
        // filter it, it would precede B's own response in B's output.
        write_json_line(
            &mut conn_b.input,
            json!({"jsonrpc":"2.0","id":"for-b","method":"ping","params":null}),
        )
        .await?;
        let response_b = read_value(&mut conn_b.output).await?;
        assert_eq!(response_b["id"], json!("for-b"));
        Ok(())
    }

    /// An origin-less envelope (broadcast) reaches every lane on the
    /// router — emitted via the `NotificationWriter::broadcast`
    /// constructor (spec mu-046 INV-8 fan-out).
    #[tokio::test]
    async fn broadcast_envelope_fans_out_to_all_connections(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let router = Router::new();
        let handler =
            |req: Request<Value>, _| async move { ok_response(req.id, json!({"pong": true})) };
        let mut conn_a = spawn_harness_on_router(router.clone(), handler);
        let mut conn_b = spawn_harness_on_router(router.clone(), handler);

        // Prove both lanes are registered before broadcasting: a
        // delivered response means the connection's writer is live.
        write_json_line(
            &mut conn_a.input,
            json!({"jsonrpc":"2.0","id":1,"method":"ping","params":null}),
        )
        .await?;
        write_json_line(
            &mut conn_b.input,
            json!({"jsonrpc":"2.0","id":2,"method":"ping","params":null}),
        )
        .await?;
        read_value(&mut conn_a.output).await?;
        read_value(&mut conn_b.output).await?;

        let broadcast_writer = NotificationWriter::broadcast(router.clone());
        broadcast_writer
            .emit("daemon.announce", json!({"msg": "hello"}))
            .await?;

        let seen_a = read_value(&mut conn_a.output).await?;
        let seen_b = read_value(&mut conn_b.output).await?;
        assert_eq!(seen_a["method"], json!("daemon.announce"));
        assert_eq!(seen_a["params"], json!({"msg": "hello"}));
        assert_eq!(seen_b["method"], json!("daemon.announce"));
        assert_eq!(seen_b["params"], json!({"msg": "hello"}));
        Ok(())
    }

    #[test]
    fn notification_writer_is_clone_send_and_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        fn assert_clone<T: Clone>() {}
        assert_send::<NotificationWriter>();
        assert_sync::<NotificationWriter>();
        assert_clone::<NotificationWriter>();
    }

    #[tokio::test]
    async fn id_is_preserved_as_value() -> Result<(), Box<dyn std::error::Error>> {
        for id in [json!("abc"), json!(7), Value::Null] {
            let mut harness =
                spawn_harness(|req, _| async move { ok_response(req.id, json!({"ok": true})) });
            write_json_line(
                &mut harness.input,
                json!({"jsonrpc":"2.0","id":id,"method":"echo","params":null}),
            )
            .await?;

            let response = read_value(&mut harness.output).await?;
            assert_eq!(response["id"], id);
        }
        Ok(())
    }

    // ===== spec mu-046 WP9: two-tier router tests =====

    fn test_origin(name: &str) -> Origin {
        Origin {
            transport: "test".into(),
            connection_id: Some(name.into()),
        }
    }

    /// An ephemeral notification envelope (method on the allowlist).
    fn delta_envelope(origin: &Origin, n: usize) -> OutboundEnvelope {
        OutboundEnvelope {
            origin: Some(origin.clone()),
            request_id: None,
            command_seq: None,
            item: Outbound(json!({
                "jsonrpc": "2.0",
                "method": "session.text_delta",
                "params": { "session_id": "s", "delta": format!("d{n}") },
            })),
        }
    }

    /// A durable response envelope.
    fn response_envelope(origin: &Origin, id: usize) -> OutboundEnvelope {
        OutboundEnvelope {
            origin: Some(origin.clone()),
            request_id: Some(json!(id)),
            command_seq: None,
            item: Outbound(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "ok": true },
            })),
        }
    }

    async fn recv_ok(lane: &ConnectionLane) -> OutboundEnvelope {
        tokio::time::timeout(Duration::from_secs(2), lane.recv())
            .await
            .expect("lane recv within 2s")
            .expect("lane open")
    }

    /// Responses are durable; allowlisted live-feed methods are
    /// ephemeral; unknown notification methods fail safe to durable.
    #[test]
    fn tier_classification_defaults_unknown_methods_to_durable() {
        let origin = test_origin("t");
        assert_eq!(response_envelope(&origin, 1).tier(), OutboundTier::Durable);
        assert_eq!(delta_envelope(&origin, 1).tier(), OutboundTier::Ephemeral);
        let provider_status = OutboundEnvelope {
            origin: Some(origin.clone()),
            request_id: None,
            command_seq: None,
            item: Outbound(json!({
                "jsonrpc": "2.0", "method": "session.provider_status", "params": {},
            })),
        };
        assert_eq!(provider_status.tier(), OutboundTier::Ephemeral);
        for durable_method in [
            "session.done",
            "session.error",
            "session.input_required",
            "session.tool_call_started",
            "session.tool_call_completed",
            "session.mailbox_message",
            "session.assistant_text_finalized",
            "some.future.method",
        ] {
            let envelope = OutboundEnvelope {
                origin: Some(origin.clone()),
                request_id: None,
                command_seq: None,
                item: Outbound(json!({
                    "jsonrpc": "2.0", "method": durable_method, "params": {},
                })),
            };
            assert_eq!(
                envelope.tier(),
                OutboundTier::Durable,
                "{durable_method} must classify durable"
            );
        }
    }

    /// Under no pressure a lane is a plain FIFO: durable and ephemeral
    /// interleave in exactly emission order (the single-queue design —
    /// per-connection wire ordering is preserved).
    #[tokio::test]
    async fn interleaved_tiers_arrive_in_emission_order_under_no_pressure() {
        let router = Router::new();
        let origin = test_origin("order");
        let lane = router.register(origin.clone());
        for n in 0..8 {
            router.send(delta_envelope(&origin, n));
            router.send(response_envelope(&origin, n));
        }
        for n in 0..8 {
            let delta = recv_ok(&lane).await;
            assert_eq!(delta.item.0["params"]["delta"], json!(format!("d{n}")));
            let response = recv_ok(&lane).await;
            assert_eq!(response.item.0["id"], json!(n));
        }
        assert_eq!(lane.dropped_ephemeral(), 0);
    }

    /// Pressure policy (spec mu-046 INV-11): with the consumer gated
    /// (not draining), a flood past EPHEMERAL_PRESSURE_CAP evicts the
    /// oldest ephemeral envelopes; every durable item survives in
    /// order; the drop counter matches; and once pressure clears, the
    /// next push is preceded by a `connection.lagged` notice carrying
    /// the dropped count.
    #[tokio::test]
    async fn pressure_evicts_oldest_ephemeral_keeps_durable_and_notifies_lagged() {
        let router = Router::new();
        let origin = test_origin("pressure");
        let lane = router.register(origin.clone());

        // Gated writer: nobody calls recv while we flood. Interleave
        // durable responses among the deltas.
        let deltas_sent = EPHEMERAL_PRESSURE_CAP + 500;
        let mut responses_sent = 0;
        for n in 0..deltas_sent {
            router.send(delta_envelope(&origin, n));
            if n % 100 == 0 {
                router.send(response_envelope(&origin, responses_sent));
                responses_sent += 1;
            }
        }

        // Release the gate: drain everything currently queued.
        let mut deltas_received = Vec::new();
        let mut responses_received = Vec::new();
        let queued = {
            // No more pushes are coming; recv never blocks until the
            // queue empties, so count by draining with a short poll.
            let mut drained = Vec::new();
            loop {
                let recv = tokio::time::timeout(Duration::from_millis(100), lane.recv()).await;
                match recv {
                    Ok(Ok(envelope)) => drained.push(envelope),
                    Ok(Err(t)) => panic!("lane terminated during drain: {t:?}"),
                    Err(_elapsed) => break,
                }
            }
            drained
        };
        for envelope in queued {
            match envelope.item.0.get("method").and_then(Value::as_str) {
                Some("session.text_delta") => deltas_received.push(
                    envelope.item.0["params"]["delta"]
                        .as_str()
                        .expect("delta string")
                        .to_string(),
                ),
                Some(other) => panic!("unexpected notification during flood drain: {other}"),
                None => responses_received.push(envelope.item.0["id"].as_u64().expect("id")),
            }
        }

        // ALL durable items arrived, in order.
        assert_eq!(
            responses_received,
            (0..responses_sent as u64).collect::<Vec<_>>(),
            "every durable response survives pressure, in emission order"
        );
        // Ephemeral arrived count < sent count; the dropped counter
        // accounts exactly for the difference; survivors kept order.
        assert!(
            deltas_received.len() < deltas_sent,
            "pressure must evict some deltas ({} sent, {} received)",
            deltas_sent,
            deltas_received.len()
        );
        assert_eq!(
            lane.dropped_ephemeral(),
            (deltas_sent - deltas_received.len()) as u64,
            "drop counter matches evictions"
        );
        let mut sorted = deltas_received.clone();
        sorted.sort_by_key(|d| d[1..].parse::<usize>().expect("delta index"));
        assert_eq!(deltas_received, sorted, "surviving deltas keep order");

        // Pressure has cleared (queue empty). The next push is
        // preceded by the one-shot connection.lagged notice.
        router.send(response_envelope(&origin, 9999));
        let notice = recv_ok(&lane).await;
        assert_eq!(notice.item.0["method"], json!(CONNECTION_LAGGED_METHOD));
        assert_eq!(
            notice.item.0["params"]["dropped"],
            json!(lane.dropped_ephemeral()),
            "the notice reports the dropped count"
        );
        let trailing = recv_ok(&lane).await;
        assert_eq!(trailing.item.0["id"], json!(9999));
    }

    /// Isolation: connection A wedged (consumer gated, lane under
    /// pressure) neither delays nor drops connection B's traffic —
    /// the per-consumer-queue point of the design.
    #[tokio::test]
    async fn wedged_lane_does_not_delay_or_drop_another_connection() {
        let router = Router::new();
        let origin_a = test_origin("wedged");
        let origin_b = test_origin("healthy");
        let lane_a = router.register(origin_a.clone());
        let lane_b = router.register(origin_b.clone());

        // Wedge A well past the pressure cap.
        for n in 0..(EPHEMERAL_PRESSURE_CAP * 2) {
            router.send(delta_envelope(&origin_a, n));
        }
        assert!(lane_a.dropped_ephemeral() > 0, "A is under pressure");

        // B's traffic flows promptly and intact.
        router.send(delta_envelope(&origin_b, 0));
        router.send(response_envelope(&origin_b, 1));
        let delta = recv_ok(&lane_b).await;
        assert_eq!(delta.item.0["params"]["delta"], json!("d0"));
        let response = recv_ok(&lane_b).await;
        assert_eq!(response.item.0["id"], json!(1));
        assert_eq!(lane_b.dropped_ephemeral(), 0, "B dropped nothing");
    }

    /// Hard cap (spec mu-046 INV-11 slow-consumer DISCONNECT): a
    /// durable-only flood past LANE_HARD_CAP poisons the lane — the
    /// consumer observes SlowConsumer — while the router and every
    /// other lane keep working (the daemon stays alive).
    #[tokio::test]
    async fn durable_flood_past_hard_cap_poisons_lane_and_others_survive() {
        let router = Router::new();
        let origin_a = test_origin("flooded");
        let origin_b = test_origin("alive");
        let lane_a = router.register(origin_a.clone());
        let lane_b = router.register(origin_b.clone());

        // Durable-only: nothing ephemeral to shed, so the queue grows
        // to the hard cap and the lane poisons.
        for n in 0..=LANE_HARD_CAP {
            router.send(response_envelope(&origin_a, n));
        }
        match lane_a.recv().await {
            Err(LaneTerminated::SlowConsumer { dropped_ephemeral }) => {
                assert_eq!(dropped_ephemeral, 0, "nothing ephemeral was ever queued");
            }
            other => panic!("expected SlowConsumer, got {other:?}"),
        }
        // Sends to the poisoned lane are no-ops, not errors.
        router.send(response_envelope(&origin_a, 0));

        // The other connection still round-trips.
        router.send(response_envelope(&origin_b, 42));
        let response = recv_ok(&lane_b).await;
        assert_eq!(response.item.0["id"], json!(42));
    }

    /// The writer terminates with `TransportError::SlowConsumer` on a
    /// poisoned lane (and logs the counters) — the connection's
    /// outbound closes; nothing else does.
    #[tokio::test]
    async fn write_loop_terminates_on_poisoned_lane() {
        let router = Router::new();
        let origin = test_origin("writer-poison");
        let lane = router.register(origin.clone());
        for n in 0..=LANE_HARD_CAP {
            router.send(response_envelope(&origin, n));
        }
        let (writer, _read_side) = duplex(64 * 1024);
        let result = write_loop(writer, lane).await;
        assert!(
            matches!(result, Err(TransportError::SlowConsumer)),
            "poisoned lane must terminate the writer: {result:?}"
        );
    }

    /// A connection whose outbound half has died (write IO failure or
    /// slow-consumer poison) must stop executing commands: writer
    /// death ends serve_with_ingest's read loop even though the
    /// client keeps the input open — disconnect means disconnect, not
    /// execute-without-result.
    #[tokio::test]
    async fn writer_death_ends_ingest_read_loop() {
        let (mut input, server_reader) = duplex(64 * 1024);
        let (server_writer, read_side) = duplex(256);
        // Outbound half dead: every write fails with BrokenPipe.
        drop(read_side);
        let serve_task = tokio::spawn(serve_with_ingest(
            BufReader::new(server_reader),
            server_writer,
            Router::new(),
            |req, _notif, _origin| async move { Some(ok_response(req.id, json!({"pong": true}))) },
        ));
        write_json_line(
            &mut input,
            json!({"jsonrpc":"2.0","id":1,"method":"ping","params":null}),
        )
        .await
        .expect("input write");
        // `input` stays OPEN — serve must return on its own.
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), serve_task)
            .await
            .expect("serve must return despite the open input")
            .expect("serve task join");
        assert!(
            result.is_err(),
            "writer death is the connection's outcome: {result:?}"
        );
        drop(input);
    }

    /// Dropping the last Router producer clone closes lanes; a
    /// consumer drains what is queued, then observes Closed — the
    /// shutdown-cascade contract write_loop relies on.
    #[tokio::test]
    async fn router_drop_closes_lanes_after_drain() {
        let router = Router::new();
        let origin = test_origin("drain");
        let lane = router.register(origin.clone());
        router.send(response_envelope(&origin, 1));
        drop(router);
        let envelope = recv_ok(&lane).await;
        assert_eq!(envelope.item.0["id"], json!(1));
        match lane.recv().await {
            Err(LaneTerminated::Closed) => {}
            other => panic!("expected Closed after drain, got {other:?}"),
        }
    }
}
