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

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use mu_dialogue::mesh::{self, MeshDmEvent, MeshTarget};
use mu_peer::PeerId;

use crate::adapter::{
    IrcMessage, IsupportSettings, JoinAccount, Registration, Step, SystemClock, Transport,
};
use crate::config::{GatewayConfig, IrcConfig};
use crate::framing::{frame_privmsg, FrameParams};
use crate::mapping::{channel_for, fold_nick};
use crate::membership::{Attribution, ChannelEffect, ChannelReconciler, HumanEffect, Membership};
use crate::outbound::{
    Answer, CommandReply, MemoryDestination, OutDrop, OutEnv, Outbound, OutboundDecision,
    RefuseReason, NO_AGENTS, USAGE,
};
use crate::routing::{RouteDecision, RouteEnv, Router};
use crate::transport::{self, Connection, FromServer, LineWriter, SendError};

use super::mesh_side::{
    discard_stale_discovery, discard_stale_dms, publish_worker, Discovery, LinkState, MeshInputs,
    MeshSide, PresenceOp, PublishJob, PUBLISH_QUEUE,
};
use super::puppet_io::{self, Executor, PuppetCommand, PuppetEvent};
use super::puppet_wire::pong;
use crate::puppets::{self, AttemptBudget, FanIn, Pool, PoolAction, PuppetState};
use crate::slots::{Grant, Slots};

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
    /// The server cannot carry the puppets this gateway is configured for:
    /// stop, rather than bridge without the identity (invariant 7).
    Refuse(String),
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
    // in outlive a session: the server's throttle window does not reset when
    // the main connection does.
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
            Ok(Stop::Refuse(why)) => {
                fatal = Some(anyhow!(why));
                break;
            }
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
    /// Reports of a member still unattributed when a pass that should have
    /// answered for it ended, or when nobody could be asked: occurrences,
    /// not distinct members — a member two passes left pending counts
    /// twice. A rate, for the operator to watch (plan, *Identity by
    /// account*, rule 5).
    attribution_overdue: u64,
    /// The puppet pool and its executor — `None` when `[irc.puppets]
    /// enabled = false`, in which case no puppet transport is ever created.
    puppets: Option<Puppetry>,
    /// Process-lifetime clock the pool's timestamps are taken from (its
    /// attempt budget outlives the session; see [`run`]).
    process_started: Instant,
    /// Whether the capabilities a provisioned pool needs have been judged —
    /// once the welcome burst is over, or at the registration deadline
    /// without it. No puppet is dialled before the server's word is in.
    caps_settled: bool,
}

/// The puppet half of a session: the pool decides, the executor does, the
/// slot pool says which account. Identity is never kept here — a member is
/// ours by the account the server puts on its lines (`membership`), and the
/// pool's nick for a puppet comes from the puppet's own connection only
/// (`specs/plans/mu-irc-gateway-v1-puppets.md`, *Identity by account*).
struct Puppetry {
    pool: Pool,
    exec: Executor,
    slots: Slots,
    counters: PuppetCounters,
}

/// What stayed unlikely, counted and said aloud when it happens — never
/// designed around (plan, *Identity by account*, rule 5). The fourth such
/// count, `attribution_overdue`, is the session's: it is about the roster,
/// not the pool.
#[derive(Debug, Default, Clone, Copy)]
struct PuppetCounters {
    /// A returned slot whose QUIT was never read here within the window.
    reuse_waited_out: u64,
    /// A member attributed to a slot account that is neither leased nor
    /// waiting: another process holds our credential, or our bookkeeping is
    /// wrong (R7). The member is ours all the same.
    slot_account_unleased: u64,
    /// A member attributed to a leased account under a nick the pool does
    /// not know for that peer.
    pool_nick_behind: u64,
}

impl Session {
    /// Process-lifetime milliseconds, the pool's clock.
    fn puppet_now_ms(&self) -> u64 {
        self.process_started.elapsed().as_millis() as u64
    }

