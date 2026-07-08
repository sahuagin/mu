//! Lifecycle hooks — operator-declared subprocess hooks on agent-loop
//! events. Bead mu-bb2v.
//!
//! Hooks let an operator attach small scripts to the agent lifecycle
//! without forking the runtime: guard a tool before it runs, observe
//! results, run something at session start or turn end. The design is
//! deliberately compatible in spirit with Claude Code's hooks so
//! existing hook scripts port with minimal change:
//!
//! - Each hook is one config entry: an event, an optional tool-name
//!   matcher, a shell command, a timeout.
//! - The hook receives a single JSON object on **stdin** describing the
//!   event (`hook_event_name`, plus event-specific fields).
//! - **`PreToolUse` is the only gate.** Exit code 2 denies the tool
//!   call (stderr becomes the deny reason), as does a stdout JSON
//!   object `{"decision":"deny","reason":"…"}`. Every other outcome —
//!   exit 0, other exit codes, spawn failure, timeout — allows.
//! - `PostToolUse`, `Stop`, and `SessionStart` are observational:
//!   fire-and-forget, output logged at debug level, never blocking.
//!
//! Fail-open is the invariant: a broken hook script must degrade to
//! "no hook", never to a wedged or dead session. The one deliberate
//! exception is an explicit deny, which is the whole point of a gate.
//!
//! Subprocesses run with a scrubbed environment: `PATH`, `HOME`,
//! `USER`, `LANG`, `TMPDIR` pass through; everything else is dropped
//! (no provider keys leak into hook scripts). `MU_HOOK_EVENT` carries
//! the event name.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::HooksConfig;

/// Which lifecycle moment a hook is attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEventKind {
    SessionStart,
    PreToolUse,
    PostToolUse,
    Stop,
}

impl HookEventKind {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "SessionStart" => Some(Self::SessionStart),
            "PreToolUse" => Some(Self::PreToolUse),
            "PostToolUse" => Some(Self::PostToolUse),
            "Stop" => Some(Self::Stop),
            _ => None,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::Stop => "Stop",
        }
    }
}

/// Default per-hook wall-clock budget. Generous enough for a script
/// that shells out once; short enough that a hung hook can't wedge a
/// turn (the PreToolUse gate awaits inline).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on captured stdout/stderr per hook run — hooks are control
/// surfaces, not data pipes.
const OUTPUT_CAP: usize = 16 * 1024;

/// Tool-result content is truncated to this many bytes in the
/// PostToolUse payload; observers that need full output should read
/// the event log instead.
const POST_CONTENT_CAP: usize = 8 * 1024;

#[derive(Debug, Clone)]
struct HookEntry {
    event: HookEventKind,
    /// `None` matches every tool; `"name"` matches exactly; a trailing
    /// `*` matches by prefix (`"mcp_*"`). Ignored for non-tool events.
    matcher: Option<String>,
    command: String,
    timeout: Duration,
}

impl HookEntry {
    fn matches_tool(&self, tool_name: &str) -> bool {
        match &self.matcher {
            None => true,
            Some(m) => match m.strip_suffix('*') {
                Some(prefix) => tool_name.starts_with(prefix),
                None => tool_name == m,
            },
        }
    }
}

/// A hook's verdict on a PreToolUse gate check.
#[derive(Debug)]
struct RunOutput {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

/// The engine: parsed entries + dispatch. Construct once per daemon
/// (or session) from config; share via `Arc`.
pub struct HookEngine {
    entries: Vec<HookEntry>,
}

impl HookEngine {
    /// Build from the `[hooks]` config section. Returns `None` when
    /// hooks are disabled or no valid entries exist, so callers can
    /// carry `Option<Arc<HookEngine>>` and pay nothing on the common
    /// path. Entries with unknown event names are skipped loudly.
    pub fn from_config(cfg: &HooksConfig) -> Option<Arc<Self>> {
        if !cfg.enabled {
            return None;
        }
        let mut entries = Vec::new();
        for e in &cfg.entries {
            let Some(event) = HookEventKind::parse(&e.event) else {
                tracing::warn!(
                    event = %e.event,
                    command = %e.command,
                    "hooks: unknown event name; entry skipped \
                     (expected SessionStart|PreToolUse|PostToolUse|Stop)"
                );
                continue;
            };
            if e.command.trim().is_empty() {
                tracing::warn!(event = %e.event, "hooks: empty command; entry skipped");
                continue;
            }
            entries.push(HookEntry {
                event,
                matcher: e.matcher.clone(),
                command: expand_leading_tilde(&e.command),
                timeout: e
                    .timeout_ms
                    .map(Duration::from_millis)
                    .unwrap_or(DEFAULT_TIMEOUT),
            });
        }
        if entries.is_empty() {
            return None;
        }
        Some(Arc::new(Self { entries }))
    }

