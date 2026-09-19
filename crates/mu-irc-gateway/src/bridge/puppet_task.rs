//! One puppet CONNECTION, birth to death: the task the executor spawns per
//! attempt (`bridge::puppet_io`), and what it speaks — the commands it takes
//! and the events it reports. The pool decides (`crate::puppets`, pure); the
//! executor carries decisions out; this task is the one that holds a socket,
//! runs the unchanged registration adapter on it, and turns what happens into
//! [`PuppetEvent`]s the session's one state-owning loop consumes (design:
//! `specs/plans/mu-irc-gateway-v1-puppets.md`, "Connections and lifecycle";
//! increment 2b-i).
//!
//! What the task promises, and its tests hold it to:
//!
//! - **Lifecycle reports arrive; lines may not.** `Registered`, `Renamed`,
//!   `NickRejected` and `Ended` are awaited — the pool's state depends on each
//!   one — and a report a stop pre-empts is delivered after the QUIT, ahead of
//!   the `Ended`. Protocol lines are `try_send` and counted when dropped: a
//!   burst never displaces a lifecycle event.
//! - **It always leaves by QUIT, and always reports leaving.** The stop
//!   reaches it whatever it is parked on (a full command queue, a report the
//!   session is not draining); the QUIT is written on a line boundary and the
//!   task reads on until the SERVER closes — that, and only that, makes the
//!   `Ended` a confirmed departure the session may order on — bounded by the
//!   configured grace, past which the socket is cut and the `Ended` says so.
//! - **A JOIN is membership, not a mirrored line.** One the outbound queue
//!   refuses is held and re-offered until it goes.
//! - **The socket never outlives its purpose.** Every exit lets go of the
//!   connection guard before its final reports, so a queue nobody drains
//!   cannot keep a dead connection open.
//!
//! What an inbound line on a puppet becomes: `PING` is answered by the task
//! itself (the server drops a silent client); the puppet's own `NICK` is a
//! lifecycle event; everything else after registration is handed up as
//! [`PuppetEvent::Line`] for the session to classify
//! (`crate::puppets::classify`). In 2b-i the session drops and counts every
//! such line; routing through the private-class tag is increment 2b-ii.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mu_peer::PeerId;
use tokio::sync::{mpsc, Notify};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::adapter::{AdapterError, IrcMessage, Registration, SystemClock, Transport};
use crate::config::IrcConfig;
use crate::transport::{self, Connection, FromServer};

/// Commands a puppet task takes from the session loop, by bounded `try_send`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PuppetCommand {
    /// Join these channels (after registration).
    Join(Vec<String>),
    /// Write one raw line.
    Send(String),
    /// Send `QUIT :<reason>` gracefully and end; the task closes the
    /// connection at `grace` regardless.
    Quit { reason: String, grace: Duration },
}

/// What a puppet task reports. Every variant names the peer and the attempt
/// it belongs to; the executor drops any whose attempt is no longer current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PuppetEvent {
    /// The server welcomed the connection as `nick` (the spelling from `001`).
    Registered {
        peer: PeerId,
        attempt: u64,
        nick: String,
    },
    /// The server rejected the offered nick at registration.
    NickRejected {
        peer: PeerId,
        attempt: u64,
        numeric: String,
    },
    /// The connection ended: before registration (connect error, handshake
    /// failure or timeout), after it (closed by the server or the socket), or
    /// because it was told to QUIT and did. `nick` is the registered nick if
    /// it got that far — the session resolves the nick's departure from THIS
    /// event, which arrives whether or not the gateway ever shared a channel
    /// with the puppet. Body-free reason; `confirmed` says whose doing the
    /// end was.
    Ended {
        peer: PeerId,
        attempt: u64,
        nick: Option<String>,
        why: String,
        /// Whether the SERVER ended the connection — it closed after the
        /// QUIT, or dropped the puppet itself — as opposed to this side (the
        /// grace cut a QUIT the server never answered, a write or read
        /// failed locally). Only a confirmed end says the server has
        /// processed everything this puppet sent; the session orders its
        /// departure barrier on that, and on nothing less.
        confirmed: bool,
    },
    /// The server renamed this registered puppet (a `NICK` on its own
    /// connection whose source is the nick it held). A lifecycle event, not a
    /// line: the pool's table and membership's ownership follow it, so it is
    /// awaited like `Registered`, never dropped under load. The task's own
    /// record of its nick follows too, so its `Ended` names the nick the
    /// server last knew it by.
    Renamed {
        peer: PeerId,
        attempt: u64,
        from: String,
        to: String,
    },
    /// A post-registration protocol line other than `PING` and the puppet's
    /// own `NICK`, for the session to classify.
    Line {
        peer: PeerId,
        attempt: u64,
        line: String,
    },
}

/// A boxed connect: the production one dials `IrcConfig::server` with the
/// configured trust; tests inject one that yields an in-process pair.
pub type Connector = Arc<
    dyn Fn(IrcConfig) -> Pin<Box<dyn Future<Output = Result<Connection, String>> + Send>>
        + Send
        + Sync,
>;

/// The production connector.
pub fn dial_connector(connect_timeout: Duration) -> Connector {
    Arc::new(move |irc: IrcConfig| {
        Box::pin(async move {
            transport::connect_with_trust(&irc.server, irc.tls, &irc.tls_trust, connect_timeout)
                .await
                .map_err(|e| format!("{e:#}"))
        })
    })
}

/// Everything one puppet task is born with: the executor's side of the
/// contract. The executor (`bridge::puppet_io`) builds one per attempt.
pub struct Spawn {
    pub peer: PeerId,
    /// The attempt id every event of this task carries.
    pub attempt: u64,
    /// The gateway's `[irc]` config with THIS puppet's nick and no SASL.
    pub irc: IrcConfig,
    pub connector: Connector,
    pub events: mpsc::Sender<PuppetEvent>,
    pub cmd_rx: mpsc::Receiver<PuppetCommand>,
    /// The out-of-band QUIT; see the module doc.
    pub stop: Arc<Notify>,
    pub registration_timeout: Duration,
    /// Shared with the executor: lines this task could not hand up.
    pub lines_unqueued: Arc<AtomicU64>,
}

