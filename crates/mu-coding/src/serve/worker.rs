//! mu-slat: spawned worker lifecycle.
//!
//! Spawns a non-POT worker through `mu-spawn`, tracks it as a
//! `SubprocessSession` in the session registry, and monitors the child process
//! for exit/timeout/failure. POT/jail setup is deliberately gone; `mu-spawn`
//! dispatches to current agent runtimes (`mu ask` or `claude -p`).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::process::Command;

use mu_core::capability::Capability;
use mu_core::event_log::{EventActor, EventPayload, SessionEventLog, WorkerTerminalStatus};
use mu_core::protocol::WorkerStatus;

use super::daemon_info::DaemonInfo;
use super::mailbox::MailboxState;
use super::sessions::{Sessions, SubprocessSession};

/// Everything the caller needs to know after a successful spawn.
pub(crate) struct SpawnResult {
    pub session_id: String,
    pub worker_name: String, // wire-compatible worker name (legacy field)
}

/// Configuration for spawning a worker.
pub(crate) struct SpawnWorkerConfig {
    pub prompt: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub worker_name: Option<String>, // optional worker name (legacy field)
    pub timeout_secs: Option<u64>,
    pub parent_session_id: Option<String>,
    /// Built-in mu tools the child may receive via `mu ask --tools` / agent_dispatch.
    /// Always derived from the parent's session posture; never hardcoded to full power.
    pub tools: Vec<String>,
}

/// Built-in tools that can honestly be passed through `mu ask --tools` and
/// mapped to Claude's `--allowedTools` by scripts/lib/agent-dispatch.sh. Session
/// control tools (spawn_worker/watch/discover/start_autonomous/...) are NOT
/// delegable to a subprocess child by default; the child gets work tools only.
const WORKER_TOOL_ORDER: &[&str] = &["read", "write", "edit", "glob", "grep", "ls", "bash"];

/// Minimal no-inheritance fallback for non-session / direct RPC spawns. This is
/// intentionally useful for inspection/review, but not write-capable.
const DEFAULT_WORKER_TOOLS: &[&str] = &["read", "grep"];

fn ordered_tool_intersection(names: impl IntoIterator<Item = String>) -> Vec<String> {
    let requested: HashSet<String> = names
        .into_iter()
        .filter(|name| WORKER_TOOL_ORDER.contains(&name.as_str()))
        .collect();
    WORKER_TOOL_ORDER
        .iter()
        .filter(|name| requested.iter().any(|requested| requested == *name))
        .map(|name| (*name).to_string())
        .collect()
}

/// Normalize a parent session's daemon/tool-list grant into the built-in tools
/// that may be delegated to a subprocess worker.
pub(crate) fn normalize_worker_tool_grant(names: impl IntoIterator<Item = String>) -> Vec<String> {
    ordered_tool_intersection(names)
}

/// Derive the child subprocess tool grant from a parent session's delegable tool
/// grant and (when available) its live capability. This is the attenuation point:
/// child tools are always a subset of the parent grant, further narrowed by
/// `Capability.allowed_tools` when that axis is populated.
pub(crate) fn derive_child_tool_grant(
    parent_grant: &[String],
    parent_capability: Option<&Capability>,
) -> Vec<String> {
    let base = if parent_grant.is_empty() {
        DEFAULT_WORKER_TOOLS
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>()
    } else {
        parent_grant.to_vec()
    };
    let Some(allowed) = parent_capability.and_then(|cap| cap.allowed_tools.as_ref()) else {
        return ordered_tool_intersection(base);
    };
    ordered_tool_intersection(base.into_iter().filter(|name| allowed.contains(name)))
}

/// Direct RPC fallback: no per-session `SpawnWorkerTool` exists to carry the
/// daemon's base grant, so derive from the parent capability when possible and
/// otherwise use the minimal floor.
pub(crate) fn derive_child_tool_grant_from_capability(
    parent_capability: Option<&Capability>,
) -> Vec<String> {
    let Some(allowed) = parent_capability.and_then(|cap| cap.allowed_tools.as_ref()) else {
        return DEFAULT_WORKER_TOOLS
            .iter()
            .map(|name| (*name).to_string())
            .collect();
    };
    ordered_tool_intersection(allowed.iter().cloned())
}

