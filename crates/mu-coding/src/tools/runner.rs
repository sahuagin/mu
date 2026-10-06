//! A runner-backed tool: a command the operator configured, executed through
//! an external *runner* under a named grant (mu-aws-mi2-18xx1.4).
//!
//! The shape is `runner <grant> -- <command...>`. The runner is the program
//! that resolves the grant name against an operator-managed catalog and
//! materializes the authority (assumes a cloud role, selects a signing key,
//! …) before exec'ing the command; mu knows nothing about what the grant
//! means. mu's side of the boundary is:
//!
//! * the tool declares `required_grant = <grant>`, so the dispatch gate
//!   refuses the call unless the session holds that grant
//!   (`Capability::grants`), and `derived_effects` marks it as reaching the
//!   network and spending. The runner receives only the grant NAME; a grant
//!   held with a narrowing `policy` is therefore refused at the gate rather
//!   than run un-narrowed (see `Grant::policy`);
//! * the subprocess leads its own process group; the outer timeout, a cancel,
//!   or a capture that outlives the child takes the whole group down, and so
//!   does a clean exit (deliberately unlike `bash`, which disarms for detached
//!   jobs), so an ordinary background job does not outlive the call under the
//!   grant. This is best-effort containment: a descendant that calls
//!   `setsid`/`setpgid` and redirects its output leaves the group and is
//!   beyond mu's reach. Containing such a process (a jail, a cgroup, a
//!   reaper) is the runner's job, as is the authority it hands out;
//! * stdout and stderr are captured up to a byte bound, and the captures are
//!   themselves bounded by the outer deadline — a descendant that keeps the
//!   pipe open cannot hang the call;
//! * the result is a structured JSON record (exit code, parsed summary when
//!   stdout is JSON, truncation and capture faults, the catalog digest hashed
//!   at this call) so an auditor can join it to the runner's own record and to
//!   whatever the external system logged. A capture fault is an error result,
//!   never a clean-looking partial one (invariant 7).
//!
//! Everything is wired from `[[tools.runner]]` in the mu config
//! ([`RunnerToolConfig`]); nothing is read from the environment.

use std::future::Future;
#[cfg(test)]
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::time::{Duration, Instant};

use mu_core::agent::{RetryPolicy, Tool, ToolPolicy, ToolResult, ToolSpec};
use mu_core::config::RunnerToolConfig;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time;

use super::bash::ProcessGroup;

/// Spawn retries on `ETXTBSY` (bead o702): another `fork()` elsewhere can
/// briefly hold a writable fd to a freshly written runner script.
const SPAWN_ATTEMPTS: u32 = 4;
const SPAWN_RETRY_BACKOFF: Duration = Duration::from_millis(20);
/// Slack past the drain deadline before a drain task is abandoned outright.
/// A drain ends itself at its deadline; this only guards one that does not,
/// so it is a mechanism bound, not a tunable (the tunable is
/// `capture_grace_secs`).
const CAPTURE_JOIN_SLACK: Duration = Duration::from_secs(1);

/// The furthest instant a call starting now could need: timeout, then the
/// capture grace, then the join slack. `None` when it is not representable,
/// which construction refuses.
fn call_horizon(now: Instant, timeout_secs: u64, capture_grace_secs: u64) -> Option<Instant> {
    now.checked_add(Duration::from_secs(timeout_secs))?
        .checked_add(Duration::from_secs(capture_grace_secs))?
        .checked_add(CAPTURE_JOIN_SLACK)
}

/// How a capture ended, for every result that carries captured text: was
/// it cut at the byte limit, abandoned at the deadline, or ended by a fault?
/// A partial capture is never indistinguishable from a complete one.
fn capture_meta(capture: &StreamCapture) -> Value {
    json!({
        "truncated": capture.truncated,
        "timed_out": capture.timed_out,
        "error": capture.error,
    })
}

/// Aborts the listed tasks when dropped (aborting a finished task is a
/// no-op), so tasks owned by a call never outlive the call's future.
struct AbortOnDrop(Vec<tokio::task::AbortHandle>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for handle in &self.0 {
            handle.abort();
        }
    }
}

/// Shorten a drain deadline (never lengthen it).
fn shrink_deadline(tx: &watch::Sender<Instant>, to: Instant) {
    tx.send_if_modified(|current| {
        if to < *current {
            *current = to;
            true
        } else {
            false
        }
    });
}

#[derive(Debug, Clone)]
pub struct RunnerTool {
    cfg: RunnerToolConfig,
    /// `sha256:<hex>` of the catalog file at construction. This is what a
    /// skill activation records ("the catalog in force when the grant was
    /// handed over"); every CALL re-hashes the file and records that digest,
    /// plus whether it differs from this one. `None` when no catalog is
    /// configured.
    catalog_digest: Option<String>,
}

impl RunnerTool {
    /// Build from one `[[tools.runner]]` entry. Fails loud on an entry that
    /// cannot be a working tool: empty name or grant, a zero timeout or one
    /// too large to schedule, a zero output limit, a runner that is not an executable
    /// file, a `cwd` that is not a directory, or a catalog that is not a
    /// readable regular file.
    pub fn from_config(cfg: &RunnerToolConfig) -> Result<Self, String> {
        if cfg.name.trim().is_empty() {
            return Err("[[tools.runner]] entry has an empty `name`".to_owned());
        }
        if cfg.grant.trim().is_empty() {
            return Err(format!(
                "[[tools.runner]] `{}` has an empty `grant`; a runner tool is grant-gated by construction",
                cfg.name
            ));
        }
        if cfg.timeout_secs == 0 {
            return Err(format!(
                "[[tools.runner]] `{}` has timeout_secs = 0; the outer timeout must be positive",
                cfg.name
            ));
        }
        if call_horizon(Instant::now(), cfg.timeout_secs, cfg.capture_grace_secs).is_none() {
            return Err(format!(
                "[[tools.runner]] `{}`: timeout_secs = {} with capture_grace_secs = {} is too large to schedule; lower them",
                cfg.name, cfg.timeout_secs, cfg.capture_grace_secs
            ));
        }
        if cfg.side_effects.rank() < mu_core::agent::SideEffects::External.rank() {
            return Err(format!(
                "[[tools.runner]] `{}` declares side_effects = {:?}; a runner reaches an external system under a grant, so declare `external` or higher",
                cfg.name, cfg.side_effects
            ));
        }
        check_runner_executable(cfg)?;
        if let Some(cwd) = &cfg.cwd {
            if !cwd.is_dir() {
                return Err(format!(
                    "[[tools.runner]] `{}`: cwd {} is not a directory; fix `cwd` or remove it",
                    cfg.name,
                    cwd.display()
                ));
            }
        }
        if cfg.max_output_bytes == 0 {
            return Err(format!(
                "[[tools.runner]] `{}` has max_output_bytes = 0; every call would return no output flagged as truncated",
                cfg.name
            ));
        }
        if cfg.capture_grace_secs == 0 {
            return Err(format!(
                "[[tools.runner]] `{}` has capture_grace_secs = 0; output still in the pipe at exit would race a zero deadline and a clean run could report a capture timeout. Use at least 1",
                cfg.name
            ));
        }
        let catalog_digest = catalog_digest(cfg)?;
        Ok(Self {
            cfg: cfg.clone(),
            catalog_digest,
        })
    }

