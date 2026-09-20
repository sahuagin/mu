//! What a puppet speaks on the wire, apart from its lifecycle: the answer
//! to a PING, the recognition of its own NICK, the control lines it holds
//! back when the outbound queue is full, and the QUIT it leaves by and how
//! that QUIT is confirmed. [`puppet_task`](super::puppet_task) is the
//! lifecycle that calls these; nothing here knows a peer, an attempt, or
//! the session — only a connection's writer, its inbound stream, and the
//! nick the server last called us.
//!
//! **Control lines under backpressure.** A puppet's outbound queue is the
//! session's to fill (mirrored lines), and a full queue refuses the next
//! line (`Overflow`). Two lines must not be lost to that: a JOIN, which is
//! membership — dropped, the puppet would be registered but absent from its
//! channels for good, while the executor already answered "queued" — and a
//! PONG, which is the connection itself: a server whose PING goes unanswered
//! drops the puppet, and a flood of mirrored lines must not cost it that.
//! Both are held, in order, and retried until they go; a dead connection
//! takes them with it. Bounded: at most one JOIN per channel is ever held,
//! and at most ONE PONG — the latest PING's — whatever tokens the server
//! sends ([`hold_pong`], [`send_pending`]).
//!
//! **Leaving.** [`quit_connection`] writes the QUIT on a line boundary and
//! reads the connection until the server closes it, within a grace; whether
//! the server's close was seen is what its `confirmed` says, and the
//! session orders a departure barrier only behind a confirmed one (see
//! [`transport::SERVER_CLOSED`] for what a server's close does and does not
//! say).

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
/// held set stays bounded whatever the tokens do.
pub fn hold_pong(writer: &mut transport::LineWriter, pending: &mut Vec<String>, line: String) {
    if matches!(writer.send_line(&line), Err(transport::SendError::Overflow)) {
        pending.retain(|l| !l.starts_with("PONG"));
        pending.push(line);
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
    let done = writer.stop_completion();
    let deadline = tokio::time::sleep(grace);
    tokio::pin!(deadline);
    // One loop for both halves — the QUIT going out, then the server's
    // side of it — reading the connection THROUGHOUT: a NICK the server
    // sends while our write is still congested is as much a fact about the
    // nick the Ended will name as one sent after. Lines are otherwise
    // drained and dropped: nothing after a QUIT is routed. The server
    // answers a QUIT with an ERROR and closes, which reaches us as the
    // reader ending, and only the SERVER's close confirms — a `Closed` for
    // any other reason (the forced stop's own, a QUIT write that failed
    // locally after completion fired, a read error) is this side's and
    // confirms nothing. The two halves are two tasks with no order between
    // them: the reader can deliver the server's close before the writer's
    // completion has propagated (the QUIT was written, the server closed
    // on it, and the writer is still in its shutdown). So the server's
    // close is held until the completion arrives — the writer ends on the
    // same lifecycle the close ended, so it is a tick behind at most, and
    // the deadline bounds it like everything else here — and a completion
    // that never comes (the channel closed with nothing written) is this
    // side's failure, not the server's word.
    let mut written = false;
    let mut server_closed = false;
    let mut done = done;
    let confirmed = loop {
        tokio::select! {
            biased;
            _ = &mut deadline => { writer.force_stop(); break false; }
            landed = async {
                match done.as_mut() {
                    Some(done) => {
                        while done.changed().await.is_ok() {
                            if *done.borrow() { return true; }
                        }
                        false
                    }
                    None => std::future::pending().await,
                }
            }, if !written => {
                written = landed;
                if server_closed {
                    break landed;
                }
                if !landed {
                    // The completion channel closed without a QUIT landing:
                    // the writer is gone. The close reason follows.
                    done = None;
                }
            }
            event = inbound.recv(), if !server_closed => match event {
                Some(FromServer::Line(line)) => {
                    let msg = IrcMessage::parse(&line);
                    if let Some(to) = own_rename(&msg, nick, cm) {
                        let from = std::mem::replace(nick, to.clone());
                        renames.push((from, to));
                    }
                }
                Some(FromServer::Closed(why)) => {
                    if why != transport::SERVER_CLOSED || written || done.is_none() {
                        break written && why == transport::SERVER_CLOSED;
                    }
                    server_closed = true;
                }
                None => break false,
            }
        }
    };
    Quit { confirmed, renames }
}

/// What a QUIT came to: whether the server confirmed it by closing, and the
/// renames the server made in the meantime, in order.
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
        // The queue moves: the held lines go, in order, until it refuses again.
        let peer = PeerId::parse("cc:abc");
        assert_eq!(lines.try_recv().ok().as_deref(), Some("JOIN #fill"));
        send_pending(&mut writer, &mut pending, &peer, "cc-abc");
        assert_eq!(lines.try_recv().ok().as_deref(), Some("JOIN #a"));
        assert_eq!(
            pending,
            vec!["PONG :c".to_string()],
            "refused again: still held"
        );
        send_pending(&mut writer, &mut pending, &peer, "cc-abc");
        assert_eq!(lines.try_recv().ok().as_deref(), Some("PONG :c"));
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
    async fn a_server_close_that_outruns_the_writers_completion_still_confirms() {
        // The reader and the writer are two tasks: the server can answer the
        // QUIT by closing, and the reader deliver that close, before the
        // writer's completion has propagated. That close is the server's
        // word all the same. Scripted at the seam, in that order, so the
        // ordering is the test and not the scheduler's mood.
        let (writer, _lines, done) = transport::LineWriter::scripted_with_stop(4);
        let (tx, mut inbound) = mpsc::channel(4);
        let mut nick = "cc-abc".to_string();
        let quit = quit_connection(
            &writer,
            &mut inbound,
            &mut nick,
            CaseMapping::default(),
            "bye".into(),
            Duration::from_secs(5),
        );
        tokio::pin!(quit);
        // The server's close arrives first, and is not yet an answer.
        tx.send(FromServer::Closed(transport::SERVER_CLOSED.to_string()))
            .await
            .expect("inbound open");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut quit)
                .await
                .is_err(),
            "held for the writer's completion"
        );
        // The completion propagates: confirmed.
        done.send(true).expect("completion watched");
        let quit = tokio::time::timeout(Duration::from_secs(1), &mut quit)
            .await
            .expect("settled once the completion arrived");
        assert!(quit.confirmed, "the server closed on a QUIT that landed");
        // A completion that never comes — the writer went away with nothing
        // written — is this side's failure, whatever the reader reported.
        let (writer, _lines, done) = transport::LineWriter::scripted_with_stop(4);
        let (tx, mut inbound) = mpsc::channel(4);
        let mut nick = "cc-abc".to_string();
        let quit = quit_connection(
            &writer,
            &mut inbound,
            &mut nick,
            CaseMapping::default(),
            "bye".into(),
            Duration::from_secs(5),
        );
        tokio::pin!(quit);
        tx.send(FromServer::Closed(transport::SERVER_CLOSED.to_string()))
            .await
            .expect("inbound open");
        drop(done);
        let quit = tokio::time::timeout(Duration::from_secs(1), &mut quit)
            .await
            .expect("settled once the completion closed");
        assert!(!quit.confirmed, "nothing landed, nothing confirmed");
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
