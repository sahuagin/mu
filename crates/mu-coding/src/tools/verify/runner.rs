//! `node` / `python` runner — a bounded wrapper over `tokio::process`
//! (mu-lg8j1 phase 1). The discipline of a strict-mode `bash`: timeout,
//! output cap, stdin null, kill on timeout, and the SAME scrubbed
//! environment (`bash::scrubbed_env_vars`: the whitelist minus
//! secret-pattern names) — the artifact is model-written code and must
//! never see the daemon's API keys. Streams are drained concurrently so
//! a chatty script can never block on a full pipe (the memory_hints
//! deadlock, mu-pcvqx), and draining continues past the cap so the cap
//! bounds what the MODEL sees, not what the child may write.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

/// Per-stream cap on bytes returned to the model.
pub const STREAM_CAP_BYTES: usize = 24 * 1024;

#[derive(Debug, Default, Clone)]
pub struct RunOutcome {
    /// `None` ⇒ killed by a signal, or by us on timeout.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    /// The child exited but something it spawned kept stdout/stderr open
    /// past the grace period; the readers were aborted and late output
    /// discarded.
    pub streams_held_open: bool,
    pub elapsed: Duration,
}

/// A reader task that is ABORTED when dropped — on the join grace
/// timeout, and on every other drop path (the tool call cancelled, the
/// future dropped mid-await). Dropping a bare `JoinHandle` only detaches
/// the task, and `read_capped` returns only at EOF, which a grandchild
/// holding the pipe can postpone indefinitely (review panel finding).
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl RunOutcome {
    pub fn passed(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}

/// Run `bin script args…` in `cwd`, bounded by `timeout`.
pub async fn run_script(
    bin: &Path,
    script: &Path,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
) -> anyhow::Result<RunOutcome> {
    let mut cmd = Command::new(bin);
    cmd.arg(script)
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(crate::tools::bash::scrubbed_env_vars())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group, so the timeout kill reaches everything the
        // artifact spawned (a `sleep 30 &`, forked workers), not just the
        // direct runtime — bounded execution means the whole tree.
        .process_group(0)
        .kill_on_drop(true);
    let start = Instant::now();
    let mut child = cmd.spawn().map_err(|e| {
        anyhow::anyhow!(
            "cannot spawn runtime '{}': {e} (set [verify].node / [verify].python)",
            bin.display()
        )
    })?;
    let mut out_task = AbortOnDrop(tokio::spawn(read_capped(child.stdout.take())));
    let mut err_task = AbortOnDrop(tokio::spawn(read_capped(child.stderr.take())));

    let pid = child.id();
    // Group kill on EVERY exit path, including the future being dropped
    // (the tool call cancelled, the session torn down): `kill_on_drop`
    // reaches only the direct runtime, the guard reaches what it spawned.
    let mut group = GroupKillOnDrop(pid);
    let (exit_code, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            // The runtime exited; anything it left behind in the group
            // (a backgrounded helper) goes with it — immediately after
            // the reap, while the group id is still ours.
            group.fire();
            (status.code(), false)
        }
        Ok(Err(e)) => return Err(anyhow::anyhow!("waiting on runtime: {e}")),
        Err(_) => {
            group.fire();
            let _ = child.kill().await;
            (None, true)
        }
    };
    // Readers see EOF once the child (and anything it spawned that held
    // the pipes) is gone; bound the wait so a grandchild cannot stall us,
    // and abort the reader when the bound expires (the guard does it).
    let (stdout, stdout_truncated, out_held) = join_capped(&mut out_task).await;
    let (stderr, stderr_truncated, err_held) = join_capped(&mut err_task).await;
    Ok(RunOutcome {
        exit_code,
        timed_out,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        streams_held_open: out_held || err_held,
        elapsed: start.elapsed(),
    })
}

/// Grace period for a reader to reach EOF after the child is gone.
const READER_GRACE: Duration = Duration::from_secs(3);

/// Kills the process group when dropped — the cancel/drop path's
/// counterpart to the explicit kills on the wait arms. Killing an
/// already-dead group is a harmless ESRCH.
/// One-shot: [`GroupKillOnDrop::fire`] kills and DISARMS, so the drop
/// never re-signals a group id that may have been recycled after the
/// leader was reaped.
struct GroupKillOnDrop(Option<u32>);

impl GroupKillOnDrop {
    fn fire(&mut self) {
        kill_process_group(self.0.take());
    }
}

