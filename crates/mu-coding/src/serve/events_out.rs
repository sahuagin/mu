//! mu-051 P4: every session out over NATS
//! (`specs/mu-051-client-api-over-nats.md` §5, §6, §9).
//!
//! Two streams per session, both core NATS, nothing stored on the server:
//!
//! - `mu.api.v1.<daemon>.<session>.evt.<kind>`: the session log itself.
//!   Every line appended to a session's log is published as written to its
//!   JSONL file, from inside the log's append lock ([`LogTap`], an
//!   [`AppendTap`]), so lines go out in id order. The log id is the only
//!   number: ids are contiguous per session, so a gap is a loss, and
//!   `session.events` (the session itself) returns the same records to
//!   fill it or to page back. `<kind>` is the payload's kind.
//! - `mu.api.v1.<daemon>.<session>.live.<kind>`: the notifications a stdio
//!   client gets (deltas, finalized text, tool calls, approval prompts),
//!   read from a tap lane on the outbound [`Router`], so delivery to the
//!   connection that created the session is unchanged. Unnumbered and
//!   lossy: it is for rendering a turn as it happens, and the log holds
//!   what lasts.
//!
//! The client reconnects on its own, including on the first connect; while
//! it is not connected, lines and notifications are dropped and counted,
//! not queued. A drop is logged with its own cause ([`Dropped`]). An
//! approval prompt is logged too (`input_required`, mu-051 P8a), so a
//! dropped live one is recovered like any other line.
//!
//! Gated by `[mesh] enabled = true` + `events = true`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_nats::connection::State;
use mu_core::command_journal::Origin;
use mu_core::config::MeshConfig;
use mu_core::event_log::{AppendTap, SessionEvent};
use mu_core::transport::{
    ConnectionLane, LaneTerminated, Router, WeakRouter, CONNECTION_LAGGED_METHOD,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const SESSION_PREFIX: &str = "session.";

/// Why [`spawn`] could not start a publisher; both are refusals to start.
#[derive(Debug)]
pub(crate) enum SpawnError {
    /// No client can be built from `[mesh].nats_url`.
    Client(async_nats::ConnectError),
    /// Building the client ran out `[mesh].events_call_timeout_ms`.
    TimedOut(Duration),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::Client(e) => write!(f, "no NATS client can be built: {e}"),
            SpawnError::TimedOut(t) => write!(
                f,
                "creating the NATS client took longer than {t:?} \
                 ([mesh].events_call_timeout_ms)"
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EventsConfig {
    pub nats_url: String,
    /// `[mesh].events_call_timeout_ms`.
    pub call_timeout: Duration,
    /// `[mesh].events_queue_len`.
    pub queue_len: usize,
}

/// `Ok(None)` when events out is off. A zero publish timeout or queue is a
/// misconfiguration: every line would drop, so the daemon refuses to start.
pub(crate) fn from_mesh(mesh: &MeshConfig) -> anyhow::Result<Option<EventsConfig>> {
    if !mesh.publishes_events() {
        return Ok(None);
    }
    if mesh.events_call_timeout_ms == 0 {
        anyhow::bail!("[mesh].events_call_timeout_ms must be at least 1 (default 1000)");
    }
    if mesh.events_queue_len == 0 {
        anyhow::bail!("[mesh].events_queue_len must be at least 1 (default 4096)");
    }
    Ok(Some(EventsConfig {
        nats_url: mesh.nats_url.clone(),
        call_timeout: Duration::from_millis(mesh.events_call_timeout_ms),
        queue_len: mesh.events_queue_len,
    }))
}

/// A subject token: NATS splits subjects on `.` and treats `*`, `>` and
/// whitespace specially. Daemon ids are hex and session ids `session-N`,
/// but a resumed or foreign id could carry anything; those characters
/// become `_` rather than publishing onto someone else's subject.
pub(crate) fn token(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == '.' || c == '*' || c == '>' || c.is_whitespace() || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect()
}

fn subject_base(daemon_id: &str, session: &str) -> String {
    format!("mu.api.v1.{}.{}", token(daemon_id), token(session))
}

/// One appended log line waiting to be published.
#[derive(Debug)]
struct LogLine {
    session: String,
    kind: &'static str,
    line: String,
}

/// The [`AppendTap`] the daemon sets on every session log. It only queues
/// (a bounded `try_send`, never blocking the append); when the queue is full
/// or the publisher is gone the line is counted as dropped, a gap in its
/// session's ids.
#[derive(Debug)]
pub(crate) struct LogTap {
    tx: mpsc::Sender<LogLine>,
    dropped: Arc<AtomicU64>,
}

impl AppendTap for LogTap {
    fn appended(&self, event: &SessionEvent, line: &str) {
        let queued = self.tx.try_send(LogLine {
            session: event.session_id.clone(),
            kind: event.payload.kind_str(),
            line: line.to_string(),
        });
        if queued.is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The publisher's end of [`log_tap`].
#[derive(Debug)]
pub(crate) struct LogLines {
    rx: mpsc::Receiver<LogLine>,
    dropped: Arc<AtomicU64>,
}

/// The tap is made before the publisher starts, so the daemon can hand it to
/// every session log from the first one on (the supervisor's is created
/// before the router exists). Lines queue until [`spawn`] drains them.
pub(crate) fn log_tap(queue_len: usize) -> (Arc<LogTap>, LogLines) {
    let (tx, rx) = mpsc::channel(queue_len);
    let dropped = Arc::new(AtomicU64::new(0));
    (
        Arc::new(LogTap {
            tx,
            dropped: dropped.clone(),
        }),
        LogLines { rx, dropped },
    )
}

/// A session notification's `(subject, payload)` on the live stream, or
/// `None` for anything else: responses, daemon-wide notifications,
/// `connection.lagged`.
fn live(daemon_id: &str, item: &Value) -> Option<(String, Vec<u8>)> {
    if item.get("id").is_some() {
        return None;
    }
    let method = item.get("method")?.as_str()?;
    let kind = method.strip_prefix(SESSION_PREFIX)?;
    let params = item.get("params")?;
    let session = params.get("session_id")?.as_str()?;
    let subject = format!("{}.live.{}", subject_base(daemon_id, session), token(kind));
    let event = json!({
        "v": 1,
        "daemon": daemon_id,
        "session": session,
        "method": method,
        "params": params,
    });
    Some((
        subject,
        serde_json::to_vec(&event).expect("notification serializes"),
    ))
}

/// The `dropped` count of a `connection.lagged` notice, or `None`.
fn lagged(item: &Value) -> Option<u64> {
    if item.get("method")?.as_str()? != CONNECTION_LAGGED_METHOD {
        return None;
    }
    item.get("params")?.get("dropped")?.as_u64()
}

fn evt_subject(daemon_id: &str, line: &LogLine) -> String {
    format!(
        "{}.evt.{}",
        subject_base(daemon_id, &line.session),
        token(line.kind)
    )
}

/// Why one message was not published. Each is logged as itself: a
/// disconnect heals on its own, a failed publish may not (a payload over the
/// server's `max_payload`, say, or a connection lost mid-call; the error
/// says which), and the timeout is our own limit.
#[derive(Debug, Clone, PartialEq)]
enum Dropped {
    NotConnected,
    PublishFailed(String),
    TimedOut,
}

impl Dropped {
    fn report(&self, nats_url: &str, subject: &str, timeout: Duration) {
        match self {
            Dropped::NotConnected => tracing::warn!(nats = %nats_url,
                "events out: NATS not connected; dropping session log lines and live \
                 notifications until the client reconnects (each dropped log line is a gap \
                 in its session's event ids; session.events fills it)"),
            Dropped::PublishFailed(e) => tracing::warn!(nats = %nats_url, %subject, error = %e,
                "events out: publishing failed (`error` says why: a payload over the server's \
                 max_payload does not heal, a connection lost mid-call does); dropped (a log \
                 line is a gap in its session's event ids; session.events fills it)"),
            Dropped::TimedOut => tracing::warn!(nats = %nats_url, %subject, ?timeout,
                "events out: our publish timeout ([mesh].events_call_timeout_ms) fired with \
                 the client connected; dropped (a log line is a gap in its session's event \
                 ids; session.events fills it)"),
        }
    }
}

fn register_tap(router: &Router) -> ConnectionLane {
    router.register_tap(Origin {
        transport: "nats-events".into(),
        connection_id: None,
    })
}

/// Logs a NATS connection event. Core NATS does not acknowledge a publish,
/// so a server refusal (a permissions violation) arrives here, after
/// `publish()` succeeded. Returns whether the event may mean a loss.
fn on_nats_event(event: &async_nats::Event, nats_url: &str) -> bool {
    use async_nats::Event;
    match event {
        Event::ServerError(e) => {
            tracing::warn!(nats = %nats_url, error = %e,
                "events out: the NATS server reported an error; what it refused is lost (log \
                 lines show as gaps in their session's event ids, which session.events \
                 fills). A permissions violation means this daemon's NATS user may not \
                 publish on mu.api.v1.>");
            true
        }
        Event::ClientError(e) => {
            tracing::warn!(nats = %nats_url, error = %e,
                "events out: the NATS client reported an error; messages may be lost (log \
                 lines show as gaps in their session's event ids, which session.events \
                 fills)");
            true
        }
        Event::Connected => {
            tracing::info!(nats = %nats_url, "events out: connected to NATS");
            false
        }
        Event::Disconnected => {
            tracing::warn!(nats = %nats_url,
                "events out: disconnected from NATS; the last messages published before \
                 this may not have reached the server (log lines show as gaps in their \
                 session's event ids, which session.events fills). Dropping until the \
                 client reconnects");
            true
        }
        other => {
            tracing::info!(nats = %nats_url, event = %other, "events out: NATS connection event");
            false
        }
    }
}

/// Start publishing. The client is created here so a bad `[mesh].nats_url`
/// is the caller's error (a refusal to start), not a task that quietly
/// exits. With `retry_on_initial_connect` the client is built without I/O
/// and connects in the background (async-nats 0.49 `connect_with_options`
/// skips `try_connect` and spawns the connector); `call_timeout` bounds it
/// anyway, so no broker behaviour can stall boot. The router tap is
/// registered before this returns, so every notification emitted afterwards
/// is seen. The task holds the router only weakly, so it ends when the
/// daemon's router drops.
pub(crate) async fn spawn(
    cfg: EventsConfig,
    daemon_id: String,
    router: &Router,
    mut lines: LogLines,
) -> Result<JoinHandle<()>, SpawnError> {
    let EventsConfig {
        nats_url,
        call_timeout,
        ..
    } = cfg;
    // A server refusal arrives later as a connection event; they come back
    // to the loop here.
    let (events_tx, mut nats_events) = mpsc::unbounded_channel();
    let connect = async_nats::ConnectOptions::new()
        .retry_on_initial_connect()
        .event_callback(move |event| {
            let events_tx = events_tx.clone();
            async move {
                // The loop has ended (daemon exit) when this fails.
                let _ = events_tx.send(event);
            }
        })
        .connect(nats_url.as_str());
    let client = match tokio::time::timeout(call_timeout, connect).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return Err(SpawnError::Client(e)),
        Err(_) => return Err(SpawnError::TimedOut(call_timeout)),
    };
    let mut lane = register_tap(router);
    let weak: WeakRouter = router.downgrade();
    tracing::info!(nats = %nats_url,
        log = %format!("mu.api.v1.{}.*.evt.>", token(&daemon_id)),
        live = %format!("mu.api.v1.{}.*.live.>", token(&daemon_id)),
        "events out: publishing session logs and live notifications");
    Ok(tokio::spawn(async move {
        // Log lines the tap could not queue, as last reported.
        let mut tap_dropped_seen = 0u64;
        // The cause of the current run of drops and its length, so a run is
        // logged once when it starts (or its cause changes) and once, with
        // its count, when publishing resumes.
        let mut dropping: Option<(Dropped, u64)> = None;
        loop {
            // Unbiased: when several branches are ready one is picked at
            // random, so sustained log traffic cannot starve the live lane
            // (approval prompts travel there) or keep it from reaching its
            // slow-consumer recovery, nor the reverse.
            let (subject, payload) = tokio::select! {
                Some(event) = nats_events.recv() => {
                    on_nats_event(&event, &nats_url);
                    continue;
                }
                Some(line) = lines.rx.recv() => {
                    let tap_dropped = lines.dropped.load(Ordering::Relaxed);
                    if tap_dropped > tap_dropped_seen {
                        tracing::warn!(
                            dropped = tap_dropped - tap_dropped_seen,
                            "events out: log lines were dropped because the publish queue \
                             was full ([mesh].events_queue_len; publishing is slower than the \
                             sessions append, and the drop warnings above say why). Each is a \
                             gap in its session's event ids; session.events fills it"
                        );
                        tap_dropped_seen = tap_dropped;
                    }
                    (evt_subject(&daemon_id, &line), line.line.into_bytes())
                }
                received = lane.recv() => match received {
                    Ok(envelope) => match live(&daemon_id, &envelope.item.0) {
                        Some(routed) => routed,
                        None => {
                            // The router's notice that this lane evicted
                            // ephemeral notifications under pressure (text
                            // deltas and provider-status ticks only; nothing
                            // else is evictable).
                            if let Some(dropped) = lagged(&envelope.item.0) {
                                tracing::warn!(dropped,
                                    "events out: the live tap shed streaming notifications \
                                     (text deltas, provider-status ticks) under pressure; \
                                     publishing is slower than the sessions stream. The \
                                     finalized text and the log carry what they streamed");
                            }
                            continue;
                        }
                    },
                    Err(LaneTerminated::Closed) => break,
                    Err(LaneTerminated::SlowConsumer { dropped_ephemeral }) => {
                        // A connected but stalled server makes every publish
                        // wait out the deadline, and the tap lane (thinking
                        // and tool-call deltas are durable on the router)
                        // fills to the cap. Re-register and carry on; the
                        // queued live notifications are gone, the log is not.
                        // The strong router lives only for this call.
                        let Some(router) = weak.upgrade() else { break };
                        lane = register_tap(&router);
                        drop(router);
                        tracing::warn!(
                            dropped_ephemeral,
                            "events out: the live tap fell past the router's slow-consumer \
                             cap (publishing is slower than the sessions emit; the drop \
                             warnings above say why). The live notifications queued on it \
                             are lost; the log stream is separate and carries every line that \
                             lasts, approval prompts included. Tap re-registered, publishing \
                             continues"
                        );
                        continue;
                    }
                },
            };
            let outcome = if client.connection_state() != State::Connected {
                Err(Dropped::NotConnected)
            } else {
                match tokio::time::timeout(
                    call_timeout,
                    client.publish(subject.clone(), payload.into()),
                )
                .await
                {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(Dropped::PublishFailed(e.to_string())),
                    Err(_) => Err(Dropped::TimedOut),
                }
            };
            match (outcome, &mut dropping) {
                (Ok(()), _) => {
                    if let Some((_, dropped)) = dropping.take() {
                        tracing::info!(dropped, "events out: publishing again");
                    }
                }
                (Err(cause), Some((current, count))) if *current == cause => *count += 1,
                (Err(cause), _) => {
                    if let Some((previous, dropped)) = dropping.take() {
                        tracing::info!(dropped, cause = ?previous,
                            "events out: drop cause changed");
                    }
                    cause.report(&nats_url, &subject, call_timeout);
                    dropping = Some((cause, 1));
                }
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_core::event_log::{EventActor, EventPayload, SessionEventLog};
    use mu_core::transport::{Outbound, OutboundEnvelope};

    fn notification(method: &str, session: &str) -> Value {
        json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": { "session_id": session, "x": 1 },
        })
    }

    fn decode(payload: &[u8]) -> Value {
        serde_json::from_slice(payload).expect("json")
    }

    fn user_msg(t: &str) -> EventPayload {
        EventPayload::UserMessage { content: t.into() }
    }

    fn cfg(url: &str) -> EventsConfig {
        EventsConfig {
            nats_url: url.into(),
            call_timeout: Duration::from_secs(1),
            queue_len: 64,
        }
    }

    async fn next(sub: &mut async_nats::Subscriber) -> async_nats::Message {
        use futures::StreamExt as _;
        tokio::time::timeout(Duration::from_secs(5), sub.next())
            .await
            .expect("message within 5s")
            .expect("subscription open")
    }

    #[test]
    fn session_notifications_go_on_the_live_stream() {
        let (subj, payload) =
            live("d1", &notification("session.text_delta", "session-1")).expect("routed");
        assert_eq!(subj, "mu.api.v1.d1.session-1.live.text_delta");
        let ev = decode(&payload);
        assert_eq!(ev["v"], 1);
        assert_eq!(ev["daemon"], "d1");
        assert_eq!(ev["session"], "session-1");
        assert_eq!(ev["method"], "session.text_delta");
        assert_eq!(ev["params"]["x"], 1);
        let (subj, _) = live(
            "d1",
            &notification(mu_core::protocol::InputRequiredEvent::METHOD, "s"),
        )
        .unwrap();
        assert!(subj.ends_with(".live.input_required"), "{subj}");
    }

    #[test]
    fn non_session_traffic_is_not_published_live() {
        // A response.
        assert!(live(
            "d1",
            &json!({ "jsonrpc": "2.0", "id": 1, "result": { "session_id": "s" } })
        )
        .is_none());
        // Daemon-wide and connection notifications.
        assert!(live("d1", &notification("daemon.announce", "s")).is_none());
        assert!(live(
            "d1",
            &json!({ "jsonrpc": "2.0", "method": "connection.lagged", "params": { "dropped": 3 } })
        )
        .is_none());
        // A session method without a session id.
        assert!(live(
            "d1",
            &json!({ "jsonrpc": "2.0", "method": "session.done", "params": {} })
        )
        .is_none());
    }

    #[test]
    fn ids_cannot_escape_their_subject_tokens() {
        let (subj, payload) = live("d.1", &notification("session.done", "a.b *>\tc")).unwrap();
        assert_eq!(subj, "mu.api.v1.d_1.a_b____c.live.done");
        // The envelope keeps the real ids.
        assert_eq!(decode(&payload)["session"], "a.b *>\tc");
        assert_eq!(decode(&payload)["daemon"], "d.1");
        let line = LogLine {
            session: "a.b".into(),
            kind: "user_message",
            line: String::new(),
        };
        assert_eq!(
            evt_subject("d.1", &line),
            "mu.api.v1.d_1.a_b.evt.user_message"
        );
    }

    /// The tap never blocks an append: past the queue's length a line is
    /// counted as dropped.
    #[test]
    fn the_log_tap_queues_lines_and_counts_what_it_cannot() {
        let (tap, mut lines) = log_tap(1);
        let log = SessionEventLog::new("s");
        assert!(log.set_append_tap(tap));
        log.append(EventActor::User, user_msg("a"));
        log.append(EventActor::User, user_msg("b"));
        let first = lines.rx.try_recv().expect("first line queued");
        assert_eq!(first.session, "s");
        assert_eq!(first.kind, "user_message");
        assert_eq!(decode(first.line.as_bytes())["id"], 1);
        assert!(lines.rx.try_recv().is_err());
        assert_eq!(lines.dropped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_lagged_notice_yields_its_count() {
        assert_eq!(
            lagged(
                &json!({ "jsonrpc": "2.0", "method": "connection.lagged", "params": { "dropped": 3 } })
            ),
            Some(3)
        );
        assert_eq!(lagged(&notification("session.done", "s")), None);
    }

    /// A server refusal arrives as a connection event, not a publish error;
    /// a disconnect may have lost what was published just before it.
    #[test]
    fn a_server_or_client_error_is_reported_as_a_possible_loss() {
        assert!(!on_nats_event(&async_nats::Event::Connected, "u"));
        assert!(on_nats_event(&async_nats::Event::Disconnected, "u"));
        assert!(on_nats_event(
            &async_nats::Event::ServerError(async_nats::ServerError::Other(
                "Permissions Violation for Publish".into()
            )),
            "u",
        ));
    }

    /// LIVE (skips without nats-server): a server whose ACL denies publishing
    /// on `mu.api.v1.>` reports it as a connection event, which this client
    /// does deliver; the publish call itself succeeds.
    #[tokio::test]
    async fn an_acl_denied_publish_surfaces_as_a_server_error() {
        let bin = std::env::var("NATS_BIN").unwrap_or_else(|_| "nats-server".to_string());
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("nats.conf");
        std::fs::write(
            &conf,
            format!(
                "listen: 127.0.0.1:{port}\nauthorization {{ users = [ {{ user: u, password: p, \
                 permissions: {{ publish: {{ deny: [\"mu.api.v1.>\"] }} }} }} ] }}\n"
            ),
        )
        .unwrap();
        let Ok(mut srv) = std::process::Command::new(&bin)
            .args(["-c", conf.to_str().unwrap()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            eprintln!("skipping: cannot spawn nats-server ({bin})");
            return;
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let url = format!("127.0.0.1:{port}");
        let mut client = None;
        for _ in 0..50 {
            let tx = tx.clone();
            if let Ok(c) =
                async_nats::ConnectOptions::with_user_and_password("u".into(), "p".into())
                    .event_callback(move |e| {
                        let tx = tx.clone();
                        async move {
                            let _ = tx.send(e);
                        }
                    })
                    .connect(url.as_str())
                    .await
            {
                client = Some(c);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let client = client.expect("nats-server came up");
        client
            .publish("mu.api.v1.d1.s.evt.done", "{}".into())
            .await
            .expect("the publish call itself succeeds");
        client.flush().await.unwrap();
        let refused = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(e) = rx.recv().await {
                if on_nats_event(&e, &url) {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        let _ = srv.kill();
        assert!(refused, "the server's refusal was never observed");
    }

    /// LIVE (skips without nats-server): each line appended to a session
    /// log goes out on `evt.<kind>` as written to the log file, in id
    /// order; notifications addressed to any connection, and broadcast
    /// ones, go out on `live.<kind>`; dropping the router ends the task.
    #[tokio::test]
    async fn publishes_the_log_and_live_notifications_and_ends_with_the_router() {
        use futures::StreamExt as _;

        use crate::serve::registry::tests::{connect_retry, nats_server};

        let Some((mut srv, url, _dir)) = nats_server(false) else {
            return;
        };
        let sub_client = connect_retry(&url).await;
        let mut sub = sub_client.subscribe("mu.api.v1.d1.>").await.unwrap();
        sub_client.flush().await.unwrap();

        let router = Router::new();
        let conn = Origin {
            transport: "stdio".into(),
            connection_id: None,
        };
        let _conn_lane = router.register(conn.clone());
        let (tap, lines) = log_tap(64);
        let task = spawn(cfg(&url), "d1".into(), &router, lines)
            .await
            .expect("client created");
        let send = |origin: Option<Origin>, method: &str| {
            router.send(OutboundEnvelope {
                origin,
                request_id: None,
                command_seq: None,
                item: Outbound(notification(method, "session-1")),
            })
        };

        // The publisher's client connects in the background and drops
        // messages until it has; probe until one lands.
        let mut up = false;
        for _ in 0..50 {
            send(None, "session.warmup");
            if let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(100), sub.next()).await
            {
                up = true;
                break;
            }
        }
        assert!(up, "publisher never connected");
        while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(200), sub.next()).await {
        }

        let log_dir = tempfile::tempdir().unwrap();
        let log_path = log_dir.path().join("session-1.jsonl");
        let log = SessionEventLog::new("session-1");
        log.attach_disk_writer(&log_path).unwrap();
        assert!(log.set_append_tap(tap));
        log.append(EventActor::User, user_msg("a"));
        log.append(EventActor::User, user_msg("b"));
        let disk: Vec<String> = std::fs::read_to_string(&log_path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        for (i, line) in disk.iter().enumerate() {
            let m = next(&mut sub).await;
            assert_eq!(
                m.subject.as_str(),
                "mu.api.v1.d1.session-1.evt.user_message"
            );
            assert_eq!(
                std::str::from_utf8(&m.payload).unwrap(),
                line,
                "line {i} is published as written to the log"
            );
        }

        send(Some(conn.clone()), "session.text_delta");
        send(None, "session.mailbox_message");
        let a = next(&mut sub).await;
        assert_eq!(a.subject.as_str(), "mu.api.v1.d1.session-1.live.text_delta");
        let b = next(&mut sub).await;
        assert_eq!(
            b.subject.as_str(),
            "mu.api.v1.d1.session-1.live.mailbox_message"
        );

        drop(_conn_lane);
        drop(router);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("task ends once the router is gone")
            .expect("task did not panic");
        let _ = srv.kill();
    }

    /// LIVE (skips without nats-server): a backlog of log lines does not
    /// hold up the live stream. With thousands of lines queued, an approval
    /// prompt sent after them is published long before the backlog drains.
    #[tokio::test]
    async fn a_log_backlog_does_not_starve_the_live_stream() {
        use futures::StreamExt as _;

        use crate::serve::registry::tests::{connect_retry, nats_server};

        let Some((mut srv, url, _dir)) = nats_server(false) else {
            return;
        };
        let sub_client = connect_retry(&url).await;
        let mut sub = sub_client.subscribe("mu.api.v1.d1.>").await.unwrap();
        sub_client.flush().await.unwrap();

        let router = Router::new();
        const BACKLOG: usize = 5000;
        let (tap, lines) = log_tap(BACKLOG + 1);
        let _task = spawn(cfg(&url), "d1".into(), &router, lines)
            .await
            .expect("client created");
        let send = |method: &str| {
            router.send(OutboundEnvelope {
                origin: None,
                request_id: None,
                command_seq: None,
                item: Outbound(notification(method, "s")),
            })
        };
        let mut up = false;
        for _ in 0..50 {
            send("session.warmup");
            if let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(100), sub.next()).await
            {
                up = true;
                break;
            }
        }
        assert!(up, "publisher never connected");
        while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(200), sub.next()).await {
        }

        // The test runtime is single-threaded: the whole backlog and the
        // prompt are queued before the task runs again.
        let log = SessionEventLog::new("s");
        assert!(log.set_append_tap(tap));
        for _ in 0..BACKLOG {
            log.append(EventActor::User, user_msg("x"));
        }
        send(mu_core::protocol::InputRequiredEvent::METHOD);
        let mut log_lines_first = 0;
        loop {
            let m = next(&mut sub).await;
            if m.subject.as_str().ends_with(".live.input_required") {
                break;
            }
            log_lines_first += 1;
        }
        assert!(
            log_lines_first < BACKLOG / 2,
            "the prompt waited behind {log_lines_first} of {BACKLOG} log lines"
        );
        let _ = srv.kill();
    }

    /// LIVE (skips without nats-server): a live tap poisoned past the
    /// router's hard cap re-registers, and later notifications still reach
    /// NATS. The test runtime is single-threaded, so a synchronous flood
    /// lands before the task gets to drain any of it.
    #[tokio::test]
    async fn a_poisoned_live_tap_re_registers_and_publishing_continues() {
        use futures::StreamExt as _;
        use mu_core::transport::LANE_HARD_CAP;

        use crate::serve::registry::tests::{connect_retry, nats_server};

        let Some((mut srv, url, _dir)) = nats_server(false) else {
            return;
        };
        let sub_client = connect_retry(&url).await;
        let mut sub = sub_client.subscribe("mu.api.v1.d1.after.>").await.unwrap();
        sub_client.flush().await.unwrap();

        let router = Router::new();
        let (_tap, lines) = log_tap(64);
        let _task = spawn(cfg(&url), "d1".into(), &router, lines)
            .await
            .expect("client created");
        let send = |session: &str| {
            router.send(OutboundEnvelope {
                origin: None,
                request_id: None,
                command_seq: None,
                item: Outbound(notification("session.done", session)),
            })
        };
        for _ in 0..=LANE_HARD_CAP {
            send("flood");
        }
        // Keep sending until one lands on the re-registered tap (the client
        // may also still be connecting).
        let mut landed = false;
        for _ in 0..50 {
            send("after");
            if let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(100), sub.next()).await
            {
                assert_eq!(m.subject.as_str(), "mu.api.v1.d1.after.live.done");
                landed = true;
                break;
            }
        }
        assert!(
            landed,
            "publishing did not continue after the tap was poisoned"
        );
        let _ = srv.kill();
    }

    /// A URL the client cannot even be built from is the caller's error,
    /// not a spawned task that exits.
    #[tokio::test]
    async fn malformed_url_is_an_error_from_spawn() {
        let router = Router::new();
        let (_tap, lines) = log_tap(1);
        let res = spawn(cfg("nats://host:notaport"), "d1".into(), &router, lines).await;
        assert!(res.is_err(), "a malformed URL must not spawn a publisher");
    }

    #[test]
    fn gated_by_master_switch_and_events_and_floors() {
        let mut mesh = MeshConfig {
            events: true,
            ..MeshConfig::default()
        };
        assert_eq!(from_mesh(&mesh).unwrap(), None, "enabled = false wins");
        mesh.enabled = true;
        assert_eq!(
            from_mesh(&mesh).unwrap(),
            Some(EventsConfig {
                nats_url: mesh.nats_url.clone(),
                call_timeout: Duration::from_millis(1000),
                queue_len: 4096,
            })
        );
        mesh.events_queue_len = 0;
        assert!(from_mesh(&mesh).is_err(), "a zero queue refuses to start");
        mesh.events_queue_len = 1;
        mesh.events_call_timeout_ms = 0;
        assert!(from_mesh(&mesh).is_err(), "a zero timeout refuses to start");
        mesh.events = false;
        assert_eq!(from_mesh(&mesh).unwrap(), None);
    }
}
