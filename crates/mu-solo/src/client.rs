//! JSON-RPC client for `mu serve`. Spawns the daemon as a child process
//! and communicates via stdin/stdout.
//!
//! Hybrid sync/async: `request()` is synchronous, for short setup RPCs
//! (auth, create_session). Long-lived RPCs whose response arrives much
//! later (`ask_session` — end of turn) go through `request_nowait`
//! (mu-d3v6), which delivers the response on the async channel instead
//! of blocking. Notifications flow through a
//! `tokio::sync::mpsc::UnboundedReceiver` so the async event loop can
//! `select!` on them without polling.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use rand::RngCore;
use serde_json::Value;

use tokio::sync::mpsc as tokio_mpsc;

/// One message read off the daemon's stdout. Either a response to a
/// request we sent (id present) or a notification (method present, no
/// id).
#[derive(Debug, Clone)]
pub enum Message {
    Response {
        id: i64,
        result: Option<Value>,
        error: Option<Value>,
    },
    Notification {
        method: String,
        params: Value,
    },
    /// stdout reader hit EOF — daemon closed its output pipe.
    Eof,
    ReaderError(String),
}

pub struct Client {
    child: Child,
    /// `None` after [`Client::shutdown`] closed it to signal EOF.
    stdin: Option<ChildStdin>,
    /// Synchronous receiver for request/response pairing. mu-9x4j:
    /// the reader thread routes ONLY responses (plus Eof/ReaderError)
    /// here — notifications never touch this channel, so `request()`
    /// has nothing to stash and nothing can be delivered twice.
    rx: std::sync::mpsc::Receiver<Message>,
    next_id: AtomicI64,
    default_read_timeout: Duration,
    /// Async notification stream for the tokio event loop — the ONLY
    /// delivery path for notifications (exactly-once by construction).
    notif_rx: Option<tokio_mpsc::UnboundedReceiver<Message>>,
    /// mu-d3v6: request ids registered by `request_nowait` whose
    /// responses must be routed to the ASYNC channel instead of the
    /// sync one. Shared with the reader thread's router. Each id is
    /// one-shot: routing a response removes it.
    async_pending: Arc<Mutex<HashSet<i64>>>,
}

/// mu-9x4j: single routing point for everything the reader thread
/// pulls off the daemon's stdout. Notifications go to the async
/// channel only; responses go to the sync channel only — EXCEPT
/// responses whose id was registered via `request_nowait` (mu-d3v6),
/// which go to the async channel so the event loop can keep rendering
/// while a long-lived RPC (ask_session: the response lands at END of
/// turn) is in flight. Each message still has exactly one delivery
/// path, decided here. Eof and ReaderError fan out to BOTH (an
/// in-flight `request()` must fail fast, and the event loop must see
/// the daemon die even when no request is pending). Errors when both
/// destinations are gone — the reader thread uses that as its exit
/// signal.
fn route_message(
    msg: Message,
    resp_tx: &mpsc::Sender<Message>,
    notif_tx: &tokio_mpsc::UnboundedSender<Message>,
    async_pending: &Mutex<HashSet<i64>>,
) -> Result<(), ()> {
    match msg {
        Message::Notification { .. } => notif_tx.send(msg).map_err(|_| ()),
        Message::Response { id, .. } => {
            // One-shot: a registered id is consumed by its response.
            let is_async = async_pending
                .lock()
                .map(|mut s| s.remove(&id))
                .unwrap_or(false);
            if is_async {
                notif_tx.send(msg).map_err(|_| ())
            } else {
                resp_tx.send(msg).map_err(|_| ())
            }
        }
        Message::Eof | Message::ReaderError(_) => {
            let a = resp_tx.send(msg.clone()).is_ok();
            let b = notif_tx.send(msg).is_ok();
            if a || b {
                Ok(())
            } else {
                Err(())
            }
        }
    }
}

