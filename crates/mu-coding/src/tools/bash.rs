//! `bash` tool — controlled shell execution.
//!
//! Two modes:
//! - **Strict** (default). Direct exec via tokio::process::Command,
//!   no shell. Allowlist-checked, shell-metachar-rejected, env-
//!   scrubbed. Conservative.
//! - **Yolo** (`--bash-yolo`). Full `bash -c`, full env, no
//!   allowlist. Explicit user opt-in.
//!
//! Both modes enforce timeout (60s default) and output cap (64KB)
//! to bound denial-of-service / context-flood risks.
//!
//! See spec mu-026.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::time::{Duration, Instant};

use mu_core::agent::{
    PermissionLevel, RetryPolicy, SideEffects, Tool, ToolPolicy, ToolResult, ToolSpec,
};
use serde_json::{json, Value};
use tokio::sync::oneshot;

/// Default-baked allowlist of read-only commands. Each entry is the
/// shell-parsed argv prefix (split by `shlex` semantics in the
/// match function). The model uses these constantly for orientation.
pub const DEFAULT_ALLOWLIST: &[&str] = &[
    "git status",
    "git log",
    "git diff",
    "git show",
    "git branch",
    "git remote",
    "git rev-parse",
    "ls",
    "pwd",
    "cat",
    "head",
    "tail",
    "wc",
    "file",
    "which",
    "date",
    "echo",
    "uname",
];

/// Max output bytes returned to the model. Both modes enforce.
pub const OUTPUT_CAP_BYTES: usize = 64 * 1024;

/// Default timeout when the call doesn't specify one. Both modes
/// enforce; max is enforced at 600s (10 min).
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
pub const MAX_TIMEOUT_SECS: u64 = 600;

/// Env vars to pass through to spawned processes in strict mode.
/// Everything else is dropped (notably API keys and custom-named
/// sensitive values). The secret-name check remains a defense in
/// depth if a future whitelist entry accidentally matches a secret
/// pattern.
const ENV_WHITELIST: &[&str] = &[
    "PATH", "HOME", "USER", "SHELL", "TERM", "LANG", "TZ", "TMPDIR", "PWD",
];

/// Regex-ish secret-pattern check on env var name. Matches names
/// ending in API_KEY, TOKEN, SECRET, or PASSWORD.
fn is_secret_env_var(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.ends_with("_API_KEY")
        || upper.ends_with("_TOKEN")
        || upper.ends_with("_SECRET")
        || upper.ends_with("_PASSWORD")
        || upper == "API_KEY"
        || upper == "TOKEN"
        || upper == "SECRET"
        || upper == "PASSWORD"
}

/// Shell-metacharacters that strict mode rejects in the command
/// string. Allowing any of these would let the agent bypass the
/// allowlist via shell features (chaining, substitution, redirect).
const SHELL_METACHARS: &[char] = &[';', '&', '|', '>', '<', '`', '$', '\\', '\n'];

/// Two-mode tool. See module docs.
#[derive(Debug, Clone)]
pub enum BashMode {
    Strict {
        allowlist: Vec<Vec<String>>,
        /// When true, the bash tool's policy is `PermissionLevel::Ask`
        /// — every **allowlist-passing** invocation triggers a
        /// `session.input_required` prompt before running. When false,
        /// runs immediately on allowlist match. Defaults to false to
        /// preserve current behavior; users opt in via `--bash-prompt`.
        ///
        /// Order of operations (mu-bkjr): the dispatcher calls
        /// `Tool::validate(&args)` BEFORE the approval gate.
        /// `BashTool::validate` performs the metachar + allowlist
        /// checks, so a non-allowlisted command (e.g. `curl`) fails
        /// immediately with the allowlist-rejection message — no
        /// approval modal is dispatched. The "allowlist gates first"
        /// promise from mu-030 is honored architecturally rather than
        /// by convention.
        prompt: bool,
    },
    Yolo,
}

