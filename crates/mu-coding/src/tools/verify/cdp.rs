//! Minimal Chrome DevTools Protocol client over `--remote-debugging-pipe`
//! (mu-lg8j1 phase 2).
//!
//! Chrome speaks CDP on file descriptors 3 (commands in) and 4 (messages
//! out) as NUL-terminated JSON. A tiny `sh` wrapper dup's our stdin/stdout
//! pipes onto those fds and execs Chrome, so the client is plain
//! stdio — no WebSocket stack, no port to discover, and the SAME launch
//! works on another host through `ssh` (ssh stdio is 8-bit clean). When
//! Chrome is remote the artifact server on our loopback is carried to the
//! remote loopback by a reverse port-forward on that ssh session, so the
//! only thing that has to be reachable is ssh itself. Closing our stdin
//! (fd 3 on Chrome's side) makes Chrome exit — the wrapper then removes
//! the throwaway profile directory.
//!
//! Two message kinds arrive on fd 4: responses (`{"id":N,…}`) routed to
//! the waiting caller, and events (`{"method":…}`) queued for the probe.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

/// Headless flags proven on the mu-316wl grading rig (WebGL via
/// SwiftShader, CPU-only; `--use-angle=swiftshader` alone is deprecated
/// and fails). `--remote-debugging-pipe` is the transport this client
/// speaks.
pub const BASE_FLAGS: &[&str] = &[
    "--headless=new",
    "--disable-gpu",
    "--enable-unsafe-swiftshader",
    "--no-sandbox",
    "--remote-debugging-pipe",
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-dev-shm-usage",
    "--disable-background-timer-throttling",
    "--disable-renderer-backgrounding",
    "--window-size=1280,800",
    "--hide-scrollbars",
    "--mute-audio",
];

/// The fd shim. `$1` is the profile dir, the rest is the Chrome argv.
/// Chrome's own stdio goes to /dev/null — its stderr log is noisy and
/// would otherwise share the ssh channel with the protocol.
const WRAPPER: &str = r#"udd="$1"; shift
trap 'rm -rf "$udd"' EXIT
trap 'rm -rf "$udd"; exit 1' HUP TERM INT
exec 3<&0 4>&1 </dev/null >/dev/null 2>&1
"$@" --user-data-dir="$udd"
exit $?"#;

/// Bound on queued CDP events. The probed page is untrusted: a
/// `console.log` in a tight loop must not grow our memory while the
/// driver is inside a call or a sleep. Past the bound events are dropped
/// and counted ([`Cdp::dropped_events`]); the report says so.
pub const EVENT_QUEUE_CAP: usize = 4096;

/// Per-command response wait.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct Launcher {
    /// Chrome binary — local path, or the path on `ssh` host.
    pub chrome: PathBuf,
    pub ssh: Option<String>,
    pub extra_args: Vec<String>,
    /// With `ssh`: forward this loopback port from the remote host back
    /// to us (`ssh -R port:127.0.0.1:port`) so the remote Chrome can
    /// fetch from our artifact server at the same `127.0.0.1:port`.
    pub forward_port: Option<u16>,
}

impl Launcher {
    fn chrome_argv(&self, profile_dir: &str) -> Vec<String> {
        let mut argv = vec![
            "sh".to_string(),
            profile_dir.to_string(),
            self.chrome.display().to_string(),
        ];
        argv.extend(BASE_FLAGS.iter().map(|s| s.to_string()));
        argv.extend(self.extra_args.iter().cloned());
        argv.push("about:blank".to_string());
        argv
    }