impl Client {
    /// Spawn `mu serve` and start a stdout-reader thread. `mu_binary`
    /// is the daemon executable path (typically `target/release/mu`).
    /// `bash_yolo` controls whether the daemon auto-approves bash
    /// invocations (convenience for solo development).
    pub fn spawn(
        mu_binary: &str,
        cwd: &std::path::Path,
        bash_yolo: bool,
        tools: &str,
    ) -> Result<Self> {
        // Per-spawn bearer token, set via env. Daemon reads
        // `MU_BEARER_TOKEN`, requires every protected RPC to present
        // the same token via `peer.auth_initiate`.
        let bearer_token = generate_bearer_token();

        let mut cmd = Command::new(mu_binary);
        cmd.arg("serve");
        cmd.env("MU_BEARER_TOKEN", &bearer_token);
        if !tools.is_empty() {
            cmd.arg("--tools").arg(tools);
        }
        if bash_yolo {
            cmd.arg("--bash-yolo");
        }
        cmd.current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn mu binary at {mu_binary}"))?;
        let stdin = child.stdin.take().context("missing stdin pipe")?;
        let stdout = child.stdout.take().context("missing stdout pipe")?;
        let stderr = child.stderr.take().context("missing stderr pipe")?;

        // Drain stderr in a background thread. Two reasons:
        //  1. If we just drop the handle, daemon writes to stderr
        //     eventually fill the pipe buffer and block — bad with
        //     verbose tracing turned on.
        //  2. We can route daemon logs somewhere visible. If
        //     MU_SOLO_DAEMON_LOG is set to a path, daemon stderr is
        //     appended there (good for RUST_LOG=mu_ai=debug runs).
        //     Otherwise the bytes are read and discarded so the
        //     pipe stays drained.
        let log_path: Option<std::path::PathBuf> =
            std::env::var_os("MU_SOLO_DAEMON_LOG").map(std::path::PathBuf::from);
        thread::Builder::new()
            .name("mu-solo-daemon-stderr".into())
            .spawn(move || {
                use std::io::Write;
                let mut sink: Option<std::fs::File> = log_path.and_then(|p| {
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&p)
                        .ok()
                });
                let mut reader = BufReader::new(stderr);
                let mut buf = String::new();
                loop {
                    buf.clear();
                    match reader.read_line(&mut buf) {
                        Ok(0) => return, // EOF
                        Ok(_) => {
                            if let Some(f) = sink.as_mut() {
                                let _ = f.write_all(buf.as_bytes());
                            }
                        }
                        Err(_) => return,
                    }
                }
            })
            .context("spawning daemon-stderr drain thread")?;

        let (tx, rx) = mpsc::channel::<Message>();
        let (notif_tx, notif_rx) = tokio_mpsc::unbounded_channel();