impl BashMode {
    /// Construct a strict-mode allowlist from the default + extras.
    /// Each entry in `extras` is parsed via shlex; invalid entries
    /// are dropped with a warning log. `prompt = false` matches the
    /// classic strict semantics; `prompt = true` activates the
    /// mu-029 session.input_required gate on every allowlisted call.
    /// Non-allowlisted commands short-circuit via `Tool::validate`
    /// before the approval gate fires (mu-bkjr).
    pub fn strict_with_extras(extras: &[String], prompt: bool) -> Self {
        let mut allowlist: Vec<Vec<String>> = DEFAULT_ALLOWLIST
            .iter()
            .filter_map(|s| shlex::split(s))
            .collect();
        for extra in extras {
            match shlex::split(extra) {
                Some(tokens) if !tokens.is_empty() => allowlist.push(tokens),
                _ => {
                    tracing::warn!(
                        entry = %extra,
                        "bash: ignoring unparseable allowlist entry"
                    );
                }
            }
        }
        BashMode::Strict { allowlist, prompt }
    }
}

#[derive(Debug)]
pub struct BashTool {
    mode: BashMode,
    /// mu-8puo: point-of-action memory advisory (see
    /// [`crate::tools::action_recall`]). Arc because `execute`'s
    /// async block must own a handle independent of `&self`.
    action_recall: std::sync::Arc<crate::tools::action_recall::ActionRecall>,
}

impl BashTool {
    pub fn new(mode: BashMode) -> Self {
        Self {
            mode,
            action_recall: std::sync::Arc::new(crate::tools::action_recall::ActionRecall::default()),
        }
    }

    /// mu-8puo test hook: substitute the advisory engine (stub binary
    /// or disabled).
    pub fn with_action_recall(
        mut self,
        action_recall: crate::tools::action_recall::ActionRecall,
    ) -> Self {
        self.action_recall = std::sync::Arc::new(action_recall);
        self
    }
}

impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        let (mode_note, policy) = match &self.mode {
            BashMode::Strict { prompt: false, .. } => (
                "STRICT MODE: only allowlisted commands run. Shell metas (; & | > < ` $ \\) are \
                 rejected. The allowlist includes read-only commands like `git status`, `ls`, \
                 `cat`. IMPORTANT: if a command is rejected (allowlist miss, metachar reject), \
                 DO NOT retry with variants of the same command — the runtime tracks repeated \
                 failures and will refuse them. Use a different tool, or report the obstacle to \
                 the user.",
                ToolPolicy {
                    side_effects: SideEffects::Mutating,
                    permission: PermissionLevel::Allow,
                    // Strict mode rejects on allowlist + metachar grounds.
                    // Same-call retries can't possibly succeed (the allowlist
                    // doesn't change mid-session). RetryPolicy::Never makes
                    // the runtime enforce this even when the model gets confused.
                    retry: RetryPolicy::Never,
                    required_aws_capability: None,
                    idempotent: false, // file system can change between calls
                },
            ),
            BashMode::Strict { prompt: true, .. } => (
                "STRICT MODE WITH APPROVAL: only allowlisted commands run, AND each invocation \
                 requires explicit user approval via session.input_required before it dispatches. \
                 Same allowlist + metachar rejection rules as classic strict. If a command is \
                 rejected, DO NOT retry with variants. If approval is denied, accept that and \
                 either try a different approach or report to the user.",
                ToolPolicy {
                    side_effects: SideEffects::Mutating,
                    permission: PermissionLevel::Ask,
                    retry: RetryPolicy::Never,
                    required_aws_capability: None,
                    idempotent: false,
                },
            ),
            BashMode::Yolo => (
                "YOLO MODE: any command runs via bash -c. Pipes, redirects, env vars all work. \
                 Treat this as if you have a shell prompt; behave responsibly. \
                 Bash path quoting: unquoted ~/path expands, but quoted \"~/path\" and '~/path' do not. \
                 For paths that need expansion and may need quoting, use \"$HOME/path\" or an absolute path.",
                ToolPolicy {
                    side_effects: SideEffects::Destructive,
                    permission: PermissionLevel::Allow,
                    retry: RetryPolicy::ModelDecides,
                    required_aws_capability: None,
                    idempotent: false,
                },
            ),
        };
        ToolSpec {
            name: "bash".to_owned(),
            description: format!(
                "Run a shell command. {mode_note} \
                 Output capped at 64KB (stdout+stderr). Default timeout 60s; override via timeout_secs (max 600). \
                 Exit code is reflected in is_error. \
                 NOTE: some destructive commands (rm, git push --force, jj abandon, …) may return a \
                 one-time memory advisory INSTEAD of executing — a standing operator rule surfaced at \
                 the point of action. Read it; if the command is still appropriate, re-issue it \
                 verbatim and it will run."
            ),
            display: None,
            when: None,
            policy,
            // mu-2e0h: bash output is logs/test/grep territory — the
            // tier-1 ingestion filter applies (ANSI strip, repeat
            // collapse, line cap, truncation past its own 64KB cap).
            verbatim_result: false,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The command to execute."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_TIMEOUT_SECS,
                        "description": "Optional per-call timeout in seconds. Default 60, max 600."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    fn validate(&self, arguments: &Value) -> Result<(), String> {
        // mu-bkjr: argument-aware pre-flight. The dispatcher calls this
        // BEFORE the PermissionLevel::Ask gate, so non-allowlisted
        // commands fail immediately without prompting the user for
        // approval on a doomed call.
        let command = parse_command_arg(arguments)?;

        // Anti-pattern guard (Yolo mode only — strict mode already rejects via allowlist):
        // Block bare `jj describe` which opens $EDITOR and hijacks stdin,
        // wrecking the terminal session. The `-m` flag is mandatory.
        if let Err(suggestion) = check_bare_jj_describe(&command) {
            return Err(format!(
                "bash: anti-pattern detected — bare `jj describe` without -m flag. \
                 This opens $EDITOR and hijacks stdin, corrupting the terminal session. \
                 {}\n\n\
                 Use `jj describe -m \"<message>\"` instead.\n\
                 Alternatively, if you meant to edit an existing commit's message:\n\
                 `jj git commit --reset-author` or `jj squash`\n\
                 If you need this command despite the guard, use --bash-yolo again.",
                suggestion
            ));
        }

        // The security gate proper (metachar + allowlist for strict, pass
        // for yolo) lives in the shared `validate_command` so other
        // command-runners enforce the IDENTICAL policy (mu-qnag).
        validate_command(&self.mode, &command)
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
        // Defense-in-depth: re-run validate. The dispatcher (mu-bkjr)
        // calls validate before the policy gate, but direct-call paths
        // (unit tests, future API surfaces) bypass the dispatcher.
        if let Err(reason) = self.validate(&arguments) {
            return Box::pin(async move {
                ToolResult {
                    content: reason,
                    is_error: true,
                }
            });
        }
        let mode = self.mode.clone();
        let action_recall = std::sync::Arc::clone(&self.action_recall);
        Box::pin(async move {
            // validate() succeeded — parse_command_arg cannot fail now.
            let command = parse_command_arg(&arguments)
                .expect("parse_command_arg succeeded in validate() just above");

            // mu-8puo: point-of-action memory advisory. Fires at most
            // once per command; the advisory result is is_error:false
            // ON PURPOSE — strict mode's RetryPolicy::Never refuses
            // identical retries of errored calls, and the whole point
            // is that an identical re-issue proceeds.
            if let Some(advice) = action_recall.advisory_for(&command).await {
                return ToolResult {
                    content: advice,
                    is_error: false,
                };
            }

            let timeout_secs = arguments
                .get("timeout_secs")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .clamp(1, MAX_TIMEOUT_SECS);
            let timeout = Duration::from_secs(timeout_secs);

            // Build the child through the shared gate (mu-qnag) — the same
            // path the `watch` tool uses. validate() already accepted this
            // command above; build_command re-applies the strict gate to
            // obtain the tokenized argv and can never construct an ungated
            // command.
            let mut cmd = match build_command(&mode, &command) {
                Ok(c) => c,
                Err(reason) => {
                    return ToolResult {
                        content: reason,
                        is_error: true,
                    }
                }
            };
            cmd.stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            // `kill_on_drop` ensures the child gets reaped if we drop
            // the handle (timeout or cancel).
            cmd.kill_on_drop(true);

            let started_at = Instant::now();
            let child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    return ToolResult {
                        content: format!("bash: failed to spawn: {e}"),
                        is_error: true,
                    };
                }
            };

            // Race the wait against timeout and external cancel.
            let wait = child.wait_with_output();
            let outcome = tokio::select! {
                out = wait => Some(out),
                _ = tokio::time::sleep(timeout) => None,
                _ = cancel_rx => {
                    return ToolResult {
                        content: "bash: cancelled".to_owned(),
                        is_error: true,
                    };
                }
            };

            let elapsed_ms = started_at.elapsed().as_millis() as u64;

            let output = match outcome {
                Some(Ok(o)) => o,
                Some(Err(e)) => {
                    return ToolResult {
                        content: format!("bash: wait failed after {elapsed_ms}ms: {e}"),
                        is_error: true,
                    };
                }
                None => {
                    // Timeout: child is dropped (kill_on_drop fires).
                    // We can't recover its partial output cleanly
                    // because we moved it into wait_with_output. The
                    // process is dead; report timeout.
                    return ToolResult {
                        content: format!(
                            "bash: timed out after {timeout_secs}s (command: {command:?})"
                        ),
                        is_error: true,
                    };
                }
            };

            // Cap and format output.
            let stdout = truncate_to_cap(&output.stdout, OUTPUT_CAP_BYTES / 2);
            let stderr = truncate_to_cap(&output.stderr, OUTPUT_CAP_BYTES / 2);
            let exit_code = output.status.code();
            let is_error = !output.status.success();

            let mut content = stdout;
            if !stderr.is_empty() {
                if !content.is_empty() {
                    content.push('\n');
                }
                content.push_str("stderr:\n");
                content.push_str(&stderr);
            }
            if let Some(code) = exit_code {
                if code != 0 {
                    content.push_str(&format!("\nexit: {code}"));
                }
            }
            content.push_str(&format!("\nelapsed: {elapsed_ms}ms"));

            ToolResult { content, is_error }
        })
    }
}

