//! mu-slat: pot-hosted claude-code worker lifecycle.
//!
//! Spawns an interactive claude-code session inside a FreeBSD pot via
//! `agent-spawn-v2`, tracks it as a `SubprocessSession` in the session
//! registry, and monitors the child process for exit/timeout/failure.
//!
//! Communication between the supervisor (mu daemon) and the worker is
//! via MCP mailbox — the worker connects to the daemon's MCP unix
//! socket through a socat bridge configured at spawn time.

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
    pub pot_name: String,
}

/// Configuration for spawning a worker.
pub(crate) struct SpawnWorkerConfig {
    pub prompt: String,
    pub model: Option<String>,
    pub pot_name: Option<String>,
    pub timeout_secs: Option<u64>,
    pub parent_session_id: Option<String>,
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Spawn a pot-hosted claude-code worker via mu-spawn.
///
/// Returns the session_id and pot_name on success. The worker is
/// registered in `sessions.workers` and a background monitor task
/// watches the child process.
pub(crate) async fn spawn_worker(
    config: SpawnWorkerConfig,
    sessions: Sessions,
    daemon_info: DaemonInfo,
) -> Result<SpawnResult, String> {
    let session_id = Sessions::next_id();
    let model = config.model.unwrap_or_else(|| "claude-opus-4-7".into());
    let pot_name = config
        .pot_name
        .unwrap_or_else(|| format!("mu-worker-{}", &session_id));
    let _timeout_secs = config.timeout_secs.unwrap_or(3600);

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
            provider_kind: "claude-subprocess".into(),
            model: model.clone(),
            parent_session_id: config.parent_session_id.clone(),
            branched_at_parent_event_id: None,
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

    // Post the task to the worker's own mailbox so it's waiting when
    // the worker starts and checks. The worker's system prompt tells
    // it to read mailbox on startup.
    let task_seq = mailbox.allocate_seq();
    event_log.append(
        EventActor::System,
        EventPayload::MailboxMessagePosted {
            seq: task_seq,
            from_daemon_id: daemon_id_str.clone(),
            from_session_id: config
                .parent_session_id
                .clone()
                .unwrap_or_else(|| "supervisor".into()),
            message_kind: "task".into(),
            subject: truncate(&config.prompt, 100),
            body: serde_json::json!({
                "instruction": config.prompt,
                "parent_session_id": config.parent_session_id,
                "daemon_id": daemon_id_str,
            }),
            expires_at_unix_ms: None,
        },
    );

    // Now spawn mu-spawn — the task is already in the mailbox.
    let mut cmd = Command::new("mu-spawn");
    cmd.arg(&pot_name)
        .env("MU_SPAWN_MODEL", &model)
        .env("MU_DAEMON_ID", daemon_info.daemon_id())
        .env("MU_SESSION_ID", &session_id)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn mu-spawn: {}", e))?;

    let pid = child.id();

    event_log.append(
        EventActor::System,
        EventPayload::WorkerSpawned {
            pot_name: pot_name.clone(),
            model: model.clone(),
            pid,
            prompt_summary: Some(truncate(&config.prompt, 200)),
        },
    );

    // Update status from Spawning → Running
    if let Some(st) = sessions.worker_status(&session_id) {
        drop(st);
    }

    let monitor_log = event_log.clone();
    let monitor_status = Arc::new(Mutex::new(WorkerStatus::Running));
    let monitor_session_id = session_id.clone();
    let monitor_sessions = sessions.clone();
    tokio::spawn(async move {
        monitor_worker(
            child,
            monitor_log,
            monitor_status,
            monitor_session_id,
            monitor_sessions,
            started_at,
        )
        .await;
    });

    Ok(SpawnResult {
        session_id,
        pot_name,
    })
}

async fn monitor_worker(
    mut child: tokio::process::Child,
    event_log: Arc<SessionEventLog>,
    status: Arc<Mutex<WorkerStatus>>,
    session_id: String,
    _sessions: Sessions,
    started_at: u64,
) {
    match child.wait().await {
        Ok(exit_status) => {
            let elapsed = now_unix_ms().saturating_sub(started_at);
            let code = exit_status.code().unwrap_or(-1);

            if exit_status.success() {
                tracing::info!(session_id = %session_id, elapsed_ms = elapsed, "worker exited normally");
                event_log.append(
                    EventActor::System,
                    EventPayload::WorkerExited {
                        exit_code: code,
                        elapsed_ms: elapsed,
                    },
                );
                if let Ok(mut s) = status.lock() {
                    *s = WorkerStatus::Done {
                        exit_code: code,
                        elapsed_ms: elapsed,
                    };
                }
            } else if code == 124 {
                tracing::warn!(session_id = %session_id, elapsed_ms = elapsed, "worker timed out");
                event_log.append(
                    EventActor::System,
                    EventPayload::WorkerTimeout { elapsed_ms: elapsed },
                );
                if let Ok(mut s) = status.lock() {
                    *s = WorkerStatus::Failed {
                        reason: format!("timeout after {}ms", elapsed),
                    };
                }
            } else {
                tracing::warn!(session_id = %session_id, exit_code = code, elapsed_ms = elapsed, "worker failed");
                event_log.append(
                    EventActor::System,
                    EventPayload::WorkerFailed {
                        reason: format!("exit code {}", code),
                    },
                );
                if let Ok(mut s) = status.lock() {
                    *s = WorkerStatus::Failed {
                        reason: format!("exit code {}", code),
                    };
                }
            }
        }
        Err(e) => {
            let _elapsed = now_unix_ms().saturating_sub(started_at);
            tracing::error!(session_id = %session_id, error = %e, "worker process wait failed");
            event_log.append(
                EventActor::System,
                EventPayload::WorkerFailed {
                    reason: format!("wait error: {}", e),
                },
            );
            if let Ok(mut s) = status.lock() {
                *s = WorkerStatus::Failed {
                    reason: format!("wait error: {}", e),
                };
            }
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
