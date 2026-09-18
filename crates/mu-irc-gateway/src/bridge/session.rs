//! One IRC connection, and the loop that owns the bridge's state.
//!
//! [`run`] starts the [mesh half](super::mesh_side) once and then runs one
//! [`session`] after another over it, with reconnect backoff in between. A
//! session opens a socket, drives the adapter through registration, and then
//! mirrors both directions from ONE select loop — because both directions
//! mutate the same membership and routing state, and a second state-owning task
//! would need a lock around all of it and would buy nothing. The tasks that do
//! exist are on the other side of this module, exactly where an *await* would
//! otherwise stall the mirror.
//!
//! Every decision here belongs to a module that was reviewed offline — the
//! [`adapter`](crate::adapter) registers, [`membership`](crate::membership)
//! tracks who is in which channel and which humans to front,
//! [`routing`](crate::routing) decides the mesh→IRC output,
//! [`outbound`](crate::outbound) decides the IRC→mesh publishes. This module
//! opens the socket, runs the timers, executes the effects, and decides nothing
//! else.
//!
//! **Nothing is replayed across a reconnect, and nothing is queued for one.** On
//! the IRC side the connection's queues die with it. On the mesh side the
//! subscriptions stay up (dropping them would lose the refusal detection that
//! tells an operator the observer is not watching), but the gate drops every
//! event arriving while no session is registered rather than retaining bodies
//! for an outage's duration. The IRC→mesh direction is the mirror image: its
//! queue is bounded, created per session and cancelled with it, and while NATS
//! is down a typed line is refused to the human's face instead of being held.
//! [`Session`] itself is rebuilt empty every time a connection registers, which
//! is what makes that a property of the structure rather than of remembering to
//! clear things. The one thing this side cannot empty is the mesh client's OWN
//! outbound buffer — `async-nats` re-writes a message it accepted an instant
//! before a drop onto the next connection, and the pinned version has no option
//! to turn that off — so a publish the link did not hold still across is
//! reported by [`mesh::Publish`], counted here, and never counted as delivered.
//! See [`publish_is_uncertain`].
//!
//! **Secret-safe and body-free.** No log line here carries a message body or a
//! credential — not even at `debug`. Bodies exist only inside a
//! [`RouteDecision`] on its way to the socket; diagnostics name classes, counts
//! and peer ids.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use mu_dialogue::mesh::{self, MeshDmEvent, MeshTarget};
use mu_peer::PeerId;

use crate::adapter::{IrcMessage, IsupportSettings, Registration, Step, SystemClock, Transport};
use crate::config::{GatewayConfig, IrcConfig};
use crate::framing::{frame_privmsg, FrameParams};
use crate::mapping::{channel_for, fold_nick};
use crate::membership::{ChannelEffect, ChannelReconciler, HumanEffect, Membership};
use crate::outbound::{
    Answer, CommandReply, MemoryDestination, OutDrop, OutEnv, Outbound, OutboundDecision,
    RefuseReason, NO_AGENTS, USAGE,
};
use crate::puppets::{self, AttemptBudget, FanIn, Pool, PoolAction};
use crate::routing::{RouteDecision, RouteEnv, Router};
use crate::transport::{self, Connection, FromServer, LineWriter, SendError};

use super::mesh_side::{
    discard_stale_discovery, discard_stale_dms, publish_worker, Discovery, LinkState, MeshInputs,
    MeshSide, PresenceOp, PublishJob, PUBLISH_QUEUE,
};
use super::puppet_io::{self, Executor, PuppetCommand, PuppetEvent};

/// How long to wait for the TCP connect and the TLS handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// How long registration (CAP → SASL → welcome) may take before the connection
/// is abandoned. A server that never finishes the handshake is not one the
/// gateway can mirror, and hanging here would hang the whole gateway.
const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(60);

/// Reconnect backoff: the first wait, and the ceiling it doubles up to.
const RECONNECT_MIN: Duration = Duration::from_secs(2);

const RECONNECT_MAX: Duration = Duration::from_secs(300);

/// How long a connection has to have stayed REGISTERED for the next failure to
/// start over at [`RECONNECT_MIN`].
///
/// Without this the backoff only ever grows: a gateway that ran for a week and
/// then lost its connection would wait the accumulated ceiling before trying
/// again, punishing it for uptime. A connection that lasted this long was
/// working, so whatever ended it is a fresh problem.
///
/// **Registered uptime, not attempt time.** Measured from the start of the
/// attempt this would be self-defeating: an attempt includes the connect and
/// the [`REGISTRATION_TIMEOUT`], which is the same 60 seconds, so a server that
/// accepts TCP/TLS and then stalls the handshake would reset the schedule on
/// every single attempt and be hammered at [`RECONNECT_MIN`] forever — the
/// exact failure the doubling exists for. A connection that never registered
/// was never working, whatever the clock says. See [`Backoff`].
const BACKOFF_RESET_AFTER: Duration = Duration::from_secs(60);

/// How long to let a `QUIT` reach the server before the socket is dropped.
const QUIT_GRACE: Duration = Duration::from_secs(3);

/// The quit message. Public by nature, so it names the software and nothing
/// else — no host, no account, no reason.
const QUIT_MESSAGE: &str = "mu-irc-gateway: shutting down";

/// Numerics that mean "the JOIN you sent did not happen": no such channel or
/// nick, and the invite/key/ban/limit/mask/too-many refusals.
const JOIN_REFUSED: [&str; 9] = [
    "403", "405", "471", "473", "474", "475", "476", "477", "479",
];

/// `ERR_USERONCHANNEL` — the JOIN did nothing because the gateway is ALREADY in
/// the channel.
///
/// Deliberately not in [`JOIN_REFUSED`]: the desired state holds, so backing off
/// would be wrong. It is not silence either. A server answering this sends no
/// self-JOIN echo and no NAMES, so the two things a JOIN normally delivers —
/// the reconciler's confirmation and a roster sync — have to be produced here
/// instead, the second by asking for `NAMES` explicitly.
const ALREADY_ON_CHANNEL: &str = "443";

/// Why a connection ended.
enum Stop {
    /// The operator asked the process to stop; the gateway quit cleanly.
    Shutdown,
    /// The connection ended and should be retried, with a body-free reason.
    Reconnect(String),
}

/// Why a connection attempt failed, and whether another one could do better.
///
/// The distinction is not a guess: it is WHERE the failure came from. A
/// configuration the adapter refuses before a byte is framed — SASL over
/// cleartext, a nick that cannot be interpolated safely — is the same refusal on
/// every attempt, so retrying it forever spins a misconfigured gateway instead
/// of telling its operator. Everything a server or a socket did is the opposite:
/// that is what a gateway to a chat network exists to survive.
enum SessionError {
    /// No reconnect can fix it. [`run`] returns it, and the process exits
    /// non-zero for an operator to act on.
    Fatal(anyhow::Error),
    /// Transport or registration trouble. Retried with backoff.
    Retry(anyhow::Error),
}

impl From<SendError> for SessionError {
    /// A write that failed is the socket's problem, which the next connection
    /// does not inherit.
    fn from(e: SendError) -> Self {
        SessionError::Retry(anyhow!("{e}"))
    }
}

/// The reconnect schedule: how long to wait before the next attempt, and the
/// rule for when that starts over.
///
/// A value rather than a bare `Duration` in [`run`] because the rule is the part
/// that was wrong, and a rule that lives in one place can be tested without a
/// server. What resets it is [`BACKOFF_RESET_AFTER`] of REGISTERED uptime and
/// nothing else.
struct Backoff(Duration);

impl Backoff {
    fn new() -> Self {
        Backoff(RECONNECT_MIN)
    }

    /// Account for a finished attempt and hand back the wait before the next
    /// one. `registered_for` is how long the attempt stayed registered, or
    /// `None` if it never got that far — a connect that failed, a handshake the
    /// server stalled, a nick it refused. None of those reset anything.
    fn after(&mut self, registered_for: Option<Duration>) -> Duration {
        if registered_for.is_some_and(|up| up >= BACKOFF_RESET_AFTER) {
            self.0 = RECONNECT_MIN;
        }
        self.0
    }

    /// Grow the schedule, once the wait it handed out has been served.
    fn grow(&mut self) {
        self.0 = (self.0 * 2).min(RECONNECT_MAX);
    }
}

/// Whether a completed publish must be treated as DROPPED rather than delivered.
///
/// Two independent signals, because neither covers the other. `outcome` is the
/// mesh client's own reading of its connection either side of the write
/// ([`mesh::Publish`]); it catches a link that was down when the flush returned,
/// but not a drop AND a reconnect that both landed inside that window, because a
/// state read cannot see a transition it was not present for. The link
/// generation can: [`nats_watcher`](super::mesh_side::nats_watcher) bumps it
/// once per outage, so a generation that moved between the job being accepted
/// and the publish finishing says an outage happened whatever the flag reads
/// now.
///
/// Either one means the same thing here. `async-nats` keeps its outbound buffer
/// across a reconnect, so the message may still be written to the NEXT
/// connection — minutes later, into a conversation that has moved on. This
/// gateway does not get to call that delivered, and does not retry it either: a
/// retry would be the replay the bridge is not. It is counted and reported,
/// body-free, and that is all this side can honestly do.
fn publish_is_uncertain(outcome: mesh::Publish, accepted: u64, now: u64) -> bool {
    outcome != mesh::Publish::Delivered || accepted != now
}