/// Token-prefix match: tokens must start with one of allowlist's
/// entries. Equal-length match counts as prefix.
/// Extract the `command` field from the JSON arguments. Returns the
/// command string or an `Err` with a user-facing reason. Used by both
/// `Tool::validate` (pre-flight) and `Tool::execute` (run-time);
/// keeping the parse logic here means the two paths agree on what
/// "valid arguments" means. (mu-bkjr)
fn parse_command_arg(arguments: &Value) -> Result<String, String> {
    match arguments.get("command").and_then(Value::as_str) {
        Some(c) if !c.trim().is_empty() => Ok(c.to_owned()),
        Some(_) => Err("bash: empty `command` is not allowed".to_owned()),
        None => Err("bash: missing required `command` argument".to_owned()),
    }
}

/// Strict-mode pre-flight check: reject shell metacharacters,
/// tokenize via shlex, allowlist-check. Returns the tokenized argv
/// on success (`execute` consumes the tokens to build the
/// `tokio::process::Command`). Called by both `Tool::validate` and
/// `Tool::execute` for the strict mode path. (mu-bkjr)
fn validate_strict_command(
    command: &str,
    allowlist: &[Vec<String>],
) -> Result<Vec<String>, String> {
    if let Some(c) = command.chars().find(|c| SHELL_METACHARS.contains(c)) {
        return Err(format!(
            "bash: shell metacharacter '{c}' rejected in strict mode. \
             Use --bash-yolo for full shell, or break the command into \
             separate tool calls."
        ));
    }
    let tokens = match shlex::split(command) {
        Some(t) if !t.is_empty() => t,
        _ => return Err(format!("bash: could not tokenize command: {command:?}")),
    };
    if !is_allowed(&tokens, allowlist) {
        return Err(format!(
            "bash: command {tokens:?} is not in the strict-mode allowlist. \
             Currently allowed prefixes: {}. \
             Extend with --bash-allow on `mu serve`, or use --bash-yolo.",
            fmt_allowlist(allowlist),
        ));
    }
    Ok(tokens)
}

/// Gate a raw command string against a [`BashMode`]'s policy: the
/// metachar + allowlist check for strict mode, an unconditional pass for
/// yolo. Public so other command-runners route through the IDENTICAL
/// policy instead of shipping their own ungated execution path — the
/// `watch` tool validates every command here before spawning (mu-qnag).
/// The bare-`jj describe` UX guard stays in [`BashTool::validate`]; this
/// is the security gate proper.
pub fn validate_command(mode: &BashMode, command: &str) -> Result<(), String> {
    match mode {
        BashMode::Strict { allowlist, .. } => {
            validate_strict_command(command, allowlist).map(|_| ())
        }
        BashMode::Yolo => Ok(()),
    }
}

