//! mu-slat: spawned worker lifecycle.
//!
//! Spawns a non-POT worker through `mu-spawn`, tracks it as a
//! `SubprocessSession` in the session registry, and monitors the child process
//! for exit/timeout/failure. POT/jail setup is deliberately gone; `mu-spawn`
//! dispatches to current agent runtimes (`mu ask` or `claude -p`).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::process::Command;

use mu_core::event_log::{EventActor, EventPayload, SessionEventLog};
use mu_core::protocol::WorkerStatus;

use super::daemon_info::DaemonInfo;
use super::mailbox::MailboxState;
use super::sessions::{Sessions, SubprocessSession};

/// Everything the caller needs to know after a successful spawn.
pub(crate) struct SpawnResult {
    pub session_id: String,
    pub pot_name: String, // wire-compatible worker name (legacy field)
}

/// Configuration for spawning a worker.
pub(crate) struct SpawnWorkerConfig {
    pub prompt: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub pot_name: Option<String>, // optional worker name (legacy field)
    pub timeout_secs: Option<u64>,
    pub parent_session_id: Option<String>,
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
    let pot_name = config
        .pot_name
        .unwrap_or_else(|| format!("mu-worker-{}", &session_id));
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
            pot_name: pot_name.clone(),
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
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(provider) = config.provider.as_deref() {
        cmd.env("MU_SPAWN_PROVIDER", provider);
        if let Some(model) = config.model.as_deref() {
            cmd.env("MU_SPAWN_MODEL", model);
        }
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to run mu-spawn: {}", e))?;
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin
            .write_all(config.prompt.as_bytes())
            .await
            .map_err(|e| format!("failed to write prompt to mu-spawn: {}", e))?;
    }

    event_log.append(
        EventActor::System,
        EventPayload::WorkerSpawned {
            pot_name: pot_name.clone(),
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
        pot_name,
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
            notify_parent(&sessions, &reply_to, &session_id, &reason);
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
            notify_parent(
                &sessions,
                &reply_to,
                &session_id,
                &format!("Worker timed out after {}s", timeout_secs),
            );
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
        post_result_to_parent(
            &sessions,
            &reply_to,
            &session_id,
            &daemon_id,
            &stdout,
            &stderr,
        );
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
        notify_parent(
            &sessions,
            &reply_to,
            &session_id,
            &format!("Worker failed ({})", truncate(&reason, 100)),
        );
    }
}

fn post_result_to_parent(
    sessions: &Sessions,
    reply_to: &str,
    worker_session_id: &str,
    daemon_id: &str,
    stdout: &str,
    stderr: &str,
) {
    let Some(event_log) = sessions.event_log(reply_to) else {
        return;
    };
    let Some(mailbox) = sessions.mailbox(reply_to) else {
        return;
    };
    let seq = mailbox.allocate_seq();
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

fn notify_parent(sessions: &Sessions, reply_to: &str, worker_session_id: &str, subject: &str) {
    if let Some(tx) = sessions.input_sender(reply_to) {
        let _ = tx.try_send(mu_core::agent::AgentInput::MailboxMessage {
            from_session_id: worker_session_id.to_string(),
            message_kind: "worker_status".into(),
            subject: subject.to_string(),
            seq: 0,
        });
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

    #[tokio::test]
    async fn spawn_worker_runs_mu_spawn_and_marks_done() {
        let _guard = env_lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let script = tmp.path().join("mu-spawn-test");
        std::fs::write(
            &script,
            "#!/bin/sh\ncat >/dev/null\necho hello-from-worker\n",
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
                pot_name: None,
                timeout_secs: Some(5),
                parent_session_id: None,
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
    }
}
