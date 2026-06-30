//! mu-slat Phase 2: agent tool for spawning worker agents.
//!
//! The LLM calls this tool to delegate work to a new worker session.
//! The tool calls the existing `spawn_worker` function and returns
//! the session id and worker name. Results arrive back via mailbox
//! (injected as AgentInput::MailboxMessage by the monitor or mailbox.post handler).

use std::future::Future;
use std::pin::Pin;

use mu_core::agent::{Tool, ToolResult, ToolSpec};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::serve::worker::{derive_child_tool_grant, spawn_worker, SpawnWorkerConfig};
use crate::serve::DaemonInfo;
use crate::serve::WeakSessions;

pub struct SpawnWorkerTool {
    /// Non-owning handle to the registry (mu-qc08). MUST be `Weak`: this
    /// tool lives in its owning session's tool list, so a strong clone
    /// would keep alive the map holding that session's own `input_tx`,
    /// deadlocking shutdown (the loop can't exit until `input_tx` drops,
    /// but the loop's own tool keeps it alive). Upgraded transiently in
    /// `execute`.
    sessions: WeakSessions,
    daemon_info: DaemonInfo,
    /// Session that owns this tool instance — the worker's results are
    /// routed back here via mailbox. `None` falls back to the
    /// "supervisor" alias (no live session), so this should be set to
    /// the calling session's id for results to reach the operator.
    parent_session_id: Option<String>,
    /// Built-in tools from this session that may be delegated to the child.
    parent_tool_grant: Vec<String>,
}

impl SpawnWorkerTool {
    pub fn new(
        sessions: WeakSessions,
        daemon_info: DaemonInfo,
        parent_session_id: Option<String>,
    ) -> Self {
        Self {
            sessions,
            daemon_info,
            parent_session_id,
            parent_tool_grant: Vec::new(),
        }
    }

    pub fn with_parent_tool_grant(mut self, parent_tool_grant: Vec<String>) -> Self {
        self.parent_tool_grant = parent_tool_grant;
        self
    }

    /// Build the spawn config from the model's tool arguments, stamping
    /// in THIS tool's `parent_session_id` as the worker's reply_to so
    /// results route back to the calling session. Factored out of
    /// `execute` so the wiring is unit-testable without spawning a worker process.
    fn build_config(
        &self,
        arguments: &Value,
        parent_capability: Option<&mu_core::capability::Capability>,
    ) -> Result<SpawnWorkerConfig, String> {
        let prompt = arguments
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing required argument: prompt".to_string())?
            .to_string();
        let provider = arguments
            .get("provider")
            .and_then(Value::as_str)
            .map(String::from);
        let model = arguments
            .get("model")
            .and_then(Value::as_str)
            .map(String::from);
        if model.is_some() && provider.is_none() {
            return Err("model override requires provider (for example provider=ollama model=qwen3.6:35b-a3b-q8_0, or omit both for the configured coding role)".into());
        }
        Ok(SpawnWorkerConfig {
            prompt,
            provider,
            model,
            pot_name: None,
            timeout_secs: arguments.get("timeout_secs").and_then(Value::as_u64),
            parent_session_id: self.parent_session_id.clone(),
            tools: derive_child_tool_grant(&self.parent_tool_grant, parent_capability),
        })
    }
}