fn resolve_mu_spawn_binary() -> String {
    if let Ok(path) = std::env::var("MU_SPAWN") {
        return path;
    }
    if std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join("mu-spawn"))
                .find(|candidate| candidate.is_file())
        })
        .is_some()
    {
        return "mu-spawn".into();
    }
    for candidate in [
        PathBuf::from("scripts/mu-spawn"),
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join("src/public_github/mu/scripts/mu-spawn"),
    ] {
        if candidate.is_file() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    "mu-spawn".into()
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Spawn a non-POT worker via mu-spawn.
///
/// Returns the session_id and worker name on success. The worker is
/// registered in `sessions.workers` and a background monitor task
/// watches the child process.
pub(crate) async fn spawn_worker(
    config: SpawnWorkerConfig,
    sessions: Sessions,
    daemon_info: DaemonInfo,
) -> Result<SpawnResult, String> {
    let session_id = Sessions::next_id();
    let model = config.model.clone().unwrap_or_else(|| {
        format!(
            "agent-role:{}@{}",
            std::env::var("MU_SPAWN_ROLE").unwrap_or_else(|_| "coding".into()),
            std::env::var("MU_SPAWN_RANK").unwrap_or_else(|_| "0".into())
        )
    });
    let worker_name = config
        .worker_name
        .unwrap_or_else(|| format!("mu-worker-{}", session_id));
    let timeout_secs = config.timeout_secs.unwrap_or(3600);

    let event_log = Arc::new(SessionEventLog::new(session_id.clone()));

    if let Some(events_dir) = daemon_info.events_dir() {
        let path = events_dir
            .join(daemon_info.daemon_id())
            .join(format!("{}.jsonl", session_id));
        if let Err(e) = event_log.attach_disk_writer(&path) {
            tracing::warn!(
                session_id = %session_id,
                path = %path.display(),
                error = %e,
                "worker: could not attach disk writer; continuing in-memory only",
            );
        }
    }

    let daemon_id_str = daemon_info.daemon_id().to_string();

    event_log.append(
        EventActor::System,
        EventPayload::SessionCreated {
            provider_kind: "worker-subprocess".into(),
            model: model.clone(),
            parent_session_id: config.parent_session_id.clone(),
            branched_at_parent_event_id: None,
            // The wrapper can dispatch to multiple runtimes; no single provider
            // usage convention honestly applies at session-create time.
            usage_semantics: None,
        },
    );

    // Create mailbox and register the session BEFORE spawning mu-spawn
    // so the task can be posted to the mailbox before the worker starts.
    let mailbox = Arc::new(MailboxState::new());
    let started_at = now_unix_ms();

    sessions.insert_worker(
        session_id.clone(),
        SubprocessSession {
            event_log: event_log.clone(),
            mailbox: mailbox.clone(),
            parent_session_id: config.parent_session_id.clone(),
            worker_name: worker_name.clone(),
            status: Mutex::new(WorkerStatus::Spawning),
            started_at_unix_ms: started_at,
            child_handle: None,
        },
    );

    // Record the task in the worker's own mailbox/event log for observability.
    // The non-POT child receives the same prompt on stdin; stdout is the fallback
    // result channel when dialogue/MCP is unavailable.
    let reply_to = config
        .parent_session_id
        .clone()
        .unwrap_or_else(|| "supervisor".into());

    // mu-session-context-orchestration-rurq.7: journal the parent-side
    // assignment obligation BEFORE spawning the process. This is the
    // "I owe a result" record. On daemon restart, a WorkerAssignmentObligation
    // without a matching WorkerTerminalResult is an orphaned obligation.
    //
    // Resolution: parent log → supervisor log (same fallback as the
    // terminal result delivery). The target_log_session_id field records
    // where this obligation landed so the recovery pass can locate it
    // even if the parent is evicted before the worker completes (the
    // terminal result may then land in the supervisor log instead).
    let deadline_unix_ms = started_at.saturating_add(timeout_secs.saturating_mul(1000));
    let obligation_log = sessions
        .live_event_log(&reply_to)
        .or_else(|| sessions.live_event_log("supervisor"));
    if let Some(log) = &obligation_log {
        let target_id = log.session_id().to_string();
        log.append_durable_or_fallback(
            EventActor::System,
            EventPayload::WorkerAssignmentObligation {
                worker_session_id: session_id.clone(),
                prompt_summary: truncate(&config.prompt, 200).to_string(),
                deadline_unix_ms,
                target_log_session_id: target_id,
            },
        );
    } else {
        // Neither parent nor supervisor in memory (ephemeral daemon).
        // Record in the worker's own log as a last resort.
        let target_id = session_id.clone();
        event_log.append_durable_or_fallback(
            EventActor::System,
            EventPayload::WorkerAssignmentObligation {
                worker_session_id: session_id.clone(),
                prompt_summary: truncate(&config.prompt, 200).to_string(),
                deadline_unix_ms,
                target_log_session_id: target_id,
            },
        );
    }

    let task_seq = mailbox.allocate_seq();
    event_log.append(
        EventActor::System,
        EventPayload::MailboxMessagePosted {
            seq: task_seq,
            from_daemon_id: daemon_id_str.clone(),
            from_session_id: reply_to.clone(),
            message_kind: "task".into(),
            subject: truncate(&config.prompt, 100),
            body: serde_json::json!({
                "instruction": config.prompt,
                "reply_to": reply_to,
                "daemon_id": daemon_id_str,
            }),
            expires_at_unix_ms: None,
        },
    );

    // Launch current non-POT worker through mu-spawn. The script resolves the
    // provider/model via scripts/agent-role (unless model/provider env override
    // says otherwise) and dispatches through `mu ask` or `claude -p`. The prompt
    // is passed on stdin to avoid ARG_MAX and shell-quoting failure modes.
    let mut cmd = Command::new(resolve_mu_spawn_binary());
    cmd.arg("--session-id")
        .arg(&session_id)
        .arg("--daemon-id")
        .arg(daemon_info.daemon_id())
        .arg("--reply-to")
        .arg(&reply_to)
        .env("MU_DAEMON_ID", daemon_info.daemon_id())
        .env("MU_SESSION_ID", &session_id)
        .env("MU_REPLY_TO", &reply_to)
        .env("MU_SPAWN_TOOLS", config.tools.join(","))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(provider) = config.provider.as_deref() {
        cmd.env("MU_SPAWN_PROVIDER", provider);
        if let Some(model) = config.model.as_deref() {
            cmd.env("MU_SPAWN_MODEL", model);
        }
    }

    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            let reason = format!("failed to run mu-spawn: {}", e);
            // Compensating terminal result: the obligation was journaled
            // but the worker never started.
            if let Some(log) = sessions
                .live_event_log(&reply_to)
                .or_else(|| sessions.live_event_log("supervisor"))
            {
                log.append_durable_or_fallback(
                    EventActor::System,
                    EventPayload::WorkerTerminalResult {
                        worker_session_id: session_id.clone(),
                        status: WorkerTerminalStatus::Failed { exit_code: None },
                        stdout: String::new(),
                        stderr: reason.clone(),
                        elapsed_ms: now_unix_ms().saturating_sub(started_at),
                    },
                );
            } else {
                event_log.append_durable_or_fallback(
                    EventActor::System,
                    EventPayload::WorkerTerminalResult {
                        worker_session_id: session_id.clone(),
                        status: WorkerTerminalStatus::Failed { exit_code: None },
                        stdout: String::new(),
                        stderr: reason.clone(),
                        elapsed_ms: now_unix_ms().saturating_sub(started_at),
                    },
                );
            }
            sessions.set_worker_status(
                &session_id,
                WorkerStatus::Failed {
                    reason: reason.clone(),
                },
            );
            return Err(reason);
        }
    };
    let mut child = child;
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        if let Err(e) = stdin.write_all(config.prompt.as_bytes()).await {
            let reason = format!("failed to write prompt to mu-spawn: {}", e);
            // Compensating terminal result: stdin write failed.
            if let Some(log) = sessions
                .live_event_log(&reply_to)
                .or_else(|| sessions.live_event_log("supervisor"))
            {
                log.append_durable_or_fallback(
                    EventActor::System,
                    EventPayload::WorkerTerminalResult {
                        worker_session_id: session_id.clone(),
                        status: WorkerTerminalStatus::Failed { exit_code: None },
                        stdout: String::new(),
                        stderr: reason.clone(),
                        elapsed_ms: now_unix_ms().saturating_sub(started_at),
                    },
                );
            } else {
                event_log.append_durable_or_fallback(
                    EventActor::System,
                    EventPayload::WorkerTerminalResult {
                        worker_session_id: session_id.clone(),
                        status: WorkerTerminalStatus::Failed { exit_code: None },
                        stdout: String::new(),
                        stderr: reason.clone(),
                        elapsed_ms: now_unix_ms().saturating_sub(started_at),
                    },
                );
            }
            sessions.set_worker_status(
                &session_id,
                WorkerStatus::Failed {
                    reason: reason.clone(),
                },
            );
            return Err(reason);
        }
    }

    event_log.append(
        EventActor::System,
        EventPayload::WorkerSpawned {
            worker_name: worker_name.clone(),
            model: model.clone(),
            pid: child.id(),
            prompt_summary: Some(truncate(&config.prompt, 200)),
        },
    );

    let monitor_log = event_log.clone();
    sessions.set_worker_status(&session_id, WorkerStatus::Running);
    let monitor_session_id = session_id.clone();
    let monitor_sessions = sessions.clone();
    let monitor_reply_to = reply_to.clone();
    tokio::spawn(async move {
        monitor_worker(MonitorArgs {
            child,
            event_log: monitor_log,
            session_id: monitor_session_id,
            sessions: monitor_sessions,
            started_at,
            timeout_secs,
            reply_to: monitor_reply_to,
            daemon_id: daemon_id_str.clone(),
        })
        .await;
    });

    Ok(SpawnResult {
        session_id,
        worker_name,
    })
}