/// One puppet connection, birth to death.
pub async fn puppet_task(spawn: Spawn) {
    let Spawn {
        peer,
        attempt,
        irc,
        connector,
        events,
        mut cmd_rx,
        stop,
        registration_timeout,
        lines_unqueued,
    } = spawn;
    let quit_grace = Duration::from_secs(irc.puppets.quit_grace_secs);
    // Two delivery classes. LIFECYCLE events (Registered / NickRejected /
    // Ended) are awaited: the pool's state depends on each one arriving, and
    // this is the puppet's own task, so waiting for a queue slot blocks nobody
    // but this puppet. A session that has stopped draining has ended or is
    // ending, and the send then fails when the receiver is dropped. Protocol
    // LINES are lossy on purpose (`try_send`): a burst must never displace a
    // lifecycle event, and a dropped line is counted, not mourned.
    let report = |ev: PuppetEvent| {
        let events = events.clone();
        async move {
            let _ = events.send(ev).await;
        }
    };
    // A lifecycle report from a REGISTERED puppet also watches the stop: a
    // queue that is full (the session is wedged, or tearing down and no
    // longer draining) would otherwise park the task on the report, and a
    // Quit arriving meanwhile could not start the QUIT until the queue moved
    // — the teardown's deadline would then cut the socket with no QUIT
    // written. `Some(ev)` means the stop won and `ev` was NOT delivered: the
    // caller leaves by QUIT and delivers it afterwards, before the Ended, so
    // the record stays whole (a Renamed the session never saw would leave
    // the Ended naming a nick it never heard of).
    let report_or_stop = |ev: PuppetEvent| {
        let events = events.clone();
        let stop = stop.clone();
        async move {
            let send = events.send(ev.clone());
            tokio::pin!(send);
            tokio::select! {
                biased;
                _ = stop.notified() => Some(ev),
                _ = &mut send => None,
            }
        }
    };
    let report_line = |ev: PuppetEvent| {
        let events = events.clone();
        let unqueued = lines_unqueued.clone();
        async move {
            if events.try_send(ev).is_err() {
                unqueued.fetch_add(1, Ordering::Relaxed);
            }
        }
    };
    // Registration machine first, socket second: a config the adapter refuses
    // costs no connection (the puppet config is the gateway's with a nick the
    // pool already validated, so this is defensive).
    let (mut reg, first) = match Registration::start(&irc, SystemClock) {
        Ok(v) => v,
        Err(e) => {
            report(PuppetEvent::Ended {
                peer,
                attempt,
                nick: None,
                confirmed: false,
                why: format!("adapter refused the puppet config: {e}"),
            })
            .await;
            return;
        }
    };
    let Connection {
        mut writer,
        mut inbound,
        guard,
    } = match (connector)(irc.clone()).await {
        Ok(c) => c,
        Err(why) => {
            report(PuppetEvent::Ended {
                peer,
                attempt,
                nick: None,
                confirmed: false,
                why,
            })
            .await;
            return;
        }
    };
    for line in &first {
        if writer.send_line(line).is_err() {
            drop(guard);
            report(PuppetEvent::Ended {
                peer,
                attempt,
                nick: None,
                confirmed: false,
                why: "write failed during registration".into(),
            })
            .await;
            return;
        }
    }
    let deadline = Instant::now() + registration_timeout;
    let mut nick = irc.nick.clone();
    // ── Registration ────────────────────────────────────────────────────────
    // Biased, stop first, for the same reason as the registered loop below:
    // an executor that could not queue the Quit signals the stop and drops
    // the command sender, and the queue's `None` must not win that race.
    loop {
        let event = tokio::select! {
            biased;
            _ = stop.notified() => {
                drop(guard);
                report(PuppetEvent::Ended {
                    peer, attempt, nick: None, confirmed: false, why: "quit before registration".into(),
                }).await;
                return;
            }
            _ = tokio::time::sleep_until(deadline) => {
                drop(guard);
                report(PuppetEvent::Ended {
                    peer, attempt, nick: None,
                confirmed: false,
                    why: format!("registration did not complete within {registration_timeout:?}"),
                }).await;
                return;
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    // A QUIT before registration: just go — but say so, so
                    // the executor can forget this attempt.
                    Some(PuppetCommand::Quit { .. }) => {
                        drop(guard);
                        report(PuppetEvent::Ended {
                            peer, attempt, nick: None, confirmed: false, why: "quit before registration".into(),
                        }).await;
                        return;
                    }
                    // The executor dropped our command sender: cancelled.
                    None => return,
                    Some(_) => continue,
                }
            }
            event = inbound.recv() => event,
        };
        let line = match event {
            Some(FromServer::Line(line)) => line,
            Some(FromServer::Closed(why)) => {
                drop(guard);
                report(PuppetEvent::Ended {
                    peer,
                    attempt,
                    nick: None,
                    confirmed: false,
                    why,
                })
                .await;
                return;
            }
            None => {
                drop(guard);
                report(PuppetEvent::Ended {
                    peer,
                    attempt,
                    nick: None,
                    confirmed: false,
                    why: "reader ended".into(),
                })
                .await;
                return;
            }
        };
        let msg = IrcMessage::parse(&line);
        if msg.command == "PING" {
            let _ = writer.send_line(&pong(&msg));
        }
        if msg.command == "001" {
            if let Some(given) = msg.params.first() {
                nick.clone_from(given);
            }
        }
        match reg.on_message(&msg) {
            Ok(step) => {
                let ready = step.became_ready;
                if let Some(diagnostic) = step.diagnostic {
                    info!(peer = %peer, "puppet: {diagnostic}");
                }
                for out in step.out {
                    if writer.send_line(&out).is_err() {
                        drop(guard);
                        report(PuppetEvent::Ended {
                            peer,
                            attempt,
                            nick: None,
                            confirmed: false,
                            why: "write failed during registration".into(),
                        })
                        .await;
                        return;
                    }
                }
                if ready {
                    break;
                }
            }
            Err(AdapterError::NickRejected { numeric, .. }) => {
                report(PuppetEvent::NickRejected {
                    peer,
                    attempt,
                    numeric,
                })
                .await;
                // The pool decides (retry tailed / give up); either way this
                // connection is cancelled by the executor. Nothing more to do
                // here but hold the socket until told.
                tokio::select! {
                    _ = cmd_rx.recv() => {}
                    _ = stop.notified() => {}
                }
                return;
            }
            Err(e) => {
                drop(guard);
                report(PuppetEvent::Ended {
                    peer,
                    attempt,
                    nick: None,
                    confirmed: false,
                    why: format!("registration failed: {e}"),
                })
                .await;
                return;
            }
        }
    }
    if let Some(undelivered) = report_or_stop(PuppetEvent::Registered {
        peer: peer.clone(),
        attempt,
        nick: nick.clone(),
    })
    .await
    {
        let reason = format!("{nick}: agent left the mesh");
        let quit = quit_connection(&writer, &mut inbound, &mut nick, reason, quit_grace).await;
        // The connection is over: let go of it BEFORE the reports, which
        // may wait on a queue nobody is draining — a socket must not stay
        // open for a report's sake.
        drop(guard);
        report(undelivered).await;
        for (from, to) in quit.renames {
            report(PuppetEvent::Renamed {
                peer: peer.clone(),
                attempt,
                from,
                to,
            })
            .await;
        }
        report(PuppetEvent::Ended {
            peer,
            attempt,
            nick: Some(nick),
            why: quit_why(quit.confirmed),
            confirmed: quit.confirmed,
        })
        .await;
        return;
    }
    // JOINs the outbound queue could not take yet (`Overflow`: the server is
    // not reading fast enough). A JOIN is membership, not a line to mirror:
    // dropping it would leave the puppet registered but absent from its
    // channels for good, while the executor already answered "queued". Held
    // and retried until they go; a dead connection ends in `Ended` and takes
    // them with it.
    // Deduplicated: a channel is held at most once, so this is bounded by
    // the DISTINCT channels the session ever asks this puppet to join (the
    // lobby and its own channel, in this increment), however often it asks;
    // a count would cap a single Join's channels, not the session's asking.
    let mut pending_joins: Vec<String> = Vec::new();
    // The retry timer is PINNED outside the loop: a timer built inside the
    // select would be dropped and restarted by every other branch that wins,
    // and a busy connection (lines every 100 ms) would never let it fire.
    // It is armed when a JOIN is held and, besides, held JOINs are re-offered
    // after any activity on the connection — the queue drains as the server
    // reads, and any event is a chance it has.
    let join_retry = Duration::from_millis(irc.puppets.join_retry_ms);
    let retry_at = tokio::time::sleep(join_retry);
    tokio::pin!(retry_at);
    // ── Registered: serve commands, answer PING, hand lines up ──────────────
    // Biased, stop first: the out-of-band QUIT is sent by an executor that
    // has already dropped the command sender, so the queue reads as its
    // buffered commands and then `None`. Unbiased, `None` could win the race
    // against the pending stop and the task would leave as if cancelled — no
    // QUIT, no report. Stop first, the buffered commands are simply not
    // served: nothing after a QUIT is routed anyway.
    loop {
        if !pending_joins.is_empty() {
            send_joins(&mut writer, &mut pending_joins, &peer, &nick);
            if !pending_joins.is_empty() {
                retry_at.as_mut().reset(Instant::now() + join_retry);
            }
        }
        tokio::select! {
            biased;
            // The out-of-band QUIT. Same exit, same report, as the command.
            _ = stop.notified() => {
                let reason = format!("{nick}: agent left the mesh");
                let quit = quit_connection(&writer, &mut inbound, &mut nick, reason, quit_grace).await;
                drop(guard);
                for (from, to) in quit.renames {
                    report(PuppetEvent::Renamed { peer: peer.clone(), attempt, from, to }).await;
                }
                report(PuppetEvent::Ended { peer, attempt, nick: Some(nick), why: quit_why(quit.confirmed), confirmed: quit.confirmed }).await;
                return;
            }
            cmd = cmd_rx.recv() => match cmd {
                Some(PuppetCommand::Join(channels)) => {
                    // Offered at the top of the next iteration.
                    for ch in channels {
                        if !pending_joins.contains(&ch) {
                            pending_joins.push(ch);
                        }
                    }
                }
                Some(PuppetCommand::Send(line)) => {
                    let _ = writer.send_line(&line);
                }
                Some(PuppetCommand::Quit { reason, grace }) => {
                    let quit = quit_connection(&writer, &mut inbound, &mut nick, reason, grace).await;
                    drop(guard);
                    for (from, to) in quit.renames {
                        report(PuppetEvent::Renamed { peer: peer.clone(), attempt, from, to }).await;
                    }
                    // The one report a quitting task still owes: the server
                    // has closed this connection (or the grace ran out on
                    // it, and the reason says so), and the session resolves
                    // the nick from this, whatever channels it shared.
                    report(PuppetEvent::Ended { peer, attempt, nick: Some(nick), why: quit_why(quit.confirmed), confirmed: quit.confirmed }).await;
                    return;
                }
                // The executor dropped our command sender: cancelled.
                None => return,
            },
            // Held JOINs, on a quiet connection: the timer is the only wake.
            _ = &mut retry_at, if !pending_joins.is_empty() => {}
            event = inbound.recv() => match event {
                Some(FromServer::Line(line)) => {
                    let msg = IrcMessage::parse(&line);
                    if msg.command == "PING" {
                        let _ = writer.send_line(&pong(&msg));
                        continue;
                    }
                    if let Some(to) = own_rename(&msg, &nick) {
                        // The server respelled us. Our record follows first,
                        // so the Ended after this names the current nick.
                        let from = std::mem::replace(&mut nick, to.clone());
                        if let Some(undelivered) = report_or_stop(PuppetEvent::Renamed { peer: peer.clone(), attempt, from, to }).await {
                            let reason = format!("{nick}: agent left the mesh");
                            let quit = quit_connection(&writer, &mut inbound, &mut nick, reason, quit_grace).await;
                            drop(guard);
                            // The rename first, so the Ended names a nick the
                            // session has heard of.
                            report(undelivered).await;
                            for (from, to) in quit.renames {
                                report(PuppetEvent::Renamed { peer: peer.clone(), attempt, from, to }).await;
                            }
                            report(PuppetEvent::Ended { peer, attempt, nick: Some(nick), why: quit_why(quit.confirmed), confirmed: quit.confirmed }).await;
                            return;
                        }
                        continue;
                    }
                    report_line(PuppetEvent::Line { peer: peer.clone(), attempt, line }).await;
                }
                Some(FromServer::Closed(why)) => {
                    // The server's own close (EOF) is confirmed: it processed
                    // everything before it hung up. Any other reason is this
                    // side's — a read or write failure, a dropped writer —
                    // and says nothing about what the server has seen.
                    let confirmed = why == transport::SERVER_CLOSED;
                    drop(guard);
                    report(PuppetEvent::Ended {
                        peer,
                        attempt,
                        nick: Some(nick.clone()),
                        why,
                        confirmed,
                    }).await;
                    return;
                }
                None => {
                    drop(guard);
                    report(PuppetEvent::Ended {
                        peer,
                        attempt,
                        nick: Some(nick.clone()),
                        why: "reader ended".into(),
                        confirmed: false,
                    }).await;
                    return;
                }
            },
        }
    }
}