/// Construct the child process for `command` under `mode`, applying the
/// same gate as [`validate_command`] FIRST so an ungated command can
/// never be built. Strict mode execs the tokenized argv directly with a
/// scrubbed env (no shell); yolo runs it via `bash -c` with the full
/// inherited env. The caller owns lifecycle (stdio, `kill_on_drop`,
/// timeout). Shared by the `bash` tool and the `watch` tool so both run
/// commands through one path (mu-qnag).
pub fn build_command(mode: &BashMode, command: &str) -> Result<tokio::process::Command, String> {
    match mode {
        BashMode::Strict { allowlist, .. } => {
            let tokens = validate_strict_command(command, allowlist)?;
            let mut c = tokio::process::Command::new(&tokens[0]);
            if tokens.len() > 1 {
                c.args(&tokens[1..]);
            }
            c.env_clear();
            // Strict mode inherits only explicit non-secret whitelist entries.
            for (k, v) in std::env::vars() {
                if ENV_WHITELIST.contains(&k.as_str()) && !is_secret_env_var(&k) {
                    c.env(&k, &v);
                }
            }
            Ok(c)
        }
        BashMode::Yolo => {
            let mut c = tokio::process::Command::new("bash");
            c.arg("-c").arg(command);
            // Yolo passes the full env, inherited from the parent.
            Ok(c)
        }
    }
}

fn is_allowed(tokens: &[String], allowlist: &[Vec<String>]) -> bool {
    allowlist
        .iter()
        .any(|entry| tokens.len() >= entry.len() && tokens[..entry.len()] == entry[..])
}

fn fmt_allowlist(allowlist: &[Vec<String>]) -> String {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut entries: Vec<String> = Vec::new();
    for entry in allowlist {
        let joined = entry.join(" ");
        if seen.insert(Box::leak(joined.clone().into_boxed_str())) {
            entries.push(joined);
        }
    }
    if entries.len() > 8 {
        let head = entries[..8].join(", ");
        format!("{head}, …+{}", entries.len() - 8)
    } else {
        entries.join(", ")
    }
}

/// Anti-pattern guard: detect bare `jj describe` (without -m flag).
/// In Yolo mode this is the sole gate before execution. Bare `jj describe`
/// opens $EDITOR and hijacks stdin/TTY, which corrupts the agent session.
///
/// Returns an Err with a helpful suggestion if an anti-pattern is detected;
/// Ok(()) otherwise. The caller (validate) converts the Err into a rejection
/// with context about why it was blocked.
fn check_bare_jj_describe(command: &str) -> Result<(), String> {
    let tokens = match shlex::split(command) {
        Some(t) if !t.is_empty() => t,
        _ => return Ok(()), // couldn't tokenize, skip guard (won't be jj describe anyway)
    };

    // Match: [jj, describe] with nothing else — bare form.
    // Anything with -m or other flags/positional args is fine.
    if tokens.len() == 2
        && tokens[0].to_lowercase() == "jj"
        && tokens[1].to_lowercase() == "describe"
    {
        return Err(
            "The `jj describe` command opens an interactive editor when called \
             without arguments, which hijacks stdin and breaks the agent's terminal. \
             Always use -m to provide the message inline."
                .to_string(),
        );
    }

    Ok(())
}

