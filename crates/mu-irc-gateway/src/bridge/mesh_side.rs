//! The mesh half of the bridge: everything that outlives one IRC connection.
//!
//! The NATS connection and the IRC connection fail independently, so the tasks
//! and the state that belong to the mesh are owned here rather than by a
//! session — they are started once, they keep running through a connect, a
//! registration and a five-minute reconnect backoff, and what they know is what
//! the next session starts from.
//!
//! One task per *input*, and none of them owns the bridge's state:
//!
//! - [`ingress_gate`] drains the endpoint and observer subscriptions ALWAYS and
//!   forwards only while an IRC session is registered, which is what makes an
//!   IRC outage lossy rather than retentive;
//! - [`presence_worker`] runs `front`/`release` in order (a rename is a release
//!   and a front that must not overlap) and keeps trying the fronting that
//!   failed, because a human IRC still reports present is a human the gateway
//!   still owes a mesh inbox;
//! - [`publish_worker`] runs the fan-out publishes in order, one worker per IRC
//!   session over a bounded queue that dies with it, and drops rather than
//!   delivers anything the mesh went down under;
//! - [`discovery_worker`] sweeps `$SRV` on the refresh timer into a `watch`, so
//!   the NEWEST snapshot wins the slot instead of the oldest;
//! - [`nats_watcher`] tracks the connection itself, in every phase, so what a
//!   session reads is true of the process and not of one loop's attention — and
//!   it counts the outages, because a [`LinkState::generation`] survives a flap
//!   that a `watch` coalesces away and a boolean does not.
//!
//! **Teardown is a property of the type, not of the caller.** Every task is held
//! through an abort-on-drop guard and the fronted endpoints are released from
//! [`MeshSide`]'s `Drop` as well as from [`MeshSide::shutdown`], so a panic, an
//! early return or a cancelled shutdown cannot leave a worker running or a human
//! registered on `$SRV`. `shutdown` remains the graceful path: it is the one
//! that can WAIT for the releases.
//!
//! **Secret-safe and body-free.** No log line here carries a message body or a
//! credential — not even at `debug`. Diagnostics name classes, counts and peer
//! ids.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use mu_dialogue::mesh::{
    self, ConnectionEvent, Gateway, InboundDm, MeshDmEvent, MeshTarget, ObserverSubscription,
};
use mu_peer::PeerId;

use crate::config::GatewayConfig;

/// How often `$SRV` discovery is swept.
///
/// Each sweep is also the channel-reconciliation tick: which channels the
/// gateway should be in is a function of who is on the mesh, so the two happen
/// together rather than on two timers that can disagree.
const DISCOVERY_REFRESH: Duration = Duration::from_secs(30);

/// How many mesh events may wait for the select loop.
///
/// A live mirror, not a queue: deep enough that a burst of DMs behind one slow
/// IRC write is not lost, shallow enough that it can never become a store. Past
/// it, and at ALL times while no session is registered, events are dropped and
/// counted by [`ingress_gate`].
const INGRESS_QUEUE: usize = 256;

/// How many fan-out publishes may wait for the publish worker.
///
/// Bounded for the same reason and created per session: a human types at human
/// speed, so this absorbs a burst behind one slow fan-out and nothing more. A
/// full queue drops the line and counts it.
pub const PUBLISH_QUEUE: usize = 64;

/// The ceiling on the presence retry backoff, in reconcile ticks.
///
/// A human whose fronting failed is retried on the tick after the failure, then
/// after two, four, eight — up to this many. At [`DISCOVERY_REFRESH`] a tick,
/// that is a five-minute ceiling: long enough not to hammer a mesh that is
/// refusing, short enough that a human does not sit without an inbox for an
/// hour after one refused. It is a CEILING, not a give-up count: they are still
/// on IRC, so the gateway still owes them an endpoint.
const PRESENCE_RETRY_MAX_TICKS: u32 = 10;

/// How long to wait for every human endpoint to be released at shutdown.
const RELEASE_GRACE: Duration = Duration::from_secs(10);

/// The mesh as `$SRV` discovery last saw it: which peers are live, and the
/// subject each one advertises.
///
/// The subject is the peer's OWN advertisement, never re-derived here — a peer
/// predating the hierarchical subjects listens only on its flat one, and
/// deriving a subject for it would publish where nobody is listening.
#[derive(Debug, Clone, Default)]
pub struct Discovery {
    /// The peers the sweep found, sorted.
    pub peers: Vec<PeerId>,
    subjects: HashMap<PeerId, String>,
}

impl Discovery {
    /// Build a snapshot from a `$SRV` sweep (`peer id → advertised subject`).
    pub fn from_srv(agents: HashMap<String, String>) -> Self {
        let mut peers: Vec<PeerId> = Vec::with_capacity(agents.len());
        let mut subjects = HashMap::with_capacity(agents.len());
        for (id, subject) in agents {
            let peer = PeerId::parse(&id);
            peers.push(peer.clone());
            subjects.insert(peer, subject);
        }
        // Stable order, so channel effects and log lines are reproducible.
        peers.sort();
        Discovery { peers, subjects }
    }

    /// Where to publish for `peer`, using the subject it advertised.
    ///
    /// `session` stays `None`: the advertised subject IS this peer's address,
    /// which is the case `Gateway::address` treats as needing no session field.
    /// The gateway only ever targets a peer it found in this snapshot, so there
    /// is no daemon-fallback case to name a session for.
    pub fn target(&self, peer: &PeerId) -> Option<MeshTarget> {
        self.subjects.get(peer).map(|subject| MeshTarget {
            subject: subject.clone(),
            session: None,
        })
    }
}

/// The mesh link: whether it is up, and WHICH link it is.
///
/// A bare `bool` cannot answer the second question, and a `watch` keeps only the
/// latest value: a `false` followed by a `true` while the publish worker is
/// parked inside one `publish(job).await` coalesces into a single `true`, and
/// the outage vanishes from the record before anything reads it. The generation
/// is the missing edge — it counts DOWN transitions, so a reader that remembers
/// the generation it saw can tell "still the link I was on" from "a different
/// link that happens to be up again", however many flaps it slept through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkState {
    /// Whether NATS is connected right now.
    pub up: bool,
    /// How many times the link has gone down. Incremented on the down
    /// transition, so every generation names one unbroken connection.
    pub generation: u64,
}

impl LinkState {
    /// The state a freshly connected mesh starts in.
    pub const fn connected() -> Self {
        LinkState {
            up: true,
            generation: 0,
        }
    }
}

/// A presence change to run against the mesh, in order.
pub enum PresenceOp {
    Front(PeerId),
    Release(PeerId),
    /// The reconcile tick: retry every human that is wanted on the mesh but
    /// whose registration failed, and whose backoff has run out.
    Retry,
    /// Release every peer this gateway fronts, and answer when done — the
    /// shutdown path, which must not race the process exiting.
    ReleaseAll(oneshot::Sender<usize>),
}

/// What the presence worker needs of the mesh.
///
/// A trait rather than an `Arc<Gateway>` for the same reason the publish and
/// discovery workers take closures: the part worth testing here is the RETRY
/// discipline — who stays wanted after a failure, and when it is tried again —
/// and none of that should need a NATS server to exercise.
pub trait MeshPresence: Send + Sync + 'static {
    /// Front `peer`, routing its DMs to `events`. `Ok(false)` means it was
    /// already fronted.
    fn front(
        &self,
        peer: &str,
        events: mpsc::UnboundedSender<MeshDmEvent>,
    ) -> impl Future<Output = Result<bool>> + Send;
    /// Release `peer`; whether it was fronted at all.
    fn release(&self, peer: &str) -> impl Future<Output = bool> + Send;
    /// Every peer currently fronted.
    fn fronted(&self) -> impl Future<Output = Vec<String>> + Send;
}