/// Write every pending JOIN the outbound queue will take, in order; the ones
/// it refuses (`Overflow`) stay pending, in order, for the next try. On a
/// connection that is gone the rest are dropped: an `Ended` is on its way.
fn send_joins(
    writer: &mut transport::LineWriter,
    pending: &mut Vec<String>,
    peer: &PeerId,
    nick: &str,
) {
    while !pending.is_empty() {
        match writer.send_line(&format!("JOIN {}", pending[0])) {
            Ok(()) => {
                pending.remove(0);
            }
            Err(transport::SendError::Overflow) => {
                debug!(peer = %peer, nick = %nick, pending = pending.len(), "puppet: JOIN held (outbound queue full)");
                return;
            }
            Err(transport::SendError::Disconnected) => {
                warn!(peer = %peer, nick = %nick, dropped = pending.len(), "puppet: JOINs dropped (connection gone)");
                pending.clear();
                return;
            }
        }
    }
}

/// Leave by QUIT, and see it through: the line is written on a line
/// boundary, then the connection is read until the SERVER closes it — its
/// acknowledgement that the QUIT, and everything this connection sent before
/// it, has been processed. `true` then: the session may treat the `Ended`
/// that follows as "the server has seen this puppet leave" and order its
/// departure barrier on the main connection behind it. One deadline covers
/// both halves: at `grace` the writer is forced and the socket cut whatever
/// the server has done (a peer that never answers cannot hold the task), and
/// that is `false` — the server has NOT confirmed anything, and the `Ended`
/// says so (`why: "quit (forced)"`), so the session does not order on it.
///
/// The server may still rename this puppet while its QUIT is in flight: a
/// `NICK` for us among the lines drained here moves `nick` and is returned,
/// so the caller reports it before the `Ended` and the `Ended` names the
/// nick the server last knew.
async fn quit_connection(
    writer: &transport::LineWriter,
    inbound: &mut mpsc::Receiver<FromServer>,
    nick: &mut String,
    reason: String,
    grace: Duration,
) -> Quit {
    let mut renames = Vec::new();
    writer.begin_graceful_stop(format!("QUIT :{reason}"));
    let done = writer.stop_completion();
    let deadline = tokio::time::sleep(grace);
    tokio::pin!(deadline);
    let written = match done {
        Some(mut done) => {
            tokio::select! {
                _ = &mut deadline => { writer.force_stop(); false }
                _ = async { while done.changed().await.is_ok() { if *done.borrow() { break; } } } => true,
            }
        }
        None => {
            (&mut deadline).await;
            false
        }
    };
    if !written {
        return Quit {
            confirmed: false,
            renames,
        };
    }
    // The QUIT is on the wire. Now the server's side of it: it answers with
    // an ERROR and closes, which reaches us as the reader ending. Lines
    // before that are drained and dropped — nothing after a QUIT is routed.
    // Only the SERVER's close confirms. A `Closed` for any other reason —
    // the forced stop's own, a QUIT write that failed locally after
    // completion fired, a read error — is this side's, and confirms nothing.
    let confirmed = loop {
        tokio::select! {
            biased;
            _ = &mut deadline => { writer.force_stop(); break false; }
            event = inbound.recv() => match event {
                Some(FromServer::Line(line)) => {
                    let msg = IrcMessage::parse(&line);
                    if let Some(to) = own_rename(&msg, nick) {
                        let from = std::mem::replace(nick, to.clone());
                        renames.push((from, to));
                    }
                }
                Some(FromServer::Closed(why)) => break why == transport::SERVER_CLOSED,
                None => break false,
            }
        }
    };
    Quit { confirmed, renames }
}

