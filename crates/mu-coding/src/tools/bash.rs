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
        /// — every (allowlist-passing) invocation triggers a
        /// `session.input_required` prompt before running. When
        /// false, runs immediately on allowlist match. Defaults to
        /// false to preserve current behavior; users opt in via
        /// `--bash-prompt`.
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
}

impl BashTool {
    pub fn new(mode: BashMode) -> Self {
        Self { mode }
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
                 Treat this as if you have a shell prompt; behave responsibly.",
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
                 Exit code is reflected in is_error."
            ),
            policy,
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

    fn execute<'life0, 'async_trait>(
        &'life0 self,
        arguments: Value,
        cancel_rx: oneshot::Receiver<()>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        let mode = self.mode.clone();
        Box::pin(async move {
            let command = match arguments.get("command").and_then(Value::as_str) {
                Some(c) if !c.trim().is_empty() => c.to_owned(),
                Some(_) => {
                    return ToolResult {
                        content: "bash: empty `command` is not allowed".to_owned(),
                        is_error: true,
                    };
                }
                None => {
                    return ToolResult {
                        content: "bash: missing required `command` argument".to_owned(),
                        is_error: true,
                    };
                }
            };

            let timeout_secs = arguments
                .get("timeout_secs")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .clamp(1, MAX_TIMEOUT_SECS);
            let timeout = Duration::from_secs(timeout_secs);

            let mut cmd = match &mode {
                BashMode::Strict { allowlist, .. } => {
                    // Reject shell metas first — cheaper than parsing.
                    if let Some(c) = command.chars().find(|c| SHELL_METACHARS.contains(c)) {
                        return ToolResult {
                            content: format!(
                                "bash: shell metacharacter '{c}' rejected in strict mode. \
                                 Use --bash-yolo for full shell, or break the command into \
                                 separate tool calls."
                            ),
                            is_error: true,
                        };
                    }
                    let tokens = match shlex::split(&command) {
                        Some(t) if !t.is_empty() => t,
                        _ => {
                            return ToolResult {
                                content: format!("bash: could not tokenize command: {command:?}"),
                                is_error: true,
                            };
                        }
                    };
                    if !is_allowed(&tokens, allowlist) {
                        return ToolResult {
                            content: format!(
                                "bash: command {tokens:?} is not in the strict-mode allowlist. \
                                 Currently allowed prefixes: {}. \
                                 Extend with --bash-allow on `mu serve`, or use --bash-yolo.",
                                fmt_allowlist(allowlist),
                            ),
                            is_error: true,
                        };
                    }
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
                    c
                }
                BashMode::Yolo => {
                    let mut c = tokio::process::Command::new("bash");
                    c.arg("-c").arg(&command);
                    // Yolo passes the full env. Inherited from parent.
                    c
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
        BashTool::new(mode).execute(args, cancel_rx).await
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
}