impl MeshPresence for Gateway {
    fn front(
        &self,
        peer: &str,
        events: mpsc::UnboundedSender<MeshDmEvent>,
    ) -> impl Future<Output = Result<bool>> + Send {
        self.front_peer_events(peer, events)
    }

    fn release(&self, peer: &str) -> impl Future<Output = bool> + Send {
        self.release_peer(peer)
    }

    fn fronted(&self) -> impl Future<Output = Vec<String>> + Send {
        self.fronted_peers()
    }
}

/// One fan-out publish, already decided and already loop-guarded.
pub struct PublishJob {
    pub id: String,
    pub from: String,
    pub targets: Vec<MeshTarget>,
    pub body: String,
    /// The [`LinkState::generation`] current when this job was accepted.
    ///
    /// It travels WITH the job because the job's whole claim to being delivered
    /// is "the mesh was up when a human typed it" — and that claim is about one
    /// particular link, not about the mesh being up again by the time the worker
    /// gets to it. [`publish_worker`] drops a job whose generation is not the
    /// current one.
    pub generation: u64,
}

/// A spawned task that is CANCELLED when its owner is dropped, rather than
/// detached.
///
/// A bare `JoinHandle` does the opposite: dropping it lets the task run on
/// forever, still holding its `Arc<Gateway>`, still sweeping. Every task the
/// mesh side owns is held through one of these so that "the owner is gone"
/// means "the task is gone" on the panic and early-return paths too, not only
/// on the one where somebody remembered to call [`MeshSide::shutdown`].
struct AbortOnDrop(JoinHandle<()>);

impl AbortOnDrop {
    fn new(task: JoinHandle<()>) -> Self {
        AbortOnDrop(task)
    }

