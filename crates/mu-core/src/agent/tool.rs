use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;

/// Public description of a tool, sent to the provider so the model
/// knows what tools exist.
///
/// `description` and `input_schema` are the LLM-facing surface — the
/// model reads these to decide which tool to call and with what
/// arguments. `policy` is the RUNTIME-facing surface — mu's
/// AgentLoop and dispatch acts on it directly (retry guard, future
/// permission gating, side-effect classification for UI/audit).
/// See `specs/architecture/capability-delegation.md`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema describing the arguments. The provider feeds this
    /// to the model.
    pub input_schema: Value,
    /// Human-facing display name for /help, /status, TUI tool lists.
    /// Falls back to `name` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    /// Model-facing routing hint: when is this tool the right choice?
    /// Injected into the tool routing index alongside `description`.
    /// When absent, `description` serves both purposes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    /// Runtime-enforced policy. Defaults to a benign "read-only,
    /// always allow, model decides retry" so legacy tools that don't
    /// override it keep working.
    #[serde(default)]
    pub policy: ToolPolicy,
    /// mu-2e0h: when true, this tool's results bypass the tier-1
    /// ingestion filter (ANSI strip / repeat collapse / line cap /
    /// truncation). Set by read-like tools whose output the model
    /// must treat as verbatim disk truth — exact-match `edit` builds
    /// on what `read` showed, so the filter must never alter the
    /// model's belief about file contents. Defaults to false: tool
    /// output gets hygiene unless the tool declares otherwise.
    #[serde(default)]
    pub verbatim_result: bool,
}

impl Default for ToolSpec {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            input_schema: Value::Object(Default::default()),
            display: None,
            when: None,
            policy: ToolPolicy::default(),
            verbatim_result: false,
        }
    }
}

impl ToolSpec {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            ..Default::default()
        }
    }

    pub fn with_display(mut self, display: impl Into<String>) -> Self {
        self.display = Some(display.into());
        self
    }

    pub fn with_when(mut self, when: impl Into<String>) -> Self {
        self.when = Some(when.into());
        self
    }

    /// Builder method: results bypass the tier-1 ingestion filter
    /// (mu-2e0h) — for read-like tools whose output is verbatim disk
    /// truth.
    pub fn with_verbatim_result(mut self) -> Self {
        self.verbatim_result = true;
        self
    }

    /// Builder method to attach a custom policy.
    pub fn with_policy(mut self, policy: ToolPolicy) -> Self {
        self.policy = policy;
        self
    }
}

/// Runtime-enforced metadata about a tool. Not sent to the model
/// directly — the model reads the prose `description` and the JSON
/// `input_schema`. The runtime reads this struct.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolPolicy {
    pub side_effects: SideEffects,
    pub permission: PermissionLevel,
    pub retry: RetryPolicy,
    /// Optional AWS capability name required before dispatching this
    /// tool. This is checked against `Capability::aws` by the agent
    /// loop before `Tool::execute` runs. `None` means no AWS-specific
    /// grant is required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_aws_capability: Option<String>,
    /// True if running this tool with the same arguments twice
    /// produces the same observable state (e.g. read, edit-with-
    /// unique-old_string, write-with-same-content). False if the
    /// tool's effect depends on external state (e.g. bash, since
    /// the file system can change between calls).
    pub idempotent: bool,
}

impl Default for ToolPolicy {
    fn default() -> Self {
        Self {
            side_effects: SideEffects::ReadOnly,
            permission: PermissionLevel::Allow,
            retry: RetryPolicy::ModelDecides,
            required_aws_capability: None,
            idempotent: true,
        }
    }
}

/// Side-effect class. UI/audit/orchestration use this to categorize
/// tools without parsing the description.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffects {
    /// No observable change to the world. Reads, queries, lookups.
    ReadOnly,
    /// Modifies state the user/agent cares about. Writes, edits.
    Mutating,
    /// Irreversible without backup. Deletes, force-pushes, drops.
    Destructive,
    /// Reaches the network. Fetches, posts, calls APIs. (Not yet
    /// used by any tool but reserved for clarity.)
    External,
    /// Runs arbitrary code or spawns a process whose effects are not
    /// statically known — a shell command, a spawned worker, an exec.
    /// The MOST dangerous class: it subsumes every other (a shell can
    /// read, mutate, destroy, AND network), so it can never be treated
    /// as benign. Tools in this class must not ride on
    /// `permission: Allow` once the side-effects gate lands (mu-n25a
    /// Phase 2), and it is the natural seam for OS-enforced
    /// least-privilege — at the exec boundary, grant only the rights it
    /// implies via `cap_enter()` then fork-exec (mu-627). It is also
    /// what a session forks-and-attenuates AWAY from when it
    /// voluntarily drops to read-only (mu-mh4). (mu-usfj)
    Execute,
}

/// Permission posture. v1 only honors `Allow` and `Deny` at the
/// runtime level; `Ask` / `AskOnce` are reserved for the future
/// `session.input_required` flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionLevel {
    Allow,
    Ask,
    AskOnce,
    Deny,
}

