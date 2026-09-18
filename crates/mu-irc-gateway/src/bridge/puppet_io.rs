//! The puppet EXECUTOR: one connection task per puppet, driven by the pool's
//! decisions and reporting back tagged events. The pool decides
//! (`crate::puppets`, pure); this module owns the tasks — one per attempt,
//! each a [`puppet_task`](super::puppet_task) that holds the socket and speaks
//! the [`PuppetEvent`]s the session's one state-owning loop consumes (design:
//! `specs/plans/mu-irc-gateway-v1-puppets.md`, "Connections and lifecycle";
//! increment 2b-i).
//!
//! Three rules hold the shape:
//!
//! - **Actions are complete instructions.** A [`PoolAction`] carries the peer
//!   and the nick; the executor needs no pool state to carry one out.
//! - **Every event is tagged with an attempt id.** A cancelled attempt's task
//!   may still be finishing when its replacement starts; a stale completion
//!   (an id the executor no longer tracks for that peer) is rejected here and
//!   counted, so the pool never sees a registration from a connection it
//!   already gave up on.
//! - **The loop never awaits a puppet.** Commands to a puppet task go through a
//!   bounded `try_send`; a task that cannot take one is stalled, and a stalled
//!   puppet costs nothing but its own voice. A QUIT reaches a stalled task
//!   anyway, out of band, so a registered puppet always ends by reporting its
//!   own departure. Cancel is an abort: dropping the connection through its
//!   `ConnectionGuard` closes both halves.
//!
//! What a task does with its connection, and what its events mean, is the
//! task module's contract; this one is about attempts: which is live, which
//! is quitting, which is stale, and that no task outlives its executor.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mu_peer::PeerId;
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, info};

use super::puppet_task::{puppet_task, Spawn};
use crate::config::IrcConfig;
use crate::puppets::PoolAction;

pub use super::puppet_task::{dial_connector, Connector, PuppetCommand, PuppetEvent};

/// Body-free counters the executor keeps; reported when the session ends.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExecutorStats {
    /// Events that named an attempt the executor no longer tracks.
    pub stale_events: u64,
    /// Commands a puppet task could not take (queue full or task gone).
    pub commands_dropped: u64,
    /// Puppet lines the session dropped at the fan-in (the executor carries
    /// the field so one report covers both sides).
    pub lines_dropped: u64,
    /// Protocol lines lost at a puppet task's full queues: inbound lines it
    /// could not hand up (the event queue was full), mirrored lines the
    /// socket's bounded queue refused. Counted on the task side, so a
    /// backpressure loss is never invisible.
    pub lines_unqueued: u64,
}

/// A puppet task's handle, abort-on-drop. Every owner of a puppet task —
/// the live map, the quitting list, a `join_quitting` in progress — holds one
/// of these, so there is no path (an executor dropped, a session future
/// cancelled mid-await) on which a task is detached with its socket open.
struct Task(JoinHandle<()>);

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Future for Task {
    type Output = Result<(), tokio::task::JoinError>;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}

struct Handle {
    attempt: u64,
    nick_offered: String,
    cmd: mpsc::Sender<PuppetCommand>,
    /// The out-of-band QUIT: reaches the task whatever it is parked on — a
    /// full command queue, a lifecycle report the session is not draining —
    /// so a registered puppet always leaves by QUIT and always reports its
    /// `Ended`. A `Notify` keeps a permit, so a stop signalled before the
    /// task next waits is not lost.
    stop: Arc<Notify>,
    task: Task,
}

