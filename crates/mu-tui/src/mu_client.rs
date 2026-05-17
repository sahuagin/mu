//! Synchronous JSON-RPC client for a spawned `mu serve`.
//!
//! Ported from the Python defensive layer (`mu_client.py`, jj
//! qmlywmnm 186d5970 in claude-personal) — same architecture:
//!
//!   - spawn the daemon as a child process
//!   - drain stdout in a background thread → `mpsc::Sender<Message>`
//!   - drain stderr in a background thread → bounded ring buffer
//!   - send requests on a single producer; pair them with responses
//!     by id from the incoming queue
//!   - request reads have a configurable timeout; notification reads
//!     are non-blocking (poll-style)
//!
//! Why not tokio? The TUI's main loop is synchronous around
//! `crossterm::event::poll`. A tokio runtime would mean either an
//! async event loop (extra cost for v1) or bridging channels between
//! runtimes (extra moving parts). Plain `std::thread` + `mpsc` matches
//! the surface area of the Python defensive layer and is enough until
//! we need cancellation tokens or fan-out.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use rand::RngCore;
use serde_json::Value;

/// One message read off the daemon's stdout. Either a response to one
/// of our requests (id present) or a notification (method present, no
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
    /// Reader thread itself errored (rare).
    ReaderError(String),
}

/// Bounded stderr ring buffer for diagnostics.
const STDERR_BUF_CAP: usize = 8000;

pub struct MuClient {
    /// RAII handle for the spawned `mu serve` child. Held for the
    /// lifetime of the client; `close()` consumes it for cooperative
    /// shutdown (cooperative-then-SIGKILL fallback). Tokio's `Child`
    /// does NOT kill on drop, so without explicit close() the daemon
    /// outlives the client — see ping/close below.
    #[allow(dead_code)]
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<Message>,
    /// Buffer of notifications drained but not yet handed to the
    /// consumer. `recv_notification` pulls from here when present.
    pending_notifications: VecDeque<Message>,
    next_id: AtomicI64,
    stderr_buf: Arc<Mutex<VecDeque<String>>>,
    /// Default per-request read timeout; can be overridden per call.
    default_read_timeout: Duration,
}

