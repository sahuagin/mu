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
use tokio::process::Child;
use tokio::sync::oneshot;
use tokio::task::AbortHandle;

use crate::serve::WeakSessions;
use crate::tools::{bash, BashMode};

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

/// One live watch: the background task's handle plus the registration
/// identity (`command` + `note`) used for duplicate detection (mu-spk7).
struct LiveWatch {
    handle: AbortHandle,
    command: String,
    note: String,
}

/// Per-session registry of live watch tasks. Holds each background task's
/// [`AbortHandle`] so the tool can (a) enforce the concurrency cap,
/// (b) abort every live watch on session teardown — which drops each
/// task's `kill_on_drop` `Child` and kills the process — and (c) refuse
/// duplicate registrations while the original is still running.
type WatchRegistry = Arc<Mutex<Vec<LiveWatch>>>;

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
    /// mu-qnag: the daemon's command-execution policy. Every watched
    /// command is gated through this — the SAME [`BashMode`] the `bash`
    /// tool uses — so a session's watch authority matches its bash
    /// authority. A strict session (no `--bash-*` flags) rejects anything
    /// outside the read-only allowlist; yolo runs anything.
    bash_mode: BashMode,
}

impl WatchTool {
    pub fn new(sessions: WeakSessions, parent_session_id: String, bash_mode: BashMode) -> Self {
        Self {
            sessions,
            parent_session_id,
            registry: Arc::new(Mutex::new(Vec::new())),
            bash_mode,
        }
    }

    /// Atomically prune finished watches, enforce the concurrency cap,
    /// and register `entry` if there's room. Returns `Err` (without
    /// registering) when the cap is reached — the caller then aborts the
    /// just-spawned task so its child is killed rather than orphaned.
    fn reserve_slot(&self, entry: LiveWatch) -> Result<(), String> {
        let mut live = self
            .registry
            .lock()
            .map_err(|_| "watch: registry lock poisoned".to_string())?;
        live.retain(|w| !w.handle.is_finished());
        if live.len() >= MAX_CONCURRENT_WATCHES {
            return Err(format!(
                "watch: this session already has {MAX_CONCURRENT_WATCHES} live watches \
                 (the per-session cap); let one finish before registering another."
            ));
        }
        live.push(entry);
        Ok(())
    }

    /// mu-spk7: is an identical registration (same `command` AND `note`)
    /// still live? Weak models answer the "Watch registered — end your
    /// turn" tool result by re-issuing the very same call; each duplicate
    /// used to spawn a second process and deliver a second wakeup.
    /// Finished entries are pruned first, so a COMPLETED watch stays
    /// re-registerable — only a still-running twin is a duplicate.
    fn has_live_duplicate(&self, command: &str, note: &str) -> bool {
        match self.registry.lock() {
            Ok(mut live) => {
                live.retain(|w| !w.handle.is_finished());
                live.iter().any(|w| w.command == command && w.note == note)
            }
            Err(_) => false,
        }
    }
}

impl Drop for WatchTool {
    fn drop(&mut self) {
        // Session teardown: abort every live watch. Aborting drops the
        // task, dropping its `kill_on_drop` `Child`, which SIGKILLs the
        // spawned process — no orphans survive the session (mu-xac).
        if let Ok(mut live) = self.registry.lock() {
            for w in live.drain(..) {
                w.handle.abort();
            }
        }
    }
}