/// Inputs to [`monitor_worker`], bundled so the spawned task takes one struct
/// instead of eight positional args.
struct MonitorArgs {
    child: tokio::process::Child,
    event_log: Arc<SessionEventLog>,
    session_id: String,
    sessions: Sessions,
    started_at: u64,
    timeout_secs: u64,
    reply_to: String,
    daemon_id: String,
}

async fn monitor_worker(args: MonitorArgs) {
    let MonitorArgs {
        mut child,
        event_log,
        session_id,
        sessions,
        started_at,
        timeout_secs,
        reply_to,
        daemon_id,
    } = args;

    let stdout_task = child.stdout.take().map(|mut out| {
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut s = String::new();
            let _ = out.read_to_string(&mut s).await;
            s
        })
    });
    let stderr_task = child.stderr.take().map(|mut out| {
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut s = String::new();
            let _ = out.read_to_string(&mut s).await;
            s
        })
    });

    let status_result = match tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        child.wait(),
    )
    .await
    {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => {
            let reason = format!("mu-spawn wait error: {}", e);
            let elapsed = now_unix_ms().saturating_sub(started_at);
            tracing::error!(session_id = %session_id, error = %reason, "worker wait failed");
            event_log.append(
                EventActor::System,
                EventPayload::WorkerFailed {
                    reason: reason.clone(),
                },
            );
            sessions.set_worker_status(
                &session_id,
                WorkerStatus::Failed {
                    reason: reason.clone(),
                },
            );
            notify_parent(TerminalNotification {
                sessions: &sessions,
                reply_to: &reply_to,
                worker_session_id: &session_id,
                daemon_id: &daemon_id,
                status: WorkerTerminalStatus::Failed { exit_code: None },
                stderr: &reason,
                elapsed_ms: elapsed,
                subject: &format!("Worker failed: {reason}"),
                worker_log: &event_log,
            });
            return;
        }
        Err(_) => {
            let elapsed = now_unix_ms().saturating_sub(started_at);
            let _ = child.kill().await;
            tracing::warn!(session_id = %session_id, elapsed_ms = elapsed, timeout_secs, "worker exceeded deadline, killing");
            event_log.append(
                EventActor::System,
                EventPayload::WorkerTimeout {
                    elapsed_ms: elapsed,
                },
            );
            sessions.set_worker_status(
                &session_id,
                WorkerStatus::Failed {
                    reason: format!("deadline exceeded ({}s)", timeout_secs),
                },
            );
            notify_parent(TerminalNotification {
                sessions: &sessions,
                reply_to: &reply_to,
                worker_session_id: &session_id,
                daemon_id: &daemon_id,
                status: WorkerTerminalStatus::Timeout,
                stderr: "",
                elapsed_ms: elapsed,
                subject: &format!("Worker timed out after {}s", timeout_secs),
                worker_log: &event_log,
            });
            return;
        }
    };

    let stdout = match stdout_task {
        Some(t) => t.await.unwrap_or_default(),
        None => String::new(),
    };
    let stderr = match stderr_task {
        Some(t) => t.await.unwrap_or_default(),
        None => String::new(),
    };

    let elapsed = now_unix_ms().saturating_sub(started_at);
    if status_result.success() {
        let exit_code = status_result.code().unwrap_or(0);
        tracing::info!(session_id = %session_id, elapsed_ms = elapsed, "worker completed");
        event_log.append(
            EventActor::System,
            EventPayload::WorkerExited {
                exit_code,
                elapsed_ms: elapsed,
            },
        );
        sessions.set_worker_status(
            &session_id,
            WorkerStatus::Done {
                exit_code,
                elapsed_ms: elapsed,
            },
        );
        post_result_to_parent(SuccessDelivery {
            sessions: &sessions,
            reply_to: &reply_to,
            worker_session_id: &session_id,
            daemon_id: &daemon_id,
            stdout: &stdout,
            stderr: &stderr,
            elapsed_ms: elapsed,
            worker_log: &event_log,
        });
    } else {
        let reason = format!(
            "exit code {:?}: {}",
            status_result.code(),
            truncate(&stderr, 500)
        );
        tracing::warn!(session_id = %session_id, exit_code = ?status_result.code(), elapsed_ms = elapsed, "worker failed");
        event_log.append(
            EventActor::System,
            EventPayload::WorkerFailed {
                reason: reason.clone(),
            },
        );
        sessions.set_worker_status(
            &session_id,
            WorkerStatus::Failed {
                reason: reason.clone(),
            },
        );
        notify_parent(TerminalNotification {
            sessions: &sessions,
            reply_to: &reply_to,
            worker_session_id: &session_id,
            daemon_id: &daemon_id,
            status: WorkerTerminalStatus::Failed {
                exit_code: status_result.code(),
            },
            stderr: &stderr,
            elapsed_ms: elapsed,
            subject: &format!("Worker failed ({})", truncate(&reason, 100)),
            worker_log: &event_log,
        });
    }
}

