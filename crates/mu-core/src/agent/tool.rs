use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use t4c::{Effects, FsEffect};
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

    /// Builder: declare this tool genuinely read-only (`ReadOnly` +
    /// `Allow`). The EXPLICIT opt-in to the benign posture.
    ///
    /// mu-cvm5 (mu-n25a Phase 5): since `ToolPolicy::default()` now fails
    /// CLOSED (`Mutating` + `Ask`), a tool that forgets `.with_policy()`
    /// ships restricted, not benign. A genuinely read-only tool must SAY
    /// SO with this call — making "this tool is benign" a deliberate,
    /// auditable declaration rather than the silent fallthrough that let
    /// the mu-usfj SELF-CLASSIFIED-AUTHORITY bug class exist.
    pub fn read_only(self) -> Self {
        self.with_policy(ToolPolicy::read_only())
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
    /// mu-cvm5 (mu-n25a Phase 5): the default FAILS CLOSED. A tool that
    /// forgets to call `.with_policy()` / `.read_only()` ships RESTRICTED
    /// (`Mutating` side-effects + `Ask` permission), not benign-and-allowed.
    ///
    /// Before this flip the default was `ReadOnly` + `Allow` — the silent
    /// fallthrough that let the mu-usfj SELF-CLASSIFIED-AUTHORITY bug class
    /// exist: a tool that could affect the world but omitted its policy was
    /// treated as benign. Now omission is the conservative posture, so the
    /// failure mode of forgetting is "too restrictive" (a visible Ask
    /// prompt / a refused dispatch under a tight ceiling) rather than
    /// "silently dangerous". Genuinely read-only tools must OPT IN with
    /// `ToolPolicy::read_only()` / `ToolSpec::read_only()`.
    ///
    /// `Mutating` (not `Execute`) is the floor: a forgotten policy is most
    /// likely a write-class tool, and `Mutating` + `Ask` already blocks the
    /// free-ride past a restrictive `max_side_effects` posture. Tools that
    /// are actually exec/destructive still declare so explicitly (bash,
    /// watch, spawn_worker), and the `policy_invariants` test keeps those
    /// declarations honest.
    fn default() -> Self {
        Self {
            side_effects: SideEffects::Mutating,
            permission: PermissionLevel::Ask,
            retry: RetryPolicy::ModelDecides,
            required_aws_capability: None,
            idempotent: false,
        }
    }
}

impl ToolPolicy {
    /// The benign posture: `ReadOnly` + `Allow` + `ModelDecides`,
    /// idempotent. The EXPLICIT opt-in a genuinely read-only tool uses
    /// now that `default()` fails closed (mu-cvm5 / mu-n25a Phase 5).
    pub fn read_only() -> Self {
        Self {
            side_effects: SideEffects::ReadOnly,
            permission: PermissionLevel::Allow,
            retry: RetryPolicy::ModelDecides,
            required_aws_capability: None,
            idempotent: true,
        }
    }