    /// Build the process to spawn. Public for tests (argv inspection).
    pub fn command(&self, profile_dir: &str) -> Command {
        let argv = self.chrome_argv(profile_dir);
        let mut cmd = match &self.ssh {
            None => {
                let mut c = Command::new("sh");
                c.arg("-c").arg(WRAPPER).args(&argv);
                c
            }
            Some(host) => {
                // The remote login shell re-parses one string: quote every
                // word so paths with spaces and the wrapper script survive.
                let remote = std::iter::once("sh -c".to_string())
                    .chain(std::iter::once(shell_quote(WRAPPER)))
                    .chain(argv.iter().map(|a| shell_quote(a)))
                    .collect::<Vec<_>>()
                    .join(" ");
                let mut c = Command::new("ssh");
                c.arg("-o")
                    .arg("BatchMode=yes")
                    .arg("-o")
                    .arg("ConnectTimeout=15")
                    .arg("-o")
                    .arg("ServerAliveInterval=15")
                    .arg("-T");
                if let Some(port) = self.forward_port {
                    // Fail fast if the remote port can't be bound; otherwise
                    // ssh warns and continues and Chrome can't reach the
                    // artifact server.
                    c.arg("-o")
                        .arg("ExitOnForwardFailure=yes")
                        .arg("-R")
                        .arg(format!("{port}:127.0.0.1:{port}"));
                }
                c.arg(host).arg(remote);
                c
            }
        };
        // Same scrubbed environment as the node/python runner and a
        // strict-mode shell: Chrome renders model-authored pages and ssh
        // carries the launch to another host; neither needs the daemon's
        // API keys. Key-based ssh does need its agent socket, which the
        // whitelist does not carry, so pass it through when present.
        cmd.env_clear()
            .envs(crate::tools::bash::scrubbed_env_vars());
        for key in ["SSH_AUTH_SOCK", "SSH_AGENT_PID"] {
            if let Ok(v) = std::env::var(key) {
                cmd.env(key, v);
            }
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group: the backstop kill reaches Chrome's whole
            // tree (or the local ssh), not just the wrapper.
            .process_group(0)
            .kill_on_drop(true);
        cmd
    }

    /// Where the probe's throwaway profile goes (on the host Chrome runs).
    pub fn profile_dir() -> String {
        format!(
            "/tmp/mu-verify-profile-{}-{:08x}",
            std::process::id(),
            rand::random::<u32>()
        )
    }
}

/// POSIX single-quote shell quoting.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

pub struct Cdp {
    /// `Some` until [`Cdp::close`] reaps it or [`Drop`] hands it to a
    /// reaper task.
    child: Option<Child>,
    /// `Some` until [`Cdp::close`] takes it; dropping it closes Chrome's
    /// fd 3, which makes Chrome exit.
    writer: Option<ChildStdin>,
    next_id: AtomicU64,
    pending: Pending,
    events: mpsc::Receiver<Value>,
    dropped_events: Arc<AtomicU64>,
    /// Throwaway profile dir on the host Chrome runs on.
    pub profile: String,
    /// Collected stderr of the launcher (ssh diagnostics, wrapper
    /// failures) — surfaced when Chrome dies early.
    stderr: Arc<Mutex<Vec<u8>>>,
}

impl std::fmt::Debug for Cdp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cdp").finish()
    }
}

impl Cdp {
    /// Spawn Chrome via `launcher` and start the reader.
    pub async fn launch(launcher: &Launcher) -> anyhow::Result<Self> {
        let profile = Launcher::profile_dir();
        let mut child = launcher.command(&profile).spawn().map_err(|e| {
            anyhow::anyhow!(
                "cannot launch {}: {e}",
                if launcher.ssh.is_some() { "ssh" } else { "sh" }
            )
        })?;
        let writer = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr_pipe = child.stderr.take().expect("stderr piped");
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, events) = mpsc::channel(EVENT_QUEUE_CAP);
        let stderr: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let dropped_events = Arc::new(AtomicU64::new(0));