/// What a QUIT came to: whether the server confirmed it by closing, and the
/// renames the server made in the meantime, in order.
struct Quit {
    confirmed: bool,
    renames: Vec<(String, String)>,
}

/// The `Ended` reason after a QUIT: whether the server closed the
/// connection, or the grace cut it.
fn quit_why(confirmed: bool) -> String {
    if confirmed { "quit" } else { "quit (forced)" }.to_string()
}

/// If `msg` is a `NICK` whose source is `nick` — the server renaming THIS
/// connection — the new nick. The source is compared as the server spelled
/// it at `001` (or at the last rename), with an ASCII case fold as the hedge
/// for a server that respells case in the prefix.
fn own_rename(msg: &IrcMessage, nick: &str) -> Option<String> {
    if msg.command != "NICK" {
        return None;
    }
    let source = msg.prefix.as_deref()?;
    let source = source.split('!').next().unwrap_or(source);
    if source != nick && !source.eq_ignore_ascii_case(nick) {
        return None;
    }
    msg.params.first().filter(|n| !n.is_empty()).cloned()
}

/// The PONG for a PING, echoing its token.
fn pong(msg: &IrcMessage) -> String {
    match msg.params.last() {
        Some(token) => format!("PONG :{token}"),
        None => "PONG".to_string(),
    }
}