    /// The tool's structured [`Effects`] for the DISCOVERY surface: the
    /// side-effects projection ([`SideEffects::effects`]) plus the one
    /// inference mu can make — a tool gated on an AWS capability reaches the
    /// network and spends. The dispatch gate uses `side_effects.effects()`
    /// directly (the AWS axis has its own gate via `required_aws_capability`),
    /// so this `aws` inference is a discovery-display hint only. (mu-8stm.2)
    pub fn derived_effects(&self) -> Effects {
        let mut e = self.side_effects.effects();
        if self.required_aws_capability.is_some() {
            e.network = true;
            e.spend = true;
        }
        e
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

impl SideEffects {
    /// Danger rank — a total order over the side-effect classes, from
    /// least to most dangerous. Used by the session-permission gate
    /// (mu-n25a Phase 2/3) to decide whether a tool's declared
    /// side-effects EXCEED the session's `max_side_effects` ceiling.
    ///
    /// Total order (ascending danger):
    ///   `ReadOnly` < `Mutating` < `External` < `Destructive` < `Execute`
    ///
    /// Rationale for the contested middle ranks:
    /// - `Mutating` (local writes/edits) below `External` (reaches the
    ///   network) because exfiltration / remote side-effects leave the
    ///   blast radius of the local workspace — a network reach is harder
    ///   to undo and audit than a local edit.
    /// - `External` below `Destructive` because destructive operations
    ///   (irreversible deletes, force-pushes) are unrecoverable without
    ///   backups, the worst outcome short of arbitrary code.
    /// - `Execute` is the maximum: it SUBSUMES every other class (a shell
    ///   can read, mutate, network, AND destroy), so nothing may rank
    ///   above it and no ceiling above it is meaningful.
    ///
    /// (DECISION FOR DIRECTOR: the External-vs-Mutating relative order is
    /// the one genuinely arguable choice; see PR body. The gate only
    /// needs SOME total order, and ReadOnly-as-min / Execute-as-max are
    /// the load-bearing endpoints.)
    pub fn rank(self) -> u8 {
        match self {
            SideEffects::ReadOnly => 0,
            SideEffects::Mutating => 1,
            SideEffects::External => 2,
            SideEffects::Destructive => 3,
            SideEffects::Execute => 4,
        }
    }

    /// True iff `self` is at most as dangerous as `ceiling` — i.e. a tool
    /// declaring `self` side-effects is permitted under a session whose
    /// `max_side_effects` ceiling is `ceiling`. Equivalent to
    /// `self.rank() <= ceiling.rank()`.
    pub fn within(self, ceiling: SideEffects) -> bool {
        self.rank() <= ceiling.rank()
    }

    /// Project this coarse class onto t4c's structured [`Effects`] — the
    /// canonical, multi-axis representation the dispatch gate and discovery
    /// surface both reason over (mu-8stm.2 phase 1). The coarse class stays
    /// the ergonomic DECLARATION vocabulary; `Effects` is what gets enforced.
    ///
    /// CAVEAT: the linear ladder is a TOTAL order; `Effects` is a product of
    /// INDEPENDENT axes. They are isomorphic only at the endpoints and for the
    /// classes tools actually declare (ReadOnly/Mutating/Execute). `Effects`
    /// has no irreversibility axis, so `Destructive` collapses to a write; no
    /// tool declares it today. When a tool genuinely needs `External`/
    /// `Destructive` semantics, express them as per-axis session constraints
    /// directly rather than through the coarse ceiling.
    pub fn effects(self) -> Effects {
        let mut e = Effects::default();
        match self {
            SideEffects::ReadOnly => e.filesystem = FsEffect::Read,
            SideEffects::Mutating | SideEffects::Destructive => e.filesystem = FsEffect::Write,
            SideEffects::External => e.network = true,
            SideEffects::Execute => {
                e.filesystem = FsEffect::Write;
                e.network = true;
                e.process = true;
            }
        }
        e
    }
}

impl PartialOrd for SideEffects {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SideEffects {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank().cmp(&other.rank())
    }
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
    fn tool_policy_default_fails_closed() {
        // mu-cvm5 (mu-n25a Phase 5): the default is now RESTRICTED, not
        // benign. A forgotten `.with_policy()` ships Mutating + Ask, so the
        // failure mode of omission is "too restrictive", never "silently
        // dangerous" (the mu-usfj class).
        let p = ToolPolicy::default();
        assert_eq!(p.side_effects, SideEffects::Mutating);
        assert_eq!(p.permission, PermissionLevel::Ask);
        assert!(matches!(p.retry, RetryPolicy::ModelDecides));
        assert_eq!(p.required_aws_capability, None);
        assert!(!p.idempotent);
    }

    #[test]
    fn tool_policy_read_only_is_the_benign_opt_in() {
        // The explicit benign posture lives in read_only(), not default().
        let p = ToolPolicy::read_only();
        assert_eq!(p.side_effects, SideEffects::ReadOnly);
        assert_eq!(p.permission, PermissionLevel::Allow);
        assert!(matches!(p.retry, RetryPolicy::ModelDecides));
        assert_eq!(p.required_aws_capability, None);
        assert!(p.idempotent);
        // ToolSpec::read_only() applies the same posture.
        let spec = ToolSpec::new("x", "d", Value::Object(Default::default())).read_only();
        assert_eq!(spec.policy, ToolPolicy::read_only());
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