        let reader_pending = pending.clone();
        let reader_dropped = dropped_events.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut frame = Vec::new();
            loop {
                frame.clear();
                match reader.read_until(0, &mut frame).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if frame.last() == Some(&0) {
                            frame.pop();
                        }
                        let Ok(msg) = serde_json::from_slice::<Value>(&frame) else {
                            continue;
                        };
                        if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                            let waiter = reader_pending.lock().ok().and_then(|mut p| p.remove(&id));
                            if let Some(tx) = waiter {
                                let _ = tx.send(msg);
                            }
                        } else {
                            match event_tx.try_send(msg) {
                                Ok(()) => {}
                                Err(mpsc::error::TrySendError::Full(_)) => {
                                    reader_dropped.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => break,
                            }
                        }
                    }
                }
            }
            // EOF: fail every waiter so callers see "chrome exited".
            if let Ok(mut p) = reader_pending.lock() {
                p.clear();
            }
        });
        let stderr_sink = stderr.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr_pipe);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if let Ok(mut s) = stderr_sink.lock() {
                            if s.len() < 16 * 1024 {
                                s.extend_from_slice(line.as_bytes());
                            }
                        }
                    }
                }
            }
        });
        Ok(Self {
            child: Some(child),
            writer: Some(writer),
            next_id: AtomicU64::new(1),
            pending,
            events,
            dropped_events,
            profile,
            stderr,
        })
    }

    /// Events discarded because the queue was full (a flooding page).
    pub fn dropped_events(&self) -> u64 {
        self.dropped_events.load(Ordering::Relaxed)
    }

    /// Launcher stderr so far (ssh / wrapper diagnostics).
    pub fn launcher_stderr(&self) -> String {
        self.stderr
            .lock()
            .map(|s| String::from_utf8_lossy(&s).trim().to_string())
            .unwrap_or_default()
    }

    /// Send a command and wait for its response `result`. `session` scopes
    /// it to an attached page (flattened session id); `None` = browser.
    pub async fn call(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = Value::String(s.to_string());
        }
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .map_err(|_| anyhow::anyhow!("cdp: pending map poisoned"))?
            .insert(id, tx);
        let mut bytes = serde_json::to_vec(&msg)?;
        bytes.push(0);
        let Some(writer) = self.writer.as_mut() else {
            self.forget(id);
            return Err(anyhow::anyhow!(
                "cdp: connection already closed (sending {method})"
            ));
        };
        if let Err(e) = writer.write_all(&bytes).await {
            self.forget(id);
            return Err(anyhow::anyhow!(
                "cdp: chrome pipe closed while sending {method}: {e}{}",
                self.death_note()
            ));
        }
        let _ = writer.flush().await;
        let response = match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => {
                return Err(anyhow::anyhow!(
                    "cdp: chrome exited before answering {method}{}",
                    self.death_note()
                ))
            }
            Err(_) => {
                self.forget(id);
                return Err(anyhow::anyhow!(
                    "cdp: no response to {method} within {}s",
                    CALL_TIMEOUT.as_secs()
                ));
            }
        };
        if let Some(err) = response.get("error") {
            let text = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(anyhow::anyhow!("cdp: {method} failed: {text}"));
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Drop a pending waiter (send failed / timed out) so the map never
    /// accumulates stale entries.
    fn forget(&self, id: u64) {
        if let Ok(mut p) = self.pending.lock() {
            p.remove(&id);
        }
    }

    /// Pending waiters right now (tests: must be empty after failures).
    #[cfg(test)]
    fn pending_count(&self) -> usize {
        self.pending.lock().map(|p| p.len()).unwrap_or(0)
    }

    fn death_note(&self) -> String {
        let s = self.launcher_stderr();
        if s.is_empty() {
            String::new()
        } else {
            format!(
                " (launcher stderr: {})",
                s.lines().take(5).collect::<Vec<_>>().join(" | ")
            )
        }
    }

    /// Next event, or `None` at `timeout` / EOF.
    pub async fn next_event(&mut self, timeout: Duration) -> Option<Value> {
        tokio::time::timeout(timeout, self.events.recv())
            .await
            .ok()
            .flatten()
    }

    /// Everything queued right now, without waiting.
    pub fn drain_events(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(ev) = self.events.try_recv() {
            out.push(ev);
        }
        out
    }

    /// Create a page target and attach to it (flattened). Returns the
    /// session id to pass to page-scoped calls.
    pub async fn open_page(&mut self) -> anyhow::Result<String> {
        let created = self
            .call(None, "Target.createTarget", json!({"url": "about:blank"}))
            .await?;
        let target_id = created
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("cdp: createTarget returned no targetId"))?
            .to_string();
        let attached = self
            .call(
                None,
                "Target.attachToTarget",
                json!({"targetId": target_id, "flatten": true}),
            )
            .await?;
        attached
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("cdp: attachToTarget returned no sessionId"))
    }

    /// Ask Chrome to quit, then close the pipe (which also ends it), and
    /// reap. Never hangs: kill after a short grace.
    pub async fn close(mut self) {
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            self.call(None, "Browser.close", json!({})),
        )
        .await;
        drop(self.writer.take());
        if let Some(mut child) = self.child.take() {
            let pid = child.id();
            if tokio::time::timeout(Duration::from_secs(5), child.wait())
                .await
                .is_err()
            {
                super::runner::kill_process_group(pid);
                let _ = child.kill().await;
            }
        }
    }
}