struct SuccessDelivery<'a> {
    sessions: &'a Sessions,
    reply_to: &'a str,
    worker_session_id: &'a str,
    daemon_id: &'a str,
    stdout: &'a str,
    stderr: &'a str,
    elapsed_ms: u64,
    worker_log: &'a Arc<SessionEventLog>,
}

fn post_result_to_parent(args: SuccessDelivery<'_>) {
    let SuccessDelivery {
        sessions,
        reply_to,
        worker_session_id,
        daemon_id,
        stdout,
        stderr,
        elapsed_ms,
        worker_log,
    } = args;
    // mu-session-context-orchestration-rurq.7: journal the durable
    // terminal result on the parent's log BEFORE any in-memory wake.
    // If the parent is gone, fall back to the supervisor/dead-letter log.
    // Uses live_event_log (gates rehydrated tier on has_disk_writer)
    // so the append reaches a log with a disk writer.
    let parent_log = sessions
        .live_event_log(reply_to)
        .or_else(|| sessions.live_event_log("supervisor"));
    if let Some(log) = &parent_log {
        log.append_durable_or_fallback(
            EventActor::System,
            EventPayload::WorkerTerminalResult {
                worker_session_id: worker_session_id.to_string(),
                status: WorkerTerminalStatus::Success,
                stdout: stdout.to_string(),
                stderr: stderr.to_string(),
                elapsed_ms,
            },
        );
    } else {
        // Last resort: write to the worker's own log so the terminal
        // record is not lost. The recovery pass can find it via the
        // obligation's target_log_session_id.
        worker_log.append_durable_or_fallback(
            EventActor::System,
            EventPayload::WorkerTerminalResult {
                worker_session_id: worker_session_id.to_string(),
                status: WorkerTerminalStatus::Success,
                stdout: stdout.to_string(),
                stderr: stderr.to_string(),
                elapsed_ms,
            },
        );
        tracing::warn!(
            worker_session_id = %worker_session_id,
            reply_to = %reply_to,
            "worker terminal result: no parent or supervisor log; wrote to worker's own log"
        );
    }

    // In-memory wake (lossy — the durable record above is the source of truth).
    if let Some(mailbox) = sessions.mailbox(reply_to) {
        let seq = mailbox.allocate_seq();
        if let Some(event_log) = sessions.event_log(reply_to) {
            event_log.append(
                EventActor::System,
                EventPayload::MailboxMessagePosted {
                    seq,
                    from_daemon_id: daemon_id.to_string(),
                    from_session_id: worker_session_id.to_string(),
                    message_kind: "task_result".into(),
                    subject: "worker result".into(),
                    body: serde_json::json!({
                        "stdout": stdout,
                        "stderr": stderr,
                    }),
                    expires_at_unix_ms: None,
                },
            );
        }
    }
    if let Some(tx) = sessions.input_sender(reply_to) {
        let summary = format!(
            "Worker {worker_session_id} completed.
stdout:
{stdout}
stderr:
{stderr}"
        );
        let _ = tx.try_send(mu_core::agent::AgentInput::WatchCompleted {
            note: format!("worker {worker_session_id}"),
            summary,
        });
    }
}