    /// The catalog digest at construction (what an activation span records).
    pub fn catalog_digest(&self) -> Option<&str> {
        self.catalog_digest.as_deref()
    }

    pub fn grant(&self) -> &str {
        &self.cfg.grant
    }
}

/// The runner must be an executable regular file: an absolute path, or a
/// bare name found on `PATH` (how `Command` resolves it). A relative path
/// with a separator is refused: whether it resolves against the daemon's
/// directory or `cwd` differs by platform.
fn check_runner_executable(cfg: &RunnerToolConfig) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let runner = &cfg.runner;
    let candidates: Vec<std::path::PathBuf> = if runner.is_absolute() {
        vec![runner.clone()]
    } else if runner.components().count() == 1 {
        std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).map(|d| d.join(runner)).collect())
            .unwrap_or_default()
    } else {
        return Err(format!(
            "[[tools.runner]] `{}`: runner {} is a relative path; give an absolute path or a bare name on PATH",
            cfg.name,
            runner.display()
        ));
    };
    let found = candidates.iter().any(|p| {
        std::fs::metadata(p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    });
    if found {
        Ok(())
    } else {
        Err(format!(
            "[[tools.runner]] `{}`: runner {} is not an executable file; fix `runner`",
            cfg.name,
            runner.display()
        ))
    }
}

/// `sha256:<hex>` of the configured catalog file, or `None` when the config
/// names none. An unreadable catalog is an error: the digest is an audit
/// claim, and a claim that cannot be checked is not made. Only a regular file
/// is read: a FIFO or device would block the reader with no way to stop it.
fn catalog_digest(cfg: &RunnerToolConfig) -> Result<Option<String>, String> {
    match &cfg.catalog {
        None => Ok(None),
        Some(path) => {
            let meta = std::fs::metadata(path).map_err(|e| {
                format!(
                    "[[tools.runner]] `{}`: cannot read catalog {}: {e}",
                    cfg.name,
                    path.display()
                )
            })?;
            if !meta.is_file() {
                return Err(format!(
                    "[[tools.runner]] `{}`: catalog {} is not a regular file; point `catalog` at the catalog file",
                    cfg.name,
                    path.display()
                ));
            }
            let bytes = std::fs::read(path).map_err(|e| {
                format!(
                    "[[tools.runner]] `{}`: cannot read catalog {}: {e}",
                    cfg.name,
                    path.display()
                )
            })?;
            Ok(Some(format!("sha256:{}", sha256_hex(&bytes))))
        }
    }
}

/// [`catalog_digest`] for the per-call path. The read happens in a child
/// process (`sh`: regular-file check, then `cat`) rather than on a runtime
/// or blocking-pool thread, so a read that stalls is abandoned with the
/// call: dropping this future kills the child, and nothing is left holding
/// a runtime thread or delaying shutdown.
async fn catalog_digest_async(cfg: &RunnerToolConfig) -> Result<Option<String>, String> {
    let Some(path) = &cfg.catalog else {
        return Ok(None);
    };
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"[ -f "$1" ] || { echo "not a regular file" >&2; exit 3; }; exec cat -- "$1""#)
        .arg("sh")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| {
            format!(
                "[[tools.runner]] `{}`: cannot start the catalog reader for {}: {e}",
                cfg.name,
                path.display()
            )
        })?;
    if !output.status.success() {
        let why = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(format!(
            "[[tools.runner]] `{}`: cannot read catalog {}: {why}; point `catalog` at the catalog file",
            cfg.name,
            path.display()
        ));
    }
    Ok(Some(format!("sha256:{}", sha256_hex(&output.stdout))))
}

impl Tool for RunnerTool {
    fn spec(&self) -> ToolSpec {
        let mut properties = serde_json::Map::new();
        properties.insert(
            "timeout_secs".to_owned(),
            json!({
                "type": "integer",
                "minimum": 1,
                "maximum": self.cfg.timeout_secs,
                "description": format!(
                    "Outer timeout for the runner subprocess in seconds. Defaults to the configured {}.",
                    self.cfg.timeout_secs
                )
            }),
        );
        if self.cfg.allow_args {
            properties.insert(
                "args".to_owned(),
                json!({
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Extra arguments appended to the configured command."
                }),
            );
        }
        ToolSpec::new(
            self.cfg.name.clone(),
            format!(
                "{} Runs through the capability runner under grant `{}`; the session must hold that grant.",
                self.cfg.description.trim(),
                self.cfg.grant
            ),
            json!({ "type": "object", "properties": properties }),
        )
        .with_policy(ToolPolicy {
            side_effects: self.cfg.side_effects,
            permission: self.cfg.permission,
            retry: RetryPolicy::ModelDecides,
            required_grant: Some(self.cfg.grant.clone()),
            idempotent: false,
            ends_turn_on_success: false,
        })
    }

    fn execute<'life0, 'async_trait>(
        &'life0 self,
        arguments: Value,
        cancel_rx: oneshot::Receiver<()>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { self.execute_inner(arguments, cancel_rx).await })
    }
}

/// Why a call did not produce a clean result. Each maps to a `reason` string
/// a model can branch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    Timeout,
    Cancelled,
    WaitFailed,
}

impl Fault {
    fn reason(self) -> &'static str {
        match self {
            Fault::Timeout => "timeout",
            Fault::Cancelled => "cancelled",
            Fault::WaitFailed => "wait_failed",
        }
    }
}

