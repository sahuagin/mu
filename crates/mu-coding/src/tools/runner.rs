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
//!   or a capture that outlives the child takes the whole group down;
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
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time;

use super::bash::ProcessGroup;

/// Spawn retries on `ETXTBSY` (bead o702): another `fork()` elsewhere can
/// briefly hold a writable fd to a freshly written runner script.
const SPAWN_ATTEMPTS: u32 = 4;
const SPAWN_RETRY_BACKOFF: Duration = Duration::from_millis(20);
/// How long past the outer deadline the pipe drains may run: enough to
/// deliver what was already written after the group has been taken down,
/// not enough for a descendant that kept the pipe open to hold the call.
const CAPTURE_GRACE: Duration = Duration::from_secs(2);
/// Slack past the drain deadline before a drain task is abandoned outright
/// (it ends itself at the deadline; this only guards a task that does not).
const CAPTURE_JOIN_SLACK: Duration = Duration::from_secs(1);

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
    /// cannot be a working tool (empty name/grant, zero timeout, unreadable
    /// catalog).
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
        if cfg.max_output_bytes == 0 {
            return Err(format!(
                "[[tools.runner]] `{}` has max_output_bytes = 0; every call would return no output flagged as truncated",
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

/// `sha256:<hex>` of the configured catalog file, or `None` when the config
/// names none. An unreadable catalog is an error: the digest is an audit
/// claim, and a claim that cannot be checked is not made.
fn catalog_digest(cfg: &RunnerToolConfig) -> Result<Option<String>, String> {
    match &cfg.catalog {
        None => Ok(None),
        Some(path) => {
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

/// [`catalog_digest`] for the per-call path: the read runs on the blocking
/// pool, not on a runtime worker thread.
async fn catalog_digest_async(cfg: &RunnerToolConfig) -> Result<Option<String>, String> {
    let cfg = cfg.clone();
    tokio::task::spawn_blocking(move || catalog_digest(&cfg))
        .await
        .map_err(|e| format!("catalog digest task failed: {e}"))?
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
        // Hash the catalog NOW: the digest in the result must name the catalog
        // the runner is about to read, not the one that existed at boot.
        let digest_now = match catalog_digest_async(&self.cfg).await {
            Ok(d) => d,
            Err(message) => return self.refusal("catalog_unreadable", &message, None, None),
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
            // teardown reach every descendant (the runner, the command it
            // execs, anything that forked), not just the direct child.
            .process_group(0)
            .kill_on_drop(true);
        if let Some(cwd) = &self.cfg.cwd {
            command.current_dir(cwd);
        }

        let started = Instant::now();
        let deadline = started + Duration::from_secs(timeout_secs);
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
        let drain_deadline = deadline + CAPTURE_GRACE;
        let join_deadline = drain_deadline + CAPTURE_JOIN_SLACK;
        let mut stdout_task =
            tokio::spawn(async move { read_limited(stdout, limit, drain_deadline).await });
        let mut stderr_task =
            tokio::spawn(async move { read_limited(stderr, limit, drain_deadline).await });

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
                // drains end at EOF (or at their own deadline).
                group.terminate(&mut child).await;
                let stderr_capture = await_capture(&mut stderr_task, join_deadline).await;
                let stdout_capture = await_capture(&mut stdout_task, join_deadline).await;
                let mut content = self.refusal_value(
                    fault.reason(),
                    &message,
                    Some(&stderr_capture),
                    digest_now.as_deref(),
                );
                content["stdout_partial"] = json!(stdout_capture.text);
                content["duration_ms"] = json!(started.elapsed().as_millis() as u64);
                return ToolResult {
                    content: pretty(&content),
                    is_error: true,
                };
            }
        };

        // The child has exited. The pipes close when EVERY holder closes
        // them, so a descendant that kept stdout open could otherwise hold
        // the call forever: the drains end at the outer deadline plus grace,
        // then the group is killed and the call reports a capture timeout.
        // Cancellation stays live through this phase: a cancel kills the
        // group at once instead of waiting out the deadline.
        let captured = tokio::select! {
            captured = async {
                let stdout_capture = await_capture(&mut stdout_task, join_deadline).await;
                let stderr_capture = await_capture(&mut stderr_task, join_deadline).await;
                (stdout_capture, stderr_capture)
            } => Some(captured),
            _ = &mut cancel_rx => None,
        };
        let Some((stdout_capture, stderr_capture)) = captured else {
            group.terminate(&mut child).await;
            let after_kill = Instant::now() + CAPTURE_GRACE;
            let stderr_capture = await_capture(&mut stderr_task, after_kill).await;
            let stdout_capture = await_capture(&mut stdout_task, after_kill).await;
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
            content["duration_ms"] = json!(started.elapsed().as_millis() as u64);
            return ToolResult {
                content: pretty(&content),
                is_error: true,
            };
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
            "stderr_truncated": stderr.map(|s| s.truncated),
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

/// Drain `reader` up to `limit` bytes, until EOF, a read error, or
/// `deadline`. Whatever arrived is kept in every case; only a clean EOF
/// leaves `error` unset.
async fn read_limited<R>(reader: Option<R>, limit: usize, deadline: Instant) -> StreamCapture
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
        let read = match time::timeout_at(deadline.into(), reader.read(&mut buf)).await {
            Ok(read) => read,
            Err(_) => {
                timed_out = true;
                error = Some(
                    "capture did not reach EOF before the deadline; a descendant of the runner still held the pipe"
                        .to_owned(),
                );
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
        let tool = RunnerTool::from_config(&cfg("infra_recon", Path::new("/bin/true"), &[]))
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
        let mut c = cfg("x", Path::new("/bin/true"), &[]);
        c.max_output_bytes = 0;
        let err = RunnerTool::from_config(&c).expect_err("zero limit must fail");
        assert!(err.contains("max_output_bytes"));
    }

    /// A path that is not UTF-8 goes into the record as its lossy display
    /// form; building the record must not panic.
    #[tokio::test]
    async fn non_utf8_runner_path_is_reported_not_panicked() {
        use std::os::unix::ffi::OsStrExt;
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(b"/nonexistent/mu-\xff-runner"));
        let tool = RunnerTool::from_config(&cfg("x", &path, &[])).expect("ok");
        let result = execute(&tool, json!({})).await;
        let value: Value = serde_json::from_str(&result.content).expect("json");
        assert!(result.is_error);
        assert_eq!(value["reason"], "spawn_failed");
        assert!(value["runner"]["path"]
            .as_str()
            .expect("path")
            .contains("mu-"));
    }

    #[test]
    fn config_without_a_grant_is_refused() {
        let mut c = cfg("x", Path::new("/bin/true"), &[]);
        c.grant = String::new();
        let err = RunnerTool::from_config(&c).expect_err("empty grant must fail");
        assert!(err.contains("grant"));
    }

    #[test]
    fn catalog_digest_is_recorded_when_configured() {
        let dir = temp_test_dir("runner-catalog");
        let catalog = dir.join("catalog.json");
        fs::write(&catalog, b"{\"schema_version\":1}").expect("write catalog");
        let mut c = cfg("x", Path::new("/bin/true"), &[]);
        c.catalog = Some(catalog);
        let tool = RunnerTool::from_config(&c).expect("config ok");
        let digest = tool.catalog_digest().expect("digest present");
        assert!(digest.starts_with("sha256:"));
        assert_eq!(digest.len(), "sha256:".len() + 64);

        let mut missing = cfg("x", Path::new("/bin/true"), &[]);
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
        let tool = RunnerTool::from_config(&cfg("x", Path::new("/bin/true"), &[])).expect("ok");
        let result = execute(&tool, json!({"args": ["--verbose"]})).await;
        let value: Value = serde_json::from_str(&result.content).expect("json");
        assert!(result.is_error);
        assert_eq!(value["kind"], "runner_refusal");
        assert_eq!(value["reason"], "invalid_args");
    }

    #[tokio::test]
    async fn missing_runner_is_a_structured_refusal() {
        let tool = RunnerTool::from_config(&cfg(
            "x",
            Path::new("/nonexistent/mu-runner-does-not-exist"),
            &[],
        ))
        .expect("ok");
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
        let result = execute(&tool, json!({"timeout_secs": 1})).await;
        let value: Value = serde_json::from_str(&result.content).expect("json");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "call was bounded by the outer timeout, not by the straggler"
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
        let capture = read_limited(
            Some(reader),
            limit,
            Instant::now() + Duration::from_secs(30),
        )
        .await;
        writer_task.await.expect("writer task joins");
        assert!(capture.truncated);
        assert!(capture.error.is_none());
        assert_eq!(capture.text.len(), limit);
    }

    /// A drain that never reaches EOF ends at its deadline with what arrived
    /// and reports a fault, not a clean capture.
    #[tokio::test]
    async fn capture_past_deadline_is_a_fault_not_an_eof() {
        let (reader, mut writer) = tokio::io::duplex(64);
        writer.write_all(b"partial").await.expect("write");
        let deadline = Instant::now() + Duration::from_millis(100);
        let mut task =
            tokio::spawn(async move { read_limited(Some(reader), 1024, deadline).await });
        let capture = await_capture(&mut task, deadline + Duration::from_secs(5)).await;
        assert!(capture.timed_out);
        assert!(capture.error.is_some());
        assert!(!capture.truncated);
        assert_eq!(capture.text, "partial");
        drop(writer);
    }
}
