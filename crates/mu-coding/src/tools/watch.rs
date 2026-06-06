//! mu-watch-tool-wakeup-o03p: the `watch` daemon tool.
//!
//! A turn-based model is inert between turns — only the daemon can wake
//! it. `schedule_wakeup` (mu-036 Phase C) is the TIMER primitive: it
//! parks an autonomous run and resumes at iteration N+1 after a
//! wall-clock delay. `watch` is its EVENT sibling: the model registers a
//! command, the tool returns IMMEDIATELY, and a detached task wakes the
//! session the moment that command exits — feeding the exit status +
//! output tail back as the next turn's motivation.
//!
//! Canonical use:
//!   watch("gh pr checks 42 --watch", "CI for PR 42")   // then end turn
//! The model ends its turn; later, CI finishes; the loop wakes with the
//! result and can act (e.g. merge) without the operator re-prompting it.
//!
//! Wakeup channel (NOT a parallel bespoke path): the task sends
//! `AgentInput::WatchCompleted` over the session's existing input channel
//! — the same `mpsc::Sender<AgentInput>` `schedule_wakeup` and
//! `mailbox.post` use (spec mu-036 line 59, "the agent loop's wakeup
//! channel"). The loop's idle `input_rx.recv().await` unblocks and the
//! result lands as a synthesized user message.
//!
//! Lifecycle (bead requirements):
//!   - Session-scoped: each command runs with `kill_on_drop(true)`; the
//!     per-session registry of task `AbortHandle`s is held by the tool, so
//!     when the session ends (its tool list drops) `Drop` aborts every
//!     live watch, dropping each task's `Child` and SIGKILLing the
//!     process. No orphans (mu-xac orphan-popen hang is the cautionary
//!     bead).
//!   - Capped: at most [`MAX_CONCURRENT_WATCHES`] live watches per session.
//!   - Timeout with a killed-status wakeup: a watch that hits its timeout
//!     is killed but STILL wakes the model with a "timed out" summary, so
//!     silence is impossible — a dead watch is otherwise indistinguishable
//!     from one still running.
//!
//! FreeBSD note: `tokio::process::Child::wait_with_output()` is
//! `kqueue` `EVFILT_PROC`/`NOTE_EXIT` under the hood here, so awaiting a
//! child exit needs no manual `kevent` bookkeeping (bead platform note).

use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mu_core::agent::{AgentInput, Tool, ToolResult, ToolSpec};
use serde_json::{json, Value};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio::task::AbortHandle;

use crate::serve::WeakSessions;

/// Max live watches per session. A turn that fans out more than this is
/// almost certainly a mistake (and risks a fork-bomb of background
/// processes); the cap makes the failure legible instead of silent.
pub const MAX_CONCURRENT_WATCHES: usize = 8;

/// Default per-watch timeout when the model doesn't specify one. Matches
/// `spawn_worker`'s default; long enough for CI / build watches, bounded
/// so a hung command can't pin a background slot forever.
const DEFAULT_TIMEOUT_SECS: u64 = 3600;

/// Cap on the combined stdout+stderr tail injected back into the
/// session. Keeps a chatty command from blowing up the next prompt;
/// finer tier-1 filtering (mu-2e0h) is a follow-up, tracked separately.
const OUTPUT_TAIL_BYTES: usize = 4000;

/// Per-session registry of live watch tasks. Holds each background task's
/// [`AbortHandle`] so the tool can (a) enforce the concurrency cap and
/// (b) abort every live watch on session teardown — which drops each
/// task's `kill_on_drop` `Child` and kills the process.
type WatchRegistry = Arc<Mutex<Vec<AbortHandle>>>;

pub struct WatchTool {
    /// Non-owning handle to the registry (mu-qc08): a strong clone would
    /// keep alive the map holding THIS session's own `input_tx`,
    /// deadlocking shutdown (the loop can't exit until `input_tx` drops,
    /// but the loop's own tool would keep it alive). Upgraded transiently
    /// when a watch fires.
    sessions: WeakSessions,
    /// The session that owns this tool — the one a finished watch wakes.
    parent_session_id: String,
    registry: WatchRegistry,
}