impl MuClient {
    /// Spawn `mu serve` and start drainer threads.
    ///
    /// `mu_binary` is the daemon executable (typically the workspace
    /// release build). `tools` is the comma-separable list (`["read",
    /// "glob", ...]`) used to populate `--tools`. `cwd` is the
    /// working directory for the daemon (often the project root the
    /// agent will reason about).
    pub fn spawn(
        mu_binary: &str,
        tools: &[&str],
        cwd: &std::path::Path,
        bash_yolo: bool,
        bash_prompt: bool,
    ) -> Result<Self> {
        // Warn on orphan mu serves — same heuristic as the Python
        // defensive layer (claude-personal scripts/mu_client.py:172).
        // Empirically (2026-05-11) orphans correlate with Popen-style
        // hangs on the next spawn.
        warn_on_orphans(mu_binary);

        // mu-fnn: generate a per-spawn bearer token and pass it via env
        // to the child. Daemon (serve::run) reads MU_BEARER_TOKEN, sets
        // BEARER auth, and rejects every protected RPC until we present
        // the same token in peer.auth_initiate below.
        let bearer_token = generate_bearer_token();

        let mut cmd = Command::new(mu_binary);
        cmd.arg("serve");
        cmd.env("MU_BEARER_TOKEN", &bearer_token);
        if !tools.is_empty() {
            cmd.arg("--tools").arg(tools.join(","));
        }
        if bash_yolo {
            cmd.arg("--bash-yolo");
        }
        if bash_prompt {
            cmd.arg("--bash-prompt");
        }
        cmd.current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn mu binary {mu_binary}"))?;
        let stdin = child.stdin.take().context("missing stdin pipe")?;
        let stdout = child.stdout.take().context("missing stdout pipe")?;
        let stderr = child.stderr.take().context("missing stderr pipe")?;

        let (tx, rx) = mpsc::channel::<Message>();
        let stderr_buf = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_BUF_CAP)));

        // Stdout reader thread.
        let tx_clone = tx.clone();
        thread::Builder::new()
            .name("mu-stdout".into())
            .spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    match line {
                        Ok(raw) => match parse_message(&raw) {
                            Ok(msg) => {
                                if tx_clone.send(msg).is_err() {
                                    return; // receiver dropped
                                }
                            }
                            Err(e) => {
                                let _ = tx_clone.send(Message::ReaderError(format!(
                                    "stdout parse error: {e}: line={raw:?}"
                                )));
                            }
                        },
                        Err(e) => {
                            let _ = tx_clone
                                .send(Message::ReaderError(format!("stdout io error: {e}")));
                            return;
                        }
                    }
                }
                let _ = tx_clone.send(Message::Eof);
            })?;

        // Stderr reader thread → ring buffer.
        let buf_clone = stderr_buf.clone();
        thread::Builder::new()
            .name("mu-stderr".into())
            .spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    let line = match line {
                        Ok(l) => l,
                        Err(_) => return,
                    };
                    let mut buf = match buf_clone.lock() {
                        Ok(b) => b,
                        Err(_) => return,
                    };
                    if buf.len() >= STDERR_BUF_CAP {
                        buf.pop_front();
                    }
                    buf.push_back(line);
                }
            })?;

        let mut this = Self {
            child,
            stdin,
            rx,
            pending_notifications: VecDeque::new(),
            next_id: AtomicI64::new(1),
            stderr_buf,
            default_read_timeout: Duration::from_secs(60),
        };

        // mu-fnn: present the spawn-time token before any protected RPC.
        // Without this every session.list / daemon.stats poll returns
        // -32001 and the firehose fills with auth errors.
        this.authenticate_with(&bearer_token)?;

        Ok(this)
    }

    /// mu-fnn handshake. Sends peer.auth_initiate with the BEARER token
    /// shared at spawn time. Bails on Denied or any non-success.
    fn authenticate_with(&mut self, token: &str) -> Result<()> {
        let result = self.request_with_timeout(
            "peer.auth_initiate",
            serde_json::json!({
                "mechanism": "bearer",
                "initial_response": token,
            }),
            Duration::from_secs(10),
        )?;
        let outcome = result.get("outcome").and_then(|v| v.as_str());
        if outcome == Some("accepted") {
            return Ok(());
        }
        Err(anyhow!(
            "peer.auth_initiate did not accept spawn-time token: {result}; stderr tail:\n{}",
            self.stderr_tail()
        ))
    }

    /// Tail of recently received stderr lines. Useful for diagnostics
    /// when a request times out.
    pub fn stderr_tail(&self) -> String {
        match self.stderr_buf.lock() {
            Ok(b) => b.iter().cloned().collect::<Vec<_>>().join("\n"),
            Err(_) => String::new(),
        }
    }

    /// Send a request and block until its response arrives or the
    /// timeout fires. Any notifications received in the meantime are
    /// queued in `pending_notifications` for `recv_notification` to
    /// pull later — they are not silently dropped.
    pub fn request_with_timeout(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let line = format!("{body}\n");
        self.stdin
            .write_all(line.as_bytes())
            .with_context(|| format!("write to mu serve stdin (request {method})"))?;
        self.stdin
            .flush()
            .with_context(|| format!("flush mu serve stdin (request {method})"))?;

        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| {
                    anyhow!(
                        "timed out after {:?} waiting for response to {method}; stderr tail:\n{}",
                        timeout,
                        self.stderr_tail()
                    )
                })?;
            match self.rx.recv_timeout(remaining) {
                Ok(Message::Response {
                    id: rid,
                    result,
                    error,
                }) => {
                    if rid != id {
                        // Cross-request scramble — keep waiting, but
                        // don't lose it.
                        self.pending_notifications.push_back(Message::Response {
                            id: rid,
                            result,
                            error,
                        });
                        continue;
                    }
                    if let Some(err) = error {
                        return Err(anyhow!("mu serve error on {method}: {err}"));
                    }
                    return Ok(result.unwrap_or(Value::Null));
                }
                Ok(notif @ Message::Notification { .. }) => {
                    self.pending_notifications.push_back(notif);
                }
                Ok(Message::Eof) => {
                    return Err(anyhow!(
                        "mu serve closed stdout while waiting for {method}; stderr tail:\n{}",
                        self.stderr_tail()
                    ));
                }
                Ok(Message::ReaderError(e)) => {
                    return Err(anyhow!("stdout reader error: {e}"));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(anyhow!("mu serve reader thread disconnected"));
                }
            }
        }
    }

    /// Convenience: use the default timeout.
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.request_with_timeout(method, params, self.default_read_timeout)
    }

    /// Pop one pending notification non-blockingly. Returns `None` if
    /// no notification has arrived since the last call. The TUI main
    /// loop calls this every tick to drain incoming events into its
    /// view state.
    pub fn try_recv_notification(&mut self) -> Option<Message> {
        // First serve anything that was queued during a prior request.
        while let Some(msg) = self.pending_notifications.pop_front() {
            match msg {
                n @ Message::Notification { .. } => return Some(n),
                Message::Response { .. } => {
                    // Orphaned response with no waiting caller — drop.
                }
                Message::Eof | Message::ReaderError(_) => return Some(msg),
            }
        }
        match self.rx.try_recv() {
            Ok(n @ Message::Notification { .. }) => Some(n),
            Ok(Message::Response { .. }) => None,
            Ok(other) => Some(other),
            Err(_) => None,
        }
    }

    /// Ping check. Returns true if the daemon answers within `timeout`.
    ///
    /// Not yet wired to the TUI's liveness path — held as the protocol
    /// hook for future health-check integration.
    #[allow(dead_code)]
    pub fn ping(&mut self, timeout: Duration) -> bool {
        self.request_with_timeout("ping", Value::Object(Default::default()), timeout)
            .is_ok()
    }

    /// Shut the daemon down cooperatively (close stdin → exit).
    ///
    /// Not yet wired to the TUI's exit path — daemon currently outlives
    /// the TUI session. Wiring at TUI shutdown is a future TUI bead.
    #[allow(dead_code)]
    pub fn close(mut self) {
        // Dropping stdin would close the pipe; just be explicit.
        drop(self.stdin);
        // Wait briefly for clean exit; SIGKILL if needed.
        let mut waited = 0u32;
        let max_wait_ms = 2000;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if waited >= max_wait_ms {
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                    waited += 50;
                }
                Err(_) => {
                    let _ = self.child.kill();
                    break;
                }
            }
        }
    }
}