/// Test support shared by this module's and the executor's tests: a config,
/// scripted connectors, and how to read the server side of a puppet.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Arc;
    use std::time::Duration;

    use mu_peer::PeerId;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::sync::mpsc;

    use super::{Connector, PuppetEvent};
    use crate::config::{IrcConfig, PuppetsConfig};
    use crate::transport::{self, TlsTrust};

    pub(crate) fn base() -> IrcConfig {
        IrcConfig {
            server: "irc.invalid:6667".into(),
            tls: false,
            tls_trust: TlsTrust::default(),
            nick: "mu-gw".into(),
            sasl: None,
            channel_prefix: "#".into(),
            lobby: "#mu".into(),
            observe_agent_dms: true,
            puppets: PuppetsConfig::default(),
        }
    }

    pub(crate) type Hand = mpsc::UnboundedSender<(String, tokio::io::DuplexStream)>;

    /// A connector that hands each connection's server half to `hand`, so a
    /// test can script the server side of every puppet.
    pub(crate) fn scripted_connector(hand: Hand) -> Connector {
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

    /// `scripted_connector` with a tiny outbound queue and a small socket
    /// buffer, so a server that stops reading fills the writer in a few lines.
    pub(crate) fn choked_connector(hand: Hand) -> Connector {
        Arc::new(move |irc: IrcConfig| {
            let hand = hand.clone();
            Box::pin(async move {
                // Deep enough for the registration burst (CAP LS, NICK,
                // USER, CAP END) while the server is reading; a dozen lines
                // fill it once the server stops.
                let (client, server) = tokio::io::duplex(64);
                hand.send((irc.nick.clone(), server))
                    .map_err(|_| "no server".to_string())?;
                Ok(transport::spawn_connection_with_queue(Box::new(client), 4))
            })
        })
    }

    /// A connector that always fails to connect.
    pub(crate) fn refusing_connector() -> Connector {
        Arc::new(|_irc: IrcConfig| Box::pin(async { Err("connection refused".to_string()) }))
    }

    pub(crate) type ServerRead = BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>;
    pub(crate) type ServerWrite = tokio::io::WriteHalf<tokio::io::DuplexStream>;

    /// Read lines from the server half until one starts with `prefix`.
    pub(crate) async fn read_until(r: &mut ServerRead, prefix: &str) -> String {
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

    pub(crate) fn peer() -> PeerId {
        PeerId::parse("cc:abc")
    }

    /// The next event, or a panic after 5 s.
    pub(crate) async fn next(ev_rx: &mut mpsc::Receiver<PuppetEvent>) -> PuppetEvent {
        tokio::time::timeout(Duration::from_secs(5), ev_rx.recv())
            .await
            .expect("an event")
            .expect("open")
    }

    /// The next `Ended`, skipping whatever comes before it.
    pub(crate) async fn next_ended(ev_rx: &mut mpsc::Receiver<PuppetEvent>) -> PuppetEvent {
        loop {
            let ev = next(ev_rx).await;
            if matches!(ev, PuppetEvent::Ended { .. }) {
                return ev;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    use tokio::io::{AsyncWriteExt, BufReader};

    /// One puppet task spawned by hand — no executor: the test is the
    /// executor, holding the command sender, the stop and the event queue.
    struct Puppet {
        cmd: mpsc::Sender<PuppetCommand>,
        stop: Arc<Notify>,
        ev_rx: mpsc::Receiver<PuppetEvent>,
        hand_rx: mpsc::UnboundedReceiver<(String, tokio::io::DuplexStream)>,
        unqueued: Arc<AtomicU64>,
        task: tokio::task::JoinHandle<()>,
    }

    const ATTEMPT: u64 = 7;

    fn spawn(irc: IrcConfig, connector: impl Fn(Hand) -> Connector, events: usize) -> Puppet {
        let (hand_tx, hand_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::channel(events);
        let (cmd_tx, cmd_rx) = mpsc::channel(irc.puppets.command_queue);
        let stop = Arc::new(Notify::new());
        let unqueued = Arc::new(AtomicU64::new(0));
        let mut irc = irc;
        irc.nick = "cc-abc".into();
        irc.sasl = None;
        let task = tokio::spawn(puppet_task(Spawn {
            peer: peer(),
            attempt: ATTEMPT,
            irc,
            connector: connector(hand_tx),
            events: ev_tx,
            cmd_rx,
            stop: stop.clone(),
            registration_timeout: Duration::from_secs(5),
            lines_unqueued: unqueued.clone(),
        }));
        Puppet {
            cmd: cmd_tx,
            stop,
            ev_rx,
            hand_rx,
            unqueued,
            task,
        }
    }

    impl Puppet {
        /// What the executor does for a Quit: the stop, and the command for
        /// the record; the sender is dropped either way.
        fn quit(&mut self) {
            let _ = self.cmd.try_send(PuppetCommand::Quit {
                reason: "cc-abc: agent left the mesh".into(),
                grace: Duration::from_secs(3),
            });
            self.stop.notify_one();
        }

        /// Walk the puppet through registration; the `Registered` event has
        /// been consumed. Returns the server side.
        async fn registered(&mut self) -> (ServerRead, ServerWrite) {
            let (_nick, server) = self.hand_rx.recv().await.expect("a connection was opened");
            let (rh, mut wh) = tokio::io::split(server);
            let mut r = BufReader::new(rh);
            read_until(&mut r, "NICK ").await;
            wh.write_all(b":srv CAP * LS :\r\n").await.unwrap();
            // 001 alone registers; a MOTD tail would be handed up as lines.
            wh.write_all(b":srv 001 cc-abc :Welcome\r\n").await.unwrap();
            let ev = next(&mut self.ev_rx).await;
            assert!(
                matches!(&ev, PuppetEvent::Registered { attempt: ATTEMPT, nick, .. } if nick == "cc-abc"),
                "{ev:?}"
            );
            (r, wh)
        }
    }

    #[tokio::test]
    async fn registration_reports_the_welcome_nick_and_the_task_serves_its_commands() {
        let mut p = spawn(base(), scripted_connector, 16);
        let (nick, server) = p.hand_rx.recv().await.expect("a connection was opened");
        assert_eq!(nick, "cc-abc", "the puppet config carries the offered nick");
        let (rh, mut wh) = tokio::io::split(server);
        let mut r = BufReader::new(rh);
        // The adapter sends NICK/USER (after CAP LS); welcome it, respelled.
        read_until(&mut r, "NICK ").await;
        wh.write_all(b":srv CAP * LS :\r\n").await.unwrap();
        wh.write_all(b":srv 001 CC-abc :Welcome\r\n").await.unwrap();
        wh.write_all(b":srv 376 CC-abc :End of MOTD\r\n")
            .await
            .unwrap();
        let ev = next(&mut p.ev_rx).await;
        assert!(
            matches!(&ev, PuppetEvent::Registered { nick, attempt: ATTEMPT, .. } if nick == "CC-abc"),
            "{ev:?}"
        );
        // The MOTD tail is a line, handed up.
        let ev = next(&mut p.ev_rx).await;
        assert!(matches!(ev, PuppetEvent::Line { .. }), "{ev:?}");
        p.cmd
            .try_send(PuppetCommand::Join(vec!["#mu".into(), "#cc-abc".into()]))
            .unwrap();
        let j1 = read_until(&mut r, "JOIN ").await;
        let j2 = read_until(&mut r, "JOIN ").await;
        assert_eq!((j1.trim(), j2.trim()), ("JOIN #mu", "JOIN #cc-abc"));
        // PING is answered by the task itself and never handed up.
        wh.write_all(b"PING :tok\r\n").await.unwrap();
        assert_eq!(read_until(&mut r, "PONG").await.trim(), "PONG :tok");
        // A Send goes out as given; any other inbound line is handed up.
        p.cmd
            .try_send(PuppetCommand::Send("PRIVMSG #mu :hello".into()))
            .unwrap();
        assert_eq!(
            read_until(&mut r, "PRIVMSG").await.trim(),
            "PRIVMSG #mu :hello"
        );
        wh.write_all(b":alice!u@h PRIVMSG cc-abc :hi\r\n")
            .await
            .unwrap();
        let ev = next(&mut p.ev_rx).await;
        assert!(
            matches!(&ev, PuppetEvent::Line { line, .. } if line.contains("hi")),
            "{ev:?}"
        );
        // QUIT: the line goes out, the server closes, the departure is
        // reported with the nick, confirmed.
        p.quit();
        // The stop's QUIT names the nick the task holds — the respelled one.
        assert!(read_until(&mut r, "QUIT").await.starts_with("QUIT :CC-abc"));
        drop(wh);
        drop(r);
        let ev = next_ended(&mut p.ev_rx).await;
        assert!(
            matches!(&ev, PuppetEvent::Ended { nick, why, confirmed: true, attempt: ATTEMPT, .. }
                if nick.as_deref() == Some("CC-abc") && why == "quit"),
            "{ev:?}"
        );
        p.task.await.unwrap();
    }

    #[tokio::test]
    async fn a_nick_rejection_is_reported_and_the_connection_waits_to_be_told() {
        let mut p = spawn(base(), scripted_connector, 16);
        let (_nick, server) = p.hand_rx.recv().await.unwrap();
        let (rh, mut wh) = tokio::io::split(server);
        let mut r = BufReader::new(rh);
        read_until(&mut r, "NICK ").await;
        wh.write_all(b":srv CAP * LS :\r\n").await.unwrap();
        wh.write_all(b":srv 433 * cc-abc :Nickname is already in use\r\n")
            .await
            .unwrap();
        let ev = next(&mut p.ev_rx).await;
        assert!(
            matches!(&ev, PuppetEvent::NickRejected { numeric, attempt: ATTEMPT, .. } if numeric == "433"),
            "{ev:?}"
        );
        // The task holds the socket until the executor decides: nothing
        // more is reported, and the connection stays up…
        assert!(
            tokio::time::timeout(Duration::from_millis(200), p.ev_rx.recv())
                .await
                .is_err()
        );
        wh.write_all(b":srv NOTICE * :still here\r\n")
            .await
            .unwrap();
        // …until the sender is dropped (a cancel), which ends it silently.
        drop(p.cmd);
        tokio::time::timeout(Duration::from_secs(5), p.task)
            .await
            .expect("the task ends when cancelled")
            .unwrap();
    }

    #[tokio::test]
    async fn a_refused_connection_and_a_registration_timeout_report_ended() {
        let mut p = spawn(base(), |_hand| refusing_connector(), 16);
        let ev = next(&mut p.ev_rx).await;
        assert!(
            matches!(&ev, PuppetEvent::Ended { why, nick: None, confirmed: false, .. } if why.contains("refused")),
            "{ev:?}"
        );
        // A server that never welcomes: the registration timeout ends it.
        let (hand_tx, mut hand_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::channel(16);
        let (_cmd_tx, cmd_rx) = mpsc::channel(4);
        let mut irc = base();
        irc.nick = "cc-abc".into();
        let task = tokio::spawn(puppet_task(Spawn {
            peer: peer(),
            attempt: ATTEMPT,
            irc,
            connector: scripted_connector(hand_tx),
            events: ev_tx,
            cmd_rx,
            stop: Arc::new(Notify::new()),
            registration_timeout: Duration::from_millis(300),
            lines_unqueued: Arc::new(AtomicU64::new(0)),
        }));
        let (_nick, _server) = hand_rx.recv().await.unwrap();
        let ev = next(&mut ev_rx).await;
        assert!(
            matches!(&ev, PuppetEvent::Ended { why, nick: None, .. } if why.contains("did not complete")),
            "{ev:?}"
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn a_stalled_puppet_told_to_quit_still_quits_and_reports_its_departure() {
        // A one-deep command queue the session fills before the Quit: the
        // Quit cannot be queued, and the puppet must still leave by QUIT and
        // still report the Ended that resolves its nick.
        let mut cfg = base();
        cfg.puppets.command_queue = 1;
        let mut p = spawn(cfg, scripted_connector, 16);
        let (mut r, wh) = p.registered().await;
        // Fill the queue while the task is parked on the socket: nothing
        // reads the command channel until the task's select wakes.
        assert!(p
            .cmd
            .try_send(PuppetCommand::Send("PRIVMSG #mu :x".into()))
            .is_ok());
        assert!(p
            .cmd
            .try_send(PuppetCommand::Send("PRIVMSG #mu :y".into()))
            .is_err());
        p.quit();
        // The QUIT reaches the server all the same…
        let quit = read_until(&mut r, "QUIT").await;
        assert!(quit.starts_with("QUIT :cc-abc"), "{quit}");
        drop(wh);
        // Both halves: a split duplex closes only when both are gone,
        // the way a server closing the socket reads as EOF here.
        drop(r);
        // …and the departure is reported under the nick.
        let ev = next_ended(&mut p.ev_rx).await;
        assert!(
            matches!(&ev, PuppetEvent::Ended { nick, why, .. }
                if nick.as_deref() == Some("cc-abc") && why == "quit"),
            "{ev:?}"
        );
    }

    #[tokio::test]
    async fn a_forced_rename_is_a_lifecycle_event_and_the_ended_names_the_new_nick() {
        let mut p = spawn(base(), scripted_connector, 16);
        let (mut r, mut wh) = p.registered().await;
        // Somebody else's NICK is a line like any other.
        wh.write_all(b":alice!u@h NICK :alice2\r\n").await.unwrap();
        let ev = next(&mut p.ev_rx).await;
        assert!(matches!(ev, PuppetEvent::Line { .. }), "{ev:?}");
        // Ours — sourced by the nick the server welcomed us as — is a rename.
        wh.write_all(b":cc-abc!u@h NICK :Guest42\r\n")
            .await
            .unwrap();
        let ev = next(&mut p.ev_rx).await;
        assert_eq!(
            ev,
            PuppetEvent::Renamed {
                peer: peer(),
                attempt: ATTEMPT,
                from: "cc-abc".into(),
                to: "Guest42".into(),
            }
        );
        // The task's record followed: the departure names the new nick.
        p.quit();
        read_until(&mut r, "QUIT").await;
        drop(wh);
        drop(r);
        let ev = next_ended(&mut p.ev_rx).await;
        assert!(
            matches!(&ev, PuppetEvent::Ended { nick, .. } if nick.as_deref() == Some("Guest42")),
            "{ev:?}"
        );
    }

    #[tokio::test]
    async fn a_join_the_outbound_queue_refuses_is_held_and_sent_once_it_drains() {
        // The server stops reading; the puppet's outbound queue (4 deep here)
        // and socket buffer fill with lines; the JOIN then cannot be queued.
        // It is not dropped: when the server reads again, the JOIN follows.
        let mut p = spawn(base(), choked_connector, 16);
        let (mut r, wh) = p.registered().await;
        // Nobody reads the server side now. Enough lines to fill the socket
        // buffer and the queue; each Send is consumed by the task before the
        // next is offered (a yield lets it run), so the writer is what fills.
        for i in 0..12 {
            p.cmd
                .try_send(PuppetCommand::Send(format!("PRIVMSG #mu :{i:0>40}")))
                .unwrap();
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
        }
        p.cmd
            .try_send(PuppetCommand::Join(vec!["#mu".into()]))
            .unwrap();
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        // Meanwhile the connection is busy: a line arrives every few ms,
        // faster than the retry interval, so a timer that restarted on every
        // event would never fire. The held JOIN must still go once the
        // server reads again.
        let mut wh = wh;
        let chatter = tokio::spawn(async move {
            for i in 0..400u32 {
                if wh
                    .write_all(format!(":alice!u@h PRIVMSG #mu :{i}\r\n").as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            wh
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        // The server reads again: every line comes out, and the JOIN with
        // them — after the lines that were ahead of it, and before anything
        // sent later — and well inside the two seconds of chatter, which is
        // what a timer restarted by every line would have waited out.
        let reading = Instant::now();
        let join = read_until(&mut r, "JOIN ").await;
        assert_eq!(join.trim(), "JOIN #mu");
        assert!(
            reading.elapsed() < Duration::from_millis(800),
            "the held JOIN waited {:?} for a quiet gap",
            reading.elapsed()
        );
        p.cmd
            .try_send(PuppetCommand::Send("PRIVMSG #mu :after".into()))
            .unwrap();
        assert_eq!(
            read_until(&mut r, "PRIVMSG").await.trim(),
            "PRIVMSG #mu :after"
        );
        // The chatter was handed up (or, past the queue, counted): a line
        // the queue could not take is never lost silently.
        assert!(p.unqueued.load(Ordering::Relaxed) > 0 || !p.ev_rx.is_empty());
        drop(p.ev_rx);
        let _wh = chatter.await.unwrap();
    }

    #[tokio::test]
    async fn a_stalled_puppet_told_to_quit_before_registration_still_reports_ended() {
        // Same race as the registered loop, before 001: the command queue is
        // full, the Quit cannot be queued, the executor signals the stop and
        // drops the sender. The task must take the stop over the queue's None.
        let mut cfg = base();
        cfg.puppets.command_queue = 1;
        let mut p = spawn(cfg, scripted_connector, 16);
        let (_nick, server) = p.hand_rx.recv().await.unwrap();
        let (rh, _wh) = tokio::io::split(server);
        let mut r = BufReader::new(rh);
        read_until(&mut r, "NICK ").await;
        // Fill the (one-deep) queue while the task is parked on the socket.
        assert!(p.cmd.try_send(PuppetCommand::Send("x".into())).is_ok());
        assert!(p.cmd.try_send(PuppetCommand::Send("y".into())).is_err());
        p.stop.notify_one();
        drop(p.cmd);
        let ev = next(&mut p.ev_rx).await;
        assert!(
            matches!(&ev, PuppetEvent::Ended { why, nick: None, .. } if why == "quit before registration"),
            "{ev:?}"
        );
        // A Quit that IS queued before registration says the same.
        let mut p = spawn(base(), scripted_connector, 16);
        let (_nick, server) = p.hand_rx.recv().await.unwrap();
        let (rh, _wh) = tokio::io::split(server);
        let mut r = BufReader::new(rh);
        read_until(&mut r, "NICK ").await;
        p.cmd
            .try_send(PuppetCommand::Quit {
                reason: "bye".into(),
                grace: Duration::from_secs(1),
            })
            .unwrap();
        let ev = next(&mut p.ev_rx).await;
        assert!(
            matches!(&ev, PuppetEvent::Ended { why, nick: None, .. } if why == "quit before registration"),
            "{ev:?}"
        );
    }

    #[tokio::test]
    async fn a_quit_is_reported_ended_when_the_server_closes_or_the_grace_runs_out() {
        // The QUIT is written; the server has not closed yet. No Ended: the
        // report means "the server has seen this puppet leave". When the
        // server closes, the Ended follows at once.
        let mut p = spawn(base(), scripted_connector, 16);
        let (mut r, mut wh) = p.registered().await;
        p.quit();
        read_until(&mut r, "QUIT").await;
        // The server is slow to close; it even says something first.
        wh.write_all(b"ERROR :Closing link\r\n").await.unwrap();
        let early =
            tokio::time::timeout(Duration::from_millis(300), next_ended(&mut p.ev_rx)).await;
        assert!(early.is_err(), "Ended reported before the server closed");
        drop(wh);
        drop(r);
        let ev = next_ended(&mut p.ev_rx).await;
        assert!(
            matches!(&ev, PuppetEvent::Ended { why, confirmed: true, .. } if why == "quit"),
            "{ev:?}"
        );
        // A server that never closes: the grace bounds it.
        let mut cfg = base();
        cfg.puppets.quit_grace_secs = 1;
        let mut p = spawn(cfg, scripted_connector, 16);
        let (mut r, _wh) = p.registered().await;
        let started = Instant::now();
        p.stop.notify_one();
        read_until(&mut r, "QUIT").await;
        let ev = next_ended(&mut p.ev_rx).await;
        assert!(
            started.elapsed() >= Duration::from_millis(900),
            "{:?}",
            started.elapsed()
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        // …and the report says the server never confirmed it.
        assert!(
            matches!(&ev, PuppetEvent::Ended { why, confirmed: false, .. } if why == "quit (forced)"),
            "{ev:?}"
        );
    }

    #[tokio::test]
    async fn a_live_drop_by_the_server_is_a_confirmed_departure() {
        // No QUIT was issued: the server ends the connection itself. Its
        // EOF is the one close reason that is the server's, so the Ended is
        // confirmed — the server processed everything before it hung up.
        // (Every other reason — a forced stop, a QUIT write that failed
        // locally, a read error — is this side's and is not, by the same
        // comparison against `transport::SERVER_CLOSED`.)
        let mut p = spawn(base(), scripted_connector, 16);
        let (r, wh) = p.registered().await;
        drop(wh);
        drop(r);
        let ev = next_ended(&mut p.ev_rx).await;
        assert!(
            matches!(&ev, PuppetEvent::Ended { confirmed: true, why, nick, .. }
                if why == transport::SERVER_CLOSED && nick.as_deref() == Some("cc-abc")),
            "{ev:?}"
        );
    }

    #[tokio::test]
    async fn a_report_blocked_on_a_full_event_queue_still_yields_to_a_quit() {
        // The session is not draining (queue of 1, already full). The
        // puppet is renamed, so it owes a Renamed it cannot deliver; the
        // Quit must still get the QUIT written and the task ended — and the
        // Renamed is delivered after all, ahead of the Ended.
        let mut p = spawn(base(), scripted_connector, 1);
        let (mut r, mut wh) = p.registered().await;
        // Fill the queue with a line nobody reads, then rename the puppet:
        // its Renamed now blocks.
        wh.write_all(b":alice!u@h PRIVMSG #mu :fill\r\n")
            .await
            .unwrap();
        wh.write_all(b":cc-abc!u@h NICK :Guest42\r\n")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!p.ev_rx.is_empty(), "the queue should be full");
        p.quit();
        // The QUIT goes out although the Renamed was never delivered.
        let quit = read_until(&mut r, "QUIT").await;
        assert!(quit.starts_with("QUIT :Guest42"), "{quit}");
        drop(wh);
        drop(r);
        let mut saw_renamed = false;
        let ev = loop {
            let ev = next(&mut p.ev_rx).await;
            match ev {
                PuppetEvent::Ended { .. } => break ev,
                PuppetEvent::Renamed { ref to, .. } => {
                    assert_eq!(to, "Guest42");
                    saw_renamed = true;
                }
                _ => {}
            }
        };
        assert!(saw_renamed, "the pre-empted Renamed was never delivered");
        assert!(
            matches!(&ev, PuppetEvent::Ended { nick, why, .. }
                if nick.as_deref() == Some("Guest42") && why == "quit"),
            "{ev:?}"
        );
    }

    #[tokio::test]
    async fn a_forced_deadline_closes_the_socket_even_with_nobody_draining_reports() {
        // The server never closes after the QUIT and the session is not
        // draining events (queue of 1, full). The grace must still end the
        // connection, whatever reports the task is still waiting to deliver.
        let mut cfg = base();
        cfg.puppets.quit_grace_secs = 1;
        let mut p = spawn(cfg, scripted_connector, 1);
        let (mut r, mut wh) = p.registered().await;
        wh.write_all(b":alice!u@h PRIVMSG #mu :fill\r\n")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!p.ev_rx.is_empty());
        let started = Instant::now();
        p.quit();
        read_until(&mut r, "QUIT").await;
        // The server keeps the connection open and keeps talking. Its writes
        // land while the client's read side is up (the QUIT half-closed only
        // the write side) and fail once the client has cut the connection —
        // which must happen at the grace, not when the reports drain.
        let cut = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if wh
                    .write_all(b":srv NOTICE cc-abc :still here\r\n")
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(
            cut.is_ok(),
            "the socket outlived the grace with reports pending"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(900),
            "{:?}",
            started.elapsed()
        );
        drop(r);
        // Only now does the task get to deliver: the fill line, then Ended —
        // unconfirmed, since the server never closed.
        let ev = next_ended(&mut p.ev_rx).await;
        assert!(
            matches!(&ev, PuppetEvent::Ended { why, confirmed: false, .. } if why == "quit (forced)"),
            "{ev:?}"
        );
    }

    #[tokio::test]
    async fn a_rename_the_server_makes_while_the_quit_is_in_flight_is_reported() {
        // The QUIT is written; before the server closes, it renames the
        // puppet (a forced NICK). The rename is reported ahead of the Ended,
        // and the Ended names the nick the server last knew.
        let mut p = spawn(base(), scripted_connector, 16);
        let (mut r, mut wh) = p.registered().await;
        p.quit();
        read_until(&mut r, "QUIT").await;
        wh.write_all(b":cc-abc!u@h NICK :Guest9\r\n").await.unwrap();
        wh.write_all(b"ERROR :Closing link\r\n").await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(wh);
        drop(r);
        let mut renamed_to = None;
        let ev = loop {
            let ev = next(&mut p.ev_rx).await;
            match ev {
                PuppetEvent::Ended { .. } => break ev,
                PuppetEvent::Renamed { ref to, .. } => renamed_to = Some(to.clone()),
                _ => {}
            }
        };
        assert_eq!(
            renamed_to.as_deref(),
            Some("Guest9"),
            "the in-flight rename was dropped"
        );
        assert!(
            matches!(&ev, PuppetEvent::Ended { nick, confirmed: true, .. } if nick.as_deref() == Some("Guest9")),
            "{ev:?}"
        );
    }

    #[test]
    fn own_rename_matches_the_source_and_pong_echoes_the_token() {
        let m = IrcMessage::parse(":cc-abc!u@h NICK :Guest1");
        assert_eq!(own_rename(&m, "cc-abc").as_deref(), Some("Guest1"));
        assert_eq!(
            own_rename(&m, "CC-ABC").as_deref(),
            Some("Guest1"),
            "ascii-folded hedge"
        );
        assert_eq!(own_rename(&m, "alice"), None);
        assert_eq!(
            own_rename(&IrcMessage::parse("NICK :x"), "cc-abc"),
            None,
            "no source"
        );
        assert_eq!(
            own_rename(&IrcMessage::parse(":cc-abc!u@h JOIN #mu"), "cc-abc"),
            None
        );
        assert_eq!(pong(&IrcMessage::parse("PING :tok")), "PONG :tok");
        assert_eq!(pong(&IrcMessage::parse("PING")), "PONG");
    }
}
