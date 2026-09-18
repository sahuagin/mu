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
//! - **A JOIN is membership and a PONG is the connection, not mirrored
//!   lines.** One the outbound queue refuses is held and re-offered until
//!   it goes; a flood of mirrored lines costs neither.
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
use tracing::{info, warn};

use super::puppet_wire::{
    hold_pong, own_rename, pong, quit_connection, quit_why, registration_write_failed,
    send_pending, stop_pending,
};
use crate::adapter::{AdapterError, IrcMessage, Registration, SystemClock, Transport};
use crate::config::IrcConfig;
use crate::transport::{self, Connection, FromServer, SendError};

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
        /// failed locally). A confirmed end is the server's close, and that
        /// is all it is: the server deregistered the puppet before closing,
        /// so its departure was processed and broadcast there first, by
        /// whatever brought it about, and the session orders its departure
        /// barrier behind that. It is the best word the transport has, not
        /// an acknowledgement — a link cut between the two looks the same
        /// from here, and the server then keeps the puppet until its own
        /// timeout; see [`transport::SERVER_CLOSED`]. An unconfirmed end
        /// says nothing about the server at all.
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
///
/// The executor ends a task two ways. A QUIT: it signals `stop` (and queues
/// a `Quit` for the record), then drops `cmd_rx`'s sender; the task leaves
/// by QUIT and reports. A CANCEL: it ABORTS the task — `JoinHandle::abort`,
/// which the executor's handle wrapper calls on drop (a bare `JoinHandle`
/// dropped would only detach the task) — and that ends it wherever it is,
/// socket and all, with no report. The task's own "sender dropped" paths are
/// the second half of a QUIT, not the cancel; a cancel needs nothing from
/// the task.
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
    /// Shared with the executor: lines lost at this task's full queues —
    /// inbound lines it could not hand up, mirrored lines the socket's
    /// bounded queue refused.
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
    // the Ended naming a nick it never heard of). A CANCEL is not watched
    // here, because a cancel is not a signal this task waits for: the
    // executor cancels an attempt by aborting its task (`Spawn`), which ends
    // a report in flight along with everything else.
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
    // The dial itself is interruptible: a stop, or the executor dropping the
    // command sender (a cancel), ends the attempt at once rather than after
    // the connector's own timeout — a teardown must not wait on a stalled
    // dial. A connection that resolves against a pending stop is closed
    // unused.
    // `Some(dialled)`, or `None` with `told_to_quit`: a stop or a Quit
    // command owes an Ended; a cancel (the sender dropped, no stop) nothing.
    // The dial lives in this block and no longer: a dial interrupted is
    // DROPPED — socket, TLS handshake and all — before anything is reported,
    // never held by a report waiting on a queue nobody drains.
    let mut told_to_quit = false;
    let connected = {
        let dial = (connector)(irc.clone());
        tokio::pin!(dial);
        loop {
            tokio::select! {
                result = &mut dial => break Some(result),
                _ = stop.notified() => { told_to_quit = true; break None; }
                cmd = cmd_rx.recv() => match cmd {
                    Some(PuppetCommand::Quit { .. }) => { told_to_quit = true; break None; }
                    None => { told_to_quit = stop_pending(&stop).await; break None; }
                    Some(_) => continue,
                },
            }
        }
    };
    let Connection {
        mut writer,
        mut inbound,
        guard,
    } = match connected {
        Some(Ok(c)) => c,
        Some(Err(why)) => {
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
            // The dial is gone with its block: a connection that was about
            // to resolve is closed unused.
            if told_to_quit {
                report(PuppetEvent::Ended {
                    peer,
                    attempt,
                    nick: None,
                    confirmed: false,
                    why: "quit before registration".into(),
                })
                .await;
            }
            return;
        }
    };
    for line in &first {
        if let Err(e) = writer.send_line(line) {
            drop(guard);
            report(PuppetEvent::Ended {
                peer,
                attempt,
                nick: None,
                confirmed: false,
                why: registration_write_failed(e),
            })
            .await;
            return;
        }
    }
    let deadline = Instant::now() + registration_timeout;
    let mut nick = irc.nick.clone();
    // CONTROL lines the outbound queue could not take yet (`Overflow`: the
    // server is not reading fast enough): a JOIN, or a PONG. Neither is a
    // line to mirror. A JOIN is membership — dropping it would leave the
    // puppet registered but absent from its channels for good, while the
    // executor already answered "queued". A PONG is the connection itself —
    // a server whose PING goes unanswered drops the puppet, and a flood of
    // mirrored lines must not cost it that; a server may PING before `001`
    // too, while the registration burst is still queued. Held, in order,
    // and retried until they go; a dead connection ends in `Ended` and
    // takes them with it. Bounded: a JOIN is held at most once per channel
    // (the session asks for two channels, in this increment), and at most
    // ONE PONG — the latest PING's — is held, whatever tokens the server
    // sends.
    let mut pending: Vec<String> = Vec::new();
    // The retry timer is PINNED outside the loops: a timer built inside a
    // select would be dropped and restarted by every other branch that wins,
    // and a busy connection (lines every 100 ms) would never let it fire.
    // It is armed when a line is held and, besides, held lines are
    // re-offered after any activity on the connection — the queue drains as
    // the server reads, and any event is a chance it has.
    let join_retry = Duration::from_millis(irc.puppets.join_retry_ms);
    let retry_at = tokio::time::sleep(join_retry);
    tokio::pin!(retry_at);
    // ── Registration ────────────────────────────────────────────────────────
    // Unbiased: no branch can starve another. The one race that needs an
    // order — the executor signals the stop and then drops the command
    // sender, so the queue's `None` can be read while the stop is still
    // pending — is settled by asking for the stop before treating `None`
    // as a cancel (`stop_pending`).
    loop {
        if !pending.is_empty() {
            send_pending(&mut writer, &mut pending, &peer, &nick);
            if !pending.is_empty() {
                retry_at.as_mut().reset(Instant::now() + join_retry);
            }
        }
        let event = tokio::select! {
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
                    // The executor dropped our command sender: cancelled —
                    // unless a stop was signalled first.
                    None => {
                        if stop_pending(&stop).await {
                            drop(guard);
                            report(PuppetEvent::Ended {
                                peer, attempt, nick: None, confirmed: false, why: "quit before registration".into(),
                            }).await;
                        }
                        return;
                    }
                    Some(_) => continue,
                }
            }
            // A held PONG, on a quiet connection: the timer is the only wake.
            _ = &mut retry_at, if !pending.is_empty() => continue,
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
            hold_pong(&mut writer, &mut pending, pong(&msg));
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
                    if let Err(e) = writer.send_line(&out) {
                        drop(guard);
                        report(PuppetEvent::Ended {
                            peer,
                            attempt,
                            nick: None,
                            confirmed: false,
                            why: registration_write_failed(e),
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
                if let Some(undelivered) = report_or_stop(PuppetEvent::NickRejected {
                    peer: peer.clone(),
                    attempt,
                    numeric,
                })
                .await
                {
                    // Told to go while the report waited: let go of the
                    // socket, then say what happened, in order.
                    drop(guard);
                    report(undelivered).await;
                    report(PuppetEvent::Ended {
                        peer,
                        attempt,
                        nick: None,
                        confirmed: false,
                        why: "quit before registration".into(),
                    })
                    .await;
                    return;
                }
                // The pool decides (retry tailed / give up); either way this
                // connection is cancelled by the executor. Nothing more to do
                // here but hold the socket until told — and a stop or a Quit,
                // rather than a cancel, is told back with an Ended, like on
                // every other path before registration. Any other command
                // is nothing for a connection that never registered: the
                // hold goes on, rather than ending with no word (panel
                // finding, PR #673 run 8).
                let told_to_quit = loop {
                    tokio::select! {
                        cmd = cmd_rx.recv() => match cmd {
                            Some(PuppetCommand::Quit { .. }) => break true,
                            None => break stop_pending(&stop).await,
                            Some(_) => continue,
                        },
                        _ = stop.notified() => break true,
                    }
                };
                drop(guard);
                if told_to_quit {
                    report(PuppetEvent::Ended {
                        peer,
                        attempt,
                        nick: None,
                        confirmed: false,
                        why: "quit before registration".into(),
                    })
                    .await;
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
    // The server's own CASEMAPPING, from this connection's ISUPPORT: what a
    // NICK prefix naming us is compared under from here. A server sends its
    // ISUPPORT after `001` — after registration is READY — so this is
    // refreshed from every `005` the registered loop reads, as the adapter
    // keeps tracking them; the value here is the default until the first.
    let mut cm = reg.isupport().casemapping;
    if let Some(undelivered) = report_or_stop(PuppetEvent::Registered {
        peer: peer.clone(),
        attempt,
        nick: nick.clone(),
    })
    .await
    {
        let reason = format!("{nick}: agent left the mesh");
        let quit = quit_connection(&writer, &mut inbound, &mut nick, cm, reason, quit_grace).await;
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
    // ── Registered: serve commands, answer PING, hand lines up ──────────────
    // The out-of-band QUIT is sent by an executor that has already dropped
    // the command sender, so the queue reads as its buffered commands and
    // then `None`, and `None` could be read while the stop is still pending
    // — the task would leave as if cancelled, no QUIT, no report. So `None`
    // asks for a pending stop first (`stop_pending`), and the select itself
    // is UNBIASED: a command queue the session keeps full must not starve
    // the inbound side (the PINGs that keep the server from dropping us,
    // the server's close), nor the other way round.
    loop {
        if !pending.is_empty() {
            send_pending(&mut writer, &mut pending, &peer, &nick);
            if !pending.is_empty() {
                retry_at.as_mut().reset(Instant::now() + join_retry);
            }
        }
        tokio::select! {
            // The out-of-band QUIT. Same exit, same report, as the command.
            _ = stop.notified() => {
                let reason = format!("{nick}: agent left the mesh");
                let quit = quit_connection(&writer, &mut inbound, &mut nick, cm, reason, quit_grace).await;
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
                        let line = format!("JOIN {ch}");
                        if !pending.contains(&line) {
                            pending.push(line);
                        }
                    }
                }
                Some(PuppetCommand::Send(line)) => match writer.send_line(&line) {
                    Ok(()) => {}
                    Err(SendError::Overflow) => {
                        // A mirrored line the socket's bounded queue refused:
                        // dropped by design (a live mirror, not a queue), but
                        // never silently — counted with the lines this task
                        // could not hand up, and said, body-free (panel
                        // finding, PR #673 run 8).
                        lines_unqueued.fetch_add(1, Ordering::Relaxed);
                        warn!(peer = %peer, "puppet: outbound queue full; dropped one mirrored line");
                    }
                    // The connection is gone: the Ended that says so comes
                    // from the inbound side, below.
                    Err(SendError::Disconnected) => {}
                },
                Some(PuppetCommand::Quit { reason, grace }) => {
                    let quit = quit_connection(&writer, &mut inbound, &mut nick, cm, reason, grace).await;
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
                // The executor dropped our command sender: cancelled —
                // unless a stop was signalled first, which is a QUIT.
                None => {
                    if stop_pending(&stop).await {
                        let reason = format!("{nick}: agent left the mesh");
                        let quit = quit_connection(&writer, &mut inbound, &mut nick, cm, reason, quit_grace).await;
                        drop(guard);
                        for (from, to) in quit.renames {
                            report(PuppetEvent::Renamed { peer: peer.clone(), attempt, from, to }).await;
                        }
                        report(PuppetEvent::Ended { peer, attempt, nick: Some(nick), why: quit_why(quit.confirmed), confirmed: quit.confirmed }).await;
                    }
                    return;
                }
            },
            // Held JOINs, on a quiet connection: the timer is the only wake.
            _ = &mut retry_at, if !pending.is_empty() => {}
            event = inbound.recv() => match event {
                Some(FromServer::Line(line)) => {
                    let msg = IrcMessage::parse(&line);
                    if msg.command == "PING" {
                        hold_pong(&mut writer, &mut pending, pong(&msg));
                        continue;
                    }
                    if msg.command == "005" {
                        // ISUPPORT after readiness: CASEMAPPING (normally
                        // here, after 001) and any live change to it.
                        let _ = reg.on_message(&msg);
                        cm = reg.isupport().casemapping;
                    }
                    if let Some(to) = own_rename(&msg, &nick, cm) {
                        // The server respelled us. Our record follows first,
                        // so the Ended after this names the current nick.
                        let from = std::mem::replace(&mut nick, to.clone());
                        if let Some(undelivered) = report_or_stop(PuppetEvent::Renamed { peer: peer.clone(), attempt, from, to }).await {
                            let reason = format!("{nick}: agent left the mesh");
                            let quit = quit_connection(&writer, &mut inbound, &mut nick, cm, reason, quit_grace).await;
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
                    // The server's own close (EOF) is confirmed: it
                    // deregistered the puppet before it hung up, so the
                    // departure was processed there first (what that word is
                    // worth: `PuppetEvent::Ended`). Any other reason is this
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

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// One puppet task spawned by hand — no executor: the test is the
    /// executor, holding the command sender, the stop and the event queue.
    struct Puppet {
        cmd: mpsc::Sender<PuppetCommand>,
        stop: Arc<Notify>,
        ev_rx: mpsc::Receiver<PuppetEvent>,
        /// A second sender on the event queue, so a test can fill it.
        ev_tx: mpsc::Sender<PuppetEvent>,
        hand_rx: mpsc::UnboundedReceiver<(String, tokio::io::DuplexStream)>,
        unqueued: Arc<AtomicU64>,
        grace: Duration,
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
        let grace = Duration::from_secs(irc.puppets.quit_grace_secs);
        let task = tokio::spawn(puppet_task(Spawn {
            peer: peer(),
            attempt: ATTEMPT,
            irc,
            connector: connector(hand_tx),
            events: ev_tx.clone(),
            cmd_rx,
            stop: stop.clone(),
            registration_timeout: Duration::from_secs(5),
            lines_unqueued: unqueued.clone(),
        }));
        Puppet {
            cmd: cmd_tx,
            stop,
            ev_rx,
            ev_tx,
            hand_rx,
            unqueued,
            grace,
            task,
        }
    }

    impl Puppet {
        /// What the executor does for a Quit: the command for the record,
        /// with the configured grace, and the stop. The task takes whichever
        /// it reads first; both leave the same way.
        fn quit(&mut self) {
            let _ = self.cmd.try_send(PuppetCommand::Quit {
                reason: "cc-abc: agent left the mesh".into(),
                grace: self.grace,
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
        // The QUIT: by the command's reason or the stop's (which names the
        // nick the task holds, respelled) — whichever the task read first.
        assert!(read_until(&mut r, "QUIT").await.starts_with("QUIT :"));
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
        // …until told. A stop: it leaves with an Ended, like every path
        // before registration.
        p.stop.notify_one();
        drop(p.cmd);
        let ev = next(&mut p.ev_rx).await;
        assert!(
            matches!(&ev, PuppetEvent::Ended { why, nick: None, .. } if why == "quit before registration"),
            "{ev:?}"
        );
        tokio::time::timeout(Duration::from_secs(5), p.task)
            .await
            .expect("the task ends")
            .unwrap();
        // A cancel (the sender dropped, no stop) ends it silently.
        let mut p = spawn(base(), scripted_connector, 16);
        let (_nick, server) = p.hand_rx.recv().await.unwrap();
        let (rh, mut wh) = tokio::io::split(server);
        let mut r = BufReader::new(rh);
        read_until(&mut r, "NICK ").await;
        wh.write_all(b":srv CAP * LS :\r\n").await.unwrap();
        wh.write_all(b":srv 433 * cc-abc :Nickname is already in use\r\n")
            .await
            .unwrap();
        let _ = next(&mut p.ev_rx).await;
        drop(p.cmd);
        tokio::time::timeout(Duration::from_secs(5), p.task)
            .await
            .expect("the task ends when cancelled")
            .unwrap();
        assert!(p.ev_rx.try_recv().is_err(), "a cancel reports nothing");
    }

    #[tokio::test]
    async fn a_command_that_is_not_a_quit_does_not_end_the_hold_after_a_rejection() {
        // A stray Join reaches a puppet whose nick was rejected: nothing for
        // a connection that never registered, so the hold goes on, and the
        // Quit that follows is still told back with an Ended.
        let mut p = spawn(base(), scripted_connector, 16);
        let (_nick, server) = p.hand_rx.recv().await.unwrap();
        let (rh, mut wh) = tokio::io::split(server);
        let mut r = BufReader::new(rh);
        read_until(&mut r, "NICK ").await;
        wh.write_all(b":srv CAP * LS :\r\n").await.unwrap();
        wh.write_all(b":srv 433 * cc-abc :Nickname is already in use\r\n")
            .await
            .unwrap();
        let _ = next(&mut p.ev_rx).await;
        p.cmd
            .try_send(PuppetCommand::Join(vec!["#mu".into()]))
            .unwrap();
        tokio::task::yield_now().await;
        p.quit();
        let ev = tokio::time::timeout(Duration::from_secs(2), next(&mut p.ev_rx))
            .await
            .expect("the Quit is answered with an Ended");
        assert!(
            matches!(&ev, PuppetEvent::Ended { why, nick: None, .. } if why == "quit before registration"),
            "{ev:?}"
        );
    }

    #[tokio::test]
    async fn a_mirrored_line_the_outbound_queue_refuses_is_counted() {
        // The server stops reading; the outbound queue (4 deep) fills; the
        // mirrored lines past it are dropped by design — and counted, so
        // the loss is never invisible.
        let mut p = spawn(base(), choked_connector, 16);
        let (_r, _wh) = p.registered().await;
        for i in 0..12 {
            p.cmd
                .try_send(PuppetCommand::Send(format!("PRIVMSG #mu :{i:0>40}")))
                .unwrap();
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
        }
        assert!(
            p.unqueued.load(Ordering::Relaxed) > 0,
            "lines the queue refused were dropped uncounted"
        );
        assert!(
            p.ev_rx.try_recv().is_err(),
            "nothing was handed up for them"
        );
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
        // confirmed — the server deregistered the puppet before it hung up.
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
    async fn the_casemapping_a_server_advertises_after_001_decides_whose_nick_a_prefix_is() {
        // ISUPPORT comes after 001: the server registers us, then says
        // CASEMAPPING=ascii. Under ascii, `cc-abc{` and `cc-abc[` are two
        // clients; under the rfc1459 default they are one. A NICK from the
        // other one must not be taken for our own rename — and without the
        // 005 being tracked after readiness, it would be.
        let mut p = spawn(base(), scripted_connector, 16);
        let (_r, mut wh) = p.registered().await;
        wh.write_all(b":srv 005 cc-abc CASEMAPPING=ascii :are supported\r\n")
            .await
            .unwrap();
        // The server renames us onto a bracket spelling (our own prefix).
        wh.write_all(b":cc-abc!u@h NICK :cc-abc[\r\n")
            .await
            .unwrap();
        let ev = loop {
            let ev = next(&mut p.ev_rx).await;
            if matches!(ev, PuppetEvent::Renamed { .. }) {
                break ev;
            }
        };
        assert!(
            matches!(&ev, PuppetEvent::Renamed { to, .. } if to == "cc-abc["),
            "{ev:?}"
        );
        // Another client, the brace spelling, renames: under ascii not us.
        wh.write_all(b":cc-abc{!u@h NICK :someone\r\n")
            .await
            .unwrap();
        let ev = next(&mut p.ev_rx).await;
        assert!(
            matches!(&ev, PuppetEvent::Line { line, .. } if line.contains("NICK :someone")),
            "taken for our own rename under the wrong casemapping: {ev:?}"
        );
        // Without the 005 (the rfc1459 default), the same prefix IS us.
        let mut p = spawn(base(), scripted_connector, 16);
        let (_r, mut wh) = p.registered().await;
        wh.write_all(b":cc-abc!u@h NICK :cc-abc[\r\n")
            .await
            .unwrap();
        let ev = loop {
            let ev = next(&mut p.ev_rx).await;
            if matches!(ev, PuppetEvent::Renamed { .. }) {
                break ev;
            }
        };
        assert!(matches!(&ev, PuppetEvent::Renamed { .. }), "{ev:?}");
        wh.write_all(b":cc-abc{!u@h NICK :someone\r\n")
            .await
            .unwrap();
        let ev = next(&mut p.ev_rx).await;
        assert!(
            matches!(&ev, PuppetEvent::Renamed { from, to, .. } if from == "cc-abc[" && to == "someone"),
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

    #[tokio::test]
    async fn a_ping_under_a_flood_of_mirrored_lines_is_still_answered() {
        // The session floods the puppet with Sends faster than the server
        // reads; meanwhile the server PINGs. The PONG must still go out —
        // held when the outbound queue is full, ahead of anything mirrored,
        // never dropped with the overflow — and the inbound side must not
        // be starved by the command side.
        let mut p = spawn(base(), scripted_connector, 64);
        let (mut r, mut wh) = p.registered().await;
        let cmd = p.cmd.clone();
        let flood = tokio::spawn(async move {
            let mut n = 0u32;
            while cmd
                .send(PuppetCommand::Send(format!("PRIVMSG #mu :{n}")))
                .await
                .is_ok()
            {
                n += 1;
                if n > 20_000 {
                    break;
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        wh.write_all(
            b"PING :tok
",
        )
        .await
        .unwrap();
        let pinged = Instant::now();
        // The server reads (it has to, or the writer just fills) until the
        // PONG shows up among the flood.
        let pong = tokio::time::timeout(Duration::from_secs(3), read_until(&mut r, "PONG")).await;
        assert!(
            pong.is_ok(),
            "the PONG never came: the flood starved the inbound side"
        );
        assert!(
            pinged.elapsed() < Duration::from_secs(2),
            "{:?}",
            pinged.elapsed()
        );
        drop(p.cmd);
        flood.abort();
        let _ = flood.await;
    }

    #[tokio::test]
    async fn a_ping_before_registration_is_answered_once_the_queue_moves() {
        // A server that PINGs during registration while not reading: the
        // registration burst fills the socket, the PONGs fill the queue, and
        // the ones the queue refuses are held — the latest — rather than
        // dropped, and go out once the server reads. A server that never
        // hears the answer drops the puppet before it is registered.
        let mut p = spawn(base(), choked_connector, 16);
        let (_nick, server) = p.hand_rx.recv().await.unwrap();
        let (rh, mut wh) = tokio::io::split(server);
        let mut r = BufReader::new(rh);
        for i in 1..=12 {
            wh.write_all(format!("PING :t{i}\r\n").as_bytes())
                .await
                .unwrap();
        }
        // Now the server reads: the burst, the PONGs the queue took, and —
        // on the retry — the held answer to the last PING.
        let answered =
            tokio::time::timeout(Duration::from_secs(3), read_until(&mut r, "PONG :t12")).await;
        assert!(
            answered.is_ok(),
            "the last PING's answer was dropped with the overflow"
        );
        wh.write_all(b":srv CAP * LS :\r\n").await.unwrap();
        wh.write_all(b":srv 001 cc-abc :Welcome\r\n").await.unwrap();
        let ev = next(&mut p.ev_rx).await;
        assert!(matches!(&ev, PuppetEvent::Registered { .. }), "{ev:?}");
    }

    #[tokio::test]
    async fn a_nick_rejection_report_blocked_on_a_full_queue_yields_to_the_stop() {
        // The event queue is full and nobody drains it; the server rejects
        // the nick; the report blocks. A stop must still end the task —
        // socket let go of first — with the report and an Ended following
        // once the queue moves.
        let mut p = spawn(base(), scripted_connector, 1);
        let (_nick, server) = p.hand_rx.recv().await.unwrap();
        let (rh, mut wh) = tokio::io::split(server);
        let mut r = BufReader::new(rh);
        read_until(&mut r, "NICK ").await;
        p.ev_tx
            .try_send(PuppetEvent::Line {
                peer: peer(),
                attempt: 99,
                line: "fill".into(),
            })
            .unwrap();
        wh.write_all(b":srv CAP * LS :\r\n").await.unwrap();
        wh.write_all(b":srv 433 * cc-abc :Nickname is already in use\r\n")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        // The task is parked on the report. Tell it to go.
        p.stop.notify_one();
        drop(p.cmd);
        // The socket is let go of: the server's writes fail.
        let cut = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if wh.write_all(b":srv NOTICE * :x\r\n").await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(cut.is_ok(), "the stopped task kept the socket");
        drop(r);
        // Draining: the fill, the rejection, then the Ended.
        let ev = next(&mut p.ev_rx).await;
        assert!(matches!(ev, PuppetEvent::Line { .. }), "{ev:?}");
        let ev = next(&mut p.ev_rx).await;
        assert!(matches!(ev, PuppetEvent::NickRejected { .. }), "{ev:?}");
        let ev = next(&mut p.ev_rx).await;
        assert!(
            matches!(&ev, PuppetEvent::Ended { why, nick: None, .. } if why == "quit before registration"),
            "{ev:?}"
        );
        tokio::time::timeout(Duration::from_secs(5), p.task)
            .await
            .expect("the task ends")
            .unwrap();
    }

    #[tokio::test]
    async fn a_stop_during_a_stalled_dial_ends_the_attempt_at_once() {
        // The connector never resolves (a dial that hangs). A stop must not
        // wait for it: the attempt ends now, with its Ended.
        // The connector holds the token WEAKLY; only the dial in flight
        // holds it strongly, so the count says whether the dial is alive.
        let token = Arc::new(());
        let watched = Arc::downgrade(&token);
        let (ev_tx, mut ev_rx) = mpsc::channel(16);
        let (cmd_tx, cmd_rx) = mpsc::channel(4);
        let stop = Arc::new(Notify::new());
        let mut irc = base();
        irc.nick = "cc-abc".into();
        let connector: Connector = Arc::new(move |_irc: IrcConfig| {
            let held = watched.upgrade();
            Box::pin(async move {
                let _held = held;
                std::future::pending::<Result<Connection, String>>().await
            })
        });
        let task = tokio::spawn(puppet_task(Spawn {
            peer: peer(),
            attempt: ATTEMPT,
            irc,
            connector,
            events: ev_tx,
            cmd_rx,
            stop: stop.clone(),
            registration_timeout: Duration::from_secs(5),
            lines_unqueued: Arc::new(AtomicU64::new(0)),
        }));
        tokio::task::yield_now().await;
        assert!(Arc::strong_count(&token) >= 2, "the dial is in flight");
        let started = Instant::now();
        stop.notify_one();
        drop(cmd_tx);
        let ev = next(&mut ev_rx).await;
        assert!(
            matches!(&ev, PuppetEvent::Ended { why, nick: None, .. } if why == "quit before registration"),
            "{ev:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        task.await.unwrap();
        assert_eq!(
            Arc::strong_count(&token),
            1,
            "the dial was dropped with the attempt"
        );
        // And it is dropped BEFORE the report: with the event queue full
        // and nobody draining, the stop still lets the dial go at once.
        let token = Arc::new(());
        let watched = Arc::downgrade(&token);
        let (ev_tx, ev_rx) = mpsc::channel(1);
        ev_tx
            .try_send(PuppetEvent::Line {
                peer: peer(),
                attempt: 99,
                line: "fill".into(),
            })
            .unwrap();
        let (cmd_tx, cmd_rx) = mpsc::channel(4);
        let stop = Arc::new(Notify::new());
        let mut irc = base();
        irc.nick = "cc-abc".into();
        let connector: Connector = Arc::new(move |_irc: IrcConfig| {
            let held = watched.upgrade();
            Box::pin(async move {
                let _held = held;
                std::future::pending::<Result<Connection, String>>().await
            })
        });
        let _task = tokio::spawn(puppet_task(Spawn {
            peer: peer(),
            attempt: ATTEMPT,
            irc,
            connector,
            events: ev_tx,
            cmd_rx,
            stop: stop.clone(),
            registration_timeout: Duration::from_secs(5),
            lines_unqueued: Arc::new(AtomicU64::new(0)),
        }));
        tokio::task::yield_now().await;
        assert!(Arc::strong_count(&token) >= 2);
        stop.notify_one();
        drop(cmd_tx);
        for _ in 0..50 {
            if Arc::strong_count(&token) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            Arc::strong_count(&token),
            1,
            "the dial outlived the stop while the report waited on a full queue"
        );
        drop(ev_rx);
    }

    #[tokio::test]
    async fn only_the_latest_pong_is_held_under_overflow() {
        // The server keeps pinging with fresh tokens while not reading. The
        // held control lines stay bounded: one PONG, the latest.
        let mut p = spawn(base(), choked_connector, 64);
        let (mut r, mut wh) = p.registered().await;
        for i in 0..12 {
            p.cmd
                .try_send(PuppetCommand::Send(format!("PRIVMSG #mu :{i:0>40}")))
                .unwrap();
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
        }
        for i in 0..50 {
            wh.write_all(
                format!(
                    "PING :tok{i}
"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        // The server reads again: PONGs come out — but not fifty of them.
        // Between the ones written straight through and the one held, the
        // last token is answered and the count is small.
        let mut pongs = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let mut line = String::new();
            match tokio::time::timeout(Duration::from_millis(300), r.read_line(&mut line)).await {
                Ok(Ok(n)) if n > 0 => {
                    if line.starts_with("PONG") {
                        pongs.push(line.trim().to_string());
                    }
                }
                _ => break,
            }
        }
        assert!(
            pongs.contains(&"PONG :tok49".to_string()),
            "the latest PING is answered: {pongs:?}"
        );
        assert!(
            pongs.len() < 50,
            "every token was answered: {}",
            pongs.len()
        );
    }

    #[tokio::test]
    async fn a_rename_during_a_congested_quit_write_is_still_reported() {
        // The server has stopped reading, so the QUIT never lands within the
        // grace — but it keeps writing, and renames the puppet meanwhile.
        // The rename is read while the write is stalled, reported ahead of
        // the (unconfirmed) Ended, and the Ended names the new nick.
        let mut cfg = base();
        cfg.puppets.quit_grace_secs = 1;
        let mut p = spawn(cfg, choked_connector, 64);
        let (r, mut wh) = p.registered().await;
        for i in 0..12 {
            p.cmd
                .try_send(PuppetCommand::Send(format!("PRIVMSG #mu :{i:0>40}")))
                .unwrap();
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
        }
        p.quit();
        tokio::time::sleep(Duration::from_millis(100)).await;
        wh.write_all(b":cc-abc!u@h NICK :Guest7\r\n").await.unwrap();
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
            Some("Guest7"),
            "the rename during the stalled write was lost"
        );
        assert!(
            matches!(&ev, PuppetEvent::Ended { nick, confirmed: false, .. } if nick.as_deref() == Some("Guest7")),
            "{ev:?}"
        );
        drop(r);
    }
}
