//! mu-slat Phase 2: agent tool for spawning pot-hosted workers.
//!
//! The LLM calls this tool to delegate work to a new worker session.
//! The tool calls the existing `spawn_worker` function and returns
//! the session_id and pot_name. Results arrive back via mailbox
//! (injected as AgentInput::MailboxMessage by the mailbox.post handler).

use std::future::Future;
use std::pin::Pin;

use mu_core::agent::{Tool, ToolResult, ToolSpec};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::serve::DaemonInfo;
use crate::serve::Sessions;
use crate::serve::worker::{SpawnWorkerConfig, spawn_worker};

pub struct SpawnWorkerTool {
    sessions: Sessions,
    daemon_info: DaemonInfo,
}

impl SpawnWorkerTool {
    pub fn new(sessions: Sessions, daemon_info: DaemonInfo) -> Self {
        Self {
            sessions,
            daemon_info,
        }
    }
}

impl Tool for SpawnWorkerTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "spawn_worker",
            "Spawn a pot-hosted claude-code worker to perform a task autonomously. \
             The worker runs in an isolated FreeBSD jail with full tool access. \
             Post results back to your mailbox when done. Returns session_id and pot_name.",
            json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "The task instruction for the worker."
                    },
                    "model": {
                        "type": "string",
                        "description": "Model to use (default: claude-opus-4-7)."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Maximum wall-clock time in seconds (default: 3600)."
                    }
                },
                "required": ["prompt"]
            }),
        )
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
        let sessions = self.sessions.clone();
        let daemon_info = self.daemon_info.clone();

        Box::pin(async move {
            let prompt = match arguments.get("prompt").and_then(Value::as_str) {
                Some(p) => p.to_string(),
                None => {
                    return ToolResult {
                        content: "missing required argument: prompt".into(),
                        is_error: true,
                    };
                }
            };

            let model = arguments
                .get("model")
                .and_then(Value::as_str)
                .map(String::from);

            let timeout_secs = arguments
                .get("timeout_secs")
                .and_then(Value::as_u64);

            let config = SpawnWorkerConfig {
                prompt: prompt.clone(),
                model,
                pot_name: None,
                timeout_secs,
                parent_session_id: None,
            };

            let spawn_fut = spawn_worker(config, sessions, daemon_info);

            tokio::select! {
                result = spawn_fut => match result {
                    Ok(r) => ToolResult {
                        content: format!(
                            "Worker spawned successfully.\n\
                             session_id: {}\n\
                             pot_name: {}\n\n\
                             The task has been posted to the worker's mailbox. \
                             Results will arrive in your mailbox when the worker finishes.",
                            r.session_id, r.pot_name,
                        ),
                        is_error: false,
                    },
                    Err(e) => ToolResult {
                        content: format!("Failed to spawn worker: {e}"),
                        is_error: true,
                    },
                },
                _ = cancel_rx => ToolResult {
                    content: "spawn_worker cancelled".into(),
                    is_error: true,
                },
            }
        })
    }
}