        // Stdout reader thread. Parses each line as a JSON-RPC message
        // and routes it: notifications to the async channel, responses
        // to the sync channel (mu-9x4j: the split lives HERE so each
        // message has exactly one delivery path — the previous design
        // had request() forwarding notifications to BOTH the async
        // channel and a sync buffer, and the app consumed both, so
        // every notification arriving during a synchronous RPC window
        // was processed twice: replayed scrollback blocks, double-
        // counted session.done usage).
        let reader_notif_tx = notif_tx.clone();
        let async_pending = Arc::new(Mutex::new(HashSet::new()));
        let reader_async_pending = Arc::clone(&async_pending);
        thread::Builder::new()
            .name("mu-solo-stdout".into())
            .spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    match line {
                        Ok(raw) => match parse_message(&raw) {
                            Ok(msg) => {
                                if route_message(msg, &tx, &reader_notif_tx, &reader_async_pending)
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            Err(e) => {
                                let _ = route_message(
                                    Message::ReaderError(format!("stdout parse: {e}: {raw:?}")),
                                    &tx,
                                    &reader_notif_tx,
                                    &reader_async_pending,
                                );
                            }
                        },
                        Err(e) => {
                            let _ = route_message(
                                Message::ReaderError(format!("stdout read: {e}")),
                                &tx,
                                &reader_notif_tx,
                                &reader_async_pending,
                            );
                            return;
                        }
                    }
                }
                let _ = route_message(Message::Eof, &tx, &reader_notif_tx, &reader_async_pending);
            })
            .context("spawning stdout reader thread")?;
        let mut client = Self {
            child,
            stdin: Some(stdin),
            rx,
            next_id: AtomicI64::new(1),
            default_read_timeout: Duration::from_secs(120),
            notif_rx: Some(notif_rx),
            async_pending,
        };

        // Authenticate immediately. Daemon rejects every protected RPC
        // until peer.auth_initiate succeeds. Schema per mu-fnn:
        // { mechanism: "bearer", initial_response: <token> } and the
        // result must carry outcome == "accepted".
        let result = client
            .request(
                "peer.auth_initiate",
                serde_json::json!({
                    "mechanism": "bearer",
                    "initial_response": bearer_token,
                }),
            )
            .context("peer.auth_initiate failed (daemon rejected handshake)")?;
        if result.get("outcome").and_then(|v| v.as_str()) != Some("accepted") {
            return Err(anyhow!(
                "peer.auth_initiate did not accept spawn-time token: {result}"
            ));
        }

        Ok(client)
    }

    /// Send a JSON-RPC request and wait for the response. Returns the
    /// `result` field on success; errors on RPC error or timeout.
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&req).context("serialize request")? + "\n";
        let stdin = self
            .stdin
            .as_mut()
            .context("daemon stdin already closed (client shut down)")?;
        stdin
            .write_all(line.as_bytes())
            .context("write request to daemon")?;
        stdin.flush().context("flush request")?;

        let deadline = Instant::now() + self.default_read_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(anyhow!("RPC {method} timed out"));
            }
            match self.rx.recv_timeout(remaining) {
                Ok(Message::Response {
                    id: rid,
                    result,
                    error,
                }) if rid == id => {
                    if let Some(err) = error {
                        return Err(anyhow!("RPC {method} error: {err}"));
                    }
                    return Ok(result.unwrap_or(Value::Null));
                }
                Ok(Message::Response { .. }) => {
                    // Out-of-order response — would happen with
                    // concurrent requests; for v1 we issue them
                    // sequentially. Log and continue waiting.
                    continue;
                }
                Ok(Message::Notification { .. }) => {
                    // mu-9x4j: structurally unreachable — the reader
                    // thread routes notifications to the async channel
                    // only. Tolerate rather than panic if it ever
                    // regresses; the notification is dropped HERE
                    // rather than delivered twice.
                    continue;
                }
                Ok(Message::Eof) => return Err(anyhow!("daemon closed stdout")),
                Ok(Message::ReaderError(e)) => return Err(anyhow!("reader error: {e}")),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(anyhow!("reader thread disconnected"))
                }
            }
        }
    }

    /// mu-d3v6: send a JSON-RPC request WITHOUT waiting for the
    /// response. The response will be delivered on the async
    /// notification channel as a `Message::Response`, so the tokio
    /// event loop keeps rendering while the RPC is in flight. Use for
    /// `ask_session`, whose response only arrives when the whole turn
    /// completes — waiting synchronously freezes streaming (and a
    /// turn longer than the read timeout would spuriously error).
    /// Returns the request id so the caller can correlate the
    /// eventual response.
    pub fn request_nowait(&mut self, method: &str, params: Value) -> Result<i64> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        // Register BEFORE writing so the response cannot race past the
        // router unregistered.
        if let Ok(mut s) = self.async_pending.lock() {
            s.insert(id);
        }
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&req).context("serialize request")? + "\n";
        let written = match self.stdin.as_mut() {
            Some(stdin) => stdin
                .write_all(line.as_bytes())
                .and_then(|()| stdin.flush()),
            None => Err(std::io::Error::other(
                "daemon stdin already closed (client shut down)",
            )),
        };
        if let Err(e) = written {
            // Nothing was (reliably) sent — unregister so a future id
            // collision can't misroute.
            if let Ok(mut s) = self.async_pending.lock() {
                s.remove(&id);
            }
            return Err(anyhow!("write request to daemon: {e}"));
        }
        Ok(id)
    }

    /// Take the async notification receiver. Called once during App
    /// setup to hand ownership to the tokio event loop. mu-9x4j: this
    /// is the ONLY notification delivery path — anything that arrived
    /// during setup `request()` calls is already queued on it in
    /// arrival order (the channel is unbounded and buffers until the
    /// receiver is consumed).
    pub fn take_notification_rx(&mut self) -> Option<tokio_mpsc::UnboundedReceiver<Message>> {
        self.notif_rx.take()
    }

    /// Shut the daemon down, bounded by construction
    /// (mu-mu-solo-loop-terminate-5ek5). Pre-fix, quit never touched
    /// the child at all: the daemon was left to notice stdin EOF at
    /// process exit, so a wedged daemon (the 1.88 GB-read incident)
    /// survived as an orphan grinding CPU/memory. There is no
    /// graceful-shutdown RPC to wait on (none exists in the protocol,
    /// and a wedged daemon wouldn't answer one) — the graceful signal
    /// IS the stdin close. Sequence: close stdin (EOF → daemon's
    /// command loop exits), poll up to `grace`, then SIGKILL + reap.
    /// Every step is non-blocking or bounded; this can never hang the
    /// quit path.
    pub fn shutdown(&mut self, grace: Duration) {
        let stdin = self.stdin.take();
        shutdown_child(&mut self.child, stdin, grace);
    }
}