    /// Cancel now, without waiting for the drop.
    fn abort(&self) {
        self.0.abort();
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The mesh half of the bridge, shared across IRC connections: it outlives any
/// one of them, because the NATS connection and the IRC connection fail
/// independently.
///
/// Generic over the mesh only so the teardown paths can be tested against the
/// [`MeshPresence`] fake the presence worker already uses; in the gateway it is
/// always `MeshSide<Gateway>`.
pub struct MeshSide<P: MeshPresence = Gateway> {
    pub gw: Arc<P>,
    pub presence: mpsc::UnboundedSender<PresenceOp>,
    /// The mesh link, tracked CONTINUOUSLY by [`nats_watcher`] — through
    /// connect, registration, the mirror loop and the whole reconnect backoff
    /// alike.
    ///
    /// It has to be continuous, and it has to be shared: the two connections
    /// fail independently, so an outage that starts while IRC is reconnecting
    /// must already be known to the session that follows rather than being
    /// discovered from an event nobody was reading. A publish decided while
    /// [`LinkState::up`] is false is refused to the human, not held; a publish
    /// decided under an EARLIER [`LinkState::generation`] is dropped rather than
    /// delivered over the link that replaced it.
    pub mesh_up: watch::Receiver<LinkState>,
    /// How many mesh events [`ingress_gate`] has dropped since a session last
    /// reported. Read (and zeroed) at session start, which is where "what
    /// arrived while IRC was down" is diagnosed.
    pub ingress_dropped: Arc<AtomicU64>,
    /// Whether the agent-DM observer is live. False means humans-only: the
    /// gateway still mirrors DMs addressed to the humans it fronts, and simply
    /// cannot see agent-to-agent traffic.
    ///
    /// Shared and mutable because an observation can END after it started — a
    /// server re-applying permissions across a NATS reconnect is the ordinary
    /// case — and the sessions that read it outlive that moment.
    pub observing: Arc<AtomicBool>,

    /// The store-sink half of the mesh connection. This gateway fronts humans
    /// through the EVENT sink, so nothing is read from here — it is held only so
    /// the channel stays open for the life of the connection.
    _store_rx: mpsc::UnboundedReceiver<InboundDm>,
    /// The observer subscription, dropped at shutdown after its watch is.
    observer: Option<ObserverSubscription>,
    observer_watch: Option<AbortOnDrop>,
    /// The tasks above, cancelled together at shutdown — and, failing that, when
    /// this struct is dropped.
    tasks: Vec<AbortOnDrop>,
    /// Whether [`shutdown`](MeshSide::shutdown) already released the fronted
    /// humans. Only then does `Drop` skip its own best-effort release.
    released: bool,
}

/// The ends of the mesh half an IRC session reads and writes.
///
/// Handed out once by [`MeshSide::start`] and reused by every session: they are
/// the mesh half's, not a connection's, which is the whole point — the gate goes
/// on dropping and the sweeper goes on sweeping while IRC is down.
pub struct MeshInputs {
    /// Verified mesh DMs that arrived while a session was registered.
    ///
    /// The slot is the mesh half's, so it survives the session that was reading
    /// it: an event forwarded a moment before `session_live` went false is still
    /// sitting here when the next session starts. [`discard_stale_dms`] is what
    /// empties it, at both ends of a session.
    pub dm_rx: mpsc::Receiver<MeshDmEvent>,
    /// Whether an IRC session is registered. Set false the moment one ends, so
    /// the gate is dropping again before the backoff starts.
    pub session_live: watch::Sender<bool>,
    /// The newest `$SRV` snapshot.
    pub disc_rx: watch::Receiver<Discovery>,
    /// Ask for a sweep now rather than at the next refresh.
    pub kick: mpsc::Sender<()>,
}

impl MeshSide<Gateway> {
    /// Connect to the mesh and start everything that outlives an IRC
    /// connection: the human endpoints' event channel, the optional agent-DM
    /// observer and the watch on its ending, the ordered presence worker, the
    /// ingress gate, the `$SRV` sweeper, and the connection watcher.
    ///
    /// The only failure here is a mesh that cannot be reached at all, which is
    /// one of the two failures no IRC reconnect could fix.
    pub async fn start(config: &GatewayConfig) -> Result<(MeshSide, MeshInputs)> {
        let (gw, store_rx, nats_events) = mesh::connect_with_events(&config.mesh)
            .await
            .map_err(|e| anyhow!("mesh: {e:#}"))?;
        let gw = Arc::new(gw);

        // Verified mesh DMs, from the human endpoints and (optionally) the
        // observer. The subscription APIs hand events to an UNBOUNDED sender by
        // contract, so the bound lives one hop later: `ingress_gate` drains this
        // continuously and forwards into a bounded slot only while a session is
        // registered.
        let (dm_tx, dm_raw) = mpsc::unbounded_channel::<MeshDmEvent>();

        // The observer is opt-in AND fallible: a server that refuses the
        // wildcard leaves the gateway working, just humans-only.
        let observer: Option<ObserverSubscription> = if config.irc.observe_agent_dms {
            match gw.observe_agent_dms(dm_tx.clone()).await {
                Ok(sub) => Some(sub),
                Err(e) => {
                    warn!(
                        "agent-DM observer refused ({e:#}); continuing humans-only — DMs to the \
                         humans this gateway fronts still mirror, agent-to-agent traffic will not"
                    );
                    None
                }
            }
        } else {
            info!("agent-DM observer disabled by config ([irc] observe_agent_dms = false)");
            None
        };
        let observing = Arc::new(AtomicBool::new(observer.is_some()));

        // An observation that started can still end — the ordinary case is a
        // server re-applying permissions across a NATS reconnect — and the event
        // channel CANNOT say so: it is shared with the human endpoints, whose
        // clone of the sender keeps it open. `mu-dialogue` signals the end
        // explicitly, so this watches for it; without it the gateway would
        // silently stop seeing agent-to-agent traffic while still reporting
        // itself as observing.
        let observer_watch = observer.as_ref().map(|sub| {
            let mut ended = sub.ended();
            let observing = observing.clone();
            tokio::spawn(async move {
                let why = ended.ended().await;
                observing.store(false, Ordering::Relaxed);
                warn!(
                    "agent-DM observation ended ({why}); continuing humans-only — DMs to the \
                     humans this gateway fronts still mirror, agent-to-agent traffic will not"
                );
            })
        });
        let observer_watch = observer_watch.map(AbortOnDrop::new);

        let (presence_tx, presence_rx) = mpsc::unbounded_channel();
        let presence_task = tokio::spawn(presence_worker(gw.clone(), dm_tx.clone(), presence_rx));

        // The ingress boundary. `session_live` is what makes an IRC outage lossy
        // rather than retentive, and it is owned out here so the gate keeps
        // draining through connect, registration and the whole backoff.
        let (dm_tx_gated, dm_rx) = mpsc::channel::<MeshDmEvent>(INGRESS_QUEUE);
        let (session_live, live_rx) = watch::channel(false);
        let ingress_dropped = Arc::new(AtomicU64::new(0));
        let gate_task = tokio::spawn(ingress_gate(
            dm_raw,
            live_rx,
            dm_tx_gated,
            ingress_dropped.clone(),
        ));

        let (disc_tx, disc_rx) = watch::channel(Discovery::default());
        let (kick_tx, kick_rx) = mpsc::channel::<()>(1);
        let sweeper = gw.clone();
        let discovery_task = tokio::spawn(discovery_worker(
            move || {
                let gw = sweeper.clone();
                async move { gw.srv_agents().await }
            },
            disc_tx,
            kick_rx,
        ));

        // The mesh link, tracked by a task of its own. The events arrive whether
        // or not an IRC session is up, and this is what makes the flag true of
        // the whole process rather than only of the moments a mirror loop
        // happens to be selecting on them. Starts up, at generation 0: this is
        // reached only on a successful connect.
        let (link_tx, link_rx) = watch::channel(LinkState::connected());
        let nats_task = tokio::spawn(nats_watcher(nats_events, link_tx));

        let mesh_side = MeshSide {
            gw,
            presence: presence_tx,
            mesh_up: link_rx,
            ingress_dropped,
            observing,
            _store_rx: store_rx,
            observer,
            observer_watch,
            tasks: [presence_task, gate_task, discovery_task, nats_task]
                .into_iter()
                .map(AbortOnDrop::new)
                .collect(),
            released: false,
        };
        let inputs = MeshInputs {
            dm_rx,
            session_live,
            disc_rx,
            kick: kick_tx,
        };
        Ok((mesh_side, inputs))
    }
}

impl<P: MeshPresence> MeshSide<P> {
    /// Release every human endpoint, then stop every task.
    ///
    /// The release is not optional and not best-effort-skippable: a fronted peer
    /// that is not released stays discoverable on `$SRV` until something else
    /// notices it is gone, and phantom peers are what the release path exists to
    /// prevent. It is also why this is `async` and awaited: it runs the releases
    /// through the ordered presence worker and WAITS for them, which a `Drop`
    /// cannot do.
    ///
    /// This is the graceful path, not the only one. [`Drop`](MeshSide::drop)
    /// covers the rest — a panic, an early return, a cancelled shutdown — by
    /// cancelling the tasks and spawning the same releases without being able to
    /// wait for them.
    pub async fn shutdown(mut self) {
        let (done_tx, done_rx) = oneshot::channel();
        if self.presence.send(PresenceOp::ReleaseAll(done_tx)).is_ok() {
            match tokio::time::timeout(RELEASE_GRACE, done_rx).await {
                Ok(Ok(n)) => {
                    self.released = true;
                    info!("released {n} human mesh endpoint(s)");
                }
                Ok(Err(_)) => warn!("presence worker ended before releasing endpoints"),
                Err(_) => {
                    warn!("releasing human mesh endpoints timed out; some may linger on $SRV")
                }
            }
        }
        // The watch is torn down BEFORE the subscription, so its own
        // abort-on-drop is not reported as an observation that failed.
        self.observer_watch = None;
        self.observer = None;
        // The task guards do the aborting; this only makes it happen here rather
        // than at the end of the statement that drops `self`.
        self.tasks.clear();
    }
}

/// Teardown on every path that is not [`shutdown`](MeshSide::shutdown).
///
/// A gateway that drops its mesh side — a panic, an early return, a shutdown
/// whose future was cancelled mid-await — used to detach its workers and leave
/// every human it fronted registered on `$SRV`, which is exactly the phantom
/// presence the release path exists to prevent. So the guarantee lives in the
/// type instead of in a calling convention: the tasks are cancelled by their
/// guards, and the releases are SPAWNED, because `Drop` cannot await them.
///
/// Spawned is weaker than awaited — a process exiting immediately may not give
/// the task a poll — which is why `shutdown` still exists and is still the path
/// the gateway takes. This is the floor, not the plan.
impl<P: MeshPresence> Drop for MeshSide<P> {
    fn drop(&mut self) {
        if let Some(watch) = &self.observer_watch {
            watch.abort();
        }
        for task in &self.tasks {
            task.abort();
        }
        if self.released {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            warn!(
                "mesh side dropped outside a runtime; fronted human endpoint(s) may linger on $SRV"
            );
            return;
        };
        let gw = self.gw.clone();
        runtime.spawn(async move {
            let mut n = 0;
            for id in gw.fronted().await {
                if gw.release(&id).await {
                    n += 1;
                }
            }
            if n > 0 {
                warn!(
                    "mesh side dropped without a graceful shutdown; released {n} human mesh \
                     endpoint(s) after the fact"
                );
            }
        });
    }
}

// ─────────────────────────────── Workers ────────────────────────────────────

// ─────────────────────────────── Workers ────────────────────────────────────

/// A human who should be fronted on the mesh but is not, because the
/// registration failed. Kept until it succeeds or they leave IRC.
struct WantedFront {
    /// Reconcile ticks still to wait before the next attempt.
    wait: u32,
    /// How many attempts have failed, which is what the backoff doubles on.
    attempts: u32,
}

impl WantedFront {
    /// Schedule the next attempt: 1 tick, then 2, 4, 8 … up to
    /// [`PRESENCE_RETRY_MAX_TICKS`].
    fn failed_again(&mut self) {
        self.attempts = self.attempts.saturating_add(1);
        self.wait = (1u32 << (self.attempts - 1).min(31)).min(PRESENCE_RETRY_MAX_TICKS);
    }
}

/// Runs presence changes in order, and keeps trying the ones that failed.
///
/// Ordering is the point: a rename is a release of the old identity and a front
/// of the new one, and running those concurrently can leave the old endpoint
/// alive.
///
/// So is persistence. A human is present because IRC says they are, and
/// membership goes on saying so; a `front` that failed once is therefore a
/// human the gateway still owes an inbox, not an event to log and forget. Every
/// failure leaves them in `wanted`, every [`PresenceOp::Retry`] tick tries the
/// ones whose backoff has run out, and only a release (or a success) takes them
/// out. Without it a human who joined during a mesh outage would stay invisible
/// to the mesh until they left IRC and came back.
async fn presence_worker<P: MeshPresence>(
    mesh: Arc<P>,
    events: mpsc::UnboundedSender<MeshDmEvent>,
    mut ops: mpsc::UnboundedReceiver<PresenceOp>,
) {
    // Peer id → its retry schedule. Keyed by the id the mesh knows, so a rename
    // is two different entries, exactly as it is two different endpoints.
    let mut wanted: HashMap<String, WantedFront> = HashMap::new();
    while let Some(op) = ops.recv().await {
        match op {
            PresenceOp::Front(peer) => {
                let id = peer.to_string();
                front_once(&*mesh, &events, &mut wanted, &id).await;
            }
            PresenceOp::Release(peer) => {
                let id = peer.to_string();
                // They are gone from IRC: stop wanting them on the mesh.
                wanted.remove(&id);
                if mesh.release(&id).await {
                    info!(peer = %id, "released human from the mesh");
                }
            }
            PresenceOp::Retry => {
                let due: Vec<String> = wanted
                    .iter_mut()
                    .filter_map(|(id, w)| match w.wait {
                        0 => Some(id.clone()),
                        _ => {
                            w.wait -= 1;
                            None
                        }
                    })
                    .collect();
                for id in due {
                    front_once(&*mesh, &events, &mut wanted, &id).await;
                }
            }
            PresenceOp::ReleaseAll(done) => {
                wanted.clear();
                let mut n = 0;
                for id in mesh.fronted().await {
                    if mesh.release(&id).await {
                        n += 1;
                    }
                }
                let _ = done.send(n);
            }
        }
    }
}

/// One fronting attempt, and what it does to the retry set.
async fn front_once<P: MeshPresence>(
    mesh: &P,
    events: &mpsc::UnboundedSender<MeshDmEvent>,
    wanted: &mut HashMap<String, WantedFront>,
    id: &str,
) {
    match mesh.front(id, events.clone()).await {
        Ok(true) => {
            wanted.remove(id);
            info!(peer = %id, "fronting human on the mesh");
        }
        Ok(false) => {
            wanted.remove(id);
            debug!(peer = %id, "human already fronted");
        }
        Err(e) => {
            let entry = wanted.entry(id.to_string()).or_insert(WantedFront {
                wait: 0,
                attempts: 0,
            });
            entry.failed_again();
            warn!(
                peer = %id,
                retry_in_ticks = entry.wait,
                "cannot front human on the mesh: {e:#}"
            );
        }
    }
}

/// Tracks the NATS connection for the whole process, not just for the moments an
/// IRC mirror loop is selecting on it.
///
/// The bridge has three phases where nothing was reading these events — the TCP
/// connect, registration, and a backoff that reaches five minutes — and an
/// outage that starts in one of them is exactly the outage a session must not
/// begin by assuming away. One task, one flag, every phase.
///
/// Only a real transition is published, so a session's `changed()` arm means
/// "the link flipped", never "another `Connected` for a link that never went
/// down".
///
/// Every down transition also starts a new [`LinkState::generation`]. That is
/// the part a reader can still use after the fact: a `watch` coalesces, so a
/// flap that happened while somebody was parked in an `await` is gone from the
/// flag by the time they look — but not from the count.
pub async fn nats_watcher(
    mut events: mpsc::UnboundedReceiver<ConnectionEvent>,
    up: watch::Sender<LinkState>,
) {
    while let Some(event) = events.recv().await {
        let want = match event {
            ConnectionEvent::Disconnected => false,
            ConnectionEvent::Connected => true,
            other => {
                debug!("mesh connection event: {other}");
                continue;
            }
        };
        let changed = up.send_if_modified(|state| {
            if state.up == want {
                return false;
            }
            state.up = want;
            if !want {
                // A new link begins here, whoever is or is not watching.
                state.generation = state.generation.wrapping_add(1);
            }
            true
        });
        if changed && !want {
            warn!("mesh connection lost; nothing will mirror until it returns");
        } else if changed {
            info!("mesh connection re-established");
        }
    }
}

/// The mesh→IRC ingress boundary: drain the subscriptions ALWAYS, forward only
/// while an IRC session is registered.
///
/// The subscriptions themselves stay up across an IRC outage on purpose — 4a's
/// observer-refusal detection is a property of a subscription that was made and
/// is still watched, and re-subscribing per session would trade a memory bound
/// for a blind spot. So the bound and the drop live here instead: this task
/// never awaits anything but the next event, which is what makes it impossible
/// for a connect, a registration or a five-minute backoff to accumulate bodies.
///
/// A full forward slot drops too. The consumer is one select loop writing to a
/// socket; if it is that far behind, the events behind it are already stale.
async fn ingress_gate(
    mut raw: mpsc::UnboundedReceiver<MeshDmEvent>,
    live: watch::Receiver<bool>,
    out: mpsc::Sender<MeshDmEvent>,
    dropped: Arc<AtomicU64>,
) {
    while let Some(event) = raw.recv().await {
        if !*live.borrow() {
            // No IRC session to put it on. Not held for the next one.
            dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        match out.try_send(event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                dropped.fetch_add(1, Ordering::Relaxed);
            }
            // The bridge is gone.
            Err(mpsc::error::TrySendError::Closed(_)) => return,
        }
    }
}

/// Runs fan-out publishes in order, and only while the mesh is up. Failures are
/// reported body-free and dropped: the gateway is a live mirror and has nowhere
/// to queue a failed publish.
///
/// **A disconnect empties the queue.** A job was accepted because the mesh was
/// up when the line was typed; if the link drops before the job runs, delivering
/// it after the reconnect would put a line minutes late into a conversation that
/// has moved on — the replay this gateway is not, arrived at by accident. So the
/// link is watched here too: a transition to down drains whatever is waiting
/// (counted).
///
/// The drain alone is not enough, because this worker spends most of its life
/// parked inside `publish(job).await` and a `watch` keeps only the latest value.
/// A `false` and a `true` that both land during one such await coalesce: the
/// `changed()` arm resumes reading `up`, the drain never runs, and a job
/// accepted before the outage publishes after the reconnect. So the per-job
/// check is on [`LinkState::generation`] rather than on the flag — the job
/// carries the generation it was accepted under, and a job from any earlier one
/// is dropped whatever the flag says now.
///
/// `publish` is a seam rather than a `Gateway` so the queue discipline — what
/// the worker does with a job, and what it must NOT do with one after a
/// disconnect or after teardown — is testable without a mesh. One worker per IRC
/// session, over a bounded queue that dies with it.
pub async fn publish_worker<F, Fut>(
    publish: F,
    mut jobs: mpsc::Receiver<PublishJob>,
    mut mesh_up: watch::Receiver<LinkState>,
    dropped: Arc<AtomicU64>,
) where
    F: Fn(PublishJob) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    loop {
        tokio::select! {
            changed = mesh_up.changed() => {
                // The sender lives as long as the process; an error here means
                // the bridge is gone.
                if changed.is_err() {
                    return;
                }
                if !mesh_up.borrow_and_update().up {
                    let n = drain_jobs(&mut jobs);
                    if n > 0 {
                        dropped.fetch_add(n, Ordering::Relaxed);
                        warn!("mesh connection lost; dropped {n} queued publish(es) rather than \
                               delivering them after it returns");
                    }
                }
            }
            job = jobs.recv() => {
                let Some(job) = job else { return };
                // Accepted under a link that has since gone down — whether it is
                // still down or already back — is dropped, not held. The
                // generation is what survives a flap the `watch` coalesced away.
                let link = *mesh_up.borrow();
                if !link.up || job.generation != link.generation {
                    dropped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                let (id, from, targets) = (job.id.clone(), job.from.clone(), job.targets.len());
                if let Err(e) = publish(job).await {
                    warn!(
                        id = %id,
                        from = %from,
                        targets = targets,
                        "publishing to the mesh failed: {e:#}"
                    );
                }
            }
        }
    }
}

/// Throw away every publish job waiting in `jobs`, returning how many. Bodies go
/// with them; nothing about a dropped job is logged but its count.
fn drain_jobs(jobs: &mut mpsc::Receiver<PublishJob>) -> u64 {
    let mut n = 0;
    while jobs.try_recv().is_ok() {
        n += 1;
    }
    n
}

/// Sweeps `$SRV` on the refresh timer, or immediately when kicked (a fresh IRC
/// connection and a NATS reconnect both need a snapshot now rather than in half
/// a minute).
///
/// The slot holds ONE snapshot and the NEWEST one wins: a `watch`, not a
/// one-deep queue. A queue of depth one keeps the OLDEST value and rejects
/// what follows, which is the opposite of what a liveness view wants — a stale
/// view of who is on the mesh is worth nothing, and reconciling channels from
/// one causes JOIN/PART churn against peers that are already gone.
///
/// `sweep` is a seam rather than a `Gateway` so that discipline is testable
/// without a mesh.
async fn discovery_worker<F, Fut>(
    sweep: F,
    out: watch::Sender<Discovery>,
    mut kick: mpsc::Receiver<()>,
) where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<HashMap<String, String>>>,
{
    let mut tick = tokio::time::interval(DISCOVERY_REFRESH);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        match sweep().await {
            Ok(agents) => {
                if out.send(Discovery::from_srv(agents)).is_err() {
                    return; // the bridge is gone
                }
            }
            Err(e) => warn!("mesh discovery sweep failed: {e:#}"),
        }
        tokio::select! {
            _ = tick.tick() => {}
            kicked = kick.recv() => {
                if kicked.is_none() {
                    return;
                }
            }
        }
    }
}

/// Forget whatever discovery snapshot is sitting in the slot.
///
/// It was collected for a connection that no longer exists, and the reconcile a
/// fresh session runs first is the one that decides which channels it joins. The
/// kick that follows replaces it within a sweep.
pub fn discard_stale_discovery(rx: &mut watch::Receiver<Discovery>) {
    rx.mark_unchanged();
}

/// Throw away every mesh event still sitting in the ingress slot, returning how
/// many. Bodies go with them; nothing about a discarded event is logged but its
/// count.
///
/// The same argument as [`discard_stale_discovery`], for the other slot the mesh
/// half hands out. [`ingress_gate`] stops FORWARDING the moment `session_live`
/// goes false, but what it forwarded a moment earlier is already in the channel,
/// and the channel is the mesh half's — it outlives the session that was reading
/// it. Left there, those bodies are delivered to the NEXT session: a DM answered
/// minutes late by a connection it was never addressed to, which is the replay
/// this gateway is not.
///
/// Called at BOTH ends of a session. At teardown because that is the moment the
/// events stop being anybody's; at start because the gate and the teardown race
/// — an event can be forwarded between `session_live` going false and the
/// discard that follows it.
pub fn discard_stale_dms(rx: &mut mpsc::Receiver<MeshDmEvent>) -> u64 {
    let mut n = 0;
    while rx.try_recv().is_ok() {
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use mu_dialogue::mesh::Reception;

    fn dm(id: &str) -> MeshDmEvent {
        MeshDmEvent {
            id: id.into(),
            destination: "mu.agent.human.alice.dm".into(),
            from: "cc:abc".into(),
            body: "a body that must not be retained across an outage".into(),
            subject: None,
            session: None,
            reception: Reception::Observer,
        }
    }

    /// A job accepted under the first link the gateway had.
    fn job(id: &str) -> PublishJob {
        job_at(id, 0)
    }

    /// A job accepted under link generation `generation`.
    fn job_at(id: &str, generation: u64) -> PublishJob {
        PublishJob {
            id: id.into(),
            from: "human:alice".into(),
            targets: vec![MeshTarget {
                subject: "mu.agent.cc.abc.dm".into(),
                session: None,
            }],
            body: "b".into(),
            generation,
        }
    }

    /// The link, after `generation` outages, in the state `up`.
    fn link_state(up: bool, generation: u64) -> LinkState {
        LinkState { up, generation }
    }

    /// Wait for `cond`, or fail the test rather than hang forever.
    async fn until(what: &str, mut cond: impl FnMut() -> bool) {
        for _ in 0..500 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// A mesh whose fronting fails a set number of times per peer, and which
    /// records every attempt. The presence worker's retry discipline is the
    /// thing under test, so the mesh it runs against is this and not NATS.
    #[derive(Default)]
    struct FlakyMesh {
        /// Peer id → how many further attempts to refuse.
        refuse: Mutex<HashMap<String, u32>>,
        fronted: Mutex<Vec<String>>,
        attempts: Mutex<Vec<String>>,
        released: Mutex<Vec<String>>,
    }

    impl FlakyMesh {
        /// A mesh that refuses `times` fronting attempts for `peer`, then works.
        fn refusing(peer: &str, times: u32) -> Self {
            let mesh = FlakyMesh::default();
            mesh.refuse.lock().unwrap().insert(peer.into(), times);
            mesh
        }

        fn attempts(&self) -> Vec<String> {
            self.attempts.lock().unwrap().clone()
        }
    }

    impl MeshPresence for FlakyMesh {
        fn front(
            &self,
            peer: &str,
            _events: mpsc::UnboundedSender<MeshDmEvent>,
        ) -> impl Future<Output = Result<bool>> + Send {
            self.attempts.lock().unwrap().push(peer.to_string());
            let refused = {
                let mut refuse = self.refuse.lock().unwrap();
                let left = refuse.entry(peer.to_string()).or_insert(0);
                let refused = *left > 0;
                *left = left.saturating_sub(1);
                refused
            };
            if !refused {
                self.fronted.lock().unwrap().push(peer.to_string());
            }
            async move {
                match refused {
                    true => Err(anyhow!("the mesh refused this registration")),
                    false => Ok(true),
                }
            }
        }

        fn release(&self, peer: &str) -> impl Future<Output = bool> + Send {
            self.released.lock().unwrap().push(peer.to_string());
            let was = {
                let mut fronted = self.fronted.lock().unwrap();
                let before = fronted.len();
                fronted.retain(|p| p != peer);
                fronted.len() != before
            };
            async move { was }
        }

        fn fronted(&self) -> impl Future<Output = Vec<String>> + Send {
            let fronted = self.fronted.lock().unwrap().clone();
            async move { fronted }
        }
    }

    /// Run a presence worker over `ops` to completion, against `mesh`.
    async fn run_presence(mesh: Arc<FlakyMesh>, ops: Vec<PresenceOp>) {
        let (events, _events_rx) = mpsc::unbounded_channel();
        let (tx, rx) = mpsc::unbounded_channel();
        let worker = tokio::spawn(presence_worker(mesh, events, rx));
        for op in ops {
            tx.send(op).expect("the worker is alive");
        }
        drop(tx);
        worker.await.expect("the worker ends when its ops do");
    }

    // ───────────────────────── Discovery ────────────────────────────────────

    #[test]
    fn discovery_keeps_the_subject_a_peer_advertised() {
        // A legacy daemon advertises a FLAT subject; re-deriving one from the
        // peer id would publish where it is not listening.
        let snapshot = Discovery::from_srv(HashMap::from([
            ("mu:legacy".to_string(), "mu.agent.legacy.dm".to_string()),
            ("cc:abc".to_string(), "mu.agent.cc.abc.dm".to_string()),
        ]));
        assert_eq!(snapshot.peers.len(), 2);
        assert_eq!(
            snapshot
                .target(&PeerId::parse("mu:legacy"))
                .unwrap()
                .subject,
            "mu.agent.legacy.dm"
        );
        assert_eq!(
            snapshot.target(&PeerId::parse("cc:abc")).unwrap().subject,
            "mu.agent.cc.abc.dm"
        );
        // A peer that is not in the snapshot has no target at all.
        assert!(snapshot.target(&PeerId::parse("cc:nope")).is_none());
    }

    // ───────────────────────────── Discovery ────────────────────────────────

    #[tokio::test]
    async fn the_newest_sweep_wins_the_slot_the_bridge_has_not_read() {
        // A one-deep queue keeps the OLDEST value and rejects everything after
        // it, which is backwards for a liveness view: the bridge would reconcile
        // channels against peers that left minutes ago.
        let (out, mut rx) = watch::channel(Discovery::default());
        let (kick_tx, kick_rx) = mpsc::channel(1);
        let sweeps = Arc::new(AtomicU64::new(0));
        let counter = sweeps.clone();
        let worker = tokio::spawn(discovery_worker(
            move || {
                let counter = counter.clone();
                async move {
                    let n = counter.fetch_add(1, Ordering::Relaxed);
                    Ok(HashMap::from([(
                        format!("cc:sweep{n}"),
                        "mu.agent.cc.x.dm".to_string(),
                    )]))
                }
            },
            out,
            kick_rx,
        ));

        // The consumer never reads while the sweeps happen.
        kick_tx.send(()).await.unwrap();
        drop(kick_tx);
        worker.await.unwrap();

        let total = sweeps.load(Ordering::Relaxed);
        assert!(total >= 2, "the worker swept more than once: {total}");
        assert_eq!(
            rx.borrow_and_update().peers,
            vec![PeerId::parse(&format!("cc:sweep{}", total - 1))],
            "the slot holds the newest sweep, not the first"
        );
    }

    #[test]
    fn a_stale_snapshot_does_not_reach_a_fresh_session() {
        let (out, mut rx) = watch::channel(Discovery::default());
        out.send(Discovery::from_srv(HashMap::from([(
            "cc:gone".to_string(),
            "mu.agent.cc.gone.dm".to_string(),
        )])))
        .unwrap();
        assert!(rx.has_changed().unwrap());

        // A session starts here: whatever is in the slot was collected for a
        // connection that no longer exists.
        discard_stale_discovery(&mut rx);
        assert!(
            !rx.has_changed().unwrap(),
            "the first reconcile would have run against a dead peer"
        );

        // The kick's sweep is what the session actually sees.
        out.send(Discovery::from_srv(HashMap::from([(
            "cc:live".to_string(),
            "mu.agent.cc.live.dm".to_string(),
        )])))
        .unwrap();
        assert!(rx.has_changed().unwrap());
        assert_eq!(rx.borrow_and_update().peers, vec![PeerId::parse("cc:live")]);
    }

    // ─────────────────────── The mesh→IRC ingress bound ──────────────────────

    // ─────────────────────── The mesh→IRC ingress bound ──────────────────────

    #[tokio::test]
    async fn an_irc_outage_retains_no_observed_dm() {
        // The subscriptions stay up through connect, registration and backoff —
        // that is what keeps 4a's observer-refusal detection alive — so the drop
        // has to happen at this boundary instead. Bodies are not held for a
        // connection that does not exist.
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::channel(64);
        let (session_live, live_rx) = watch::channel(false);
        let dropped = Arc::new(AtomicU64::new(0));
        let gate = tokio::spawn(ingress_gate(raw_rx, live_rx, out_tx, dropped.clone()));

        for i in 0..32 {
            raw_tx.send(dm(&format!("id{i}"))).unwrap();
        }
        drop(raw_tx);
        gate.await.unwrap();

        assert_eq!(dropped.load(Ordering::Relaxed), 32);
        assert!(out_rx.try_recv().is_err(), "nothing was retained");
        drop(session_live);
    }

    #[tokio::test]
    async fn a_registered_session_receives_what_the_gate_forwards() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::channel(64);
        let (session_live, live_rx) = watch::channel(true);
        let dropped = Arc::new(AtomicU64::new(0));
        let gate = tokio::spawn(ingress_gate(raw_rx, live_rx, out_tx, dropped.clone()));

        for i in 0..3 {
            raw_tx.send(dm(&format!("id{i}"))).unwrap();
        }
        drop(raw_tx);
        gate.await.unwrap();

        assert_eq!(dropped.load(Ordering::Relaxed), 0);
        let mut ids = Vec::new();
        while let Ok(ev) = out_rx.try_recv() {
            ids.push(ev.id);
        }
        assert_eq!(ids, vec!["id0", "id1", "id2"]);
        drop(session_live);
    }

    #[tokio::test]
    async fn a_full_ingress_slot_drops_and_counts_rather_than_growing() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::channel(2);
        let (session_live, live_rx) = watch::channel(true);
        let dropped = Arc::new(AtomicU64::new(0));
        let gate = tokio::spawn(ingress_gate(raw_rx, live_rx, out_tx, dropped.clone()));

        for i in 0..10 {
            raw_tx.send(dm(&format!("id{i}"))).unwrap();
        }
        drop(raw_tx);
        gate.await.unwrap();

        assert_eq!(dropped.load(Ordering::Relaxed), 8);
        let mut held = 0;
        while out_rx.try_recv().is_ok() {
            held += 1;
        }
        assert_eq!(held, 2, "the slot is a bound, not a backlog");
        drop(session_live);
    }

    #[tokio::test]
    async fn a_dm_buffered_as_a_session_ended_does_not_reach_the_next_one() {
        // The gate stops forwarding the moment `session_live` goes false, but
        // the slot is the mesh half's and outlives the session that was reading
        // it. Whatever was forwarded a moment earlier is still sitting there
        // when the next connection registers — addressed to a session that no
        // longer exists, and minutes old by the time it would be written.
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (out_tx, mut dm_rx) = mpsc::channel(INGRESS_QUEUE);
        let fresh = out_tx.clone();
        let (session_live, live_rx) = watch::channel(true);
        let dropped = Arc::new(AtomicU64::new(0));
        let gate = tokio::spawn(ingress_gate(raw_rx, live_rx, out_tx, dropped.clone()));

        // A live session that never gets to read its slot.
        for i in 0..8 {
            raw_tx.send(dm(&format!("id{i}"))).unwrap();
        }
        drop(raw_tx);
        gate.await.unwrap();
        assert_eq!(dropped.load(Ordering::Relaxed), 0, "the session was live");

        // …and then it ends.
        session_live.send_replace(false);
        assert_eq!(
            discard_stale_dms(&mut dm_rx),
            8,
            "the slot is emptied at teardown, and counted"
        );

        // The next session starts. Nothing from the last one is waiting for it,
        // and the second discard is what covers the gate/teardown race.
        session_live.send_replace(true);
        assert_eq!(discard_stale_dms(&mut dm_rx), 0);
        assert!(
            dm_rx.try_recv().is_err(),
            "a body from the previous connection survived the boundary"
        );

        // The slot still works: this session gets what arrives during it.
        fresh.send(dm("after")).await.unwrap();
        assert_eq!(dm_rx.try_recv().expect("the slot is live").id, "after");
        drop(session_live);
    }

    // ───────────────────────── The IRC→mesh publish bound ────────────────────

    // ───────────────────────── The IRC→mesh publish bound ────────────────────

    #[tokio::test]
    async fn a_torn_down_session_leaves_no_publish_job_to_fire_later() {
        // A worker and a queue per session, both cancelled with it. Otherwise a
        // job typed on one connection is delivered against the next, which is
        // exactly the replay this gateway is not.
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let published: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        let sink = |gate: Arc<tokio::sync::Semaphore>, seen: Arc<Mutex<Vec<String>>>| {
            move |job: PublishJob| {
                let (gate, seen) = (gate.clone(), seen.clone());
                async move {
                    let _permit = gate.acquire().await.expect("the gate is never closed");
                    seen.lock().unwrap().push(job.id);
                    Ok(())
                }
            }
        };

        // A control: with the sink open, the worker delivers what it is given.
        let (tx, rx) = mpsc::channel(8);
        let (up_tx, up_rx) = watch::channel(LinkState::connected());
        gate.add_permits(3);
        let worker = tokio::spawn(publish_worker(
            sink(gate.clone(), published.clone()),
            rx,
            up_rx,
            Arc::new(AtomicU64::new(0)),
        ));
        for id in ["a", "b", "c"] {
            tx.send(job(id)).await.unwrap();
        }
        drop(tx);
        worker.await.unwrap();
        assert_eq!(*published.lock().unwrap(), ["a", "b", "c"]);

        // Now the real case: jobs queued behind a slow fan-out when the session
        // ends. None of them may fire, then or later.
        let stuck = Arc::new(tokio::sync::Semaphore::new(0));
        let late: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::channel(8);
        let worker = tokio::spawn(publish_worker(
            sink(stuck.clone(), late.clone()),
            rx,
            up_tx.subscribe(),
            Arc::new(AtomicU64::new(0)),
        ));
        for id in ["d", "e", "f"] {
            tx.send(job(id)).await.unwrap();
        }
        tokio::task::yield_now().await;

        worker.abort(); // …session teardown…
        drop(tx); // …and the queue dies with it.
        let _ = worker.await;
        stuck.add_permits(10);
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert!(
            late.lock().unwrap().is_empty(),
            "a job from a dead session fired: {:?}",
            late.lock().unwrap()
        );
        drop(up_tx);
    }

    #[tokio::test]
    async fn a_publish_queued_before_a_disconnect_never_fires_after_the_reconnect() {
        // The queue is per session, but a NATS outage is not: jobs accepted
        // while the mesh was up used to sit in it through the outage and publish
        // on the other side, which is the replay this gateway is not.
        let published: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = published.clone();
        let (tx, rx) = mpsc::channel(8);
        let (up, up_rx) = watch::channel(LinkState::connected());
        let dropped = Arc::new(AtomicU64::new(0));

        for id in ["a", "b", "c"] {
            tx.send(job(id))
                .await
                .expect("the queue takes them while up");
        }
        // …and the mesh goes away before the worker gets to any of them.
        up.send_replace(link_state(false, 1));
        let worker = tokio::spawn(publish_worker(
            move |job: PublishJob| {
                let seen = seen.clone();
                async move {
                    seen.lock().unwrap().push(job.id);
                    Ok(())
                }
            },
            rx,
            up_rx,
            dropped.clone(),
        ));

        // The mesh comes back. Nothing that was waiting may be delivered now.
        tokio::time::sleep(Duration::from_millis(20)).await;
        up.send_replace(link_state(true, 1));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            published.lock().unwrap().is_empty(),
            "a line typed before the outage was delivered after it: {:?}",
            published.lock().unwrap()
        );
        assert_eq!(dropped.load(Ordering::Relaxed), 3, "dropped, and counted");

        // The worker is still live and still publishes what is typed now.
        tx.send(job_at("d", 1)).await.unwrap();
        drop(tx);
        worker.await.unwrap();
        assert_eq!(*published.lock().unwrap(), ["d"]);
    }