/// Retry posture — what should the runtime do if the model issues
/// a duplicate (or shortly-after-error) call to this tool?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RetryPolicy {
    /// The runtime refuses to dispatch a tool call when the same
    /// (tool, args) was just attempted and errored. Specifically:
    /// if any of the last `RETRY_HISTORY_WINDOW` tool calls had the
    /// same name AND same arguments AND `is_error: true`, the new
    /// call is refused at dispatch with a diagnostic callout. Use
    /// this for tools where retrying the same input has no chance
    /// of a different result (e.g. allowlist rejections).
    Never,
    /// Bounded retry — the runtime allows up to `times` matching
    /// retries before refusing. v1 doesn't implement this; reserved.
    UpTo { times: u32 },
    /// The current default: don't gate retries; let the model
    /// decide. Some legitimate cases want retries (transient
    /// failures, race conditions).
    ModelDecides,
}

/// Tool execution result. Errors are EXPRESSED via `is_error: true`
/// rather than propagated — the LLM expects to see the error text and
/// react to it, not get a "the tool failed" rejection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

#[async_trait]
pub trait Tool: Send + Sync {
    /// What the model sees about this tool.
    fn spec(&self) -> ToolSpec;

    /// Argument-aware pre-flight check. Tools that reject specific
    /// argument shapes (e.g. bash's allowlist, aws_recon's
    /// `unsupported_capability`) implement this to short-circuit
    /// doomed calls *before* the dispatcher dispatches them.
    ///
    /// The agent loop calls `validate` BEFORE the `PermissionLevel::Ask`
    /// approval gate. Returning `Err(reason)` causes the call to fail
    /// immediately with `ToolResult { content: reason, is_error: true }`
    /// — the user is never asked to approve a call that would fail.
    ///
    /// Default: `Ok(())` (no pre-checks). Tools whose policy is
    /// argument-agnostic (most of them) can ignore this. Implementations
    /// that delegate to per-call validation in `execute` SHOULD also
    /// keep that call as defense-in-depth — direct unit tests of
    /// `execute` bypass the dispatcher.
    ///
    /// `cancel_rx` is intentionally absent: validation must be cheap
    /// and synchronous-shaped (no I/O, no async). If validation needs
    /// to wait on something, do it in `execute`. (mu-bkjr)
    fn validate(&self, _arguments: &Value) -> Result<(), String> {
        Ok(())
    }

    /// Execute the tool. The Tool impl owns `cancel_rx` and must
    /// abort when it fires.
    async fn execute(&self, arguments: Value, cancel_rx: oneshot::Receiver<()>) -> ToolResult;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_spec_round_trips() -> Result<(), serde_json::Error> {
        let spec = ToolSpec {
            name: "echo".to_owned(),
            description: "Echo input".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" }
                },
                "required": ["text"]
            }),
            policy: ToolPolicy::default(),

            ..Default::default()
        };

        let value = serde_json::to_value(&spec)?;
        let decoded: ToolSpec = serde_json::from_value(value)?;

        assert_eq!(decoded, spec);
        Ok(())
    }

    #[test]
    fn tool_spec_decodes_without_policy_field() -> Result<(), serde_json::Error> {
        // Backward compat: a wire-format ToolSpec that lacks the
        // `policy` field (e.g. emitted by an older client) must
        // still decode and fill in defaults.
        let value = json!({
            "name": "legacy",
            "description": "old shape",
            "input_schema": {"type": "object"},
        });
        let decoded: ToolSpec = serde_json::from_value(value)?;
        assert_eq!(decoded.policy, ToolPolicy::default());
        Ok(())
    }

    #[test]
    fn tool_policy_default_is_safe() {
        let p = ToolPolicy::default();
        assert_eq!(p.side_effects, SideEffects::ReadOnly);
        assert_eq!(p.permission, PermissionLevel::Allow);
        assert!(matches!(p.retry, RetryPolicy::ModelDecides));
        assert_eq!(p.required_aws_capability, None);
        assert!(p.idempotent);
    }

    #[test]
    fn retry_policy_round_trips() -> Result<(), serde_json::Error> {
        let samples = [
            RetryPolicy::Never,
            RetryPolicy::UpTo { times: 3 },
            RetryPolicy::ModelDecides,
        ];
        for policy in samples {
            let value = serde_json::to_value(policy)?;
            let decoded: RetryPolicy = serde_json::from_value(value)?;
            assert_eq!(decoded, policy);
        }
        Ok(())
    }

    #[test]
    fn tool_result_round_trips() -> Result<(), serde_json::Error> {
        let samples = [
            ToolResult {
                content: "done".to_owned(),
                is_error: false,
            },
            ToolResult {
                content: "boom".to_owned(),
                is_error: true,
            },
        ];

        for result in samples {
            let value = serde_json::to_value(&result)?;
            let decoded: ToolResult = serde_json::from_value(value)?;
            assert_eq!(decoded, result);
        }
        Ok(())
    }

    #[test]
    fn tool_trait_is_send_and_sync() {
        fn assert_send<T: Send + Sync + ?Sized>() {}
        assert_send::<dyn Tool>();
    }
}
