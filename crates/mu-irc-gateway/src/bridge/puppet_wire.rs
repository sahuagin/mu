//! What a puppet speaks on the wire, apart from its lifecycle: the answer
//! to a PING, the recognition of its own NICK, the control lines it holds
//! back when the outbound queue is full, and the QUIT it leaves by and how
//! its departure is confirmed. The puppet task — the increment above this
//! one — is the lifecycle that calls these; nothing here knows a peer, an
//! attempt, or the session — only a connection's writer, its inbound
//! stream, and the nick the server last called us. The main connection
//! shares what it can ([`pong`]).
//!
//! **Control lines under backpressure.** A puppet's outbound queue is the
//! session's to fill (mirrored lines), and a full queue refuses the next
//! line (`Overflow`). Two lines must not be lost to that: a JOIN, which is
//! membership — dropped, the puppet would be registered but absent from its
//! channels for good, while the executor already answered "queued" — and a
//! PONG, which is the connection itself: a server whose PING goes unanswered
//! drops the puppet, and a flood of mirrored lines must not cost it that.
//! Both are held, in order, and retried until they go; a dead connection
//! takes them with it. Bounded on this module's side: at most ONE PONG — the
//! latest PING's — is ever held, whatever tokens the server sends
//! ([`hold_pong`]); what the caller holds besides (the task holds a JOIN at
//! most once per channel) is the caller's rule. [`send_pending`] writes what
//! is held, in order, as far as the queue will take it.
//!
//! **Leaving.** [`quit_connection`] writes the QUIT on a line boundary and
//! reads the connection until the server closes it, within a grace. Its
//! `confirmed` is exactly that and no more: the connection ended with the
//! PEER's close ([`transport::SERVER_CLOSED`]) — not this side's (the
//! forced deadline, a failed write, a read error), which confirms nothing.
//! It is the best word there is that the server is done with the puppet
//! — an IRC server drops a client after deregistering it, whatever brought
//! that about — and it is not an acknowledgement: a timeout, a kill, a
//! crash or a link cut between the two produce the same EOF and say
//! nothing about what was processed. The session orders its departure
//! barrier behind a confirmed end on that basis and carries the residual
//! (a puppet the server still holds under the name, until its own timeout
//! of it); whether our QUIT was the reason is not asked.

use std::time::Duration;

use mu_peer::PeerId;
use tokio::sync::{mpsc, Notify};
use tracing::{debug, warn};

use crate::adapter::{IrcMessage, Transport};
use crate::mapping::{fold_nick, CaseMapping};
use crate::transport::{self, FromServer};

/// Answer a PING at once when the queue has room; hold the answer otherwise,
/// ahead of anything mirrored. Only the LATEST PING's answer is worth
/// holding: a server that keeps pinging while not reading wants to hear from
/// us once the queue moves, not to be answered for every token — and so the
/// held set stays bounded whatever the tokens do. An answer held earlier is
/// superseded either way: by the one held now, or by the one that went out
/// (a stale PONG sent after a fresh one is traffic the server never asked
/// for).
pub fn hold_pong(writer: &mut transport::LineWriter, pending: &mut Vec<String>, line: String) {
    pending.retain(|l| !l.starts_with("PONG"));
    match writer.send_line(&line) {
        Ok(()) => {}
        Err(transport::SendError::Overflow) => {
            pending.push(line);
        }
        Err(transport::SendError::Disconnected) => {
            // Nothing to answer with: an Ended is on its way.
            debug!("puppet: PONG dropped (connection gone)");
        }
    }
}

/// The `Ended` reason for a registration line the queue would not take: a
/// full queue names itself, so an operator who set `command_queue` below
/// the registration burst reads the cause, not "write failed".
pub fn registration_write_failed(e: transport::SendError) -> String {
    match e {
        transport::SendError::Overflow => {
            "outbound queue full during registration (command_queue too small)".to_string()
        }
        transport::SendError::Disconnected => "write failed during registration".to_string(),
    }
}

/// Write every held control line the outbound queue will take, in order; the
/// ones it refuses (`Overflow`) stay held, in order, for the next try. On a
/// connection that is gone the rest are dropped: an `Ended` is on its way.
pub fn send_pending(
    writer: &mut transport::LineWriter,
    pending: &mut Vec<String>,
    peer: &PeerId,
    nick: &str,
) {
    while !pending.is_empty() {
        match writer.send_line(&pending[0]) {
            Ok(()) => {
                pending.remove(0);
            }
            Err(transport::SendError::Overflow) => {
                debug!(peer = %peer, nick = %nick, held = pending.len(), "puppet: control line held (outbound queue full)");
                return;
            }
            Err(transport::SendError::Disconnected) => {
                warn!(peer = %peer, nick = %nick, dropped = pending.len(), "puppet: held control lines dropped (connection gone)");
                pending.clear();
                return;
            }
        }
    }
}