/// Run the gateway until `shutdown` goes true.
///
/// Returns an error only for a failure no reconnect can fix — a mesh that cannot
/// be reached at startup, or a configuration the adapter refuses before a byte
/// is sent (`SessionError::Fatal`). An IRC server that is down is not one of
/// those: it is retried with backoff, because that is what a gateway to a chat
/// network has to survive. The distinction is the adapter's and the transport's,
/// not a guess made here — see `SessionError`.
pub async fn run(config: GatewayConfig, mut shutdown: watch::Receiver<bool>) -> Result<()> {
    let (mesh_side, mut inputs) = MeshSide::start(&config).await?;

    let mut backoff = Backoff::new();
    // A failure no reconnect can fix. Recorded rather than returned on the spot,
    // because the mesh presence this gateway fronts must be released either way.
    let mut fatal: Option<anyhow::Error> = None;
    // The puppet pool's connection-attempt history and the clock it is kept
    // in. Both OUTLIVE every session on purpose: the pool is rebuilt empty on
    // every registration of the main connection, but the server's per-IP
    // throttle window does not reset when `mu-gw` reconnects, so the history
    // is handed to each new pool and taken back when the session ends.
    let mut puppet_budget = AttemptBudget::new();
    let process_started = Instant::now();
    loop {
        if *shutdown.borrow() {
            break;
        }
        // When this attempt registered, if it did. `session` stamps it; an
        // attempt that never got that far leaves it `None`, which is exactly
        // what must not reset the schedule.
        let mut registered_at: Option<Instant> = None;
        let outcome = session(
            &config.irc,
            &mesh_side,
            &mut inputs,
            &mut shutdown,
            &mut registered_at,
            &mut puppet_budget,
            process_started,
        )
        .await;
        // Whatever ended the session — including the `?` paths — the mesh side
        // goes back to dropping at the boundary before the backoff starts.
        inputs.session_live.send_replace(false);
        // A connection that stayed REGISTERED starts the schedule over; a run of
        // failures — including ones that reached the socket and stalled there —
        // keeps doubling.
        let wait = backoff.after(registered_at.map(|since| since.elapsed()));
        match outcome {
            Ok(Stop::Shutdown) => break,
            Ok(Stop::Reconnect(why)) => {
                warn!("IRC connection ended ({why}); reconnecting in {wait:?}")
            }
            Err(SessionError::Retry(e)) => {
                warn!("IRC connection failed ({e:#}); retrying in {wait:?}")
            }
            // Retrying this one forever would spin a misconfigured gateway
            // instead of telling its operator, which is the whole reason `run`
            // has a fallible signature.
            Err(SessionError::Fatal(e)) => {
                fatal = Some(e);
                break;
            }
        }
        tokio::select! {
            _ = shutdown.changed() => {}
            _ = tokio::time::sleep(wait) => {}
        }
        backoff.grow();
    }

    mesh_side.shutdown().await;
    match fatal {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

// ──────────────────────────── One IRC connection ────────────────────────────

// ──────────────────────────── One IRC connection ────────────────────────────

/// The per-connection state. All of it is disposable and rebuilt from scratch
/// every time a connection registers — that is what makes "no replay across a
/// reconnect" a property of the structure rather than of remembering to clear
/// things.
struct Session {
    reg: Registration<SystemClock>,
    membership: Membership,
    router: Router,
    out: Outbound,
    reconciler: ChannelReconciler,
    /// Folded human nick → the folded channel a reply to them belongs in. The
    /// mesh→IRC side reads it; the IRC→mesh side writes it — a directed line in
    /// an agent's channel puts an entry here, and an explicit address typed
    /// anywhere else removes one, which is how a reply falls back to a DM.
    remembered: HashMap<String, String>,
    /// Folded channel → the NAMES generation whose replies are current.
    names_gen: HashMap<String, u64>,
    discovery: Discovery,
    isupport: IsupportSettings,
    /// The gateway's own nick in the server's spelling (the welcome numeric's,
    /// not the configured one, since a server may hand back something else).
    self_nick: String,
    prefix: String,
    lobby: String,
    started: Instant,
    /// This session's publish queue. Bounded and session-scoped: it is created
    /// with the session and dropped with it, so a job typed on this connection
    /// can never fire against a later one.
    publish: mpsc::Sender<PublishJob>,
    /// The mesh link, shared with [`MeshSide`] and kept current by
    /// [`nats_watcher`] in every phase, including the ones before this session
    /// existed. Read for both halves: whether a line can go out at all, and
    /// which link it is going out under.
    mesh_up: watch::Receiver<LinkState>,
    /// Body-free counters, reported when the connection ends.
    dropped_ingress: u64,
    dropped_route: u64,
    dropped_publish: u64,
    /// Publishes this session's worker threw away because the mesh went down
    /// between accepting them and running them. Shared with the worker, which is
    /// where the drop happens.
    dropped_offline: Arc<AtomicU64>,
    /// Publishes that RAN, but whose mesh link did not hold still across them —
    /// so `async-nats` may deliver them on the next connection and this gateway
    /// cannot call them delivered. Shared with the publish closure, which is
    /// where the check happens. See [`publish_is_uncertain`].
    uncertain_publish: Arc<AtomicU64>,
    refused_out: u64,
    /// The puppet pool and its executor — `None` when `[irc.puppets]
    /// enabled = false`, in which case no puppet transport is ever created.
    puppets: Option<Puppetry>,
    /// Process-lifetime clock the pool's timestamps are taken from (its
    /// attempt budget outlives the session; see [`run`]).
    process_started: Instant,
    /// Control lines the puppet side owes the main connection, sent by the
    /// loop right after the puppet event that produced them: the departure
    /// barriers ([`DEPARTURE_PING`]).
    puppet_control: Vec<String>,
}

/// The puppet half of a session: the pool decides, the executor does.
struct Puppetry {
    pool: Pool,
    exec: Executor,
    /// The executor attempt each registered peer's nick is held by: the
    /// CONNECTION id membership files the nick under, so a departure report
    /// (tagged with its attempt) resolves that connection's nick and never a
    /// later connection's under the same spelling. Set at `Registered`,
    /// dropped at `Ended`.
    holder: HashMap<PeerId, u64>,
}

impl Session {
    /// Milliseconds on the process-lifetime clock — the pool's time base.
    fn puppet_now_ms(&self) -> u64 {
        self.process_started.elapsed().as_millis() as u64
    }
    /// Monotonic milliseconds, for the reconciler's backoff schedule.
    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Whether `nick` is this gateway itself, under the live fold rule. The
    /// stored nick is the WIRE spelling, so folding it here is a first fold, not
    /// a lossy re-fold of an already-folded value.
    fn is_self(&self, nick: &str) -> bool {
        fold_nick(nick, self.isupport.casemapping)
            == fold_nick(&self.self_nick, self.isupport.casemapping)
    }
}

/// Ask the adapter for a registration machine, BEFORE a socket exists.
///
/// The order is the contract: `run` documents that a configuration the adapter
/// refuses is refused "before a byte is sent", and that is only true if this
/// happens before the connect. It is also the only failure here that another
/// attempt cannot change — [`Registration::start`] reads the config and nothing
/// else — which is what makes it the fatal one.
fn start_registration(
    irc: &IrcConfig,
) -> Result<(Registration<SystemClock>, Vec<String>), SessionError> {
    Registration::start(irc, SystemClock)
        .map_err(|e| SessionError::Fatal(anyhow!("the adapter refused this configuration: {e}")))
}

/// Open the socket. Everything that can go wrong here belongs to a server, a
/// network or a moment, so it is retried.
async fn open_connection(irc: &IrcConfig) -> Result<Connection, SessionError> {
    transport::connect_with_trust(&irc.server, irc.tls, &irc.tls_trust, CONNECT_TIMEOUT)
        .await
        .map_err(|e| SessionError::Retry(anyhow!("{e:#}")))
}

/// Open one connection, register, and mirror until it ends.
/// `registered_at` is an out-parameter rather than part of the return, because
/// [`run`] needs it on EVERY path including the `?` ones: it is what the
/// reconnect backoff measures, and a connection that registered and then failed
/// has to be told apart from one that never registered at all.
async fn session(
    irc: &IrcConfig,
    mesh_side: &MeshSide,
    inputs: &mut MeshInputs,
    shutdown: &mut watch::Receiver<bool>,
    registered_at: &mut Option<Instant>,
    puppet_budget: &mut AttemptBudget,
    process_started: Instant,
) -> Result<Stop, SessionError> {
    let MeshInputs {
        dm_rx,
        session_live,
        disc_rx,
        kick,
    } = inputs;
    // ── Registration. The adapter decides every line; this drives it. It is
    // built FIRST, so a configuration it refuses costs no connection. ────────
    let (mut reg, first) = start_registration(irc)?;

    info!(
        server = %irc.server,
        tls = irc.tls,
        // What this connection will verify against, so a private-CA setup is
        // legible in the log without reading the config back.
        ca_anchors = irc.tls_trust.extra_anchors(),
        system_roots = irc.tls_trust.system_roots(),
        sasl = irc.sasl.is_some(),
        observing = mesh_side.observing.load(Ordering::Relaxed),
        "connecting to IRC"
    );
    let Connection {
        mut writer,
        mut inbound,
        guard: _guard,
    } = open_connection(irc).await?;

    for line in &first {
        send(&mut writer, line)?;
    }
    let deadline = Instant::now() + REGISTRATION_TIMEOUT;
    let mut self_nick = irc.nick.clone();
    loop {
        let event = tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    quit(&mut writer, &mut inbound).await;
                    return Ok(Stop::Shutdown);
                }
                continue;
            }
            _ = tokio::time::sleep_until(deadline.into()) => {
                // A server that stalls the handshake is a server that might not
                // stall the next one.
                return Err(SessionError::Retry(anyhow!(
                    "registration did not complete within {REGISTRATION_TIMEOUT:?}"
                )));
            }
            event = inbound.recv() => event,
        };
        let line = match event {
            Some(FromServer::Line(line)) => line,
            Some(FromServer::Closed(why)) => return Ok(Stop::Reconnect(why)),
            None => return Ok(Stop::Reconnect("reader ended".into())),
        };
        let msg = IrcMessage::parse(&line);
        // A server may PING mid-handshake, and one that gets no PONG closes the
        // connection before registration can finish.
        if msg.command == "PING" {
            send(&mut writer, &pong(&msg))?;
        }
        // The welcome names the nick the server actually gave us, which is the
        // one every fold and both loop guards must use.
        if msg.command == "001" {
            if let Some(given) = msg.params.first() {
                self_nick.clone_from(given);
            }
        }
        // A registration the SERVER ended — a refused nick, a SASL exchange it
        // rejected, an unexpected line — is a failure of this attempt, not of the
        // configuration: the next connection gets to try again.
        let step = reg
            .on_message(&msg)
            .map_err(|e| SessionError::Retry(anyhow!("registration failed: {e}")))?;
        let ready = step.became_ready;
        apply_step(&mut writer, step)?;
        if ready {
            break;
        }
    }

    // Registered, and only now. This is the instant the backoff schedule
    // measures from: every failure above this line — a refused connect, a
    // handshake the server stalled for the full REGISTRATION_TIMEOUT, a nick it
    // would not give us — leaves the schedule alone and keeps doubling.
    *registered_at = Some(Instant::now());

    let isupport = reg.isupport();
    info!(
        nick = %self_nick,
        casemapping = ?isupport.casemapping,
        channellen = isupport.channellen,
        message_tags = reg.negotiated().message_tags,
        "registered"
    );

    // This session's publish queue and worker. Both die with the session: the
    // worker is aborted below, and dropping the sender ends it even if the abort
    // races a job it has already taken.
    let (publish_tx, publish_rx) = mpsc::channel::<PublishJob>(PUBLISH_QUEUE);
    let publish_gw = mesh_side.gw.clone();
    let publish_link = mesh_side.mesh_up.clone();
    let dropped_offline = Arc::new(AtomicU64::new(0));
    let uncertain_publish = Arc::new(AtomicU64::new(0));
    let publish_uncertain = uncertain_publish.clone();
    let publish_task = tokio::spawn(publish_worker(
        move |job: PublishJob| {
            let gw = publish_gw.clone();
            let link = publish_link.clone();
            let uncertain = publish_uncertain.clone();
            async move {
                // Held across the publish so the link can be compared with what
                // the job was accepted under; both are body-free.
                let (id, from, accepted) = (job.id.clone(), job.from.clone(), job.generation);
                let outcome = gw
                    .publish_dm_fanout(&job.id, &job.from, &job.targets, &job.body, None)
                    .await?;
                let now = link.borrow().generation;
                if publish_is_uncertain(outcome, accepted, now) {
                    uncertain.fetch_add(1, Ordering::Relaxed);
                    warn!(
                        id = %id,
                        from = %from,
                        "the mesh link did not hold across this publish; counted as dropped \
                         rather than delivered, and not retried"
                    );
                }
                Ok(())
            }
        },
        publish_rx,
        mesh_side.mesh_up.clone(),
        dropped_offline.clone(),
    ));

    // The puppet pool, only when enabled: it takes the process-lifetime
    // attempt budget and gives it back at the end of this session. Its
    // executor reports on a bounded queue the loop below drains; the loop
    // never awaits a puppet.
    let (mut puppet_rx, puppets) = if irc.puppets.enabled {
        let (ev_tx, ev_rx) = mpsc::channel::<PuppetEvent>(irc.puppets.event_queue);
        let pool = Pool::with_budget(
            irc.puppets.clone(),
            isupport.nicklen,
            isupport.casemapping,
            std::mem::take(puppet_budget),
        );
        let exec = Executor::new(
            irc.clone(),
            puppet_io::dial_connector(CONNECT_TIMEOUT),
            ev_tx,
            REGISTRATION_TIMEOUT,
        );
        (
            Some(ev_rx),
            Some(Puppetry {
                pool,
                exec,
                holder: HashMap::new(),
            }),
        )
    } else {
        (None, None)
    };

    let mut session = Session {
        puppets,
        process_started,
        puppet_control: Vec::new(),
        membership: Membership::new(&self_nick, isupport.casemapping),
        router: Router::new(mesh_side.gw.issuer()),
        out: Outbound::new(
            &self_nick,
            isupport.casemapping,
            &irc.channel_prefix,
            &irc.lobby,
            isupport.channellen,
        ),
        reconciler: ChannelReconciler::new(
            &irc.channel_prefix,
            &irc.lobby,
            isupport.casemapping,
            isupport.channellen,
        ),
        remembered: HashMap::new(),
        names_gen: HashMap::new(),
        discovery: Discovery::default(),
        isupport,
        self_nick,
        prefix: irc.channel_prefix.clone(),
        lobby: irc.lobby.clone(),
        started: Instant::now(),
        publish: publish_tx,
        mesh_up: mesh_side.mesh_up.clone(),
        dropped_ingress: 0,
        dropped_route: 0,
        dropped_publish: 0,
        dropped_offline,
        uncertain_publish,
        refused_out: 0,
        reg,
    };

    // Connection-aware dropping, mesh side. The gate has been dropping since the
    // last session ended; this reports what it dropped and clears the slot of
    // anything that slipped in as that session was tearing down. Only THEN does
    // the gate start forwarding, so nothing from before this line can arrive.
    let stale = mesh_side.ingress_dropped.swap(0, Ordering::Relaxed) + discard_stale_dms(dm_rx);
    if stale > 0 {
        info!("dropped {stale} mesh event(s) that arrived while IRC was down");
    }
    session_live.send_replace(true);
    // A snapshot collected for a connection that is gone must not drive this
    // one's first reconcile. A fresh connection needs a snapshot now, not at the
    // next refresh, so the stale one is discarded and a sweep is kicked.
    discard_stale_discovery(disc_rx);
    let _ = kick.try_send(());

    // The ownership barrier's first edge: membership learns the (empty) owned
    // set before the first pool decision can be executed, so there is no
    // window in which a puppet JOIN is observed unowned.
    sync_owned_nicks(&mut session, &mesh_side.presence);

    // The mesh link, synchronized BEFORE the loop rather than raced inside it.
    // `nats_watcher` has been tracking it through the connect and the
    // registration, so whatever it says now is current — and reading it here
    // means the loop never has to decide between a transition queued before the
    // session began and a DM that arrived after it. Both branches are no-ops on
    // this session's empty state, which is the point: the reset has already
    // happened, structurally, by being new.
    let mut link_rx = mesh_side.mesh_up.clone();
    apply_mesh_link(
        &mut session,
        &mesh_side.presence,
        dm_rx,
        kick,
        link_rx.borrow_and_update().up,
        LinkApply::AtStart,
    );

    // ── The mirror. ─────────────────────────────────────────────────────────
    let stop = loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    // Puppets first, then the main connection (design:
                    // "the pool is torn down before the main connection").
                    teardown_puppets(&mut session, &mesh_side.presence, puppet_budget).await;
                    quit(&mut writer, &mut inbound).await;
                    break Stop::Shutdown;
                }
            }
            ev = async {
                match puppet_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => match ev {
                Some(ev) => {
                    on_puppet_event(&mut session, &mesh_side.presence, ev);
                    let owed = std::mem::take(&mut session.puppet_control);
                    let mut failed = None;
                    for line in owed {
                        if let Err(e) = send(&mut writer, &line) {
                            failed = Some(e);
                            break;
                        }
                    }
                    if let Some(e) = failed {
                        break Stop::Reconnect(format!("write failed: {e}"));
                    }
                }
                None => {
                    // Every executor sender is gone: only possible after the
                    // pool was torn down, so nothing more will arrive.
                    puppet_rx = None;
                }
            },
            event = inbound.recv() => match event {
                Some(FromServer::Line(line)) => {
                    if let Err(e) = on_irc_line(&mut session, &mesh_side.presence, &mut writer, &line) {
                        break Stop::Reconnect(format!("write failed: {e}"));
                    }
                }
                Some(FromServer::Closed(why)) => break Stop::Reconnect(why),
                None => break Stop::Reconnect("reader ended".into()),
            },
            event = dm_rx.recv() => match event {
                Some(ev) => {
                    if let Err(e) = on_mesh_event(&mut session, &mut writer, ev) {
                        break Stop::Reconnect(format!("write failed: {e}"));
                    }
                }
                None => break Stop::Reconnect("mesh event stream ended".into()),
            },
            changed = disc_rx.changed() => match changed {
                Ok(()) => {
                    session.discovery = disc_rx.borrow_and_update().clone();
                    if let Err(e) = reconcile(&mut session, &mut writer) {
                        break Stop::Reconnect(format!("write failed: {e}"));
                    }
                    // The discovery tick is also the pool's tick: new peers
                    // start aging, departed peers' puppets quit, due peers
                    // connect — under the pool's own pacing and budget.
                    puppets_tick(&mut session, &mesh_side.presence);
                    // The reconcile tick is also the presence retry tick: a
                    // human whose fronting failed is owed another attempt, and
                    // this is the beat the rest of the reconciliation runs on.
                    let _ = mesh_side.presence.send(PresenceOp::Retry);
                }
                Err(_) => break Stop::Reconnect("discovery worker ended".into()),
            },
            changed = link_rx.changed() => match changed {
                Ok(()) => {
                    let up = link_rx.borrow_and_update().up;
                    apply_mesh_link(
                        &mut session,
                        &mesh_side.presence,
                        dm_rx,
                        kick,
                        up,
                        LinkApply::OnTransition,
                    );
                }
                Err(_) => break Stop::Reconnect("mesh connection watch ended".into()),
            },
        }
    };

    // Teardown, in the order that makes "nothing outlives the session" true:
    // stop new events reaching this session, empty the slot of what reached it
    // already, then cancel the publish worker and drop the queue with it, so
    // neither a body nor a job from this connection can fire against the next
    // one. The ingress slot is the MESH half's and outlives this function, which
    // is exactly why it has to be emptied here.
    session_live.send_replace(false);
    let orphaned = discard_stale_dms(dm_rx);
    publish_task.abort();
    // The main connection is gone (or going): every puppet goes with it, and
    // the attempt history goes back to `run` for the next pool. Idempotent
    // with the shutdown branch above, which already did this.
    teardown_puppets(&mut session, &mesh_side.presence, puppet_budget).await;

    info!(
        uptime = ?session.started.elapsed(),
        ingress_refused = session.dropped_ingress,
        ingress_orphaned = orphaned,
        routing_drops = session.dropped_route,
        publish_drops = session.dropped_publish,
        publish_drops_mesh_down = session.dropped_offline.load(Ordering::Relaxed),
        publish_link_uncertain = session.uncertain_publish.load(Ordering::Relaxed),
        outbound_refusals = session.refused_out,
        observer_verify_failures = mesh_side.gw.observer_verify_failures(),
        observing = mesh_side.observing.load(Ordering::Relaxed),
        "IRC session ended"
    );
    // Every human this connection observed is gone as far as IRC is concerned,
    // so every endpoint it fronted for them is released.
    let effects = session.membership.reset();
    apply_human_effects(&mut session, &mesh_side.presence, effects);
    Ok(stop)
}