impl Drop for GroupKillOnDrop {
    fn drop(&mut self) {
        self.fire();
    }
}

/// SIGKILL every process in the group `pid` leads (the child was spawned
/// with `process_group(0)`, so its pid is the pgid). Goes through the
/// `kill` utility rather than `libc::kill` because this crate forbids
/// `unsafe`; a negative pid addresses the whole group. No-op when the
/// child never started or already vanished.
pub(crate) fn kill_process_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        let _ = std::process::Command::new("kill")
            .args(["-9", "--", &format!("-{pid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// `(output, truncated, held_open)`. On the grace timeout the reader is
/// aborted (dropping its read end) and `held_open` is true.
async fn join_capped(task: &mut AbortOnDrop<(String, bool)>) -> (String, bool, bool) {
    match tokio::time::timeout(READER_GRACE, &mut task.0).await {
        Ok(Ok((s, t))) => (s, t, false),
        Ok(Err(_)) => (String::new(), false, false),
        Err(_) => {
            task.0.abort();
            (String::new(), false, true)
        }
    }
}

/// Read a stream to EOF, keeping the first [`STREAM_CAP_BYTES`] and
/// draining (discarding) the rest.
async fn read_capped<R: AsyncRead + Unpin>(stream: Option<R>) -> (String, bool) {
    let Some(mut stream) = stream else {
        return (String::new(), false);
    };
    let mut kept: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if kept.len() < STREAM_CAP_BYTES {
                    let room = STREAM_CAP_BYTES - kept.len();
                    if n > room {
                        kept.extend_from_slice(&buf[..room]);
                        truncated = true;
                    } else {
                        kept.extend_from_slice(&buf[..n]);
                    }
                } else {
                    truncated = true;
                }
            }
        }
    }
    (String::from_utf8_lossy(&kept).into_owned(), truncated)
}