/// Every drop path that is not [`Cdp::close`] — the tool call cancelled,
/// the probe future dropped, a panic — still ends the browser and cleans
/// up: our end of Chrome's fd 3 closes with the writer, so Chrome exits
/// on its own and the wrapper's EXIT trap removes the profile directory
/// (a SIGKILL would skip the trap, so the kill is only the backstop). The
/// child moves into a small reaper task: wait up to 10 s, then kill. If
/// no runtime is available (already shutting down) the kill is
/// immediate. Only the graceful `Browser.close` is skipped.
impl Drop for Cdp {
    fn drop(&mut self) {
        drop(self.writer.take());
        let Some(mut child) = self.child.take() else {
            return;
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let pid = child.id();
                    if tokio::time::timeout(Duration::from_secs(10), child.wait())
                        .await
                        .is_err()
                    {
                        super::runner::kill_process_group(pid);
                        let _ = child.kill().await;
                    }
                });
            }
            Err(_) => {
                super::runner::kill_process_group(child.id());
                let _ = child.start_kill();
            }
        }
    }
}

/// A fake Chrome for tests: a Python script that speaks the pipe
/// protocol on fds 3/4 through the same `sh` wrapper the real launch
/// uses, so the launcher, framing, routing and event delivery are all
/// exercised end to end without a browser.
#[cfg(test)]
pub(crate) fn fake_chrome_script() -> &'static str {
    r#"#!/usr/bin/env python3
import json, os, sys, base64
rd = os.fdopen(3, "rb", 0)
wr = os.fdopen(4, "wb", 0)
for a in sys.argv:
    if a.startswith("--user-data-dir="):
        os.makedirs(a.split("=", 1)[1], exist_ok=True)
def send(obj):
    wr.write(json.dumps(obj).encode() + b"\0"); wr.flush()
# 1x1 white PNG
PNG = base64.b64encode(bytes.fromhex(
  "89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000d4944415478da63f8ffff3f0005fe02fea72c1b3f0000000049454e44ae426082")).decode()
buf = b""
navigated = 0
while True:
    chunk = rd.read(4096)
    if not chunk: break
    buf += chunk
    while b"\0" in buf:
        raw, buf = buf.split(b"\0", 1)
        m = json.loads(raw)
        mid, method, p = m.get("id"), m.get("method"), m.get("params", {})
        sid = m.get("sessionId")
        if method == "Target.createTarget":
            send({"id": mid, "result": {"targetId": "T1"}})
        elif method == "Target.attachToTarget":
            send({"id": mid, "result": {"sessionId": "S1"}})
        elif method == "Page.navigate":
            navigated += 1
            send({"id": mid, "result": {"frameId": "F1"}})
            send({"method": "Runtime.consoleAPICalled", "sessionId": sid, "params": {"type": "log", "args": [{"type": "string", "value": "booting"}, {"type": "number", "value": 7}]}})
            send({"method": "Runtime.exceptionThrown", "sessionId": sid, "params": {"exceptionDetails": {"text": "Uncaught", "lineNumber": 119, "columnNumber": 14, "url": p.get("url"), "exception": {"description": "TypeError: Cannot read properties of null (reading 'getContext')\n    at boot (game.html:120:15)"}}}})
            send({"method": "Runtime.consoleAPICalled", "sessionId": sid, "params": {"type": "error", "args": [{"type": "string", "value": "texture missing"}]}})
            send({"method": "Log.entryAdded", "sessionId": sid, "params": {"entry": {"level": "error", "source": "network", "text": "Failed to load resource: 404", "url": "http://127.0.0.1/tex.png"}}})
            send({"method": "Page.loadEventFired", "sessionId": sid, "params": {"timestamp": 1.0}})
        elif method == "Runtime.evaluate":
            send({"id": mid, "result": {"result": {"type": "number", "value": 58}}})
        elif method == "Page.captureScreenshot":
            send({"id": mid, "result": {"data": PNG}})
        elif method == "Input.dispatchKeyEvent" and p.get("type") == "keyDown" and p.get("key") == "x":
            send({"method": "Runtime.exceptionThrown", "sessionId": sid, "params": {"exceptionDetails": {"text": "Uncaught", "exception": {"description": "ReferenceError: jump is not defined"}, "stackTrace": {"callFrames": [{"url": "http://127.0.0.1/game.html", "lineNumber": 41, "columnNumber": 2}]}}}})
            send({"id": mid, "result": {}})
        elif method == "Browser.close":
            send({"id": mid, "result": {}})
            sys.exit(0)
        elif method == "Fail.Please":
            send({"id": mid, "error": {"code": -32601, "message": "'Fail.Please' wasn't found"}})
        else:
            send({"id": mid, "result": {}})