/// Truncate UTF-8 bytes to ~cap chars (best effort — char boundary aware).
fn truncate_to_cap(bytes: &[u8], cap: usize) -> String {
    if bytes.len() <= cap {
        return String::from_utf8_lossy(bytes).to_string();
    }
    // Find a char boundary near `cap`.
    let mut end = cap;
    while end > 0 && (bytes[end - 1] & 0xC0) == 0x80 {
        end -= 1;
    }
    let dropped = bytes.len() - end;
    let s = String::from_utf8_lossy(&bytes[..end]).to_string();
    format!("{s}…[truncated {dropped} bytes]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    async fn execute_bash(mode: BashMode, args: Value) -> ToolResult {
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        // mu-8puo: advisory disabled so tests never consult the REAL
        // operator memory store (hermetic + deterministic). The
        // advisory path has its own stub-driven tests below.
        BashTool::new(mode)
            .with_action_recall(crate::tools::action_recall::ActionRecall::disabled())
            .execute(args, cancel_rx)
            .await
    }

    #[test]
    fn spec_describes_bash_tool() {
        let strict = BashTool::new(BashMode::strict_with_extras(&[], false));
        let s = strict.spec();
        assert_eq!(s.name, "bash");
        assert!(s.description.contains("STRICT MODE"));
        assert_eq!(s.input_schema["required"], json!(["command"]));

        let yolo = BashTool::new(BashMode::Yolo);
        let y = yolo.spec();
        assert!(y.description.contains("YOLO MODE"));
    }

    /// mu-bkjr: `validate()` is the dispatcher-facing pre-flight check.
    /// These tests pin its behavior directly (without going through
    /// `execute()`), since the dispatcher consults it BEFORE the
    /// PermissionLevel::Ask approval gate.
    #[test]
    fn validate_strict_accepts_allowlisted_command() {
        let tool = BashTool::new(BashMode::strict_with_extras(&[], true));
        assert!(tool.validate(&json!({ "command": "echo hi" })).is_ok());
        assert!(tool.validate(&json!({ "command": "git status" })).is_ok());
    }

    #[test]
    fn validate_strict_rejects_non_allowlisted() {
        let tool = BashTool::new(BashMode::strict_with_extras(&[], true));
        let err = tool
            .validate(&json!({ "command": "curl -I example.com" }))
            .expect_err("curl must be rejected by allowlist");
        assert!(err.contains("not in the strict-mode allowlist"));
        assert!(err.contains("--bash-allow") || err.contains("--bash-yolo"));
    }

    #[test]
    fn validate_strict_rejects_metacharacters() {
        let tool = BashTool::new(BashMode::strict_with_extras(&[], true));
        for cmd in ["ls; rm -rf /", "echo $(whoami)", "ls > /tmp/x"] {
            let err = tool
                .validate(&json!({ "command": cmd }))
                .expect_err("metachar must be rejected");
            assert!(err.contains("metacharacter"), "for {cmd}, got: {err}");
        }
    }

    #[test]
    fn validate_rejects_missing_or_empty_command() {
        let tool = BashTool::new(BashMode::strict_with_extras(&[], false));
        assert!(tool
            .validate(&json!({}))
            .expect_err("missing must reject")
            .contains("missing required"));
        assert!(tool
            .validate(&json!({ "command": "" }))
            .expect_err("empty must reject")
            .contains("empty"));
        assert!(tool
            .validate(&json!({ "command": "   " }))
            .expect_err("whitespace-only must reject")
            .contains("empty"));
    }

    #[test]
    fn validate_yolo_always_ok() {
        let tool = BashTool::new(BashMode::Yolo);
        // Yolo accepts anything that has a non-empty `command` — no
        // allowlist, no metachar rejection at the pre-flight level.
        assert!(tool
            .validate(&json!({ "command": "rm -rf / ; curl whatever" }))
            .is_ok());
        // But still rejects missing/empty.
        assert!(tool.validate(&json!({})).is_err());
    }

    #[test]
    fn validate_yolo_rejects_bare_jj_describe() {
        let tool = BashTool::new(BashMode::Yolo);

        // Bare `jj describe` is blocked.
        assert!(tool.validate(&json!({ "command": "jj describe" })).is_err());

        // With -m flag, it's allowed.
        assert!(tool
            .validate(&json!({ "command": "jj describe -m \"message\"" }))
            .is_ok());

        // With positional change ID, it's allowed (not bare).
        assert!(tool
            .validate(&json!({ "command": "jj describe abc123" }))
            .is_ok());

        // Case-insensitive match on command name.
        assert!(tool.validate(&json!({ "command": "JJ DESCRIBE" })).is_err());
    }

    #[test]
    fn allowlist_token_prefix_match() {
        let allowlist = vec![vec!["git".into(), "status".into()], vec!["echo".into()]];
        assert!(is_allowed(&["git".into(), "status".into()], &allowlist));
        assert!(is_allowed(
            &["git".into(), "status".into(), "-s".into()],
            &allowlist
        ));
        assert!(is_allowed(
            &["echo".into(), "hello".into(), "world".into()],
            &allowlist
        ));
        assert!(!is_allowed(&["git".into(), "push".into()], &allowlist));
        assert!(!is_allowed(&["echoxyz".into()], &allowlist));
        // Shorter than allowlist entry doesn't match.
        assert!(!is_allowed(&["git".into()], &allowlist));
    }

    #[test]
    fn secret_env_var_detection() {
        assert!(is_secret_env_var("ANTHROPIC_API_KEY"));
        assert!(is_secret_env_var("OPENAI_API_KEY"));
        assert!(is_secret_env_var("GITHUB_TOKEN"));
        assert!(is_secret_env_var("MY_SECRET"));
        assert!(is_secret_env_var("DB_PASSWORD"));
        assert!(is_secret_env_var("api_key"));
        assert!(!is_secret_env_var("PATH"));
        assert!(!is_secret_env_var("CARGO_HOME"));
        assert!(!is_secret_env_var("RUSTC"));
    }

    #[tokio::test]
    async fn b1_strict_allowlisted_runs() -> Result<(), Box<dyn Error>> {
        let mode = BashMode::strict_with_extras(&[], false);
        let result = execute_bash(mode, json!({ "command": "echo hello" })).await;
        assert!(!result.is_error, "got: {}", result.content);
        assert!(result.content.contains("hello"));
        assert!(result.content.contains("elapsed:"));
        Ok(())
    }

    #[tokio::test]
    async fn b2_strict_not_allowed_refused() {
        let mode = BashMode::strict_with_extras(&[], false);
        let result = execute_bash(mode, json!({ "command": "rm /tmp/foo" })).await;
        assert!(result.is_error);
        assert!(result.content.contains("not in the strict-mode allowlist"));
        assert!(result.content.contains("--bash-allow") || result.content.contains("--bash-yolo"));
    }

    #[tokio::test]
    async fn b3_extended_allowlist() {
        let mode = BashMode::strict_with_extras(&["true".to_string()], false);
        let result = execute_bash(mode, json!({ "command": "true" })).await;
        assert!(!result.is_error, "got: {}", result.content);
    }

    #[tokio::test]
    async fn b4_strict_metacharacters_rejected() {
        let mode = BashMode::strict_with_extras(&[], false);
        for cmd in [
            "ls; rm -rf /",
            "echo hi | wc -l",
            "echo $(whoami)",
            "echo `whoami`",
            "echo > /tmp/x",
            "echo & background",
        ] {
            let result = execute_bash(mode.clone(), json!({ "command": cmd })).await;
            assert!(result.is_error, "expected error for: {cmd}");
            assert!(
                result.content.contains("metacharacter"),
                "expected metachar error for {cmd}, got: {}",
                result.content
            );
        }
    }

    #[tokio::test]
    async fn b5_strict_env_scrub() -> Result<(), Box<dyn Error>> {
        // Inject a secret env var; confirm it doesn't leak.
        std::env::set_var("MU_TEST_BASH_API_KEY", "should-not-leak");
        // Use printenv with a single var name — read-only & cheap.
        // It's not in the default allowlist, so extend.
        let mode = BashMode::strict_with_extras(&["printenv".to_string()], false);
        let result =
            execute_bash(mode, json!({ "command": "printenv MU_TEST_BASH_API_KEY" })).await;
        std::env::remove_var("MU_TEST_BASH_API_KEY");
        // printenv with a missing var exits 1 with no output. So
        // is_error may be true, but the content should NOT contain
        // the secret value.
        assert!(
            !result.content.contains("should-not-leak"),
            "env scrub failed; result: {}",
            result.content
        );
        Ok(())
    }

    #[tokio::test]
    async fn b5_strict_env_requires_whitelist_even_when_not_secret() -> Result<(), Box<dyn Error>> {
        std::env::set_var("MU_TEST_BASH_PUBLIC_VAR", "should-not-pass");
        let mode = BashMode::strict_with_extras(&["printenv".to_string()], false);
        let result = execute_bash(
            mode,
            json!({ "command": "printenv MU_TEST_BASH_PUBLIC_VAR" }),
        )
        .await;
        std::env::remove_var("MU_TEST_BASH_PUBLIC_VAR");
        assert!(
            !result.content.contains("should-not-pass"),
            "non-whitelisted env var leaked; result: {}",
            result.content
        );
        Ok(())
    }

    #[tokio::test]
    async fn b6_strict_timeout() {
        let mode = BashMode::strict_with_extras(&["sleep".to_string()], false);
        let result = execute_bash(mode, json!({ "command": "sleep 5", "timeout_secs": 1 })).await;
        assert!(result.is_error);
        assert!(result.content.contains("timed out") || result.content.contains("timeout"));
    }

    #[tokio::test]
    async fn b8_strict_nonzero_exit_is_error() -> Result<(), Box<dyn Error>> {
        let mode = BashMode::strict_with_extras(&["false".to_string()], false);
        let result = execute_bash(mode, json!({ "command": "false" })).await;
        assert!(result.is_error);
        assert!(result.content.contains("exit:"));
        Ok(())
    }

    #[tokio::test]
    async fn b9_yolo_pipes_work() -> Result<(), Box<dyn Error>> {
        let result =
            execute_bash(BashMode::Yolo, json!({ "command": "echo hi | tr a-z A-Z" })).await;
        assert!(!result.is_error, "got: {}", result.content);
        assert!(result.content.contains("HI"));
        Ok(())
    }

    #[tokio::test]
    async fn b10_yolo_env_passes_through() -> Result<(), Box<dyn Error>> {
        std::env::set_var("MU_TEST_BASH_YOLO_VAR", "yolo-value");
        let result = execute_bash(
            BashMode::Yolo,
            json!({ "command": "echo $MU_TEST_BASH_YOLO_VAR" }),
        )
        .await;
        std::env::remove_var("MU_TEST_BASH_YOLO_VAR");
        assert!(!result.is_error, "got: {}", result.content);
        assert!(result.content.contains("yolo-value"));
        Ok(())
    }

    #[tokio::test]
    async fn b11_strict_default_allowlist_runs_git_status() {
        // git status is in the default allowlist.
        let mode = BashMode::strict_with_extras(&[], false);
        let result = execute_bash(mode, json!({ "command": "git status --short" })).await;
        // Doesn't matter what `git status` returns — what matters
        // is that the allowlist check passed (i.e. we don't see
        // the "not in the strict-mode allowlist" message).
        assert!(
            !result.content.contains("not in the strict-mode allowlist"),
            "default allowlist should accept `git status --short`; got: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn missing_command_errors() {
        let mode = BashMode::strict_with_extras(&[], false);
        let result = execute_bash(mode, json!({})).await;
        assert!(result.is_error);
        assert!(result.content.contains("missing required `command`"));
    }

    #[tokio::test]
    async fn empty_command_errors() {
        let mode = BashMode::strict_with_extras(&[], false);
        let result = execute_bash(mode, json!({ "command": "   " })).await;
        assert!(result.is_error);
        assert!(result.content.contains("empty"));
    }

    #[test]
    fn truncate_to_cap_respects_utf8_boundary() {
        // Long ASCII works fine.
        let s = "a".repeat(100);
        let truncated = truncate_to_cap(s.as_bytes(), 50);
        assert!(truncated.contains("truncated"));
        // Non-ASCII: each `é` is 2 bytes.
        let s = "é".repeat(100); // 200 bytes
        let truncated = truncate_to_cap(s.as_bytes(), 51);
        assert!(truncated.contains("truncated"));
        // Should not panic, should produce valid UTF-8.
        assert!(truncated.is_char_boundary(truncated.find('…').unwrap_or(0)));
    }

    // ── mu-8puo: point-of-action memory advisory ──────────────────

    /// End-to-end through BashTool: the first dangerous command
    /// returns the advisory WITHOUT executing; the identical
    /// re-issue executes. Yolo mode + a marker file prove execution
    /// state at each step.
    #[tokio::test]
    async fn advisory_intercepts_then_reissue_executes() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("mu-8puo-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Stub memory CLI: always returns one labeled rule.
        let stub = dir.join("agent-stub.sh");
        {
            let mut f = std::fs::File::create(&stub).unwrap();
            writeln!(
                f,
                "#!/bin/sh\necho '[838c3bf4] (feedback) never-batch-destructive — rule  [2026-06-04]'"
            )
            .unwrap();
            let mut perms = f.metadata().unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&stub, perms).unwrap();
        }
        let marker = dir.join("marker");
        std::fs::write(&marker, "x").unwrap();

        let tool = BashTool::new(BashMode::Yolo).with_action_recall(
            crate::tools::action_recall::ActionRecall::with_binary(&stub),
        );
        let cmd = format!("rm {}", marker.display());

        // First call: advisory, NOT executed, NOT an error (strict
        // mode's RetryPolicy::Never must not eat the re-issue).
        let (_t1, rx1) = oneshot::channel();
        let first = tool.execute(json!({"command": cmd}), rx1).await;
        assert!(
            !first.is_error,
            "advisory is not an error: {}",
            first.content
        );
        assert!(first.content.contains("NOT executed"));
        assert!(first.content.contains("never-batch-destructive"));
        assert!(marker.exists(), "command must not have run");

        // Identical re-issue: executes.
        let (_t2, rx2) = oneshot::channel();
        let second = tool.execute(json!({"command": cmd}), rx2).await;
        assert!(!second.is_error, "{}", second.content);
        assert!(!marker.exists(), "re-issue must actually run the command");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