    /// True when at least one entry listens on `kind` — lets hot paths
    /// skip payload construction entirely.
    pub fn listens(&self, kind: HookEventKind) -> bool {
        self.entries.iter().any(|e| e.event == kind)
    }

    /// The PreToolUse gate. Runs every matching hook **sequentially**
    /// (a gate that raced itself would be order-dependent anyway) and
    /// returns the first deny reason, or `None` to allow. Everything
    /// but an explicit deny allows — see module docs.
    pub async fn gate_pre_tool_use(&self, tool_name: &str, tool_input: &Value) -> Option<String> {
        let payload = json!({
            "hook_event_name": "PreToolUse",
            "tool_name": tool_name,
            "tool_input": tool_input,
        });
        for entry in self
            .entries
            .iter()
            .filter(|e| e.event == HookEventKind::PreToolUse && e.matches_tool(tool_name))
        {
            let out = run_hook(entry, &payload).await;
            if let Some(reason) = deny_reason(entry, &out) {
                tracing::info!(
                    command = %entry.command,
                    tool = %tool_name,
                    %reason,
                    "hooks: PreToolUse denied tool call"
                );
                return Some(reason);
            }
        }
        None
    }

    /// Observational dispatch: fire-and-forget on a spawned task. The
    /// caller's path never blocks on hook execution.
    fn observe(self: &Arc<Self>, kind: HookEventKind, tool_name: Option<String>, payload: Value) {
        if !self.listens(kind) {
            return;
        }
        let engine = Arc::clone(self);
        tokio::spawn(async move {
            for entry in engine.entries.iter().filter(|e| {
                e.event == kind
                    && tool_name
                        .as_deref()
                        .map(|t| e.matches_tool(t))
                        .unwrap_or(true)
            }) {
                let out = run_hook(entry, &payload).await;
                tracing::debug!(
                    event = kind.name(),
                    command = %entry.command,
                    exit = ?out.exit_code,
                    timed_out = out.timed_out,
                    "hooks: observational hook ran"
                );
            }
        });
    }

    pub fn observe_session_start(self: &Arc<Self>, session_id: &str, cwd: &str) {
        self.observe(
            HookEventKind::SessionStart,
            None,
            json!({
                "hook_event_name": "SessionStart",
                "session_id": session_id,
                "cwd": cwd,
            }),
        );
    }

    pub fn observe_post_tool_use(
        self: &Arc<Self>,
        tool_name: &str,
        tool_input: Option<&Value>,
        content: &str,
        is_error: bool,
    ) {
        let truncated: String = content.chars().take(POST_CONTENT_CAP).collect();
        self.observe(
            HookEventKind::PostToolUse,
            Some(tool_name.to_owned()),
            json!({
                "hook_event_name": "PostToolUse",
                "tool_name": tool_name,
                "tool_input": tool_input.cloned().unwrap_or(Value::Null),
                "tool_response": {
                    "content": truncated,
                    "content_truncated": content.len() > POST_CONTENT_CAP,
                    "is_error": is_error,
                },
            }),
        );
    }

    pub fn observe_stop(self: &Arc<Self>, stop_reason: &str, turn_count: u32) {
        self.observe(
            HookEventKind::Stop,
            None,
            json!({
                "hook_event_name": "Stop",
                "stop_reason": stop_reason,
                "turn_count": turn_count,
            }),
        );
    }
}

impl std::fmt::Debug for HookEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookEngine")
            .field("entries", &self.entries.len())
            .finish()
    }
}