    #[tokio::test]
    async fn a_flap_the_watch_coalesced_still_drops_the_jobs_behind_it() {
        // The worker spends its life parked inside `publish(job).await`, and a
        // `watch` keeps only the latest value. A disconnect and a reconnect that
        // both land in there coalesce into one `true`: the `changed()` arm
        // resumes to a link that is up, the drain never runs, and the per-job
        // flag check sees up as well — so a line typed before the outage goes
        // out after it. The generation is what the flag cannot say.
        let published: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (started_tx, mut started_rx) = mpsc::unbounded_channel::<String>();
        let (tx, rx) = mpsc::channel(8);
        let (up, up_rx) = watch::channel(LinkState::connected());
        let dropped = Arc::new(AtomicU64::new(0));

        let worker = {
            let (gate, seen) = (gate.clone(), published.clone());
            tokio::spawn(publish_worker(
                move |job: PublishJob| {
                    let (gate, seen, started) = (gate.clone(), seen.clone(), started_tx.clone());
                    async move {
                        let _ = started.send(job.id.clone());
                        let _permit = gate.acquire().await.expect("the gate is never closed");
                        seen.lock().unwrap().push(job.id);
                        Ok(())
                    }
                },
                rx,
                up_rx,
                dropped.clone(),
            ))
        };

        // One job is in flight and parked…
        tx.send(job("a")).await.unwrap();
        assert_eq!(
            started_rx.recv().await.expect("the worker took it"),
            "a",
            "the worker is parked in the publish"
        );
        // …another was accepted behind it, under the same link…
        tx.send(job("b")).await.unwrap();
        // …and the link flaps entirely within that await.
        up.send_replace(link_state(false, 1));
        up.send_replace(link_state(true, 1));

        // A line typed now, on the new link, is this session's to deliver.
        tx.send(job_at("c", 1)).await.unwrap();
        gate.add_permits(10);
        drop(tx);
        worker.await.unwrap();

        // "a" was already in flight when the link went — that publish is the
        // mesh's to finish or fail. "b" is the one this closes: accepted under
        // the old link, still queued, and never delivered over the new one.
        assert_eq!(
            *published.lock().unwrap(),
            ["a", "c"],
            "a job queued under the old link was delivered over the new one"
        );
        assert_eq!(dropped.load(Ordering::Relaxed), 1, "dropped, and counted");
    }