"#
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    pub(crate) fn write_fake_chrome() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("fake-chrome");
        std::fs::write(&p, fake_chrome_script()).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, p)
    }

    #[test]
    fn local_and_ssh_argv_shapes() {
        let l = Launcher {
            chrome: PathBuf::from("/opt/c h/chrome"),
            ssh: None,
            extra_args: vec!["--extra".into()],
            forward_port: None,
        };
        let argv = l.chrome_argv("/tmp/p");
        assert_eq!(&argv[..3], &["sh", "/tmp/p", "/opt/c h/chrome"]);
        assert!(argv.contains(&"--remote-debugging-pipe".to_string()));
        assert!(argv.contains(&"--extra".to_string()));
        assert_eq!(argv.last().map(String::as_str), Some("about:blank"));
        let cmd = l.command("/tmp/p");
        let std_cmd = cmd.as_std();
        assert_eq!(std_cmd.get_program(), "sh");
        let args: Vec<String> = std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args[0], "-c");
        assert!(args[1].contains("exec 3<&0 4>&1"));
        assert_eq!(args[2], "sh");

        let r = Launcher {
            ssh: Some("gpubox".into()),
            forward_port: Some(40123),
            ..l
        };
        let cmd = r.command("/tmp/p");
        let std_cmd = cmd.as_std();
        assert_eq!(std_cmd.get_program(), "ssh");
        let args: Vec<String> = std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(args.contains(&"BatchMode=yes".to_string()));
        let r_pos = args.iter().position(|a| a == "-R").unwrap();
        assert_eq!(args[r_pos + 1], "40123:127.0.0.1:40123");
        assert!(args.contains(&"ExitOnForwardFailure=yes".to_string()));
        assert_eq!(args[args.len() - 2], "gpubox");
        let remote = &args[args.len() - 1];
        assert!(remote.starts_with("sh -c '"), "{remote}");
        assert!(remote.contains("'/opt/c h/chrome'"), "{remote}");
        assert!(remote.contains("'--remote-debugging-pipe'"));
        assert_eq!(shell_quote("it's"), "'it'\\''s'");

        // The launcher never carries the daemon's secrets: env is cleared
        // and only the whitelist (+ the ssh agent socket) is set.
        std::env::set_var("MU_VERIFY_TEST_API_KEY", "hunter2");
        std::env::set_var("SSH_AUTH_SOCK", "/tmp/fake-agent.sock");
        let cmd = r.command("/tmp/p");
        let std_cmd = cmd.as_std();
        let envs: Vec<(String, Option<String>)> = std_cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().to_string(),
                    v.map(|v| v.to_string_lossy().to_string()),
                )
            })
            .collect();
        std::env::remove_var("MU_VERIFY_TEST_API_KEY");
        std::env::remove_var("SSH_AUTH_SOCK");
        assert!(
            !envs.iter().any(|(k, _)| k == "MU_VERIFY_TEST_API_KEY"),
            "{envs:?}"
        );
        assert!(
            envs.iter()
                .any(|(k, v)| k == "SSH_AUTH_SOCK" && v.as_deref() == Some("/tmp/fake-agent.sock")),
            "{envs:?}"
        );
        assert!(envs.iter().any(|(k, _)| k == "PATH"), "{envs:?}");
    }

    #[tokio::test]
    async fn roundtrip_events_and_errors_through_the_pipe_wrapper() {
        let (_dir, fake) = write_fake_chrome();
        let launcher = Launcher {
            chrome: fake,
            ssh: None,
            extra_args: Vec::new(),
            forward_port: None,
        };
        let mut cdp = Cdp::launch(&launcher).await.unwrap();
        let session = cdp.open_page().await.unwrap();
        assert_eq!(session, "S1");
        let nav = cdp
            .call(
                Some(&session),
                "Page.navigate",
                json!({"url": "http://127.0.0.1:1/game.html"}),
            )
            .await
            .unwrap();
        assert_eq!(nav["frameId"], "F1");
        // Events queued by the navigate arrive in order; the load event last.
        let mut methods = Vec::new();
        while let Some(ev) = cdp.next_event(Duration::from_secs(5)).await {
            let m = ev["method"].as_str().unwrap_or("").to_string();
            let done = m == "Page.loadEventFired";
            methods.push(m);
            if done {
                break;
            }
        }
        assert_eq!(
            methods,
            vec![
                "Runtime.consoleAPICalled",
                "Runtime.exceptionThrown",
                "Runtime.consoleAPICalled",
                "Log.entryAdded",
                "Page.loadEventFired"
            ]
        );
        let err = cdp.call(None, "Fail.Please", json!({})).await.unwrap_err();
        assert!(err.to_string().contains("wasn't found"), "{err}");
        let shot = cdp
            .call(
                Some(&session),
                "Page.captureScreenshot",
                json!({"format": "png"}),
            )
            .await
            .unwrap();
        assert!(shot["data"].as_str().unwrap().len() > 20);
        cdp.close().await;
    }

    /// Dropping the client without `close()` (the cancel path) still ends
    /// the browser and the wrapper's trap removes the profile directory;
    /// a failed send leaves no stale waiter behind.
    #[tokio::test]
    async fn drop_without_close_cleans_up_and_failed_sends_leave_no_waiters() {
        let (_dir, fake) = write_fake_chrome();
        let launcher = Launcher {
            chrome: fake,
            ssh: None,
            extra_args: Vec::new(),
            forward_port: None,
        };
        let mut cdp = Cdp::launch(&launcher).await.unwrap();
        let session = cdp.open_page().await.unwrap();
        assert_eq!(session, "S1");
        let profile = cdp.profile.clone();
        assert!(
            std::path::Path::new(&profile).is_dir(),
            "fake chrome creates its profile dir once it is up"
        );
        // The cancel path: drop without close(). The pipe closes, the fake
        // exits at EOF, the wrapper's trap removes the profile.
        drop(cdp);
        for _ in 0..150 {
            if !std::path::Path::new(&profile).exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !std::path::Path::new(&profile).exists(),
            "wrapper trap must remove the profile dir"
        );

        // Stale-waiter check: a dead pipe fails the send and forgets the id.
        let dead = Launcher {
            chrome: PathBuf::from("/no/such/chrome-binary"),
            ssh: None,
            extra_args: Vec::new(),
            forward_port: None,
        };
        let mut cdp = Cdp::launch(&dead).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = cdp.call(None, "Target.createTarget", json!({})).await;
        assert_eq!(
            cdp.pending_count(),
            0,
            "failed sends must not leave waiters"
        );
        assert_eq!(cdp.dropped_events(), 0);
        drop(cdp); // Drop impl path, no close(): must not hang or panic.
    }

    #[tokio::test]
    async fn missing_chrome_surfaces_as_pipe_closed() {
        let launcher = Launcher {
            chrome: PathBuf::from("/no/such/chrome-binary"),
            ssh: None,
            extra_args: Vec::new(),
            forward_port: None,
        };
        let mut cdp = Cdp::launch(&launcher).await.unwrap();
        let err = cdp.open_page().await.unwrap_err().to_string();
        assert!(
            err.contains("chrome exited") || err.contains("pipe closed"),
            "{err}"
        );
        cdp.close().await;
    }
}
