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
use crate::config::PuppetsConfig;
use crate::config::{GatewayConfig, IrcConfig};
use crate::framing::{frame_privmsg, FrameParams};
use crate::mapping::{channel_for, fold_nick, CaseMapping};
use crate::membership::{Attribution, ChannelEffect, ChannelReconciler, HumanEffect, Membership};
use crate::outbound::{
    Answer, CommandReply, MemoryDestination, OutDrop, OutEnv, Outbound, OutboundDecision,
    RefuseReason, NO_AGENTS, USAGE,
};
use crate::puppets::{self, AttemptBudget, FanIn, Pool, PoolAction, PuppetState};
use crate::routing::{RouteDecision, RouteEnv, Router};
use crate::slots::{Grant, Slots};
use crate::transport::{self, Connection, FromServer, LineWriter, SendError};

use super::mesh_side::{
    discard_stale_discovery, discard_stale_dms, publish_worker, Discovery, LinkState, MeshInputs,
    MeshSide, PresenceOp, PublishJob, PUBLISH_QUEUE,
};
use super::puppet_io::{self, Executor, PuppetCommand, PuppetEvent};
use super::puppet_wire::pong;

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
}

/// The puppet half of a session: the pool decides, the executor does.
struct Puppetry {
    pool: Pool,
    exec: Executor,
    /// The leased slot accounts — `Some` when `[irc.puppets] slot_certs_dir`
    /// provisions a pool, in which case every puppet connects AS a leased
    /// account with its certificate and membership is told the leased set;
    /// `None` for the unprovisioned pool, whose puppets connect anonymously
    /// under the pool's label nicks exactly as before.
    slots: Option<Slots>,
    /// The executor attempt each registered peer's nick is held by, and
    /// where its renames stand. Set at `Registered`, dropped at `Ended`.
    holder: HashMap<PeerId, Holder>,
    /// Departing puppets the MAIN connection has not yet seen leave: the
    /// nick stays owned, and a provisioned pool's lease stays held, until
    /// that nick's QUIT here or the departure window. Ownership dropped
    /// before the QUIT arrives would front the gateway's own puppet as a
    /// human (R1; mu-irc-remote-session-zgbdz.4.1). Entered when the pool
    /// issues a `Quit`, and at an `Ended` whose puppet is still listed as
    /// ours. A list, not a map by folded spelling: a CASEMAPPING change can
    /// fold two peers' spellings together while both departures are still
    /// unseen.
    pending_release: Vec<PendingRelease>,
}

/// A departure the main connection has yet to see.
struct PendingRelease {
    peer: PeerId,
    /// The connection that departed. A peer gone, back, and gone again
    /// before the main connection saw the first QUIT has two departures
    /// pending — one QUIT each, and the second connection's JOIN between.
    attempt: u64,
    /// The nick as the server spelled it: held in membership's owned set.
    nick: String,
    /// `nick` folded under the casemapping in force — what a QUIT or NICK
    /// echo is matched by. Re-keyed when the mapping changes.
    key: String,
    /// The account the puppet is on the roster under, in a provisioned
    /// pool: its own lease, or the one an eviction moved to a newcomer.
    /// Held, whoever holds it, until this departure is seen.
    account: Option<String>,
    deadline_ms: u64,
    /// How many departures from this spelling are yet to be seen: a rename
    /// cycle that revisits a spelling leaves it more than once, and the
    /// spelling stays held until the LAST of them is echoed.
    holds: u32,
}

/// Hold `nick` for `peer` until the main connection sees it leave: one more
/// departure from that spelling, or a fresh hold. A spelling held for a
/// different peer too is held for both — each departure is its own.
fn hold(p: &mut Puppetry, peer: &PeerId, attempt: u64, nick: &str, now_ms: u64, cm: CaseMapping) {
    let account = account_backing(p, peer);
    hold_as(p, peer, attempt, nick, now_ms, cm, account);
}

/// [`hold`], under `account`: the one an eviction has just moved away from
/// `peer`, which `peer`'s own lease no longer names.
fn hold_as(
    p: &mut Puppetry,
    peer: &PeerId,
    attempt: u64,
    nick: &str,
    now_ms: u64,
    cm: CaseMapping,
    account: Option<String>,
) {
    let wait_ms = p.pool.config().departure_wait_secs.saturating_mul(1000);
    let key = fold_nick(nick, cm);
    match p
        .pending_release
        .iter_mut()
        .find(|pr| pr.key == key && pr.peer == *peer && pr.attempt == attempt)
    {
        Some(pr) => {
            pr.holds += 1;
            pr.deadline_ms = now_ms + wait_ms;
            if pr.account.is_none() {
                pr.account = account;
            }
        }
        None => p.pending_release.push(PendingRelease {
            peer: peer.clone(),
            attempt,
            nick: nick.to_string(),
            key,
            account,
            deadline_ms: now_ms + wait_ms,
            holds: 1,
        }),
    }
}

/// The account `peer` is on the roster under: its lease, or — the lease
/// moved to a newcomer by an eviction — the one its departure is already
/// held under. `None` in the unprovisioned pool.
fn account_backing(p: &Puppetry, peer: &PeerId) -> Option<String> {
    p.slots
        .as_ref()
        .and_then(|s| s.account_of(peer))
        .map(str::to_string)
        .or_else(|| {
            p.pending_release
                .iter()
                .find(|pr| pr.peer == *peer)
                .and_then(|pr| pr.account.clone())
        })
}

/// Whether `account`, leased to `peer`, still backs a departure the main
/// connection has not seen: `peer`'s own last connection, or the puppet an
/// eviction took the account from. Either is on the roster under that
/// account until its QUIT, and fronted as a human if the account is let go
/// first (R1).
fn backs_a_departure(p: &Puppetry, peer: &PeerId, account: &str) -> bool {
    p.pending_release
        .iter()
        .any(|pr| pr.peer == *peer || pr.account.as_deref() == Some(account))
}

/// The hold a departure of `nick` (folded: `key`) settles: one under that
/// exact spelling, else one that folds to it — two holds fold alike only
/// once a CASEMAPPING change has merged their spellings. Of several, the
/// earliest connection's: QUITs arrive in the order the connections left.
fn held_index(p: &Puppetry, key: &str, nick: &str) -> Option<usize> {
    let earliest = |exact: bool| {
        p.pending_release
            .iter()
            .enumerate()
            .filter(|(_, pr)| pr.key == key && (!exact || pr.nick == nick))
            .min_by_key(|(_, pr)| pr.attempt)
            .map(|(i, _)| i)
    };
    earliest(true).or_else(|| earliest(false))
}

/// The connection a registered peer's nick is held by, and the renames
/// applied to the pool for it counted against the reports received from
/// it: the Nth report is stale once the wire has applied N renames, and
/// spelling cannot tell (after A→B→A a late A→B names the nick the pool
/// holds again, and applying it would evict a human who took B since).
struct Holder {
    attempt: u64,
    /// Renames applied to the pool for this attempt, from the wire's NICK
    /// or from the puppet's own report, whichever came first.
    applied: u32,
    /// Rename reports received from this attempt.
    reports: u32,
    /// Renames applied from the puppet's own reports whose echo on the main
    /// connection has not arrived yet. A NICK matching one is that echo —
    /// consumed, not replayed, even in a cycle that revisits a spelling. One
    /// matching none is a rename the main connection saw first, or one the
    /// puppet reported before any shared channel (the two sources do not
    /// see equal sequences), and it applies.
    unechoed: Vec<Unechoed>,
}

/// A rename applied from the puppet's own report, its echo not yet seen.
/// Wire spellings, compared under the casemapping in force when a NICK
/// arrives (a CASEMAPPING change in between needs no re-key), and the
/// window within which the echo comes if it comes at all: a rename before
/// any shared channel is never echoed, and past the window it no longer
/// passes for the echo of a later rename over the same spellings.
struct Unechoed {
    from: String,
    to: String,
    deadline_ms: u64,
}