fn parse_message(raw: &str) -> Result<Message> {
    let v: Value = serde_json::from_str(raw).context("invalid JSON")?;
    if let Some(method) = v.get("method").and_then(Value::as_str) {
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        Ok(Message::Notification {
            method: method.to_string(),
            params,
        })
    } else if let Some(id) = v.get("id").and_then(Value::as_i64) {
        let result = v.get("result").cloned();
        let error = v.get("error").cloned();
        Ok(Message::Response { id, result, error })
    } else {
        Err(anyhow!("message has neither method nor id: {v}"))
    }
}

/// Best-effort `pgrep` for pre-existing `mu serve` processes. Prints
/// a stderr warning if any are found but does not kill them — matches
/// the Python defensive layer's behavior.
fn warn_on_orphans(mu_binary: &str) {
    let output = match Command::new("pgrep").arg("-af").arg("mu serve").output() {
        Ok(o) => o,
        Err(_) => return,
    };
    let s = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = s
        .lines()
        .filter(|l| l.contains(mu_binary) || l.contains("mu serve"))
        .collect();
    if !lines.is_empty() {
        eprintln!(
            "[mu_client] WARNING: {} pre-existing `mu serve` process(es) detected; \
             new daemon spawn may stall. Consider `pkill -f 'mu serve'` first.",
            lines.len()
        );
        for ln in lines.iter().take(3) {
            eprintln!("  {ln}");
        }
    }
}

/// Per-spawn opaque BEARER token (128 bits hex). Mirrors
/// crates/mu-coding/src/ask.rs:generate_bearer_token — kept inline to
/// avoid a mu-coding dep just for one helper.
fn generate_bearer_token() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}
