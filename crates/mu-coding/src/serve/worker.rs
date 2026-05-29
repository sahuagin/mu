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
use super::pty_spawn::{self, PtyExit, PtyWorker};
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
            killer: Mutex::new(None),
            reaped: std::sync::atomic::AtomicBool::new(false),
        },
    );

    // Post the task to the worker's own mailbox so it's waiting when
    // the worker starts and checks. The worker's system prompt tells
    // it to read mailbox on startup.
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

    // Phase 1: pot setup via mu-spawn in setup-only mode. This does the
    // gnarly, battle-tested pot lifecycle (clone/start/vnet/DHCP/devfs +
    // MCP config + system prompt files) and exits before launching
    // claude. We then take over the claude launch under a Rust-owned pty.
    let mut cmd = Command::new("mu-spawn");
    cmd.arg(&pot_name)
        .env("MU_SPAWN_MODEL", &model)
        .env("MU_SPAWN_SETUP_ONLY", "1")
        .env("MU_DAEMON_ID", daemon_info.daemon_id())
        .env("MU_SESSION_ID", &session_id)
        .env("MU_REPLY_TO", &reply_to)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    let setup_out = cmd
        .output()
        .await
        .map_err(|e| format!("failed to run mu-spawn setup: {}", e))?;
    if !setup_out.status.success() {
        return Err(format!(
            "mu-spawn setup failed (exit {:?})",
            setup_out.status.code()
        ));
    }

    // Phase 2: launch claude under a Rust-owned pty + vt100 driver.
    let pty_worker = pty_spawn::spawn_pty_worker(pty_spawn::PtyWorkerConfig {
        pot_name: pot_name.clone(),
        model: model.clone(),
        daemon_id: daemon_id_str.clone(),
        session_id: session_id.clone(),
        reply_to: reply_to.clone(),
        kickstart: std::env::var("MU_KICKSTART").unwrap_or_else(|_| "go".into()),
    })?;

    // mu-slat Phase 3: register a host-side killer so the mailbox
    // handler can reap this worker when it posts its result.
    sessions.set_worker_killer(&session_id, pty_worker.clone_killer());

    event_log.append(
        EventActor::System,
        EventPayload::WorkerSpawned {
            pot_name: pot_name.clone(),
            model: model.clone(),
            pid: None,
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
    let monitor_reply_to = reply_to.clone();
    tokio::spawn(async move {
        monitor_worker(
            pty_worker,
            monitor_log,
            monitor_status,
            monitor_session_id,
            monitor_sessions,
            started_at,
            timeout_secs,
            monitor_reply_to,
        )
        .await;
    });

    Ok(SpawnResult {
        session_id,
        pot_name,
    })
}

#[allow(clippy::too_many_arguments)]
async fn monitor_worker(
    mut pty_worker: PtyWorker,
    event_log: Arc<SessionEventLog>,
    status: Arc<Mutex<WorkerStatus>>,
    session_id: String,
    sessions: Sessions,
    started_at: u64,
    timeout_secs: u64,
    reply_to: String,
) {
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
    tokio::pin!(deadline);

    let exit = tokio::select! {
        result = &mut pty_worker.exit_rx => result.unwrap_or(PtyExit::Error {
            reason: "pty waiter dropped without reporting".into(),
        }),
        _ = &mut deadline => {
            let elapsed = now_unix_ms().saturating_sub(started_at);
            tracing::warn!(
                session_id = %session_id,
                elapsed_ms = elapsed,
                timeout_secs = timeout_secs,
                "worker exceeded deadline, killing",
            );
            pty_worker.kill();
            event_log.append(
                EventActor::System,
                EventPayload::WorkerTimeout { elapsed_ms: elapsed },
            );
            if let Ok(mut s) = status.lock() {
                *s = WorkerStatus::Failed {
                    reason: format!("deadline exceeded ({}s)", timeout_secs),
                };
            }
            // Scrape the final screen so the operator sees where it stalled.
            log_final_screen(&session_id, &pty_worker);
            notify_parent(
                &sessions,
                &reply_to,
                &session_id,
                &format!("Worker timed out after {}s", timeout_secs),
            );
            return;
        }
    };

    let elapsed = now_unix_ms().saturating_sub(started_at);
    // An intentional reap (worker posted its result, we killed idle
    // claude) exits via signal — non-zero — but is a success.
    let reaped = sessions.worker_was_reaped(&session_id);
    match exit {
        PtyExit::Exited { success, code } => {
            if success || reaped {
                tracing::info!(session_id = %session_id, elapsed_ms = elapsed, reaped, "worker completed");
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
                notify_parent(
                    &sessions,
                    &reply_to,
                    &session_id,
                    &format!("Worker completed ({}ms)", elapsed),
                );
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
                log_final_screen(&session_id, &pty_worker);
                notify_parent(
                    &sessions,
                    &reply_to,
                    &session_id,
                    &format!("Worker failed (exit code {})", code),
                );
            }
        }
        PtyExit::Error { reason } => {
            tracing::error!(session_id = %session_id, error = %reason, "worker pty wait failed");
            event_log.append(
                EventActor::System,
                EventPayload::WorkerFailed {
                    reason: format!("pty wait error: {}", reason),
                },
            );
            if let Ok(mut s) = status.lock() {
                *s = WorkerStatus::Failed {
                    reason: format!("pty wait error: {}", reason),
                };
            }
            notify_parent(
                &sessions,
                &reply_to,
                &session_id,
                &format!("Worker pty error: {}", reason),
            );
        }
    }
}

/// Scrape the worker's final rendered screen and log it. Observability
/// for the case where a worker stalls or fails without posting results
/// — the operator can see what the TUI last showed.
fn log_final_screen(session_id: &str, pty_worker: &PtyWorker) {
    let screen = pty_worker.scrape();
    let tail: String = screen
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" | ");
    tracing::info!(
        session_id = %session_id,
        screen = %truncate(&tail, 500),
        "worker final screen",
    );
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