impl Holder {
    fn new(attempt: u64) -> Self {
        Holder {
            attempt,
            applied: 0,
            reports: 0,
            unechoed: Vec::new(),
        }
    }
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
                holder: HashMap::new(),
                slots: slot_pool(&irc.puppets),
                pending_release: Vec::new(),
            }),
        )
    } else {
        (None, None)
    };

    let mut session = Session {
        puppets,
        process_started,
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
    // A provisioned pool's puppets are logged in as their slot accounts, so
    // membership may read the server's "not logged in" as "not ours"
    // (`Membership::is_puppet`); an unprovisioned pool's puppets have no
    // account and that answer says nothing.
    let provisioned = session.puppets.as_ref().is_some_and(|p| p.slots.is_some());
    let effects = session.membership.set_puppets_hold_accounts(provisioned);
    debug_assert!(effects.is_empty(), "set before any roster exists");

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
    // Unbiased: no source starves another. The one order that matters
    // across sources — a puppet's report before the server's consequence of
    // the same fact on the main connection (a rename before the JOIN echo
    // under the new name, say) — is kept by draining the reports already
    // queued BEFORE each main-connection line is handled: the puppet's own
    // connection read the fact before its consequence could reach the main
    // connection, so the report is queued by the time the line is. The
    // drain takes at most the queue's depth (the tasks keep refilling it)
    // and nothing from the other sources. Not a guarantee — two tasks — but
    // a puppet mis-fronted by a report queued after its consequence is
    // withdrawn at the next ownership sync, which evicts a "human" under a
    // nick that turns out to be ours.
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
                }
                None => {
                    // Every executor sender is gone: only possible after the
                    // pool was torn down, so nothing more will arrive.
                    puppet_rx = None;
                }
            },
            event = inbound.recv() => match event {
                Some(FromServer::Line(line)) => {
                    // Reports already queued come first (see the loop's note).
                    for _ in 0..puppet_drain {
                        let Some(ev) = puppet_rx.as_mut().and_then(|rx| rx.try_recv().ok()) else {
                            break;
                        };
                        on_puppet_event(&mut session, &mesh_side.presence, ev);
                    }
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
                request_roster_accounts(session, writer, &channel)?;
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
            // Whose departure, decided before the roster forgets the member:
            // a HUMAN's QUIT under a spelling a puppet's departure holds (the
            // puppet renamed away from it with no echo to come, and a human
            // took the name) settles nothing — it is not the departure the
            // hold waits for, and the puppet's other spellings are still
            // listed under our account.
            let ours = session.membership.is_owned(&nick);
            let effects = session.membership.quit(&nick);
            apply_human_effects(session, presence, effects);
            if ours {
                settle_release(session, presence, &nick);
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
            // be queued; idempotent with the report. Only a spelling the pool
            // HOLDS moves. Whose NICK is this? The pool cannot tell — its
            // table is keyed by spelling — so membership answers, the way it
            // answers everywhere: by account where the server has attributed
            // the holder (a human who took a listed spelling has none, or a
            // different one, and is not ours however the name reads), by the
            // nick fallback where it has not.
            let ours = session.membership.is_owned(&nick);
            let cm = session.isupport.casemapping;
            let tick_ms = session.puppet_now_ms();
            if ours {
                if let Some(p) = session.puppets.as_mut() {
                    // A spelling held for a connection now GONE: this NICK is
                    // the echo of a rename that connection made before it
                    // left. The pool's entry under the spelling, if any, is
                    // the same peer's REPLACEMENT, back under the name the
                    // server freed — not renamed. The hold moves, below.
                    let departed = held_index(p, &fold_nick(&nick, cm), &nick)
                        .map(|i| {
                            let pr = &p.pending_release[i];
                            (pr.peer.clone(), pr.attempt)
                        })
                        .filter(|(peer, attempt)| !p.exec.is_live(peer, *attempt));
                    // The peer: by the pool's spelling, or by a held old
                    // spelling when the pool is already past this echo.
                    let peer = match departed {
                        Some((peer, attempt)) => {
                            debug!(peer = %peer, attempt, from = %nick, to = %to, "puppet: NICK echo of a departed connection's rename; the pool is not moved");
                            None
                        }
                        None => p.pool.resolve(&nick).cloned().or_else(|| {
                            held_index(p, &fold_nick(&nick, cm), &nick)
                                .map(|i| p.pending_release[i].peer.clone())
                        }),
                    };
                    if let Some(peer) = peer {
                        let replay = p.holder.get_mut(&peer).is_some_and(|h| {
                            // An echo comes within the window or never: a
                            // transition past it is not this NICK's.
                            h.unechoed.retain(|u| tick_ms < u.deadline_ms);
                            let same = |a: &str, b: &str| fold_nick(a, cm) == fold_nick(b, cm);
                            match h
                                .unechoed
                                .iter()
                                .position(|u| same(&u.from, &nick) && same(&u.to, &to))
                            {
                                Some(i) => {
                                    h.unechoed.remove(i);
                                    true
                                }
                                None => false,
                            }
                        });
                        if !replay {
                            if let Some(actions) = p.pool.renamed(&peer, &to) {
                                if let Some(h) = p.holder.get_mut(&peer) {
                                    h.applied += 1;
                                }
                                for a in actions {
                                    p.exec.execute(a);
                                }
                            }
                        } else {
                            debug!(peer = %peer, from = %nick, to = %to, "puppet: NICK echo of a rename the puppet already reported; not replayed");
                        }
                    }
                }
            }
            let effects = session.membership.renamed(&nick, &to);
            apply_human_effects(session, presence, effects);
            // A held old spelling is settled by this NICK — one departure
            // from it — when the NICK is ours; a human's rename under a held
            // spelling settles nothing, as a human's QUIT does. A connection
            // no longer live has not been seen leaving yet: it is now listed
            // under the NEW spelling, so the hold moves there rather than
            // ending — the connection's own liveness, not the peer's, since
            // the peer may be back on a replacement.
            let settled = ours
                && session.puppets.as_mut().is_some_and(|p| {
                    let Some(i) = held_index(p, &fold_nick(&nick, cm), &nick) else {
                        return false;
                    };
                    let pr = &mut p.pending_release[i];
                    pr.holds -= 1;
                    let peer = pr.peer.clone();
                    let attempt = pr.attempt;
                    let account = pr.account.clone();
                    if pr.holds == 0 {
                        p.pending_release.remove(i);
                    }
                    if !p.exec.is_live(&peer, attempt) {
                        let account = account.or_else(|| account_backing(p, &peer));
                        hold_as(p, &peer, attempt, &to, tick_ms, cm, account);
                    }
                    true
                });
            if settled {
                sync_owned_nicks(session, presence);
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
    // A line from a peer is the sign of life its lease goes by: the sweep
    // carries no heartbeat time and lists a `mu ask` session an hour past its
    // last line, so presence would hold slots for sessions long finished
    // (`slots.rs`: leases go by ACTIVITY).
    let now = session.puppet_now_ms();
    if let Some(slots) = session.puppets.as_mut().and_then(|p| p.slots.as_mut()) {
        slots.touch(&PeerId::parse(&ev.from), now);
    }
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
        // nick now folds onto another's is quit and re-offered the tail — its
        // Quit through `run_actions`, so the departing spelling is held like
        // any other until the main connection sees it leave.
        let tick_ms = session.puppet_now_ms();
        let protected = session.protected_peers();
        if let Some(p) = session.puppets.as_mut() {
            // Quits only — `set_casemapping` never emits a Connect, so there
            // is no lease to take here; the re-offer comes from the next tick.
            // Holds are keyed by the folded spelling: re-keyed under the new
            // rule, or a QUIT folded the new way would never settle them.
            let old = std::mem::take(&mut p.pending_release);
            for mut pr in old {
                pr.key = fold_nick(&pr.nick, now.casemapping);
                // One peer's holds under a now-shared spelling merge; two
                // PEERS whose spellings now fold together keep one each.
                match p.pending_release.iter_mut().find(|cur| {
                    cur.key == pr.key && cur.peer == pr.peer && cur.attempt == pr.attempt
                }) {
                    Some(cur) => {
                        cur.holds += pr.holds;
                        cur.deadline_ms = cur.deadline_ms.max(pr.deadline_ms);
                        if cur.account.is_none() {
                            cur.account = pr.account;
                        }
                    }
                    None => p.pending_release.push(pr),
                }
            }
            let actions = p.pool.set_casemapping(now.casemapping);
            let _ = run_actions(p, actions, tick_ms, now.casemapping, &|peer: &PeerId| {
                protected.contains(peer)
            });
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
    send(writer, &format!("NAMES {channel}"))?;
    request_roster_accounts(session, writer, channel)
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
) -> Result<(), SendError> {
    if !session.reg.has_whox() {
        return Ok(());
    }
    send(
        writer,
        &format!("WHO {channel} {WHOX_ROSTER_FIELDS},{WHOX_ROSTER_TOKEN}"),
    )
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

// ────────────────────────────── Puppets (2b-i) ──────────────────────────────

/// A puppet's own rename report, applied when read: the pool moves the peer
/// to `to` while it still files it under `from`. The main connection's NICK
/// moves the pool too when a channel is shared; whichever arrives first does
/// the work. Freshness is the report's ordinal against the holder's applied
/// count ([`Holder`]), never spelling. Membership is not told: ownership is
/// the account, which travels with the rename. A holder gone is history.
fn apply_puppet_rename(
    session: &mut Session,
    presence: &mpsc::UnboundedSender<PresenceOp>,
    attempt: u64,
    report: u32,
    from: &str,
    to: &str,
) {
    let cm = session.isupport.casemapping;
    let now = session.puppet_now_ms();
    let carried = session
        .puppets
        .as_ref()
        .and_then(|p| p.holder.values().find(|h| h.attempt == attempt))
        .is_none_or(|h| h.applied >= report);
    if carried {
        debug!(attempt, report, from = %from, to = %to, "puppet: rename carried by the wire since its report; history");
        return;
    }
    // Membership is told nothing: a puppet's rename moves the member and its
    // account travels with it, so there is nothing to settle and no vacated
    // spelling to hold. Only the POOL needs to learn the new nick.
    let Some(p) = session.puppets.as_mut() else {
        return;
    };
    let Some(peer) = p.pool.resolve(from).cloned() else {
        // The pool is past this rename (the main connection carried it, or
        // carried it further), or the peer is quitting and released; its
        // Ended settles it.
        debug!(attempt, from = %from, to = %to, "puppet: rename report behind the pool; ignored");
        return;
    };
    if p.holder.get(&peer).map(|h| h.attempt) != Some(attempt) {
        debug!(peer = %peer, attempt, "puppet: rename report from an attempt that no longer holds the nick");
        return;
    }
    // A rename onto a nick another puppet holds is a collision like one at
    // registration: the pool answers with Quit + ChannelOnly.
    let wait_ms = p.pool.config().departure_wait_secs.saturating_mul(1000);
    match p.pool.renamed(&peer, to) {
        Some(actions) => {
            info!(peer = %peer, from = %from, to = %to, collided = !actions.is_empty(), "puppet: renamed by the server");
            if let Some(h) = p.holder.get_mut(&peer) {
                h.applied += 1;
                // Transitions past their window are never echoed: dropped
                // here as well as at a NICK, so a puppet renamed and renamed
                // with no shared channel does not accumulate them.
                h.unechoed.retain(|u| now < u.deadline_ms);
                h.unechoed.push(Unechoed {
                    from: from.to_string(),
                    to: to.to_string(),
                    deadline_ms: now.saturating_add(wait_ms),
                });
            }
            // The main connection shows this puppet under the OLD spelling
            // until its NICK arrives here — or will pass through it, when
            // several renames are reported before any echo — so the old
            // spelling stays owned until that NICK (or the window). For both
            // pools: a provisioned puppet the server has not attributed is
            // ours by that spelling alone, and the echo finds its holder
            // through this hold.
            hold(p, &peer, attempt, from, now, cm);
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

impl Session {
    /// Peers whose lease must not be taken by an eviction: the spec's "never
    /// evict an agent that has exchanged a line with a human inside the
    /// routing memory's window".
    ///
    /// EMPTY today, and said so rather than faked: the routing memory keeps
    /// message hashes and channels, not peer↔human pairs, so there is no pair
    /// memory to consult yet. Until there is, a lease's idle window is its
    /// only protection — a busy agent keeps its slot, a silent one may lose
    /// it, the safe side of the error — and the pair memory, when it lands,
    /// only ever ADDS protection here. Tracked: mu-irc-remote-session-zgbdz.8.2.
    fn protected_peers(&self) -> HashSet<PeerId> {
        HashSet::new()
    }
}

/// The slot pool for a config, or `None` when it is not provisioned.
fn slot_pool(cfg: &PuppetsConfig) -> Option<Slots> {
    cfg.slot_certs_dir
        .as_ref()
        .map(|_| Slots::new(cfg.slot_accounts(), cfg.slot_idle_secs.saturating_mul(1000)))
}

/// Execute the pool's actions; a `Connect` on a provisioned pool leases a
/// slot first. The ONE place a `Connect` becomes a connection, where the
/// pool's decision ("this peer should have a puppet") meets the slot pool's
/// ("this peer may have THIS account"):
///
///   - a free or evictable slot is leased and the puppet connects AS that
///     account, its nick the account and its credential the slot's;
///   - an eviction quits the previous holder first, loudly, and tells the
///     pool, so it backs off and asks again;
///   - no lease is the spec's SPILLOVER (`Pool::no_slot`): retried on the
///     backoff schedule, never a shared nick.
///
/// `protected` is asked only about eviction candidates ([`Slots::lease`]).
fn run_actions(
    p: &mut Puppetry,
    actions: Vec<PoolAction>,
    now_ms: u64,
    cm: CaseMapping,
    protected: &dyn Fn(&PeerId) -> bool,
) -> bool {
    // Whether the set of leased accounts changed under this batch — a lease
    // taken, returned with a cancel, or returned by a refused dial — so the
    // caller syncs membership in the same breath.
    let mut changed = false;
    // Peers whose Connect this batch has not reached yet: their attempt is
    // spent but no socket is open, so an eviction that moves one refunds
    // it. A victim already past its Connect — dialling since an earlier
    // tick — is a real attempt the server saw, and keeps its charge.
    let mut queued: HashSet<PeerId> = actions
        .iter()
        .filter_map(|a| match a {
            PoolAction::Connect { peer, .. } => Some(peer.clone()),
            _ => None,
        })
        .collect();
    for a in actions {
        match a {
            PoolAction::Quit { peer, nick } => {
                let account = account_backing(p, &peer);
                quit_holding(p, PoolAction::Quit { peer, nick }, now_ms, cm, account);
            }
            PoolAction::Connect { peer, nick } => {
                queued.remove(&peer);
                // An earlier action in this batch moved this peer (an eviction
                // unwound its Connect): a lease taken now would evict a second
                // holder and dial a peer the pool has backing off.
                if !matches!(p.pool.state_of(&peer), Some(PuppetState::Connecting { .. })) {
                    debug!(peer = %peer, "puppet: a Connect unwound earlier in its batch; skipped");
                    continue;
                }
                let grant = match p.slots.as_mut() {
                    Some(slots) => slots.lease(&peer, now_ms, protected),
                    None => {
                        p.exec.execute(PoolAction::Connect { peer, nick });
                        continue;
                    }
                };
                match grant {
                    Grant::Leased { account, evicted } => {
                        changed = true;
                        if let Some(prev) = evicted {
                            warn!(
                                evicted = %prev, to = %peer, account = %account,
                                evictions = p.slots.as_ref().map_or(0, Slots::evictions),
                                "puppet: slot pool full — evicting the least recently active lease"
                            );
                            let quit = match p.pool.nick_of(&prev) {
                                Some(n) => PoolAction::Quit {
                                    peer: prev.clone(),
                                    nick: n.to_string(),
                                },
                                None => PoolAction::Cancel { peer: prev.clone() },
                            };
                            // Held under the account the newcomer now holds:
                            // `prev`'s own lease no longer names it.
                            quit_holding(p, quit, now_ms, cm, Some(account.clone()));
                            // The pool is told too: the evicted peer's state and
                            // its nick-table entry go now, so the newcomer's
                            // registration under that nick is not read as a
                            // collision and the evicted peer backs off to ask
                            // again — its own connection's `Ended` is a quitting
                            // one and decides nothing.
                            if queued.contains(&prev) {
                                p.pool.not_dialled(&prev, now_ms);
                            } else {
                                let _ = p.pool.disconnected(&prev, now_ms);
                            }
                        }
                        changed |= dial_leased(p, peer, account, now_ms);
                    }
                    Grant::Held(account) => changed |= dial_leased(p, peer, account, now_ms),
                    Grant::Spillover => {
                        warn!(peer = %peer, free = p.slots.as_ref().map_or(0, Slots::free), "puppet: no slot to lease — spillover, channel-only");
                        for a in p.pool.no_slot(&peer, now_ms) {
                            p.exec.execute(a);
                        }
                    }
                }
            }
            PoolAction::Cancel { peer } => {
                // A cancelled attempt has no connection to end, so this is the
                // only place its lease can come back (a peer that left
                // discovery mid-dial, an attempt given up on) — unless a
                // departure hold still covers its account: the peer's own last
                // connection, or the puppet it evicted, is on the roster under
                // it until the QUIT, and the hold owns the release.
                let account = p
                    .slots
                    .as_ref()
                    .and_then(|s| s.account_of(&peer))
                    .map(str::to_string);
                let held = account
                    .as_deref()
                    .is_some_and(|a| backs_a_departure(p, &peer, a));
                if !held {
                    if let Some(slots) = p.slots.as_mut() {
                        if let Some(account) = slots.release(&peer) {
                            debug!(peer = %peer, %account, "puppet: slot released with the cancelled attempt");
                            changed = true;
                        }
                    }
                }
                p.exec.execute(PoolAction::Cancel { peer });
            }
            other => p.exec.execute(other),
        }
    }
    changed
}

/// Execute a `Quit` — or the `Cancel` that stands in for one when the peer
/// has no nick yet — holding the departing spelling until the main
/// connection sees it leave: the nick is about to vanish from the server,
/// but this connection has not seen it go, and the roster would front our
/// own puppet as a human meanwhile (R1). The one path every departure
/// takes, the pool's own and an eviction's alike. `account`: the one the
/// puppet is on the roster under — its own lease, or the one an eviction
/// just moved to the newcomer.
fn quit_holding(
    p: &mut Puppetry,
    action: PoolAction,
    now_ms: u64,
    cm: CaseMapping,
    account: Option<String>,
) {
    if let PoolAction::Quit { peer, nick } = &action {
        let attempt = p.exec.live_attempt(peer).unwrap_or(0);
        hold_as(p, peer, attempt, nick, now_ms, cm, account);
    }
    p.exec.execute(action);
}

/// Dial `peer` as the leased `account`. A credential the executor cannot
/// produce is resolved here and now — lease returned, attempt failed in the
/// pool so it backs off and asks again — rather than through an event no
/// attempt would own (see [`Executor::connect_leased`]). `true` if a lease
/// was returned. A refused RETRY of a peer whose last connection is still
/// held keeps its lease: the hold owns the release, as for a `Cancel`. So
/// does a refused NEWCOMER whose account an eviction just took from a puppet
/// still on the roster under it.
fn dial_leased(p: &mut Puppetry, peer: PeerId, account: String, now_ms: u64) -> bool {
    if let Err(why) = p.exec.connect_leased(peer.clone(), account.clone()) {
        let held = backs_a_departure(p, &peer, &account);
        let mut returned = false;
        if !held {
            if let Some(slots) = p.slots.as_mut() {
                returned = slots.release(&peer).is_some();
            }
        }
        if !p.pool.not_dialled(&peer, now_ms) {
            debug!(peer = %peer, "puppet: refused dial for a peer the pool had already moved");
        }
        warn!(peer = %peer, account = %account, %why, held, "puppet: leased dial refused; retrying on the backoff schedule");
        return returned;
    }
    false
}

/// Hand membership the current owned-nick set (wire spellings, from the
/// pool) and execute the human effects that produces. Called before the first
/// pool action and after every ownership transition — the ownership barrier.
fn sync_owned_nicks(session: &mut Session, presence: &mpsc::UnboundedSender<PresenceOp>) {
    // Plain nicks: the old API carried a connection id per nick so a
    // departure report could resolve the right retiring entry. There are no
    // departure reports and no retiring entries, so the id has nothing to
    // identify. A provisioned pool adds the leased accounts, handed over
    // first (mu-irc-remote-session-zgbdz.8).
    let (owned, accounts): (Vec<String>, Vec<String>) = match session.puppets.as_ref() {
        Some(p) => (
            p.pool
                .owned_nicks()
                .into_iter()
                .chain(p.pending_release.iter().map(|pr| pr.nick.clone()))
                .collect(),
            p.slots
                .as_ref()
                .map(|s| {
                    s.leased_accounts()
                        .into_iter()
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        ),
        None => (Vec::new(), Vec::new()),
    };
    // Accounts first: they are the authoritative half, and a leased account
    // whose puppet is attributed on the roster must be ours before the nick
    // fallback is consulted for members the server has not attributed.
    let mut effects = session.membership.set_owned_accounts(accounts);
    effects.extend(session.membership.set_owned_nicks(owned));
    apply_human_effects(session, presence, effects);
}

/// Feed the discovery snapshot to the pool and execute what it decides.
fn puppets_tick(session: &mut Session, presence: &mpsc::UnboundedSender<PresenceOp>) {
    let now = session.puppet_now_ms();
    let cm = session.isupport.casemapping;
    let peers = session.discovery.peers.clone();
    let protected = session.protected_peers();
    let Some(p) = session.puppets.as_mut() else {
        return;
    };
    // Being listed is PRESENCE, not activity (a `mu ask` peer stays listed
    // an hour past its last line), so a tick touches no lease; a line from
    // the peer does (`on_mesh_event`).
    let mut released = settle_overdue(p, now);
    let mut actions = p.pool.observe(&peers, now);
    actions.extend(p.pool.tick(now));
    released |= actions
        .iter()
        .any(|a| matches!(a, PoolAction::Quit { .. } | PoolAction::Cancel { .. }));
    released |= run_actions(p, actions, now, cm, &|peer: &PeerId| {
        protected.contains(peer)
    });
    if released {
        sync_owned_nicks(session, presence);
    }
}

/// The main connection saw `nick` QUIT: a lease waiting on that departure
/// comes back now — unless its peer has reconnected meanwhile and holds the
/// account live, in which case there is nothing to return.
fn settle_release(session: &mut Session, presence: &mpsc::UnboundedSender<PresenceOp>, nick: &str) {
    let key = fold_nick(nick, session.isupport.casemapping);
    let Some(p) = session.puppets.as_mut() else {
        return;
    };
    let Some(i) = held_index(p, &key, nick) else {
        return;
    };
    let pending = p.pending_release.remove(i);
    // The connection is gone: every spelling IT was still held under goes
    // with it (any echo of those renames preceded this QUIT on this
    // connection, or will never come). A later connection of the same
    // peer, gone too before this QUIT was seen, keeps its own hold for its
    // own QUIT.
    p.pending_release
        .retain(|pr| !(pr.peer == pending.peer && pr.attempt == pending.attempt));
    release_settled(p, &pending, "departure seen on the main connection");
    sync_owned_nicks(session, presence);
}

/// Pending departures past their window: the QUIT never came (a puppet on no
/// shared channel, or a departure this connection missed). Let go anyway,
/// loudly — a name held for ever is the failure mode, not a brief roster
/// correction.
fn settle_overdue(p: &mut Puppetry, now_ms: u64) -> bool {
    let mut settled = false;
    while let Some(i) = p
        .pending_release
        .iter()
        .position(|pr| now_ms >= pr.deadline_ms)
    {
        let pending = p.pending_release.remove(i);
        warn!(peer = %pending.peer, nick = %pending.nick, "puppet: departure not seen on the main connection within the window; letting go anyway");
        release_settled(p, &pending, "window elapsed");
        settled = true;
    }
    settled
}

/// The lease a settled departure returns, if any: the departed peer's own,
/// or — an eviction having moved it — the newcomer's. A LIVE holder keeps
/// it (the departed peer reconnected; the newcomer is dialling or on), and
/// so does another departure still unseen under the same account. A holder
/// that is neither — a newcomer refused or cancelled meanwhile — holds an
/// account nothing backs any more, and it goes with this hold.
fn release_settled(p: &mut Puppetry, pending: &PendingRelease, why: &str) -> bool {
    let peer = &pending.peer;
    let Some(slots) = p.slots.as_mut() else {
        return false;
    };
    let holder = match pending.account.as_deref() {
        Some(account) => {
            if p.pending_release
                .iter()
                .any(|pr| pr.account.as_deref() == Some(account))
            {
                debug!(peer = %peer, %account, why, "puppet: another departure under the account is still unseen; the lease stays");
                return false;
            }
            match slots.holder_of(account) {
                Some(holder) => holder.clone(),
                None => return false,
            }
        }
        None => peer.clone(),
    };
    if p.exec.has_live(&holder) {
        debug!(peer = %peer, holder = %holder, why, "puppet: the lease is a live connection's; it stays");
        return false;
    }
    match slots.release(&holder) {
        Some(account) => {
            debug!(peer = %peer, holder = %holder, %account, why, "puppet: slot released");
            true
        }
        None => false,
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
    let protected = session.protected_peers();
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
            p.holder.insert(peer.clone(), Holder::new(attempt));
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
                    // A registered puppet that cannot be told to JOIN has no
                    // voice and no ears, and is not left standing: quit it —
                    // the stop reaches a task whatever it is parked on — with
                    // its spelling held until the main connection sees it
                    // leave, and the pool told it fell, so it backs off and
                    // dials again.
                    warn!(peer = %peer, nick = %nick, "puppet: JOIN not queued (task stalled); quit, to dial again on the backoff schedule");
                    let account = account_backing(p, &peer);
                    let quit = PoolAction::Quit {
                        peer: peer.clone(),
                        nick: nick.clone(),
                    };
                    quit_holding(p, quit, now, cm, account);
                    let actions = p.pool.disconnected(&peer, now);
                    run_actions(p, actions, now, cm, &|peer: &PeerId| {
                        protected.contains(peer)
                    });
                    sync_owned_nicks(session, presence);
                }
            }
        }
        PuppetEvent::NickRejected { peer, numeric, .. } => {
            info!(peer = %peer, numeric = %numeric, "puppet: nick rejected at registration");
            if p.slots.is_some() && numeric == "433" {
                // The nick IS the account: nothing to tail. A 433 means the
                // account's previous connection has not left the server yet
                // (an eviction's QUIT in flight). Keep the lease, cancel this
                // attempt directly — not via `run_actions`, whose `Cancel`
                // arm would return the lease — and ask again as the account.
                p.exec.execute(PoolAction::Cancel { peer: peer.clone() });
                let actions = p.pool.disconnected(&peer, now);
                run_actions(p, actions, now, cm, &|peer: &PeerId| {
                    protected.contains(peer)
                });
            } else {
                let actions = p.pool.nick_rejected(&peer, &numeric, now);
                let changed = run_actions(p, actions, now, cm, &|peer: &PeerId| {
                    protected.contains(peer)
                });
                if changed {
                    sync_owned_nicks(session, presence);
                }
            }
        }
        PuppetEvent::Ended {
            peer,
            attempt,
            why,
            // `nick`: the one the server last knew the puppet by — what the
            // main connection's roster lists it under, if at all. `confirmed`
            // was the departure barrier's and is bound off (zgbdz.4.1).
            nick,
            confirmed: _,
        } => {
            // A LIVE attempt's end is a disconnect the pool has not decided:
            // release the nick, schedule the retry. A QUITTING attempt's end
            // completes a Quit the pool issued; its peer may be on a newer
            // attempt, whose state the old connection's last word must not touch.
            let live = p.exec.is_live(&peer, attempt);
            let was_registered = live && p.pool.nick_of(&peer).is_some();
            p.exec.forget(&peer, attempt);
            if p.holder.get(&peer).map(|h| h.attempt) == Some(attempt) {
                p.holder.remove(&peer);
            }
            // The lease. A SUPERSEDED connection's end never returns it (a
            // newer attempt owns the account). Otherwise it comes back once
            // the MAIN connection has seen the puppet leave: while its roster
            // lists this nick as OUR account the QUIT has not arrived here,
            // and releasing would front our own puppet as a human (R1) — so
            // it waits for that QUIT, bounded by `departure_wait_secs`.
            let superseded = p.exec.has_live(&peer);
            let mut released = false;
            // Still on this connection's roster as ours — by the account
            // where the server has answered, by the nick fallback where it
            // has not (the WHOX pass pending, a server without account caps),
            // exactly as membership decides it. A holder the server says is
            // someone else is not held.
            let still_listed = !superseded
                && nick.as_deref().is_some_and(|n| {
                    session.membership.is_owned(n) && session.membership.is_listed(n)
                });
            if superseded {
                debug!(peer = %peer, attempt, "puppet: a superseded connection ended; ownership stays with the live one");
            } else if still_listed {
                let n = nick.clone().unwrap_or_default();
                debug!(peer = %peer, nick = %n, "puppet: connection ended; held until the main connection sees it leave");
                hold(p, &peer, attempt, &n, now, cm);
            } else if p.pending_release.iter().any(|pr| pr.peer == peer) {
                // Not listed under its LAST spelling, but held under an
                // earlier one (a rename reported, its echo not yet here):
                // that hold owns the release — it moves to the new spelling
                // at the echo and ends at the QUIT.
                debug!(peer = %peer, "puppet: connection ended; an earlier spelling's hold covers the departure");
            } else if p
                .slots
                .as_ref()
                .and_then(|s| s.account_of(&peer))
                .is_some_and(|a| {
                    p.pending_release
                        .iter()
                        .any(|pr| pr.account.as_deref() == Some(a))
                })
            {
                // This attempt's account came from an eviction, and the
                // puppet it was taken from is still on the roster under it:
                // that hold owns the release.
                debug!(peer = %peer, "puppet: connection ended; the account still backs an evicted puppet's departure");
            } else if let Some(slots) = p.slots.as_mut() {
                if let Some(account) = slots.release(&peer) {
                    debug!(peer = %peer, %account, "puppet: slot released");
                    released = true;
                }
            }
            if live {
                let actions = p.pool.disconnected(&peer, now);
                released |= run_actions(p, actions, now, cm, &|peer: &PeerId| {
                    protected.contains(peer)
                });
            }
            if live {
                // Not a Quit or Cancel of ours: the dial failed, registration
                // did not complete, or the server dropped a registered
                // puppet. Said aloud once per run of failures — the loss that
                // starts the pool backing off — and at debug for the retries
                // that follow, which the schedule already shows; a pool that
                // gives up says so once more (channel-only). No counter and
                // no window: the pool's own attempt count is the state.
                let state = p.pool.state_of(&peer);
                if matches!(state, Some(PuppetState::BackingOff { attempt: 1, .. })) {
                    warn!(peer = %peer, why = %why, registered = was_registered, "puppet: connection lost; backing off");
                } else {
                    debug!(peer = %peer, why = %why, registered = was_registered, ?state, "puppet: connection lost again");
                }
            } else {
                debug!(peer = %peer, why = %why, registered = was_registered, "puppet: connection ended as told");
            }
            // The connection is over (closed after a QUIT, the grace ran out,
            // or it dropped). Ownership is the account, not a spelling, so the
            // sync needs no ordering against the main connection's echoes; the
            // LEASE waits for the observed departure (above).
            if was_registered || released {
                sync_owned_nicks(session, presence);
            }
        }
        PuppetEvent::Renamed {
            peer,
            attempt,
            from,
            to,
        } => {
            // A NICK for the puppet itself (a server can force one), reported
            // by its own connection. A report the wire has already carried —
            // the main connection's NICK moved the pool first — is history,
            // whatever spelling it names ([`Holder`]).
            let Some(h) = p.holder.get_mut(&peer) else {
                debug!(peer = %peer, attempt, "puppet: rename report for a peer with no holder; ignored");
                return;
            };
            if h.attempt != attempt {
                debug!(peer = %peer, attempt, "puppet: rename report from an attempt that no longer holds the nick");
                return;
            }
            h.reports += 1;
            let report = h.reports;
            if report <= h.applied {
                debug!(peer = %peer, attempt, from = %from, to = %to, "puppet: rename report already carried by the wire; ignored");
                return;
            }
            // Applied straight away: ownership is the account, so nothing on
            // the main connection has to drain first.
            apply_puppet_rename(session, presence, attempt, report, &from, &to);
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

/// Tear the pool down: every puppet task is aborted — its socket drops, the
/// server sees the connection end and lets the nick go — and the attempt
/// history goes back to `run` for the next pool. The graceful form, a QUIT
/// from every registered puppet within the grace before the main
/// connection's own, is the increment above this one. Idempotent.
async fn teardown_puppets(
    session: &mut Session,
    presence: &mpsc::UnboundedSender<PresenceOp>,
    puppet_budget: &mut AttemptBudget,
) {
    let Some(mut p) = session.puppets.take() else {
        return;
    };
    let registered = p.pool.registered_count();
    // The puppets are leaving, but this connection's roster still lists
    // them; dropping ownership first would front them as humans — phantoms
    // nothing would withdraw across a reconnect. Off the roster first, as
    // their QUITs would take them.
    let leaving: Vec<String> = p
        .pool
        .owned_nicks()
        .into_iter()
        .chain(p.pending_release.iter().map(|pr| pr.nick.clone()))
        .collect();
    for nick in &leaving {
        let effects = session.membership.quit(nick);
        apply_human_effects(session, presence, effects);
    }
    p.exec.abort_all();
    let stats = p.exec.stats().clone();
    info!(
        puppets_registered = registered,
        stale_events = stats.stale_events,
        commands_dropped = stats.commands_dropped,
        puppet_lines_dropped = stats.lines_dropped,
        puppet_lines_unqueued = stats.lines_unqueued,
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

    use super::super::puppet_task::test_support::{
        next, next_ended, read_until, refusing_connector, scripted_connector, Hand,
    };
    use super::super::puppet_task::Connector;
    use crate::adapter::DEFAULT_CHANNELLEN;
    use crate::bridge::mesh_side::nats_watcher;
    use crate::config::{SaslCreds, Secret, SlotCredential};
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
        discover_abc(session);
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
    use tokio::io::{AsyncWriteExt, BufReader};

    /// Give a scripted session a pool + executor, the way `session()` does
    /// when `[irc.puppets] enabled = true`.
    fn with_puppets(
        s: &mut Session,
        cfg: PuppetsConfig,
        connector: puppet_io::Connector,
    ) -> mpsc::Receiver<PuppetEvent> {
        let (ev_tx, ev_rx) = mpsc::channel(64);
        let mut irc = irc_config();
        // A provisioned pool is TLS-only — `parse_puppets` refuses
        // `slot_certs_dir` under `tls = false` (`PuppetsCertsWithoutTls`) and
        // the adapter refuses EXTERNAL over cleartext — so the executor's view
        // of the connection says so. The scripted connector opens no socket;
        // the flag only tells the adapter which handshake is legitimate.
        irc.tls = cfg.slot_certs_dir.is_some();
        irc.puppets = cfg.clone();
        let slots = slot_pool(&cfg);
        assert!(
            s.membership
                .set_puppets_hold_accounts(slots.is_some())
                .is_empty(),
            "the pool is given before any roster exists"
        );
        let pool = Pool::with_budget(cfg, 32, s.isupport.casemapping, AttemptBudget::new());
        let exec = Executor::new(irc, connector, ev_tx, Duration::from_secs(5));
        s.puppets = Some(Puppetry {
            pool,
            exec,
            holder: HashMap::new(),
            slots,
            pending_release: Vec::new(),
        });
        ev_rx
    }

    /// The next event `is` accepts, every event before it handed to the
    /// session first — the reports a live loop would have drained.
    async fn drive_until(
        session: &mut Session,
        presence: &mpsc::UnboundedSender<PresenceOp>,
        events: &mut mpsc::Receiver<PuppetEvent>,
        is: impl Fn(&PuppetEvent) -> bool,
    ) -> PuppetEvent {
        loop {
            let ev = next(events).await;
            if is(&ev) {
                return ev;
            }
            on_puppet_event(session, presence, ev);
        }
    }

    /// The pool's spelling for `cc:abc`, the one puppet these tests drive.
    /// The main connection sees `from` rename to `to`.
    fn wire_nick(
        session: &mut Session,
        presence: &mpsc::UnboundedSender<PresenceOp>,
        writer: &mut transport::LineWriter,
        from: &str,
        to: &str,
    ) {
        on_irc_line(
            session,
            presence,
            writer,
            &format!(":{from}!u@h NICK :{to}"),
        )
        .unwrap();
    }

    /// The puppet's next rename report, handed to the session with every
    /// event before it.
    async fn next_renamed(
        session: &mut Session,
        presence: &mpsc::UnboundedSender<PresenceOp>,
        events: &mut mpsc::Receiver<PuppetEvent>,
    ) {
        let is = |ev: &PuppetEvent| matches!(ev, PuppetEvent::Renamed { .. });
        let ev = drive_until(session, presence, events, is).await;
        on_puppet_event(session, presence, ev);
    }

    /// The pool's spelling for `cc:abc`, the one puppet these tests drive.
    fn pool_nick(session: &Session) -> Option<&str> {
        session
            .puppets
            .as_ref()
            .unwrap()
            .pool
            .nick_of(&PeerId::parse("cc:abc"))
    }

    /// Discovery lists `cc:abc` alone: the next tick offers it a puppet.
    fn discover_abc(session: &mut Session) {
        session.discovery = Discovery::from_srv(HashMap::from([(
            "cc:abc".to_string(),
            "mu.agent.cc.abc.dm".to_string(),
        )]));
    }

    fn eager() -> PuppetsConfig {
        PuppetsConfig {
            min_age_secs: 0,
            ..PuppetsConfig::default()
        }
    }

    // ───────────────── a PROVISIONED pool: slots, certificates, accounts ────
    //
    // The slot fixtures are the committed self-signed client certs; a pool of
    // `max` slots gets `cc-1..cc-max` provisioned from them (two exist).
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
    /// A pool of `max` slots, provisioned in a private temp dir. Eager
    /// (`min_age_secs: 0`) like [`eager`], and `slot_idle_secs: 1` so idleness
    /// is reachable in a test that waits a second.
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
            min_age_secs: 0,
            max,
            slot_certs_dir: Some(dir),
            slot_idle_secs: 30,
            // Holds outlive the idle window here, so a test can evict a
            // held, idle lease; and every due peer dials in one tick.
            departure_wait_secs: 60,
            connect_parallelism: 4,
            ..PuppetsConfig::default()
        }
    }
    /// Negotiate the account capabilities on the scripted MAIN connection —
    /// through its own registration, the way the real one does — so a JOIN
    /// with an account parameter attributes, and an ACCOUNT line is honoured.
    /// Without this the main connection can attribute nothing and a
    /// provisioned pool cannot tell a puppet from a human under its name.
    fn negotiate_accounts(session: &mut Session) {
        for line in [
            "CAP * LS :extended-join account-notify account-tag",
            "CAP * ACK :extended-join account-notify account-tag",
            ":srv 001 mu-gw :Welcome",
        ] {
            let _ = session.reg.on_message(&IrcMessage::parse(line));
        }
        assert!(
            session.reg.negotiated().extended_join,
            "extended-join negotiated"
        );
    }
    /// A connector that hands the server half over like [`scripted_connector`]
    /// and records whether a credential came with the dial.
    fn slot_connector(hand: Hand, presented: Arc<std::sync::atomic::AtomicBool>) -> Connector {
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
    /// Register a LEASED puppet: the script speaks SASL — offers `sasl`,
    /// acknowledges it, answers the EXTERNAL challenge, confirms with `903` —
    /// and welcomes the puppet under its slot account, which is what a
    /// `force-nick-equals-account` server does. Then the puppet joins the
    /// lobby on the main connection WITH its account, so the roster
    /// attributes it. Returns the server halves and the account.
    /// The scripted server's side of a slot registration: offer `sasl`, take
    /// EXTERNAL, answer the challenge, welcome the puppet as `account`.
    /// `payload` pins the base64 the puppet must send, where a test cares.
    async fn sasl_register(
        server: tokio::io::DuplexStream,
        account: &str,
        payload: Option<&str>,
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
        assert!(
            auth.starts_with("AUTHENTICATE EXTERNAL"),
            "EXTERNAL, not PLAIN: {auth}"
        );
        wh.write_all(b"AUTHENTICATE +\r\n").await.unwrap();
        let sent = read_until(&mut r, "AUTHENTICATE ").await;
        match payload {
            Some(p) => assert_eq!(sent.trim(), p, "the account, base64, not a secret"),
            None => assert!(sent.trim() != "AUTHENTICATE +", "{sent}"),
        }
        wh.write_all(format!(":srv 903 {account} :Authentication successful\r\n").as_bytes())
            .await
            .unwrap();
        read_until(&mut r, "CAP END").await;
        wh.write_all(format!(":srv 001 {account} :Welcome\r\n").as_bytes())
            .await
            .unwrap();
        (r, wh)
    }
    async fn register_slot_puppet(
        session: &mut Session,
        presence: &mpsc::UnboundedSender<PresenceOp>,
        _writer: &mut transport::LineWriter,
        events: &mut mpsc::Receiver<PuppetEvent>,
        hand_rx: &mut mpsc::UnboundedReceiver<(String, tokio::io::DuplexStream)>,
    ) -> (
        BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
        String,
    ) {
        discover_abc(session);
        puppets_tick(session, presence);
        let (nick, server) = hand_rx.recv().await.unwrap();
        assert_eq!(
            nick, "cc-1",
            "a leased puppet registers AS its slot account"
        );
        let (r, wh) = sasl_register(server, "cc-1", Some("AUTHENTICATE Y2MtMQ==")).await;
        let ev = next(events).await;
        assert!(
            matches!(&ev, PuppetEvent::Registered { nick, .. } if nick == "cc-1"),
            "{ev:?}"
        );
        on_puppet_event(session, presence, ev);
        (r, wh, "cc-1".to_string())
    }
    /// [`register_slot_puppet`], then the puppet joins the lobby on the main
    /// connection WITH its account, so the roster attributes it.
    async fn registered_slot_puppet(
        session: &mut Session,
        presence: &mpsc::UnboundedSender<PresenceOp>,
        writer: &mut transport::LineWriter,
        events: &mut mpsc::Receiver<PuppetEvent>,
        hand_rx: &mut mpsc::UnboundedReceiver<(String, tokio::io::DuplexStream)>,
    ) -> (
        BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
        String,
    ) {
        let (r, wh, account) =
            register_slot_puppet(session, presence, writer, events, hand_rx).await;
        // The main connection sees the puppet join WITH its account.
        on_irc_line(session, presence, writer, ":cc-1!u@h JOIN #mu cc-1 :puppet").unwrap();
        assert!(
            session.membership.is_owned_by_account("cc-1"),
            "attributed, and ours by account"
        );
        (r, wh, account)
    }
    /// What every leased-puppet test starts from: a scripted session with a
    /// provisioned pool of `max` slots, the account capabilities negotiated on
    /// the main connection, and the ends a test drives it from.
    struct Leased {
        session: Session,
        presence: mpsc::UnboundedSender<PresenceOp>,
        presence_rx: mpsc::UnboundedReceiver<PresenceOp>,
        hand_rx: mpsc::UnboundedReceiver<(String, tokio::io::DuplexStream)>,
        presented: Arc<std::sync::atomic::AtomicBool>,
        events: mpsc::Receiver<PuppetEvent>,
        /// The slot files, for a test that breaks a credential mid-way.
        dir: std::path::PathBuf,
    }
    fn leased(max: usize) -> Leased {
        let Scripted { mut session, .. } = scripted_session(4);
        negotiate_accounts(&mut session);
        let (presence, presence_rx) = mpsc::unbounded_channel();
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
            presence_rx,
            hand_rx,
            presented,
            events,
            dir,
        }
    }
    fn slots_of(session: &Session) -> &Slots {
        session
            .puppets
            .as_ref()
            .unwrap()
            .slots
            .as_ref()
            .expect("a provisioned pool")
    }
    /// Move the pool's clock forward by `secs` without waiting: it reads
    /// process uptime, so its origin moves back.
    fn advance_puppet_clock(session: &mut Session, secs: u64) {
        session.process_started = session
            .process_started
            .checked_sub(Duration::from_secs(secs))
            .expect("the host has been up longer than the jump");
    }
    /// Two qualifying peers listed: the one already leased, and a newcomer.
    fn abc_and_def() -> Discovery {
        Discovery::from_srv(HashMap::from([
            ("cc:abc".to_string(), "mu.agent.cc.abc.dm".to_string()),
            ("cc:def".to_string(), "mu.agent.cc.def.dm".to_string()),
        ]))
    }

    // ───────────────── renames without a barrier: the puppet's word is final ──

    #[tokio::test]
    async fn a_server_forced_rename_keeps_the_pool_and_membership_in_step() {
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        let (mut r, mut wh) =
            registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        let _ = read_until(&mut r, "JOIN ").await;
        wh.write_all(b":cc-abc!u@h NICK :cc-abc2\r\n")
            .await
            .unwrap();
        let ev = next(&mut events).await;
        assert!(
            matches!(&ev, PuppetEvent::Renamed { from, to, .. } if from == "cc-abc" && to == "cc-abc2"),
            "{ev:?}"
        );
        on_puppet_event(&mut session, &presence, ev);
        // Applied at once in the pool; membership holds the OLD spelling too,
        // until the main connection's NICK arrives (its roster shows the
        // puppet under it meanwhile).
        assert_eq!(pool_nick(&session), Some("cc-abc2"));
        assert_eq!(session.membership.owned_nicks(), vec!["cc-abc", "cc-abc2"]);
        // The main connection's NICK settles the old spelling and moves
        // nothing twice.
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":cc-abc!u@h NICK :cc-abc2",
        )
        .unwrap();
        assert_eq!(pool_nick(&session), Some("cc-abc2"));
        assert_eq!(session.membership.owned_nicks(), vec!["cc-abc2"]);
        assert!(
            !session.membership.is_owned("cc-abc"),
            "the old spelling is free"
        );
        // Leaving under the new name ends under the new name.
        let p = session.puppets.as_mut().unwrap();
        p.exec.execute(PoolAction::Quit {
            peer: PeerId::parse("cc:abc"),
            nick: "cc-abc2".into(),
        });
        read_until(&mut r, "QUIT").await;
        drop(wh);
        drop(r);
        let ev = next_ended(&mut events).await;
        assert!(
            matches!(&ev, PuppetEvent::Ended { nick: Some(n), .. } if n == "cc-abc2"),
            "{ev:?}"
        );
    }

    #[tokio::test]
    async fn a_rename_report_and_the_main_connections_nick_agree_in_either_order() {
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, mut presence_rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, mut wh) =
            registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        let _ = read_until(&mut r, "JOIN ").await;
        while presence_rx.try_recv().is_ok() {}
        // Report first, then the main connection's echo under the OLD name and
        // its NICK: the echo is ours (nick fallback still lists the old
        // spelling until the sync), the NICK moves nothing twice.
        wh.write_all(b":cc-abc!u@h NICK :cc-b\r\n").await.unwrap();
        next_renamed(&mut session, &presence, &mut events).await;
        assert_eq!(pool_nick(&session), Some("cc-b"));
        on_irc_line(&mut session, &presence, &mut writer, ":cc-b!u@h JOIN #mu").unwrap();
        assert!(
            !session.membership.is_present("cc-b"),
            "the puppet under its new name is not a human"
        );
        wire_nick(&mut session, &presence, &mut writer, "cc-abc", "cc-b");
        assert_eq!(pool_nick(&session), Some("cc-b"));
        assert_eq!(session.membership.owned_nicks(), vec!["cc-b"]);
        assert!(
            presence_rx.try_recv().is_err(),
            "a presence op for the gateway's own puppet"
        );
        drop(wh);
        drop(r);
    }

    // ───────────────── the account answers "whose NICK is this?" ─────────────

    #[tokio::test]
    async fn a_humans_nick_under_a_vacated_spelling_does_not_move_the_pool() {
        let Leased {
            mut session,
            presence,
            mut presence_rx,
            mut hand_rx,
            mut events,
            ..
        } = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, mut wh, account) = registered_slot_puppet(
            &mut session,
            &presence,
            &mut writer,
            &mut events,
            &mut hand_rx,
        )
        .await;
        let _ = read_until(&mut r, "JOIN ").await;
        while presence_rx.try_recv().is_ok() {}
        // The server renames the puppet (its own connection sees it) — and the
        // main connection sees the same NICK under the account. Both are the
        // puppet's: the pool moves to cc-b.
        wh.write_all(b":cc-1!u@h NICK :cc-b\r\n").await.unwrap();
        next_renamed(&mut session, &presence, &mut events).await;
        wire_nick(&mut session, &presence, &mut writer, &account, "cc-b");
        assert_eq!(pool_nick(&session), Some("cc-b"));
        assert!(
            session.membership.is_owned_by_account("cc-b"),
            "the account travelled with the rename"
        );
        // A human takes the vacated cc-1 (no account: `*`) and renames to
        // carol. NOT ours — the account says so — so the pool stays on cc-b,
        // and the human is fronted at once, under both names.
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":cc-1!h@h JOIN #mu * :a human",
        )
        .unwrap();
        assert!(
            session.membership.is_present("cc-1"),
            "a human under the vacated spelling, fronted now"
        );
        assert!(!session.membership.is_owned_by_account("cc-1"));
        wire_nick(&mut session, &presence, &mut writer, "cc-1", "carol");
        assert_eq!(
            pool_nick(&session),
            Some("cc-b"),
            "the human's NICK did not move the pool"
        );
        assert!(session.membership.is_present("carol") && !session.membership.is_present("cc-1"));
        assert!(
            presence_rx
                .try_recv()
                .map(|op| matches!(op, PresenceOp::Front(ref p) if p == &PeerId::human("cc-1")))
                .unwrap_or(false),
            "the human was fronted when they joined, not after a barrier"
        );
        drop(wh);
        drop(r);
    }

    #[tokio::test]
    async fn a_humans_nick_under_a_departed_puppets_name_does_not_move_the_pool() {
        let Leased {
            mut session,
            presence,
            mut hand_rx,
            mut events,
            ..
        } = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, wh, account) = registered_slot_puppet(
            &mut session,
            &presence,
            &mut writer,
            &mut events,
            &mut hand_rx,
        )
        .await;
        let _ = read_until(&mut r, "JOIN ").await;
        // The main connection sees the puppet QUIT; the pool learns only at the
        // puppet's Ended, still queued. A human takes the freed name — with no
        // account — and renames. Theirs, not the pool's.
        on_irc_line(&mut session, &presence, &mut writer, ":cc-1!u@h QUIT :bye").unwrap();
        assert!(
            !session.membership.is_owned_by_account(&account),
            "seen to leave"
        );
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":cc-1!h@h JOIN #mu * :a human",
        )
        .unwrap();
        assert!(
            session.membership.is_present("cc-1"),
            "a human took the name"
        );
        wire_nick(&mut session, &presence, &mut writer, "cc-1", "xavier");
        assert!(session.membership.is_present("xavier") && !session.membership.is_present("cc-1"));
        assert_eq!(
            pool_nick(&session),
            Some("cc-1"),
            "the human's NICK did not rename the pool's puppet"
        );
        // The puppet's Ended: the pool releases the nick AND the lease; the
        // human is untouched.
        drop(wh);
        drop(r);
        let ev = drive_until(&mut session, &presence, &mut events, |ev| {
            matches!(ev, PuppetEvent::Ended { .. })
        })
        .await;
        on_puppet_event(&mut session, &presence, ev);
        assert!(session.membership.owned_nicks().is_empty());
        assert_eq!(
            slots_of(&session).free(),
            1,
            "the lease came back with the departure"
        );
        assert!(session.membership.is_present("xavier"));
    }

    // ───────────────── the pool leases, presents, spills over, releases ──────

    #[tokio::test]
    async fn a_leased_puppet_connects_as_its_slot_with_its_certificate_and_membership_is_told() {
        let Leased {
            mut session,
            presence,
            mut hand_rx,
            mut events,
            presented,
            ..
        } = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (r, wh, account) = registered_slot_puppet(
            &mut session,
            &presence,
            &mut writer,
            &mut events,
            &mut hand_rx,
        )
        .await;
        assert!(
            presented.load(std::sync::atomic::Ordering::SeqCst),
            "the dial carried the slot's credential"
        );
        assert_eq!(
            slots_of(&session).account_of(&PeerId::parse("cc:abc")),
            Some(account.as_str())
        );
        assert_eq!(slots_of(&session).free(), 0);
        // Membership holds the leased account as ours — the authoritative half.
        assert!(session.membership.is_owned_by_account(&account));
        drop(wh);
        drop(r);
    }

    #[tokio::test]
    async fn a_full_pool_with_nothing_evictable_spills_over_to_channel_only() {
        let Leased {
            mut session,
            presence,
            mut hand_rx,
            mut events,
            ..
        } = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (r, wh, _) = registered_slot_puppet(
            &mut session,
            &presence,
            &mut writer,
            &mut events,
            &mut hand_rx,
        )
        .await;
        // A second qualifying peer appears while the first's lease is fresh
        // (granted moments ago): one slot, nothing evictable — spillover.
        session.discovery = abc_and_def();
        puppets_tick(&mut session, &presence);
        assert!(
            hand_rx.try_recv().is_err(),
            "no dial for a peer with no slot"
        );
        let p = session.puppets.as_ref().unwrap();
        assert!(
            matches!(
                p.pool.state_of(&PeerId::parse("cc:def")),
                Some(PuppetState::BackingOff { .. })
            ),
            "{:?}",
            p.pool.state_of(&PeerId::parse("cc:def"))
        );
        assert_eq!(slots_of(&session).free(), 0, "and the first keeps its slot");
        drop(wh);
        drop(r);
    }

    #[tokio::test]
    async fn an_idle_lease_is_evicted_for_a_newcomer_and_its_puppet_is_quit_first() {
        let Leased {
            mut session,
            presence,
            mut hand_rx,
            mut events,
            ..
        } = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, wh, account) = registered_slot_puppet(
            &mut session,
            &presence,
            &mut writer,
            &mut events,
            &mut hand_rx,
        )
        .await;
        let _ = read_until(&mut r, "JOIN ").await;
        // cc:abc says nothing for longer than the idle window — it stays
        // LISTED the whole time, which is presence, not life — and cc:def
        // qualifies with every slot held.
        advance_puppet_clock(&mut session, 31);
        session.discovery = abc_and_def();
        puppets_tick(&mut session, &presence);
        // The idle holder is quit first; the newcomer dials AS the slot.
        let quit = read_until(&mut r, "QUIT").await;
        assert!(quit.starts_with("QUIT"), "{quit}");
        let (nick, _server) = tokio::time::timeout(Duration::from_secs(5), hand_rx.recv())
            .await
            .expect("the newcomer dials")
            .unwrap();
        assert_eq!(
            nick, account,
            "the newcomer registers as the account it took"
        );
        let slots = slots_of(&session);
        assert_eq!(slots.evictions(), 1, "counted, not silent");
        assert_eq!(
            slots.account_of(&PeerId::parse("cc:def")),
            Some(account.as_str())
        );
        assert_eq!(
            slots.account_of(&PeerId::parse("cc:abc")),
            None,
            "the lease is a fact the moment it is granted"
        );
        let p = session.puppets.as_ref().unwrap();
        assert!(
            matches!(
                p.pool.state_of(&PeerId::parse("cc:abc")),
                Some(PuppetState::BackingOff { .. })
            ),
            "the evicted peer is told, and will ask again: {:?}",
            p.pool.state_of(&PeerId::parse("cc:abc"))
        );
        drop(wh);
    }

    #[tokio::test]
    async fn a_peer_that_leaves_discovery_mid_dial_returns_its_lease_with_the_cancel() {
        let Leased {
            mut session,
            presence,
            mut hand_rx,
            ..
        } = leased(1);
        discover_abc(&mut session);
        puppets_tick(&mut session, &presence);
        let (nick, _server) = tokio::time::timeout(Duration::from_secs(5), hand_rx.recv())
            .await
            .expect("dialled")
            .unwrap();
        assert_eq!(nick, "cc-1");
        assert_eq!(slots_of(&session).free(), 0);
        // Gone from discovery before it registered: the pool cancels the
        // attempt. A cancelled attempt has no connection to end, so the
        // `Cancel` is the only place its lease can come back — and it does.
        session.discovery = Discovery::from_srv(HashMap::new());
        puppets_tick(&mut session, &presence);
        assert_eq!(
            slots_of(&session).free(),
            1,
            "returned with the cancel, not held until idle"
        );
        assert!(session.membership.owned_nicks().is_empty());
        // …so the next peer can have it.
        session.discovery = Discovery::from_srv(HashMap::from([(
            "cc:def".to_string(),
            "mu.agent.cc.def.dm".to_string(),
        )]));
        puppets_tick(&mut session, &presence);
        let (nick, _server) = tokio::time::timeout(Duration::from_secs(5), hand_rx.recv())
            .await
            .expect("the freed slot is dialled by the newcomer")
            .unwrap();
        assert_eq!(nick, "cc-1");
    }

    #[tokio::test]
    async fn a_dropped_puppet_still_on_the_roster_stays_owned_until_its_quit_is_seen() {
        // Unprovisioned, and the puppet IS on this connection's roster when
        // its socket drops: the server's QUIT has not arrived here. Dropping
        // the spelling from the owned set now would front our own puppet as
        // a human until it does (R1). Held, like a departure the pool issued.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (r, wh) = registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        on_irc_line(&mut session, &presence, &mut writer, ":cc-abc!u@h JOIN #mu").unwrap();
        drop(wh);
        drop(r);
        let ev = drive_until(&mut session, &presence, &mut events, |ev| {
            matches!(ev, PuppetEvent::Ended { .. })
        })
        .await;
        on_puppet_event(&mut session, &presence, ev);
        assert!(
            session.membership.is_owned("cc-abc"),
            "held: the roster still lists it"
        );
        assert!(!session.membership.is_present("cc-abc"), "not a human");
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":cc-abc!u@h QUIT :Connection closed",
        )
        .unwrap();
        assert!(!session.membership.is_owned("cc-abc"), "let go at the QUIT");
        assert!(session.membership.owned_nicks().is_empty());
    }

    #[tokio::test]
    async fn a_rename_report_before_the_main_connections_nick_keeps_the_old_spelling_owned() {
        // Unprovisioned: ownership is the spelling. The puppet's own report
        // of a server-forced rename can arrive before the main connection's
        // NICK; until then the OLD spelling is still on the roster here, and
        // must stay ours rather than be fronted as a human for the gap.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, mut wh) =
            registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        let _ = read_until(&mut r, "JOIN ").await;
        on_irc_line(&mut session, &presence, &mut writer, ":cc-abc!u@h JOIN #mu").unwrap();
        wh.write_all(b":cc-abc!u@h NICK :cc-b\r\n").await.unwrap();
        next_renamed(&mut session, &presence, &mut events).await;
        assert!(session.membership.is_owned("cc-b"), "the pool moved");
        assert!(
            session.membership.is_owned("cc-abc"),
            "the old spelling is held until the NICK is seen here"
        );
        assert!(!session.membership.is_present("cc-abc"), "not a human");
        wire_nick(&mut session, &presence, &mut writer, "cc-abc", "cc-b");
        assert!(
            !session.membership.is_owned("cc-abc"),
            "settled by the NICK"
        );
        assert!(session.membership.is_owned("cc-b") && !session.membership.is_present("cc-b"));
        drop(wh);
        drop(r);
    }

    #[tokio::test]
    async fn a_departing_puppets_nick_stays_owned_until_the_main_connection_sees_it_leave() {
        // Unprovisioned pool. The peer leaves discovery: the pool forgets it
        // and issues a Quit — but the puppet is still on the server and on
        // this connection's roster until its QUIT echoes here. Dropping the
        // nick from the owned set at the Quit would front our own puppet as
        // a human for that round trip (R1). It is held until the QUIT.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, wh) =
            registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        on_irc_line(&mut session, &presence, &mut writer, ":cc-abc!u@h JOIN #mu").unwrap();
        assert!(session.membership.is_owned("cc-abc") && !session.membership.is_present("cc-abc"));
        session.discovery = Discovery::from_srv(HashMap::new());
        puppets_tick(&mut session, &presence);
        read_until(&mut r, "QUIT").await;
        assert!(
            session.membership.is_owned("cc-abc"),
            "still ours: the QUIT has not been seen here"
        );
        assert!(!session.membership.is_present("cc-abc"), "not a human");
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":cc-abc!u@h QUIT :bye",
        )
        .unwrap();
        assert!(!session.membership.is_owned("cc-abc"), "let go at the QUIT");
        assert!(session.membership.owned_nicks().is_empty());
        drop(wh);
    }

    #[tokio::test]
    async fn a_quitting_puppets_lease_comes_back_at_the_quit_and_membership_is_told() {
        // Provisioned. The peer leaves discovery (Quit issued); the main
        // connection sees the QUIT before the puppet's own Ended arrives.
        // The lease comes back at the QUIT, and membership is told then —
        // not left holding a stale account until an unrelated sync.
        let Leased {
            mut session,
            presence,
            mut hand_rx,
            mut events,
            ..
        } = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, wh, account) = registered_slot_puppet(
            &mut session,
            &presence,
            &mut writer,
            &mut events,
            &mut hand_rx,
        )
        .await;
        session.discovery = Discovery::from_srv(HashMap::new());
        puppets_tick(&mut session, &presence);
        read_until(&mut r, "QUIT").await;
        assert_eq!(slots_of(&session).free(), 0, "held until seen");
        assert!(session.membership.is_owned_by_account(&account));
        on_irc_line(&mut session, &presence, &mut writer, ":cc-1!u@h QUIT :bye").unwrap();
        assert_eq!(slots_of(&session).free(), 1, "released at the QUIT");
        assert!(
            !session.membership.is_owned_by_account(&account),
            "and membership told at once"
        );
        // The puppet's own Ended, arriving after: nothing left to do.
        drop(wh);
        drop(r);
        let ev = drive_until(&mut session, &presence, &mut events, |ev| {
            matches!(ev, PuppetEvent::Ended { .. })
        })
        .await;
        on_puppet_event(&mut session, &presence, ev);
        assert_eq!(slots_of(&session).free(), 1);
        assert!(session.membership.owned_nicks().is_empty());
    }

    #[tokio::test]
    async fn a_peer_that_leaves_discovery_while_its_departure_is_held_keeps_the_hold() {
        // The puppet's socket dropped (Ended; the lease held, the pool backing
        // off), then discovery removes the peer: the pool's Cancel for a
        // backing-off peer must not return a lease the hold still covers —
        // the puppet is on the roster until its QUIT.
        let Leased {
            mut session,
            presence,
            mut hand_rx,
            mut events,
            ..
        } = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (r, wh, account) = registered_slot_puppet(
            &mut session,
            &presence,
            &mut writer,
            &mut events,
            &mut hand_rx,
        )
        .await;
        drop(wh);
        drop(r);
        let ev = drive_until(&mut session, &presence, &mut events, |ev| {
            matches!(ev, PuppetEvent::Ended { .. })
        })
        .await;
        on_puppet_event(&mut session, &presence, ev);
        assert_eq!(slots_of(&session).free(), 0, "held");
        session.discovery = Discovery::from_srv(HashMap::new());
        puppets_tick(&mut session, &presence);
        assert_eq!(
            slots_of(&session).free(),
            0,
            "the Cancel does not undo the hold"
        );
        assert!(
            session.membership.is_owned_by_account(&account)
                && !session.membership.is_present(&account)
        );
        on_irc_line(&mut session, &presence, &mut writer, ":cc-1!u@h QUIT :bye").unwrap();
        assert_eq!(slots_of(&session).free(), 1, "released at the QUIT");
        assert!(!session.membership.is_owned_by_account(&account));
    }

    #[tokio::test]
    async fn a_rename_cycle_the_puppet_reported_is_not_replayed_by_late_echoes() {
        // The puppet reports A→B then B→A; the main connection's echoes of
        // both arrive after. Both sources echo the same server sequence, so
        // the late echoes are renames already applied and move nothing —
        // and the old spellings they name are held, so nothing is fronted.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, mut presence_rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, mut wh) =
            registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        let _ = read_until(&mut r, "JOIN ").await;
        on_irc_line(&mut session, &presence, &mut writer, ":cc-abc!u@h JOIN #mu").unwrap();
        while presence_rx.try_recv().is_ok() {}
        wh.write_all(b":cc-abc!u@h NICK :cc-b\r\n").await.unwrap();
        next_renamed(&mut session, &presence, &mut events).await;
        wh.write_all(b":cc-b!u@h NICK :cc-abc\r\n").await.unwrap();
        next_renamed(&mut session, &presence, &mut events).await;
        assert_eq!(pool_nick(&session), Some("cc-abc"), "back where it started");
        wire_nick(&mut session, &presence, &mut writer, "cc-abc", "cc-b");
        assert_eq!(
            pool_nick(&session),
            Some("cc-abc"),
            "a late echo of rename 1 is not replayed"
        );
        assert!(
            !session.membership.is_present("cc-b"),
            "the roster's spelling is ours meanwhile"
        );
        wire_nick(&mut session, &presence, &mut writer, "cc-b", "cc-abc");
        assert_eq!(pool_nick(&session), Some("cc-abc"));
        assert!(
            presence_rx.try_recv().is_err(),
            "no presence op for our own puppet"
        );
        // A fresh rename after the cycle still applies.
        wh.write_all(b":cc-abc!u@h NICK :cc-c\r\n").await.unwrap();
        next_renamed(&mut session, &presence, &mut events).await;
        assert_eq!(pool_nick(&session), Some("cc-c"));
        drop(wh);
        drop(r);
    }

    #[tokio::test]
    async fn teardown_takes_the_puppets_off_the_roster_before_dropping_ownership() {
        // A reconnect tears the pool down while the puppets are still on
        // this connection's roster. Ownership dropped first would front them
        // as humans — phantoms the next session would never withdraw.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, mut presence_rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (_r, _wh) = registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        on_irc_line(&mut session, &presence, &mut writer, ":cc-abc!u@h JOIN #mu").unwrap();
        while presence_rx.try_recv().is_ok() {}
        let mut budget = AttemptBudget::new();
        teardown_puppets(&mut session, &presence, &mut budget).await;
        assert!(session.puppets.is_none());
        assert!(
            !session.membership.is_listed("cc-abc"),
            "off the roster, as its QUIT would take it"
        );
        assert!(!session.membership.is_present("cc-abc"), "never a human");
        assert!(
            presence_rx.try_recv().is_err(),
            "no presence op for the gateway's own puppet"
        );
    }

    #[tokio::test]
    async fn an_unattributed_provisioned_puppet_is_held_at_its_departure_too() {
        // The main connection lists the puppet without an account (a JOIN
        // before the WHOX pass answers, or a server without account caps):
        // membership knows it as ours by the nick fallback, and the
        // departure hold must recognise it the same way.
        let Leased {
            mut session,
            presence,
            mut hand_rx,
            mut events,
            ..
        } = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (r, wh, _) = register_slot_puppet(
            &mut session,
            &presence,
            &mut writer,
            &mut events,
            &mut hand_rx,
        )
        .await;
        on_irc_line(&mut session, &presence, &mut writer, ":cc-1!u@h JOIN #mu").unwrap();
        assert!(
            session.membership.is_owned("cc-1") && !session.membership.is_owned_by_account("cc-1")
        );
        drop(wh);
        drop(r);
        let ev = drive_until(&mut session, &presence, &mut events, |ev| {
            matches!(ev, PuppetEvent::Ended { .. })
        })
        .await;
        on_puppet_event(&mut session, &presence, ev);
        assert_eq!(
            slots_of(&session).free(),
            0,
            "held: listed, and ours by nick"
        );
        assert!(session.membership.is_owned("cc-1") && !session.membership.is_present("cc-1"));
        on_irc_line(&mut session, &presence, &mut writer, ":cc-1!u@h QUIT :bye").unwrap();
        assert_eq!(slots_of(&session).free(), 1);
        assert!(!session.membership.is_owned("cc-1"));
    }

    #[tokio::test]
    async fn an_evicted_peers_queued_connect_is_unwound_and_its_attempt_refunded() {
        // cc:abc holds the one slot, idle, backing off after a drop with its
        // departure held. A newcomer that sorts first (cc:aaa) evicts it in
        // the tick that also emitted cc:abc's retry Connect. The eviction
        // unwinds that never-dialled Connect (attempt refunded); when the
        // newcomer's own dial is refused — its credential broke meanwhile —
        // cc:abc's queued Connect finds the pool has moved it and unwinds
        // the same way. No dial, one attempt on the books (the original
        // registration), and the slot still held: cc:abc's puppet is on the
        // roster under it until its QUIT is seen here.
        let Leased {
            mut session,
            presence,
            mut hand_rx,
            mut events,
            dir,
            ..
        } = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (r, wh, _) = registered_slot_puppet(
            &mut session,
            &presence,
            &mut writer,
            &mut events,
            &mut hand_rx,
        )
        .await;
        drop(wh);
        drop(r);
        let ev = drive_until(&mut session, &presence, &mut events, |ev| {
            matches!(ev, PuppetEvent::Ended { .. })
        })
        .await;
        on_puppet_event(&mut session, &presence, ev);
        assert_eq!(slots_of(&session).free(), 0, "held");
        std::fs::remove_file(dir.join("cc-1.key")).unwrap();
        advance_puppet_clock(&mut session, 31);
        session.discovery = Discovery::from_srv(HashMap::from([
            ("cc:aaa".to_string(), "mu.agent.cc.aaa.dm".to_string()),
            ("cc:abc".to_string(), "mu.agent.cc.abc.dm".to_string()),
        ]));
        puppets_tick(&mut session, &presence);
        assert!(
            hand_rx.try_recv().is_err(),
            "no dial: the newcomer was refused, the evicted peer's Connect was stale"
        );
        assert_eq!(
            slots_of(&session).free(),
            0,
            "the refusal returns no lease the evicted puppet is still listed under"
        );
        assert_eq!(slots_of(&session).evictions(), 1);
        assert!(
            session.membership.is_owned_by_account("cc-1")
                && !session.membership.is_present("cc-1"),
            "still ours, not a human"
        );
        let now = session.puppet_now_ms();
        assert_eq!(
            session.puppets.as_mut().unwrap().pool.attempts_in_window(now),
            1,
            "only the original registration is on the books: both never-dialled attempts were refunded"
        );
        on_irc_line(&mut session, &presence, &mut writer, ":cc-1!u@h QUIT :bye").unwrap();
        assert_eq!(
            slots_of(&session).free(),
            1,
            "released at the QUIT: the refused newcomer holding it is neither live nor backed"
        );
        assert!(!session.membership.is_owned_by_account("cc-1"));
    }

    #[tokio::test]
    async fn an_evicted_peer_already_dialling_keeps_its_attempt_on_the_books() {
        // cc:abc's Connect ran in an earlier tick: its socket is open, the
        // server saw the attempt. Evicted while still registering (idle
        // window shorter than a registration), its attempt is NOT refunded —
        // only a Connect this batch has not reached is a free one.
        let Leased {
            mut session,
            presence,
            mut hand_rx,
            ..
        } = leased(1);
        discover_abc(&mut session);
        puppets_tick(&mut session, &presence);
        let (nick, _a) = tokio::time::timeout(Duration::from_secs(5), hand_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(nick, "cc-1", "dialling, not yet registered");
        advance_puppet_clock(&mut session, 31);
        session.discovery = abc_and_def();
        puppets_tick(&mut session, &presence);
        let (nick, _b) = tokio::time::timeout(Duration::from_secs(5), hand_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(nick, "cc-1", "the newcomer dials as the evicted account");
        assert_eq!(slots_of(&session).evictions(), 1);
        let now = session.puppet_now_ms();
        assert_eq!(
            session
                .puppets
                .as_mut()
                .unwrap()
                .pool
                .attempts_in_window(now),
            2,
            "both attempts opened a socket; both stay charged"
        );
    }

    #[tokio::test]
    async fn a_provisioned_puppets_rename_holds_the_old_spelling_and_correlates_both_sources() {
        // Provisioned but unattributed on this connection (a plain JOIN):
        // ours by spelling alone. The puppet reports A→B first: A is held
        // until the echo, which finds its holder through the hold and is not
        // replayed. Then the main connection's NICK B→C arrives BEFORE the
        // puppet's report: an echo ahead of the reports applies, and the
        // report that follows is carried.
        let Leased {
            mut session,
            presence,
            mut hand_rx,
            mut events,
            ..
        } = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, mut wh, _) = register_slot_puppet(
            &mut session,
            &presence,
            &mut writer,
            &mut events,
            &mut hand_rx,
        )
        .await;
        let _ = read_until(&mut r, "JOIN ").await;
        on_irc_line(&mut session, &presence, &mut writer, ":cc-1!u@h JOIN #mu").unwrap();
        assert!(
            session.membership.is_owned("cc-1") && !session.membership.is_owned_by_account("cc-1")
        );
        wh.write_all(b":cc-1!u@h NICK :cc-1b\r\n").await.unwrap();
        next_renamed(&mut session, &presence, &mut events).await;
        assert_eq!(pool_nick(&session), Some("cc-1b"));
        assert!(
            session.membership.is_owned("cc-1"),
            "the old spelling is held"
        );
        assert!(!session.membership.is_present("cc-1"));
        wire_nick(&mut session, &presence, &mut writer, "cc-1", "cc-1b");
        assert_eq!(
            pool_nick(&session),
            Some("cc-1b"),
            "the echo is not replayed"
        );
        assert!(!session.membership.is_owned("cc-1"), "settled by the echo");
        wire_nick(&mut session, &presence, &mut writer, "cc-1b", "cc-1c");
        assert_eq!(
            pool_nick(&session),
            Some("cc-1c"),
            "an echo ahead of the reports applies"
        );
        wh.write_all(b":cc-1b!u@h NICK :cc-1c\r\n").await.unwrap();
        next_renamed(&mut session, &presence, &mut events).await;
        assert_eq!(
            pool_nick(&session),
            Some("cc-1c"),
            "the report that follows is carried"
        );
        assert!(session.membership.is_owned("cc-1c") && !session.membership.is_present("cc-1c"));
        drop(wh);
        drop(r);
    }

    #[tokio::test]
    async fn a_connect_unwound_by_an_eviction_does_not_take_a_second_slot() {
        // Two slots, both held by idle, backing-off peers. A newcomer that
        // sorts first evicts cc:abc from cc-1 in the tick that also emitted
        // cc:abc's own retry Connect. That Connect, run, would evict cc:xyz
        // from cc-2 and dial a peer the pool has backing off — with a valid
        // credential, and its attempt already refunded. It is skipped.
        let Leased {
            mut session,
            presence,
            mut hand_rx,
            mut events,
            ..
        } = leased(2);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        session.discovery = Discovery::from_srv(HashMap::from([
            ("cc:abc".to_string(), "mu.agent.cc.abc.dm".to_string()),
            ("cc:xyz".to_string(), "mu.agent.cc.xyz.dm".to_string()),
        ]));
        puppets_tick(&mut session, &presence);
        let mut halves = Vec::new();
        for expect in ["cc-1", "cc-2"] {
            let (nick, server) = tokio::time::timeout(Duration::from_secs(5), hand_rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(nick, expect);
            halves.push(sasl_register(server, expect, None).await);
            let ev = next(&mut events).await;
            assert!(matches!(&ev, PuppetEvent::Registered { .. }), "{ev:?}");
            on_puppet_event(&mut session, &presence, ev);
            on_irc_line(
                &mut session,
                &presence,
                &mut writer,
                &format!(":{expect}!u@h JOIN #mu {expect} :puppet"),
            )
            .unwrap();
        }
        assert_eq!(slots_of(&session).free(), 0);
        drop(halves);
        for _ in 0..2 {
            let ev = drive_until(&mut session, &presence, &mut events, |ev| {
                matches!(ev, PuppetEvent::Ended { .. })
            })
            .await;
            on_puppet_event(&mut session, &presence, ev);
        }
        assert_eq!(slots_of(&session).free(), 0, "both held");
        advance_puppet_clock(&mut session, 31);
        session.discovery = Discovery::from_srv(HashMap::from([
            ("cc:aaa".to_string(), "mu.agent.cc.aaa.dm".to_string()),
            ("cc:abc".to_string(), "mu.agent.cc.abc.dm".to_string()),
            ("cc:xyz".to_string(), "mu.agent.cc.xyz.dm".to_string()),
        ]));
        puppets_tick(&mut session, &presence);
        // Two dials: the newcomer as cc-1, cc:xyz's own retry as cc-2.
        let (n1, _s1) = tokio::time::timeout(Duration::from_secs(5), hand_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let (n2, _s2) = tokio::time::timeout(Duration::from_secs(5), hand_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!((n1.as_str(), n2.as_str()), ("cc-1", "cc-2"));
        assert!(hand_rx.try_recv().is_err(), "no third dial");
        let slots = slots_of(&session);
        assert_eq!(slots.evictions(), 1, "one eviction, not a cascade");
        assert_eq!(slots.account_of(&PeerId::parse("cc:aaa")), Some("cc-1"));
        assert_eq!(
            slots.account_of(&PeerId::parse("cc:abc")),
            None,
            "unwound, not re-leased"
        );
        assert_eq!(
            slots.account_of(&PeerId::parse("cc:xyz")),
            Some("cc-2"),
            "kept its slot"
        );
    }

    #[tokio::test]
    async fn an_evicted_puppets_forced_nick_is_held_until_the_main_connection_sees_it_leave() {
        // The victim was force-renamed by the server and is unattributed on
        // this connection: after its eviction the newcomer's lease keeps the
        // ACCOUNT owned, but the victim's spelling on the roster is another
        // one — held, like any other departure, until its QUIT is seen here.
        let Leased {
            mut session,
            presence,
            mut hand_rx,
            mut events,
            ..
        } = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, mut wh, _) = register_slot_puppet(
            &mut session,
            &presence,
            &mut writer,
            &mut events,
            &mut hand_rx,
        )
        .await;
        let _ = read_until(&mut r, "JOIN ").await;
        on_irc_line(&mut session, &presence, &mut writer, ":cc-1!u@h JOIN #mu").unwrap();
        wh.write_all(b":cc-1!u@h NICK :cc-1x\r\n").await.unwrap();
        next_renamed(&mut session, &presence, &mut events).await;
        wire_nick(&mut session, &presence, &mut writer, "cc-1", "cc-1x");
        assert!(
            session.membership.is_owned("cc-1x")
                && !session.membership.is_owned_by_account("cc-1x")
        );
        advance_puppet_clock(&mut session, 31);
        session.discovery = abc_and_def();
        puppets_tick(&mut session, &presence);
        read_until(&mut r, "QUIT").await;
        let (nick, _b) = tokio::time::timeout(Duration::from_secs(5), hand_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(nick, "cc-1", "the newcomer dials as the account");
        assert!(
            session.membership.is_owned("cc-1x"),
            "the evicted spelling is held"
        );
        assert!(!session.membership.is_present("cc-1x"), "not a human");
        on_irc_line(&mut session, &presence, &mut writer, ":cc-1x!u@h QUIT :bye").unwrap();
        assert!(!session.membership.is_owned("cc-1x"), "let go at the QUIT");
        drop(wh);
        drop(r);
    }

    #[tokio::test]
    async fn a_refused_newcomer_keeps_the_account_its_evicted_puppet_is_still_listed_under() {
        // The newcomer's credential is gone by the time the eviction hands
        // it the account. Its dial is refused — and the refusal must not
        // return the lease: the evicted puppet is still on the roster under
        // that account, its QUIT not yet seen here, and an account let go
        // would front it as a human (R1). The hold owns the release.
        let Leased {
            mut session,
            presence,
            mut hand_rx,
            mut events,
            dir,
            ..
        } = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, wh, account) = registered_slot_puppet(
            &mut session,
            &presence,
            &mut writer,
            &mut events,
            &mut hand_rx,
        )
        .await;
        let _ = read_until(&mut r, "JOIN ").await;
        std::fs::remove_file(dir.join("cc-1.key")).unwrap();
        advance_puppet_clock(&mut session, 31);
        session.discovery = abc_and_def();
        puppets_tick(&mut session, &presence);
        read_until(&mut r, "QUIT").await;
        assert!(
            hand_rx.try_recv().is_err(),
            "the newcomer's dial was refused"
        );
        assert_eq!(slots_of(&session).evictions(), 1);
        assert_eq!(
            slots_of(&session).free(),
            0,
            "the refusal returns no lease the evicted puppet is still listed under"
        );
        assert!(
            session.membership.is_owned_by_account(&account)
                && !session.membership.is_present(&account),
            "still ours, not a human"
        );
        drop(wh);
        drop(r);
        let ev = drive_until(&mut session, &presence, &mut events, |ev| {
            matches!(ev, PuppetEvent::Ended { .. })
        })
        .await;
        on_puppet_event(&mut session, &presence, ev);
        assert_eq!(
            slots_of(&session).free(),
            0,
            "its own connection's end changes nothing: the QUIT is not seen here yet"
        );
        on_irc_line(&mut session, &presence, &mut writer, ":cc-1!u@h QUIT :bye").unwrap();
        assert_eq!(
            slots_of(&session).free(),
            1,
            "released at the QUIT: the newcomer holding it is neither live nor backed"
        );
        assert!(!session.membership.is_owned_by_account(&account));
    }

    #[tokio::test]
    async fn two_departures_under_one_account_let_it_go_at_the_second_quit() {
        // cc:abc is evicted for cc:def, which registers as the same account
        // and is then quit by the pool — before the main connection has
        // seen either leave. Its first QUIT is cc:abc's and settles that
        // hold; the account stays leased, because cc:def's puppet is about
        // to be listed under it. The second QUIT lets it go.
        let Leased {
            mut session,
            presence,
            mut hand_rx,
            mut events,
            ..
        } = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r1, wh1, account) = registered_slot_puppet(
            &mut session,
            &presence,
            &mut writer,
            &mut events,
            &mut hand_rx,
        )
        .await;
        let _ = read_until(&mut r1, "JOIN ").await;
        advance_puppet_clock(&mut session, 31);
        session.discovery = abc_and_def();
        puppets_tick(&mut session, &presence);
        read_until(&mut r1, "QUIT").await;
        let (nick, server) = tokio::time::timeout(Duration::from_secs(5), hand_rx.recv())
            .await
            .expect("the newcomer dials")
            .unwrap();
        assert_eq!(nick, account);
        let (mut r2, wh2) = sasl_register(server, "cc-1", Some("AUTHENTICATE Y2MtMQ==")).await;
        let ev = drive_until(&mut session, &presence, &mut events, |ev| {
            matches!(ev, PuppetEvent::Registered { .. })
        })
        .await;
        on_puppet_event(&mut session, &presence, ev);
        drop(wh1);
        drop(r1);
        let ev = drive_until(&mut session, &presence, &mut events, |ev| {
            matches!(ev, PuppetEvent::Ended { .. })
        })
        .await;
        on_puppet_event(&mut session, &presence, ev);
        // The pool quits cc:def too: gone from discovery.
        session.discovery = Discovery::from_srv(HashMap::new());
        puppets_tick(&mut session, &presence);
        read_until(&mut r2, "QUIT").await;
        assert_eq!(
            session.puppets.as_ref().unwrap().pending_release.len(),
            2,
            "two departures under the account, neither seen here"
        );
        on_irc_line(&mut session, &presence, &mut writer, ":cc-1!u@h QUIT :bye").unwrap();
        assert_eq!(
            slots_of(&session).free(),
            0,
            "the first QUIT settles one departure; the other still backs the account"
        );
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":cc-1!u@h JOIN #mu cc-1 :puppet",
        )
        .unwrap();
        assert!(
            session.membership.is_owned_by_account(&account)
                && !session.membership.is_present(&account),
            "the second puppet is listed as ours, not a human"
        );
        on_irc_line(&mut session, &presence, &mut writer, ":cc-1!u@h QUIT :bye").unwrap();
        assert_eq!(slots_of(&session).free(), 1, "let go at the second QUIT");
        assert!(session.puppets.as_ref().unwrap().pending_release.is_empty());
        drop(wh2);
        drop(r2);
    }

    #[tokio::test]
    async fn a_peers_second_departure_is_held_for_its_own_quit() {
        // The puppet's socket drops; it dials again and drops again before
        // the main connection has processed the first QUIT. Two departures
        // of one peer, under one spelling and one account: the first QUIT
        // settles the first — not both — and the account stays ours while
        // the second connection's JOIN and QUIT are still to come, or that
        // JOIN would be read as a human's.
        let Leased {
            mut session,
            presence,
            mut hand_rx,
            mut events,
            ..
        } = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (r, wh, account) = registered_slot_puppet(
            &mut session,
            &presence,
            &mut writer,
            &mut events,
            &mut hand_rx,
        )
        .await;
        drop(wh);
        drop(r);
        let ev = drive_until(&mut session, &presence, &mut events, |ev| {
            matches!(ev, PuppetEvent::Ended { .. })
        })
        .await;
        on_puppet_event(&mut session, &presence, ev);
        advance_puppet_clock(&mut session, 10);
        puppets_tick(&mut session, &presence);
        let (nick, server) = tokio::time::timeout(Duration::from_secs(5), hand_rx.recv())
            .await
            .expect("dialled again")
            .unwrap();
        assert_eq!(nick, account);
        let (r2, wh2) = sasl_register(server, "cc-1", Some("AUTHENTICATE Y2MtMQ==")).await;
        let ev = drive_until(&mut session, &presence, &mut events, |ev| {
            matches!(ev, PuppetEvent::Registered { .. })
        })
        .await;
        on_puppet_event(&mut session, &presence, ev);
        drop(wh2);
        drop(r2);
        let ev = drive_until(&mut session, &presence, &mut events, |ev| {
            matches!(ev, PuppetEvent::Ended { .. })
        })
        .await;
        on_puppet_event(&mut session, &presence, ev);
        assert_eq!(
            session.puppets.as_ref().unwrap().pending_release.len(),
            2,
            "two departures, one connection each"
        );
        on_irc_line(&mut session, &presence, &mut writer, ":cc-1!u@h QUIT :bye").unwrap();
        assert_eq!(
            slots_of(&session).free(),
            0,
            "the first QUIT settles the first departure only"
        );
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":cc-1!u@h JOIN #mu cc-1 :puppet",
        )
        .unwrap();
        assert!(
            session.membership.is_owned_by_account(&account)
                && !session.membership.is_present(&account),
            "the second connection's JOIN is ours, not a human's"
        );
        on_irc_line(&mut session, &presence, &mut writer, ":cc-1!u@h QUIT :bye").unwrap();
        assert_eq!(slots_of(&session).free(), 1, "let go at the second QUIT");
    }

    #[tokio::test]
    async fn a_registered_puppet_whose_join_cannot_be_queued_is_quit_and_dialled_again() {
        // The task's command queue is full when the session tells it to
        // JOIN. A registered puppet with no way to be told anything is not
        // left standing: it is quit — the stop reaches a task whatever it is
        // parked on — its spelling held until the main connection sees it
        // leave, and the pool backs off to dial again.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        discover_abc(&mut session);
        puppets_tick(&mut session, &presence);
        let (_nick, server) = hand_rx.recv().await.unwrap();
        let (rh, mut wh) = tokio::io::split(server);
        let mut r = BufReader::new(rh);
        read_until(&mut r, "NICK ").await;
        wh.write_all(b":srv CAP * LS :\r\n").await.unwrap();
        wh.write_all(b":srv 001 cc-abc :Welcome\r\n").await.unwrap();
        let ev = next(&mut events).await;
        assert!(matches!(ev, PuppetEvent::Registered { .. }), "{ev:?}");
        // The queue is filled before the session learns of the registration;
        // the task cannot drain it without being polled.
        let abc = PeerId::parse("cc:abc");
        {
            let p = session.puppets.as_mut().unwrap();
            let mut queued = 0;
            while p.exec.command(&abc, PuppetCommand::Send("PING :x".into())) {
                queued += 1;
                assert!(queued < 10_000, "the command queue is bounded");
            }
        }
        on_puppet_event(&mut session, &presence, ev);
        let p = session.puppets.as_ref().unwrap();
        assert!(
            matches!(p.pool.state_of(&abc), Some(PuppetState::BackingOff { .. })),
            "quit, and backing off: {:?}",
            p.pool.state_of(&abc)
        );
        assert!(
            session.membership.is_owned("cc-abc"),
            "held until the main connection sees it leave"
        );
        let quit = read_until(&mut r, "QUIT").await;
        assert!(quit.starts_with("QUIT"), "{quit}");
        drop(wh);
        drop(r);
        let ev = drive_until(&mut session, &presence, &mut events, |ev| {
            matches!(ev, PuppetEvent::Ended { .. })
        })
        .await;
        on_puppet_event(&mut session, &presence, ev);
        advance_puppet_clock(&mut session, 10);
        puppets_tick(&mut session, &presence);
        let (nick, _server) = tokio::time::timeout(Duration::from_secs(5), hand_rx.recv())
            .await
            .expect("dialled again")
            .unwrap();
        assert_eq!(nick, "cc-abc");
    }

    #[tokio::test]
    async fn a_humans_quit_under_a_held_spelling_settles_nothing() {
        // The puppet reports cc-1→cc-1x before sharing a channel: cc-1 is
        // held with no echo to come, and a human takes the name. The puppet
        // then drops while listed as cc-1x under our account. The human's
        // QUIT at cc-1 is not the departure the hold waits for: it settles
        // nothing, and cc-1x stays ours until its own QUIT.
        let Leased {
            mut session,
            presence,
            mut hand_rx,
            mut events,
            ..
        } = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, mut wh, _account) = register_slot_puppet(
            &mut session,
            &presence,
            &mut writer,
            &mut events,
            &mut hand_rx,
        )
        .await;
        let _ = read_until(&mut r, "JOIN ").await;
        wh.write_all(b":cc-1!u@h NICK :cc-1x\r\n").await.unwrap();
        next_renamed(&mut session, &presence, &mut events).await;
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":cc-1!h@h JOIN #mu bob :Bob",
        )
        .unwrap();
        assert!(
            session.membership.is_present("cc-1") && !session.membership.is_owned("cc-1"),
            "the human under the held name is fronted"
        );
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":cc-1x!u@h JOIN #mu cc-1 :puppet",
        )
        .unwrap();
        drop(wh);
        drop(r);
        let ev = drive_until(&mut session, &presence, &mut events, |ev| {
            matches!(ev, PuppetEvent::Ended { .. })
        })
        .await;
        on_puppet_event(&mut session, &presence, ev);
        assert_eq!(
            slots_of(&session).free(),
            0,
            "held: cc-1x is listed as ours"
        );
        on_irc_line(&mut session, &presence, &mut writer, ":cc-1!h@h QUIT :bye").unwrap();
        assert_eq!(
            slots_of(&session).free(),
            0,
            "the human's QUIT settles nothing"
        );
        assert!(
            session.membership.is_owned_by_account("cc-1x")
                && !session.membership.is_present("cc-1x"),
            "cc-1x is still ours, not a human"
        );
        on_irc_line(&mut session, &presence, &mut writer, ":cc-1x!u@h QUIT :bye").unwrap();
        assert_eq!(
            slots_of(&session).free(),
            1,
            "let go at the puppet's own QUIT"
        );
    }

    #[tokio::test]
    async fn a_humans_rename_under_a_held_spelling_settles_nothing() {
        // As for a human's QUIT: the puppet renamed away from cc-1 with no
        // echo to come and a human took the name. The human's own rename
        // is not the departure cc-1's hold waits for — the hold stays, and
        // nothing follows the human to their new name.
        let Leased {
            mut session,
            presence,
            mut hand_rx,
            mut events,
            ..
        } = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, mut wh, _account) = register_slot_puppet(
            &mut session,
            &presence,
            &mut writer,
            &mut events,
            &mut hand_rx,
        )
        .await;
        let _ = read_until(&mut r, "JOIN ").await;
        wh.write_all(b":cc-1!u@h NICK :cc-1x\r\n").await.unwrap();
        next_renamed(&mut session, &presence, &mut events).await;
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":cc-1!h@h JOIN #mu bob :Bob",
        )
        .unwrap();
        wire_nick(&mut session, &presence, &mut writer, "cc-1", "bob2");
        let held: Vec<String> = session
            .puppets
            .as_ref()
            .unwrap()
            .pending_release
            .iter()
            .map(|pr| pr.nick.clone())
            .collect();
        assert_eq!(
            held,
            vec!["cc-1".to_string()],
            "the hold neither settled nor moved"
        );
        assert!(
            session.membership.is_present("bob2") && !session.membership.is_owned("bob2"),
            "the human is a human under the new name too"
        );
        drop(wh);
        drop(r);
    }

    #[tokio::test]
    async fn a_departed_connections_echo_does_not_rename_its_replacement() {
        // The puppet reports cc-abc→cc-b and its connection ends; it dials
        // again and registers as cc-abc (the server freed it) before the
        // main connection's NICK cc-abc→cc-b — the OLD connection's echo —
        // arrives. That echo is not the replacement's rename: the pool stays
        // at cc-abc, and the old connection's hold moves to cc-b until its
        // QUIT is seen.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, mut wh) =
            registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        let _ = read_until(&mut r, "JOIN ").await;
        on_irc_line(&mut session, &presence, &mut writer, ":cc-abc!u@h JOIN #mu").unwrap();
        wh.write_all(b":cc-abc!u@h NICK :cc-b\r\n").await.unwrap();
        next_renamed(&mut session, &presence, &mut events).await;
        assert_eq!(pool_nick(&session), Some("cc-b"));
        drop(wh);
        drop(r);
        let ev = drive_until(&mut session, &presence, &mut events, |ev| {
            matches!(ev, PuppetEvent::Ended { .. })
        })
        .await;
        on_puppet_event(&mut session, &presence, ev);
        // Within the departure window (the hold on cc-abc is still pending).
        advance_puppet_clock(&mut session, 3);
        puppets_tick(&mut session, &presence);
        let (nick, server) = tokio::time::timeout(Duration::from_secs(5), hand_rx.recv())
            .await
            .expect("dialled again")
            .unwrap();
        assert_eq!(nick, "cc-abc", "back under the name the server freed");
        let (rh, mut wh2) = tokio::io::split(server);
        let mut r2 = BufReader::new(rh);
        read_until(&mut r2, "NICK ").await;
        wh2.write_all(b":srv CAP * LS :\r\n").await.unwrap();
        wh2.write_all(b":srv 001 cc-abc :Welcome\r\n")
            .await
            .unwrap();
        let ev = drive_until(&mut session, &presence, &mut events, |ev| {
            matches!(ev, PuppetEvent::Registered { .. })
        })
        .await;
        on_puppet_event(&mut session, &presence, ev);
        assert_eq!(pool_nick(&session), Some("cc-abc"));
        wire_nick(&mut session, &presence, &mut writer, "cc-abc", "cc-b");
        assert_eq!(
            pool_nick(&session),
            Some("cc-abc"),
            "the old connection's echo does not rename the replacement"
        );
        assert!(
            session.membership.is_owned("cc-b"),
            "the old connection's hold moved to cc-b"
        );
        on_irc_line(&mut session, &presence, &mut writer, ":cc-b!u@h QUIT :bye").unwrap();
        assert!(!session.membership.is_owned("cc-b"), "settled by its QUIT");
        assert_eq!(pool_nick(&session), Some("cc-abc"));
        drop(wh2);
        drop(r2);
    }

    #[tokio::test]
    async fn a_puppet_dropped_after_a_rename_report_stays_held_across_the_echo() {
        // The puppet reports cc-1→cc-1x and its socket drops before the main
        // connection's NICK. Its last spelling is not listed here, but the
        // old one is held: the lease stays. At the echo the hold moves to
        // the new spelling (the departure is still unseen); the QUIT ends it.
        let Leased {
            mut session,
            presence,
            mut hand_rx,
            mut events,
            mut presence_rx,
            ..
        } = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, mut wh, _) = registered_slot_puppet(
            &mut session,
            &presence,
            &mut writer,
            &mut events,
            &mut hand_rx,
        )
        .await;
        let _ = read_until(&mut r, "JOIN ").await;
        while presence_rx.try_recv().is_ok() {}
        wh.write_all(b":cc-1!u@h NICK :cc-1x\r\n").await.unwrap();
        next_renamed(&mut session, &presence, &mut events).await;
        drop(wh);
        drop(r);
        let ev = drive_until(&mut session, &presence, &mut events, |ev| {
            matches!(ev, PuppetEvent::Ended { .. })
        })
        .await;
        on_puppet_event(&mut session, &presence, ev);
        assert_eq!(slots_of(&session).free(), 0, "held under the old spelling");
        assert!(!session.membership.is_present("cc-1"));
        wire_nick(&mut session, &presence, &mut writer, "cc-1", "cc-1x");
        assert_eq!(
            slots_of(&session).free(),
            0,
            "the hold moved with the echo; the departure is still unseen"
        );
        assert!(session.membership.is_owned("cc-1x") && !session.membership.is_present("cc-1x"));
        on_irc_line(&mut session, &presence, &mut writer, ":cc-1x!u@h QUIT :bye").unwrap();
        assert_eq!(slots_of(&session).free(), 1, "released at the QUIT");
        assert!(!session.membership.is_owned("cc-1x"));
        assert!(
            presence_rx.try_recv().is_err(),
            "no presence op for our own puppet"
        );
    }

    #[tokio::test]
    async fn a_spelling_revisited_by_a_rename_cycle_stays_held_until_its_last_echo() {
        // Reports A→B, B→A, A→B before any echo: A is left twice. The first
        // echo of A→B settles one departure from A, not both — A is still
        // on the roster, about to be left again.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, mut presence_rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, mut wh) =
            registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        let _ = read_until(&mut r, "JOIN ").await;
        on_irc_line(&mut session, &presence, &mut writer, ":cc-abc!u@h JOIN #mu").unwrap();
        while presence_rx.try_recv().is_ok() {}
        for line in [
            ":cc-abc!u@h NICK :cc-b\r\n",
            ":cc-b!u@h NICK :cc-abc\r\n",
            ":cc-abc!u@h NICK :cc-b\r\n",
        ] {
            wh.write_all(line.as_bytes()).await.unwrap();
            next_renamed(&mut session, &presence, &mut events).await;
        }
        assert_eq!(pool_nick(&session), Some("cc-b"));
        wire_nick(&mut session, &presence, &mut writer, "cc-abc", "cc-b");
        assert!(
            session.membership.is_owned("cc-abc"),
            "left once more still: held"
        );
        wire_nick(&mut session, &presence, &mut writer, "cc-b", "cc-abc");
        assert!(session.membership.is_owned("cc-abc") && !session.membership.is_present("cc-abc"));
        wire_nick(&mut session, &presence, &mut writer, "cc-abc", "cc-b");
        assert!(
            !session.membership.is_owned("cc-abc"),
            "the last echo lets it go"
        );
        assert_eq!(session.membership.owned_nicks(), vec!["cc-b"]);
        assert!(
            presence_rx.try_recv().is_err(),
            "no presence op for our own puppet"
        );
        drop(wh);
        drop(r);
    }

    #[tokio::test]
    async fn a_hold_survives_a_casemapping_change() {
        // A held spelling whose fold differs under the new CASEMAPPING must
        // still be settled by its QUIT, folded the new way.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, mut wh) =
            registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        let _ = read_until(&mut r, "JOIN ").await;
        on_irc_line(&mut session, &presence, &mut writer, ":cc-abc!u@h JOIN #mu").unwrap();
        // Server-renamed to a spelling that folds differently under ascii.
        wh.write_all(b":cc-abc!u@h NICK :cc-a[x]\r\n")
            .await
            .unwrap();
        next_renamed(&mut session, &presence, &mut events).await;
        wire_nick(&mut session, &presence, &mut writer, "cc-abc", "cc-a[x]");
        drop(wh);
        drop(r);
        let ev = drive_until(&mut session, &presence, &mut events, |ev| {
            matches!(ev, PuppetEvent::Ended { .. })
        })
        .await;
        on_puppet_event(&mut session, &presence, ev);
        assert!(session.membership.is_owned("cc-a[x]"), "held");
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":srv 005 mu-gw CASEMAPPING=ascii :are supported by this server",
        )
        .unwrap();
        assert!(
            session.membership.is_owned("cc-a[x]"),
            "still held across the change"
        );
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":cc-a[x]!u@h QUIT :bye",
        )
        .unwrap();
        assert!(
            !session.membership.is_owned("cc-a[x]"),
            "settled by its QUIT under the new folding"
        );
    }

    #[tokio::test]
    async fn a_casemapping_change_keeps_both_holds_whose_spellings_now_fold_together() {
        // Two departures held under ascii as `cc[` and `cc{`; under rfc1459
        // those are one spelling. Both are still unseen, so both holds stay
        // — one per peer — and each QUIT settles its own.
        let Leased {
            mut session,
            presence,
            ..
        } = leased(2);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":srv 005 mu-gw CASEMAPPING=ascii :are supported by this server",
        )
        .unwrap();
        let (abc, def) = (PeerId::parse("cc:abc"), PeerId::parse("cc:def"));
        {
            let p = session.puppets.as_mut().unwrap();
            hold(p, &abc, 1, "cc[", 0, CaseMapping::Ascii);
            hold(p, &def, 1, "cc{", 0, CaseMapping::Ascii);
        }
        sync_owned_nicks(&mut session, &presence);
        assert!(session.membership.is_owned("cc[") && session.membership.is_owned("cc{"));
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":srv 005 mu-gw CASEMAPPING=rfc1459 :are supported by this server",
        )
        .unwrap();
        let held = |session: &Session| -> Vec<(PeerId, String)> {
            session
                .puppets
                .as_ref()
                .unwrap()
                .pending_release
                .iter()
                .map(|pr| (pr.peer.clone(), pr.nick.clone()))
                .collect()
        };
        assert_eq!(
            held(&session),
            vec![
                (abc.clone(), "cc[".to_string()),
                (def.clone(), "cc{".to_string())
            ],
            "both holds survive the fold"
        );
        on_irc_line(&mut session, &presence, &mut writer, ":cc{!u@h QUIT :bye").unwrap();
        assert_eq!(
            held(&session),
            vec![(abc.clone(), "cc[".to_string())],
            "a QUIT settles the hold under its own spelling"
        );
        on_irc_line(&mut session, &presence, &mut writer, ":cc[!u@h QUIT :bye").unwrap();
        assert!(held(&session).is_empty());
    }

    #[tokio::test]
    async fn a_refused_retry_keeps_a_lease_its_departure_hold_still_covers() {
        // The puppet's socket dropped; its lease is held until the QUIT is
        // seen here. Its retry finds the credential gone. A refusal returns
        // a lease — but not one a hold still covers: the puppet is on the
        // roster under our account, and releasing would front it.
        let Leased {
            mut session,
            presence,
            mut hand_rx,
            mut events,
            dir,
            ..
        } = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (r, wh, account) = registered_slot_puppet(
            &mut session,
            &presence,
            &mut writer,
            &mut events,
            &mut hand_rx,
        )
        .await;
        drop(wh);
        drop(r);
        let ev = drive_until(&mut session, &presence, &mut events, |ev| {
            matches!(ev, PuppetEvent::Ended { .. })
        })
        .await;
        on_puppet_event(&mut session, &presence, ev);
        assert_eq!(slots_of(&session).free(), 0, "held");
        std::fs::remove_file(dir.join("cc-1.key")).unwrap();
        advance_puppet_clock(&mut session, 3);
        puppets_tick(&mut session, &presence);
        assert!(hand_rx.try_recv().is_err(), "the retry was refused");
        assert_eq!(
            slots_of(&session).free(),
            0,
            "still held: the refusal does not undo the hold"
        );
        assert!(
            session.membership.is_owned_by_account(&account)
                && !session.membership.is_present(&account)
        );
        on_irc_line(&mut session, &presence, &mut writer, ":cc-1!u@h QUIT :bye").unwrap();
        assert_eq!(slots_of(&session).free(), 1, "released at the QUIT");
    }

    #[tokio::test]
    async fn a_quit_settles_every_hold_of_the_departed_peer() {
        // Reports A→B then B→A with no echoes yet, then the QUIT arrives
        // under A: the connection is gone, so B's hold goes with it rather
        // than outliving the peer until the window.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, mut wh) =
            registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        let _ = read_until(&mut r, "JOIN ").await;
        on_irc_line(&mut session, &presence, &mut writer, ":cc-abc!u@h JOIN #mu").unwrap();
        for line in [":cc-abc!u@h NICK :cc-b\r\n", ":cc-b!u@h NICK :cc-abc\r\n"] {
            wh.write_all(line.as_bytes()).await.unwrap();
            next_renamed(&mut session, &presence, &mut events).await;
        }
        assert_eq!(
            session.puppets.as_ref().unwrap().pending_release.len(),
            2,
            "A and B held"
        );
        drop(wh);
        drop(r);
        let ev = drive_until(&mut session, &presence, &mut events, |ev| {
            matches!(ev, PuppetEvent::Ended { .. })
        })
        .await;
        on_puppet_event(&mut session, &presence, ev);
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":cc-abc!u@h QUIT :bye",
        )
        .unwrap();
        assert!(
            session.puppets.as_ref().unwrap().pending_release.is_empty(),
            "every hold of the peer settled"
        );
        assert!(!session.membership.is_owned("cc-b") && !session.membership.is_owned("cc-abc"));
    }

    #[tokio::test]
    async fn a_rename_before_any_shared_channel_does_not_suppress_the_next_wire_first_rename() {
        // The puppet reports A→B while sharing no channel (no echo will ever
        // come), then joins the lobby as B, and the main connection sees
        // B→C before the puppet's report: a fresh rename, not an echo of
        // anything — it applies.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, mut presence_rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, mut wh) =
            registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        let _ = read_until(&mut r, "JOIN ").await;
        wh.write_all(b":cc-abc!u@h NICK :cc-b\r\n").await.unwrap();
        next_renamed(&mut session, &presence, &mut events).await;
        assert_eq!(pool_nick(&session), Some("cc-b"));
        on_irc_line(&mut session, &presence, &mut writer, ":cc-b!u@h JOIN #mu").unwrap();
        while presence_rx.try_recv().is_ok() {}
        wire_nick(&mut session, &presence, &mut writer, "cc-b", "cc-c");
        assert_eq!(
            pool_nick(&session),
            Some("cc-c"),
            "a rename the main connection saw first applies"
        );
        wh.write_all(b":cc-b!u@h NICK :cc-c\r\n").await.unwrap();
        next_renamed(&mut session, &presence, &mut events).await;
        assert_eq!(
            pool_nick(&session),
            Some("cc-c"),
            "the report that follows is carried"
        );
        assert!(session.membership.is_owned("cc-c") && !session.membership.is_present("cc-c"));
        assert!(presence_rx.try_recv().is_err());
        drop(wh);
        drop(r);
    }

    #[tokio::test]
    async fn a_report_past_its_echo_window_does_not_pass_for_a_later_echo() {
        // The puppet reports A→B sharing no channel: no echo will come, and
        // the transition must not wait for one for ever. Past the window,
        // in a shared channel, the main connection sees B→A and then A→B
        // first — the second is a fresh rename over the same spellings, not
        // the old report's echo, and the pool follows it.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, mut wh) =
            registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        let _ = read_until(&mut r, "JOIN ").await;
        wh.write_all(b":cc-abc!u@h NICK :cc-b\r\n").await.unwrap();
        next_renamed(&mut session, &presence, &mut events).await;
        assert_eq!(pool_nick(&session), Some("cc-b"));
        advance_puppet_clock(&mut session, 61);
        on_irc_line(&mut session, &presence, &mut writer, ":cc-b!u@h JOIN #mu").unwrap();
        wire_nick(&mut session, &presence, &mut writer, "cc-b", "cc-abc");
        assert_eq!(pool_nick(&session), Some("cc-abc"));
        wire_nick(&mut session, &presence, &mut writer, "cc-abc", "cc-b");
        assert_eq!(
            pool_nick(&session),
            Some("cc-b"),
            "a rename over the old report's spellings, past its window, is not its echo"
        );
        drop(wh);
        drop(r);
    }

    #[tokio::test]
    async fn a_report_past_its_window_is_dropped_when_the_next_is_added() {
        // Renamed twice with no shared channel, the second past the first's
        // window: the first transition is dropped as the second is added —
        // the list is bounded by the window, not by the rename count.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, mut wh) =
            registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        let _ = read_until(&mut r, "JOIN ").await;
        wh.write_all(b":cc-abc!u@h NICK :cc-b\r\n").await.unwrap();
        next_renamed(&mut session, &presence, &mut events).await;
        advance_puppet_clock(&mut session, 61);
        wh.write_all(b":cc-b!u@h NICK :cc-c\r\n").await.unwrap();
        next_renamed(&mut session, &presence, &mut events).await;
        let h = session
            .puppets
            .as_ref()
            .unwrap()
            .holder
            .values()
            .next()
            .expect("held");
        assert_eq!(
            h.unechoed
                .iter()
                .map(|u| (u.from.as_str(), u.to.as_str()))
                .collect::<Vec<_>>(),
            vec![("cc-b", "cc-c")],
            "the stale transition went as the next was added"
        );
        drop(wh);
        drop(r);
    }

    #[tokio::test]
    async fn an_echo_after_a_casemapping_change_is_still_the_echo() {
        // The puppet reports cc-abc→cc-a[x] under rfc1459; the server's
        // CASEMAPPING changes to ascii before the main connection's NICK for
        // it arrives. Spellings, not folds, are what was reported: the NICK
        // is the echo, consumed, and the pool is not moved a second time.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, mut wh) =
            registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        let _ = read_until(&mut r, "JOIN ").await;
        on_irc_line(&mut session, &presence, &mut writer, ":cc-abc!u@h JOIN #mu").unwrap();
        wh.write_all(b":cc-abc!u@h NICK :cc-a[x]\r\n")
            .await
            .unwrap();
        next_renamed(&mut session, &presence, &mut events).await;
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":srv 005 mu-gw CASEMAPPING=ascii :are supported by this server",
        )
        .unwrap();
        wire_nick(&mut session, &presence, &mut writer, "cc-abc", "cc-a[x]");
        let h = session
            .puppets
            .as_ref()
            .unwrap()
            .holder
            .values()
            .next()
            .expect("held");
        assert!(
            h.unechoed.is_empty(),
            "the echo was recognised and consumed"
        );
        assert_eq!(h.applied, 1, "applied once, by the report");
        assert_eq!(pool_nick(&session), Some("cc-a[x]"));
        drop(wh);
        drop(r);
    }

    #[tokio::test]
    async fn a_lease_comes_back_when_the_main_connection_sees_the_puppet_leave_not_before() {
        let Leased {
            mut session,
            presence,
            mut hand_rx,
            mut events,
            ..
        } = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (r, wh, account) = registered_slot_puppet(
            &mut session,
            &presence,
            &mut writer,
            &mut events,
            &mut hand_rx,
        )
        .await;
        // The puppet's socket closes. The main connection still lists cc-1 on
        // #mu as OUR account — its QUIT has not arrived here — so the lease
        // is held: returning it now would reconcile that member as a human.
        drop(wh);
        drop(r);
        let ev = drive_until(&mut session, &presence, &mut events, |ev| {
            matches!(ev, PuppetEvent::Ended { .. })
        })
        .await;
        on_puppet_event(&mut session, &presence, ev);
        assert_eq!(
            slots_of(&session).free(),
            0,
            "held until the departure is seen here"
        );
        assert!(
            session.membership.is_owned_by_account(&account),
            "still ours on the roster"
        );
        assert!(!session.membership.is_present(&account), "and not a human");
        // The server's QUIT reaches the main connection: now it comes back.
        on_irc_line(
            &mut session,
            &presence,
            &mut writer,
            ":cc-1!u@h QUIT :Connection closed",
        )
        .unwrap();
        assert_eq!(slots_of(&session).free(), 1, "released at the QUIT");
        assert!(!session.membership.is_owned_by_account(&account));
        assert!(session.membership.owned_nicks().is_empty());
    }

    #[tokio::test]
    async fn a_lease_is_kept_by_a_peer_that_speaks_not_by_one_that_is_merely_listed() {
        let Leased {
            mut session,
            presence,
            mut hand_rx,
            mut events,
            ..
        } = leased(1);
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (r, wh, account) = registered_slot_puppet(
            &mut session,
            &presence,
            &mut writer,
            &mut events,
            &mut hand_rx,
        )
        .await;
        // Past the idle window and still listed: ticks alone refresh nothing.
        advance_puppet_clock(&mut session, 31);
        puppets_tick(&mut session, &presence);
        // A line from cc:abc is life: the lease is fresh again…
        on_mesh_event(&mut session, &mut writer, dm("m1")).unwrap();
        // …so when cc:def qualifies, nothing is evictable: spillover.
        session.discovery = abc_and_def();
        puppets_tick(&mut session, &presence);
        assert!(hand_rx.try_recv().is_err(), "no dial for cc:def");
        let p = session.puppets.as_ref().unwrap();
        assert!(
            matches!(
                p.pool.state_of(&PeerId::parse("cc:def")),
                Some(PuppetState::BackingOff { .. })
            ),
            "{:?}",
            p.pool.state_of(&PeerId::parse("cc:def"))
        );
        assert_eq!(
            slots_of(&session).account_of(&PeerId::parse("cc:abc")),
            Some(account.as_str())
        );
        assert_eq!(slots_of(&session).evictions(), 0);
        drop(wh);
        drop(r);
    }

    #[tokio::test]
    async fn a_credential_that_went_bad_after_load_is_refused_in_the_session_and_retried_once_repaired(
    ) {
        let Scripted { mut session, .. } = scripted_session(4);
        negotiate_accounts(&mut session);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let presented = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cfg = provisioned(1);
        let dir = cfg.slot_certs_dir.clone().unwrap();
        let mut events = with_puppets(&mut session, cfg, slot_connector(hand_tx, presented));
        // The key vanishes between load and the first connect.
        std::fs::remove_file(dir.join("cc-1.key")).unwrap();
        discover_abc(&mut session);
        puppets_tick(&mut session, &presence);
        // No dial, and no event either: an `Ended` for an attempt the
        // executor never tracked would be thrown away as stale, leaving the
        // peer `Connecting` and the lease held for good (board finding, PR
        // #662 round 1). The refusal is resolved where it happens instead.
        assert!(hand_rx.try_recv().is_err(), "nothing was dialled");
        assert!(events.try_recv().is_err(), "no phantom event");
        let p = session.puppets.as_ref().unwrap();
        assert!(
            matches!(
                p.pool.state_of(&PeerId::parse("cc:abc")),
                Some(PuppetState::BackingOff { .. })
            ),
            "{:?}",
            p.pool.state_of(&PeerId::parse("cc:abc"))
        );
        assert_eq!(slots_of(&session).free(), 1, "the lease came straight back");
        let now = session.puppet_now_ms();
        assert_eq!(
            session
                .puppets
                .as_mut()
                .unwrap()
                .pool
                .attempts_in_window(now),
            0,
            "no socket was opened: the attempt is refunded"
        );
        // Repaired, the next due tick dials — with the credential.
        std::fs::copy(SLOT_PEMS[0].1, dir.join("cc-1.key")).unwrap();
        advance_puppet_clock(&mut session, 10);
        puppets_tick(&mut session, &presence);
        let (nick, _server) = tokio::time::timeout(Duration::from_secs(5), hand_rx.recv())
            .await
            .expect("dialled once repaired")
            .unwrap();
        assert_eq!(nick, "cc-1");
        assert_eq!(slots_of(&session).free(), 0);
    }

    #[test]
    fn disabled_puppets_build_no_pool_and_no_executor() {
        // `session()` builds the puppet half only under `enabled = true`; a
        // scripted session mirrors that: with no Puppetry every puppet path is
        // a no-op and no transport can exist.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        assert!(session.puppets.is_none());
        discover_abc(&mut session);
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
        discover_abc(&mut session);
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
        let ev = next(&mut events).await;
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
        discover_abc(&mut session);
        puppets_tick(&mut session, &presence);
        let (_nick, server) = hand_rx.recv().await.unwrap();
        let (rh, mut wh) = tokio::io::split(server);
        let mut r = BufReader::new(rh);
        read_until(&mut r, "NICK ").await;
        wh.write_all(b":srv CAP * LS :\r\n").await.unwrap();
        wh.write_all(b":srv 433 * cc-abc :Nickname is already in use\r\n")
            .await
            .unwrap();
        let ev = next(&mut events).await;
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
        let ev = next(&mut events).await;
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
        discover_abc(session);
        puppets_tick(session, presence);
        let (_nick, server) = hand_rx.recv().await.unwrap();
        let (rh, mut wh) = tokio::io::split(server);
        let mut r = BufReader::new(rh);
        read_until(&mut r, "NICK ").await;
        wh.write_all(b":srv CAP * LS :\r\n").await.unwrap();
        wh.write_all(b":srv 001 cc-abc :Welcome\r\n").await.unwrap();
        let ev = next(events).await;
        assert!(matches!(ev, PuppetEvent::Registered { .. }), "{ev:?}");
        on_puppet_event(session, presence, ev);
        assert!(session.membership.is_owned("cc-abc"));
        (r, wh)
    }

    #[tokio::test]
    async fn a_registered_puppets_socket_dropping_frees_its_nick_at_once() {
        // The server drops a registered puppet's connection (no Quit was
        // issued) and the puppet is on no roster this connection holds: its
        // Ended is a LIVE attempt's. The pool releases the nick and schedules
        // the retry, and the nick is nobody's at once — not owned, not held —
        // so a human may take it by a live JOIN.
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
        let ev = drive_until(&mut session, &presence, &mut events, |ev| {
            matches!(ev, PuppetEvent::Ended { .. })
        })
        .await;
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
        session.membership.self_joined("#mu");
        assert_eq!(
            session.membership.joined("#mu", "cc-abc", None),
            vec![HumanEffect::Register(PeerId::human("cc-abc"))]
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
        wire_nick(&mut session, &presence, &mut writer, "cc-abc", "cc-abc2");
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
        next_renamed(&mut session, &presence, &mut events).await;
        assert!(
            session.membership.is_present("cc-abc"),
            "the puppet's rename report moved the human"
        );
        assert!(session.membership.is_owned("cc-abc2"));
        assert_eq!(session.membership.owned_nicks(), vec!["cc-abc2"]);
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
        wire_nick(&mut session, &presence, &mut writer, "cc-abc", "cc-abc2");
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
            session.membership.is_present("cc-abc"),
            "the human was evicted"
        );
        // The puppet's own report, late: nothing changes.
        wh.write_all(b":cc-abc!u@h NICK :cc-abc2\r\n")
            .await
            .unwrap();
        next_renamed(&mut session, &presence, &mut events).await;
        assert_eq!(session.membership.owned_nicks(), vec!["cc-abc2"]);
        assert!(session.membership.is_present("cc-abc"));
        drop(wh);
        drop(r);
    }

    #[tokio::test]
    async fn a_stale_rename_report_does_not_roll_the_pool_back() {
        // The main connection carried the puppet A→B→C before the puppet's
        // own A→B report is read. The report is history: the pool stays at
        // C, and so does membership.
        let Scripted { mut session, .. } = scripted_session(4);
        let (presence, _rx) = mpsc::unbounded_channel();
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let mut events = with_puppets(&mut session, eager(), scripted_connector(hand_tx));
        let (mut writer, _lines) = transport::LineWriter::scripted(64);
        on_irc_line(&mut session, &presence, &mut writer, ":mu-gw!u@h JOIN #mu").unwrap();
        let (mut r, mut wh) =
            registered_puppet(&mut session, &presence, &mut events, &mut hand_rx).await;
        let _ = read_until(&mut r, "JOIN ").await;
        wire_nick(&mut session, &presence, &mut writer, "cc-abc", "cc-b");
        on_irc_line(&mut session, &presence, &mut writer, ":cc-b!u@h NICK :cc-c").unwrap();
        assert_eq!(pool_nick(&session), Some("cc-c"));
        assert_eq!(session.membership.owned_nicks(), vec!["cc-c"]);
        // Now the puppet's own connection reports A→B, then B→C.
        wh.write_all(b":cc-abc!u@h NICK :cc-b\r\n").await.unwrap();
        next_renamed(&mut session, &presence, &mut events).await;
        assert_eq!(
            pool_nick(&session),
            Some("cc-c"),
            "the stale report rolled the pool back"
        );
        assert_eq!(session.membership.owned_nicks(), vec!["cc-c"]);
        wh.write_all(b":cc-b!u@h NICK :cc-c\r\n").await.unwrap();
        next_renamed(&mut session, &presence, &mut events).await;
        assert_eq!(pool_nick(&session), Some("cc-c"));
        assert_eq!(session.membership.owned_nicks(), vec!["cc-c"]);
        drop(wh);
        drop(r);
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
            let ev = next(&mut events).await;
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
}
