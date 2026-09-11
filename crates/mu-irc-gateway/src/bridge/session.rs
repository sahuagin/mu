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
use crate::mapping::fold_nick;
use crate::membership::{ChannelEffect, ChannelReconciler, HumanEffect, Membership};
use crate::outbound::{OutDrop, OutEnv, Outbound, OutboundDecision, RefuseReason};
use crate::routing::{RouteDecision, RouteEnv, Router};
use crate::transport::{self, Connection, FromServer, LineWriter, SendError};

use super::mesh_side::{
    discard_stale_discovery, discard_stale_dms, publish_worker, Discovery, LinkState, MeshInputs,
    MeshSide, PresenceOp, PublishJob, PUBLISH_QUEUE,
};

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
/// is sent ([`SessionError::Fatal`]). An IRC server that is down is not one of
/// those: it is retried with backoff, because that is what a gateway to a chat
/// network has to survive. The distinction is the adapter's and the transport's,
/// not a guess made here — see [`SessionError`].
pub async fn run(config: GatewayConfig, mut shutdown: watch::Receiver<bool>) -> Result<()> {
    let (mesh_side, mut inputs) = MeshSide::start(&config).await?;

    let mut backoff = Backoff::new();
    // A failure no reconnect can fix. Recorded rather than returned on the spot,
    // because the mesh presence this gateway fronts must be released either way.
    let mut fatal: Option<anyhow::Error> = None;
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
    /// Folded human nick → folded channel they were last addressed toward. The
    /// mesh→IRC side reads it; the IRC→mesh side writes it.
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
}

impl Session {
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
    transport::connect(&irc.server, irc.tls, CONNECT_TIMEOUT)
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

    let mut session = Session {
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
                    quit(&mut writer, &mut inbound).await;
                    break Stop::Shutdown;
                }
            }
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
        } => {
            if let Some(update) = memory {
                session.remembered.insert(update.human, update.channel);
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
                // Every destination vanished between the sweep and now.
                session.refused_out += 1;
                return notify(
                    writer,
                    reply_to,
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
        RefuseReason::NoDestinations => "no agents are on the mesh right now".into(),
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

    const LOBBY: &str = "#mu";

    /// The IRC side of a session that never opens a socket.
    fn irc_config() -> IrcConfig {
        IrcConfig {
            server: "irc.invalid:6667".into(),
            tls: false,
            nick: "mu-gw".into(),
            sasl: None,
            channel_prefix: "#".into(),
            lobby: LOBBY.into(),
            observe_agent_dms: true,
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
}