/// Build the watched child through the SHARED bash gate (mu-qnag) and
/// spawn it, with `kill_on_drop` so a dropped (aborted / timed-out) task
/// kills the process. The command is validated against `mode` FIRST: a
/// strict session rejects anything outside its allowlist with bash's own
/// message, BEFORE any process starts — so a rejected command surfaces an
/// error THIS turn and never parks the session on a wakeup that won't
/// come. Returns the rejection (or a spawn failure) as a `String` so the
/// caller can surface it directly.
fn spawn_command(mode: &BashMode, command: &str) -> Result<Child, String> {
    let mut cmd = bash::build_command(mode, command)?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("watch: failed to spawn command: {e}"))
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
        // mu-qnag: watch inherits the FULL bash command policy, including the
        // `--bash-prompt` approval posture. When the daemon's BashMode is
        // Strict { prompt: true }, `bash` advertises PermissionLevel::Ask
        // (per-call approval) for allowlisted commands — watch must too, or
        // watch authority would still exceed bash authority. Strict-without-
        // prompt and yolo run on Allow (the substantive gate is the allowlist
        // in `validate` / `build_command`, not holding the tool).
        let permission = match &self.bash_mode {
            BashMode::Strict { prompt: true, .. } => mu_core::agent::PermissionLevel::Ask,
            _ => mu_core::agent::PermissionLevel::Allow,
        };
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
                        "description": "Command to run. Gated by the SAME policy as the `bash` \
                                        tool: in a strict session only allowlisted (read-only) \
                                        commands run and shell metacharacters are rejected; a \
                                        `--bash-yolo` session runs anything via `bash -c`; a \
                                        `--bash-prompt` session requires per-call approval. A \
                                        rejected command errors immediately (it is not registered)."
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
        // mu-usfj / mu-qnag: watch runs a command — it is Execute, NOT the
        // defaulted ReadOnly. It no longer ships its own ungated `sh -c`:
        // every command is validated through the SHARED bash gate
        // (`validate` / `bash::build_command`), so a `watch("rm -rf x")` /
        // `watch("cargo test")` in a strict session is rejected by the exact
        // allowlist that gates `bash`, and a `--bash-prompt` session requires
        // the same per-call approval (`permission` above). `idempotent: false`
        // (the world changes between runs).
        .with_policy(mu_core::agent::ToolPolicy {
            side_effects: mu_core::agent::SideEffects::Execute,
            permission,
            retry: mu_core::agent::RetryPolicy::ModelDecides,
            required_aws_capability: None,
            idempotent: false,
        })
    }

    fn validate(&self, arguments: &Value) -> Result<(), String> {
        // mu-qnag: mirror bash's pre-flight (mu-bkjr). The dispatcher runs
        // `validate` BEFORE the PermissionLevel::Ask approval gate, so a
        // command the allowlist will reject fails immediately WITHOUT
        // prompting, and watch's command authority matches bash's exactly
        // (allowlist + metachars + the `--bash-prompt` approval posture).
        // `execute` re-gates via `build_command` for direct-call paths.
        let command = arguments
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| "watch: missing required argument: command".to_string())?;
        bash::validate_command(&self.bash_mode, command)
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

            // mu-spk7: idempotent registration — checked BEFORE spawn so a
            // duplicate never starts a second process or books a second
            // wakeup. Returned as a non-error so the model reads it as
            // state, not as a failure to retry differently.
            if self.has_live_duplicate(&command, &note) {
                return Ok(format!(
                    "Watch '{note}' is ALREADY registered and still running `{command}` — \
                     duplicate ignored, no second process started. End your turn; the \
                     existing watch will wake you with its result."
                ));
            }

            // Gate + spawn synchronously so a rejected (allowlist miss) or
            // un-spawnable command is reported NOW — the model can fix it
            // this turn, and a rejected watch is never registered, so the
            // session is never parked on a wakeup that won't come. mu-qnag.
            let child = spawn_command(&self.bash_mode, &command)?;

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
            let entry = LiveWatch {
                handle: task.abort_handle(),
                command: command.clone(),
                note: note.clone(),
            };
            if let Err(e) = self.reserve_slot(entry) {
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

    fn tool_with_mode(mode: BashMode) -> WatchTool {
        // Tests of execute/reserve don't need a live registry — a dead
        // weak from the dropped temporary is fine (wakeups become no-ops).
        WatchTool::new(Sessions::new().downgrade(), "session-1".to_string(), mode)
    }

    fn tool() -> WatchTool {
        // Default to yolo so the pre-mu-qnag execute/reserve tests (which
        // use shell features / non-allowlisted commands) keep their meaning.
        tool_with_mode(BashMode::Yolo)
    }

    #[tokio::test]
    async fn summarize_echo_reports_exit_zero_and_output() {
        let child = spawn_command(&BashMode::Yolo, "echo hello-watch").expect("spawn echo");
        let summary = wait_and_summarize(child, 30).await;
        assert!(summary.contains("Exit status: 0"), "summary: {summary}");
        assert!(summary.contains("hello-watch"), "summary: {summary}");
    }

    #[tokio::test]
    async fn summarize_nonzero_exit_reports_code() {
        let child = spawn_command(&BashMode::Yolo, "exit 3").expect("spawn");
        let summary = wait_and_summarize(child, 30).await;
        assert!(summary.contains("Exit status: 3"), "summary: {summary}");
    }

    #[tokio::test]
    async fn summarize_stderr_is_captured() {
        let child = spawn_command(&BashMode::Yolo, "echo oops 1>&2; exit 1").expect("spawn");
        let summary = wait_and_summarize(child, 30).await;
        assert!(summary.contains("Exit status: 1"), "summary: {summary}");
        assert!(summary.contains("oops"), "stderr captured: {summary}");
    }

    #[tokio::test]
    async fn timeout_kills_and_still_summarizes() {
        // A long sleeper with a sub-second timeout: the watch must NOT
        // hang — it returns a TIMED OUT summary (and the child is killed
        // when `child` drops). Silence is impossible.
        let child = spawn_command(&BashMode::Yolo, "sleep 30").expect("spawn sleep");
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

    fn entry(handle: AbortHandle, tag: usize) -> LiveWatch {
        LiveWatch {
            handle,
            command: format!("cmd-{tag}"),
            note: format!("note-{tag}"),
        }
    }

    #[tokio::test]
    async fn concurrency_cap_rejects_overflow() {
        let t = tool();
        // Fill the registry with MAX never-finishing tasks.
        for i in 0..MAX_CONCURRENT_WATCHES {
            let task = tokio::spawn(async { std::future::pending::<()>().await });
            t.reserve_slot(entry(task.abort_handle(), i))
                .expect("under cap");
        }
        // The next reservation is over the cap and must be rejected.
        let extra = tokio::spawn(async { std::future::pending::<()>().await });
        let err = t
            .reserve_slot(entry(extra.abort_handle(), 99))
            .expect_err("over cap must reject");
        assert!(err.contains("cap"), "{err}");
    }

    // ── mu-spk7: idempotent registration ──

    #[tokio::test]
    async fn duplicate_live_registration_is_ignored() {
        // The reported incident: the model answers "Watch registered —
        // end your turn" by re-issuing the identical call; the duplicate
        // used to spawn a second process and deliver a second wakeup.
        let t = tool();
        let (_tx1, rx1) = oneshot::channel();
        let res1 = t
            .execute(json!({ "command": "sleep 30", "note": "dup" }), rx1)
            .await;
        assert!(
            res1.content.contains("Watch registered"),
            "{}",
            res1.content
        );
        let (_tx2, rx2) = oneshot::channel();
        let res2 = t
            .execute(json!({ "command": "sleep 30", "note": "dup" }), rx2)
            .await;
        assert!(!res2.is_error, "duplicate is state, not an error");
        assert!(
            res2.content.contains("ALREADY registered"),
            "{}",
            res2.content
        );
        assert_eq!(
            t.registry.lock().unwrap().len(),
            1,
            "second registration must not add a live watch"
        );
    }

    #[tokio::test]
    async fn same_command_different_note_is_not_a_duplicate() {
        // Only an exact (command, note) twin is refused — a deliberate
        // second watch on the same command under a new label still runs.
        let t = tool();
        let (_tx1, rx1) = oneshot::channel();
        t.execute(json!({ "command": "sleep 30", "note": "first" }), rx1)
            .await;
        let (_tx2, rx2) = oneshot::channel();
        let res = t
            .execute(json!({ "command": "sleep 30", "note": "second" }), rx2)
            .await;
        assert!(res.content.contains("Watch registered"), "{}", res.content);
        assert_eq!(t.registry.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn finished_watch_can_be_reregistered() {
        // Pruning keeps a COMPLETED watch re-registerable: only a
        // still-running twin counts as a duplicate.
        let t = tool();
        let (_tx1, rx1) = oneshot::channel();
        let res = t
            .execute(json!({ "command": "true", "note": "again" }), rx1)
            .await;
        assert!(res.content.contains("Watch registered"), "{}", res.content);
        // Wait (bounded) for the background task to finish; the wake send
        // is a no-op against the test's dead registry.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while t.has_live_duplicate("true", "again") {
            assert!(
                std::time::Instant::now() < deadline,
                "watch task should finish well within 10s"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let (_tx2, rx2) = oneshot::channel();
        let res2 = t
            .execute(json!({ "command": "true", "note": "again" }), rx2)
            .await;
        assert!(
            res2.content.contains("Watch registered"),
            "finished watch must be re-registerable: {}",
            res2.content
        );
    }

    // ── mu-qnag: watch routes commands through the shared bash gate ──

    #[tokio::test]
    async fn strict_mode_rejects_non_allowlisted_command() {
        // The reported incident: a read-only reviewer (no `--bash-*` flags
        // ⇒ strict) ran `cargo test` via watch. The command must now be
        // rejected by the SAME allowlist that gates `bash`, BEFORE any
        // process spawns — so a rejected watch is never registered and the
        // turn is never parked.
        let t = tool_with_mode(BashMode::strict_with_extras(&[], false));
        let (_tx, rx) = oneshot::channel();
        let res = t
            .execute(
                json!({ "command": "cargo test --workspace", "note": "tests" }),
                rx,
            )
            .await;
        assert!(
            res.is_error,
            "strict watch must reject cargo test: {}",
            res.content
        );
        assert!(
            res.content.contains("not in the strict-mode allowlist"),
            "expected the bash allowlist message, got: {}",
            res.content
        );
        assert!(
            !res.content.contains("Watch registered"),
            "a rejected command must not be registered: {}",
            res.content
        );
    }

    #[tokio::test]
    async fn strict_mode_rejects_shell_metacharacters() {
        // The strict metachar gate applies to watch too — no allowlist
        // bypass via chaining/substitution/redirect.
        let t = tool_with_mode(BashMode::strict_with_extras(&[], false));
        let (_tx, rx) = oneshot::channel();
        let res = t
            .execute(json!({ "command": "echo hi; cargo test", "note": "x" }), rx)
            .await;
        assert!(res.is_error, "metachar must be rejected: {}", res.content);
        assert!(
            res.content.contains("metacharacter"),
            "expected metachar rejection, got: {}",
            res.content
        );
    }

    #[tokio::test]
    async fn strict_mode_allows_allowlisted_command() {
        // `echo` is in the default read-only allowlist, so a strict session
        // can still watch it — the gate is on WHAT runs, not on holding the
        // tool.
        let t = tool_with_mode(BashMode::strict_with_extras(&[], false));
        let (_tx, rx) = oneshot::channel();
        let res = t
            .execute(json!({ "command": "echo waiting", "note": "ok" }), rx)
            .await;
        assert!(
            !res.is_error,
            "allowlisted command should register: {}",
            res.content
        );
        assert!(res.content.contains("Watch registered"), "{}", res.content);
    }

    #[test]
    fn validate_preflight_gates_like_bash() {
        // mu-qnag: validate mirrors bash's mu-bkjr pre-flight so the
        // dispatcher rejects a doomed command BEFORE the approval gate.
        let t = tool_with_mode(BashMode::strict_with_extras(&[], false));
        let err = t
            .validate(&json!({ "command": "cargo test", "note": "x" }))
            .expect_err("strict validate must reject cargo test");
        assert!(err.contains("not in the strict-mode allowlist"), "{err}");
        assert!(t
            .validate(&json!({ "command": "echo ok", "note": "x" }))
            .is_ok());
        // Yolo validate passes anything.
        assert!(tool_with_mode(BashMode::Yolo)
            .validate(&json!({ "command": "cargo build", "note": "x" }))
            .is_ok());
    }

    #[test]
    fn permission_tracks_bash_prompt_posture() {
        // mu-qnag: in a --bash-prompt (Strict { prompt: true }) daemon, bash
        // requires per-call approval; watch must advertise the same Ask
        // posture instead of running allowlisted commands unapproved.
        use mu_core::agent::PermissionLevel;
        let ask = tool_with_mode(BashMode::strict_with_extras(&[], true));
        assert_eq!(ask.spec().policy.permission, PermissionLevel::Ask);
        let allow = tool_with_mode(BashMode::strict_with_extras(&[], false));
        assert_eq!(allow.spec().policy.permission, PermissionLevel::Allow);
        assert_eq!(
            tool_with_mode(BashMode::Yolo).spec().policy.permission,
            PermissionLevel::Allow
        );
    }

    #[tokio::test]
    async fn yolo_mode_allows_non_allowlisted_command() {
        // A `--bash-yolo` session (e.g. the orchestrator worker) keeps
        // running arbitrary commands via watch — unchanged by mu-qnag.
        let t = tool_with_mode(BashMode::Yolo);
        let (_tx, rx) = oneshot::channel();
        let res = t
            .execute(json!({ "command": "echo a | cat", "note": "pipe" }), rx)
            .await;
        assert!(
            !res.is_error,
            "yolo watch should allow anything: {}",
            res.content
        );
        assert!(res.content.contains("Watch registered"), "{}", res.content);
    }

    #[tokio::test]
    async fn reserve_slot_prunes_finished_watches() {
        let t = tool();
        // Register more than the cap's worth of immediately-finishing
        // watches, one at a time: because each completes (and is pruned)
        // before the next reservation, the cap is never hit.
        for i in 0..(MAX_CONCURRENT_WATCHES * 2) {
            let handle = tokio::spawn(async {}).abort_handle();
            // Let the empty task finish so the NEXT reserve_slot prunes it.
            tokio::task::yield_now().await;
            t.reserve_slot(entry(handle, i))
                .expect("finished handles are pruned before the cap check");
        }
    }
}
