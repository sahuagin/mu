//! JSON-RPC client for `mu serve`. Spawns the daemon as a child process
//! and communicates via stdin/stdout. Synchronous (`std::thread` +
//! `mpsc`) — matches the surface of mu-tui's `mu_client.rs` but trimmed
//! to what mu-solo actually needs. Will share-via-extract with mu-tui's
//! client when both are stable.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use rand::RngCore;
use serde_json::Value;

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
    #[allow(dead_code)]
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<Message>,
    pending_notifications: VecDeque<Message>,
    next_id: AtomicI64,
    default_read_timeout: Duration,
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

        // Stdout reader thread. Parses each line as a JSON-RPC message
        // and forwards via mpsc.
        thread::Builder::new()
            .name("mu-solo-stdout".into())
            .spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    match line {
                        Ok(raw) => match parse_message(&raw) {
                            Ok(msg) => {
                                if tx.send(msg).is_err() {
                                    return;
                                }
                            }
                            Err(e) => {
                                let _ = tx.send(Message::ReaderError(format!(
                                    "stdout parse: {e}: {raw:?}"
                                )));
                            }
                        },
                        Err(e) => {
                            let _ = tx.send(Message::ReaderError(format!("stdout read: {e}")));
                            return;
                        }
                    }
                }
                let _ = tx.send(Message::Eof);
            })
            .context("spawning stdout reader thread")?;

        let mut client = Self {
            child,
            stdin,
            rx,
            pending_notifications: VecDeque::new(),
            next_id: AtomicI64::new(1),
            default_read_timeout: Duration::from_secs(120),
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
        self.stdin
            .write_all(line.as_bytes())
            .context("write request to daemon")?;
        self.stdin.flush().context("flush request")?;

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
                Ok(msg @ Message::Notification { .. }) => {
                    self.pending_notifications.push_back(msg);
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

    /// Non-blocking notification poll. Returns one buffered or
    /// fresh-from-the-wire notification if available, else None.
    pub fn try_recv_notification(&mut self) -> Option<Message> {
        if let Some(msg) = self.pending_notifications.pop_front() {
            return Some(msg);
        }
        match self.rx.try_recv() {
            Ok(msg @ Message::Notification { .. }) => Some(msg),
            Ok(Message::Eof) => Some(Message::Eof),
            Ok(Message::ReaderError(e)) => Some(Message::ReaderError(e)),
            Ok(Message::Response { .. }) => None, // stale; drop
            Err(_) => None,
        }
    }
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