/// Leave by QUIT, and see it through: the line is written on a line
/// boundary (the transport's graceful stop lets a frame in flight finish
/// first), then the connection is read until the SERVER closes it. That
/// close is `confirmed`: the peer ended the connection — for an IRC
/// server, after deregistering the puppet, whatever brought that about —
/// and the session treats the `Ended` that follows as the best word
/// available that the server has seen this puppet leave, ordering its
/// departure barrier on the main connection behind it. It is not an
/// acknowledgement ([`transport::SERVER_CLOSED`] says what the same EOF
/// can also mean), and whether our QUIT was the reason is not asked — it
/// could not be answered: the transport's completion says the stop
/// finished, not that the QUIT landed, and a server that closes on its
/// own before our QUIT is written has dropped the puppet all the same. One
/// deadline covers both
/// halves: at `grace` the writer is forced and the socket cut whatever the
/// server has done (a peer that never answers cannot hold the task), and
/// that — like a failed write or a read error, any close by this side —
/// is `false`: the server has confirmed nothing, the `Ended` says so
/// (`why: "quit (forced)"`), and the session does not order on it.
///
/// The server may still rename this puppet while its QUIT is in flight: a
/// `NICK` for us among the lines drained here moves `nick` and is returned,
/// so the caller reports it before the `Ended` and the `Ended` names the
/// nick the server last knew.
pub async fn quit_connection(
    writer: &transport::LineWriter,
    inbound: &mut mpsc::Receiver<FromServer>,
    nick: &mut String,
    cm: CaseMapping,
    reason: String,
    grace: Duration,
) -> Quit {
    let mut renames = Vec::new();
    writer.begin_graceful_stop(format!("QUIT :{reason}"));
    let deadline = tokio::time::sleep(grace);
    tokio::pin!(deadline);
    // One loop for both halves — the QUIT going out, then the server's
    // side of it — reading the connection THROUGHOUT: a NICK the server
    // sends while our write is still congested is as much a fact about the
    // nick the Ended will name as one sent after. Lines are otherwise
    // drained and dropped: nothing after a QUIT is routed. The server
    // answers a QUIT with an ERROR and closes, which reaches us as the
    // reader ending with the server's reason; a `Closed` for any other
    // reason is this side's. The writer's completion is not consulted: it
    // fires when the stop has finished, written or not, and the reason
    // the connection ended already says whose end it was.
    let confirmed = loop {
        tokio::select! {
            biased;
            _ = &mut deadline => { writer.force_stop(); break false; }
            event = inbound.recv() => match event {
                Some(FromServer::Line(line)) => {
                    let msg = IrcMessage::parse(&line);
                    if let Some(to) = own_rename(&msg, nick, cm) {
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

/// What a QUIT came to: whether the SERVER closed the connection (its
/// departure processed there, whatever the reason), and the renames the
/// server made in the meantime, in order.
pub struct Quit {
    pub confirmed: bool,
    pub renames: Vec<(String, String)>,
}

/// Whether a stop has been signalled and not yet taken: the executor signals
/// the stop and then drops the command sender, so a task that reads the
/// queue's `None` asks this before treating it as a cancel — a stop that lost
/// the race for the select is still a stop. Consumes the permit.
pub async fn stop_pending(stop: &Notify) -> bool {
    tokio::time::timeout(Duration::ZERO, stop.notified())
        .await
        .is_ok()
}

/// The `Ended` reason after a QUIT: whether the server closed the
/// connection, or the grace cut it.
pub fn quit_why(confirmed: bool) -> String {
    if confirmed { "quit" } else { "quit (forced)" }.to_string()
}

/// If `msg` is a `NICK` whose source is `nick` — the server renaming THIS
/// connection — the new nick. The source is compared as the server spelled
/// it at `001` (or at the last rename), folded under the server's own
/// `CASEMAPPING` (this connection's ISUPPORT) as the hedge for a server that
/// respells the prefix: case, and under rfc1459 the bracket, brace, pipe
/// and caret equivalences too.
pub fn own_rename(msg: &IrcMessage, nick: &str, cm: CaseMapping) -> Option<String> {
    if msg.command != "NICK" {
        return None;
    }
    let source = msg.prefix.as_deref()?;
    let source = source.split('!').next().unwrap_or(source);
    if source != nick && fold_nick(source, cm) != fold_nick(nick, cm) {
        return None;
    }
    msg.params.first().filter(|n| !n.is_empty()).cloned()
}

/// The PONG for a PING, echoing its token.
pub fn pong(msg: &IrcMessage) -> String {
    match msg.params.last() {
        Some(token) => format!("PONG :{token}"),
        None => "PONG".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_control_line_the_queue_refuses_is_held_in_order_and_a_pong_replaces_the_last() {
        // A one-deep scripted queue: the first line goes, the second is
        // refused. JOINs are held in order; a PONG replaces the PONG held
        // before it — the latest PING's answer is the one worth having.
        let (mut writer, mut lines) = transport::LineWriter::scripted(1);
        let mut pending = Vec::new();
        hold_pong(
            &mut writer,
            &mut pending,
            pong(&IrcMessage::parse("PING :a")),
        );
        assert_eq!(lines.try_recv().ok().as_deref(), Some("PONG :a"));
        assert!(pending.is_empty(), "room in the queue: answered at once");
        assert!(
            writer.send_line("JOIN #fill").is_ok(),
            "the queue's one slot"
        );
        pending.push("JOIN #a".to_string());
        hold_pong(
            &mut writer,
            &mut pending,
            pong(&IrcMessage::parse("PING :b")),
        );
        hold_pong(
            &mut writer,
            &mut pending,
            pong(&IrcMessage::parse("PING :c")),
        );
        assert_eq!(pending, vec!["JOIN #a".to_string(), "PONG :c".to_string()]);
        // The queue moves and a fresh PING is answered at once: the held
        // answer is stale now and is dropped, not sent after it.
        let peer = PeerId::parse("cc:abc");
        assert_eq!(lines.try_recv().ok().as_deref(), Some("JOIN #fill"));
        hold_pong(
            &mut writer,
            &mut pending,
            pong(&IrcMessage::parse("PING :d")),
        );
        assert_eq!(lines.try_recv().ok().as_deref(), Some("PONG :d"));
        assert_eq!(
            pending,
            vec!["JOIN #a".to_string()],
            "the stale PONG went with it"
        );
        // Held again, then the queue moves: the held lines go, in order,
        // until it refuses again.
        assert!(writer.send_line("JOIN #fill2").is_ok());
        hold_pong(
            &mut writer,
            &mut pending,
            pong(&IrcMessage::parse("PING :e")),
        );
        assert_eq!(pending, vec!["JOIN #a".to_string(), "PONG :e".to_string()]);
        assert_eq!(lines.try_recv().ok().as_deref(), Some("JOIN #fill2"));
        send_pending(&mut writer, &mut pending, &peer, "cc-abc");
        assert_eq!(lines.try_recv().ok().as_deref(), Some("JOIN #a"));
        assert_eq!(
            pending,
            vec!["PONG :e".to_string()],
            "refused again: still held"
        );
        send_pending(&mut writer, &mut pending, &peer, "cc-abc");
        assert_eq!(lines.try_recv().ok().as_deref(), Some("PONG :e"));
        assert!(pending.is_empty());
        // A connection that is gone drops what it holds: an Ended is coming.
        pending.push("JOIN #b".to_string());
        drop(lines);
        send_pending(&mut writer, &mut pending, &peer, "cc-abc");
        assert!(pending.is_empty());
        assert_eq!(
            registration_write_failed(transport::SendError::Overflow),
            "outbound queue full during registration (command_queue too small)"
        );
        assert_eq!(quit_why(true), "quit");
        assert_eq!(quit_why(false), "quit (forced)");
    }

    #[tokio::test]
    async fn the_servers_close_confirms_and_this_sides_does_not_whatever_the_writer_says() {
        // Scripted at the transport's stop seam, so the writer's completion
        // is the test's to give or withhold. The server's close confirms at
        // once, completion or not — a server that closed is done with the
        // puppet, whether our QUIT reached it or it hung up first; a close
        // by this side confirms nothing, completion or not.
        let cm = CaseMapping::default();
        let grace = Duration::from_secs(5);
        // Server EOF before the writer ever completes (it hung up on us, or
        // the reader outran the writer's shutdown): confirmed.
        let (writer, _lines, done) = transport::LineWriter::scripted_with_stop(4);
        let (tx, mut inbound) = mpsc::channel(4);
        let mut nick = "cc-abc".to_string();
        tx.send(FromServer::Closed(transport::SERVER_CLOSED.to_string()))
            .await
            .unwrap();
        let quit = tokio::time::timeout(
            Duration::from_secs(1),
            quit_connection(&writer, &mut inbound, &mut nick, cm, "bye".into(), grace),
        )
        .await
        .expect("settled on the server's close alone");
        assert!(quit.confirmed);
        drop(done);
        // The same with the completion gone before the close: still the
        // server's word.
        let (writer, _lines, done) = transport::LineWriter::scripted_with_stop(4);
        let (tx, mut inbound) = mpsc::channel(4);
        drop(done);
        tx.send(FromServer::Closed(transport::SERVER_CLOSED.to_string()))
            .await
            .unwrap();
        let quit = quit_connection(&writer, &mut inbound, &mut nick, cm, "bye".into(), grace).await;
        assert!(quit.confirmed);
        // This side's close — a failed QUIT write, a forced stop — confirms
        // nothing, even with the writer's completion fired.
        for reason in ["graceful stop, QUIT write failed", "forced stop"] {
            let (writer, _lines, done) = transport::LineWriter::scripted_with_stop(4);
            let (tx, mut inbound) = mpsc::channel(4);
            done.send(true).unwrap();
            tx.send(FromServer::Closed(reason.to_string()))
                .await
                .unwrap();
            let quit =
                quit_connection(&writer, &mut inbound, &mut nick, cm, "bye".into(), grace).await;
            assert!(!quit.confirmed, "{reason}");
        }
        // A rename the server makes in the meantime is returned, in order,
        // and the Ended will name the last one.
        let (writer, _lines, _done) = transport::LineWriter::scripted_with_stop(4);
        let (tx, mut inbound) = mpsc::channel(4);
        tx.send(FromServer::Line(":cc-abc!u@h NICK :cc-b".into()))
            .await
            .unwrap();
        tx.send(FromServer::Line(":alice!u@h NICK :bob".into()))
            .await
            .unwrap();
        tx.send(FromServer::Closed(transport::SERVER_CLOSED.to_string()))
            .await
            .unwrap();
        let quit = quit_connection(&writer, &mut inbound, &mut nick, cm, "bye".into(), grace).await;
        assert!(quit.confirmed);
        assert_eq!(
            quit.renames,
            vec![("cc-abc".to_string(), "cc-b".to_string())]
        );
        assert_eq!(nick, "cc-b");
        // The grace runs out on a server that never closes: forced, and not
        // confirmed.
        let (writer, _lines, _done) = transport::LineWriter::scripted_with_stop(4);
        let (_tx, mut inbound) = mpsc::channel::<FromServer>(4);
        let quit = quit_connection(
            &writer,
            &mut inbound,
            &mut nick,
            cm,
            "bye".into(),
            Duration::from_millis(20),
        )
        .await;
        assert!(!quit.confirmed);
    }

    #[tokio::test]
    async fn a_pending_stop_is_taken_once_and_only_a_stored_permit_counts() {
        // The executor signals with `notify_one`, which stores a permit when
        // nobody is waiting; `stop_pending` consumes it — once. A signal that
        // stores nothing (`notify_waiters`) is not a pending stop.
        let stop = Notify::new();
        assert!(!stop_pending(&stop).await);
        stop.notify_one();
        assert!(stop_pending(&stop).await);
        assert!(!stop_pending(&stop).await, "the permit is consumed");
        stop.notify_waiters();
        assert!(!stop_pending(&stop).await);
    }

    #[test]
    fn own_rename_matches_the_source_and_pong_echoes_the_token() {
        let rfc = CaseMapping::Rfc1459;
        let ascii = CaseMapping::Ascii;
        let m = IrcMessage::parse(":cc-abc!u@h NICK :Guest1");
        assert_eq!(own_rename(&m, "cc-abc", rfc).as_deref(), Some("Guest1"));
        assert_eq!(
            own_rename(&m, "CC-ABC", ascii).as_deref(),
            Some("Guest1"),
            "case-folded hedge"
        );
        assert_eq!(own_rename(&m, "alice", rfc), None);
        assert_eq!(
            own_rename(&IrcMessage::parse("NICK :x"), "cc-abc", rfc),
            None,
            "no source"
        );
        assert_eq!(
            own_rename(&IrcMessage::parse(":cc-abc!u@h JOIN #mu"), "cc-abc", rfc),
            None
        );
        // The server's own CASEMAPPING decides what a respelled prefix is:
        // under rfc1459 `Guest{1` and `Guest[1` are one client, under ascii
        // they are two.
        let m = IrcMessage::parse(":Guest{1!u@h NICK :Guest2");
        assert_eq!(
            own_rename(&m, "Guest[1", rfc).as_deref(),
            Some("Guest2"),
            "rfc1459 equates the brace and the bracket"
        );
        assert_eq!(own_rename(&m, "Guest[1", ascii), None, "ascii does not");
        assert_eq!(pong(&IrcMessage::parse("PING :tok")), "PONG :tok");
        assert_eq!(pong(&IrcMessage::parse("PING")), "PONG");
    }
}