/// Render the model-facing report. First line is the verdict.
pub fn format_report(kind: &str, artifact: &Path, o: &RunOutcome) -> String {
    let verdict = if o.passed() { "PASS" } else { "FAIL" };
    let status = if o.timed_out {
        "timed out (killed)".to_string()
    } else {
        match o.exit_code {
            Some(c) => format!("exit {c}"),
            None => "killed by signal".to_string(),
        }
    };
    let mut out = format!(
        "VERIFY {kind} {verdict} — {status} in {:.2}s — {}\n",
        o.elapsed.as_secs_f64(),
        artifact.display()
    );
    if o.streams_held_open {
        out.push_str(
            "note: the script exited but a process it started kept stdout/stderr open; \
             the streams were released after 3s and late output was discarded\n",
        );
    }
    for (name, body, truncated) in [
        ("stdout", &o.stdout, o.stdout_truncated),
        ("stderr", &o.stderr, o.stderr_truncated),
    ] {
        if body.is_empty() {
            out.push_str(&format!("--- {name}: (empty) ---\n"));
        } else {
            out.push_str(&format!(
                "--- {name} ({} bytes{}) ---\n",
                body.len(),
                if truncated { ", truncated" } else { "" }
            ));
            out.push_str(body);
            if !body.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn stub(name: &str, body: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(name);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, p)
    }

    #[tokio::test]
    async fn runs_script_and_captures_streams_and_exit() {
        // "runtime" = sh; the "script" is its argument.
        let (dir, script) = stub("app.js", "#!/bin/sh\necho out-$1\necho err >&2\nexit 3\n");
        let o = run_script(
            Path::new("/bin/sh"),
            &script,
            &["A".to_string()],
            dir.path(),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(o.exit_code, Some(3));
        assert!(!o.timed_out);
        assert_eq!(o.stdout.trim(), "out-A");
        assert_eq!(o.stderr.trim(), "err");
        let report = format_report("node", &script, &o);
        assert!(report.starts_with("VERIFY node FAIL — exit 3"), "{report}");
        assert!(report.contains("--- stdout (6 bytes) ---\nout-A"));
        let mut ok = o.clone();
        ok.exit_code = Some(0);
        assert!(format_report("node", &script, &ok).starts_with("VERIFY node PASS"));
    }

    #[tokio::test]
    async fn timeout_kills_and_reports() {
        let (dir, script) = stub("slow.py", "#!/bin/sh\necho started\nsleep 30\n");
        let start = Instant::now();
        let o = run_script(
            Path::new("/bin/sh"),
            &script,
            &[],
            dir.path(),
            Duration::from_millis(500),
        )
        .await
        .unwrap();
        assert!(o.timed_out);
        assert!(start.elapsed() < Duration::from_secs(10));
        assert!(format_report("python", &script, &o).contains("FAIL — timed out"));
    }

    #[tokio::test]
    async fn output_is_capped_but_child_is_drained() {
        // 1 MB of output: far past the cap and past any pipe buffer.
        let (dir, script) = stub(
            "big.js",
            "#!/bin/sh\nhead -c 1048576 /dev/zero | tr '\\0' 'x'\nexit 0\n",
        );
        let o = run_script(
            Path::new("/bin/sh"),
            &script,
            &[],
            dir.path(),
            Duration::from_secs(20),
        )
        .await
        .unwrap();
        assert!(o.passed(), "{o:?}");
        assert!(o.stdout_truncated);
        assert_eq!(o.stdout.len(), STREAM_CAP_BYTES);
    }

    /// A grandchild holding the pipes must not stall the call or leak a
    /// reader. With the runtime in its own process group, the helper the
    /// script backgrounded dies with the group the moment the runtime
    /// exits, so the streams close at once and nothing is "held open".
    #[tokio::test]
    async fn grandchild_holding_pipes_is_released_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("helper.pid");
        let script = dir.path().join("bg.js");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho started\nsleep 30 &\necho $! > {}\nexit 0\n",
                pidfile.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let start = Instant::now();
        let o = run_script(
            Path::new("/bin/sh"),
            &script,
            &[],
            dir.path(),
            Duration::from_secs(20),
        )
        .await
        .unwrap();
        assert!(o.passed(), "{o:?}");
        assert_eq!(o.stdout.trim(), "started");
        assert!(!o.streams_held_open, "{o:?}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        let helper: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let alive = std::process::Command::new("kill")
            .args(["-0", &helper.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(
            !alive,
            "backgrounded helper {helper} must die with the group"
        );
        // The held-open path still renders when it does happen (a helper
        // that escaped the group).
        let held = RunOutcome {
            exit_code: Some(0),
            streams_held_open: true,
            ..RunOutcome::default()
        };
        assert!(format_report("node", &script, &held).contains("kept stdout/stderr open"));
        // Drop-path abort: a live reader guard aborts its task on drop.
        let guard = AbortOnDrop(tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            (String::new(), false)
        }));
        let handle = guard.0.abort_handle();
        drop(guard);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            handle.is_finished(),
            "dropping the guard must abort the task"
        );
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("helper.pid");
        let body = format!(
            "#!/bin/sh\nsleep 60 &\necho $! > {}\nsleep 60\n",
            pidfile.display()
        );
        let script = dir.path().join("hang.js");
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let o = run_script(
            Path::new("/bin/sh"),
            &script,
            &[],
            dir.path(),
            Duration::from_millis(800),
        )
        .await
        .unwrap();
        assert!(o.timed_out);
        let helper: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // Give the kernel a moment to deliver SIGKILL to the group.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let alive = std::process::Command::new("kill")
            .args(["-0", &helper.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(
            !alive,
            "backgrounded helper {helper} must die with the group"
        );
    }

    /// Dropping the in-flight future (the tool call cancelled) still kills
    /// the artifact's whole group, not just the runtime.
    #[tokio::test]
    async fn dropping_the_run_kills_the_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("helper.pid");
        let script = dir.path().join("hang.js");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nsleep 60 &\necho $! > {}\nsleep 60\n",
                pidfile.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let dir_path = dir.path().to_path_buf();
        let script2 = script.clone();
        let task = tokio::spawn(async move {
            run_script(
                Path::new("/bin/sh"),
                &script2,
                &[],
                &dir_path,
                Duration::from_secs(60),
            )
            .await
        });
        for _ in 0..100 {
            if pidfile.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let helper: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        task.abort(); // drops the run_script future mid-wait
        let _ = task.await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let alive = std::process::Command::new("kill")
            .args(["-0", &helper.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(!alive, "helper {helper} must die when the run is dropped");
    }

    #[tokio::test]
    async fn missing_runtime_is_an_error() {
        let (dir, script) = stub("x.js", "");
        let e = run_script(
            Path::new("/no/such/runtime"),
            &script,
            &[],
            dir.path(),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(e.to_string().contains("cannot spawn runtime"));
    }
}