    // ────────────────────── Human presence, and its retries ──────────────────

    // ────────────────────── Human presence, and its retries ──────────────────

    #[tokio::test]
    async fn a_failed_front_waits_before_it_is_retried() {
        // Bounded backoff: the tick right after a failure spends the wait, it
        // does not spend another registration attempt.
        let mesh = Arc::new(FlakyMesh::refusing("human:alice", 1));
        run_presence(
            mesh.clone(),
            vec![PresenceOp::Front(PeerId::human("alice")), PresenceOp::Retry],
        )
        .await;
        assert_eq!(mesh.attempts(), ["human:alice"], "one attempt, then a wait");
    }

    #[tokio::test]
    async fn a_failed_front_is_retried_on_a_later_tick_until_it_takes() {
        // The mesh refuses twice. Membership still says alice is here, so the
        // gateway still owes her an inbox: she stays wanted, and the reconcile
        // ticks keep trying her rather than logging the failure and forgetting.
        let mesh = Arc::new(FlakyMesh::refusing("human:alice", 2));
        let mut ops = vec![PresenceOp::Front(PeerId::human("alice"))];
        ops.extend((0..6).map(|_| PresenceOp::Retry));
        run_presence(mesh.clone(), ops).await;

        assert_eq!(
            mesh.attempts().len(),
            3,
            "two refusals, then the one that took"
        );
        assert_eq!(*mesh.fronted.lock().unwrap(), ["human:alice"]);
    }