/// Extract a deny from one PreToolUse hook run. Exit code 2 denies
/// (Claude Code convention; stderr is the reason). A parseable stdout
/// JSON `{"decision":"deny"}` denies with its `reason`. Anything else
/// allows.
fn deny_reason(entry: &HookEntry, out: &RunOutput) -> Option<String> {
    if out.timed_out {
        tracing::warn!(
            command = %entry.command,
            "hooks: PreToolUse hook timed out — allowing (fail-open)"
        );
        return None;
    }
    if out.exit_code == Some(2) {
        let stderr = out.stderr.trim();
        let reason = if stderr.is_empty() {
            format!("denied by hook `{}` (exit 2)", entry.command)
        } else {
            let tail: String = stderr.chars().take(512).collect();
            format!("denied by hook `{}`: {tail}", entry.command)
        };
        return Some(reason);
    }
    if let Ok(v) = serde_json::from_str::<Value>(out.stdout.trim()) {
        if v.get("decision").and_then(Value::as_str) == Some("deny") {
            let reason = v
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("no reason given");
            return Some(format!("denied by hook `{}`: {reason}", entry.command));
        }
    }
    None
}

/// Run one hook subprocess: payload on stdin, scrubbed env, bounded
/// output, hard timeout (kill on expiry). Never errors — failures are
/// folded into `RunOutput` so callers stay fail-open.
async fn run_hook(entry: &HookEntry, payload: &Value) -> RunOutput {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(&entry.command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .env("MU_HOOK_EVENT", entry.event.name())
        .kill_on_drop(true);
    for key in ["PATH", "HOME", "USER", "LANG", "TMPDIR"] {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(command = %entry.command, %err, "hooks: spawn failed — allowing");
            return RunOutput {
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
            };
        }
    };

    let payload_bytes = payload.to_string();
    if let Some(mut stdin) = child.stdin.take() {
        // A hook that never reads stdin must not block us: write on a
        // task, drop the handle to close the pipe either way.
        tokio::spawn(async move {
            let _ = stdin.write_all(payload_bytes.as_bytes()).await;
        });
    }

    match tokio::time::timeout(entry.timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => RunOutput {
            exit_code: output.status.code(),
            stdout: cap_output(output.stdout),
            stderr: cap_output(output.stderr),
            timed_out: false,
        },
        Ok(Err(err)) => {
            tracing::warn!(command = %entry.command, %err, "hooks: wait failed — allowing");
            RunOutput {
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
            }
        }
        Err(_elapsed) => {
            // kill_on_drop reaps the child when `child` drops here.
            RunOutput {
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: true,
            }
        }
    }
}

fn cap_output(bytes: Vec<u8>) -> String {
    let mut s = String::from_utf8_lossy(&bytes).into_owned();
    if s.len() > OUTPUT_CAP {
        s.truncate(OUTPUT_CAP);
    }
    s
}