impl RunnerTool {
    async fn execute_inner(
        &self,
        arguments: Value,
        mut cancel_rx: oneshot::Receiver<()>,
    ) -> ToolResult {
        let timeout_secs = match self.timeout_argument(&arguments) {
            Ok(t) => t,
            Err(message) => return self.refusal("invalid_args", &message, None, None),
        };
        let extra_args = match self.args_argument(&arguments) {
            Ok(a) => a,
            Err(message) => return self.refusal("invalid_args", &message, None, None),
        };
        // The whole call, catalog read included, lives under one deadline
        // and stays cancellable.
        let started = Instant::now();
        let Some(horizon) = call_horizon(started, timeout_secs, self.cfg.capture_grace_secs) else {
            return self.refusal(
                "invalid_args",
                "the requested timeout is too large to schedule",
                None,
                None,
            );
        };
        let capture_grace = Duration::from_secs(self.cfg.capture_grace_secs);
        let deadline = horizon - CAPTURE_JOIN_SLACK - capture_grace;

        // Hash the catalog NOW: the digest in the result must name the catalog
        // the runner is about to read, not the one that existed at boot. Only
        // a regular file is read, by a child process the deadline or a cancel
        // kills.
        let digest_read = tokio::select! {
            read = time::timeout_at(deadline.into(), catalog_digest_async(&self.cfg)) => read,
            _ = &mut cancel_rx => {
                return self.refusal(
                    Fault::Cancelled.reason(),
                    "tool call cancelled while reading the catalog",
                    None,
                    None,
                )
            }
        };
        let digest_now = match digest_read {
            Ok(Ok(d)) => d,
            Ok(Err(message)) => return self.refusal("catalog_unreadable", &message, None, None),
            Err(_) => {
                return self.refusal(
                    "catalog_timeout",
                    &format!(
                        "reading catalog {} did not finish within the outer timeout of {timeout_secs}s (our limit); the runner was not started. Check the filesystem holding it.",
                        self.cfg
                            .catalog
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default()
                    ),
                    None,
                    None,
                )
            }
        };
        let catalog_changed = digest_now != self.catalog_digest;

        let mut command = Command::new(&self.cfg.runner);
        command
            .arg(&self.cfg.grant)
            .arg("--")
            .args(&self.cfg.command)
            .args(&extra_args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // The runner leads a fresh process group so timeout / cancel /
            // teardown reach its descendants (the runner, the command it
            // execs, anything it forked that stayed in the group), not just
            // the direct child. A descendant that leaves the group is the
            // runner's to contain; see the module doc.
            .process_group(0)
            .kill_on_drop(true);
        if let Some(cwd) = &self.cfg.cwd {
            command.current_dir(cwd);
        }

        let mut child = match spawn_with_retry(&mut command).await {
            Ok(child) => child,
            Err(err) => {
                return self.refusal(
                    "spawn_failed",
                    &format!(
                        "failed to spawn runner {}: {err}",
                        self.cfg.runner.display()
                    ),
                    None,
                    digest_now.as_deref(),
                )
            }
        };
        let mut group = ProcessGroup::new(child.id());
        let limit = self.cfg.max_output_bytes;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        // The drains carry their own deadline so a drain that never reaches
        // EOF still returns what arrived (a descendant holding the pipe open
        // is reported as a capture fault WITH the partial output, not lost).
        // The drains share one deadline the call can SHORTEN: it starts at the
        // outer deadline plus grace, and drops to "now plus grace" the moment
        // the runner exits or is killed, so nothing it left behind keeps the
        // call (and its grant) alive past the grace. A drain stops itself at
        // its deadline and keeps what it read.
        let (drain_tx, drain_rx) = watch::channel(deadline + capture_grace);
        let stdout_rx = drain_rx.clone();
        let mut stdout_task =
            tokio::spawn(async move { read_limited(stdout, limit, stdout_rx).await });
        let mut stderr_task =
            tokio::spawn(async move { read_limited(stderr, limit, drain_rx).await });
        // The drains belong to this call. If the call's future is dropped
        // (the agent loop drops it on cancel), abort them rather than leave
        // them detached; and a drain whose deadline sender is gone stops by
        // itself (see `read_limited`).
        let _drain_guard =
            AbortOnDrop(vec![stdout_task.abort_handle(), stderr_task.abort_handle()]);

        let waited = tokio::select! {
            waited = time::timeout_at(deadline.into(), child.wait()) => match waited {
                Ok(Ok(status)) => Ok(status),
                Ok(Err(err)) => Err((Fault::WaitFailed, format!("failed waiting for runner: {err}"))),
                Err(_) => Err((Fault::Timeout, format!("runner exceeded the outer timeout of {timeout_secs}s (our limit, not the runner's failure)"))),
            },
            _ = &mut cancel_rx => Err((Fault::Cancelled, "tool call cancelled".to_owned())),
        };
        let status = match waited {
            Ok(status) => status,
            Err((fault, message)) => {
                // Take the WHOLE group down; the pipes then close and the
                // drains end at EOF, or at a fresh post-kill grace if a
                // descendant escaped the group and still holds one.
                group.terminate(&mut child).await;
                let after_kill = Instant::now() + capture_grace;
                shrink_deadline(&drain_tx, after_kill);
                let join_by = after_kill + CAPTURE_JOIN_SLACK;
                let stderr_capture = await_capture(&mut stderr_task, join_by).await;
                let stdout_capture = await_capture(&mut stdout_task, join_by).await;
                let mut content = self.refusal_value(
                    fault.reason(),
                    &message,
                    Some(&stderr_capture),
                    digest_now.as_deref(),
                );
                content["stdout_partial"] = json!(stdout_capture.text);
                content["stdout_capture"] = capture_meta(&stdout_capture);
                content["duration_ms"] = json!(started.elapsed().as_millis() as u64);
                return ToolResult {
                    content: pretty(&content),
                    is_error: true,
                };
            }
        };

        // The child has exited. The pipes close when EVERY holder closes
        // them, so a descendant that kept stdout open could otherwise hold
        // the call: the drains now get the grace from this moment, not the
        // rest of the outer budget, then the group is killed and the call
        // reports a capture timeout. Cancellation stays live through this
        // phase: a cancel kills the group at once.
        let after_exit = Instant::now() + capture_grace;
        shrink_deadline(&drain_tx, after_exit);
        let join_deadline = after_exit + CAPTURE_JOIN_SLACK;
        // Each drain's result is parked in its slot the moment it is joined,
        // so a cancel never re-polls a finished JoinHandle and never loses a
        // capture that already completed.
        let mut stdout_slot: Option<StreamCapture> = None;
        let mut stderr_slot: Option<StreamCapture> = None;
        let cancelled = tokio::select! {
            _ = async {
                stdout_slot = Some(await_capture(&mut stdout_task, join_deadline).await);
                stderr_slot = Some(await_capture(&mut stderr_task, join_deadline).await);
            } => false,
            _ = &mut cancel_rx => true,
        };
        if cancelled {
            group.terminate(&mut child).await;
            let after_kill = Instant::now() + capture_grace;
            shrink_deadline(&drain_tx, after_kill);
            let join_by = after_kill + CAPTURE_JOIN_SLACK;
            let stderr_capture = match stderr_slot.take() {
                Some(c) => c,
                None => await_capture(&mut stderr_task, join_by).await,
            };
            let stdout_capture = match stdout_slot.take() {
                Some(c) => c,
                None => await_capture(&mut stdout_task, join_by).await,
            };
            let mut content = self.refusal_value(
                Fault::Cancelled.reason(),
                &format!(
                    "tool call cancelled after the runner exited with {status}, while a descendant still held its output; the group was killed"
                ),
                Some(&stderr_capture),
                digest_now.as_deref(),
            );
            content["exit_code"] = json!(status.code());
            content["stdout_partial"] = json!(stdout_capture.text);
            content["stdout_capture"] = capture_meta(&stdout_capture);
            content["duration_ms"] = json!(started.elapsed().as_millis() as u64);
            return ToolResult {
                content: pretty(&content),
                is_error: true,
            };
        }
        let (Some(stdout_capture), Some(stderr_capture)) = (stdout_slot, stderr_slot) else {
            unreachable!("the capture future fills both slots before it completes");
        };
        group.terminate(&mut child).await;
        let duration_ms = started.elapsed().as_millis() as u64;

        let capture_fault = stdout_capture
            .error
            .as_deref()
            .map(|e| format!("stdout: {e}"))
            .or_else(|| {
                stderr_capture
                    .error
                    .as_deref()
                    .map(|e| format!("stderr: {e}"))
            });
        if let Some(fault) = capture_fault {
            let reason = if stdout_capture.timed_out || stderr_capture.timed_out {
                "capture_timeout"
            } else {
                "capture_failed"
            };
            let mut content = self.refusal_value(
                reason,
                &format!(
                    "runner exited with {status} but its output could not be captured completely ({fault}); the group was killed"
                ),
                Some(&stderr_capture),
                digest_now.as_deref(),
            );
            content["exit_code"] = json!(status.code());
            content["stdout_partial"] = json!(stdout_capture.text);
            content["stdout_capture"] = capture_meta(&stdout_capture);
            content["duration_ms"] = json!(duration_ms);
            return ToolResult {
                content: pretty(&content),
                is_error: true,
            };
        }

        if !status.success() {
            let mut content = self.refusal_value(
                "nonzero_exit",
                &format!(
                    "runner exited with {status}: {}",
                    stderr_capture.text.trim()
                ),
                Some(&stderr_capture),
                digest_now.as_deref(),
            );
            content["exit_code"] = json!(status.code());
            content["stdout_partial"] = json!(stdout_capture.text);
            content["stdout_capture"] = capture_meta(&stdout_capture);
            content["duration_ms"] = json!(duration_ms);
            return ToolResult {
                content: pretty(&content),
                is_error: true,
            };
        }

        let summary: Option<Value> = serde_json::from_str(stdout_capture.text.trim()).ok();
        let report = json!({
            "kind": "runner_result",
            "tool": self.cfg.name,
            "grant": self.cfg.grant,
            "catalog_digest": digest_now,
            "catalog_changed_since_start": catalog_changed,
            "exit_code": status.code(),
            "duration_ms": duration_ms,
            "timeout_secs": timeout_secs,
            "summary": summary,
            "stdout": if summary.is_some() { Value::Null } else { json!(stdout_capture.text) },
            "stderr": stderr_capture.text,
            "truncated": {
                "stdout": stdout_capture.truncated,
                "stderr": stderr_capture.truncated,
                "limit_bytes": limit,
            },
            "runner": {
                "path": self.cfg.runner.display().to_string(),
                "command": self.cfg.command,
                "args": extra_args,
                "cwd": self.cfg.cwd.as_ref().map(|p| p.display().to_string()),
            },
        });
        ToolResult {
            content: pretty(&report),
            is_error: false,
        }
    }

    fn timeout_argument(&self, arguments: &Value) -> Result<u64, String> {
        match arguments.get("timeout_secs") {
            None | Some(Value::Null) => Ok(self.cfg.timeout_secs),
            Some(Value::Number(n)) => match n.as_u64() {
                Some(v) if (1..=self.cfg.timeout_secs).contains(&v) => Ok(v),
                _ => Err(format!(
                    "`timeout_secs` must be an integer between 1 and {}",
                    self.cfg.timeout_secs
                )),
            },
            Some(_) => Err(format!(
                "`timeout_secs` must be an integer between 1 and {}",
                self.cfg.timeout_secs
            )),
        }
    }

    fn args_argument(&self, arguments: &Value) -> Result<Vec<String>, String> {
        match arguments.get("args") {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(items)) => {
                if !self.cfg.allow_args {
                    return Err(format!(
                        "`{}` does not accept extra arguments (allow_args is false in its config)",
                        self.cfg.name
                    ));
                }
                items
                    .iter()
                    .map(|v| match v {
                        Value::String(s) => Ok(s.clone()),
                        _ => Err("`args` must be an array of strings".to_owned()),
                    })
                    .collect()
            }
            Some(_) => Err("`args` must be an array of strings".to_owned()),
        }
    }