    #[tokio::test]
    async fn a_human_who_left_is_no_longer_retried() {
        // The retry set follows presence: they are not on IRC any more, so the
        // gateway stops wanting an endpoint for them.
        let mesh = Arc::new(FlakyMesh::refusing("human:alice", 9));
        let mut ops = vec![
            PresenceOp::Front(PeerId::human("alice")),
            PresenceOp::Release(PeerId::human("alice")),
        ];
        ops.extend((0..6).map(|_| PresenceOp::Retry));
        run_presence(mesh.clone(), ops).await;

        assert_eq!(mesh.attempts(), ["human:alice"], "no retry after a release");
        assert_eq!(*mesh.released.lock().unwrap(), ["human:alice"]);
    }

    // ─────────────────────────── Teardown, on every path ─────────────────────

    #[tokio::test]
    async fn dropping_the_mesh_side_cancels_its_tasks_and_releases_its_humans() {
        // `shutdown` is the graceful path, not the only one: a panic, an early
        // return or a cancelled shutdown drops the owner instead. That used to
        // detach the workers and leave every fronted human on `$SRV` — the
        // phantom presence the release path exists to prevent.
        let mesh = Arc::new(FlakyMesh::default());
        let (events, _events_rx) = mpsc::unbounded_channel();
        let (presence_tx, presence_rx) = mpsc::unbounded_channel();

        // Each task holds a sender that only its cancellation drops, so "the
        // task ended" is observable without holding its handle.
        let (presence_alive, mut presence_ended) = mpsc::channel::<()>(1);
        let presence_task = {
            let mesh = mesh.clone();
            tokio::spawn(async move {
                let _alive = presence_alive;
                presence_worker(mesh, events, presence_rx).await;
            })
        };
        let (worker_alive, mut worker_ended) = mpsc::channel::<()>(1);
        let forever = tokio::spawn(async move {
            let _alive = worker_alive;
            std::future::pending::<()>().await;
        });

        let (_store_tx, store_rx) = mpsc::unbounded_channel();
        let (_link, link_rx) = watch::channel(LinkState::connected());
        let side = MeshSide {
            gw: mesh.clone(),
            presence: presence_tx.clone(),
            mesh_up: link_rx,
            ingress_dropped: Arc::new(AtomicU64::new(0)),
            observing: Arc::new(AtomicBool::new(false)),
            _store_rx: store_rx,
            observer: None,
            observer_watch: None,
            tasks: vec![AbortOnDrop::new(presence_task), AbortOnDrop::new(forever)],
            released: false,
        };

        // A human is fronted on the mesh…
        presence_tx
            .send(PresenceOp::Front(PeerId::human("alice")))
            .expect("the presence worker is alive");
        until("alice to be fronted", || {
            !mesh.fronted.lock().unwrap().is_empty()
        })
        .await;

        // …and the owner goes away without anybody calling shutdown.
        drop(side);

        // The tasks are cancelled rather than detached…
        for (what, ended) in [
            ("the presence worker", &mut presence_ended),
            ("the spawned worker", &mut worker_ended),
        ] {
            let end = tokio::time::timeout(Duration::from_secs(5), ended.recv()).await;
            assert!(
                matches!(end, Ok(None)),
                "{what} outlived the mesh side that owned it"
            );
        }

        // …and the endpoint is released anyway, best-effort, on the way out.
        until("alice to be released", || {
            mesh.released
                .lock()
                .unwrap()
                .iter()
                .any(|p| p == "human:alice")
        })
        .await;
        assert!(
            mesh.fronted.lock().unwrap().is_empty(),
            "a human this gateway fronted was left on $SRV"
        );
    }

