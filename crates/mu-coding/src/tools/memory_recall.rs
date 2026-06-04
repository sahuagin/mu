//! `memory_recall` — in-session semantic recall over the operator's
//! memory store (mu-oee9, goal mu-ac5k).
//!
//! The small-kernel injection rework (mu-zk2i) demotes everything but
//! the identity kernel to recall-only. Without an in-session recall
//! path that demotion is memory amputation — the tail exists but the
//! agent can't reach it at the point of need. This tool is that path:
//! it shells out to the operator's `agent` CLI (`agent memory recall
//! <query> --k N`), whose output carries testimony labels
//! (recorded/verified/by-whom — agent_tools at-baj) on every hit.
//!
//! Results enter the rope as ordinary tool_result spans at the tail
//! (Hot): they age out under normal compaction (mu-tlri drops from
//! the tail region only) and are re-recallable on demand — the
//! "dynamic injections land at the tail" half of the
//! memory-hierarchy-and-trust spec's injection economics.
//!
//! Same subprocess conventions as
//! [`SubprocessRecallProvider`](mu_core::context::recall): default
//! binary `~/.local/bin/agent`, graceful degradation when absent
//! (error result, not panic), `with_binary` for test stubs.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Command;

use mu_core::agent::{Tool, ToolResult, ToolSpec};
use serde_json::{json, Value};
use tokio::sync::oneshot;

/// Default top-k when the caller omits `k`.
const DEFAULT_K: usize = 5;
/// Cap on caller-supplied `k` — recall is a tail injection; a huge k
/// would defeat the small-kernel economics this tool exists to serve.
const MAX_K: usize = 20;

pub struct MemoryRecallTool {
    binary_path: PathBuf,
}

impl MemoryRecallTool {
    /// Standard construction: `~/.local/bin/agent`, falling back to a
    /// bare `agent` (PATH lookup) if `$HOME` is unset.
    pub fn new() -> Self {
        let path = dirs::home_dir()
            .map(|h| h.join(".local").join("bin").join("agent"))
            .unwrap_or_else(|| PathBuf::from("agent"));
        Self::with_binary(path)
    }

    /// Test hook: point at a stub script instead of the real CLI.
    pub fn with_binary(binary_path: impl Into<PathBuf>) -> Self {
        Self {
            binary_path: binary_path.into(),
        }
    }
}