/// Everything one inbound IRC line can cause.
fn on_irc_line(
    session: &mut Session,
    presence: &mpsc::UnboundedSender<PresenceOp>,
    writer: &mut LineWriter,
    line: &str,
) -> Result<(), SendError> {
    let msg = IrcMessage::parse(line);
    if msg.command == "PING" {
        return send(writer, &pong(&msg));
    }

    // The adapter keeps tracking ISUPPORT and CAP NEW/DEL after registration.
    match session.reg.on_message(&msg) {
        Ok(step) => apply_step(writer, step)?,
        // Post-registration a fault is informational: the connection is up, and
        // a capability change must not alter behaviour with nothing in the log.
        Err(e) => warn!("adapter reported a fault on a live connection: {e}"),
    }
    if session.reg.isupport() != session.isupport {
        on_isupport_change(session, presence, writer)?;
    }

    let nick = prefix_nick(msg.prefix.as_deref());
    match msg.command.as_str() {
        // The server's answer to a departure barrier ([`DEPARTURE_PING`]):
        // the puppet's departure is applied now. Any other PONG is nothing.
        "PONG" => {
            if let Some((attempt, confirmed, departed)) = msg
                .params
                .last()
                .and_then(|token| token.strip_prefix(DEPARTURE_PING))
                .and_then(|rest| {
                    let (attempt, rest) = rest.split_once('/')?;
                    let (confirmed, nick) = rest.split_once('/')?;
                    Some((attempt.parse::<u64>().ok()?, confirmed == "c", nick))
                })
            {
                if confirmed {
                    let effects = session.membership.puppet_departed(departed, attempt);
                    apply_human_effects(session, presence, effects);
                } else {
                    // The server never confirmed the QUIT: the barrier
                    // orders nothing about that puppet, so what arrived
                    // under its name is not replayed.
                    // The accepted residual of this case: the server that
                    // never answered the puppet's QUIT within the grace may
                    // still emit that puppet's JOIN echo after this, and it
                    // will front the gateway's own puppet as a human — until
                    // the same server broadcasts the puppet's QUIT to the same
                    // channel, which withdraws it. The alternative, a name
                    // that stays ours for good on a server that stopped
                    // answering, is worse.
                    warn!(nick = %departed, "puppet: departure unconfirmed by the server (grace); the name is freed, arrivals under it discarded");
                    session
                        .membership
                        .puppet_departed_unconfirmed(departed, attempt);
                }
            }
        }
        "JOIN" => {
            let Some(channel) = msg.params.first().cloned() else {
                return Ok(());
            };
            if session.is_self(&nick) {
                let gen = session.membership.self_joined(&channel);
                session
                    .names_gen
                    .insert(fold_nick(&channel, session.isupport.casemapping), gen);
                session.reconciler.join_confirmed(&channel);
                info!(channel = %channel, "joined");
            } else {
                let account = session.reg.message_account(&msg).map(str::to_string);
                let effects = session.membership.joined(&channel, &nick, account);
                apply_human_effects(session, presence, effects);
            }
        }
        "PART" | "KICK" => {
            // PART: `<channel> [reason]`; KICK: `<channel> <nick> [reason]`.
            let Some(channel) = msg.params.first().cloned() else {
                return Ok(());
            };
            let who = if msg.command == "KICK" {
                msg.params.get(1).cloned().unwrap_or_default()
            } else {
                nick.clone()
            };
            if who.is_empty() {
                return Ok(());
            }
            if session.is_self(&who) {
                session.reconciler.dropped(&channel);
                session
                    .names_gen
                    .remove(&fold_nick(&channel, session.isupport.casemapping));
            }
            let effects = session.membership.left(&channel, &who);
            apply_human_effects(session, presence, effects);
        }
        "QUIT" => {
            let effects = session.membership.quit(&nick);
            apply_human_effects(session, presence, effects);
        }
        "NICK" => {
            let Some(to) = msg.params.first().cloned() else {
                return Ok(());
            };
            if session.is_self(&nick) {
                session.self_nick.clone_from(&to);
                session.out.set_self_nick(&to);
            }
            // A puppet's NICK seen here (a shared channel): the pool learns
            // it now, not only from the puppet's own report, which may still
            // be queued — an ownership sync in between (another peer
            // registering, a tick's release) would otherwise hand membership
            // the pool's stale spelling, re-owning the old name and retiring
            // the live one. Idempotent with the report: the pool's table
            // already has the new spelling then. Only a spelling the pool
            // HOLDS moves; a rename under a retiring name is membership's to
            // hold back, and the pool has already let it go.
            if let Some(p) = session.puppets.as_mut() {
                if let Some(peer) = p.pool.resolve(&nick).cloned() {
                    if let Some(actions) = p.pool.renamed(&peer, &to) {
                        for a in actions {
                            p.exec.execute(a);
                        }
                    }
                }
            }
            let effects = session.membership.renamed(&nick, &to);
            apply_human_effects(session, presence, effects);
        }
        // RPL_NAMREPLY: `<nick> <symbol> <channel> :<names>`.
        "353" => {
            let (Some(channel), Some(names)) = (msg.params.get(2), msg.params.get(3)) else {
                return Ok(());
            };
            let folded = fold_nick(channel, session.isupport.casemapping);
            let Some(gen) = session.names_gen.get(&folded).copied() else {
                return Ok(());
            };
            let nicks: Vec<(String, Option<String>)> = names
                .split_whitespace()
                .map(|n| (n.to_string(), None))
                .collect();
            let channel = channel.clone();
            session.membership.names_reply(&channel, gen, nicks);
        }
        // RPL_ENDOFNAMES: `<nick> <channel> :End of /NAMES list`.
        "366" => {
            let Some(channel) = msg.params.get(1).cloned() else {
                return Ok(());
            };
            let folded = fold_nick(&channel, session.isupport.casemapping);
            let Some(gen) = session.names_gen.get(&folded).copied() else {
                return Ok(());
            };
            let effects = session.membership.names_end(&channel, gen);
            apply_human_effects(session, presence, effects);
        }
        "PRIVMSG" => {
            let (Some(target), Some(text)) = (msg.params.first(), msg.params.get(1)) else {
                return Ok(());
            };
            let (target, text) = (target.clone(), text.clone());
            return on_privmsg(session, writer, &nick, &target, &text);
        }
        ALREADY_ON_CHANNEL => {
            // The desired state already holds, so this CONFIRMS the JOIN rather
            // than refusing it — leaving it pending would strand the channel for
            // the life of the connection. No NAMES follows a JOIN the server
            // ignored, so the roster is asked for by hand.
            if let Some(channel) = channel_param(&msg) {
                session.reconciler.join_confirmed(&channel);
                resync_names(session, writer, &channel)?;
                info!(channel = %channel, "already on channel; asked for its roster");
            }
        }
        numeric if JOIN_REFUSED.contains(&numeric) => {
            // `<nick> <channel> :<reason>`.
            if let Some(channel) = msg.params.get(1) {
                let now = session.now_ms();
                if session.reconciler.join_refused(channel, now) {
                    warn!(channel = %channel, numeric = %numeric, "JOIN refused; backing off");
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// A human's line: the outbound decision, and its execution.
fn on_privmsg(
    session: &mut Session,
    writer: &mut LineWriter,
    sender: &str,
    target: &str,
    text: &str,
) -> Result<(), SendError> {
    // A line read from a connection whose write half has already failed must not
    // be published: the mirror is one-directional at that point, and the sender
    // would get neither an answer nor the refusal explaining why.
    session.out.set_connected(writer.is_connected());
    // Minted BEFORE the decision, because the decision records the id as this
    // gateway's own before anything can publish it and the observer see it come
    // back around.
    let id = mesh::new_dm_id();
    let decision = {
        let env = OutEnv {
            peers: &session.discovery.peers,
            membership: &session.membership,
        };
        session.out.route_line(sender, target, text, &id, &env)
    };
    // Where an operator-facing reply goes: back to the channel it was said in,
    // or privately to whoever said it.
    let reply_to = if session.is_self(target) {
        sender
    } else {
        target
    };
    match decision {
        OutboundDecision::Publish {
            id,
            from,
            targets,
            body,
            memory,
            answer,
        } => {
            if let Some(update) = memory {
                match update.destination {
                    MemoryDestination::Channel(channel) => {
                        session.remembered.insert(update.human, channel);
                    }
                    // An address typed anywhere but the agent's own channel says
                    // the reply belongs in a DM. That is a write: a channel
                    // remembered from an earlier line would otherwise keep
                    // aiming replies at a room this line did not name.
                    MemoryDestination::Private => {
                        session.remembered.remove(&update.human);
                    }
                }
            }
            // A mesh that is down is the same answer as a destination that is
            // gone, and the human gets it now rather than after a queued line is
            // delivered minutes late into a conversation that has moved on.
            let link = *session.mesh_up.borrow();
            let resolved: Vec<MeshTarget> = if link.up {
                targets
                    .iter()
                    .filter_map(|p| session.discovery.target(p))
                    .collect()
            } else {
                Vec::new()
            };
            if resolved.is_empty() {
                // Every destination vanished between the sweep and now. Where
                // that is said is the DECISION's to name: a line said to a room
                // is answered in it, but a `mu say` is a command, and a command
                // answers whoever typed it for every outcome — including this
                // one, or a channel would read the one failure the verb has.
                session.refused_out += 1;
                let failed_to = match answer {
                    Answer::WhereItWasSaid => reply_to,
                    Answer::Sender => sender,
                };
                return notify(
                    writer,
                    failed_to,
                    "no mesh destination is reachable right now",
                );
            }
            debug!(id = %id, from = %from, targets = resolved.len(), "publishing to the mesh");
            enqueue_publish(
                session,
                PublishJob {
                    id,
                    from: from.to_string(),
                    targets: resolved,
                    body,
                    // The link this line was accepted under. The worker drops it
                    // rather than delivering it over a later one.
                    generation: link.generation,
                },
            );
            Ok(())
        }
        // A bot verb: nothing is published, and the answer goes to whoever typed
        // it rather than to the room they typed it in. Every line goes out
        // through the same framing the mirror uses — a roster is not a second,
        // unchecked way onto the wire — and each is gateway-authored, which loop
        // guard 1 is what keeps out of the mesh if one is ever read back.
        OutboundDecision::Reply(reply) => {
            if matches!(reply, CommandReply::Refused(_)) {
                session.refused_out += 1;
            }
            for line in command_lines(&reply) {
                notify(writer, sender, &line)?;
            }
            Ok(())
        }
        OutboundDecision::Refuse(reason) => {
            session.refused_out += 1;
            notify(writer, reply_to, &refusal_text(&reason))
        }
        // Our own echo, or a transport we already know is gone: silent by design.
        OutboundDecision::Drop(OutDrop::OwnNick | OutDrop::Disconnected) => Ok(()),
    }
}

/// One verified mesh DM: this crate's ingress, then routing, then the wire.
fn on_mesh_event(
    session: &mut Session,
    writer: &mut LineWriter,
    ev: MeshDmEvent,
) -> Result<(), SendError> {
    // Loop guard 2: this gateway's own publication coming back around the
    // observer wildcard, or any `human:` sender (a human only reaches the mesh
    // through a gateway like this one).
    if session.out.is_own_echo(&ev) {
        return Ok(());
    }
    // The ingress size policy. Capability verification already ran, in the
    // shared fail-closed gate, inside the subscription that produced this event.
    let ev = match session.router.accept_verified(ev) {
        Ok(ev) => ev,
        Err(why) => {
            session.dropped_ingress += 1;
            debug!("mesh ingress refused an envelope: {why}");
            return Ok(());
        }
    };
    let decision = {
        let env = RouteEnv {
            peers: &session.discovery.peers,
            membership: &session.membership,
            remembered: &session.remembered,
            prefix: &session.prefix,
            lobby: &session.lobby,
            channellen: session.isupport.channellen,
            cm: session.isupport.casemapping,
            message_tags: session.reg.negotiated().message_tags,
        };
        session.router.route(&ev, &env)
    };
    match decision {
        RouteDecision::Deliver { target, lines } => {
            debug!(target = %target, lines = lines.len(), "mirroring a mesh DM to IRC");
            for line in lines {
                send_framed(writer, &line)?;
            }
            Ok(())
        }
        RouteDecision::Notice { target, line } => {
            debug!(target = %target, "sending a body-free notice");
            send_framed(writer, &line)
        }
        RouteDecision::Drop(why) => {
            session.dropped_route += 1;
            debug!("mesh DM not delivered: {why:?}");
            Ok(())
        }
    }
}

/// Reconcile the channels the gateway should be in against the ones it is in.
/// Called on every discovery snapshot, which is also the refresh tick.
fn reconcile(session: &mut Session, writer: &mut LineWriter) -> Result<(), SendError> {
    let now = session.now_ms();
    let effects = session.reconciler.reconcile(&session.discovery.peers, now);
    for (i, effect) in effects.iter().enumerate() {
        match effect {
            ChannelEffect::Join(channel) => {
                // The reconciler marks EVERY emitted channel pending before any
                // of them is executed, and only a confirmation or a refusal
                // clears that mark. A JOIN that never reached the socket gets
                // neither, so the marks come back off here — this one and every
                // JOIN behind it. Otherwise each later reconcile skips those
                // channels and they are never joined again.
                if let Err(e) = send(writer, &format!("JOIN {channel}")) {
                    for unsent in &effects[i..] {
                        if let ChannelEffect::Join(channel) = unsent {
                            session.reconciler.dropped(channel);
                        }
                    }
                    return Err(e);
                }
            }
            ChannelEffect::Part(channel) => {
                session
                    .names_gen
                    .remove(&fold_nick(channel, session.isupport.casemapping));
                send(writer, &format!("PART {channel}"))?;
            }
        }
    }
    Ok(())
}

/// Which of [`apply_mesh_link`]'s two callers is calling. The difference is not
/// cosmetic: it decides whether anything sitting in the DM slot is a leftover or
/// this session's mail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkApply {
    /// Once, before the mirror loop, on a session whose ingress gate has
    /// ALREADY opened.
    AtStart,
    /// A transition observed while the session was live — the link went away
    /// and came back.
    OnTransition,
}

/// Bring the session in line with the mesh link's state. The mesh and IRC fail
/// independently, so this resets only what the mesh side owns.
///
/// Called once before the mirror loop and then on every transition, which is
/// what makes the first application deterministic rather than a race between a
/// queued `Connected` and the first DM of the session.
fn apply_mesh_link(
    session: &mut Session,
    presence: &mpsc::UnboundedSender<PresenceOp>,
    dm_rx: &mut mpsc::Receiver<MeshDmEvent>,
    kick: &mpsc::Sender<()>,
    up: bool,
    apply: LinkApply,
) {
    if !up {
        // What a human types from here on is refused to their face rather than
        // queued for a mesh that may be back in ten minutes.
        warn!("mesh is down; typed lines are refused until it returns");
        return;
    }
    // Leftovers from an earlier link go — but only on a TRANSITION. At start the
    // slot has already been emptied once, before the gate opened, which is the
    // only moment a drain can tell "left over from the last session" from "just
    // arrived for this one". By the time this runs the gate is open, so a DM in
    // the slot was forwarded FOR this session and draining it again would throw
    // away mail that is legitimately ours.
    let dropped = match apply {
        LinkApply::AtStart => 0,
        LinkApply::OnTransition => discard_stale_dms(dm_rx),
    };
    session.router.reset();
    session.out.reset();
    session.remembered.clear();
    let _ = kick.try_send(());
    // Presence does NOT survive a mesh outage by itself: a registration made
    // before it may be gone, and one that FAILED during it was never made. IRC
    // is the authority on who is here, and it still says these humans are — so
    // every one of them is fronted again. An endpoint that is still live answers
    // "already fronted" and costs one round trip; one that is not is exactly the
    // human who would otherwise have no mesh inbox until they left and rejoined.
    let humans = session.membership.present_humans();
    for peer in &humans {
        let _ = presence.send(PresenceOp::Front(peer.clone()));
    }
    info!(
        "mesh connection established; dropped {dropped} stale event(s), routing memory cleared, \
         discovery refreshing, re-fronting {} present human(s)",
        humans.len()
    );
}

/// The server changed `CASEMAPPING` or `CHANNELLEN` under a live connection.
///
/// Both are inputs to how channels are named and how identities fold, so every
/// component that derives from them is updated together.
///
/// **A mapping change is not a reconnect, and must not be executed as one.** The
/// gateway is still in exactly the channels it was in a moment ago. Dropping the
/// channel set and re-JOINing them would be unrecoverable: a JOIN for a channel
/// the client already occupies produces no self-JOIN echo and, on the servers
/// that answer at all, [`ALREADY_ON_CHANNEL`] and no NAMES — so no generation
/// would ever open, every 353/366 would be discarded, and the channels would sit
/// pending with their humans un-fronted until the connection ended.
///
/// So the channel set is KEPT and re-derived under the new rule, the reconciler
/// is rebuilt and immediately told which channels the gateway is still in, and
/// the rosters — the one thing that genuinely cannot be re-derived, because a
/// fold is lossy — are asked for explicitly with `NAMES`, under a fresh
/// generation per channel so the replies are accepted without a JOIN.
fn on_isupport_change(
    session: &mut Session,
    presence: &mpsc::UnboundedSender<PresenceOp>,
    writer: &mut LineWriter,
) -> Result<(), SendError> {
    let now = session.reg.isupport();
    let cm_changed = now.casemapping != session.isupport.casemapping;
    session.isupport = now;
    session.out.set_channellen(now.channellen);
    // NICKLEN arrives in the 005 AFTER the 001 the pool was built on; every
    // offer from here sizes to what the server actually advertised.
    if let Some(p) = session.puppets.as_mut() {
        p.pool.set_nicklen(now.nicklen);
    }
    if cm_changed {
        warn!(
            casemapping = ?now.casemapping,
            "server changed CASEMAPPING; re-folding membership and re-syncing NAMES"
        );
        session.out.set_casemapping(now.casemapping, &session.lobby);
        // Both keys of the routing memory — human and channel — were folded
        // under the old rule. It is disposable by design, so it goes rather than
        // being re-derived, and the effects below run against an empty one.
        session.remembered.clear();
        let effects = session.membership.set_casemapping(now.casemapping);
        apply_human_effects(session, presence, effects);
        session.names_gen.clear();
        // The pool's nick table re-derives from wire spellings; a puppet whose
        // nick now folds onto another's is quit and re-offered the tail.
        if let Some(p) = session.puppets.as_mut() {
            let actions = p.pool.set_casemapping(now.casemapping);
            for a in actions {
                p.exec.execute(a);
            }
        }
        sync_owned_nicks(session, presence);
    }
    // Channel names are derived under both values, so the reconciler starts over
    // and the next reconcile re-derives every channel from current discovery.
    session.reconciler = ChannelReconciler::new(
        &session.prefix,
        &session.lobby,
        now.casemapping,
        now.channellen,
    );
    // …but "start over" is about the DERIVATION, not about where the gateway is.
    // Sorted so the lines a connection emits are reproducible.
    let mut joined: Vec<String> = session
        .membership
        .joined_channels()
        .iter()
        .filter_map(|folded| {
            session
                .membership
                .channel_display(folded)
                .map(str::to_string)
        })
        .collect();
    joined.sort();
    for channel in joined {
        session.reconciler.join_confirmed(&channel);
        if cm_changed {
            resync_names(session, writer, &channel)?;
        }
    }
    reconcile(session, writer)
}

/// Ask for a channel's roster without a JOIN.
///
/// `NAMES` is the only way to get one for a channel the gateway is already in,
/// and the generation has to be opened here because [`Membership::self_joined`]
/// is what mints it — the 353/366 handlers discard any reply they cannot match
/// to an open generation. Idempotent: a channel already syncing is left alone,
/// so a duplicate `ALREADY_ON_CHANNEL` does not restart a burst mid-flight.
fn resync_names(
    session: &mut Session,
    writer: &mut LineWriter,
    channel: &str,
) -> Result<(), SendError> {
    let folded = fold_nick(channel, session.isupport.casemapping);
    if session.names_gen.contains_key(&folded) {
        return Ok(());
    }
    let gen = session.membership.self_joined(channel);
    session.names_gen.insert(folded, gen);
    send(writer, &format!("NAMES {channel}"))
}

/// Execute the membership module's human-presence effects: the mesh endpoints to
/// front and release, and the routing memory that moves with a rename.
fn apply_human_effects(
    session: &mut Session,
    presence: &mpsc::UnboundedSender<PresenceOp>,
    effects: Vec<HumanEffect>,
) {
    for effect in effects {
        match &effect {
            HumanEffect::Register(peer) => {
                if let Some(nick) = peer.human_nick() {
                    // Their return ends the withdrawal a notice was suppressed
                    // for, so the next one is diagnosed afresh.
                    session
                        .router
                        .human_returned(nick, session.isupport.casemapping);
                }
                let _ = presence.send(PresenceOp::Front(peer.clone()));
            }
            HumanEffect::Withdraw(peer) => {
                if let Some(nick) = peer.human_nick() {
                    // Nobody to remember a channel for; membership is the only
                    // authority on whether they are here.
                    session.remembered.remove(nick);
                }
                let _ = presence.send(PresenceOp::Release(peer.clone()));
            }
            HumanEffect::Rename { from, to } => {
                if let (Some(old), Some(new)) = (from.human_nick(), to.human_nick()) {
                    if let Some(channel) = session.remembered.remove(old) {
                        session.remembered.insert(new.to_string(), channel);
                    }
                    session
                        .router
                        .human_returned(new, session.isupport.casemapping);
                }
                // Ordered on one worker: the old endpoint is gone before the new
                // one exists, so a rename never leaves two.
                let _ = presence.send(PresenceOp::Release(from.clone()));
                let _ = presence.send(PresenceOp::Front(to.clone()));
            }
        }
    }
}

/// The operator-facing text for a refusal. Names peers and channels — never a
/// body, which is what makes it safe to put back on IRC.
fn refusal_text(reason: &RefuseReason) -> String {
    match reason {
        RefuseReason::HumanDestination => {
            "that address is a human, not an agent — humans are not mesh destinations".into()
        }
        RefuseReason::AbsentDestination(peer) => format!("{peer} is not on the mesh right now"),
        RefuseReason::AmbiguousChannel(peers) => {
            let names: Vec<String> = peers.iter().map(PeerId::to_string).collect();
            format!(
                "this channel maps to more than one peer ({}) — address one explicitly, \
                 e.g. `{}: your message`",
                names.join(", "),
                names.first().map(String::as_str).unwrap_or("cc:id")
            )
        }
        RefuseReason::UnknownChannel(channel) => {
            format!("{channel} maps to no agent on the mesh right now")
        }
        RefuseReason::UnauthorizedSender(nick) => format!(
            "{nick} is not in any channel this gateway is in, so it cannot be vouched for \
             on the mesh — join a channel the gateway is in first"
        ),
        RefuseReason::AmbiguousPeer(peers) => {
            let names: Vec<String> = peers.iter().map(PeerId::to_string).collect();
            format!(
                "that name matches more than one peer ({}) — say one of them by its full id, \
                 e.g. `mu say {} your message`",
                names.join(", "),
                names.first().map(String::as_str).unwrap_or("cc:id")
            )
        }
        RefuseReason::UnknownPeer(dest) => {
            format!("{dest} names no agent on the mesh right now — `mu peers` lists them")
        }
        RefuseReason::NoDestinations => NO_AGENTS.into(),
    }
}

/// The operator-facing lines of one bot-verb answer. The roster arrives
/// already rendered (the mapping rules that produced it are the outbound side's,
/// not this module's); a refusal and a usage line are one line each.
fn command_lines(reply: &CommandReply) -> Vec<String> {
    match reply {
        CommandReply::Peers(lines) => lines.clone(),
        CommandReply::Refused(reason) => vec![refusal_text(reason)],
        CommandReply::Usage => vec![USAGE.to_string()],
    }
}

/// Send one operator-facing line, framed by the same module every other line
/// goes through — a diagnostic is not a second, unchecked way onto the wire.
fn notify(writer: &mut LineWriter, target: &str, text: &str) -> Result<(), SendError> {
    let params = FrameParams {
        target,
        mesh_id: None,
        message_tags: false,
    };
    match frame_privmsg(&params, text) {
        Ok(lines) => {
            for line in lines {
                send_framed(writer, &line)?;
            }
            Ok(())
        }
        Err(e) => {
            debug!("could not frame a diagnostic for {target}: {e}");
            Ok(())
        }
    }
}

/// Send a line the framing module already terminated. The transport owns CRLF,
/// so the framed terminator is trimmed rather than doubled — the byte budget is
/// identical either way.
fn send_framed(writer: &mut LineWriter, framed: &str) -> Result<(), SendError> {
    send_mirror(writer, framed.trim_end_matches(['\r', '\n']))
}

/// Run one adapter step: its lines, then its diagnostic.
fn apply_step(writer: &mut LineWriter, step: Step) -> Result<(), SendError> {
    for line in &step.out {
        send(writer, line)?;
    }
    if let Some(diagnostic) = step.diagnostic {
        info!("{diagnostic}");
    }
    Ok(())
}

/// Hand one CONTROL-PLANE line to the connection: registration, PONG, JOIN,
/// PART, NAMES — anything the gateway's own state machines depend on having
/// reached the server.
///
/// Every failure, overflow included, ends the session. A full outbound queue
/// means the socket is not draining, so a control line "dropped best-effort"
/// is not best-effort at all: the reconciler has already recorded a JOIN as
/// pending, the server never sees it, and the channel is stranded for the life
/// of a connection that is already stuck. Reconnecting rebuilds all of it.
/// NEITHER path logs the line: one of them is the SASL response.
fn send(writer: &mut LineWriter, line: &str) -> Result<(), SendError> {
    writer.send_line(line)
}

/// Hand one MIRRORED line to the connection: a message body on its way to IRC,
/// or a body-free diagnostic.
///
/// This is the direction that is genuinely best-effort — a full queue drops the
/// line and keeps going, because the alternative is unbounded memory for a
/// server that stopped reading, and a mirror that missed one line is still a
/// mirror. A closed connection is still returned, because that ends the session.
fn send_mirror(writer: &mut LineWriter, line: &str) -> Result<(), SendError> {
    match writer.send_line(line) {
        Ok(()) => Ok(()),
        Err(SendError::Overflow) => {
            warn!("outbound queue full; dropped one mirrored line (a live mirror, not a queue)");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Answer a `PING` with its own token.
fn pong(msg: &IrcMessage) -> String {
    match msg.params.last() {
        Some(token) => format!("PONG :{token}"),
        None => "PONG".to_string(),
    }
}

/// Hand one decided publish to this session's worker.
///
/// A full queue DROPS the job and counts it — the same best-effort rule the
/// mirrored direction follows, for the same reason: the alternative is holding a
/// human's line until a mesh that may be gone comes back, which is the replay
/// this gateway is not. `Closed` means the session's worker is already gone,
/// which is the same outcome. Returns whether the job was queued.
fn enqueue_publish(session: &mut Session, job: PublishJob) -> bool {
    match session.publish.try_send(job) {
        Ok(()) => true,
        Err(e) => {
            session.dropped_publish += 1;
            // Names the class and nothing else: the job carries a body.
            let full = matches!(e, mpsc::error::TrySendError::Full(_));
            warn!(
                queue_full = full,
                "publish queue would not take a line; dropped it (a live mirror, not a queue)"
            );
            false
        }
    }
}

/// The channel named by a numeric whose parameter layout varies between servers
/// (`<client> <channel>` and `<client> <nick> <channel>` are both in the wild
/// for [`ALREADY_ON_CHANNEL`]): the first parameter after the client's own nick
/// that is spelled like a channel.
fn channel_param(msg: &IrcMessage) -> Option<String> {
    msg.params
        .iter()
        .skip(1)
        .find(|p| p.starts_with(['#', '&', '!', '+']))
        .cloned()
}

/// The nick out of a `nick!user@host` prefix (or a server name, which has
/// neither and is returned whole — it never matches a tracked nick).
fn prefix_nick(prefix: Option<&str>) -> String {
    let Some(prefix) = prefix else {
        return String::new();
    };
    prefix
        .split('!')
        .next()
        .unwrap_or(prefix)
        .split('@')
        .next()
        .unwrap_or(prefix)
        .to_string()
}

// ────────────────────────────── Puppets (2b-i) ──────────────────────────────

/// The token of the PING the session sends on the main connection when a
/// puppet's own connection is over, followed by `<attempt>/<c|u>/<nick>`:
/// `c` when the server confirmed the departure (it closed the connection
/// after the QUIT, or dropped it itself), `u` when the grace cut a QUIT the
/// server never answered — then the barrier orders nothing about that
/// puppet and membership frees the name without replaying what arrived
/// under it (`Membership::puppet_departed_unconfirmed`; a late echo of that
/// puppet's can then front it as a human until its QUIT withdraws it — the
/// residual accepted over a name held for good). The server's PONG is
/// the ORDERING BARRIER the departure report is applied at: everything the
/// server wrote to the main connection before it read the PING has arrived
/// here by then — the puppet's own JOIN echo to a shared channel, written
/// when the server processed the JOIN, before it processed the QUIT, before
/// it read this PING. So the report never resolves the nick with such an
/// echo still on its way, which would have fronted the gateway's own puppet
/// as a human (panel finding, PR #662).
///
/// That is all the barrier orders. It does NOT make every JOIN before the
/// PONG the puppet's: once the server has processed the QUIT the name is
/// free, and a human can take it and JOIN before the server reads the PING.
/// Membership settles which it was by how the retiring entry resolves — an
/// observed QUIT (the puppet was in a shared channel; what came before was
/// the puppet) discards what arrived under the name, the report (no shared
/// channel, so nothing of the puppet's could have come) replays it as the
/// name's new holder's (`Membership::puppet_departed`).
const DEPARTURE_PING: &str = "mu-gw-departed/";

/// Hand membership the current owned-nick set (wire spellings, from the
/// pool) and execute the human effects that produces. Called before the first
/// pool action and after every ownership transition — the ownership barrier.
fn sync_owned_nicks(session: &mut Session, presence: &mpsc::UnboundedSender<PresenceOp>) {
    let owned: Vec<(String, u64)> = match session.puppets.as_ref() {
        Some(p) => p
            .pool
            .owned_nicks()
            .into_iter()
            .map(|nick| {
                let connection = p
                    .pool
                    .resolve(&nick)
                    .and_then(|peer| p.holder.get(peer))
                    .copied()
                    .unwrap_or(0);
                (nick, connection)
            })
            .collect(),
        None => Vec::new(),
    };
    let effects = session.membership.set_owned(owned);
    apply_human_effects(session, presence, effects);
}

/// Feed the discovery snapshot to the pool and execute what it decides.
fn puppets_tick(session: &mut Session, presence: &mpsc::UnboundedSender<PresenceOp>) {
    let now = session.puppet_now_ms();
    let peers = session.discovery.peers.clone();
    let Some(p) = session.puppets.as_mut() else {
        return;
    };
    let mut actions = p.pool.observe(&peers, now);
    actions.extend(p.pool.tick(now));
    let released = actions
        .iter()
        .any(|a| matches!(a, PoolAction::Quit { .. } | PoolAction::Cancel { .. }));
    for a in actions {
        p.exec.execute(a);
    }
    if released {
        sync_owned_nicks(session, presence);
    }
}

/// One event from a puppet task. Stale events (from an attempt the executor
/// no longer tracks) are counted and ignored.
fn on_puppet_event(
    session: &mut Session,
    presence: &mpsc::UnboundedSender<PresenceOp>,
    ev: PuppetEvent,
) {
    let now = session.puppet_now_ms();
    let cm = session.isupport.casemapping;
    let (prefix, lobby, channellen) = (
        session.prefix.clone(),
        session.lobby.clone(),
        session.isupport.channellen,
    );
    let Some(p) = session.puppets.as_mut() else {
        return;
    };
    if !p.exec.is_current(&ev) {
        return;
    }
    match ev {
        PuppetEvent::Registered {
            peer,
            attempt,
            nick,
        } => {
            p.holder.insert(peer.clone(), attempt);
            let actions = p.pool.registered(&peer, &nick, now);
            let held = p.pool.nick_of(&peer).is_some();
            for a in actions {
                p.exec.execute(a);
            }
            // Ownership BEFORE the JOIN: membership knows the nick is ours
            // before the server can echo its JOIN back to `mu-gw`.
            sync_owned_nicks(session, presence);
            if held {
                let Some(p) = session.puppets.as_mut() else {
                    return;
                };
                let mut channels = vec![lobby];
                if let Some(own) = channel_for(&peer, &prefix, channellen) {
                    channels.push(own);
                }
                info!(peer = %peer, nick = %nick, "puppet: registered");
                if !p.exec.command(&peer, PuppetCommand::Join(channels)) {
                    warn!(peer = %peer, nick = %nick, "puppet: JOIN not queued (task stalled)");
                }
            }
        }
        PuppetEvent::NickRejected { peer, numeric, .. } => {
            info!(peer = %peer, numeric = %numeric, "puppet: nick rejected at registration");
            let actions = p.pool.nick_rejected(&peer, &numeric, now);
            for a in actions {
                p.exec.execute(a);
            }
        }
        PuppetEvent::Ended {
            peer,
            attempt,
            nick,
            why,
            confirmed,
        } => {
            // Two kinds of end. A LIVE attempt's end is a disconnect the pool
            // has not decided: it releases the nick and schedules the retry.
            // A QUITTING attempt's end is the completion of a Quit the pool
            // already issued — its peer may by now be on a replacement
            // attempt (a casemapping change queues the Quit and the next
            // tick reconnects), and that attempt's state must not be
            // touched by the old connection's last word.
            let live = p.exec.is_live(&peer, attempt);
            let was_registered = live && p.pool.nick_of(&peer).is_some();
            p.exec.forget(&peer, attempt);
            if p.holder.get(&peer) == Some(&attempt) {
                p.holder.remove(&peer);
            }
            if live {
                let actions = p.pool.disconnected(&peer, now);
                for a in actions {
                    p.exec.execute(a);
                }
            }
            debug!(peer = %peer, why = %why, live, registered = was_registered, "puppet: connection ended");
            // Release first, then report — at a barrier. The puppet's own
            // connection is over: after a QUIT, the server closed it (or the
            // grace ran out); otherwise it dropped. The ownership sync moves
            // a nick the pool just released into membership's retiring set
            // under this attempt; the departure report resolves it — the
            // ordered resolution for a puppet released before it joined
            // anything, whose QUIT the main connection never sees. But the
            // main connection may still hold that puppet's JOIN echo to a
            // shared channel, written by the server before it closed the
            // puppet's connection and not yet drained here, so the report is
            // applied only when the server answers a PING sent now
            // ([`DEPARTURE_PING`]): the server wrote that echo before it read
            // this PING, so by the PONG it has been read here. For a
            // QUITTING attempt the release came with the Quit. Membership
            // resolves only this attempt's retiring entry: a nick held or
            // retiring under a later connection is that connection's.
            if was_registered {
                sync_owned_nicks(session, presence);
            }
            if let Some(nick) = nick {
                // What the report may order on: the executor says whether
                // the SERVER ended the connection (`c`) — it closed after the
                // QUIT, or dropped the puppet itself — or this side did
                // (`u`: the grace cut it, a write or read failed locally),
                // which says nothing about what the server has processed.
                let confirmed = if confirmed { 'c' } else { 'u' };
                session.puppet_control.push(format!(
                    "PING :{DEPARTURE_PING}{attempt}/{confirmed}/{nick}"
                ));
            }
        }
        PuppetEvent::Renamed {
            peer,
            attempt,
            from,
            to,
        } => {
            // A NICK for the puppet itself (a server can force one). Membership
            // first: the spelling it holds for this puppet — owned, or retiring
            // if the pool already released it — moves with the rename, so the
            // ownership sync below sees a nick that moved, not one released
            // and another registered, and a departure reported under the new
            // spelling resolves it. The main connection sees the same NICK
            // only when it shares a channel with the puppet; a rename in the
            // registered→JOIN gap is seen here alone.
            // Membership moves the spelling only while it still files `from`
            // under THIS connection: the main connection's NICK handler may
            // have moved it already (the puppet shares a channel), and a
            // human may have taken the old spelling since — not this
            // puppet's to move.
            if !session.membership.puppet_renamed(&from, &to, attempt) {
                debug!(peer = %peer, from = %from, to = %to, "puppet: rename report for a spelling membership no longer files under it");
            }
            let Some(p) = session.puppets.as_mut() else {
                return;
            };
            if !p.exec.is_live(&peer, attempt) {
                // A quitting attempt: the pool has let this peer go (and may
                // be on a replacement attempt whose nick this is not). The
                // retiring entry followed the spelling; its Ended resolves it.
                debug!(peer = %peer, from = %from, to = %to, "puppet: renamed while quitting");
                return;
            }
            // The pool is the authority on spellings and learns the change
            // from the puppet's own connection; the main connection's NICK
            // handler only updates membership. A rename onto a nick another
            // puppet holds is a collision like one at registration: the pool
            // answers with Quit + ChannelOnly.
            match p.pool.renamed(&peer, &to) {
                Some(actions) => {
                    info!(peer = %peer, from = %from, to = %to, collided = !actions.is_empty(), "puppet: renamed by the server");
                    for a in actions {
                        p.exec.execute(a);
                    }
                }
                None => {
                    debug!(peer = %peer, from = %from, to = %to, "puppet: rename for an unregistered peer ignored")
                }
            }
            sync_owned_nicks(session, presence);
        }
        PuppetEvent::Line { peer, line, .. } => {
            let msg = IrcMessage::parse(&line);
            let own = p.pool.nick_of(&peer).unwrap_or("").to_string();
            // 2b-i: classified for the count, routed nowhere. The private-class
            // tag (`via: Some(peer)`) is what increment 2b-ii routes on.
            let verdict = puppets::classify(puppets::Source::Puppet(&peer), &msg, &own, cm);
            match verdict {
                FanIn::Route { via: Some(_), .. } => {
                    debug!(peer = %peer, "puppet: private line held (routing lands in 2b-ii)");
                }
                FanIn::Route { via: None, .. } | FanIn::Drop(_) => {}
            }
            p.exec.count_dropped_line();
        }
    }
}

/// Tear the pool down as one bounded step: every registered puppet is asked to
/// QUIT (each task bounds its own grace and force-closes), everything in
/// flight or backing off is cancelled, then the executor aborts whatever is
/// left and the attempt history goes back to `run`. Idempotent.
async fn teardown_puppets(
    session: &mut Session,
    presence: &mpsc::UnboundedSender<PresenceOp>,
    puppet_budget: &mut AttemptBudget,
) {
    let Some(mut p) = session.puppets.take() else {
        return;
    };
    let registered = p.pool.registered_count();
    let actions = p.pool.teardown();
    for a in actions {
        p.exec.execute(a);
    }
    // One deadline for every QUIT in flight — the configured grace, once for
    // the whole pool, and only as long as the slowest puppet actually takes.
    let grace = Duration::from_secs(p.pool.config().quit_grace_secs);
    p.exec
        .join_quitting(tokio::time::Instant::now() + grace + Duration::from_secs(1))
        .await;
    p.exec.abort_all();
    let stats = p.exec.stats().clone();
    info!(
        puppets_registered = registered,
        stale_events = stats.stale_events,
        commands_dropped = stats.commands_dropped,
        puppet_lines_dropped = stats.lines_dropped,
        "puppet pool torn down"
    );
    *puppet_budget = p.pool.into_budget();
    sync_owned_nicks(session, presence);
}

/// Say goodbye properly: a `QUIT`, then wait — briefly — for the server to close
/// the connection, so the message is not truncated by dropping the socket.
async fn quit(writer: &mut LineWriter, inbound: &mut mpsc::Receiver<FromServer>) {
    let _ = send(writer, &format!("QUIT :{QUIT_MESSAGE}"));
    let grace = tokio::time::sleep(QUIT_GRACE);
    tokio::pin!(grace);
    loop {
        tokio::select! {
            _ = &mut grace => break,
            event = inbound.recv() => match event {
                Some(FromServer::Line(_)) => continue,
                _ => break,
            },
        }
    }
    info!("sent QUIT and closed the IRC connection");
}

#[cfg(test)]
mod tests {
    use super::*;

    use biscuit_auth::KeyPair;
    use mu_dialogue::mesh::{ConnectionEvent, Reception};

    use crate::adapter::DEFAULT_CHANNELLEN;
    use crate::bridge::mesh_side::nats_watcher;
    use crate::config::{SaslCreds, Secret};
    use crate::mapping::{channel_for, CaseMapping};
    use crate::transport::TlsTrust;

    const LOBBY: &str = "#mu";

    /// The IRC side of a session that never opens a socket.
    fn irc_config() -> IrcConfig {
        IrcConfig {
            server: "irc.invalid:6667".into(),
            tls: false,
            tls_trust: TlsTrust::default(),
            nick: "mu-gw".into(),
            sasl: None,
            channel_prefix: "#".into(),
            lobby: LOBBY.into(),
            observe_agent_dms: true,
            puppets: Default::default(),
        }
    }

    /// A session wired to nothing, and the two ends a test drives it from.
    struct Scripted {
        session: Session,
        jobs: mpsc::Receiver<PublishJob>,
        /// The mesh link the real [`nats_watcher`] would own.
        link: watch::Sender<LinkState>,
    }

    /// A session wired to nothing.
    ///
    /// Every field of [`Session`] is offline-constructible — that is what makes
    /// the handlers testable without a server or a mesh — so these tests drive
    /// the REAL `on_irc_line` / `on_privmsg` / `reconcile` rather than a model of
    /// them. `publish_capacity` is the caller's so a full queue is reachable.
    fn scripted_session(publish_capacity: usize) -> Scripted {
        let irc = irc_config();
        let (reg, _first) = Registration::start(&irc, SystemClock).expect("offline registration");
        let isupport = reg.isupport();
        let (publish, jobs) = mpsc::channel(publish_capacity);
        let (link, mesh_up) = watch::channel(LinkState::connected());
        let session = Session {
            membership: Membership::new(&irc.nick, isupport.casemapping),
            router: Router::new(KeyPair::new().public()),
            out: Outbound::new(
                &irc.nick,
                isupport.casemapping,
                &irc.channel_prefix,
                &irc.lobby,
                isupport.channellen,
            ),
            reconciler: ChannelReconciler::new(
                &irc.channel_prefix,
                &irc.lobby,
                isupport.casemapping,
                isupport.channellen,
            ),
            remembered: HashMap::new(),
            names_gen: HashMap::new(),
            discovery: Discovery::default(),
            isupport,
            self_nick: irc.nick.clone(),
            prefix: irc.channel_prefix.clone(),
            lobby: irc.lobby.clone(),
            started: Instant::now(),
            publish,
            mesh_up,
            dropped_ingress: 0,
            dropped_route: 0,
            dropped_publish: 0,
            dropped_offline: Arc::new(AtomicU64::new(0)),
            uncertain_publish: Arc::new(AtomicU64::new(0)),
            refused_out: 0,
            puppets: None,
            process_started: Instant::now(),
            puppet_control: Vec::new(),
            reg,
        };
        Scripted {
            session,
            jobs,
            link,
        }
    }

    /// One discovered agent, and the channel it maps to.
    fn one_agent(session: &mut Session) -> String {
        let peer = PeerId::parse("cc:abc");
        session.discovery = Discovery::from_srv(HashMap::from([(
            "cc:abc".to_string(),
            "mu.agent.cc.abc.dm".to_string(),
        )]));
        channel_for(&peer, "#", DEFAULT_CHANNELLEN).expect("an agent maps to a channel")
    }

    /// Record the gateway as joined to `channel` with `nick` in it, exactly as a
    /// self-JOIN echo followed by a NAMES burst would.
    fn already_in(session: &mut Session, channel: &str, nick: &str) {
        let gen = session.membership.self_joined(channel);
        session
            .names_gen
            .insert(fold_nick(channel, session.isupport.casemapping), gen);
        session.reconciler.join_confirmed(channel);
        session
            .membership
            .names_reply(channel, gen, [(nick.to_string(), None)]);
        session.membership.names_end(channel, gen);
    }

    /// Everything the scripted connection has been handed so far.
    fn written(lines: &mut mpsc::Receiver<String>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(line) = lines.try_recv() {
            out.push(line);
        }
        out
    }

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

    fn job(id: &str) -> PublishJob {
        PublishJob {
            id: id.into(),
            from: "human:alice".into(),
            targets: vec![MeshTarget {
                subject: "mu.agent.cc.abc.dm".into(),
                session: None,
            }],
            body: "b".into(),
            generation: 0,
        }
    }

    #[test]
    fn a_prefix_yields_the_nick_alone() {
        assert_eq!(prefix_nick(Some("alice!~a@example.org")), "alice");
        assert_eq!(prefix_nick(Some("alice@example.org")), "alice");
        assert_eq!(prefix_nick(Some("alice")), "alice");
        assert_eq!(prefix_nick(Some("irc.example.org")), "irc.example.org");
        assert_eq!(prefix_nick(None), "");
    }

    #[test]
    fn a_ping_is_answered_with_its_own_token() {
        assert_eq!(pong(&IrcMessage::parse("PING :abc123")), "PONG :abc123");
        assert_eq!(pong(&IrcMessage::parse("PING abc123")), "PONG :abc123");
        // A server that sends a bare PING gets a bare PONG rather than one
        // naming an empty token.
        assert_eq!(pong(&IrcMessage::parse("PING")), "PONG");
    }

    #[test]
    fn a_framed_line_is_not_terminated_twice() {
        // `frame_privmsg` returns complete lines; the transport adds CRLF. The
        // trim is what keeps the two from disagreeing about the 512-byte budget.
        let framed = "PRIVMSG #mu :hello\r\n";
        assert_eq!(framed.trim_end_matches(['\r', '\n']), "PRIVMSG #mu :hello");
    }

    // ───────────── A live CASEMAPPING change (recovery, not a reset) ─────────

    // ───────────── A live CASEMAPPING change (recovery, not a reset) ─────────

    #[test]
    fn a_casemapping_change_asks_for_a_fresh_roster_instead_of_rejoining() {
        // The gateway is in two channels with a human in each. The server then
        // changes CASEMAPPING under the live connection.
        //
        // Re-JOINing here is the trap: the client is already in both, so no
        // self-JOIN echo comes back, no NAMES follows, no generation ever opens,
        // every 353/366 is discarded, and both channels sit pending with their
        // humans un-fronted until the connection ends.
        let Scripted {
            mut session,
            jobs: _jobs,
            link: _link,
        } = scripted_session(PUBLISH_QUEUE);
        let agent = one_agent(&mut session);
        already_in(&mut session, LOBBY, "alice");
        already_in(&mut session, &agent, "bob");
        let (presence, mut fronted) = mpsc::unbounded_channel();
        let (mut writer, mut lines) = transport::LineWriter::scripted(64);
        let _ = written(&mut lines);
        while fronted.try_recv().is_ok() {}

        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":srv 005 mu-gw CASEMAPPING=ascii :are supported by this server",
        )
        .expect("a mapping change is not a write failure");

        assert_eq!(session.isupport.casemapping, CaseMapping::Ascii);
        let sent = written(&mut lines);
        assert!(
            sent.contains(&format!("NAMES {LOBBY}")) && sent.contains(&format!("NAMES {agent}")),
            "a roster has to be asked for explicitly: {sent:?}"
        );
        assert!(
            !sent.iter().any(|l| l.starts_with("JOIN ")),
            "the gateway is already in both channels: {sent:?}"
        );
        assert!(
            !sent.iter().any(|l| l.starts_with("PART ")),
            "a mapping change does not move the gateway: {sent:?}"
        );
        assert!(
            session.reconciler.is_joined(LOBBY) && session.reconciler.is_joined(&agent),
            "neither channel may be left pending"
        );
        assert_eq!(session.names_gen.len(), 2, "a generation per kept channel");

        // …and the replies that follow are accepted, WITHOUT a self-JOIN echo.
        for line in [
            format!(":srv 353 mu-gw = {agent} :bob carol"),
            format!(":srv 366 mu-gw {agent} :End of /NAMES list"),
        ] {
            on_irc_line(&mut session, &presence, &mut writer, &line).unwrap();
        }
        assert!(
            session.membership.is_present("carol"),
            "the roster rebuilt from the fresh burst"
        );
        assert!(session.membership.is_present("bob"));
        let fronts: Vec<PresenceOp> = std::iter::from_fn(|| fronted.try_recv().ok()).collect();
        assert!(
            fronts
                .iter()
                .any(|op| matches!(op, PresenceOp::Front(p) if p.human_nick() == Some("carol"))),
            "the newly-seen human is fronted on the mesh"
        );

        // A later reconcile still has nothing to do: no channel is stranded.
        assert!(reconcile(&mut session, &mut writer).is_ok());
        assert!(written(&mut lines).is_empty());
    }

    #[test]
    fn already_on_channel_confirms_the_join_and_asks_for_the_roster() {
        // 443 is not a refusal — the desired state holds — but it is not silence
        // either: the server sends no echo and no NAMES, so a JOIN answered this
        // way would otherwise leave the channel pending for the whole connection.
        let Scripted {
            mut session,
            jobs: _jobs,
            link: _link,
        } = scripted_session(PUBLISH_QUEUE);
        let (presence, _fronted) = mpsc::unbounded_channel();
        let (mut writer, mut lines) = transport::LineWriter::scripted(64);
        reconcile(&mut session, &mut writer).unwrap();
        assert_eq!(written(&mut lines), vec![format!("JOIN {LOBBY}")]);

        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            &format!(":srv 443 mu-gw {LOBBY} :is already on channel"),
        )
        .unwrap();

        assert!(session.reconciler.is_joined(LOBBY), "pending was cleared");
        assert_eq!(written(&mut lines), vec![format!("NAMES {LOBBY}")]);
        for line in [
            format!(":srv 353 mu-gw = {LOBBY} :alice"),
            format!(":srv 366 mu-gw {LOBBY} :End of /NAMES list"),
        ] {
            on_irc_line(&mut session, &presence, &mut writer, &line).unwrap();
        }
        assert!(session.membership.is_present("alice"));
    }

    #[test]
    fn a_numeric_names_its_channel_whichever_layout_the_server_uses() {
        // `<client> <channel>` and `<client> <nick> <channel>` are both in the
        // wild for 443; taking params[1] blindly would read a nick as a channel.
        let two = IrcMessage::parse(":srv 443 mu-gw #mu :is already on channel");
        assert_eq!(channel_param(&two).as_deref(), Some("#mu"));
        let three = IrcMessage::parse(":srv 443 mu-gw alice #mu :is already on channel");
        assert_eq!(channel_param(&three).as_deref(), Some("#mu"));
        let none = IrcMessage::parse(":srv 443 mu-gw :is already on channel");
        assert_eq!(channel_param(&none), None);
    }

    // ───────────────────────── The IRC→mesh publish bound ────────────────────

    #[test]
    fn a_full_publish_queue_counts_the_drop_instead_of_growing() {
        let Scripted {
            mut session,
            jobs: _jobs,
            link: _link,
        } = scripted_session(1);
        assert!(enqueue_publish(&mut session, job("a")));
        assert!(!enqueue_publish(&mut session, job("b")));
        assert!(!enqueue_publish(&mut session, job("c")));
        assert_eq!(session.dropped_publish, 2, "counted, not queued");
    }

    #[test]
    fn a_mesh_outage_refuses_a_typed_line_instead_of_queueing_it() {
        let Scripted {
            mut session,
            mut jobs,
            link,
        } = scripted_session(PUBLISH_QUEUE);
        one_agent(&mut session);
        already_in(&mut session, LOBBY, "alice");
        let (mut writer, mut lines) = transport::LineWriter::scripted(64);
        let _ = written(&mut lines);

        // Mesh up: alice's line reaches the queue and she is told nothing.
        on_privmsg(&mut session, &mut writer, "alice", LOBBY, "hello").unwrap();
        assert!(jobs.try_recv().is_ok());
        assert!(written(&mut lines).is_empty());

        // Mesh down: she is told so, now, and nothing is held for later.
        link.send_replace(LinkState {
            up: false,
            generation: 1,
        });
        on_privmsg(&mut session, &mut writer, "alice", LOBBY, "hello again").unwrap();
        assert!(
            jobs.try_recv().is_err(),
            "the line was queued for a dead mesh"
        );
        let sent = written(&mut lines);
        assert!(
            sent.iter()
                .any(|l| l.contains("no mesh destination is reachable")),
            "{sent:?}"
        );
        assert!(
            !sent.iter().any(|l| l.contains("hello again")),
            "a diagnostic never carries the body: {sent:?}"
        );
        assert_eq!(session.refused_out, 1);
    }

    // ───────────────────────── Bot verbs, executed ───────────────────────────

    /// The bodies of the PRIVMSGs this connection sent to `target`.
    fn said_to(lines: &[String], target: &str) -> Vec<String> {
        let prefix = format!("PRIVMSG {target} :");
        lines
            .iter()
            .filter_map(|l| l.strip_prefix(prefix.as_str()).map(str::to_string))
            .collect()
    }

    /// A scripted session with one discovered agent and alice in the lobby —
    /// what every bot verb below is typed into — and the ends it is observed
    /// through.
    struct Verb {
        session: Session,
        jobs: mpsc::Receiver<PublishJob>,
        writer: LineWriter,
        lines: mpsc::Receiver<String>,
        /// The channel the one discovered agent maps to.
        channel: String,
        /// The mesh link, so a test can take it down under a live peer.
        link: watch::Sender<LinkState>,
    }

    fn verb_session() -> Verb {
        let Scripted {
            mut session,
            jobs,
            link,
        } = scripted_session(PUBLISH_QUEUE);
        let channel = one_agent(&mut session);
        already_in(&mut session, LOBBY, "alice");
        let (writer, mut lines) = transport::LineWriter::scripted(64);
        let _ = written(&mut lines);
        Verb {
            session,
            jobs,
            writer,
            lines,
            channel,
            link,
        }
    }

    #[test]
    fn mu_peers_is_answered_privately_and_published_nowhere() {
        let Verb {
            mut session,
            mut jobs,
            mut writer,
            mut lines,
            channel,
            ..
        } = verb_session();
        // Typed in the LOBBY, where an ordinary line would fan out to the mesh.
        on_privmsg(&mut session, &mut writer, "alice", LOBBY, "mu peers").unwrap();
        assert!(jobs.try_recv().is_err(), "a command is never published");
        let sent = written(&mut lines);
        let privately = said_to(&sent, "alice");
        assert_eq!(
            privately.len(),
            sent.len(),
            "the roster is for whoever asked, not for the channel: {sent:?}"
        );
        assert!(privately[0].contains("1 agent"), "{privately:?}");
        assert!(
            privately
                .iter()
                .any(|l| l.contains("cc:abc") && l.contains(&channel)),
            "one line naming the peer's full id and its channel: {privately:?}"
        );
    }

    #[test]
    fn mu_say_publishes_the_text_alone_and_says_nothing_back() {
        let Verb {
            mut session,
            mut jobs,
            mut writer,
            mut lines,
            ..
        } = verb_session();
        on_privmsg(
            &mut session,
            &mut writer,
            "alice",
            LOBBY,
            "mu say cc:abc hello",
        )
        .unwrap();
        let job = jobs.try_recv().expect("a present peer is published to");
        assert_eq!(job.body, "hello", "the verb is not part of the body");
        assert_eq!(job.from, "human:alice");
        assert_eq!(job.targets.len(), 1);
        assert!(
            written(&mut lines).is_empty(),
            "a delivered line needs no answer"
        );
        // …and it wrote the routing memory an explicit address would: typed in
        // the lobby rather than in `#cc-abc`, so the reply comes back privately.
        assert!(!session.remembered.contains_key("alice"));
    }

    #[test]
    fn mu_say_to_an_absent_peer_is_answered_privately_and_published_nowhere() {
        let Verb {
            mut session,
            mut jobs,
            mut writer,
            mut lines,
            ..
        } = verb_session();
        on_privmsg(
            &mut session,
            &mut writer,
            "alice",
            LOBBY,
            "mu say cc:gone hi",
        )
        .unwrap();
        assert!(jobs.try_recv().is_err(), "nothing is published");
        let privately = said_to(&written(&mut lines), "alice");
        assert_eq!(privately.len(), 1, "{privately:?}");
        assert!(privately[0].contains("cc:gone"), "{privately:?}");
        assert!(
            !privately[0].contains("hi"),
            "never the body: {privately:?}"
        );
        assert_eq!(session.refused_out, 1, "a refused command is counted");
    }

    #[test]
    fn mu_say_to_an_ambiguous_alias_names_the_colliding_peers() {
        let Verb {
            mut session,
            mut jobs,
            mut writer,
            mut lines,
            ..
        } = verb_session();
        // Two peers that share the alias `cc-a-b`, and so one channel.
        session.discovery = Discovery::from_srv(HashMap::from([
            ("cc:a:b".to_string(), "mu.agent.cc.a.b.dm".to_string()),
            ("cc:a-b".to_string(), "mu.agent.cc.a-b.dm".to_string()),
        ]));
        on_privmsg(
            &mut session,
            &mut writer,
            "alice",
            LOBBY,
            "mu say cc-a-b hi",
        )
        .unwrap();
        assert!(jobs.try_recv().is_err(), "neither peer is chosen");
        let privately = said_to(&written(&mut lines), "alice");
        assert_eq!(privately.len(), 1, "{privately:?}");
        assert!(privately[0].contains("cc:a:b"), "{privately:?}");
        assert!(privately[0].contains("cc:a-b"), "{privately:?}");
    }

    #[test]
    fn mu_say_with_the_mesh_down_answers_the_sender_not_the_channel() {
        let Verb {
            mut session,
            mut jobs,
            mut writer,
            mut lines,
            link,
            ..
        } = verb_session();
        // The peer is present, so the verb resolves and publishes — but the link
        // it would publish over is gone, which is the one `mu say` outcome the
        // executor answers rather than the command code.
        link.send_replace(LinkState {
            up: false,
            generation: 1,
        });
        on_privmsg(
            &mut session,
            &mut writer,
            "alice",
            LOBBY,
            "mu say cc:abc hello",
        )
        .unwrap();
        assert!(jobs.try_recv().is_err(), "nothing is held for a dead mesh");
        let sent = written(&mut lines);
        assert!(
            said_to(&sent, LOBBY).is_empty(),
            "a command's failure is not the channel's to read: {sent:?}"
        );
        let privately = said_to(&sent, "alice");
        assert_eq!(privately.len(), 1, "{privately:?}");
        assert_eq!(sent.len(), 1, "and nothing else went out: {sent:?}");
        assert!(
            privately[0].contains("no mesh destination is reachable"),
            "{privately:?}"
        );
        assert!(
            !privately[0].contains("hello"),
            "never the body: {privately:?}"
        );
        assert_eq!(session.refused_out, 1, "a refused command is counted");
    }

    #[test]
    fn an_unknown_verb_costs_one_usage_line_and_no_publish() {
        let Verb {
            mut session,
            mut jobs,
            mut writer,
            mut lines,
            ..
        } = verb_session();
        on_privmsg(&mut session, &mut writer, "alice", LOBBY, "mu wat").unwrap();
        assert!(
            jobs.try_recv().is_err(),
            "a near-miss command must not become a broadcast"
        );
        assert_eq!(
            said_to(&written(&mut lines), "alice"),
            vec![USAGE.to_string()]
        );
        assert_eq!(session.refused_out, 0, "usage is an answer, not a refusal");
    }

    // ─────────────── What a session does with the link's state ───────────────

    // ─────────────── What a session does with the link's state ───────────────

    /// The three ends `apply_mesh_link` talks to, none of them a mesh.
    #[allow(clippy::type_complexity)]
    fn link_ends() -> (
        mpsc::UnboundedSender<PresenceOp>,
        mpsc::UnboundedReceiver<PresenceOp>,
        mpsc::Sender<()>,
        mpsc::Receiver<()>,
    ) {
        let (presence_tx, presence_rx) = mpsc::unbounded_channel();
        let (kick_tx, kick_rx) = mpsc::channel(1);
        (presence_tx, presence_rx, kick_tx, kick_rx)
    }

    #[test]
    fn the_link_is_applied_once_rather_than_raced_against_the_first_dm() {
        // A `Connected` queued before the mirror loop started used to compete
        // with `dm_rx` for an unbiased select, so whether the drain discarded a
        // live DM came down to which arm happened to be polled first. The link
        // is applied at defined points now, and only the transition one drains —
        // so the drain can only ever take what the PREVIOUS link left behind.
        let Scripted {
            mut session,
            jobs: _jobs,
            link: _link,
        } = scripted_session(PUBLISH_QUEUE);
        let (presence, _presence_rx, kick, mut kick_rx) = link_ends();
        let (dm_tx, mut dm_rx) = mpsc::channel(8);
        for i in 0..3 {
            dm_tx.try_send(dm(&format!("id{i}"))).unwrap();
        }
        session
            .remembered
            .insert("alice".into(), LOBBY.to_lowercase());

        // Down: the session keeps what it has; there is nothing to rebuild from.
        apply_mesh_link(
            &mut session,
            &presence,
            &mut dm_rx,
            &kick,
            false,
            LinkApply::OnTransition,
        );
        assert_eq!(session.remembered.len(), 1, "a disconnect resets nothing");
        assert!(kick_rx.try_recv().is_err(), "and asks for no sweep");

        // Up again: everything the old link left is gone, deterministically.
        apply_mesh_link(
            &mut session,
            &presence,
            &mut dm_rx,
            &kick,
            true,
            LinkApply::OnTransition,
        );
        assert!(
            dm_rx.try_recv().is_err(),
            "an event from the previous link survived"
        );
        assert!(session.remembered.is_empty(), "routing memory was rebuilt");
        assert!(kick_rx.try_recv().is_ok(), "discovery was refreshed");
    }

    #[test]
    fn a_mesh_reconnect_fronts_every_human_who_is_still_here() {
        // A registration that failed during the outage was never made, and one
        // made before it may be gone with the connection. IRC still says these
        // two are present, so the mesh is told again — otherwise a human who
        // joined during an outage has no inbox until they leave and rejoin.
        let Scripted {
            mut session,
            jobs: _jobs,
            link: _link,
        } = scripted_session(PUBLISH_QUEUE);
        let agent = one_agent(&mut session);
        already_in(&mut session, LOBBY, "alice");
        already_in(&mut session, &agent, "bob");
        let (presence, mut presence_rx, kick, _kick_rx) = link_ends();
        let (_dm_tx, mut dm_rx) = mpsc::channel(8);

        apply_mesh_link(
            &mut session,
            &presence,
            &mut dm_rx,
            &kick,
            true,
            LinkApply::OnTransition,
        );

        let mut fronted = Vec::new();
        while let Ok(op) = presence_rx.try_recv() {
            match op {
                PresenceOp::Front(peer) => fronted.push(peer.to_string()),
                PresenceOp::Release(peer) => panic!("a reconnect released {peer}"),
                _ => panic!("a reconnect emitted an op that is not a fronting"),
            }
        }
        assert_eq!(fronted, ["human:alice", "human:bob"]);
    }

    #[tokio::test]
    async fn a_session_starts_from_a_link_that_dropped_while_irc_was_reconnecting() {
        // The other end of the continuous tracking: the watcher saw the outage
        // with no session up, and the session that registers next must refuse a
        // typed line from its first one rather than discover the outage from an
        // event it has not read yet.
        let (events, events_rx) = mpsc::unbounded_channel();
        let (link, mut link_rx) = watch::channel(LinkState::connected());
        let watcher = tokio::spawn(nats_watcher(events_rx, link));
        events.send(ConnectionEvent::Disconnected).unwrap();
        link_rx.changed().await.expect("the watcher is alive");

        let Scripted {
            mut session,
            mut jobs,
            link: _link,
        } = scripted_session(PUBLISH_QUEUE);
        session.mesh_up = link_rx.clone();
        one_agent(&mut session);
        already_in(&mut session, LOBBY, "alice");
        let (mut writer, mut lines) = transport::LineWriter::scripted(64);
        let _ = written(&mut lines);

        on_privmsg(&mut session, &mut writer, "alice", LOBBY, "hello").unwrap();
        assert!(
            jobs.try_recv().is_err(),
            "a line was queued for a mesh the gateway already knew was down"
        );
        assert!(
            written(&mut lines)
                .iter()
                .any(|l| l.contains("no mesh destination is reachable")),
            "the human was not told"
        );
        watcher.abort();
    }

    // ─────────────────── Which failures a reconnect can fix ──────────────────

    // ─────────────────── Which failures a reconnect can fix ──────────────────

    #[test]
    fn a_configuration_the_adapter_refuses_is_fatal_not_retried_forever() {
        // SASL over cleartext: refused before a byte is framed, and refused
        // identically on every attempt. Retrying it spins a misconfigured
        // gateway instead of telling its operator.
        let mut irc = irc_config();
        irc.sasl = Some(SaslCreds {
            user: "mu-gw".into(),
            password: Secret::new("hunter2"),
        });
        irc.tls = false;
        let Err(err) = start_registration(&irc) else {
            panic!("the adapter must refuse SASL over cleartext");
        };
        assert!(
            matches!(err, SessionError::Fatal(_)),
            "a configuration refusal must end the process, not the connection"
        );
        assert!(
            start_registration(&irc_config()).is_ok(),
            "the same path accepts a configuration the adapter allows"
        );
    }

    #[tokio::test]
    async fn a_server_that_is_not_listening_is_retried() {
        // The other half of the contract: a socket that did not open is a
        // moment, not a configuration.
        let mut irc = irc_config();
        irc.server = "127.0.0.1:1".into();
        let Err(err) = open_connection(&irc).await else {
            panic!("nothing listens on port 1");
        };
        assert!(
            matches!(err, SessionError::Retry(_)),
            "an IRC server that is down is what reconnect backoff is for"
        );
    }

    // ──────────────────────── Control-plane overflow ────────────────────────

    // ──────────────────────── Control-plane overflow ────────────────────────

    #[test]
    fn an_overflowed_join_ends_the_session_rather_than_stranding_the_channel() {
        // The reconciler marks a channel pending BEFORE the JOIN is executed and
        // clears it only on a confirmation or a refusal. A JOIN reported as sent
        // but dropped gets neither, so the channel is never joined, never
        // mirrored and never retried for the life of the connection.
        let Scripted {
            mut session,
            jobs: _jobs,
            link: _link,
        } = scripted_session(PUBLISH_QUEUE);
        let (mut writer, _lines) = transport::LineWriter::scripted(1);
        writer.send_line("a line already in the queue").unwrap();

        assert_eq!(
            reconcile(&mut session, &mut writer),
            Err(SendError::Overflow)
        );
        assert!(!session.reconciler.is_joined(LOBBY));

        // …and because the mark came back off, the reconcile the next connection
        // runs emits the JOIN again instead of skipping a channel it believes is
        // already in flight.
        let (mut writer, mut lines) = transport::LineWriter::scripted(8);
        reconcile(&mut session, &mut writer).unwrap();
        assert_eq!(written(&mut lines), vec![format!("JOIN {LOBBY}")]);
    }

    #[test]
    fn an_overflowed_mirror_line_is_dropped_without_ending_the_session() {
        // The other half of the split: a message body is genuinely best-effort,
        // because a mirror that missed one line is still a mirror.
        let (mut writer, _lines) = transport::LineWriter::scripted(1);
        writer.send_line("a line already in the queue").unwrap();
        assert_eq!(send_mirror(&mut writer, "PRIVMSG #mu :hi"), Ok(()));
        assert_eq!(send(&mut writer, "JOIN #mu"), Err(SendError::Overflow));
    }

    #[test]
    fn a_refusal_names_peers_and_never_a_body() {
        let text = refusal_text(&RefuseReason::AmbiguousChannel(vec![
            PeerId::parse("cc:a"),
            PeerId::parse("cc:b"),
        ]));
        assert!(text.contains("cc:a") && text.contains("cc:b"), "{text}");
        let text = refusal_text(&RefuseReason::AbsentDestination(PeerId::parse("mu:d:s")));
        assert!(text.contains("mu:d:s"), "{text}");
    }

    // ─────────── The reconnect schedule measures REGISTERED uptime ──────────

    /// A scripted attempt against a server that accepts the socket and then
    /// says nothing: `session` spends the whole `REGISTRATION_TIMEOUT` in the
    /// handshake loop, returns `Retry`, and never stamps a registration.
    const STALLED_REGISTRATION: Option<Duration> = None;

    #[test]
    fn a_registration_that_stalls_never_resets_the_backoff() {
        // Measured from the START of the attempt this was hopeless:
        // REGISTRATION_TIMEOUT and BACKOFF_RESET_AFTER are the same 60 seconds,
        // so every stalled attempt "lasted long enough" and the schedule reset
        // to RECONNECT_MIN forever. Registered uptime is the honest measure, and
        // a stall has none.
        assert_eq!(REGISTRATION_TIMEOUT, BACKOFF_RESET_AFTER);
        let mut backoff = Backoff::new();
        let mut waits = Vec::new();
        for _ in 0..5 {
            waits.push(backoff.after(STALLED_REGISTRATION));
            backoff.grow();
        }
        assert_eq!(
            waits,
            vec![
                RECONNECT_MIN,
                RECONNECT_MIN * 2,
                RECONNECT_MIN * 4,
                RECONNECT_MIN * 8,
                RECONNECT_MIN * 16,
            ]
        );
    }

    #[test]
    fn only_uptime_spent_registered_starts_the_schedule_over() {
        let mut backoff = Backoff::new();
        for _ in 0..4 {
            backoff.after(STALLED_REGISTRATION);
            backoff.grow();
        }
        // A connection that registered and then dropped straight away is still
        // a failure: it does not reset.
        assert_eq!(
            backoff.after(Some(BACKOFF_RESET_AFTER - Duration::from_secs(1))),
            RECONNECT_MIN * 16
        );
        // One that registered and held does.
        assert_eq!(backoff.after(Some(BACKOFF_RESET_AFTER)), RECONNECT_MIN);
    }

    #[test]
    fn the_schedule_stops_doubling_at_the_ceiling() {
        let mut backoff = Backoff::new();
        for _ in 0..64 {
            backoff.grow();
        }
        assert_eq!(backoff.after(STALLED_REGISTRATION), RECONNECT_MAX);
    }

    // ────────── A publish the mesh link did not hold still across ───────────

    #[test]
    fn a_publish_over_a_link_that_held_is_delivered() {
        assert!(!publish_is_uncertain(mesh::Publish::Delivered, 7, 7));
    }

    #[test]
    fn a_publish_the_client_could_not_vouch_for_is_treated_as_dropped() {
        // The mesh client saw its own connection change across the write.
        assert!(publish_is_uncertain(mesh::Publish::Uncertain, 7, 7));
    }

    #[test]
    fn a_link_generation_that_moved_under_the_publish_is_treated_as_dropped() {
        // The client read Connected both sides because the drop AND the
        // reconnect landed inside the window. The generation is what survives
        // that coalescing, and it says an outage happened.
        assert!(publish_is_uncertain(mesh::Publish::Delivered, 7, 8));
    }

    // ──────── The first link application is not a drain of our own mail ──────

    #[tokio::test]
    async fn the_first_link_application_keeps_a_dm_the_open_gate_forwarded() {
        let Scripted { mut session, .. } = scripted_session(PUBLISH_QUEUE);
        let (dm_tx, mut dm_rx) = mpsc::channel::<MeshDmEvent>(8);
        let (kick, _kicked) = mpsc::channel::<()>(1);
        let (presence, _fronted) = mpsc::unbounded_channel::<PresenceOp>();

        // The gate is open — the leftovers from before it opened were cleared by
        // the separate drain — and it has forwarded a DM for THIS session.
        dm_tx.send(dm("mine")).await.expect("room in the slot");
        apply_mesh_link(
            &mut session,
            &presence,
            &mut dm_rx,
            &kick,
            true,
            LinkApply::AtStart,
        );
        assert_eq!(
            dm_rx.try_recv().map(|ev| ev.id).ok(),
            Some("mine".to_string()),
            "a DM the gate legitimately forwarded must still be there to deliver"
        );

        // A transition is the other case: the link went away and came back, so
        // what the old one left behind is stale and goes.
        dm_tx.send(dm("stale")).await.expect("room in the slot");
        apply_mesh_link(
            &mut session,
            &presence,
            &mut dm_rx,
            &kick,
            true,
            LinkApply::OnTransition,
        );
        assert!(
            dm_rx.try_recv().is_err(),
            "a DM from before the outage is not delivered after it"
        );
    }

    // ───────────────────────────── Puppets (2b-i) ─────────────────────────────

    use crate::config::PuppetsConfig;
    use crate::puppets::PuppetState;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// A connector that hands each opened connection's server half to `hand`.
    fn scripted_connector(
        hand: mpsc::UnboundedSender<(String, tokio::io::DuplexStream)>,
    ) -> puppet_io::Connector {
        Arc::new(move |irc: IrcConfig| {
            let hand = hand.clone();
            Box::pin(async move {
                let (client, server) = tokio::io::duplex(8192);
                hand.send((irc.nick.clone(), server))
                    .map_err(|_| "no server".to_string())?;
                Ok(transport::spawn_connection(Box::new(client)))
            })
        })
    }

    fn refusing_connector() -> puppet_io::Connector {
        Arc::new(|_irc: IrcConfig| Box::pin(async { Err("connection refused".to_string()) }))
    }

    /// Give a scripted session a pool + executor, the way `session()` does
    /// when `[irc.puppets] enabled = true`.
    fn with_puppets(
        s: &mut Session,
        cfg: PuppetsConfig,
        connector: puppet_io::Connector,
    ) -> mpsc::Receiver<PuppetEvent> {
        let (ev_tx, ev_rx) = mpsc::channel(64);
        let mut irc = irc_config();
        irc.puppets = cfg.clone();
        let pool = Pool::with_budget(cfg, 32, s.isupport.casemapping, AttemptBudget::new());
        let exec = Executor::new(irc, connector, ev_tx, Duration::from_secs(5));
        s.puppets = Some(Puppetry {
            pool,
            exec,
            holder: HashMap::new(),
        });
        ev_rx
    }

    async fn read_until(
        r: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
        prefix: &str,
    ) -> String {
        loop {
            let mut line = String::new();
            let n = tokio::time::timeout(Duration::from_secs(5), r.read_line(&mut line))
                .await
                .expect("server read timed out")
                .expect("server read");
            assert!(n > 0, "client closed before {prefix}");
            if line.starts_with(prefix) {
                return line;
            }
        }
    }

    async fn next_event(rx: &mut mpsc::Receiver<PuppetEvent>) -> PuppetEvent {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a puppet event")
            .expect("executor alive")
    }

    fn eager() -> PuppetsConfig {
        PuppetsConfig {
            min_age_secs: 0,
            ..PuppetsConfig::default()
        }
    }

    #[test]
    fn disabled_puppets_build_no_pool_and_no_executor() {
        // `session()` builds the puppet half only under `enabled = true`; a
        // scripted session mirrors that: with no Puppetry every puppet path is
        // a no-op and no transport can exist.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        assert!(session.puppets.is_none());
        session.discovery = Discovery::from_srv(HashMap::from([(
            "cc:abc".to_string(),
            "mu.agent.cc.abc.dm".to_string(),
        )]));
        puppets_tick(&mut session, &presence);
        sync_owned_nicks(&mut session, &presence);
        assert!(session.puppets.is_none());
        assert!(session.membership.owned_nicks().is_empty());
    }

    #[tokio::test]
    async fn a_discovered_peer_connects_registers_is_owned_before_its_join_and_joins_two_channels()
    {
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        sync_owned_nicks(&mut session, &presence);
        session.discovery = Discovery::from_srv(HashMap::from([(
            "cc:abc".to_string(),
            "mu.agent.cc.abc.dm".to_string(),
        )]));
        // The discovery tick is the pool's tick: the peer connects.
        puppets_tick(&mut session, &presence);
        let (nick, server) = tokio::time::timeout(Duration::from_secs(5), hand_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(nick, "cc-abc");
        let (rh, mut wh) = tokio::io::split(server);
        let mut r = BufReader::new(rh);
        read_until(&mut r, "NICK ").await;
        wh.write_all(b":srv CAP * LS :\r\n").await.unwrap();
        wh.write_all(b":srv 001 cc-abc :Welcome\r\n").await.unwrap();
        wh.write_all(b":srv 376 cc-abc :End of MOTD\r\n")
            .await
            .unwrap();
        let ev = next_event(&mut events).await;
        assert!(matches!(ev, PuppetEvent::Registered { .. }), "{ev:?}");
        assert!(
            !session.membership.is_owned("cc-abc"),
            "precondition: not yet owned"
        );
        on_puppet_event(&mut session, &presence, ev);
        // Ownership before the JOIN: by the time the server sees JOIN, the nick
        // is owned, so its echo can never be read as a human arriving.
        assert!(session.membership.is_owned("cc-abc"));
        let j1 = read_until(&mut r, "JOIN ").await;
        let j2 = read_until(&mut r, "JOIN ").await;
        assert_eq!((j1.trim(), j2.trim()), ("JOIN #mu", "JOIN #cc-abc"));
        assert!(session.membership.joined("#mu", "cc-abc", None).is_empty());
        assert!(!session.membership.is_present("cc-abc"));
        let p = session.puppets.as_ref().unwrap();
        assert_eq!(p.pool.nick_of(&PeerId::parse("cc:abc")), Some("cc-abc"));
        assert_eq!(p.pool.registered_count(), 1);
    }

    #[test]
    fn nicklen_from_a_005_after_001_reaches_the_pool_before_any_offer() {
        // The registration loop breaks on 001 and the pool is built then, with
        // the default NICKLEN of 9; the server's 005 with NICKLEN=32 comes
        // next, through on_isupport_change, and the pool must size offers to
        // it — otherwise every cc UUID session would be cut to nine bytes.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, _hand_rx) = mpsc::unbounded_channel();
        let _events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        session
            .puppets
            .as_mut()
            .unwrap()
            .pool
            .set_nicklen(crate::adapter::DEFAULT_NICKLEN);
        assert_eq!(session.puppets.as_ref().unwrap().pool.nicklen(), 9);
        let (mut writer, _lines) = LineWriter::scripted(16);
        let step = session
            .reg
            .on_message(&IrcMessage::parse(
                ":srv 005 mu-gw NICKLEN=32 :are supported",
            ))
            .unwrap();
        assert!(step.diagnostic.is_some(), "the change is diagnosed");
        on_isupport_change(&mut session, &presence, &mut writer).unwrap();
        assert_eq!(session.puppets.as_ref().unwrap().pool.nicklen(), 32);
    }

    #[tokio::test]
    async fn a_433_retries_the_tail_on_a_fresh_connection_then_gives_up_and_stale_events_are_ignored(
    ) {
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        session.discovery = Discovery::from_srv(HashMap::from([(
            "cc:abc".to_string(),
            "mu.agent.cc.abc.dm".to_string(),
        )]));
        puppets_tick(&mut session, &presence);
        let (_nick, server) = hand_rx.recv().await.unwrap();
        let (rh, mut wh) = tokio::io::split(server);
        let mut r = BufReader::new(rh);
        read_until(&mut r, "NICK ").await;
        wh.write_all(b":srv CAP * LS :\r\n").await.unwrap();
        wh.write_all(b":srv 433 * cc-abc :Nickname is already in use\r\n")
            .await
            .unwrap();
        let ev = next_event(&mut events).await;
        let first_attempt = match &ev {
            PuppetEvent::NickRejected { attempt, .. } => *attempt,
            other => panic!("{other:?}"),
        };
        on_puppet_event(&mut session, &presence, ev);
        // A second connection opens under the tailed nick; the first is gone.
        let (nick2, server2) = tokio::time::timeout(Duration::from_secs(5), hand_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let tailed = crate::mapping::nick_for_tailed(&PeerId::parse("cc:abc"), 32).unwrap();
        assert_eq!(nick2, tailed);
        assert!(matches!(
            session
                .puppets
                .as_ref()
                .unwrap()
                .pool
                .state_of(&PeerId::parse("cc:abc")),
            Some(PuppetState::Connecting { tailed: true, .. })
        ));
        // A late event from the cancelled attempt changes nothing.
        on_puppet_event(
            &mut session,
            &presence,
            PuppetEvent::Ended {
                peer: PeerId::parse("cc:abc"),
                attempt: first_attempt,
                nick: None,
                why: "late".into(),
                confirmed: false,
            },
        );
        assert!(matches!(
            session
                .puppets
                .as_ref()
                .unwrap()
                .pool
                .state_of(&PeerId::parse("cc:abc")),
            Some(PuppetState::Connecting { tailed: true, .. })
        ));
        assert_eq!(
            session.puppets.as_mut().unwrap().exec.stats().stale_events,
            1
        );
        // The tailed form is taken too: channel-only, no third connection.
        let (rh2, mut wh2) = tokio::io::split(server2);
        let mut r2 = BufReader::new(rh2);
        read_until(&mut r2, "NICK ").await;
        wh2.write_all(b":srv CAP * LS :\r\n").await.unwrap();
        wh2.write_all(b":srv 433 * x :Nickname is already in use\r\n")
            .await
            .unwrap();
        let ev = next_event(&mut events).await;
        on_puppet_event(&mut session, &presence, ev);
        let p = session.puppets.as_ref().unwrap();
        assert!(matches!(
            p.pool.state_of(&PeerId::parse("cc:abc")),
            Some(PuppetState::ChannelOnly(_))
        ));
        assert!(p.exec.is_empty(), "no task is left for a channel-only peer");
        assert!(
            tokio::time::timeout(Duration::from_millis(200), hand_rx.recv())
                .await
                .is_err(),
            "no third connection"
        );
    }

    /// The main connection answers the departure barrier(s) the last puppet
    /// event owed: takes the `PING`s, checks their shape, feeds the `PONG`s.
    /// Returns the departed nicks.
    fn answer_departure_barrier(
        session: &mut Session,
        presence: &mpsc::UnboundedSender<PresenceOp>,
    ) -> Vec<String> {
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        let owed = std::mem::take(&mut session.puppet_control);
        let mut departed = Vec::new();
        for line in owed {
            let token = line
                .strip_prefix(&format!("PING :{DEPARTURE_PING}"))
                .unwrap_or_else(|| panic!("not a departure barrier: {line}"))
                .to_string();
            let (attempt, rest) = token
                .split_once('/')
                .unwrap_or_else(|| panic!("no attempt in the token: {token}"));
            attempt.parse::<u64>().expect("an attempt id");
            let (confirmed, nick) = rest
                .split_once('/')
                .unwrap_or_else(|| panic!("no confirmation in the token: {token}"));
            assert!(confirmed == "c" || confirmed == "u", "{token}");
            on_irc_line(
                session,
                presence,
                &mut writer,
                &format!(":srv PONG srv :{DEPARTURE_PING}{token}"),
            )
            .unwrap();
            departed.push(nick.to_string());
        }
        departed
    }

    /// Register a puppet for `cc:abc` on a scripted server and return the
    /// server halves so the test can keep driving it.
    async fn registered_puppet(
        session: &mut Session,
        presence: &mpsc::UnboundedSender<PresenceOp>,
        events: &mut mpsc::Receiver<PuppetEvent>,
        hand_rx: &mut mpsc::UnboundedReceiver<(String, tokio::io::DuplexStream)>,
    ) -> (
        BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    ) {
        session.discovery = Discovery::from_srv(HashMap::from([(
            "cc:abc".to_string(),
            "mu.agent.cc.abc.dm".to_string(),
        )]));
        puppets_tick(session, presence);
        let (_nick, server) = hand_rx.recv().await.unwrap();
        let (rh, mut wh) = tokio::io::split(server);
        let mut r = BufReader::new(rh);
        read_until(&mut r, "NICK ").await;
        wh.write_all(b":srv CAP * LS :\r\n").await.unwrap();
        wh.write_all(b":srv 001 cc-abc :Welcome\r\n").await.unwrap();
        let ev = next_event(events).await;
        assert!(matches!(ev, PuppetEvent::Registered { .. }), "{ev:?}");
        on_puppet_event(session, presence, ev);
        assert!(session.membership.is_owned("cc-abc"));
        (r, wh)
    }

    #[tokio::test]
    async fn a_puppet_released_before_joining_is_freed_by_its_own_departure_report() {
        // mu-gw never shares a channel with this puppet (it is released in the
        // registered→JOIN gap), so no QUIT for it will ever be observed on the
        // main connection. The retiring entry resolves from the executor's
        // Ended report when the puppet's own socket closes after its QUIT.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        let (mut r, wh) =
            registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        // The peer leaves the mesh: the pool quits the puppet and releases the
        // nick; membership retires it (still "ours").
        session.discovery = Discovery::default();
        puppets_tick(&mut session, &presence);
        assert!(session.membership.owned_nicks().is_empty());
        assert_eq!(session.membership.retiring_nicks(), vec!["cc-abc"]);
        assert!(read_until(&mut r, "QUIT").await.starts_with("QUIT :"));
        drop(wh);
        drop(r);
        // The puppet's own connection ends; its report resolves the nick.
        let ev = loop {
            let ev = next_event(&mut events).await;
            if matches!(ev, PuppetEvent::Ended { .. }) {
                break ev;
            }
        };
        assert!(
            matches!(&ev, PuppetEvent::Ended { nick: Some(n), .. } if n == "cc-abc"),
            "{ev:?}"
        );
        on_puppet_event(&mut session, &presence, ev);
        // …at the main connection's barrier: until the server has answered,
        // the nick stays retiring (a JOIN echo of the puppet's could still be
        // on its way).
        assert_eq!(session.membership.retiring_nicks(), vec!["cc-abc"]);
        assert_eq!(
            answer_departure_barrier(&mut session, &presence),
            vec!["cc-abc".to_string()]
        );
        assert!(session.membership.retiring_nicks().is_empty());
        assert!(!session.membership.is_owned("cc-abc"));
        // A human may now take the name.
        session.membership.self_joined("#mu");
        assert_eq!(
            session.membership.joined("#mu", "cc-abc", None),
            vec![HumanEffect::Register(PeerId::human("cc-abc"))]
        );
    }

    #[tokio::test]
    async fn a_server_forced_rename_keeps_the_pool_and_membership_in_step() {
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        let (mut r, mut wh) =
            registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        let _ = read_until(&mut r, "JOIN ").await;
        // The server renames the puppet; the puppet's own connection sees it.
        wh.write_all(b":cc-abc!u@h NICK :cc-abc2\r\n")
            .await
            .unwrap();
        // A lifecycle event, not a line: it is never dropped under load.
        let ev = next_event(&mut events).await;
        assert!(
            matches!(&ev, PuppetEvent::Renamed { from, to, .. } if from == "cc-abc" && to == "cc-abc2"),
            "{ev:?}"
        );
        on_puppet_event(&mut session, &presence, ev);
        let p = session.puppets.as_ref().unwrap();
        assert_eq!(p.pool.nick_of(&PeerId::parse("cc:abc")), Some("cc-abc2"));
        assert!(session.membership.is_owned("cc-abc2"));
        // From the puppet's own report alone — the main connection sees the
        // NICK only when it shares a channel — membership holds the new
        // spelling and has retired nothing.
        assert_eq!(session.membership.owned_nicks(), vec!["cc-abc2"]);
        assert!(
            session.membership.retiring_nicks().is_empty(),
            "the rename was taken for a release"
        );
        // The main connection's NICK may arrive too (either order): membership
        // moves nothing twice, and a later ownership sync changes nothing.
        assert!(session.membership.renamed("cc-abc", "cc-abc2").is_empty());
        sync_owned_nicks(&mut session, &presence);
        assert_eq!(session.membership.owned_nicks(), vec!["cc-abc2"]);
        assert!(
            !session.membership.is_owned("cc-abc"),
            "the old spelling is free"
        );
        assert!(
            session.membership.retiring_nicks().is_empty(),
            "nothing was retired by the rename"
        );
        // The puppet later leaves under the new name: the departure report
        // names it, and resolves it.
        sync_owned_nicks(&mut session, &presence);
        let p = session.puppets.as_mut().unwrap();
        p.exec.execute(PoolAction::Quit {
            peer: PeerId::parse("cc:abc"),
            nick: "cc-abc2".into(),
        });
        read_until(&mut r, "QUIT").await;
        drop(wh);
        drop(r);
        let ev = loop {
            let ev = next_event(&mut events).await;
            if matches!(ev, PuppetEvent::Ended { .. }) {
                break ev;
            }
        };
        assert!(
            matches!(&ev, PuppetEvent::Ended { nick: Some(n), .. } if n == "cc-abc2"),
            "{ev:?}"
        );
    }

    #[tokio::test]
    async fn a_registered_puppets_socket_dropping_frees_its_nick_at_once() {
        // The server drops a registered puppet's connection (no Quit was
        // issued): its Ended is a LIVE attempt's. The pool releases the nick
        // and schedules the retry, and membership — release first, then the
        // departure report — ends with the nick nobody's: not owned, not
        // retiring, so a human may take it by a live JOIN.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        let (mut r, wh) =
            registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        let _ = read_until(&mut r, "JOIN ").await;
        assert!(session.membership.is_owned("cc-abc"));
        drop(wh);
        drop(r);
        let ev = loop {
            let ev = next_event(&mut events).await;
            if matches!(ev, PuppetEvent::Ended { .. }) {
                break ev;
            }
            on_puppet_event(&mut session, &presence, ev);
        };
        assert!(
            matches!(&ev, PuppetEvent::Ended { nick: Some(n), .. } if n == "cc-abc"),
            "{ev:?}"
        );
        on_puppet_event(&mut session, &presence, ev);
        let peer = PeerId::parse("cc:abc");
        let p = session.puppets.as_ref().unwrap();
        assert!(
            matches!(p.pool.state_of(&peer), Some(PuppetState::BackingOff { .. })),
            "{:?}",
            p.pool.state_of(&peer)
        );
        assert!(!p.exec.has(&peer));
        assert!(session.membership.owned_nicks().is_empty());
        assert_eq!(
            answer_departure_barrier(&mut session, &presence),
            vec!["cc-abc".to_string()]
        );
        assert!(
            session.membership.retiring_nicks().is_empty(),
            "the departure report did not resolve the release"
        );
        session.membership.self_joined("#mu");
        assert_eq!(
            session.membership.joined("#mu", "cc-abc", None),
            vec![HumanEffect::Register(PeerId::human("cc-abc"))]
        );
    }

    #[tokio::test]
    async fn a_puppets_join_echo_drained_after_its_departure_report_is_not_a_human() {
        // The puppet registered and JOINed the lobby; the server's echo of
        // that JOIN to the main connection is still queued when the puppet's
        // socket drops and the executor reports. The echo must not front the
        // gateway's own puppet as a human: the report is applied at the
        // barrier, after the echo has been drained.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, wh) =
            registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        let _ = read_until(&mut r, "JOIN ").await;
        drop(wh);
        drop(r);
        let ev = loop {
            let ev = next_event(&mut events).await;
            if matches!(ev, PuppetEvent::Ended { .. }) {
                break ev;
            }
            on_puppet_event(&mut session, &presence, ev);
        };
        on_puppet_event(&mut session, &presence, ev);
        assert_eq!(session.membership.retiring_nicks(), vec!["cc-abc"]);
        assert_eq!(session.puppet_control.len(), 1);
        assert!(
            session.puppet_control[0].starts_with(&format!("PING :{DEPARTURE_PING}"))
                && session.puppet_control[0].ends_with("/c/cc-abc"),
            "{:?}",
            session.puppet_control
        );
        // The main connection drains the puppet's own JOIN echo now — held
        // back under the retiring nick, not fronted.
        on_irc_line(&mut session, &presence, &mut writer, ":cc-abc!u@h JOIN #mu").unwrap();
        assert!(
            !session.membership.is_present("cc-abc"),
            "the puppet's JOIN echo was fronted as a human"
        );
        // The puppet was in the shared channel, so its QUIT is broadcast
        // here too, before the server reads our PING: that settles what the
        // echo was (the puppet) and frees the name.
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":cc-abc!u@h QUIT :Client closed",
        )
        .unwrap();
        assert!(
            !session.membership.is_present("cc-abc"),
            "the echo was the puppet's"
        );
        assert!(session.membership.retiring_nicks().is_empty());
        // The barrier after that is nothing new…
        assert_eq!(
            answer_departure_barrier(&mut session, &presence),
            vec!["cc-abc".to_string()]
        );
        assert!(!session.membership.is_present("cc-abc"));
        // …and a human under the freed name is a human.
        on_irc_line(&mut session, &presence, &mut writer, ":cc-abc!u@h JOIN #mu").unwrap();
        assert!(
            session.membership.is_present("cc-abc"),
            "a human took the name"
        );
        // An unrelated PONG is nothing.
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":srv PONG srv :keepalive",
        )
        .unwrap();
    }

    #[tokio::test]
    async fn a_human_taking_a_departed_puppets_name_before_the_pong_is_fronted_by_it() {
        // The puppet was in no channel the gateway shares (it dropped in the
        // registered→JOIN gap), so no QUIT for it reaches the main
        // connection. A human takes the freed name and JOINs the lobby
        // before the server reads our PING: that JOIN arrives while the nick
        // is still retiring, is held back, and the PONG — which resolves the
        // entry — fronts the human. Nobody is lost to the window.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, mut presence_rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, wh) =
            registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        let _ = read_until(&mut r, "JOIN ").await;
        drop(wh);
        drop(r);
        let ev = loop {
            let ev = next_event(&mut events).await;
            if matches!(ev, PuppetEvent::Ended { .. }) {
                break ev;
            }
            on_puppet_event(&mut session, &presence, ev);
        };
        on_puppet_event(&mut session, &presence, ev);
        assert_eq!(session.membership.retiring_nicks(), vec!["cc-abc"]);
        while presence_rx.try_recv().is_ok() {}
        // The human's JOIN, before the PONG.
        on_irc_line(&mut session, &presence, &mut writer, ":cc-abc!u@h JOIN #mu").unwrap();
        assert!(
            !session.membership.is_present("cc-abc"),
            "held back for now"
        );
        assert_eq!(
            answer_departure_barrier(&mut session, &presence),
            vec!["cc-abc".to_string()]
        );
        assert!(
            session.membership.is_present("cc-abc"),
            "the human who took the name in the window was lost"
        );
        assert!(session.membership.retiring_nicks().is_empty());
        assert!(
            matches!(presence_rx.try_recv(), Ok(PresenceOp::Front(ref p)) if p == &PeerId::human("cc-abc")),
            "the human is fronted by the replay"
        );
    }

    #[tokio::test]
    async fn an_unconfirmed_departure_frees_the_name_without_fronting_what_arrived() {
        // The server never closed the puppet's connection after its QUIT;
        // the grace cut it. The barrier orders nothing about that puppet,
        // so a JOIN under the name before the PONG — maybe the puppet's own
        // echo, still on its way — is not fronted as a human. The name is
        // freed all the same.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut cfg = eager();
        cfg.quit_grace_secs = 1;
        let mut events = with_puppets(&mut session, cfg, scripted_connector(hand_tx));
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, _wh) =
            registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        let _ = read_until(&mut r, "JOIN ").await;
        // The peer leaves; the pool quits the puppet; the server never
        // closes (`_wh` and `r` stay open).
        session.discovery = Discovery::default();
        puppets_tick(&mut session, &presence);
        read_until(&mut r, "QUIT").await;
        let ev = loop {
            let ev = next_event(&mut events).await;
            if matches!(ev, PuppetEvent::Ended { .. }) {
                break ev;
            }
            on_puppet_event(&mut session, &presence, ev);
        };
        assert!(
            matches!(&ev, PuppetEvent::Ended { why, .. } if why == "quit (forced)"),
            "{ev:?}"
        );
        on_puppet_event(&mut session, &presence, ev);
        assert!(
            session.puppet_control[0].contains("/u/"),
            "{:?}",
            session.puppet_control
        );
        on_irc_line(&mut session, &presence, &mut writer, ":cc-abc!u@h JOIN #mu").unwrap();
        assert_eq!(
            answer_departure_barrier(&mut session, &presence),
            vec!["cc-abc".to_string()]
        );
        assert!(
            session.membership.retiring_nicks().is_empty(),
            "the name is freed"
        );
        assert!(
            !session.membership.is_present("cc-abc"),
            "an unconfirmed departure fronted what arrived under the name"
        );
    }

    #[tokio::test]
    async fn a_puppets_rename_report_does_not_move_a_human_who_took_its_old_name() {
        // The main connection saw the puppet's NICK first (a shared channel)
        // and moved it; a human then took the old spelling; only now is the
        // puppet's own Renamed report handled. The human is not this
        // puppet's to move.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, mut wh) =
            registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        let _ = read_until(&mut r, "JOIN ").await;
        // The main connection's NICK for the puppet, then a human as cc-abc.
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":cc-abc!u@h NICK :cc-abc2",
        )
        .unwrap();
        assert!(session.membership.is_owned("cc-abc2"));
        on_irc_line(&mut session, &presence, &mut writer, ":cc-abc!h@h JOIN #mu").unwrap();
        assert!(
            session.membership.is_present("cc-abc"),
            "a human under the old spelling"
        );
        // Now the puppet's own connection reports the same rename.
        wh.write_all(b":cc-abc!u@h NICK :cc-abc2\r\n")
            .await
            .unwrap();
        let ev = loop {
            let ev = next_event(&mut events).await;
            if matches!(ev, PuppetEvent::Renamed { .. }) {
                break ev;
            }
            on_puppet_event(&mut session, &presence, ev);
        };
        on_puppet_event(&mut session, &presence, ev);
        assert!(
            session.membership.is_present("cc-abc"),
            "the puppet's rename report moved the human"
        );
        assert!(session.membership.is_owned("cc-abc2"));
        assert_eq!(session.membership.owned_nicks(), vec!["cc-abc2"]);
        assert!(session.membership.retiring_nicks().is_empty());
        drop(wh);
        drop(r);
    }

    #[tokio::test]
    async fn a_puppets_nick_seen_on_the_main_connection_reaches_the_pool_before_any_sync() {
        // The main connection sees the puppet's NICK (a shared channel).
        // Before the puppet's own Renamed report is handled, another peer
        // registers and syncs ownership. The pool must already hold the new
        // spelling, or the sync would re-own the old name and retire the
        // live one.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, mut wh) =
            registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        let _ = read_until(&mut r, "JOIN ").await;
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":cc-abc!u@h NICK :cc-abc2",
        )
        .unwrap();
        let p = session.puppets.as_ref().unwrap();
        assert_eq!(
            p.pool.nick_of(&PeerId::parse("cc:abc")),
            Some("cc-abc2"),
            "the pool learnt it"
        );
        // A human takes the old spelling; then an unrelated sync.
        on_irc_line(&mut session, &presence, &mut writer, ":cc-abc!h@h JOIN #mu").unwrap();
        assert!(session.membership.is_present("cc-abc"));
        sync_owned_nicks(&mut session, &presence);
        assert_eq!(session.membership.owned_nicks(), vec!["cc-abc2"]);
        assert!(
            session.membership.retiring_nicks().is_empty(),
            "the live name was retired"
        );
        assert!(
            session.membership.is_present("cc-abc"),
            "the human was evicted"
        );
        // The puppet's own report, late: nothing changes.
        wh.write_all(b":cc-abc!u@h NICK :cc-abc2\r\n")
            .await
            .unwrap();
        let ev = loop {
            let ev = next_event(&mut events).await;
            if matches!(ev, PuppetEvent::Renamed { .. }) {
                break ev;
            }
            on_puppet_event(&mut session, &presence, ev);
        };
        on_puppet_event(&mut session, &presence, ev);
        assert_eq!(session.membership.owned_nicks(), vec!["cc-abc2"]);
        assert!(session.membership.is_present("cc-abc"));
        drop(wh);
        drop(r);
    }

    #[tokio::test]
    async fn a_quitting_attempts_end_does_not_disturb_its_replacement() {
        // The pool queues a Quit for a registered puppet (here: teardown-free,
        // by a casemapping change that makes its nick collide) and the next
        // tick reconnects the peer. The OLD connection's Ended arrives after
        // the NEW one registered; it must resolve the old nick and leave the
        // new attempt's state alone.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        let (mut r, wh) =
            registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        let _ = read_until(&mut r, "JOIN ").await;
        let peer = PeerId::parse("cc:abc");
        // The pool decides the puppet must go (a Quit the executor carries
        // out) and releases it — what set_casemapping does for a colliding
        // peer, reproduced through the public surface as a Quit plus a
        // disconnect-and-retry.
        let now = session.puppet_now_ms();
        {
            let p = session.puppets.as_mut().unwrap();
            let nick = p.pool.nick_of(&peer).expect("registered").to_string();
            p.exec.execute(PoolAction::Quit {
                peer: peer.clone(),
                nick,
            });
            for a in p.pool.disconnected(&peer, now) {
                p.exec.execute(a);
            }
        }
        sync_owned_nicks(&mut session, &presence);
        assert_eq!(session.membership.retiring_nicks(), vec!["cc-abc"]);
        // The old connection's QUIT goes out and its task reports Ended as
        // soon as the QUIT is written. Meanwhile the retry comes due (the
        // pool's clock is ours) and the replacement connects and registers.
        read_until(&mut r, "QUIT").await;
        session.process_started = Instant::now() - Duration::from_secs(600);
        puppets_tick(&mut session, &presence);
        let (nick2, server2) = hand_rx.recv().await.expect("a replacement connection");
        assert_eq!(nick2, "cc-abc");
        let (rh2, mut wh2) = tokio::io::split(server2);
        let mut r2 = BufReader::new(rh2);
        read_until(&mut r2, "NICK ").await;
        wh2.write_all(b":srv CAP * LS :\r\n").await.unwrap();
        wh2.write_all(b":srv 001 cc-abc :Welcome\r\n")
            .await
            .unwrap();
        drop(wh);
        drop(r);
        // Both reports are in the queue; which lands first is a race the
        // production loop takes as it comes. The order that matters is the
        // one under test: the replacement's Registered is applied BEFORE the
        // old attempt's Ended.
        let (mut registered, mut ended) = (None, None);
        while registered.is_none() || ended.is_none() {
            match next_event(&mut events).await {
                ev @ PuppetEvent::Registered { .. } => registered = Some(ev),
                ev @ PuppetEvent::Ended { .. } => ended = Some(ev),
                ev => on_puppet_event(&mut session, &presence, ev),
            }
        }
        on_puppet_event(&mut session, &presence, registered.unwrap());
        assert!(session.membership.is_owned("cc-abc"));
        assert!(
            matches!(
                session.puppets.as_ref().unwrap().pool.state_of(&peer),
                Some(PuppetState::Registered { .. })
            ),
            "the replacement registered"
        );
        // Now the old connection's end. It is current (a quitting attempt)
        // but not live: the pool must not see a disconnect.
        on_puppet_event(&mut session, &presence, ended.unwrap());
        let p = session.puppets.as_ref().unwrap();
        assert!(
            matches!(p.pool.state_of(&peer), Some(PuppetState::Registered { .. })),
            "the old connection's end disturbed the replacement: {:?}",
            p.pool.state_of(&peer)
        );
        assert_eq!(p.pool.nick_of(&peer), Some("cc-abc"));
        assert!(p.exec.has(&peer), "the replacement task is live");
        assert!(session.membership.is_owned("cc-abc"));
        assert!(
            session.membership.retiring_nicks().is_empty(),
            "the old nick's departure resolved"
        );
        drop(wh2);
    }

    #[tokio::test]
    async fn the_attempt_budget_survives_teardown_into_the_next_pool() {
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), refusing_connector());
        session.discovery = Discovery::from_srv(HashMap::from([
            ("cc:abc".to_string(), "mu.agent.cc.abc.dm".to_string()),
            ("cc:bcd".to_string(), "mu.agent.cc.bcd.dm".to_string()),
        ]));
        puppets_tick(&mut session, &presence);
        // Both attempts fail at once; the pool backs them off.
        for _ in 0..2 {
            let ev = next_event(&mut events).await;
            on_puppet_event(&mut session, &presence, ev);
        }
        assert_eq!(
            session.puppets.as_mut().unwrap().pool.attempts_in_window(0),
            2
        );
        let mut budget = AttemptBudget::new();
        teardown_puppets(&mut session, &presence, &mut budget).await;
        assert!(session.puppets.is_none());
        assert_eq!(
            budget.in_window(0),
            2,
            "the history came back out of the pool"
        );
        // The next session's pool continues the same window.
        let mut next = Pool::with_budget(eager(), 32, CaseMapping::Ascii, budget);
        assert_eq!(next.attempts_in_window(0), 2);
    }

    #[tokio::test]
    async fn teardown_quits_a_registered_puppet_and_releases_its_nick() {
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        session.discovery = Discovery::from_srv(HashMap::from([(
            "cc:abc".to_string(),
            "mu.agent.cc.abc.dm".to_string(),
        )]));
        puppets_tick(&mut session, &presence);
        let (_nick, server) = hand_rx.recv().await.unwrap();
        let (rh, mut wh) = tokio::io::split(server);
        let mut r = BufReader::new(rh);
        read_until(&mut r, "NICK ").await;
        wh.write_all(b":srv CAP * LS :\r\n").await.unwrap();
        wh.write_all(b":srv 001 cc-abc :Welcome\r\n").await.unwrap();
        wh.write_all(b":srv 376 cc-abc :End of MOTD\r\n")
            .await
            .unwrap();
        let ev = next_event(&mut events).await;
        on_puppet_event(&mut session, &presence, ev);
        assert!(session.membership.is_owned("cc-abc"));
        let mut budget = AttemptBudget::new();
        let started = Instant::now();
        let teardown = teardown_puppets(&mut session, &presence, &mut budget);
        // The server: reads the QUIT, closes the connection.
        let server = async {
            let line = read_until(&mut r, "QUIT").await;
            drop(wh);
            drop(r);
            line
        };
        let (_, quit_line) = tokio::join!(teardown, server);
        assert!(quit_line.starts_with("QUIT :"), "{quit_line}");
        assert!(started.elapsed() < Duration::from_secs(3 + 2));
        assert!(session.puppets.is_none());
        // Released, not free: the nick is retiring until the main connection
        // observes its QUIT (or ends, which resets membership with it).
        assert!(session.membership.owned_nicks().is_empty());
        assert_eq!(session.membership.retiring_nicks(), vec!["cc-abc"]);
        assert!(
            session.membership.is_owned("cc-abc"),
            "retiring still reads as ours"
        );
        session.membership.reset();
        assert!(!session.membership.is_owned("cc-abc"));
    }
}