fn expand_leading_tilde(command: &str) -> String {
    match (command.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => command.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HookEntryConfig, HooksConfig};

    fn engine_of(entries: Vec<HookEntryConfig>) -> Arc<HookEngine> {
        HookEngine::from_config(&HooksConfig {
            enabled: true,
            entries,
        })
        .expect("engine")
    }

    fn entry(event: &str, matcher: Option<&str>, command: &str) -> HookEntryConfig {
        HookEntryConfig {
            event: event.to_owned(),
            matcher: matcher.map(str::to_owned),
            command: command.to_owned(),
            timeout_ms: None,
        }
    }

    #[test]
    fn disabled_or_empty_config_yields_no_engine() {
        assert!(HookEngine::from_config(&HooksConfig {
            enabled: false,
            entries: vec![entry("PreToolUse", None, "true")],
        })
        .is_none());
        assert!(HookEngine::from_config(&HooksConfig {
            enabled: true,
            entries: vec![],
        })
        .is_none());
    }

    #[test]
    fn unknown_event_names_are_skipped() {
        assert!(HookEngine::from_config(&HooksConfig {
            enabled: true,
            entries: vec![entry("UserPromptSubmit", None, "true")],
        })
        .is_none());
    }

    #[test]
    fn matcher_semantics() {
        let e = HookEntry {
            event: HookEventKind::PreToolUse,
            matcher: Some("mcp_*".to_owned()),
            command: "true".to_owned(),
            timeout: DEFAULT_TIMEOUT,
        };
        assert!(e.matches_tool("mcp_code_recall"));
        assert!(!e.matches_tool("bash"));
        let exact = HookEntry {
            matcher: Some("bash".to_owned()),
            ..e.clone()
        };
        assert!(exact.matches_tool("bash"));
        assert!(!exact.matches_tool("bash2"));
        let all = HookEntry { matcher: None, ..e };
        assert!(all.matches_tool("anything"));
    }

    #[tokio::test]
    async fn gate_allows_on_exit_zero() {
        let engine = engine_of(vec![entry("PreToolUse", None, "cat >/dev/null; exit 0")]);
        let deny = engine
            .gate_pre_tool_use("bash", &serde_json::json!({"command": "ls"}))
            .await;
        assert!(deny.is_none());
    }

    #[tokio::test]
    async fn gate_denies_on_exit_two_with_stderr_reason() {
        let engine = engine_of(vec![entry(
            "PreToolUse",
            None,
            "cat >/dev/null; echo 'nope: forbidden' >&2; exit 2",
        )]);
        let deny = engine
            .gate_pre_tool_use("bash", &serde_json::json!({}))
            .await
            .expect("denied");
        assert!(deny.contains("nope: forbidden"), "got: {deny}");
    }

    #[tokio::test]
    async fn gate_denies_on_stdout_json_decision() {
        let engine = engine_of(vec![entry(
            "PreToolUse",
            None,
            r#"cat >/dev/null; echo '{"decision":"deny","reason":"policy says no"}'"#,
        )]);
        let deny = engine
            .gate_pre_tool_use("edit", &serde_json::json!({}))
            .await
            .expect("denied");
        assert!(deny.contains("policy says no"), "got: {deny}");
    }

    #[tokio::test]
    async fn gate_fails_open_on_other_exit_codes_and_missing_command() {
        let engine = engine_of(vec![
            entry("PreToolUse", None, "cat >/dev/null; exit 1"),
            entry("PreToolUse", None, "/nonexistent/hook-script"),
        ]);
        assert!(engine
            .gate_pre_tool_use("bash", &serde_json::json!({}))
            .await
            .is_none());
    }

    #[tokio::test]
    async fn gate_fails_open_on_timeout() {
        let mut e = entry("PreToolUse", None, "cat >/dev/null; sleep 5");
        e.timeout_ms = Some(150);
        let engine = engine_of(vec![e]);
        let started = std::time::Instant::now();
        assert!(engine
            .gate_pre_tool_use("bash", &serde_json::json!({}))
            .await
            .is_none());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn gate_respects_tool_matcher() {
        let engine = engine_of(vec![entry(
            "PreToolUse",
            Some("bash"),
            "cat >/dev/null; exit 2",
        )]);
        // Non-matching tool: hook never runs, call allowed.
        assert!(engine
            .gate_pre_tool_use("edit", &serde_json::json!({}))
            .await
            .is_none());
        // Matching tool: denied.
        assert!(engine
            .gate_pre_tool_use("bash", &serde_json::json!({}))
            .await
            .is_some());
    }

    #[tokio::test]
    async fn hook_receives_payload_on_stdin() {
        let dir = std::env::temp_dir().join(format!(
            "mu-hooks-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let sink = dir.join("payload.json");
        let engine = engine_of(vec![entry(
            "PreToolUse",
            None,
            &format!("cat > {}", sink.display()),
        )]);
        let _ = engine
            .gate_pre_tool_use("bash", &serde_json::json!({"command": "ls -la"}))
            .await;
        let captured = std::fs::read_to_string(&sink).unwrap();
        let v: Value = serde_json::from_str(&captured).unwrap();
        assert_eq!(v["hook_event_name"], "PreToolUse");
        assert_eq!(v["tool_name"], "bash");
        assert_eq!(v["tool_input"]["command"], "ls -la");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn observational_hooks_do_not_block() {
        let engine = engine_of(vec![{
            let mut e = entry("Stop", None, "sleep 5");
            e.timeout_ms = Some(6000);
            e
        }]);
        let started = std::time::Instant::now();
        engine.observe_stop("end_turn", 3);
        // Dispatch returns immediately; the subprocess runs detached.
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn listens_reports_configured_events() {
        let engine = engine_of(vec![entry("Stop", None, "true")]);
        assert!(engine.listens(HookEventKind::Stop));
        assert!(!engine.listens(HookEventKind::PreToolUse));
    }
}