    /// Peers whose lease must not be taken by an eviction: the plan's "never
    /// evict an agent that has exchanged a line with a human inside the
    /// routing memory's window". Empty until the routing memory keys on
    /// peer↔human pairs (mu-irc-remote-session-zgbdz.8.2).
    fn protected_peers(&self) -> HashSet<PeerId> {
        HashSet::new()
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

    // The puppet pool, when enabled. A puppet is known by the account the
    // server puts on its lines, so the main connection must have negotiated
    // the capabilities that put it there. They are judged once the welcome
    // burst is over — WHOX is an ISUPPORT token, and ISUPPORT follows 001 —
    // or at the registration deadline if the burst never ends, and on every
    // line after (rule 1; ruling E). Until then no puppet is dialled.
    let caps_deadline = tokio::time::Instant::now() + REGISTRATION_TIMEOUT;
    let puppet_drain = irc.puppets.event_queue;
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
                slots: slot_pool(&irc.puppets),
                counters: PuppetCounters::default(),
            }),
        )
    } else {
        (None, None)
    };

    let mut session = Session {
        puppets,
        process_started,
        caps_settled: !irc.puppets.enabled,
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
        attribution_overdue: 0,
        reg,
    };
    // The slot accounts are ours by construction: told to membership once,
    // before any roster exists, leased or not.
    if let Some(p) = session.puppets.as_ref() {
        let effects = session
            .membership
            .set_owned_accounts(p.pool.config().slot_accounts());
        debug_assert!(effects.is_empty(), "told before any roster exists");
    }

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
                    teardown_puppets(&mut session, puppet_budget).await;
                    quit(&mut writer, &mut inbound).await;
                    break Stop::Shutdown;
                }
            }
            // A server that never ends its welcome burst does not get to
            // run puppets unjudged: the registration deadline judges them.
            _ = tokio::time::sleep_until(caps_deadline), if !session.caps_settled => {
                session.caps_settled = true;
                if let Some(why) = caps_refusal(&session) {
                    teardown_puppets(&mut session, puppet_budget).await;
                    quit(&mut writer, &mut inbound).await;
                    break Stop::Refuse(why);
                }
                puppets_tick(&mut session);
            }
            ev = async {
                match puppet_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => match ev {
                Some(ev) => on_puppet_event(&mut session, ev),
                None => puppet_rx = None,
            },
            event = inbound.recv() => match event {
                Some(FromServer::Line(line)) => {
                    // The puppets' own reports first: the pool's nick for a
                    // puppet comes from its own connection, and a report
                    // already queued precedes the main connection's line
                    // that may follow from it.
                    for _ in 0..puppet_drain {
                        let Some(ev) = puppet_rx.as_mut().and_then(|rx| rx.try_recv().ok()) else {
                            break;
                        };
                        on_puppet_event(&mut session, ev);
                    }
                    if let Err(e) = on_irc_line(&mut session, &mesh_side.presence, &mut writer, &line) {
                        break Stop::Reconnect(format!("write failed: {e}"));
                    }
                    // The capabilities a provisioned pool needs, judged once
                    // the welcome burst is over and on every line after: a
                    // `CAP DEL` that withdraws one is the same refusal as one
                    // never negotiated (invariant 7, ruling E). The first
                    // judgement passed, the pool's first tick runs at once.
                    if session.puppets.is_some() {
                        let first = !session.caps_settled && welcome_burst_over(&line);
                        if first {
                            session.caps_settled = true;
                        }
                        if session.caps_settled {
                            if let Some(why) = caps_refusal(&session) {
                                teardown_puppets(&mut session, puppet_budget).await;
                                quit(&mut writer, &mut inbound).await;
                                break Stop::Refuse(why);
                            }
                        }
                        if first {
                            puppets_tick(&mut session);
                        }
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
                    // …and the puppet tick: peers start aging, departed peers'
                    // puppets quit, due peers connect, under the pool's pacing.
                    puppets_tick(&mut session);
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
    // The main connection is gone (or going): every puppet goes with it.
    teardown_puppets(&mut session, puppet_budget).await;

    info!(
        uptime = ?session.started.elapsed(),
        ingress_refused = session.dropped_ingress,
        ingress_orphaned = orphaned,
        routing_drops = session.dropped_route,
        publish_drops = session.dropped_publish,
        publish_drops_mesh_down = session.dropped_offline.load(Ordering::Relaxed),
        publish_link_uncertain = session.uncertain_publish.load(Ordering::Relaxed),
        outbound_refusals = session.refused_out,
        attribution_overdue = session.attribution_overdue,
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
                // NAMES says who is there; this asks who they are. The members
                // already present when the gateway arrives are the only ones
                // `extended-join` cannot attribute, because their JOIN happened
                // before this connection existed.
                let _ = request_roster_accounts(session, writer, &channel)?;
            } else {
                // `joined` is add-only, so an explicit "not logged in" is
                // applied after it, through the one path that clears.
                let said = session.reg.join_account(&msg);
                let account = match said {
                    JoinAccount::Account(a) => Some(a.to_string()),
                    JoinAccount::LoggedOut | JoinAccount::Unknown => None,
                };
                let effects = session.membership.joined(&channel, &nick, account);
                apply_human_effects(session, presence, effects);
                if said == JoinAccount::LoggedOut {
                    // Clearing can flip the nick from one of our accounts to a
                    // human, so the correction is applied like any other — the
                    // ACCOUNT and 354 handlers already do this.
                    let effects = session
                        .membership
                        .set_account(&nick, Attribution::LoggedOut);
                    apply_human_effects(session, presence, effects);
                }
                // A JOIN that left the member pending (no account field, and
                // puppets provisioned) asks for the one nick now, rather than
                // wait for the next roster pass: pending lasts a round trip.
                if session.membership.is_pending(&nick)
                    && !request_roster_accounts(session, writer, &nick)?
                {
                    report_overdue(session, &nick, vec![nick.clone()]);
                }
                if session.puppets.is_some() {
                    note_slot_member(session, &nick);
                }
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
            // The departure a returned slot waits for: a member attributed to
            // a slot account leaving the server. Read before the roster
            // forgets the member.
            let account = session.membership.account_of(&nick).map(str::to_string);
            let effects = session.membership.quit(&nick);
            apply_human_effects(session, presence, effects);
            if let (Some(p), Some(account)) = (session.puppets.as_mut(), account) {
                if p.slots.departure_read(&account) {
                    debug!(%nick, %account, "puppet: departure read; the slot is leasable again");
                }
            }
        }
        // ACCOUNT (from `account-notify`): `<account>`, `*` when logging out.
        // Presence does not change — the same person is in the same channels —
        // so this re-attributes and emits nothing.
        "ACCOUNT" => {
            if !session.reg.negotiated().account_notify || nick.is_empty() {
                return Ok(());
            }
            // `ACCOUNT *` is an answer (logged out). A missing or empty
            // parameter is a malformed line and says nothing.
            let answer = match msg.params.first().map(String::as_str) {
                None | Some("") => return Ok(()),
                Some("*") => Attribution::LoggedOut,
                Some(a) => Attribution::Account(a.to_string()),
            };
            let effects = session.membership.set_account(&nick, answer);
            apply_human_effects(session, presence, effects);
        }
        // RPL_WHOSPCRPL, the WHOX reply to `request_roster_accounts`:
        // `<nick> <token> <channel> <nick> <account>` for the fields
        // `WHOX_ROSTER_FIELDS` asked for. A reply carrying another token (or
        // none) answers somebody else's WHO and is not ours to read.
        "354" => {
            // Guarded on the capability at the REPLY end as well as when the
            // request is made: a `354` arriving on a connection that never
            // advertised WHOX (or withdrew it with `-WHOX`) answers a request
            // this gateway did not open, and must not move attribution. The
            // symmetric `ACCOUNT` path checks `account_notify` the same way.
            if !session.reg.has_whox() {
                return Ok(());
            }
            let (Some(token), Some(who), Some(account)) =
                (msg.params.get(1), msg.params.get(3), msg.params.get(4))
            else {
                return Ok(());
            };
            if token != WHOX_ROSTER_TOKEN || who.is_empty() {
                return Ok(());
            }
            // WHOX spells "not registered" as `0`. That is an ANSWER, not a
            // silence, so it goes through the clearing path deliberately —
            // unlike a NAMES line, which has no account field at all and
            // therefore says nothing either way.
            // WHOX `0` (or `*`) is an answer: no account. An empty field is a
            // malformed reply and says nothing. The attribution can change
            // whether this nick is one of ours, so the correction it returns
            // is applied like any other effect.
            let answer = match account.as_str() {
                "" => return Ok(()),
                "0" | "*" => Attribution::LoggedOut,
                a => Attribution::Account(a.to_string()),
            };
            let effects = session.membership.set_account(who, answer);
            apply_human_effects(session, presence, effects);
        }
        // RPL_ENDOFWHO: `<client> <mask> :End of WHO list`. The pass that
        // should have answered for every member of `mask` is over: anyone
        // still pending is overdue — neither fronted nor ours, which is not
        // a state to sit in silently (invariant 7), so it is counted and said.
        "315" => {
            let Some(mask) = msg.params.get(1) else {
                return Ok(());
            };
            // A channel mask by the channel types the rest of the bridge
            // recognises (`channel_param`), not a literal `#`.
            let overdue: Vec<String> = if mask.starts_with(['#', '&', '!', '+']) {
                session.membership.unattributed_in(mask)
            } else if session.membership.is_pending(mask) {
                vec![mask.clone()]
            } else {
                Vec::new()
            };
            let mask = mask.clone();
            report_overdue(session, &mask, overdue);
        }
        "NICK" => {
            let Some(to) = msg.params.first().cloned() else {
                return Ok(());
            };
            if session.is_self(&nick) {
                session.self_nick.clone_from(&to);
                session.out.set_self_nick(&to);
            }
            // The pool is not moved from here: a puppet's nick comes from
            // its own connection (plan, *Identity by account*, rule 3).
            let effects = session.membership.renamed(&nick, &to);
            apply_human_effects(session, presence, effects);
            // A pending member that renamed before the answer came: any
            // answer naming the old nick finds nobody, so ask again under
            // the new one.
            if session.membership.is_pending(&to) && !request_roster_accounts(session, writer, &to)?
            {
                report_overdue(session, &to, vec![to.clone()]);
            }
            if session.puppets.is_some() {
                note_slot_member(session, &to);
            }
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
            // No WHOX, no roster pass, no `315` to come: whoever the snapshot
            // left pending stays so, and that is said now.
            if !session.reg.has_whox() {
                let overdue = session.membership.unattributed_in(&channel);
                report_overdue(session, &channel, overdue);
            }
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
    // A line FROM the peer is activity; being listed by discovery is not (a
    // `mu ask` peer stays listed an hour past its last line), so this is the
    // one place a lease is kept alive.
    let now = session.puppet_now_ms();
    if let Some(p) = session.puppets.as_mut() {
        p.slots.touch(&PeerId::parse(&ev.from), now);
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
        // The pool's nick table re-derives from wire spellings; a puppet
        // whose nick now folds onto another's is quit and re-offered.
        let tick_ms = session.puppet_now_ms();
        let protected = session.protected_peers();
        if let Some(p) = session.puppets.as_mut() {
            let actions = p.pool.set_casemapping(now.casemapping);
            run_actions(p, actions, tick_ms, &|peer: &PeerId| {
                protected.contains(peer)
            });
        }
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
    send(writer, &format!("NAMES {channel}"))?;
    request_roster_accounts(session, writer, channel).map(|_| ())
}

/// The WHOX token the gateway stamps its roster-attribution requests with, so
/// a `354` answering somebody else's `WHO` is not read as one of ours. Any
/// value in `0..=999` does; this one is arbitrary and stable.
const WHOX_ROSTER_TOKEN: &str = "742";

/// The WHOX field selector: token, channel, nick, account — and nothing else.
///
/// WHOX emits the requested fields in its own canonical order, not in the
/// order they were asked for. For `%tcna` the two happen to coincide, so the
/// positional parsing below is reading the canonical order and not relying on
/// the request order — worth stating, because the two agreeing here is a
/// coincidence of this selector rather than a property to lean on if fields
/// are ever added.
///
/// The shape was verified against a throwaway Ergo 2.19 rather than taken from
/// the WHOX convention:
///
/// ```text
/// 354 mu-gw 742 #probe alice 0      (anonymous: account field is `0`)
/// 354 mu-gw 742 #probe cc-1  cc-1   (a registered account)
/// ```
const WHOX_ROSTER_FIELDS: &str = "%tcna";

/// Ask the server to attribute an account to every nick in `channel`.
///
/// Only under `WHOX`: plain `WHO` has no account field, so on a server without
/// it there is nothing to ask and the roster stays attributed by
/// `extended-join` and `ACCOUNT` alone. Failing quietly here is correct — a
/// missing attribution is visible to the identity rules as "unknown", never as
/// a wrong answer.
fn request_roster_accounts(
    session: &Session,
    writer: &mut LineWriter,
    channel: &str,
) -> Result<bool, SendError> {
    if !session.reg.has_whox() {
        return Ok(false);
    }
    send(
        writer,
        &format!("WHO {channel} {WHOX_ROSTER_FIELDS},{WHOX_ROSTER_TOKEN}"),
    )?;
    Ok(true)
}

/// Members pending with no answer on its way — the WHOX pass for `mask` is
/// over, or there is no WHOX to ask with: counted (once per report; a member
/// still pending at the next pass is reported again), and said aloud.
/// Pending is a round trip's state; one that cannot end is not a state to
/// sit in silently (invariant 7).
fn report_overdue(session: &mut Session, mask: &str, nicks: Vec<String>) {
    for nick in nicks {
        session.attribution_overdue += 1;
        warn!(%nick, %mask, overdue = session.attribution_overdue, whox = session.reg.has_whox(), "unattributed with no answer coming: neither fronted nor ours until the server says");
    }
}

/// The capabilities a provisioned pool needs NEGOTIATED on the main
/// connection — the account on every line is the whole identity — or the
/// names of those missing.
/// Whether `line` ends the server's welcome burst (`RPL_ENDOFMOTD` or
/// `ERR_NOMOTD`): everything ISUPPORT had to say has been said by then.
fn welcome_burst_over(line: &str) -> bool {
    matches!(IrcMessage::parse(line).command.as_str(), "376" | "422")
}

/// The refusal a provisioned pool owes when the main connection lacks what
/// it needs — the message names what is missing.
fn caps_refusal(session: &Session) -> Option<String> {
    required_caps(&session.reg).err().map(|missing| {
        format!(
            "[irc.puppets] needs {missing}, which the server did not negotiate (or withdrew); \
             fix the server or set `enabled = false`"
        )
    })
}

fn required_caps(reg: &Registration<SystemClock>) -> Result<(), String> {
    let n = reg.negotiated();
    let missing: Vec<&str> = [
        ("account-tag", n.account_tag),
        ("extended-join", n.extended_join),
        ("account-notify", n.account_notify),
        ("WHOX", reg.has_whox()),
    ]
    .iter()
    .filter(|(_, have)| !have)
    .map(|(name, _)| *name)
    .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing.join(", "))
    }
}

/// The slot pool for a config: every slot account, the eviction window and
/// the reuse wait, in the pool's milliseconds.
fn slot_pool(cfg: &crate::config::PuppetsConfig) -> Slots {
    Slots::new(
        cfg.slot_accounts(),
        cfg.slot_idle_secs.saturating_mul(1000),
        cfg.departure_wait_secs.saturating_mul(1000),
    )
}

/// Feed the discovery snapshot to the pool and execute what it decides. Being
/// listed is PRESENCE, not activity, so a tick touches no lease.
fn puppets_tick(session: &mut Session) {
    // Nothing is dialled before the server's capabilities are judged: a
    // puppet on a server that cannot attribute it is the thing rule 1
    // forbids, and the judgement is one welcome burst away.
    if !session.caps_settled {
        return;
    }
    let now = session.puppet_now_ms();
    let peers = session.discovery.peers.clone();
    let protected = session.protected_peers();
    let Some(p) = session.puppets.as_mut() else {
        return;
    };
    for account in p.slots.waited_out(now) {
        p.counters.reuse_waited_out += 1;
        warn!(%account, waited_out = p.counters.reuse_waited_out, "puppet: a returned slot's QUIT was never read here within the window; leasing it again (a 433 may follow)");
    }
    let mut actions = p.pool.observe(&peers, now);
    actions.extend(p.pool.tick(now));
    run_actions(p, actions, now, &|peer: &PeerId| protected.contains(peer));
}

/// Execute the pool's actions. The ONE place a `Connect` becomes a
/// connection: the pool's decision ("this peer should have a puppet") meets
/// the slot pool's ("this peer may have THIS account"). A free slot, or an
/// evictable one — the previous holder quit first, loudly, and the pool
/// told so it backs off and asks again — or the plan's spillover
/// (`Pool::no_slot`): retried on the backoff schedule, never a shared nick.
/// `protected` is asked only about eviction candidates.
fn run_actions(
    p: &mut Puppetry,
    actions: Vec<PoolAction>,
    now_ms: u64,
    protected: &dyn Fn(&PeerId) -> bool,
) {
    // Peers whose Connect this batch has not reached yet: their attempt is
    // spent but no socket is open, so an eviction that moves one refunds
    // it. A victim already dialling keeps its charge; the server saw it.
    let mut queued: HashSet<PeerId> = actions
        .iter()
        .filter_map(|a| match a {
            PoolAction::Connect { peer, .. } => Some(peer.clone()),
            _ => None,
        })
        .collect();
    for a in actions {
        match a {
            PoolAction::Connect { peer, .. } => {
                queued.remove(&peer);
                // An earlier action in this batch moved this peer (an eviction
                // unwound its Connect): a lease taken now would evict a second
                // holder and dial a peer the pool has backing off.
                if !matches!(p.pool.state_of(&peer), Some(PuppetState::Connecting { .. })) {
                    debug!(peer = %peer, "puppet: a Connect unwound earlier in its batch; skipped");
                    continue;
                }
                match p.slots.lease(&peer, now_ms, protected) {
                    Grant::Leased { account, evicted } => {
                        if let Some(prev) = evicted {
                            warn!(
                                evicted = %prev, to = %peer, account = %account,
                                evictions = p.slots.evictions(),
                                "puppet: slot pool full — evicting the least recently active lease"
                            );
                            let quit = match p.pool.nick_of(&prev) {
                                Some(n) => PoolAction::Quit {
                                    peer: prev.clone(),
                                    nick: n.to_string(),
                                },
                                None => PoolAction::Cancel { peer: prev.clone() },
                            };
                            p.exec.execute(quit);
                            if queued.contains(&prev) {
                                p.pool.not_dialled(&prev, now_ms);
                            } else {
                                let _ = p.pool.disconnected(&prev, now_ms);
                            }
                        }
                        dial(p, peer, account, now_ms);
                    }
                    Grant::Held(account) => dial(p, peer, account, now_ms),
                    Grant::Spillover => {
                        warn!(peer = %peer, free = p.slots.free(), waiting = p.slots.waiting().len(), "puppet: no slot to lease — spillover, channel-only");
                        for a in p.pool.no_slot(&peer, now_ms) {
                            p.exec.execute(a);
                        }
                    }
                }
            }
            PoolAction::Cancel { peer } => {
                // Nothing registered as the account: free at once.
                if let Some(account) = p.slots.release(&peer) {
                    debug!(peer = %peer, %account, "puppet: slot released with the cancelled attempt");
                }
                p.exec.execute(PoolAction::Cancel { peer });
            }
            other => p.exec.execute(other),
        }
    }
}

/// Dial `peer` as the leased `account`. A credential the executor cannot
/// produce is resolved here: nothing was opened, so the slot is free at once
/// and the pool refunds the attempt and backs off.
fn dial(p: &mut Puppetry, peer: PeerId, account: String, now_ms: u64) {
    if let Err(why) = p.exec.connect_leased(peer.clone(), account.clone()) {
        p.slots.release(&peer);
        if !p.pool.not_dialled(&peer, now_ms) {
            debug!(peer = %peer, "puppet: refused dial for a peer the pool had already moved");
        }
        warn!(peer = %peer, account = %account, %why, "puppet: leased dial refused; retrying on the backoff schedule");
    }
}

/// A member of the main connection's roster attributed to a slot account:
/// ours (membership decided that already); here only the two counters that
/// say when the lease bookkeeping and the roster disagree.
fn note_slot_member(session: &mut Session, nick: &str) {
    let cm = session.isupport.casemapping;
    let Some(account) = session.membership.account_of(nick).map(str::to_string) else {
        return;
    };
    let Some(p) = session.puppets.as_mut() else {
        return;
    };
    if !p
        .pool
        .config()
        .slot_accounts()
        .iter()
        .any(|a| fold_nick(a, cm) == fold_nick(&account, cm))
    {
        return;
    }
    match p.slots.holder_of(&account) {
        None if !p.slots.holds(&account) => {
            p.counters.slot_account_unleased += 1;
            warn!(%nick, %account, unleased = p.counters.slot_account_unleased, "puppet: a slot account is on the roster with no lease and no return pending — another process holds its credential, or our bookkeeping is wrong");
        }
        Some(peer)
            if p.pool
                .nick_of(peer)
                .is_none_or(|n| fold_nick(n, cm) != fold_nick(nick, cm)) =>
        {
            p.counters.pool_nick_behind += 1;
            debug!(%nick, %account, peer = %peer, behind = p.counters.pool_nick_behind, "puppet: the roster shows the account under a nick the pool does not know yet");
        }
        _ => {}
    }
}

/// One event from a puppet task. Stale events (from an attempt the executor
/// no longer tracks) are counted and ignored.
fn on_puppet_event(session: &mut Session, ev: PuppetEvent) {
    let now = session.puppet_now_ms();
    let cm = session.isupport.casemapping;
    let protected = session.protected_peers();
    let (prefix, lobby, channellen) = (
        session.prefix.clone(),
        session.lobby.clone(),
        session.isupport.channellen,
    );
    let gateway = session.self_nick.clone();
    let Some(p) = session.puppets.as_mut() else {
        return;
    };
    if !p.exec.is_current(&ev) {
        return;
    }
    match ev {
        PuppetEvent::Registered { peer, nick, .. } => {
            for a in p.pool.registered(&peer, &nick, now) {
                p.exec.execute(a);
            }
            if p.pool.nick_of(&peer).is_some() {
                let mut channels = vec![lobby.clone()];
                if let Some(own) = channel_for(&peer, &prefix, channellen) {
                    channels.push(own);
                }
                info!(peer = %peer, nick = %nick, "puppet: registered");
                if !p.exec.command(&peer, PuppetCommand::Join(channels)) {
                    // A registered puppet that cannot be told to JOIN has no
                    // voice and no ears, and is not left standing (invariant
                    // 7): quit — the stop reaches a task whatever it is
                    // parked on — and the pool backs off to dial again.
                    warn!(peer = %peer, nick = %nick, "puppet: JOIN not queued (task stalled); quit, to dial again on the backoff schedule");
                    p.exec.execute(PoolAction::Quit {
                        peer: peer.clone(),
                        nick,
                    });
                    let actions = p.pool.disconnected(&peer, now);
                    run_actions(p, actions, now, &|peer: &PeerId| protected.contains(peer));
                }
            }
        }
        PuppetEvent::NickRejected { peer, numeric, .. } => {
            info!(peer = %peer, numeric = %numeric, "puppet: nick rejected at registration");
            if numeric == "433" {
                // The nick IS the account: nothing to tail. The account's
                // previous connection has not left the server yet (a reuse
                // the wait did not cover, or an eviction's QUIT in flight).
                // Keep the lease, cancel this attempt directly — not via
                // `run_actions`, whose `Cancel` arm would free the slot —
                // and ask again on the backoff schedule.
                p.exec.execute(PoolAction::Cancel { peer: peer.clone() });
                let actions = p.pool.disconnected(&peer, now);
                run_actions(p, actions, now, &|peer: &PeerId| protected.contains(peer));
            } else {
                let actions = p.pool.nick_rejected(&peer, &numeric, now);
                run_actions(p, actions, now, &|peer: &PeerId| protected.contains(peer));
            }
        }
        PuppetEvent::Ended {
            peer,
            attempt,
            nick,
            why,
            confirmed: _,
        } => {
            // A LIVE attempt's end is a disconnect the pool has not decided; a
            // QUITTING attempt's end completes a Quit the pool issued.
            let live = p.exec.is_live(&peer, attempt);
            p.exec.forget(&peer, attempt);
            if p.exec.has_live(&peer) {
                debug!(peer = %peer, attempt, "puppet: a superseded connection ended; the lease is the live one's");
            } else if nick.is_some() {
                // Registered as the account at some point, so the server may
                // still hold it: the slot waits for that departure to be read
                // here — unless no member is listed under the account, in
                // which case there is no QUIT to wait for.
                match p.slots.account_of(&peer).map(str::to_string) {
                    Some(account) if session.membership.any_member_attributed(&account) => {
                        p.slots.release_waiting(&peer, now);
                        debug!(peer = %peer, %account, "puppet: connection ended; the slot waits for its QUIT to be read here");
                    }
                    Some(account) => {
                        p.slots.release(&peer);
                        debug!(peer = %peer, %account, "puppet: connection ended; nothing listed under the account, slot freed");
                    }
                    // Evicted: the lease moved to the newcomer at grant time.
                    None => {}
                }
            } else if let Some(account) = p.slots.release(&peer) {
                debug!(peer = %peer, %account, "puppet: connection ended before registering; slot freed");
            }
            if live {
                let actions = p.pool.disconnected(&peer, now);
                run_actions(p, actions, now, &|peer: &PeerId| protected.contains(peer));
                // Said aloud once per run of failures — the loss that starts
                // the pool backing off — and at debug for the retries after.
                let state = p.pool.state_of(&peer);
                if matches!(state, Some(PuppetState::BackingOff { attempt: 1, .. })) {
                    warn!(peer = %peer, why = %why, registered = nick.is_some(), "puppet: connection lost; backing off");
                } else {
                    debug!(peer = %peer, why = %why, registered = nick.is_some(), ?state, "puppet: connection lost again");
                }
            } else {
                debug!(peer = %peer, why = %why, "puppet: connection ended as told");
            }
        }
        PuppetEvent::Renamed {
            peer,
            attempt,
            from,
            to,
        } => {
            // The puppet's own word is the only source for its nick.
            if !p.exec.is_live(&peer, attempt) {
                debug!(peer = %peer, attempt, "puppet: rename report from a connection that is not the live one; ignored");
                return;
            }
            if p.pool.resolve(&from) != Some(&peer) {
                debug!(peer = %peer, from = %from, to = %to, "puppet: rename report behind the pool; ignored");
                return;
            }
            match p.pool.renamed(&peer, &to) {
                Some(actions) => {
                    info!(peer = %peer, from = %from, to = %to, collided = !actions.is_empty(), "puppet: renamed by the server");
                    for a in actions {
                        p.exec.execute(a);
                    }
                }
                None => debug!(peer = %peer, "puppet: rename for an unregistered peer ignored"),
            }
        }
        PuppetEvent::Line { peer, line, .. } => {
            let msg = IrcMessage::parse(&line);
            if JOIN_REFUSED.contains(&msg.command.as_str()) {
                // The puppet's own JOIN was refused: registered, holding a
                // slot, and in no channel — not left standing (invariant 7).
                // Quit it, and the pool backs off to dial again; a channel
                // that keeps refusing is loud once per attempt.
                let channel = msg.params.get(1).cloned().unwrap_or_default();
                warn!(peer = %peer, numeric = %msg.command, %channel, "puppet: JOIN refused; quit, to dial again on the backoff schedule");
                if let Some(nick) = p.pool.nick_of(&peer).map(str::to_string) {
                    p.exec.execute(PoolAction::Quit {
                        peer: peer.clone(),
                        nick,
                    });
                    let actions = p.pool.disconnected(&peer, now);
                    run_actions(p, actions, now, &|peer: &PeerId| protected.contains(peer));
                }
                return;
            }
            let own = p.pool.nick_of(&peer).unwrap_or("").to_string();
            if let FanIn::Route { via: Some(_), .. } =
                puppets::classify(puppets::Source::Puppet(&peer), &msg, &own, cm)
            {
                // A private line to a puppet. Routing it to the peer is the
                // next increment; until then the sender is told so, by the
                // puppet itself, rather than left talking to a nick that
                // never answers.
                let sender = prefix_nick(msg.prefix.as_deref());
                if msg.command == "PRIVMSG" && !sender.is_empty() {
                    let notice = format!(
                        "NOTICE {sender} :{own} does not take private messages yet; say `{peer}: ...` in {lobby}, or write to {gateway}"
                    );
                    if !p.exec.command(&peer, PuppetCommand::Send(notice)) {
                        debug!(peer = %peer, "puppet: could not queue the not-yet notice");
                    }
                }
                debug!(peer = %peer, "puppet: private line not routed (routing lands in 2b-ii)");
            }
            p.exec.count_dropped_line();
        }
    }
}

/// Every puppet goes with the main connection, in one bounded step for the
/// whole pool: every registered puppet is sent QUIT, every attempt in flight
/// cancelled, the quitting tasks awaited together under the configured
/// grace (plus a second), and whatever is still open then is aborted. The
/// stats are said, and the attempt budget handed back for the next session.
async fn teardown_puppets(session: &mut Session, puppet_budget: &mut AttemptBudget) {
    let Some(mut p) = session.puppets.take() else {
        return;
    };
    let registered = p.pool.registered_count();
    for a in p.pool.teardown() {
        p.exec.execute(a);
    }
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
        puppet_lines_unqueued = stats.lines_unqueued,
        attribution_overdue = session.attribution_overdue,
        reuse_waited_out = p.counters.reuse_waited_out,
        slot_account_unleased = p.counters.slot_account_unleased,
        pool_nick_behind = p.counters.pool_nick_behind,
        "puppet pool torn down"
    );
    *puppet_budget = p.pool.into_budget();
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

    use super::super::puppet_task::test_support::{next, read_until};
    use super::super::puppet_task::Connector;
    use crate::adapter::DEFAULT_CHANNELLEN;
    use crate::bridge::mesh_side::nats_watcher;
    use crate::config::{PuppetsConfig, SlotCredential};
    use crate::config::{SaslCreds, Secret};
    use crate::mapping::{channel_for, CaseMapping};
    use crate::transport::TlsTrust;
    use tokio::io::{AsyncWriteExt, BufReader};

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
            attribution_overdue: 0,
            puppets: None,
            process_started: Instant::now(),
            caps_settled: true,
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

    #[test]
    fn without_whox_a_pending_member_is_said_to_be_overdue_at_once() {
        // Puppets provisioned (a slot set told) on a server with no WHOX:
        // nobody can be asked, so a member a JOIN or a NAMES burst leaves
        // pending is counted and said right away, not left silent.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (mut writer, mut lines) = transport::LineWriter::scripted(64);
        assert!(session.membership.set_owned_accounts(["cc-1"]).is_empty());
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":srv 353 mu-gw = #mu :alice bob",
        )
        .unwrap();
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":srv 366 mu-gw #mu :End of /NAMES list",
        )
        .unwrap();
        assert_eq!(session.attribution_overdue, 2, "both NAMES-only members");
        on_irc_line(&mut session, &presence, &mut writer, ":carol!u@h JOIN #mu").unwrap();
        assert_eq!(
            session.attribution_overdue, 3,
            "a bare JOIN with nobody to ask"
        );
        assert!(
            !written(&mut lines).iter().any(|l| l.starts_with("WHO ")),
            "nothing to ask with"
        );
        assert!(session.membership.is_pending("carol") && !session.membership.is_present("carol"));
    }

    #[test]
    fn a_pending_member_is_asked_about_again_under_its_new_name() {
        // With WHOX: a bare JOIN asks for the one nick; a rename before the
        // answer would strand the member (the answer names the old nick), so
        // it is asked again under the new one; the pass ending with it still
        // pending counts it.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (mut writer, mut lines) = transport::LineWriter::scripted(64);
        let _ = session
            .reg
            .on_message(&IrcMessage::parse(":srv 005 mu-gw WHOX :are supported"));
        assert!(session.reg.has_whox());
        assert!(session.membership.set_owned_accounts(["cc-1"]).is_empty());
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        on_irc_line(&mut session, &presence, &mut writer, ":alice!u@h JOIN #mu").unwrap();
        let asked: Vec<String> = written(&mut lines)
            .into_iter()
            .filter(|l| l.starts_with("WHO alice "))
            .collect();
        assert_eq!(asked.len(), 1, "{asked:?}");
        on_irc_line(&mut session, &presence, &mut writer, ":alice!u@h NICK :bob").unwrap();
        let asked: Vec<String> = written(&mut lines)
            .into_iter()
            .filter(|l| l.starts_with("WHO bob "))
            .collect();
        assert_eq!(asked.len(), 1, "asked again under the new name: {asked:?}");
        assert_eq!(session.attribution_overdue, 0);
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":srv 315 mu-gw bob :End of WHO list",
        )
        .unwrap();
        assert_eq!(
            session.attribution_overdue, 1,
            "the pass ended with bob still pending"
        );
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":srv 315 mu-gw alice :End of WHO list",
        )
        .unwrap();
        assert_eq!(
            session.attribution_overdue, 1,
            "the old name is nobody's: nothing to count"
        );
    }

    // ───────────────── puppets: the wiring under *Identity by account* ──────

    fn with_puppets(
        s: &mut Session,
        cfg: PuppetsConfig,
        connector: Connector,
    ) -> mpsc::Receiver<PuppetEvent> {
        let (ev_tx, ev_rx) = mpsc::channel(64);
        let mut irc = irc_config();
        irc.tls = true;
        irc.puppets = cfg.clone();
        assert!(
            s.membership
                .set_owned_accounts(cfg.slot_accounts())
                .is_empty(),
            "told before any roster exists"
        );
        let pool = Pool::with_budget(
            cfg.clone(),
            32,
            s.isupport.casemapping,
            AttemptBudget::new(),
        );
        let exec = Executor::new(irc, connector, ev_tx, Duration::from_secs(5));
        s.puppets = Some(Puppetry {
            pool,
            exec,
            slots: slot_pool(&cfg),
            counters: PuppetCounters::default(),
        });
        ev_rx
    }

    async fn drive_until(
        session: &mut Session,
        events: &mut mpsc::Receiver<PuppetEvent>,
        is: impl Fn(&PuppetEvent) -> bool,
    ) -> PuppetEvent {
        loop {
            let ev = next(events).await;
            if is(&ev) {
                return ev;
            }
            on_puppet_event(session, ev);
        }
    }

    fn discover_abc(session: &mut Session) {
        session.discovery = Discovery::from_srv(HashMap::from([(
            "cc:abc".to_string(),
            "mu.agent.cc.abc.dm".to_string(),
        )]));
    }

    fn abc_and_def() -> Discovery {
        Discovery::from_srv(HashMap::from([
            ("cc:abc".to_string(), "mu.agent.cc.abc.dm".to_string()),
            ("cc:def".to_string(), "mu.agent.cc.def.dm".to_string()),
        ]))
    }

    const SLOT_PEMS: [(&str, &str); 2] = [
        (
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/slot-a.pem"),
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/slot-a.key.pem"),
        ),
        (
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/slot-b.pem"),
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/slot-b.key.pem"),
        ),
    ];

    /// A provisioned pool of `max` slots whose certificates live in a fresh
    /// directory, so a test can break one.
    fn provisioned(max: usize) -> PuppetsConfig {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        assert!(max <= SLOT_PEMS.len());
        let dir = std::env::temp_dir().join(format!(
            "mu-irc-slots-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for (i, (crt, key)) in SLOT_PEMS.iter().enumerate().take(max) {
            std::fs::copy(crt, dir.join(format!("cc-{}.crt", i + 1))).unwrap();
            std::fs::copy(key, dir.join(format!("cc-{}.key", i + 1))).unwrap();
        }
        PuppetsConfig {
            enabled: true,
            min_age_secs: 0,
            max,
            slot_certs_dir: Some(dir),
            slot_idle_secs: 30,
            departure_wait_secs: 60,
            connect_parallelism: 4,
            ..PuppetsConfig::default()
        }
    }

    /// The main connection as a provisioned pool needs it: the account
    /// capabilities negotiated and WHOX advertised.
    fn negotiate_accounts(session: &mut Session) {
        for line in [
            "CAP * LS :extended-join account-notify account-tag",
            "CAP * ACK :extended-join account-notify account-tag",
            ":srv 001 mu-gw :Welcome",
            ":srv 005 mu-gw WHOX :are supported by this server",
        ] {
            let _ = session.reg.on_message(&IrcMessage::parse(line));
        }
        session.isupport = session.reg.isupport();
        assert!(required_caps(&session.reg).is_ok());
    }

    /// A connector that hands the server half over like `scripted_connector`
    /// and records whether a credential was presented.
    fn slot_connector(
        hand: mpsc::UnboundedSender<(String, tokio::io::DuplexStream)>,
        presented: Arc<std::sync::atomic::AtomicBool>,
    ) -> Connector {
        Arc::new(move |irc: IrcConfig, credential: Option<SlotCredential>| {
            let hand = hand.clone();
            let presented = presented.clone();
            Box::pin(async move {
                presented.store(credential.is_some(), std::sync::atomic::Ordering::SeqCst);
                let (client, server) = tokio::io::duplex(8192);
                hand.send((irc.nick.clone(), server))
                    .map_err(|_| "no server".to_string())?;
                Ok(transport::spawn_connection(Box::new(client)))
            })
        })
    }

    /// Play the server through SASL EXTERNAL registration of `account`.
    async fn sasl_register(
        server: tokio::io::DuplexStream,
        account: &str,
    ) -> (
        BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    ) {
        let (rh, mut wh) = tokio::io::split(server);
        let mut r = BufReader::new(rh);
        read_until(&mut r, "NICK ").await;
        wh.write_all(b":srv CAP * LS :sasl\r\n").await.unwrap();
        read_until(&mut r, "CAP REQ").await;
        wh.write_all(b":srv CAP * ACK :sasl\r\n").await.unwrap();
        let auth = read_until(&mut r, "AUTHENTICATE ").await;
        assert!(auth.starts_with("AUTHENTICATE EXTERNAL"), "{auth}");
        wh.write_all(b"AUTHENTICATE +\r\n").await.unwrap();
        read_until(&mut r, "AUTHENTICATE ").await;
        wh.write_all(format!(":srv 903 {account} :Authentication successful\r\n").as_bytes())
            .await
            .unwrap();
        read_until(&mut r, "CAP END").await;
        wh.write_all(format!(":srv 001 {account} :Welcome\r\n").as_bytes())
            .await
            .unwrap();
        (r, wh)
    }

    struct Leased {
        session: Session,
        presence: mpsc::UnboundedSender<PresenceOp>,
        hand_rx: mpsc::UnboundedReceiver<(String, tokio::io::DuplexStream)>,
        presented: Arc<std::sync::atomic::AtomicBool>,
        events: mpsc::Receiver<PuppetEvent>,
        dir: std::path::PathBuf,
    }

    /// What every test starts from: a scripted session with a provisioned
    /// pool of `max` slots, the capabilities negotiated, and the ends a test
    /// drives it from.
    fn leased(max: usize) -> Leased {
        let Scripted { mut session, .. } = scripted_session(4);
        negotiate_accounts(&mut session);
        let (presence, _presence_rx) = mpsc::unbounded_channel();
        let (hand_tx, hand_rx) = mpsc::unbounded_channel();
        let presented = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cfg = provisioned(max);
        let dir = cfg.slot_certs_dir.clone().unwrap();
        let events = with_puppets(
            &mut session,
            cfg,
            slot_connector(hand_tx, presented.clone()),
        );
        Leased {
            session,
            presence,
            hand_rx,
            presented,
            events,
            dir,
        }
    }

    /// `cc:abc` discovered, dialled as `cc-1`, registered — and, with `join`,
    /// seen joining the lobby on the main connection WITH its account.
    async fn registered_slot_puppet(
        t: &mut Leased,
        writer: &mut transport::LineWriter,
        join: bool,
    ) -> (
        BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    ) {
        discover_abc(&mut t.session);
        puppets_tick(&mut t.session);
        let (nick, server) = t.hand_rx.recv().await.unwrap();
        assert_eq!(
            nick, "cc-1",
            "a leased puppet registers AS its slot account"
        );
        let (r, wh) = sasl_register(server, "cc-1").await;
        let ev = next(&mut t.events).await;
        assert!(
            matches!(&ev, PuppetEvent::Registered { nick, .. } if nick == "cc-1"),
            "{ev:?}"
        );
        on_puppet_event(&mut t.session, ev);
        if join {
            on_irc_line(
                &mut t.session,
                &t.presence,
                writer,
                ":cc-1!u@h JOIN #mu cc-1 :puppet",
            )
            .unwrap();
            assert!(
                t.session.membership.is_owned("cc-1") && !t.session.membership.is_present("cc-1")
            );
        }
        (r, wh)
    }

    fn slots_of(session: &Session) -> &Slots {
        &session.puppets.as_ref().unwrap().slots
    }

    fn counters_of(session: &Session) -> PuppetCounters {
        session.puppets.as_ref().unwrap().counters
    }

    fn pool_nick(session: &Session) -> Option<&str> {
        session
            .puppets
            .as_ref()
            .unwrap()
            .pool
            .nick_of(&PeerId::parse("cc:abc"))
    }

    fn advance_puppet_clock(session: &mut Session, secs: u64) {
        session.process_started = session
            .process_started
            .checked_sub(Duration::from_secs(secs))
            .expect("the host has been up longer than the jump");
    }

    async fn ended(t: &mut Leased) {
        let ev = drive_until(&mut t.session, &mut t.events, |ev| {
            matches!(ev, PuppetEvent::Ended { .. })
        })
        .await;
        on_puppet_event(&mut t.session, ev);
    }

    #[test]
    fn puppets_refuse_a_server_without_the_account_capabilities() {
        // Rule 1: the account on every line is the identity, so the pool
        // needs the capabilities negotiated — listed is not acknowledged —
        // and WHOX advertised. WHOX arrives with ISUPPORT, after 001, so the
        // verdict waits for the end of the welcome burst. Each one missing
        // is named.
        let irc = irc_config();
        let registered = |ack: &str| {
            let (mut reg, _) = Registration::start(&irc, SystemClock).unwrap();
            for line in [
                "CAP * LS :extended-join account-notify account-tag",
                &format!("CAP * ACK :{ack}"),
                ":srv 001 mu-gw :Welcome",
            ] {
                let _ = reg.on_message(&IrcMessage::parse(line));
            }
            reg
        };
        let reg = registered("extended-join");
        assert_eq!(
            required_caps(&reg).unwrap_err(),
            "account-tag, account-notify, WHOX",
            "acknowledged counts; listed does not"
        );
        let mut reg = registered("extended-join account-notify account-tag");
        assert_eq!(
            required_caps(&reg).unwrap_err(),
            "WHOX",
            "not yet: ISUPPORT follows 001"
        );
        assert!(!welcome_burst_over(":srv 001 mu-gw :Welcome"));
        let _ = reg.on_message(&IrcMessage::parse(":srv 005 mu-gw WHOX :are supported"));
        assert!(welcome_burst_over(":srv 376 mu-gw :End of /MOTD command"));
        assert!(welcome_burst_over(":srv 422 mu-gw :MOTD File is missing"));
        assert!(
            required_caps(&reg).is_ok(),
            "judged after the burst: present"
        );
        let _ = reg.on_message(&IrcMessage::parse("CAP * DEL :account-tag"));
        assert_eq!(
            required_caps(&reg).unwrap_err(),
            "account-tag",
            "withdrawn mid-session"
        );
    }

    #[tokio::test]
    async fn a_discovered_peer_leases_a_slot_dials_as_it_and_is_ours_on_the_roster() {
        let mut t = leased(1);
        let (mut writer, lines) = transport::LineWriter::scripted(64);
        on_irc_line(
            &mut t.session,
            &t.presence,
            &mut writer,
            ":mu-gw!u@h JOIN #mu",
        )
        .unwrap();
        let (mut r, wh) = registered_slot_puppet(&mut t, &mut writer, true).await;
        assert!(
            t.presented.load(std::sync::atomic::Ordering::SeqCst),
            "the slot's certificate was presented"
        );
        assert_eq!(slots_of(&t.session).free(), 0);
        let join = read_until(&mut r, "JOIN ").await;
        assert!(join.contains("#mu"), "{join}");
        assert_eq!(pool_nick(&t.session), Some("cc-1"));
        drop(wh);
        drop(lines);
    }

    #[tokio::test]
    async fn a_puppets_end_returns_its_slot_only_after_its_quit_is_read() {
        // Rule 4: listed under the account when the connection ends, the slot
        // waits for that member's QUIT here; the QUIT frees it.
        let mut t = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(
            &mut t.session,
            &t.presence,
            &mut writer,
            ":mu-gw!u@h JOIN #mu",
        )
        .unwrap();
        let (r, wh) = registered_slot_puppet(&mut t, &mut writer, true).await;
        drop(wh);
        drop(r);
        ended(&mut t).await;
        assert_eq!(slots_of(&t.session).waiting(), vec!["cc-1"]);
        assert_eq!(slots_of(&t.session).free(), 0, "waiting is not free");
        assert!(
            t.session.membership.is_owned("cc-1"),
            "still ours on the roster"
        );
        on_irc_line(
            &mut t.session,
            &t.presence,
            &mut writer,
            ":cc-1!u@h QUIT :bye",
        )
        .unwrap();
        assert_eq!(slots_of(&t.session).free(), 1, "the QUIT frees it");
        assert!(slots_of(&t.session).waiting().is_empty());
    }

    #[tokio::test]
    async fn a_puppet_listed_nowhere_frees_its_slot_at_once() {
        // No member under the account on this connection: there is no QUIT
        // to wait for.
        let mut t = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        let (r, wh) = registered_slot_puppet(&mut t, &mut writer, false).await;
        drop(wh);
        drop(r);
        ended(&mut t).await;
        assert_eq!(slots_of(&t.session).free(), 1);
        assert!(slots_of(&t.session).waiting().is_empty());
    }

    #[tokio::test]
    async fn a_refused_dial_frees_the_slot_at_once_and_backs_off() {
        // The credential is gone: nothing is opened, the slot is free, the
        // attempt refunded, and the peer backs off to ask again.
        let mut t = leased(1);
        std::fs::remove_file(t.dir.join("cc-1.key")).unwrap();
        discover_abc(&mut t.session);
        puppets_tick(&mut t.session);
        assert!(t.hand_rx.try_recv().is_err(), "nothing was dialled");
        assert_eq!(slots_of(&t.session).free(), 1);
        let p = t.session.puppets.as_ref().unwrap();
        assert!(
            matches!(
                p.pool.state_of(&PeerId::parse("cc:abc")),
                Some(PuppetState::BackingOff { .. })
            ),
            "{:?}",
            p.pool.state_of(&PeerId::parse("cc:abc"))
        );
        let now = t.session.puppet_now_ms();
        assert_eq!(
            t.session
                .puppets
                .as_mut()
                .unwrap()
                .pool
                .attempts_in_window(now),
            0,
            "no socket was opened: the attempt is refunded"
        );
    }

    #[tokio::test]
    async fn the_main_connections_nick_never_moves_the_pool() {
        // Rule 3: the pool's nick for a puppet comes from the puppet's own
        // connection only.
        let mut t = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(
            &mut t.session,
            &t.presence,
            &mut writer,
            ":mu-gw!u@h JOIN #mu",
        )
        .unwrap();
        let (mut r, mut wh) = registered_slot_puppet(&mut t, &mut writer, true).await;
        let _ = read_until(&mut r, "JOIN ").await;
        on_irc_line(
            &mut t.session,
            &t.presence,
            &mut writer,
            ":cc-1!u@h NICK :cc-1x",
        )
        .unwrap();
        assert_eq!(
            pool_nick(&t.session),
            Some("cc-1"),
            "the main connection's NICK moved the member, not the pool"
        );
        assert!(
            t.session.membership.is_owned("cc-1x"),
            "the account travelled with the member"
        );
        wh.write_all(b":cc-1!u@h NICK :cc-1x\r\n").await.unwrap();
        let ev = drive_until(&mut t.session, &mut t.events, |ev| {
            matches!(ev, PuppetEvent::Renamed { .. })
        })
        .await;
        on_puppet_event(&mut t.session, ev);
        assert_eq!(
            pool_nick(&t.session),
            Some("cc-1x"),
            "the puppet's own report moves the pool"
        );
    }

    #[tokio::test]
    async fn an_idle_lease_is_evicted_for_a_newcomer_and_its_puppet_is_quit_first() {
        let mut t = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(
            &mut t.session,
            &t.presence,
            &mut writer,
            ":mu-gw!u@h JOIN #mu",
        )
        .unwrap();
        let (mut r, wh) = registered_slot_puppet(&mut t, &mut writer, true).await;
        let _ = read_until(&mut r, "JOIN ").await;
        // Listed the whole time (presence, not activity); idle past the window.
        advance_puppet_clock(&mut t.session, 31);
        t.session.discovery = abc_and_def();
        puppets_tick(&mut t.session);
        let quit = read_until(&mut r, "QUIT").await;
        assert!(quit.starts_with("QUIT"), "{quit}");
        let (nick, _server) = tokio::time::timeout(Duration::from_secs(5), t.hand_rx.recv())
            .await
            .expect("the newcomer dials")
            .unwrap();
        assert_eq!(
            nick, "cc-1",
            "the newcomer registers as the account it took"
        );
        let slots = slots_of(&t.session);
        assert_eq!(slots.evictions(), 1, "counted, not silent");
        assert_eq!(slots.account_of(&PeerId::parse("cc:def")), Some("cc-1"));
        assert!(
            t.session.membership.is_owned("cc-1"),
            "the evicted puppet is ours until its QUIT, by the account"
        );
        drop(wh);
    }

    #[tokio::test]
    async fn a_433_on_a_slot_account_keeps_the_lease_and_asks_again() {
        // The nick IS the account: a 433 means its previous connection has
        // not left the server yet. Not tailed, not freed — asked again.
        let mut t = leased(1);
        discover_abc(&mut t.session);
        puppets_tick(&mut t.session);
        let (_nick, server) = t.hand_rx.recv().await.unwrap();
        let (rh, mut wh) = tokio::io::split(server);
        let mut r = BufReader::new(rh);
        read_until(&mut r, "NICK ").await;
        wh.write_all(b":srv 433 * cc-1 :Nickname is already in use\r\n")
            .await
            .unwrap();
        let ev = drive_until(&mut t.session, &mut t.events, |ev| {
            matches!(ev, PuppetEvent::NickRejected { .. })
        })
        .await;
        on_puppet_event(&mut t.session, ev);
        assert_eq!(slots_of(&t.session).free(), 0, "the lease is kept");
        assert_eq!(
            slots_of(&t.session).account_of(&PeerId::parse("cc:abc")),
            Some("cc-1")
        );
        let p = t.session.puppets.as_ref().unwrap();
        assert!(matches!(
            p.pool.state_of(&PeerId::parse("cc:abc")),
            Some(PuppetState::BackingOff { .. })
        ));
        advance_puppet_clock(&mut t.session, 10);
        puppets_tick(&mut t.session);
        let (nick, _server) = tokio::time::timeout(Duration::from_secs(5), t.hand_rx.recv())
            .await
            .expect("asked again")
            .unwrap();
        assert_eq!(nick, "cc-1", "as the same account");
    }

    #[tokio::test]
    async fn a_registered_puppet_whose_join_cannot_be_queued_is_quit_and_dialled_again() {
        // Invariant 7: a puppet that cannot be told to JOIN is not left
        // standing. The queue is filled before the session learns of the
        // registration; the task cannot drain it without being polled.
        let mut t = leased(1);
        discover_abc(&mut t.session);
        puppets_tick(&mut t.session);
        let (_nick, server) = t.hand_rx.recv().await.unwrap();
        let (mut r, wh) = sasl_register(server, "cc-1").await;
        let ev = next(&mut t.events).await;
        assert!(matches!(ev, PuppetEvent::Registered { .. }), "{ev:?}");
        let abc = PeerId::parse("cc:abc");
        {
            let p = t.session.puppets.as_mut().unwrap();
            let mut queued = 0;
            while p.exec.command(&abc, PuppetCommand::Send("PING :x".into())) {
                queued += 1;
                assert!(queued < 10_000, "the command queue is bounded");
            }
        }
        on_puppet_event(&mut t.session, ev);
        let p = t.session.puppets.as_ref().unwrap();
        assert!(
            matches!(p.pool.state_of(&abc), Some(PuppetState::BackingOff { .. })),
            "{:?}",
            p.pool.state_of(&abc)
        );
        let quit = read_until(&mut r, "QUIT").await;
        assert!(quit.starts_with("QUIT"), "{quit}");
        drop(wh);
        drop(r);
        ended(&mut t).await;
        advance_puppet_clock(&mut t.session, 10);
        puppets_tick(&mut t.session);
        let (nick, _server) = tokio::time::timeout(Duration::from_secs(5), t.hand_rx.recv())
            .await
            .expect("dialled again")
            .unwrap();
        assert_eq!(nick, "cc-1");
    }

    #[tokio::test]
    async fn what_stayed_unlikely_is_counted_and_said() {
        // Rule 5: none of these changes behaviour; each is a number.
        let mut t = leased(2);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(
            &mut t.session,
            &t.presence,
            &mut writer,
            ":mu-gw!u@h JOIN #mu",
        )
        .unwrap();
        // A slot account on the roster that nothing of ours leased.
        on_irc_line(
            &mut t.session,
            &t.presence,
            &mut writer,
            ":cc-2!u@h JOIN #mu cc-2 :puppet",
        )
        .unwrap();
        assert_eq!(counters_of(&t.session).slot_account_unleased, 1);
        assert!(t.session.membership.is_owned("cc-2"), "ours all the same");
        // The account's puppet is listed under a nick the pool has not learned.
        let (mut r, wh) = registered_slot_puppet(&mut t, &mut writer, false).await;
        let _ = read_until(&mut r, "JOIN ").await;
        on_irc_line(
            &mut t.session,
            &t.presence,
            &mut writer,
            ":cc-1y!u@h JOIN #mu cc-1 :puppet",
        )
        .unwrap();
        assert_eq!(counters_of(&t.session).pool_nick_behind, 1);
        // A member still unanswered when the WHOX pass ends.
        on_irc_line(
            &mut t.session,
            &t.presence,
            &mut writer,
            ":bob!u@h JOIN #mu",
        )
        .unwrap();
        assert!(t.session.membership.is_pending("bob"));
        on_irc_line(
            &mut t.session,
            &t.presence,
            &mut writer,
            ":srv 315 mu-gw #mu :End of WHO list",
        )
        .unwrap();
        assert_eq!(t.session.attribution_overdue, 1);
        // A returned slot whose QUIT never comes.
        drop(wh);
        drop(r);
        ended(&mut t).await;
        assert_eq!(slots_of(&t.session).waiting(), vec!["cc-1"]);
        advance_puppet_clock(&mut t.session, 61);
        t.session.discovery = Discovery::from_srv(HashMap::new());
        puppets_tick(&mut t.session);
        assert_eq!(counters_of(&t.session).reuse_waited_out, 1);
        assert!(slots_of(&t.session).waiting().is_empty());
    }

    #[tokio::test]
    async fn a_private_line_to_a_puppet_is_answered_by_the_puppet() {
        // Routing a private line to the peer is the next increment; until
        // then the puppet says so to the sender, rather than sit silent
        // under a nick that looks like it listens.
        let mut t = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        let (mut r, mut wh) = registered_slot_puppet(&mut t, &mut writer, false).await;
        let _ = read_until(&mut r, "JOIN ").await;
        wh.write_all(b":bob!u@h PRIVMSG cc-1 :hi there\r\n")
            .await
            .unwrap();
        let ev = drive_until(
            &mut t.session,
            &mut t.events,
            |ev| matches!(ev, PuppetEvent::Line { line, .. } if line.contains("hi there")),
        )
        .await;
        on_puppet_event(&mut t.session, ev);
        let notice = read_until(&mut r, "NOTICE bob").await;
        assert!(
            notice.contains("does not take private messages yet")
                && notice.contains("`cc:abc: ...` in #mu"),
            "{notice}"
        );
    }

    #[tokio::test]
    async fn teardown_quits_every_registered_puppet_and_hands_the_budget_back() {
        let mut t = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        let (mut r, wh) = registered_slot_puppet(&mut t, &mut writer, false).await;
        let _ = read_until(&mut r, "JOIN ").await;
        let mut budget = AttemptBudget::new();
        let started = Instant::now();
        teardown_puppets(&mut t.session, &mut budget).await;
        assert!(t.session.puppets.is_none());
        let quit = read_until(&mut r, "QUIT").await;
        assert!(quit.starts_with("QUIT"), "{quit}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "bounded by the grace: {:?}",
            started.elapsed()
        );
        let now = t.session.puppet_now_ms();
        assert_eq!(budget.in_window(now), 1, "the attempt history came back");
        drop(wh);
    }

    #[tokio::test]
    async fn no_puppet_is_dialled_before_the_capabilities_are_judged() {
        // The judgement waits for the end of the welcome burst (or the
        // registration deadline); until then a discovery tick dials nothing.
        let mut t = leased(1);
        t.session.caps_settled = false;
        discover_abc(&mut t.session);
        puppets_tick(&mut t.session);
        // A dial is a spawned task: let one run before asserting it did not.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            t.hand_rx.try_recv().is_err(),
            "not judged yet: nothing dialled"
        );
        t.session.caps_settled = true;
        puppets_tick(&mut t.session);
        let (nick, _server) = tokio::time::timeout(Duration::from_secs(5), t.hand_rx.recv())
            .await
            .expect("judged: dialled")
            .unwrap();
        assert_eq!(nick, "cc-1");
    }

    #[tokio::test]
    async fn a_puppet_whose_join_is_refused_is_quit_and_dialled_again() {
        // The server refuses the puppet's JOIN (a +l lobby, say): registered
        // and holding a slot but in no channel is not a state to stand in.
        let mut t = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        let (mut r, mut wh) = registered_slot_puppet(&mut t, &mut writer, false).await;
        let _ = read_until(&mut r, "JOIN ").await;
        wh.write_all(b":srv 471 cc-1 #mu :Cannot join channel (+l)\r\n")
            .await
            .unwrap();
        let ev = drive_until(
            &mut t.session,
            &mut t.events,
            |ev| matches!(ev, PuppetEvent::Line { line, .. } if line.contains(" 471 ")),
        )
        .await;
        on_puppet_event(&mut t.session, ev);
        let abc = PeerId::parse("cc:abc");
        let p = t.session.puppets.as_ref().unwrap();
        assert!(
            matches!(p.pool.state_of(&abc), Some(PuppetState::BackingOff { .. })),
            "quit, and backing off: {:?}",
            p.pool.state_of(&abc)
        );
        let quit = read_until(&mut r, "QUIT").await;
        assert!(quit.starts_with("QUIT"), "{quit}");
        drop(wh);
        drop(r);
        ended(&mut t).await;
        advance_puppet_clock(&mut t.session, 10);
        puppets_tick(&mut t.session);
        let (nick, _server) = tokio::time::timeout(Duration::from_secs(5), t.hand_rx.recv())
            .await
            .expect("dialled again")
            .unwrap();
        assert_eq!(nick, "cc-1");
    }
}