    fn refusal(
        &self,
        reason: &str,
        message: &str,
        stderr: Option<&StreamCapture>,
        catalog_digest: Option<&str>,
    ) -> ToolResult {
        ToolResult {
            content: pretty(&self.refusal_value(reason, message, stderr, catalog_digest)),
            is_error: true,
        }
    }

    fn refusal_value(
        &self,
        reason: &str,
        message: &str,
        stderr: Option<&StreamCapture>,
        catalog_digest: Option<&str>,
    ) -> Value {
        json!({
            "kind": "runner_refusal",
            "reason": reason,
            "message": message,
            "tool": self.cfg.name,
            "grant": self.cfg.grant,
            "catalog_digest": catalog_digest,
            "stderr": stderr.map(|s| s.text.clone()),
            "stderr_capture": stderr.map(capture_meta),
            "runner": {
                "path": self.cfg.runner.display().to_string(),
                "command": self.cfg.command,
            },
        })
    }
}

/// Spawn, retrying a few times on `ETXTBSY` (bead o702: a concurrent
/// `fork()` elsewhere can hold a writable fd to a freshly written runner
/// across the exec; the retry is the in-tree precedent from
/// `SubprocessRecallProvider`).
async fn spawn_with_retry(command: &mut Command) -> std::io::Result<Child> {
    let mut attempts = 0;
    loop {
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(e)
                if e.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && attempts + 1 < SPAWN_ATTEMPTS =>
            {
                attempts += 1;
                time::sleep(SPAWN_RETRY_BACKOFF).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// What a pipe drain produced. `error` is set when the drain did NOT end at
/// a clean EOF: a read error, a failed drain task, or the deadline
/// (`timed_out`). `text` is then what arrived before the fault.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StreamCapture {
    text: String,
    truncated: bool,
    timed_out: bool,
    error: Option<String>,
}

/// Drain `reader` up to `limit` bytes, until EOF, a read error, or the
/// current value of `deadline` (which the caller may shorten while the
/// drain runs). Whatever arrived is kept in every case; only a clean EOF
/// leaves `error` unset.
async fn read_limited<R>(
    reader: Option<R>,
    limit: usize,
    mut deadline: watch::Receiver<Instant>,
) -> StreamCapture
where
    R: AsyncRead + Unpin,
{
    let Some(mut reader) = reader else {
        return StreamCapture {
            text: String::new(),
            truncated: false,
            timed_out: false,
            error: None,
        };
    };
    let mut buf = [0_u8; 8192];
    let mut captured = Vec::new();
    let mut truncated = false;
    let mut timed_out = false;
    let mut error = None;
    loop {
        let due = *deadline.borrow_and_update();
        let read = tokio::select! {
            read = reader.read(&mut buf) => read,
            _ = time::sleep_until(due.into()) => {
                timed_out = true;
                error = Some(
                    "capture did not reach EOF before the deadline; a descendant of the runner still held the pipe"
                        .to_owned(),
                );
                break;
            }
            // The deadline moved: re-arm with the new one (a read in flight
            // is cancel-safe and simply restarts). If the sender is gone the
            // call has ended: stop and keep what arrived.
            changed = deadline.changed() => {
                if changed.is_ok() {
                    continue;
                }
                error = Some("the call ended before the capture reached EOF".to_owned());
                break;
            }
        };
        let n = match read {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                // A read error is a fault, not an EOF: say so, keep what
                // arrived.
                error = Some(format!("pipe read failed: {e}"));
                break;
            }
        };
        if captured.len() < limit {
            let keep = (limit - captured.len()).min(n);
            captured.extend_from_slice(&buf[..keep]);
            if keep < n {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }
    StreamCapture {
        text: String::from_utf8_lossy(&captured).into_owned(),
        truncated,
        timed_out,
        error,
    }
}

/// Join a drain task. The drain ends itself at its own deadline, so this
/// normally returns promptly; `join_deadline` only guards a task that does
/// not, and a task that panicked reports that as its error. Nothing here
/// looks like a clean EOF unless it was one.
async fn await_capture(
    handle: &mut JoinHandle<StreamCapture>,
    join_deadline: Instant,
) -> StreamCapture {
    match time::timeout_at(join_deadline.into(), &mut *handle).await {
        Ok(Ok(capture)) => capture,
        Ok(Err(join)) => StreamCapture {
            text: String::new(),
            truncated: true,
            timed_out: false,
            error: Some(format!("capture task failed: {join}")),
        },
        Err(_) => {
            handle.abort();
            StreamCapture {
                text: String::new(),
                truncated: true,
                timed_out: true,
                error: Some(
                    "capture task did not end at its deadline and was abandoned".to_owned(),
                ),
            }
        }
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).expect("json serialization cannot fail")
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Root of an exec-allowed directory for tests that write a runner shim:
/// `/tmp` is `noexec` on FreeBSD and hardened Linux, so route through the
/// workspace `target/` directory (exec-allowed by construction, ignored by
/// the repo's root `/target` rule). The workspace root is the ancestor of
/// `CARGO_MANIFEST_DIR` holding `Cargo.lock`; a crate-local `target/` would
/// NOT be ignored and would dirty the tree.
#[cfg(test)]
fn exec_temp_root() -> PathBuf {
    if let Some(p) = std::env::var_os("CARGO_TARGET_TMPDIR") {
        return PathBuf::from(p);
    }
    if let Some(p) = std::env::var_os("CARGO_TARGET_DIR") {
        return PathBuf::from(p).join("test-tmp");
    }
    if let Some(m) = std::env::var_os("CARGO_MANIFEST_DIR") {
        let mut path = PathBuf::from(m);
        loop {
            if path.join("Cargo.lock").is_file() {
                return path.join("target").join("test-tmp");
            }
            if !path.pop() {
                break;
            }
        }
    }
    std::env::temp_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_core::agent::{PermissionLevel, SideEffects};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::io::AsyncWriteExt;

    fn cfg(name: &str, runner: &Path, command: &[&str]) -> RunnerToolConfig {
        RunnerToolConfig {
            name: name.to_owned(),
            description: "Test runner tool.".to_owned(),
            grant: "infra.scout.readonly".to_owned(),
            runner: runner.to_path_buf(),
            command: command.iter().map(|s| (*s).to_owned()).collect(),
            cwd: None,
            catalog: None,
            timeout_secs: 30,
            max_output_bytes: 64 * 1024,
            capture_grace_secs: 2,
            allow_args: false,
            side_effects: SideEffects::External,
            permission: PermissionLevel::Allow,
        }
    }

    fn temp_test_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let dir = exec_temp_root().join(format!("mu-{name}-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// The runner shim: drop the grant and the `--`, exec the command.
    /// Written once per test; the tool's spawn retries on ETXTBSY.
    fn write_runner_shim(dir: &Path) -> PathBuf {
        let path = dir.join("runner.sh");
        fs::write(
            &path,
            "#!/bin/sh\nprintf 'grant=%s\\n' \"$1\" >&2\nshift\n[ \"$1\" = \"--\" ] || exit 2\nshift\nexec \"$@\"\n",
        )
        .expect("write shim");
        let mut perms = fs::metadata(&path).expect("shim metadata").permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&path, perms).expect("chmod shim");
        path
    }

    async fn execute(tool: &RunnerTool, arguments: Value) -> ToolResult {
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        tool.execute(arguments, cancel_rx).await
    }

    #[test]
    fn spec_is_grant_gated_and_names_the_grant() {
        let tool = RunnerTool::from_config(&cfg("infra_recon", Path::new("/bin/sh"), &[]))
            .expect("config ok");
        let spec = tool.spec();
        assert_eq!(spec.name, "infra_recon");
        assert_eq!(
            spec.policy.required_grant.as_deref(),
            Some("infra.scout.readonly")
        );
        assert_eq!(spec.policy.side_effects, SideEffects::External);
        assert!(spec.description.contains("infra.scout.readonly"));
        // No `args` property unless the config allows extra arguments.
        assert!(spec.input_schema["properties"].get("args").is_none());
        assert!(spec.input_schema["properties"]
            .get("timeout_secs")
            .is_some());
        let eff = spec.policy.derived_effects();
        assert!(eff.network && eff.spend);
    }

    #[test]
    fn config_with_a_zero_output_limit_is_refused() {
        let mut c = cfg("x", Path::new("/bin/sh"), &[]);
        c.max_output_bytes = 0;
        let err = RunnerTool::from_config(&c).expect_err("zero limit must fail");
        assert!(err.contains("max_output_bytes"));
    }

    /// A path that is not UTF-8 is reported in its lossy display form; neither
    /// construction nor a record built from it may panic.
    #[test]
    fn non_utf8_runner_path_is_reported_not_panicked() {
        use std::os::unix::ffi::OsStrExt;
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(b"/nonexistent/mu-\xff-runner"));
        let err = RunnerTool::from_config(&cfg("x", &path, &[])).expect_err("missing runner");
        assert!(err.contains("mu-"), "{err}");
    }

    #[test]
    fn config_without_a_grant_is_refused() {
        let mut c = cfg("x", Path::new("/bin/sh"), &[]);
        c.grant = String::new();
        let err = RunnerTool::from_config(&c).expect_err("empty grant must fail");
        assert!(err.contains("grant"));
    }

    #[test]
    fn catalog_digest_is_recorded_when_configured() {
        let dir = temp_test_dir("runner-catalog");
        let catalog = dir.join("catalog.json");
        fs::write(&catalog, b"{\"schema_version\":1}").expect("write catalog");
        let mut c = cfg("x", Path::new("/bin/sh"), &[]);
        c.catalog = Some(catalog);
        let tool = RunnerTool::from_config(&c).expect("config ok");
        let digest = tool.catalog_digest().expect("digest present");
        assert!(digest.starts_with("sha256:"));
        assert_eq!(digest.len(), "sha256:".len() + 64);

        let mut missing = cfg("x", Path::new("/bin/sh"), &[]);
        missing.catalog = Some(dir.join("absent.json"));
        assert!(RunnerTool::from_config(&missing).is_err());
    }

    /// The digest in a RESULT is the catalog as hashed at that call, and the
    /// result says when it differs from the one the tool was built against.
    #[tokio::test]
    async fn catalog_is_rehashed_per_call_and_a_change_is_flagged() {
        let dir = temp_test_dir("runner-catalog-change");
        let shim = write_runner_shim(&dir);
        let catalog = dir.join("catalog.json");
        fs::write(&catalog, b"{\"v\":1}").expect("write catalog");
        let mut c = cfg("x", &shim, &["/bin/sh", "-c", "echo ok"]);
        c.catalog = Some(catalog.clone());
        let tool = RunnerTool::from_config(&c).expect("ok");
        let at_start = tool.catalog_digest().expect("digest").to_owned();

        let result = execute(&tool, json!({})).await;
        let value: Value = serde_json::from_str(&result.content).expect("json");
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(value["catalog_digest"], at_start);
        assert_eq!(value["catalog_changed_since_start"], false);

        fs::write(&catalog, b"{\"v\":2}").expect("rewrite catalog");
        let result = execute(&tool, json!({})).await;
        let value: Value = serde_json::from_str(&result.content).expect("json");
        assert!(!result.is_error, "{}", result.content);
        assert_ne!(value["catalog_digest"], at_start);
        assert_eq!(value["catalog_changed_since_start"], true);

        fs::remove_file(&catalog).expect("remove catalog");
        let result = execute(&tool, json!({})).await;
        let value: Value = serde_json::from_str(&result.content).expect("json");
        assert!(result.is_error);
        assert_eq!(value["reason"], "catalog_unreadable");
    }

    #[tokio::test]
    async fn extra_args_are_refused_unless_allowed() {
        let tool = RunnerTool::from_config(&cfg("x", Path::new("/bin/sh"), &[])).expect("ok");
        let result = execute(&tool, json!({"args": ["--verbose"]})).await;
        let value: Value = serde_json::from_str(&result.content).expect("json");
        assert!(result.is_error);
        assert_eq!(value["kind"], "runner_refusal");
        assert_eq!(value["reason"], "invalid_args");
    }

    #[test]
    fn runner_that_is_not_executable_is_refused_at_construction() {
        let err = RunnerTool::from_config(&cfg(
            "x",
            Path::new("/nonexistent/mu-runner-does-not-exist"),
            &[],
        ))
        .expect_err("missing runner must fail");
        assert!(err.contains("not an executable file"), "{err}");

        let dir = temp_test_dir("runner-not-exec");
        let plain = dir.join("plain.sh");
        fs::write(&plain, "#!/bin/sh\n").expect("write");
        let err = RunnerTool::from_config(&cfg("x", &plain, &[])).expect_err("non-exec must fail");
        assert!(err.contains("not an executable file"), "{err}");

        let err = RunnerTool::from_config(&cfg("x", Path::new("bin/run.sh"), &[]))
            .expect_err("relative path must fail");
        assert!(err.contains("relative path"), "{err}");

        // A bare name resolves on PATH, as Command does.
        assert!(RunnerTool::from_config(&cfg("x", Path::new("sh"), &[])).is_ok());

        let mut bad_cwd = cfg("x", Path::new("/bin/sh"), &[]);
        bad_cwd.cwd = Some(dir.join("absent-dir"));
        let err = RunnerTool::from_config(&bad_cwd).expect_err("bad cwd must fail");
        assert!(err.contains("not a directory"), "{err}");

        let mut huge = cfg("x", Path::new("/bin/sh"), &[]);
        huge.timeout_secs = u64::MAX;
        assert!(RunnerTool::from_config(&huge).is_err());
    }

    #[test]
    fn zero_capture_grace_is_refused() {
        let mut c = cfg("x", Path::new("/bin/sh"), &[]);
        c.capture_grace_secs = 0;
        let err = RunnerTool::from_config(&c).expect_err("zero grace must fail");
        assert!(err.contains("capture_grace_secs"), "{err}");
    }

    /// Dropping the call's future (as the agent loop does on cancel) must not
    /// leave its drains running: a straggler holding the pipe would otherwise
    /// keep a detached drain alive to the outer deadline.
    #[tokio::test]
    async fn dropping_the_call_aborts_its_drains() {
        let dir = temp_test_dir("runner-drop");
        let shim = write_runner_shim(&dir);
        let pidfile = dir.join("straggler.pid");
        // A straggler that leaves the process group (setsid) but keeps the
        // inherited stdout. `setsid(1)` is not on every platform; python is
        // used for the syscall, and the test skips without it.
        if std::process::Command::new("python3")
            .arg("-c")
            .arg("pass")
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            eprintln!("python3 unavailable; skipping");
            return;
        }
        let script = format!(
            "python3 -c 'import os,time; os.setsid(); open(\"{}\",\"w\").write(str(os.getpid())); time.sleep(30)' & sleep 30",
            pidfile.display()
        );
        let tool =
            RunnerTool::from_config(&cfg("x", &shim, &["/bin/sh", "-c", &script])).expect("ok");
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        let fut = tool.execute(json!({"timeout_secs": 30}), cancel_rx);
        // Run it briefly, then drop it mid-flight.
        let _ = time::timeout(Duration::from_millis(800), fut).await;
        // The straggler escaped the group (setsid) and still holds the pipe;
        // the drains must nonetheless be gone. Observe via the runtime: no
        // task should still be reading. Give aborts a moment to land.
        time::sleep(Duration::from_millis(100)).await;
        let metrics = tokio::runtime::Handle::current().metrics();
        assert_eq!(metrics.num_alive_tasks(), 0, "drains outlived the call");
        if let Ok(pid) = fs::read_to_string(&pidfile) {
            let _ = std::process::Command::new("kill").arg(pid.trim()).status();
        }
    }

    #[test]
    fn understated_side_effects_are_refused() {
        let mut c = cfg("x", Path::new("/bin/sh"), &[]);
        c.side_effects = SideEffects::ReadOnly;
        let err = RunnerTool::from_config(&c).expect_err("read_only must fail");
        assert!(err.contains("side_effects"), "{err}");
        c.side_effects = SideEffects::Execute;
        assert!(RunnerTool::from_config(&c).is_ok());
    }

    /// A runner removed after startup is a structured spawn refusal.
    #[tokio::test]
    async fn runner_removed_after_start_is_a_structured_refusal() {
        let dir = temp_test_dir("runner-removed");
        let shim = write_runner_shim(&dir);
        let tool =
            RunnerTool::from_config(&cfg("x", &shim, &["/bin/sh", "-c", "echo ok"])).expect("ok");
        fs::remove_file(&shim).expect("remove shim");
        let result = execute(&tool, json!({})).await;
        let value: Value = serde_json::from_str(&result.content).expect("json");
        assert!(result.is_error);
        assert_eq!(value["reason"], "spawn_failed");
        assert_eq!(value["grant"], "infra.scout.readonly");
    }

    #[tokio::test]
    async fn json_stdout_becomes_the_summary() {
        let dir = temp_test_dir("runner-ok");
        let shim = write_runner_shim(&dir);
        let tool = RunnerTool::from_config(&cfg(
            "infra_recon",
            &shim,
            &[
                "/bin/sh",
                "-c",
                "printf '{\"report\":\"r/1\",\"errors\":[]}\\n'",
            ],
        ))
        .expect("ok");
        let result = execute(&tool, json!({"timeout_secs": 10})).await;
        let value: Value = serde_json::from_str(&result.content).expect("json");
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(value["kind"], "runner_result");
        assert_eq!(value["exit_code"], 0);
        assert_eq!(value["summary"]["report"], "r/1");
        assert!(value["stdout"].is_null());
        assert!(value["stderr"]
            .as_str()
            .expect("stderr")
            .contains("grant=infra.scout.readonly"));
        assert_eq!(value["truncated"]["stdout"], false);
        assert!(value["catalog_digest"].is_null());
    }

    #[tokio::test]
    async fn plain_stdout_is_kept_verbatim() {
        let dir = temp_test_dir("runner-plain");
        let shim = write_runner_shim(&dir);
        let tool = RunnerTool::from_config(&cfg("x", &shim, &["/bin/sh", "-c", "echo hello"]))
            .expect("ok");
        let result = execute(&tool, json!({})).await;
        let value: Value = serde_json::from_str(&result.content).expect("json");
        assert!(!result.is_error, "{}", result.content);
        assert!(value["summary"].is_null());
        assert_eq!(value["stdout"], "hello\n");
    }

    #[tokio::test]
    async fn nonzero_exit_is_an_error_with_stderr() {
        let dir = temp_test_dir("runner-fail");
        let shim = write_runner_shim(&dir);
        let tool = RunnerTool::from_config(&cfg(
            "x",
            &shim,
            &["/bin/sh", "-c", "echo partial; echo refused >&2; exit 42"],
        ))
        .expect("ok");
        let result = execute(&tool, json!({})).await;
        let value: Value = serde_json::from_str(&result.content).expect("json");
        assert!(result.is_error);
        assert_eq!(value["reason"], "nonzero_exit");
        assert_eq!(value["exit_code"], 42);
        assert!(value["message"]
            .as_str()
            .expect("message")
            .contains("refused"));
        assert_eq!(value["stdout_partial"], "partial\n");
        assert_eq!(value["stdout_capture"]["truncated"], false);
        assert!(value["stdout_capture"]["error"].is_null());
        assert_eq!(value["stderr_capture"]["truncated"], false);
    }

    #[tokio::test]
    async fn timeout_kills_a_hung_runner() {
        let dir = temp_test_dir("runner-timeout");
        let shim = write_runner_shim(&dir);
        let tool =
            RunnerTool::from_config(&cfg("x", &shim, &["/bin/sh", "-c", "sleep 20"])).expect("ok");
        let started = Instant::now();
        let result = execute(&tool, json!({"timeout_secs": 1})).await;
        let value: Value = serde_json::from_str(&result.content).expect("json");
        assert!(result.is_error);
        assert_eq!(value["reason"], "timeout");
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "runner was killed"
        );
    }

    /// A descendant that outlives the runner while holding its stdout open
    /// (a backgrounded job) must not hang the call: the captures are bounded
    /// by the outer deadline, the group is killed, and the result says so.
    #[tokio::test]
    async fn descendant_holding_the_pipe_cannot_hang_the_call() {
        let dir = temp_test_dir("runner-straggler");
        let shim = write_runner_shim(&dir);
        let tool = RunnerTool::from_config(&cfg(
            "x",
            &shim,
            &["/bin/sh", "-c", "echo started; sleep 30 & exit 0"],
        ))
        .expect("ok");
        let started = Instant::now();
        let result = execute(&tool, json!({"timeout_secs": 30})).await;
        let value: Value = serde_json::from_str(&result.content).expect("json");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "call was bounded by the grace after exit, not by the 30s timeout"
        );
        assert!(result.is_error, "{}", result.content);
        assert_eq!(value["reason"], "capture_timeout");
        assert_eq!(value["exit_code"], 0);
        assert_eq!(value["stdout_partial"], "started\n");
    }

    /// A cancel that arrives after the runner exited, while a descendant
    /// still holds the pipe, kills the group at once rather than waiting out
    /// the outer deadline.
    #[tokio::test]
    async fn cancel_during_capture_kills_the_straggler() {
        let dir = temp_test_dir("runner-cancel-capture");
        let shim = write_runner_shim(&dir);
        let tool = RunnerTool::from_config(&cfg(
            "x",
            &shim,
            &["/bin/sh", "-c", "echo started; sleep 30 & exit 0"],
        ))
        .expect("ok");
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let fut = tool.execute(json!({"timeout_secs": 30}), cancel_rx);
        let canceller = tokio::spawn(async move {
            time::sleep(Duration::from_millis(500)).await;
            let _ = cancel_tx.send(());
        });
        let started = Instant::now();
        let result = fut.await;
        canceller.await.expect("canceller joins");
        let value: Value = serde_json::from_str(&result.content).expect("json");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "cancel was honoured during capture, not after the 30s deadline"
        );
        assert!(result.is_error);
        assert_eq!(value["reason"], "cancelled");
        assert_eq!(value["exit_code"], 0);
    }

    /// stdout reaches EOF but a straggler keeps stderr open; a cancel then
    /// must neither re-poll the finished stdout drain nor lose its output.
    #[tokio::test]
    async fn cancel_after_stdout_closed_keeps_stdout() {
        let dir = temp_test_dir("runner-cancel-stderr");
        let shim = write_runner_shim(&dir);
        let tool = RunnerTool::from_config(&cfg(
            "x",
            &shim,
            &["/bin/sh", "-c", "echo done; sleep 30 >/dev/null & exit 0"],
        ))
        .expect("ok");
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let fut = tool.execute(json!({"timeout_secs": 30}), cancel_rx);
        let canceller = tokio::spawn(async move {
            time::sleep(Duration::from_millis(500)).await;
            let _ = cancel_tx.send(());
        });
        let started = Instant::now();
        let result = fut.await;
        canceller.await.expect("canceller joins");
        let value: Value = serde_json::from_str(&result.content).expect("json");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(result.is_error);
        assert_eq!(value["reason"], "cancelled");
        assert_eq!(value["stdout_partial"], "done\n");
    }

    /// A catalog that is not a regular file (here a FIFO with no writer,
    /// which would block any reader) is refused before it is read, both at
    /// construction and at call time.
    #[tokio::test]
    async fn fifo_catalog_is_refused_without_reading() {
        let dir = temp_test_dir("runner-catalog-fifo");
        let shim = write_runner_shim(&dir);
        let catalog = dir.join("catalog.json");
        let status = std::process::Command::new("mkfifo")
            .arg(&catalog)
            .status()
            .expect("mkfifo runs");
        assert!(status.success());
        let mut c = cfg("x", &shim, &["/bin/sh", "-c", "echo ok"]);
        c.catalog = Some(catalog.clone());
        let err = RunnerTool::from_config(&c).expect_err("fifo at construction");
        assert!(err.contains("not a regular file"), "{err}");

        // Swapped in after startup: refused at call time, promptly.
        fs::remove_file(&catalog).expect("remove fifo");
        fs::write(&catalog, b"{}").expect("write catalog");
        let tool = RunnerTool::from_config(&c).expect("ok");
        fs::remove_file(&catalog).expect("remove catalog");
        let status = std::process::Command::new("mkfifo")
            .arg(&catalog)
            .status()
            .expect("mkfifo runs");
        assert!(status.success());
        let started = Instant::now();
        let result = execute(&tool, json!({"timeout_secs": 5})).await;
        let value: Value = serde_json::from_str(&result.content).expect("json");
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(result.is_error);
        assert_eq!(value["reason"], "catalog_unreadable");
    }

    #[tokio::test]
    async fn cancel_kills_the_runner() {
        let dir = temp_test_dir("runner-cancel");
        let shim = write_runner_shim(&dir);
        let tool =
            RunnerTool::from_config(&cfg("x", &shim, &["/bin/sh", "-c", "sleep 20"])).expect("ok");
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let fut = tool.execute(json!({}), cancel_rx);
        let canceller = tokio::spawn(async move {
            time::sleep(Duration::from_millis(200)).await;
            let _ = cancel_tx.send(());
        });
        let started = Instant::now();
        let result = fut.await;
        canceller.await.expect("canceller joins");
        let value: Value = serde_json::from_str(&result.content).expect("json");
        assert!(result.is_error);
        assert_eq!(value["reason"], "cancelled");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn large_output_is_bounded_and_flagged() {
        let limit = 10_000;
        let (reader, mut writer) = tokio::io::duplex(8192);
        let writer_task = tokio::spawn(async move {
            let chunk = vec![b'x'; 8192];
            let mut remaining = limit + 1;
            while remaining > 0 {
                let n = remaining.min(chunk.len());
                writer.write_all(&chunk[..n]).await.expect("write chunk");
                remaining -= n;
            }
            writer.shutdown().await.expect("shutdown writer");
        });
        let (_tx, rx) = watch::channel(Instant::now() + Duration::from_secs(30));
        let capture = read_limited(Some(reader), limit, rx).await;
        writer_task.await.expect("writer task joins");
        assert!(capture.truncated);
        assert!(capture.error.is_none());
        assert_eq!(capture.text.len(), limit);
    }

    /// Shortening the shared deadline stops a drain that is already waiting,
    /// keeping what it read.
    #[tokio::test]
    async fn shortened_deadline_stops_a_waiting_drain() {
        let (reader, mut writer) = tokio::io::duplex(64);
        writer.write_all(b"kept").await.expect("write");
        let (tx, rx) = watch::channel(Instant::now() + Duration::from_secs(60));
        let mut task = tokio::spawn(async move { read_limited(Some(reader), 1024, rx).await });
        time::sleep(Duration::from_millis(100)).await;
        let started = Instant::now();
        shrink_deadline(&tx, Instant::now() + Duration::from_millis(100));
        let capture = await_capture(&mut task, Instant::now() + Duration::from_secs(5)).await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(capture.timed_out);
        assert_eq!(capture.text, "kept");
        drop(writer);
    }

    /// A drain that never reaches EOF ends at its deadline with what arrived
    /// and reports a fault, not a clean capture.
    #[tokio::test]
    async fn capture_past_deadline_is_a_fault_not_an_eof() {
        let (reader, mut writer) = tokio::io::duplex(64);
        writer.write_all(b"partial").await.expect("write");
        let deadline = Instant::now() + Duration::from_millis(100);
        let (_tx, rx) = watch::channel(deadline);
        let mut task = tokio::spawn(async move { read_limited(Some(reader), 1024, rx).await });
        let capture = await_capture(&mut task, deadline + Duration::from_secs(5)).await;
        assert!(capture.timed_out);
        assert!(capture.error.is_some());
        assert!(!capture.truncated);
        assert_eq!(capture.text, "partial");
        drop(writer);
    }
}