/// mu-session-context-orchestration-rurq.7: inputs for delivering a
/// failure/timeout terminal outcome durably.
struct TerminalNotification<'a> {
    sessions: &'a Sessions,
    reply_to: &'a str,
    worker_session_id: &'a str,
    daemon_id: &'a str,
    status: WorkerTerminalStatus,
    stderr: &'a str,
    elapsed_ms: u64,
    subject: &'a str,
    worker_log: &'a Arc<SessionEventLog>,
}

/// mu-session-context-orchestration-rurq.7: deliver a failure/timeout
/// terminal outcome durably. Journals `WorkerTerminalResult` on the
/// parent's log (or supervisor dead-letter) BEFORE the in-memory wake.
/// The wake carries a real mailbox seq (not seq 0) so the model can
/// actually read the message.
fn notify_parent(args: TerminalNotification<'_>) {
    let TerminalNotification {
        sessions,
        reply_to,
        worker_session_id,
        daemon_id,
        status,
        stderr,
        elapsed_ms,
        subject,
        worker_log,
    } = args;
    // Durable terminal record first.
    // Uses live_event_log (gates rehydrated tier on has_disk_writer)
    // to avoid appending into a writer-less ghost.
    let parent_log = sessions
        .live_event_log(reply_to)
        .or_else(|| sessions.live_event_log("supervisor"));
    if let Some(log) = &parent_log {
        log.append_durable_or_fallback(
            EventActor::System,
            EventPayload::WorkerTerminalResult {
                worker_session_id: worker_session_id.to_string(),
                status: status.clone(),
                stdout: String::new(),
                stderr: stderr.to_string(),
                elapsed_ms,
            },
        );
    } else {
        // Last resort: write to the worker's own log.
        worker_log.append_durable_or_fallback(
            EventActor::System,
            EventPayload::WorkerTerminalResult {
                worker_session_id: worker_session_id.to_string(),
                status: status.clone(),
                stdout: String::new(),
                stderr: stderr.to_string(),
                elapsed_ms,
            },
        );
        tracing::warn!(
            worker_session_id = %worker_session_id,
            reply_to = %reply_to,
            "worker terminal result: no parent or supervisor log; wrote to worker's own log"
        );
    }

    // In-memory wake with a real mailbox seq (not seq 0).
    if let Some(mailbox) = sessions.mailbox(reply_to) {
        let seq = mailbox.allocate_seq();
        if let Some(event_log) = sessions.event_log(reply_to) {
            event_log.append(
                EventActor::System,
                EventPayload::MailboxMessagePosted {
                    seq,
                    from_daemon_id: daemon_id.to_string(),
                    from_session_id: worker_session_id.to_string(),
                    message_kind: "worker_status".into(),
                    subject: subject.to_string(),
                    body: serde_json::json!({
                        "status": serde_json::to_value(&status).unwrap_or_else(|_| serde_json::Value::String("unknown".into())),
                        "stderr": stderr,
                        "elapsed_ms": elapsed_ms,
                    }),
                    expires_at_unix_ms: None,
                },
            );
        }
        if let Some(tx) = sessions.input_sender(reply_to) {
            let _ = tx.try_send(mu_core::agent::AgentInput::MailboxMessage {
                from_session_id: worker_session_id.to_string(),
                message_kind: "worker_status".into(),
                subject: subject.to_string(),
                seq,
            });
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;
    use std::time::Duration;

    async fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }

    fn shell_escape(path: &std::path::Path) -> String {
        format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
    }

    #[tokio::test]
    async fn spawn_worker_runs_mu_spawn_and_marks_done() {
        let _guard = env_lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let script = tmp.path().join("mu-spawn-test");
        let env_file = tmp.path().join("tools.env");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncat >/dev/null\nprintf '%s' \"$MU_SPAWN_TOOLS\" > {}\necho hello-from-worker\n",
                shell_escape(&env_file),
            ),
        )
        .expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).expect("metadata").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).expect("chmod");
        }

        std::env::set_var("MU_SPAWN", &script);
        let sessions = Sessions::new();
        let daemon_info = DaemonInfo::new("test-daemon");
        let result = spawn_worker(
            SpawnWorkerConfig {
                prompt: "say hello".into(),
                provider: Some("test-provider".into()),
                model: Some("test-model".into()),
                worker_name: None,
                timeout_secs: Some(5),
                parent_session_id: None,
                tools: vec!["read".into(), "grep".into()],
            },
            sessions.clone(),
            daemon_info,
        )
        .await
        .expect("spawn worker");

        let mut status = None;
        for _ in 0..50 {
            status = sessions.worker_status(&result.session_id);
            if matches!(status, Some(WorkerStatus::Done { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        std::env::remove_var("MU_SPAWN");

        assert!(
            matches!(status, Some(WorkerStatus::Done { exit_code: 0, .. })),
            "worker should finish cleanly, got {status:?}"
        );
        assert_eq!(
            std::fs::read_to_string(env_file).expect("tools env file"),
            "read,grep",
            "spawn_worker must pass the attenuated child tool grant to mu-spawn"
        );
    }

    #[test]
    fn child_tool_grant_is_parent_attenuated_and_ordered() {
        let parent = normalize_worker_tool_grant(vec![
            "grep".into(),
            "read".into(),
            "spawn_worker".into(),
            "bash".into(),
        ]);
        assert_eq!(parent, vec!["read", "grep", "bash"]);

        let cap = Capability {
            allowed_tools: Some(HashSet::from([
                "read".to_string(),
                "grep".to_string(),
                "spawn_worker".to_string(),
            ])),
            ..Default::default()
        };
        assert_eq!(
            derive_child_tool_grant(&parent, Some(&cap)),
            vec!["read", "grep"]
        );
    }

    #[test]
    fn child_tool_grant_preserves_write_parent_bash_authority() {
        let parent = normalize_worker_tool_grant(vec![
            "read".into(),
            "write".into(),
            "edit".into(),
            "glob".into(),
            "grep".into(),
            "ls".into(),
            "bash".into(),
        ]);
        let cap = Capability {
            allowed_tools: Some(parent.iter().cloned().collect()),
            ..Default::default()
        };
        assert_eq!(
            derive_child_tool_grant(&parent, Some(&cap)),
            vec!["read", "write", "edit", "glob", "grep", "ls", "bash"]
        );
    }

    #[test]
    fn direct_spawn_without_parent_uses_minimal_floor() {
        assert_eq!(
            derive_child_tool_grant_from_capability(None),
            vec!["read".to_string(), "grep".to_string()]
        );
    }

    // mu-session-context-orchestration-rurq.7: dead-parent test.
    // When the parent session is not in the registry, the terminal result
    // must still be journaled durably (to the supervisor dead-letter log)
    // rather than silently dropped.
    #[tokio::test]
    async fn dead_parent_terminal_result_is_durable() {
        let _guard = env_lock().await;
        let sessions = Sessions::new();
        let daemon_info = DaemonInfo::new("test-daemon");

        // Create a supervisor session (the dead-letter fallback target).
        // Attach a disk writer so live_event_log will find it.
        let tmp = tempfile::tempdir().expect("tempdir");
        let sup_log = Arc::new(SessionEventLog::new("supervisor".to_string()));
        sup_log
            .attach_disk_writer(&tmp.path().join("supervisor.jsonl"))
            .expect("attach disk writer");
        sessions.insert_rehydrated("supervisor".to_string(), sup_log.clone(), None);

        // Simulate a worker terminal result delivery with a dead parent.
        post_result_to_parent(SuccessDelivery {
            sessions: &sessions,
            reply_to: "session-dead",
            worker_session_id: "session-worker-1",
            daemon_id: daemon_info.daemon_id(),
            stdout: "hello from worker",
            stderr: "",
            elapsed_ms: 1234,
            worker_log: &sup_log,
        });

        // The terminal result must be in the supervisor log.
        let events = sup_log.snapshot();
        let terminal: Vec<_> = events
            .iter()
            .filter(|e| matches!(e.payload, EventPayload::WorkerTerminalResult { .. }))
            .collect();
        assert_eq!(
            terminal.len(),
            1,
            "exactly one WorkerTerminalResult in supervisor log"
        );
        match &terminal[0].payload {
            EventPayload::WorkerTerminalResult {
                worker_session_id,
                status,
                stdout,
                elapsed_ms,
                ..
            } => {
                assert_eq!(worker_session_id, "session-worker-1");
                assert_eq!(*status, WorkerTerminalStatus::Success);
                assert_eq!(stdout, "hello from worker");
                assert_eq!(*elapsed_ms, 1234);
            }
            _ => unreachable!(),
        }
    }

    // mu-session-context-orchestration-rurq.7: notify_parent posts a real
    // mailbox seq (not seq 0) so the model can actually read the message.
    #[tokio::test]
    async fn notify_parent_uses_real_mailbox_seq() {
        let _guard = env_lock().await;
        let sessions = Sessions::new();
        let daemon_info = DaemonInfo::new("test-daemon");

        // Create a live parent session with a disk writer.
        let tmp = tempfile::tempdir().expect("tempdir");
        let parent_log = Arc::new(SessionEventLog::new("session-parent".to_string()));
        parent_log
            .attach_disk_writer(&tmp.path().join("session-parent.jsonl"))
            .expect("attach disk writer");
        sessions.insert_rehydrated("session-parent".to_string(), parent_log.clone(), None);

        // Simulate a timeout notification.
        notify_parent(TerminalNotification {
            sessions: &sessions,
            reply_to: "session-parent",
            worker_session_id: "session-worker-2",
            daemon_id: daemon_info.daemon_id(),
            status: WorkerTerminalStatus::Timeout,
            stderr: "timed out",
            elapsed_ms: 5000,
            subject: "Worker timed out after 5s",
            worker_log: &parent_log,
        });

        // The parent log must have a WorkerTerminalResult AND a
        // MailboxMessagePosted with a real (non-zero) seq.
        let events = parent_log.snapshot();
        let terminal: Vec<_> = events
            .iter()
            .filter(|e| matches!(e.payload, EventPayload::WorkerTerminalResult { .. }))
            .collect();
        assert_eq!(terminal.len(), 1);
        match &terminal[0].payload {
            EventPayload::WorkerTerminalResult { status, .. } => {
                assert_eq!(*status, WorkerTerminalStatus::Timeout);
            }
            _ => unreachable!(),
        }

        let mailbox_msgs: Vec<_> = events
            .iter()
            .filter(|e| matches!(e.payload, EventPayload::MailboxMessagePosted { .. }))
            .collect();
        assert_eq!(mailbox_msgs.len(), 1);
        match &mailbox_msgs[0].payload {
            EventPayload::MailboxMessagePosted { seq, .. } => {
                assert!(
                    *seq > 0,
                    "mailbox seq must be a real allocated seq, got {seq}"
                );
            }
            _ => unreachable!(),
        }
    }

    // mu-session-context-orchestration-rurq.7: obligation is journaled
    // on the parent's log at spawn time.
    #[tokio::test]
    async fn spawn_journals_obligation_on_parent_log() {
        let _guard = env_lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let script = tmp.path().join("mu-spawn-test");
        std::fs::write(&script, "#!/bin/sh\ncat >/dev/null\necho done\n").expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).expect("metadata").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).expect("chmod");
        }

        std::env::set_var("MU_SPAWN", &script);
        let sessions = Sessions::new();
        let daemon_info = DaemonInfo::new("test-daemon");

        // Create a live parent session with a disk writer.
        let parent_log = Arc::new(SessionEventLog::new("session-parent".to_string()));
        parent_log
            .attach_disk_writer(&tmp.path().join("session-parent.jsonl"))
            .expect("attach disk writer");
        sessions.insert_rehydrated("session-parent".to_string(), parent_log.clone(), None);

        let result = spawn_worker(
            SpawnWorkerConfig {
                prompt: "do the thing".into(),
                provider: Some("test".into()),
                model: Some("test-model".into()),
                worker_name: None,
                timeout_secs: Some(5),
                parent_session_id: Some("session-parent".into()),
                tools: vec!["read".into()],
            },
            sessions.clone(),
            daemon_info,
        )
        .await
        .expect("spawn worker");

        std::env::remove_var("MU_SPAWN");

        // The parent log must have a WorkerAssignmentObligation.
        let events = parent_log.snapshot();
        let obligations: Vec<_> = events
            .iter()
            .filter(|e| matches!(e.payload, EventPayload::WorkerAssignmentObligation { .. }))
            .collect();
        assert_eq!(
            obligations.len(),
            1,
            "exactly one WorkerAssignmentObligation on parent log"
        );
        match &obligations[0].payload {
            EventPayload::WorkerAssignmentObligation {
                worker_session_id,
                prompt_summary,
                deadline_unix_ms,
                target_log_session_id,
            } => {
                assert_eq!(worker_session_id, &result.session_id);
                assert!(prompt_summary.contains("do the thing"));
                assert!(*deadline_unix_ms > 0);
                assert_eq!(
                    target_log_session_id, "session-parent",
                    "obligation must record which log it landed in"
                );
            }
            _ => unreachable!(),
        }

        // Wait briefly for the worker to finish so we don't leak the child.
        for _ in 0..50 {
            if matches!(
                sessions.worker_status(&result.session_id),
                Some(WorkerStatus::Done { .. }) | Some(WorkerStatus::Failed { .. })
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