    #[tokio::test]
    async fn a_graceful_shutdown_releases_once_and_does_not_release_again_on_drop() {
        let mesh = Arc::new(FlakyMesh::default());
        let (events, _events_rx) = mpsc::unbounded_channel();
        let (presence_tx, presence_rx) = mpsc::unbounded_channel();
        let presence_task = tokio::spawn(presence_worker(mesh.clone(), events, presence_rx));

        let (_store_tx, store_rx) = mpsc::unbounded_channel();
        let (_link, link_rx) = watch::channel(LinkState::connected());
        let side = MeshSide {
            gw: mesh.clone(),
            presence: presence_tx.clone(),
            mesh_up: link_rx,
            ingress_dropped: Arc::new(AtomicU64::new(0)),
            observing: Arc::new(AtomicBool::new(false)),
            _store_rx: store_rx,
            observer: None,
            observer_watch: None,
            tasks: vec![AbortOnDrop::new(presence_task)],
            released: false,
        };

        presence_tx
            .send(PresenceOp::Front(PeerId::human("alice")))
            .expect("the presence worker is alive");
        until("alice to be fronted", || {
            !mesh.fronted.lock().unwrap().is_empty()
        })
        .await;

        side.shutdown().await;
        // The graceful path ran the release through the ordered worker and
        // waited for it; `Drop` must not run a second one behind it.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(*mesh.released.lock().unwrap(), ["human:alice"]);
    }