/// Bounded child shutdown (free function so it's testable without a
/// real daemon handshake): drop `stdin` to signal EOF, give the child
/// `grace` to exit on its own, then SIGKILL and reap. `kill` on an
/// already-dead child is a no-op error; `wait` after SIGKILL returns
/// promptly (the OS guarantees delivery).
fn shutdown_child(child: &mut Child, stdin: Option<ChildStdin>, grace: Duration) {
    drop(stdin);
    let deadline = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return, // exited gracefully and reaped
            Ok(None) => {}
            Err(_) => break, // can't observe it — go straight to kill
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn parse_message(line: &str) -> Result<Message> {
    let v: Value = serde_json::from_str(line).context("invalid JSON")?;
    if let Some(id) = v.get("id").and_then(|x| x.as_i64()) {
        return Ok(Message::Response {
            id,
            result: v.get("result").cloned(),
            error: v.get("error").cloned(),
        });
    }
    if let Some(method) = v.get("method").and_then(|x| x.as_str()) {
        return Ok(Message::Notification {
            method: method.to_string(),
            params: v.get("params").cloned().unwrap_or(Value::Null),
        });
    }
    Err(anyhow!("message has neither id nor method"))
}

fn generate_bearer_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notif(method: &str) -> Message {
        Message::Notification {
            method: method.into(),
            params: Value::Null,
        }
    }

    fn channels() -> (
        mpsc::Sender<Message>,
        mpsc::Receiver<Message>,
        tokio_mpsc::UnboundedSender<Message>,
        tokio_mpsc::UnboundedReceiver<Message>,
    ) {
        let (tx, rx) = mpsc::channel();
        let (ntx, nrx) = tokio_mpsc::unbounded_channel();
        (tx, rx, ntx, nrx)
    }

    fn no_pending() -> Mutex<HashSet<i64>> {
        Mutex::new(HashSet::new())
    }

    /// mu-9x4j: the incident invariant. A notification reaches the
    /// async channel EXACTLY once and the sync channel NEVER — the
    /// double-processing bug was a notification landing on both.
    #[test]
    fn notifications_route_to_async_channel_only() {
        let (tx, rx, ntx, mut nrx) = channels();
        route_message(notif("session.done"), &tx, &ntx, &no_pending()).unwrap();

        let delivered = nrx.try_recv().expect("async channel got the notification");
        assert!(
            matches!(delivered, Message::Notification { ref method, .. } if method == "session.done")
        );
        assert!(nrx.try_recv().is_err(), "exactly once on the async channel");
        assert!(
            rx.try_recv().is_err(),
            "sync (response) channel must never see a notification"
        );
    }

    #[test]
    fn responses_route_to_sync_channel_only() {
        let (tx, rx, ntx, mut nrx) = channels();
        route_message(
            Message::Response {
                id: 7,
                result: Some(Value::Null),
                error: None,
            },
            &tx,
            &ntx,
            &no_pending(),
        )
        .unwrap();

        assert!(matches!(
            rx.try_recv().expect("sync channel got the response"),
            Message::Response { id: 7, .. }
        ));
        assert!(
            nrx.try_recv().is_err(),
            "async (notification) channel must never see a response"
        );
    }

    /// mu-d3v6: a response whose id was registered via
    /// `request_nowait` routes to the ASYNC channel exactly once and
    /// never to the sync channel — the event loop owns it.
    #[test]
    fn nowait_responses_route_to_async_channel_only() {
        let (tx, rx, ntx, mut nrx) = channels();
        let pending = Mutex::new(HashSet::from([9i64]));
        route_message(
            Message::Response {
                id: 9,
                result: Some(Value::Null),
                error: None,
            },
            &tx,
            &ntx,
            &pending,
        )
        .unwrap();

        assert!(matches!(
            nrx.try_recv()
                .expect("async channel got the nowait response"),
            Message::Response { id: 9, .. }
        ));
        assert!(
            rx.try_recv().is_err(),
            "sync channel must never see a nowait response"
        );
        assert!(
            pending.lock().unwrap().is_empty(),
            "registration is one-shot — consumed by routing"
        );
    }

    /// mu-d3v6: registration is per-id. An unregistered response in
    /// the presence of OTHER registered ids still routes sync.
    #[test]
    fn unregistered_responses_still_route_sync() {
        let (tx, rx, ntx, mut nrx) = channels();
        let pending = Mutex::new(HashSet::from([9i64]));
        route_message(
            Message::Response {
                id: 10,
                result: Some(Value::Null),
                error: None,
            },
            &tx,
            &ntx,
            &pending,
        )
        .unwrap();

        assert!(matches!(
            rx.try_recv().expect("sync channel got the response"),
            Message::Response { id: 10, .. }
        ));
        assert!(nrx.try_recv().is_err());
        assert!(
            pending.lock().unwrap().contains(&9),
            "unrelated registration untouched"
        );
    }

    /// Eof/ReaderError fan out to BOTH: an in-flight request() must
    /// fail fast AND the event loop must see the daemon die when no
    /// request is pending.
    #[test]
    fn eof_and_reader_errors_fan_out_to_both() {
        let (tx, rx, ntx, mut nrx) = channels();
        route_message(Message::Eof, &tx, &ntx, &no_pending()).unwrap();
        assert!(matches!(rx.try_recv().unwrap(), Message::Eof));
        assert!(matches!(nrx.try_recv().unwrap(), Message::Eof));

        route_message(
            Message::ReaderError("boom".into()),
            &tx,
            &ntx,
            &no_pending(),
        )
        .unwrap();
        assert!(matches!(rx.try_recv().unwrap(), Message::ReaderError(_)));
        assert!(matches!(nrx.try_recv().unwrap(), Message::ReaderError(_)));
    }

    /// The reader thread exits when nobody is listening: routing
    /// errors only when ALL destinations for the message are gone.
    #[test]
    fn route_errors_only_when_all_receivers_dropped() {
        let (tx, rx, ntx, nrx) = channels();
        drop(nrx);
        // Notification with async receiver gone → error (its only path).
        assert!(route_message(notif("session.delta"), &tx, &ntx, &no_pending()).is_err());
        // Eof still deliverable via the surviving sync channel.
        assert!(route_message(Message::Eof, &tx, &ntx, &no_pending()).is_ok());
        drop(rx);
        // Now every destination is gone.
        assert!(route_message(Message::Eof, &tx, &ntx, &no_pending()).is_err());
    }

    // ── bounded daemon shutdown (mu-mu-solo-loop-terminate-5ek5) ────────

    /// A child that honors stdin EOF (like a healthy daemon) exits
    /// within the grace window — no SIGKILL needed.
    #[test]
    fn shutdown_child_graceful_on_stdin_eof() {
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn cat");
        let stdin = child.stdin.take();

        let started = Instant::now();
        shutdown_child(&mut child, stdin, Duration::from_secs(5));

        // cat exits 0 on EOF, well inside the grace window.
        assert!(started.elapsed() < Duration::from_secs(5));
        // Child is reaped: try_wait reports the recorded exit.
        let status = child.try_wait().expect("try_wait").expect("exited");
        assert!(status.success(), "cat should exit 0 on EOF: {status:?}");
    }

    /// A wedged child that ignores stdin EOF (the incident shape:
    /// daemon grinding in a compute loop) is force-killed after the
    /// grace window. The whole call is bounded — this is the property
    /// the /q hang fix rests on.
    #[test]
    fn shutdown_child_kills_wedged_child_within_bound() {
        // `sleep` never reads stdin, so EOF does nothing — a stand-in
        // for a wedged daemon.
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn sleep");
        let stdin = child.stdin.take();

        let grace = Duration::from_millis(200);
        let started = Instant::now();
        shutdown_child(&mut child, stdin, grace);
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(5),
            "shutdown must be bounded; took {elapsed:?}"
        );
        let status = child.try_wait().expect("try_wait").expect("reaped");
        assert!(!status.success(), "sleep must have been killed: {status:?}");
    }
}