impl Default for MemoryRecallTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for MemoryRecallTool {
    fn spec(&self) -> ToolSpec {
        // ReadOnly + Allow + ModelDecides is the default policy.
        ToolSpec::new(
            "memory_recall",
            "Semantic recall over the operator's long-term memory store. Session-start context \
             carries only a small identity kernel — project history, prior decisions, operating \
             constraints, references, and war stories live here. Query by topic in plain language \
             whenever the operator references past work, a prior decision, or context you don't \
             have (e.g. \"jj dual-remote push convention\", \"compaction trigger calibration\"). \
             Every hit carries a testimony label (recorded/verified/by-whom) — memories are \
             testimony, not ground truth: terrain-check before consequential action, and prefer \
             agreeing top-k hits over a single match. Read-only.",
            json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Topic to recall, in plain language."
                    },
                    "k": {
                        "type": "integer",
                        "description": "Max results (default 5, cap 20)."
                    },
                    "full": {
                        "type": "boolean",
                        "description": "Include full memory content instead of one-line summaries (default false). Use after a one-line pass identifies the memory that matters."
                    }
                },
                "required": ["query"]
            }),
        )
    }

    fn validate(&self, arguments: &Value) -> Result<(), String> {
        match arguments.get("query").and_then(Value::as_str) {
            Some(q) if !q.trim().is_empty() => Ok(()),
            Some(_) => Err("memory_recall: 'query' must be non-empty".to_owned()),
            None => Err("memory_recall: missing required string argument 'query'".to_owned()),
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
        Box::pin(async move {
            // Defense-in-depth: direct unit tests of execute bypass
            // the dispatcher's validate() call.
            if let Err(reason) = self.validate(&arguments) {
                return ToolResult {
                    content: reason,
                    is_error: true,
                };
            }
            let query = arguments
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let k = arguments
                .get("k")
                .and_then(Value::as_u64)
                .map(|k| (k as usize).clamp(1, MAX_K))
                .unwrap_or(DEFAULT_K);
            let full = arguments
                .get("full")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            let binary = self.binary_path.clone();
            let handle = tokio::task::spawn_blocking(move || {
                let mut cmd = Command::new(&binary);
                cmd.arg("memory")
                    .arg("recall")
                    .arg(&query)
                    .arg("--k")
                    .arg(k.to_string());
                if full {
                    cmd.arg("--full");
                }
                cmd.output()
            });

            tokio::select! {
                res = handle => match res {
                    Ok(Ok(out)) if out.status.success() => {
                        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                        if stdout.is_empty() {
                            ToolResult {
                                content: "no memories matched the query".to_owned(),
                                is_error: false,
                            }
                        } else {
                            ToolResult { content: stdout, is_error: false }
                        }
                    }
                    Ok(Ok(out)) => ToolResult {
                        content: format!(
                            "memory_recall: agent CLI exited {}: {}",
                            out.status,
                            String::from_utf8_lossy(&out.stderr).trim()
                        ),
                        is_error: true,
                    },
                    Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => ToolResult {
                        content: "memory_recall: `agent` CLI not found at ~/.local/bin/agent — \
                                  memory store unavailable on this host"
                            .to_owned(),
                        is_error: true,
                    },
                    Ok(Err(e)) => ToolResult {
                        content: format!("memory_recall: failed to spawn agent CLI: {e}"),
                        is_error: true,
                    },
                    Err(e) => ToolResult {
                        content: format!("memory_recall: task failed: {e}"),
                        is_error: true,
                    },
                },
                _ = cancel_rx => ToolResult {
                    content: "memory_recall cancelled".to_owned(),
                    is_error: true,
                },
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never_cancelled() -> oneshot::Receiver<()> {
        let (_tx, rx) = oneshot::channel();
        std::mem::forget(_tx);
        rx
    }

    /// Write an executable stub script that echoes its argv and a
    /// canned labeled hit, so tests exercise the real spawn path.
    fn stub_script(dir: &std::path::Path, body: &str) -> PathBuf {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("agent-stub.sh");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh\n{body}").unwrap();
        let mut perms = f.metadata().unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[test]
    fn validate_rejects_missing_or_empty_query() {
        let tool = MemoryRecallTool::new();
        assert!(tool.validate(&json!({})).is_err());
        assert!(tool.validate(&json!({"query": "  "})).is_err());
        assert!(tool
            .validate(&json!({"query": "jj push convention"}))
            .is_ok());
    }

    #[tokio::test]
    async fn happy_path_passes_args_and_returns_labeled_stdout() {
        let dir = std::env::temp_dir().join(format!("mu-oee9-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Stub echoes argv then a canned labeled one-liner.
        let stub = stub_script(
            &dir,
            r#"echo "argv: $@"
echo '[0.812] [abc12345] (feedback) some-rule — desc  [2026-06-04]'
echo '  recorded 2026-06-04 · never verified'"#,
        );
        let tool = MemoryRecallTool::with_binary(&stub);
        let result = tool
            .execute(
                json!({"query": "push convention", "k": 3}),
                never_cancelled(),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result
                .content
                .contains("argv: memory recall push convention --k 3"),
            "args must reach the CLI verbatim; got: {}",
            result.content
        );
        assert!(
            result.content.contains("never verified"),
            "testimony label must pass through to the agent"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn full_flag_and_k_clamp_propagate() {
        let dir = std::env::temp_dir().join(format!("mu-oee9-full-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stub = stub_script(&dir, r#"echo "argv: $@""#);
        let tool = MemoryRecallTool::with_binary(&stub);
        let result = tool
            .execute(
                json!({"query": "q", "k": 999, "full": true}),
                never_cancelled(),
            )
            .await;
        assert!(result.content.contains("--k 20"), "k must clamp to MAX_K");
        assert!(result.content.contains("--full"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_binary_degrades_to_error_result() {
        let tool = MemoryRecallTool::with_binary("/nonexistent/path/to/agent");
        let result = tool
            .execute(json!({"query": "anything"}), never_cancelled())
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("not found"));
    }

    #[tokio::test]
    async fn missing_query_is_error_result_not_panic() {
        let tool = MemoryRecallTool::with_binary("/nonexistent/path/to/agent");
        let result = tool.execute(json!({}), never_cancelled()).await;
        assert!(result.is_error);
        assert!(result.content.contains("query"));
    }
}