    // ──────────────────────────── The mesh link ──────────────────────────────

    #[tokio::test]
    async fn a_disconnect_while_no_session_is_up_is_seen_anyway() {
        // The outage starts during an IRC reconnect, when no mirror loop exists
        // to select on the event. The watcher is what makes the flag true of the
        // process rather than of one loop's attention — so the session that
        // registers next starts from it, before it accepts a single line.
        let (events, events_rx) = mpsc::unbounded_channel();
        let (link, mut link_rx) = watch::channel(LinkState::connected());
        let watcher = tokio::spawn(nats_watcher(events_rx, link));

        events.send(ConnectionEvent::Disconnected).unwrap();
        link_rx.changed().await.expect("the watcher is alive");
        assert!(
            !link_rx.borrow().up,
            "the outage was seen with no session up"
        );
        watcher.abort();
    }

    #[tokio::test]
    async fn only_a_real_transition_reaches_the_session() {
        let (events, events_rx) = mpsc::unbounded_channel();
        let (link, mut link_rx) = watch::channel(LinkState::connected());
        let watcher = tokio::spawn(nats_watcher(events_rx, link));

        // A second Connected for a link that never went down must not make a
        // session drain its slot and clear its routing memory.
        events.send(ConnectionEvent::Connected).unwrap();
        events.send(ConnectionEvent::SlowConsumer(1)).unwrap();
        events.send(ConnectionEvent::Disconnected).unwrap();
        link_rx.changed().await.expect("the watcher is alive");
        assert!(!link_rx.borrow().up);
        assert!(
            !link_rx.has_changed().unwrap(),
            "one transition, one wake-up"
        );
        watcher.abort();
    }
}