impl WatchTool {
    pub fn new(sessions: WeakSessions, parent_session_id: String) -> Self {
        Self {
            sessions,
            parent_session_id,
            registry: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Atomically prune finished watches, enforce the concurrency cap,
    /// and register `handle` if there's room. Returns `Err` (without
    /// registering) when the cap is reached — the caller then aborts the
    /// just-spawned task so its child is killed rather than orphaned.
    fn reserve_slot(&self, handle: AbortHandle) -> Result<(), String> {
        let mut live = self
            .registry
            .lock()
            .map_err(|_| "watch: registry lock poisoned".to_string())?;
        live.retain(|h| !h.is_finished());
        if live.len() >= MAX_CONCURRENT_WATCHES {
            return Err(format!(
                "watch: this session already has {MAX_CONCURRENT_WATCHES} live watches \
                 (the per-session cap); let one finish before registering another."
            ));
        }
        live.push(handle);
        Ok(())
    }
}

impl Drop for WatchTool {
    fn drop(&mut self) {
        // Session teardown: abort every live watch. Aborting drops the
        // task, dropping its `kill_on_drop` `Child`, which SIGKILLs the
        // spawned process — no orphans survive the session (mu-xac).
        if let Ok(mut live) = self.registry.lock() {
            for h in live.drain(..) {
                h.abort();
            }
        }
    }
}

/// Spawn `command` under `sh -c`, with `kill_on_drop` so a dropped
/// (aborted / timed-out) task kills the process. Synchronous: surfaces
/// spawn failures to the caller immediately, before the watch is
/// registered.
fn spawn_command(command: &str) -> std::io::Result<Child> {
    Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
}

/// Await `child` to exit (or `timeout_secs` to elapse) and render a
/// human-readable summary. On timeout the future returns and `child` is
/// dropped — `kill_on_drop` then kills the process — and the summary
/// says so, so the watch still wakes the model (silence is impossible).
async fn wait_and_summarize(child: Child, timeout_secs: u64) -> String {
    match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output()).await {
        Ok(Ok(output)) => format_output(&output),
        Ok(Err(e)) => format!("watch: error waiting on command: {e}"),
        Err(_elapsed) => format!(
            "Exit status: TIMED OUT after {timeout_secs}s — the command was killed. \
             It did not finish on its own."
        ),
    }
}

/// Render exit status + a bounded combined-output tail.
fn format_output(output: &std::process::Output) -> String {
    let status_line = match output.status.code() {
        Some(c) => format!("Exit status: {c}"),
        None => "Exit status: terminated by signal".to_string(),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut combined = String::new();
    if !stdout.trim().is_empty() {
        combined.push_str("stdout:\n");
        combined.push_str(stdout.trim_end());
        combined.push('\n');
    }
    if !stderr.trim().is_empty() {
        combined.push_str("stderr:\n");
        combined.push_str(stderr.trim_end());
        combined.push('\n');
    }
    let tail = tail_bytes(&combined, OUTPUT_TAIL_BYTES);
    if tail.trim().is_empty() {
        status_line
    } else {
        format!("{status_line}\n{tail}")
    }
}

/// Keep the last `max` bytes of `s`, on a char boundary, prefixed with a
/// truncation marker when bytes were dropped.
fn tail_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!(
        "...(output truncated to last {max} bytes)...\n{}",
        &s[start..]
    )
}

impl Tool for WatchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "watch",
            "Run a command in the background and wake this session with its result when it \
             exits. Returns immediately ('watch registered') — END YOUR TURN after calling it; \
             you'll get a new turn with the exit status and output tail once the command \
             finishes. Use it to wait on slow external events without burning model budget \
             idling, e.g. watch('gh pr checks 42 --watch', 'CI for PR 42'). A watch that hits \
             its timeout is killed but still wakes you (with a TIMED OUT status), so you are \
             never left waiting on silence.",
            json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command to run (executed via `sh -c`)."
                    },
                    "note": {
                        "type": "string",
                        "description": "Short label for what you're waiting on; echoed back \
                                        in the wakeup so you remember why."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Kill the command and wake with a TIMED OUT status after \
                                        this many seconds (default 3600)."
                    }
                },
                "required": ["command", "note"]
            }),
        )
    }

    fn execute<'life0, 'async_trait>(
        &'life0 self,
        arguments: Value,
        _cancel_rx: oneshot::Receiver<()>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        // Parse + register synchronously (no `.await` before the tool
        // returns): `watch` is fire-and-forget, so the result comes back
        // via the wakeup channel, not this return value. `_cancel_rx`
        // cancels only this (instant) registration, not the watch itself
        // — a registered watch is torn down via session teardown (Drop)
        // or its own timeout, per the lifecycle contract.
        let command = arguments
            .get("command")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let note = arguments
            .get("note")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let timeout_secs = arguments
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_SECS);

        let result = (|| {
            let command = command.ok_or("watch: missing required argument: command")?;
            let note = note.ok_or("watch: missing required argument: note")?;

            // Spawn first so a bad command is reported NOW (the model can
            // fix it this turn) rather than only via a delayed wakeup.
            let child = spawn_command(&command)
                .map_err(|e| format!("watch: failed to spawn command: {e}"))?;

            let weak = self.sessions.clone();
            let parent = self.parent_session_id.clone();
            let wake_note = note.clone();
            let task = tokio::spawn(async move {
                let summary = wait_and_summarize(child, timeout_secs).await;
                // Wake the calling session over its input channel. If the
                // registry / session is gone (daemon shutdown, session
                // ended), the send is a clean no-op — never a panic.
                if let Some(sessions) = weak.upgrade() {
                    if let Some(tx) = sessions.input_sender(&parent) {
                        let _ = tx
                            .send(AgentInput::WatchCompleted {
                                note: wake_note,
                                summary,
                            })
                            .await;
                    }
                }
            });

            // Race-free cap enforcement: if we're over the cap, abort the
            // task we just spawned — which drops the `kill_on_drop` child.
            if let Err(e) = self.reserve_slot(task.abort_handle()) {
                task.abort();
                return Err(e);
            }

            Ok(format!(
                "Watch registered: '{note}' — running `{command}` in the background. \
                 End your turn; you'll be woken with the exit status and output tail when it \
                 finishes (or after {timeout_secs}s, killed, with a TIMED OUT status)."
            ))
        })();

        Box::pin(async move {
            match result {
                Ok(content) => ToolResult {
                    content,
                    is_error: false,
                },
                Err(e) => ToolResult {
                    content: e,
                    is_error: true,
                },
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::Sessions;
    use serde_json::json;

    fn tool() -> WatchTool {
        // Tests of execute/reserve don't need a live registry — a dead
        // weak from the dropped temporary is fine (wakeups become no-ops).
        WatchTool::new(Sessions::new().downgrade(), "session-1".to_string())
    }

    #[tokio::test]
    async fn summarize_echo_reports_exit_zero_and_output() {
        let child = spawn_command("echo hello-watch").expect("spawn echo");
        let summary = wait_and_summarize(child, 30).await;
        assert!(summary.contains("Exit status: 0"), "summary: {summary}");
        assert!(summary.contains("hello-watch"), "summary: {summary}");
    }

    #[tokio::test]
    async fn summarize_nonzero_exit_reports_code() {
        let child = spawn_command("exit 3").expect("spawn");
        let summary = wait_and_summarize(child, 30).await;
        assert!(summary.contains("Exit status: 3"), "summary: {summary}");
    }

    #[tokio::test]
    async fn summarize_stderr_is_captured() {
        let child = spawn_command("echo oops 1>&2; exit 1").expect("spawn");
        let summary = wait_and_summarize(child, 30).await;
        assert!(summary.contains("Exit status: 1"), "summary: {summary}");
        assert!(summary.contains("oops"), "stderr captured: {summary}");
    }

    #[tokio::test]
    async fn timeout_kills_and_still_summarizes() {
        // A long sleeper with a sub-second timeout: the watch must NOT
        // hang — it returns a TIMED OUT summary (and the child is killed
        // when `child` drops). Silence is impossible.
        let child = spawn_command("sleep 30").expect("spawn sleep");
        let summary = tokio::time::timeout(Duration::from_secs(5), wait_and_summarize(child, 1))
            .await
            .expect("wait_and_summarize must return well before 5s");
        assert!(summary.contains("TIMED OUT"), "summary: {summary}");
    }

    #[tokio::test]
    async fn missing_command_is_error() {
        let (tx, rx) = oneshot::channel();
        let res = tool().execute(json!({ "note": "x" }), rx).await;
        drop(tx);
        assert!(res.is_error, "missing command must error");
        assert!(res.content.contains("command"), "{}", res.content);
    }

    #[tokio::test]
    async fn registers_and_reports_back() {
        let (_tx, rx) = oneshot::channel();
        let res = tool()
            .execute(json!({ "command": "true", "note": "smoke" }), rx)
            .await;
        assert!(
            !res.is_error,
            "valid watch should register: {}",
            res.content
        );
        assert!(res.content.contains("Watch registered"), "{}", res.content);
    }

    #[tokio::test]
    async fn concurrency_cap_rejects_overflow() {
        let t = tool();
        // Fill the registry with MAX never-finishing tasks.
        for _ in 0..MAX_CONCURRENT_WATCHES {
            let task = tokio::spawn(async { std::future::pending::<()>().await });
            t.reserve_slot(task.abort_handle()).expect("under cap");
        }
        // The next reservation is over the cap and must be rejected.
        let extra = tokio::spawn(async { std::future::pending::<()>().await });
        let err = t
            .reserve_slot(extra.abort_handle())
            .expect_err("over cap must reject");
        assert!(err.contains("cap"), "{err}");
    }

    #[tokio::test]
    async fn reserve_slot_prunes_finished_watches() {
        let t = tool();
        // Register more than the cap's worth of immediately-finishing
        // watches, one at a time: because each completes (and is pruned)
        // before the next reservation, the cap is never hit.
        for _ in 0..(MAX_CONCURRENT_WATCHES * 2) {
            let handle = tokio::spawn(async {}).abort_handle();
            // Let the empty task finish so the NEXT reserve_slot prunes it.
            tokio::task::yield_now().await;
            t.reserve_slot(handle)
                .expect("finished handles are pruned before the cap check");
        }
    }
}