/// Owns every live puppet connection task for one session.
pub struct Executor {
    /// The `[irc]` config puppets inherit (server, TLS, trust); each puppet's
    /// own `nick` replaces the gateway's and `sasl` is always `None`.
    base: IrcConfig,
    connector: Connector,
    events: mpsc::Sender<PuppetEvent>,
    handles: HashMap<PeerId, Handle>,
    /// Tasks told to QUIT and no longer addressable; a teardown joins them
    /// under one deadline, an ordinary departure lets them finish alone.
    /// Their attempt stays known so the `Ended` they report after the QUIT
    /// is current, not stale — known until the session has CONSUMED that
    /// `Ended` and says so through [`forget`](Self::forget), never merely
    /// until the task finished: a task is finished the moment its `Ended` is
    /// queued, which can be before anyone has read it. Abort-on-drop like
    /// every other handle here.
    quitting: Vec<(PeerId, u64, Task)>,
    next_attempt: u64,
    registration_timeout: Duration,
    stats: ExecutorStats,
    /// Shared with every task: lines it could not hand up (queue full).
    lines_unqueued: Arc<AtomicU64>,
}

impl Executor {
    /// An executor for puppets inheriting `base`, reporting on `events`.
    pub fn new(
        base: IrcConfig,
        connector: Connector,
        events: mpsc::Sender<PuppetEvent>,
        registration_timeout: Duration,
    ) -> Self {
        Executor {
            base,
            connector,
            events,
            handles: HashMap::new(),
            quitting: Vec::new(),
            next_attempt: 0,
            registration_timeout,
            stats: ExecutorStats::default(),
            lines_unqueued: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The counters so far.
    pub fn stats(&mut self) -> &ExecutorStats {
        self.stats.lines_unqueued = self.lines_unqueued.load(Ordering::Relaxed);
        &self.stats
    }

    /// Count one puppet line the session dropped at the fan-in.
    pub fn count_dropped_line(&mut self) {
        self.stats.lines_dropped += 1;
    }

    /// Whether `peer` has a live task (connecting or registered).
    pub fn has(&self, peer: &PeerId) -> bool {
        self.handles.contains_key(peer)
    }

    /// Number of live puppet tasks.
    pub fn len(&self) -> usize {
        self.handles.len()
    }

    /// Whether no puppet task is live.
    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    /// Carry out one pool decision. Never blocks.
    pub fn execute(&mut self, action: PoolAction) {
        match action {
            PoolAction::Connect { peer, nick } => self.connect(peer, nick),
            PoolAction::Cancel { peer } => self.cancel(&peer),
            PoolAction::Quit { peer, nick } => self.quit(&peer, &nick),
            PoolAction::ChannelOnly { peer, reason } => {
                // Reported once by the pool; the operator-facing notice and
                // the `mu peers` row are 2b-ii. Nothing to execute.
                info!(peer = %peer, reason = ?reason, "puppet: channel-only for this session");
            }
        }
    }

    /// Send a command to `peer`'s task. `false` if it could not be queued —
    /// the task is stalled or gone — which the caller treats as that puppet
    /// having no voice right now.
    pub fn command(&mut self, peer: &PeerId, cmd: PuppetCommand) -> bool {
        let Some(h) = self.handles.get(peer) else {
            self.stats.commands_dropped += 1;
            return false;
        };
        match h.cmd.try_send(cmd) {
            Ok(()) => true,
            Err(_) => {
                self.stats.commands_dropped += 1;
                false
            }
        }
    }

    /// Whether `event` belongs to the attempt currently tracked for its peer.
    /// A stale event is counted and must be ignored by the caller.
    pub fn is_current(&mut self, event: &PuppetEvent) -> bool {
        let (peer, attempt) = match event {
            PuppetEvent::Registered { peer, attempt, .. }
            | PuppetEvent::NickRejected { peer, attempt, .. }
            | PuppetEvent::Ended { peer, attempt, .. }
            | PuppetEvent::Renamed { peer, attempt, .. }
            | PuppetEvent::Line { peer, attempt, .. } => (peer, *attempt),
        };
        let live = self.handles.get(peer).is_some_and(|h| h.attempt == attempt);
        // A task told to QUIT still reports the facts about its NICK: a
        // `Renamed` it queued before the Quit reached it, and the one last
        // `Ended`. Both are current — the departure resolves under the nick
        // the server last knew, so the rename that got it there must not be
        // thrown away. Its lines are stale: nothing after a QUIT is routed.
        let ending = matches!(
            event,
            PuppetEvent::Ended { .. } | PuppetEvent::Renamed { .. }
        ) && self
            .quitting
            .iter()
            .any(|(p, a, _)| p == peer && *a == attempt);
        let current = live || ending;
        if !current {
            self.stats.stale_events += 1;
        }
        current
    }

    /// Whether `attempt` is `peer`'s LIVE task — connecting or registered and
    /// still addressable — as opposed to one told to QUIT and finishing. An
    /// `Ended` that is current but not live is a departure the pool already
    /// decided; only a live task's end is a disconnect the pool must learn of.
    pub fn is_live(&self, peer: &PeerId, attempt: u64) -> bool {
        self.handles.get(peer).is_some_and(|h| h.attempt == attempt)
    }

    /// The task for `peer` ended (an `Ended` event that was current): forget
    /// its handle, live or quitting (aborting is a no-op on a finished task).
    pub fn forget(&mut self, peer: &PeerId, attempt: u64) {
        if self.handles.get(peer).is_some_and(|h| h.attempt == attempt) {
            self.handles.remove(peer);
        }
        self.quitting
            .retain(|(p, a, _)| !(p == peer && *a == attempt));
    }

    /// Abort every task now — live and quitting. The bounded, QUIT-first
    /// teardown is the session's ([`PoolAction::Quit`] per registered puppet,
    /// `join_quitting`, then this).
    pub fn abort_all(&mut self) {
        self.handles.clear();
        self.quitting.clear();
    }

    fn connect(&mut self, peer: PeerId, nick: String) {
        // A replacement attempt for a peer supersedes whatever was in flight.
        self.cancel(&peer);
        self.next_attempt += 1;
        let attempt = self.next_attempt;
        let mut irc = self.base.clone();
        irc.nick = nick.clone();
        irc.sasl = None;
        let (cmd_tx, cmd_rx) = mpsc::channel(self.base.puppets.command_queue);
        let stop = Arc::new(Notify::new());
        let task = Task(tokio::spawn(puppet_task(Spawn {
            peer: peer.clone(),
            attempt,
            irc,
            connector: self.connector.clone(),
            events: self.events.clone(),
            cmd_rx,
            stop: stop.clone(),
            registration_timeout: self.registration_timeout,
            lines_unqueued: self.lines_unqueued.clone(),
        })));
        self.handles.insert(
            peer,
            Handle {
                attempt,
                nick_offered: nick,
                cmd: cmd_tx,
                stop,
                task,
            },
        );
    }

    fn cancel(&mut self, peer: &PeerId) {
        if let Some(h) = self.handles.remove(peer) {
            debug!(peer = %peer, attempt = h.attempt, nick = %h.nick_offered, "puppet: cancelled");
            // Dropping `h` aborts the task.
        }
    }

    fn quit(&mut self, peer: &PeerId, nick: &str) {
        let Some(h) = self.handles.remove(peer) else {
            // Nothing live to ask: the pool's Quit outran the task's own end,
            // whose `Ended` resolves the departure.
            self.stats.commands_dropped += 1;
            return;
        };
        // The out-of-band stop is THE quit signal: it reaches the task
        // whatever it is doing — parked on a full command queue, or on a
        // lifecycle report the session is not draining — and the task takes
        // it before anything queued. The Quit command is queued as well for
        // the record (a task that reads it first leaves the same way); when
        // the queue is full that is counted, and nothing is lost.
        let cmd = PuppetCommand::Quit {
            reason: format!("{nick}: agent left the mesh"),
            grace: self.quit_grace(),
        };
        if h.cmd.try_send(cmd).is_err() {
            self.stats.commands_dropped += 1;
        }
        h.stop.notify_one();
        // The task ends itself after QUIT (it bounds its own grace and
        // force-closes); nothing else may be sent to it, and its later
        // events other than that `Ended` are stale by construction. The
        // handle is kept so a teardown can join every quitting task under one
        // deadline, and stays until `forget` — see the field.
        drop(h.cmd);
        self.quitting.push((peer.clone(), h.attempt, h.task));
    }

    /// The configured QUIT grace.
    fn quit_grace(&self) -> Duration {
        Duration::from_secs(self.base.puppets.quit_grace_secs)
    }

    /// Wait for every quitting task, together, until `deadline`; whatever is
    /// still running then is aborted. The bounded half of the pool teardown.
    pub async fn join_quitting(&mut self, deadline: Instant) {
        // Each task is moved out one at a time and awaited under the deadline;
        // a `Task` aborts on drop, so a task past the deadline is aborted when
        // it goes out of scope here — and so is one caught mid-await if this
        // future is itself cancelled. Its grace timer started when IT read the
        // Quit command, not when the command was queued, so a stalled task can
        // be later than the caller's absolute deadline; the abort is what makes
        // the bound hold.
        while let Some((peer, attempt, mut task)) = self.quitting.pop() {
            if tokio::time::timeout_at(deadline, &mut task).await.is_err() {
                debug!(peer = %peer, attempt, "puppet: quitting task aborted at the teardown deadline");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::puppet_task::test_support::*;
    use super::*;

    use tokio::io::{AsyncWriteExt, BufReader};

    use crate::transport::Connection;

    /// Connect `peer` as `nick` and walk it through registration; the
    /// `Registered` event has been consumed. Returns the attempt and the
    /// server side.
    async fn registered(
        ex: &mut Executor,
        hand_rx: &mut mpsc::UnboundedReceiver<(String, tokio::io::DuplexStream)>,
        ev_rx: &mut mpsc::Receiver<PuppetEvent>,
        peer: &PeerId,
        nick: &str,
    ) -> (u64, (ServerRead, ServerWrite)) {
        ex.execute(PoolAction::Connect {
            peer: peer.clone(),
            nick: nick.into(),
        });
        let (_nick, server) = hand_rx.recv().await.expect("a connection was opened");
        let (rh, mut wh) = tokio::io::split(server);
        let mut r = BufReader::new(rh);
        read_until(&mut r, "NICK ").await;
        wh.write_all(b":srv CAP * LS :\r\n").await.unwrap();
        wh.write_all(format!(":srv 001 {nick} :Welcome\r\n").as_bytes())
            .await
            .unwrap();
        let ev = next(ev_rx).await;
        assert!(ex.is_current(&ev));
        let attempt = match &ev {
            PuppetEvent::Registered {
                attempt, nick: got, ..
            } => {
                assert_eq!(got, nick);
                *attempt
            }
            other => panic!("{other:?}"),
        };
        (attempt, (r, wh))
    }

    #[tokio::test]
    async fn a_connect_runs_a_task_whose_events_are_current_until_forgotten() {
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::channel(16);
        let mut ex = Executor::new(
            base(),
            scripted_connector(hand_tx),
            ev_tx,
            Duration::from_secs(5),
        );
        let (attempt, (mut r, wh)) =
            registered(&mut ex, &mut hand_rx, &mut ev_rx, &peer(), "cc-abc").await;
        assert!(ex.has(&peer()) && ex.is_live(&peer(), attempt));
        assert_eq!(ex.len(), 1);
        // A command reaches the task.
        assert!(ex.command(&peer(), PuppetCommand::Join(vec!["#mu".into()])));
        assert_eq!(read_until(&mut r, "JOIN ").await.trim(), "JOIN #mu");
        // QUIT: the handle leaves the live map at once; the task is quitting,
        // not live, and its Ended — after the server closes — is current
        // until the session says it has consumed it.
        ex.execute(PoolAction::Quit {
            peer: peer(),
            nick: "cc-abc".into(),
        });
        assert!(!ex.has(&peer()), "quitting, not live");
        assert!(!ex.is_live(&peer(), attempt));
        assert!(!ex.command(&peer(), PuppetCommand::Send("late".into())));
        read_until(&mut r, "QUIT").await;
        drop(wh);
        drop(r);
        let ev = loop {
            let ev = next(&mut ev_rx).await;
            if matches!(ev, PuppetEvent::Ended { .. }) {
                break ev;
            }
            // A line from a task already told to QUIT is stale by design.
            assert!(!ex.is_current(&ev), "{ev:?}");
        };
        assert!(
            ex.is_current(&ev),
            "a quitting task's Ended is current: {ev:?}"
        );
        ex.forget(&peer(), attempt);
        assert!(!ex.is_current(&ev), "forgotten after the report");
        assert!(ex.quitting.is_empty());
        assert!(ex.is_empty());
    }

    #[tokio::test]
    async fn a_cancelled_attempts_events_are_stale_and_counted() {
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::channel(16);
        let mut ex = Executor::new(
            base(),
            scripted_connector(hand_tx),
            ev_tx,
            Duration::from_secs(5),
        );
        ex.execute(PoolAction::Connect {
            peer: peer(),
            nick: "cc-abc".into(),
        });
        let (_nick, server) = hand_rx.recv().await.unwrap();
        let (rh, mut wh) = tokio::io::split(server);
        let mut r = BufReader::new(rh);
        read_until(&mut r, "NICK ").await;
        wh.write_all(b":srv CAP * LS :\r\n").await.unwrap();
        wh.write_all(b":srv 433 * cc-abc :Nickname is already in use\r\n")
            .await
            .unwrap();
        let ev = next(&mut ev_rx).await;
        assert!(ex.is_current(&ev));
        let old = match &ev {
            PuppetEvent::NickRejected { attempt, .. } => *attempt,
            other => panic!("{other:?}"),
        };
        // The pool answers with Cancel + Connect(tailed): the executor aborts
        // the old task and opens a new connection under the new nick; a late
        // event from the old attempt is stale.
        ex.execute(PoolAction::Cancel { peer: peer() });
        assert!(!ex.has(&peer()));
        ex.execute(PoolAction::Connect {
            peer: peer(),
            nick: "cc-abc1a2b3c4d".into(),
        });
        let (nick, _server2) = hand_rx.recv().await.unwrap();
        assert_eq!(nick, "cc-abc1a2b3c4d");
        let stale = PuppetEvent::Ended {
            peer: peer(),
            attempt: old,
            nick: None,
            why: "late".into(),
            confirmed: false,
        };
        assert!(
            !ex.is_current(&stale),
            "an event from a cancelled attempt is stale"
        );
        assert_eq!(ex.stats().stale_events, 1);
    }

    #[tokio::test]
    async fn dropping_the_executor_aborts_every_task_including_one_parked_in_connect() {
        // A connector that never resolves, holding a token the test can watch:
        // the token is released only when the connect future is dropped, i.e.
        // when the task is aborted.
        let token = Arc::new(());
        let watched = token.clone();
        let connector: Connector = Arc::new(move |_irc: IrcConfig| {
            let held = watched.clone();
            Box::pin(async move {
                let _held = held;
                std::future::pending::<Result<Connection, String>>().await
            })
        });
        let (ev_tx, _ev_rx) = mpsc::channel(16);
        let mut ex = Executor::new(base(), connector, ev_tx, Duration::from_secs(5));
        ex.execute(PoolAction::Connect {
            peer: peer(),
            nick: "cc-abc".into(),
        });
        tokio::task::yield_now().await;
        assert!(
            Arc::strong_count(&token) >= 2,
            "the task holds the token while parked"
        );
        drop(ex);
        for _ in 0..50 {
            if Arc::strong_count(&token) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            Arc::strong_count(&token),
            1,
            "the parked task was aborted with the executor, not detached"
        );
    }

    #[tokio::test]
    async fn a_refused_connection_reports_ended_and_a_dropped_command_is_counted() {
        let (ev_tx, mut ev_rx) = mpsc::channel(16);
        let mut ex = Executor::new(base(), refusing_connector(), ev_tx, Duration::from_secs(5));
        ex.execute(PoolAction::Connect {
            peer: peer(),
            nick: "cc-abc".into(),
        });
        let ev = next(&mut ev_rx).await;
        assert!(ex.is_current(&ev));
        let attempt = match &ev {
            PuppetEvent::Ended { attempt, why, .. } if why.contains("refused") => *attempt,
            other => panic!("{other:?}"),
        };
        ex.forget(&peer(), attempt);
        assert!(!ex.has(&peer()));
        // A command to a peer with no task is dropped and counted, never
        // awaited; so is a Quit for a peer with no task.
        assert!(!ex.command(&peer(), PuppetCommand::Send("x".into())));
        assert_eq!(ex.stats().commands_dropped, 1);
        ex.execute(PoolAction::Quit {
            peer: peer(),
            nick: "cc-abc".into(),
        });
        assert_eq!(ex.stats().commands_dropped, 2);
        assert!(ex.quitting.is_empty());
    }

    #[tokio::test]
    async fn a_quit_that_cannot_be_queued_is_counted_and_still_delivered_by_the_stop() {
        // A one-deep command queue the session filled: the Quit command does
        // not fit, which is counted, and the stop carries it anyway — the
        // task leaves, and its Ended is current under the quitting attempt.
        let mut cfg = base();
        cfg.puppets.command_queue = 1;
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::channel(16);
        let mut ex = Executor::new(
            cfg,
            scripted_connector(hand_tx),
            ev_tx,
            Duration::from_secs(5),
        );
        let (attempt, (mut r, wh)) =
            registered(&mut ex, &mut hand_rx, &mut ev_rx, &peer(), "cc-abc").await;
        assert!(ex.command(&peer(), PuppetCommand::Send("PRIVMSG #mu :x".into())));
        assert!(!ex.command(&peer(), PuppetCommand::Send("PRIVMSG #mu :y".into())));
        let dropped_before = ex.stats().commands_dropped;
        ex.execute(PoolAction::Quit {
            peer: peer(),
            nick: "cc-abc".into(),
        });
        assert_eq!(
            ex.stats().commands_dropped,
            dropped_before + 1,
            "the unqueued Quit is counted"
        );
        assert!(!ex.has(&peer()));
        read_until(&mut r, "QUIT").await;
        drop(wh);
        drop(r);
        let ev = loop {
            let ev = next(&mut ev_rx).await;
            if matches!(ev, PuppetEvent::Ended { .. }) {
                break ev;
            }
        };
        assert!(ex.is_current(&ev) && !ex.is_live(&peer(), attempt));
        ex.forget(&peer(), attempt);
    }

    #[tokio::test]
    async fn a_quitting_attempt_stays_known_until_its_ended_is_consumed() {
        // Two puppets quit back to back. A's task is finished — its Ended is
        // queued — before B is told to quit; B's Quit must not make A's Ended
        // stale. Only `forget`, after the session has read it, does.
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::channel(16);
        let mut ex = Executor::new(
            base(),
            scripted_connector(hand_tx),
            ev_tx,
            Duration::from_secs(5),
        );
        let a = PeerId::parse("cc:aaa");
        let b = PeerId::parse("cc:bbb");
        let (attempt_a, server_a) =
            registered(&mut ex, &mut hand_rx, &mut ev_rx, &a, "cc-aaa").await;
        let (_attempt_b, _server_b) =
            registered(&mut ex, &mut hand_rx, &mut ev_rx, &b, "cc-bbb").await;
        ex.execute(PoolAction::Quit {
            peer: a.clone(),
            nick: "cc-aaa".into(),
        });
        // Let A's task write its QUIT and, the server closing, queue its
        // Ended: A's task is finished with its report unread.
        let (mut r_a, wh_a) = server_a;
        read_until(&mut r_a, "QUIT").await;
        drop(wh_a);
        drop(r_a);
        for _ in 0..200 {
            if ex
                .quitting
                .iter()
                .any(|(p, _, t)| *p == a && t.0.is_finished())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            ex.quitting
                .iter()
                .any(|(p, _, t)| *p == a && t.0.is_finished()),
            "A's task should be finished with its Ended queued"
        );
        // Now B quits. Before, this pruned A from `quitting` because A's task
        // had finished — and A's Ended, still unread, became stale.
        ex.execute(PoolAction::Quit {
            peer: b.clone(),
            nick: "cc-bbb".into(),
        });
        let ended_a = loop {
            let ev = next(&mut ev_rx).await;
            if matches!(&ev, PuppetEvent::Ended { peer, .. } if *peer == a) {
                break ev;
            }
        };
        assert!(
            ex.is_current(&ended_a),
            "A's departure report was made stale by B's Quit: {ended_a:?}"
        );
        assert_eq!(ex.stats().stale_events, 0);
        ex.forget(&a, attempt_a);
        assert!(!ex.is_current(&ended_a), "forgotten once consumed");
    }

    #[tokio::test]
    async fn a_quitting_attempts_rename_is_current_and_its_lines_are_stale() {
        // The server renames the puppet; its Renamed is queued. Before the
        // session reads it, the pool's Quit moves the attempt to quitting.
        // The rename is a fact about the nick the Ended will name, so it
        // must not be dropped as stale on the way; its lines are.
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::channel(16);
        let mut ex = Executor::new(
            base(),
            scripted_connector(hand_tx),
            ev_tx,
            Duration::from_secs(5),
        );
        let (attempt, (mut r, mut wh)) =
            registered(&mut ex, &mut hand_rx, &mut ev_rx, &peer(), "cc-abc").await;
        wh.write_all(b":cc-abc!u@h NICK :Guest42\r\n")
            .await
            .unwrap();
        wh.write_all(b":alice!u@h PRIVMSG #mu :hi\r\n")
            .await
            .unwrap();
        // Let the task queue its Renamed (and the line) before the Quit.
        for _ in 0..100 {
            if ev_rx.len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        ex.execute(PoolAction::Quit {
            peer: peer(),
            nick: "Guest42".into(),
        });
        read_until(&mut r, "QUIT").await;
        drop(wh);
        drop(r);
        let ev = next(&mut ev_rx).await;
        assert!(
            matches!(&ev, PuppetEvent::Renamed { to, .. } if to == "Guest42"),
            "{ev:?}"
        );
        assert!(ex.is_current(&ev), "a quitting attempt's rename is current");
        let ev = next(&mut ev_rx).await;
        assert!(matches!(ev, PuppetEvent::Line { .. }), "{ev:?}");
        assert!(!ex.is_current(&ev), "a quitting attempt's lines are stale");
        let ev = loop {
            let ev = next(&mut ev_rx).await;
            if matches!(ev, PuppetEvent::Ended { .. }) {
                break ev;
            }
        };
        assert!(ex.is_current(&ev));
        ex.forget(&peer(), attempt);
    }

    #[tokio::test]
    async fn teardown_joins_every_quitting_task_under_one_deadline_then_aborts_the_rest() {
        // Two registered puppets; one server closes promptly after the QUIT,
        // the other never does. `join_quitting` returns at the deadline with
        // both accounted for, and `abort_all` leaves nothing behind.
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::channel(16);
        let mut cfg = base();
        cfg.puppets.quit_grace_secs = 1;
        let mut ex = Executor::new(
            cfg,
            scripted_connector(hand_tx),
            ev_tx,
            Duration::from_secs(5),
        );
        let a = PeerId::parse("cc:aaa");
        let b = PeerId::parse("cc:bbb");
        let (_aa, (mut r_a, wh_a)) =
            registered(&mut ex, &mut hand_rx, &mut ev_rx, &a, "cc-aaa").await;
        let (_ab, (mut r_b, _wh_b)) =
            registered(&mut ex, &mut hand_rx, &mut ev_rx, &b, "cc-bbb").await;
        for (p, n) in [(&a, "cc-aaa"), (&b, "cc-bbb")] {
            ex.execute(PoolAction::Quit {
                peer: p.clone(),
                nick: n.into(),
            });
        }
        read_until(&mut r_a, "QUIT").await;
        read_until(&mut r_b, "QUIT").await;
        drop(wh_a);
        drop(r_a);
        let started = Instant::now();
        ex.join_quitting(Instant::now() + Duration::from_millis(1500))
            .await;
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert!(
            ex.quitting.is_empty(),
            "every quitting task was joined or aborted"
        );
        ex.abort_all();
        assert!(ex.is_empty());
        drop(r_b);
    }
}