impl Tool for SpawnWorkerTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "spawn_worker",
            "Spawn a worker agent to perform a task autonomously. \
             The worker is launched through the shared agent-dispatch path \
             (mu ask or claude -p), with a child tool grant derived from and \
             no wider than this session's tools. Results are posted back to \
             your mailbox when done. Returns session_id and worker name.",
            json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "The task instruction for the worker."
                    },
                    "provider": {
                        "type": "string",
                        "description": "Optional provider override. Use with model, e.g. provider=ollama or provider=openai-codex. Omit provider+model for the configured coding role resolved by scripts/agent-role."
                    },
                    "model": {
                        "type": "string",
                        "description": "Optional model override; requires provider. Omit provider+model for the configured coding role resolved by scripts/agent-role."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Maximum wall-clock time in seconds (default: 3600)."
                    }
                },
                "required": ["prompt"]
            }),
        )
        // mu-usfj: spawning a worker with "full tool access" is the
        // Execute class, not the defaulted ReadOnly — under-declaring it
        // let spawn_worker bypass the gate bash faces. Honest now; the
        // gate that acts on Execute is mu-n25a Phase 2.
        .with_policy(mu_core::agent::ToolPolicy {
            side_effects: mu_core::agent::SideEffects::Execute,
            permission: mu_core::agent::PermissionLevel::Allow,
            retry: mu_core::agent::RetryPolicy::ModelDecides,
            required_aws_capability: None,
            idempotent: false,
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
        let weak_sessions = self.sessions.clone();
        let daemon_info = self.daemon_info.clone();

        Box::pin(async move {
            // mu-qc08: upgrade the weak registry handle only now, at
            // point-of-use. `None` means the daemon is tearing down —
            // surface it as a clean tool error, never a panic.
            let sessions = match weak_sessions.upgrade() {
                Some(s) => s,
                None => {
                    return ToolResult {
                        content:
                            "spawn_worker: session registry unavailable (daemon shutting down)"
                                .into(),
                        is_error: true,
                    };
                }
            };

            let parent_capability = self
                .parent_session_id
                .as_deref()
                .and_then(|id| sessions.capability(id))
                .and_then(|cap| cap.lock().ok().map(|c| c.clone()));
            let config = match self.build_config(&arguments, parent_capability.as_ref()) {
                Ok(c) => c,
                Err(e) => {
                    return ToolResult {
                        content: e,
                        is_error: true,
                    };
                }
            };

            let spawn_fut = spawn_worker(config, sessions, daemon_info);

            tokio::select! {
                result = spawn_fut => match result {
                    Ok(r) => ToolResult {
                        content: format!(
                            "Worker spawned successfully.\n\
                             session_id: {}\n\
                             worker_name: {}\n\n\
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::Sessions;
    use serde_json::json;

    fn tool(parent: Option<&str>) -> SpawnWorkerTool {
        // Tests only exercise `build_config` (no `execute`/`upgrade`), so a
        // dead weak from the dropped temporary is fine here.
        SpawnWorkerTool::new(
            Sessions::new().downgrade(),
            DaemonInfo::new("test"),
            parent.map(String::from),
        )
    }

    // The crux of the mu-slat fix: the tool stamps ITS OWN
    // parent_session_id (the calling session) into the spawn config's
    // reply_to. Without this the worker's results route to the dead
    // "supervisor" ghost instead of waking the caller.
    #[test]
    fn build_config_uses_caller_session_id_as_reply_to() {
        let cfg = tool(Some("session-7"))
            .build_config(&json!({ "prompt": "do the thing" }), None)
            .expect("config builds");
        assert_eq!(cfg.parent_session_id.as_deref(), Some("session-7"));
        assert_eq!(cfg.prompt, "do the thing");
        assert!(cfg.provider.is_none());
        assert!(cfg.model.is_none());
        assert!(cfg.timeout_secs.is_none());
        assert_eq!(cfg.tools, vec!["read", "grep"]);
    }

    #[test]
    fn build_config_parses_optional_model_and_timeout() {
        let cfg = tool(Some("session-1"))
            .build_config(
                &json!({
                    "prompt": "p",
                    "provider": "openai-codex",
                    "model": "gpt-5.5",
                    "timeout_secs": 42
                }),
                None,
            )
            .expect("config builds");
        assert_eq!(cfg.provider.as_deref(), Some("openai-codex"));
        assert_eq!(cfg.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(cfg.timeout_secs, Some(42));
    }

    #[test]
    fn build_config_rejects_model_without_provider() {
        let result = tool(Some("session-1")).build_config(
            &json!({
                "prompt": "p",
                "model": "claude-opus-4-7"
            }),
            None,
        );
        let Err(err) = result else {
            panic!("model-only override must fail");
        };
        assert!(err.contains("requires provider"), "{err}");
    }

    #[test]
    fn build_config_missing_prompt_is_error() {
        assert!(tool(Some("session-1"))
            .build_config(&json!({}), None)
            .is_err());
    }

    #[test]
    fn build_config_no_caller_falls_back_to_none() {
        // A tool with no caller id yields parent_session_id: None, which
        // spawn_worker maps to the "supervisor" fallback. This is the
        // pre-fix behavior, kept only for the (non-session) edge case.
        let cfg = tool(None)
            .build_config(&json!({ "prompt": "p" }), None)
            .expect("config builds");
        assert!(cfg.parent_session_id.is_none());
    }

    // mu-qc08 regression guard: the tool must hold a WEAK handle so it
    // never keeps the session registry alive. If this reverts to a strong
    // clone, dropping the last `Sessions` would NOT free the registry,
    // the per-session `input_tx` would never drop, the agent loop would
    // never exit, and `transport::serve` would deadlock on shutdown
    // (the `mu serve did not exit within 5 seconds; killed` symptom).
    #[test]
    fn tool_holds_weak_registry_and_does_not_pin_it() {
        let sessions = Sessions::new();
        let t = SpawnWorkerTool::new(
            sessions.downgrade(),
            DaemonInfo::new("test"),
            Some("s1".into()),
        );
        // While a strong ref lives, the tool can upgrade.
        assert!(t.sessions.upgrade().is_some());
        // Drop the last strong ref: a Weak handle must now fail to
        // upgrade (proving the tool was NOT pinning the registry).
        drop(sessions);
        assert!(
            t.sessions.upgrade().is_none(),
            "SpawnWorkerTool is keeping the registry alive — shutdown will deadlock (mu-qc08)"
        );
    }
}
