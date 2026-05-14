//! `grep` tool — content search via ripgrep subprocess.
//!
//! mu doesn't reimplement search; it shells out to `rg` which is
//! near-universal, fast, and respects `.gitignore` by default.
//! Arguments map directly to ripgrep flags. See spec mu-023.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;

use mu_core::agent::{Tool, ToolResult, ToolSpec};
// grep is ReadOnly by default — no policy override needed.
use serde_json::{json, Value};
use tokio::sync::oneshot;

/// mu-yyi: GrepTool now carries an optional `rg_path` field so the
/// ripgrep binary location is injectable at construction. The old
/// design read `MU_RG_BINARY` from process env every call — that
/// worked but meant the "rg not found" test path had to mutate the
/// process env, which races with parallel `#[tokio::test]`s reading
/// the same key. Injecting the path makes the test deterministic and
/// removes the anti-pattern.
///
/// Resolution order (each step takes precedence over the next):
///   1. `rg_path` field set via `with_rg_path`
///   2. `MU_RG_BINARY` env var (preserved for production overrides;
///      no longer used by tests)
///   3. PATH lookup of `rg`
#[derive(Debug, Default)]
pub struct GrepTool {
    rg_path: Option<String>,
}

impl GrepTool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the rg binary path. Tests use this to point at a
    /// non-existent path and exercise the "ripgrep not found" error
    /// without mutating process env. Production callers normally
    /// don't need to set this (PATH lookup is fine); the
    /// `MU_RG_BINARY` env var is still honored as a runtime override.
    pub fn with_rg_path(mut self, path: impl Into<String>) -> Self {
        self.rg_path = Some(path.into());
        self
    }

    fn locate_rg(&self) -> Result<String, String> {
        if let Some(p) = &self.rg_path {
            if !p.is_empty() {
                // Don't validate existence here — let the spawn fail
                // with a clear OS error. Tests rely on a fake path
                // failing at spawn time, not at locate time.
                return Ok(p.clone());
            }
        }
        if let Ok(p) = std::env::var("MU_RG_BINARY") {
            if !p.is_empty() {
                return Ok(p);
            }
        }
        let out = std::process::Command::new("which")
            .arg("rg")
            .output()
            .map_err(|e| format!("which rg failed: {e}"))?;
        if !out.status.success() {
            return Err("ripgrep not found in PATH; install `rg` or set MU_RG_BINARY".to_owned());
        }
        let path = String::from_utf8(out.stdout)
            .map_err(|_| "rg path not utf-8".to_owned())?
            .trim()
            .to_owned();
        if path.is_empty() {
            return Err("rg path empty".to_owned());
        }
        Ok(path)
    }
}

impl Tool for GrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "grep",
            "Search file contents using ripgrep (rg). \
             Returns matching lines with file:line:content, \
             or just file paths / counts based on `output_mode`. \
             Respects .gitignore by default. Pattern is a regex.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "The regular expression to search for."
                    },
                    "path": {
                        "type": "string",
                        "description": "File or directory to search. Defaults to the current working directory."
                    },
                    "glob": {
                        "type": "string",
                        "description": "File-glob filter (e.g. '*.rs', '**/*.{ts,tsx}'). Maps to rg --glob."
                    },
                    "output_mode": {
                        "type": "string",
                        "enum": ["content", "files_with_matches", "count"],
                        "description": "content: matching lines (default). files_with_matches: just file paths. count: match counts per file."
                    },
                    "case_insensitive": {
                        "type": "boolean",
                        "description": "Pass -i. Defaults to false."
                    },
                    "show_line_numbers": {
                        "type": "boolean",
                        "description": "Show line numbers in `content` mode. Defaults to true."
                    },
                    "context": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Lines of context before AND after each match (rg -C). content mode only."
                    },
                    "head_limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Truncate output to the first N lines. Defaults to 250."
                    }
                },
                "required": ["pattern"]
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
        Box::pin(async move {
            let pattern = match arguments.get("pattern").and_then(Value::as_str) {
                Some(p) if !p.is_empty() => p.to_owned(),
                Some(_) => {
                    return ToolResult {
                        content: "grep: empty `pattern` is not allowed".to_owned(),
                        is_error: true,
                    };
                }
                None => {
                    return ToolResult {
                        content: "grep: missing required `pattern` argument".to_owned(),
                        is_error: true,
                    };
                }
            };

            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .map(PathBuf::from);
            let glob = arguments
                .get("glob")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let output_mode = arguments
                .get("output_mode")
                .and_then(Value::as_str)
                .unwrap_or("content")
                .to_owned();
            let case_insensitive = arguments
                .get("case_insensitive")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let show_line_numbers = arguments
                .get("show_line_numbers")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let context = arguments
                .get("context")
                .and_then(Value::as_u64)
                .filter(|c| *c <= 50);
            let head_limit = arguments
                .get("head_limit")
                .and_then(Value::as_u64)
                .unwrap_or(250) as usize;

            let rg = match self.locate_rg() {
                Ok(p) => p,
                Err(e) => {
                    return ToolResult {
                        content: format!("grep: {e}"),
                        is_error: true,
                    };
                }
            };

            let mut cmd = tokio::process::Command::new(&rg);
            // Always use null delimiter free; default human-readable
            // output is fine for the LLM consumer. Add flags by mode.
            match output_mode.as_str() {
                "files_with_matches" => {
                    cmd.arg("--files-with-matches");
                }
                "count" => {
                    cmd.arg("--count");
                }
                "content" | _ => {
                    if show_line_numbers {
                        cmd.arg("--line-number");
                    }
                    if let Some(c) = context {
                        cmd.arg("-C").arg(c.to_string());
                    }
                }
            }
            cmd.arg("--no-heading");
            // Color always off — we're piping to a model, not a terminal.
            cmd.arg("--color").arg("never");
            if case_insensitive {
                cmd.arg("-i");
            }
            if let Some(g) = glob.as_deref() {
                cmd.arg("--glob").arg(g);
            }
            cmd.arg("--").arg(&pattern);
            if let Some(p) = path.as_ref() {
                cmd.arg(p);
            }
            cmd.stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            let child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    return ToolResult {
                        content: format!("grep: failed to spawn rg: {e}"),
                        is_error: true,
                    };
                }
            };

            let wait = child.wait_with_output();
            let output = tokio::select! {
                out = wait => match out {
                    Ok(o) => o,
                    Err(e) => {
                        return ToolResult {
                            content: format!("grep: wait failed: {e}"),
                            is_error: true,
                        };
                    }
                },
                _ = cancel_rx => {
                    // Best-effort cancel: we've moved `child` into
                    // wait_with_output, so we can't kill from here.
                    // The wait future will finish naturally and we
                    // bail without forwarding the result.
                    return ToolResult {
                        content: "grep cancelled".to_owned(),
                        is_error: true,
                    };
                }
            };

            // rg exit codes: 0 = matches, 1 = no matches, 2 = error.
            // We treat no-matches as a success result with explanatory
            // text (the agent often wants to know "nothing matched"
            // without erroring out the whole tool).
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();

            match output.status.code() {
                Some(0) => ToolResult {
                    content: truncate_head(&stdout, head_limit),
                    is_error: false,
                },
                Some(1) => ToolResult {
                    content: format!(
                        "no matches for pattern {pattern:?}{}",
                        path.as_ref()
                            .map(|p| format!(" in {}", p.display()))
                            .unwrap_or_default()
                    ),
                    is_error: false,
                },
                Some(2) => ToolResult {
                    content: format!(
                        "grep: rg returned error: {}",
                        stderr
                            .trim()
                            .is_empty()
                            .then_some("(no stderr)")
                            .unwrap_or(stderr.trim())
                    ),
                    is_error: true,
                },
                other => ToolResult {
                    content: format!(
                        "grep: rg exited with unexpected status {:?}: {}",
                        other,
                        stderr.trim()
                    ),
                    is_error: true,
                },
            }
        })
    }
}

/// Keep at most `n` lines; append a count note if anything was dropped.
fn truncate_head(s: &str, n: usize) -> String {
    let mut count = 0usize;
    let mut end = 0usize;
    for (i, ch) in s.char_indices() {
        if ch == '\n' {
            count += 1;
            if count == n {
                end = i + 1;
                break;
            }
        }
    }
    if end == 0 || count < n {
        return s.to_owned();
    }
    let remaining_lines = s[end..].lines().count();
    if remaining_lines == 0 {
        return s.to_owned();
    }
    format!(
        "{}…\n[truncated {remaining_lines} more lines; raise head_limit to see them]",
        &s[..end]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> Result<PathBuf, Box<dyn Error>> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let p = std::env::temp_dir().join(format!(
            "mu-grep-tool-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&p)?;
        Ok(p)
    }

    async fn execute_grep(args: Value) -> ToolResult {
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        GrepTool::new().execute(args, cancel_rx).await
    }

    fn rg_available() -> bool {
        std::env::var("MU_RG_BINARY")
            .map(|p| !p.is_empty())
            .unwrap_or(false)
            || std::process::Command::new("which")
                .arg("rg")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
    }

    #[test]
    fn spec_describes_grep_tool() {
        let spec = GrepTool::new().spec();
        assert_eq!(spec.name, "grep");
        assert!(spec.description.contains("ripgrep"));
        assert_eq!(spec.input_schema["required"], json!(["pattern"]));
    }

    #[tokio::test]
    async fn b1_content_mode_finds_matches() -> Result<(), Box<dyn Error>> {
        if !rg_available() {
            eprintln!("skipping b1: rg not available");
            return Ok(());
        }
        let dir = temp_dir("b1")?;
        fs::write(dir.join("a.txt"), "hello world\ngoodbye world\n")?;
        fs::write(dir.join("b.txt"), "no match here\n")?;

        let result = execute_grep(json!({
            "pattern": "hello",
            "path": dir.to_string_lossy(),
        }))
        .await;
        let _ = fs::remove_dir_all(&dir);

        assert!(!result.is_error, "got: {}", result.content);
        assert!(result.content.contains("hello world"));
        assert!(!result.content.contains("no match here"));
        Ok(())
    }

    #[tokio::test]
    async fn b2_no_matches_is_success_not_error() -> Result<(), Box<dyn Error>> {
        if !rg_available() {
            return Ok(());
        }
        let dir = temp_dir("b2")?;
        fs::write(dir.join("a.txt"), "hello\n")?;

        let result = execute_grep(json!({
            "pattern": "zzzzz_nothing_matches_this",
            "path": dir.to_string_lossy(),
        }))
        .await;
        let _ = fs::remove_dir_all(&dir);

        assert!(!result.is_error, "got: {}", result.content);
        assert!(result.content.contains("no matches"));
        Ok(())
    }

    #[tokio::test]
    async fn b3_files_with_matches_mode() -> Result<(), Box<dyn Error>> {
        if !rg_available() {
            return Ok(());
        }
        let dir = temp_dir("b3")?;
        fs::write(dir.join("a.txt"), "needle\n")?;
        fs::write(dir.join("b.txt"), "needle\n")?;
        fs::write(dir.join("c.txt"), "haystack\n")?;

        let result = execute_grep(json!({
            "pattern": "needle",
            "path": dir.to_string_lossy(),
            "output_mode": "files_with_matches",
        }))
        .await;
        let _ = fs::remove_dir_all(&dir);

        assert!(!result.is_error);
        assert!(result.content.contains("a.txt"));
        assert!(result.content.contains("b.txt"));
        assert!(!result.content.contains("c.txt"));
        Ok(())
    }

    #[tokio::test]
    async fn b4_count_mode() -> Result<(), Box<dyn Error>> {
        if !rg_available() {
            return Ok(());
        }
        let dir = temp_dir("b4")?;
        fs::write(dir.join("a.txt"), "x\nx\ny\n")?;
        fs::write(dir.join("b.txt"), "x\n")?;

        let result = execute_grep(json!({
            "pattern": "x",
            "path": dir.to_string_lossy(),
            "output_mode": "count",
        }))
        .await;
        let _ = fs::remove_dir_all(&dir);

        assert!(!result.is_error);
        // Output is "<file>:<count>" per file
        assert!(result.content.contains(":2"));
        assert!(result.content.contains(":1"));
        Ok(())
    }

    #[tokio::test]
    async fn b5_glob_filter_restricts_files() -> Result<(), Box<dyn Error>> {
        if !rg_available() {
            return Ok(());
        }
        let dir = temp_dir("b5")?;
        fs::write(dir.join("keep.rs"), "found_me\n")?;
        fs::write(dir.join("skip.txt"), "found_me\n")?;

        let result = execute_grep(json!({
            "pattern": "found_me",
            "path": dir.to_string_lossy(),
            "glob": "*.rs",
        }))
        .await;
        let _ = fs::remove_dir_all(&dir);

        assert!(!result.is_error, "got: {}", result.content);
        assert!(result.content.contains("keep.rs"));
        assert!(!result.content.contains("skip.txt"));
        Ok(())
    }

    #[tokio::test]
    async fn b6_case_insensitive() -> Result<(), Box<dyn Error>> {
        if !rg_available() {
            return Ok(());
        }
        let dir = temp_dir("b6")?;
        fs::write(dir.join("a.txt"), "Hello WORLD\n")?;

        let result = execute_grep(json!({
            "pattern": "hello",
            "path": dir.to_string_lossy(),
            "case_insensitive": true,
        }))
        .await;
        let _ = fs::remove_dir_all(&dir);

        assert!(!result.is_error);
        assert!(result.content.contains("Hello WORLD"));
        Ok(())
    }

    #[tokio::test]
    async fn b7_missing_pattern_errors() {
        let result = execute_grep(json!({})).await;
        assert!(result.is_error);
        assert!(result.content.contains("missing required `pattern`"));
    }

    #[tokio::test]
    async fn b8_empty_pattern_errors() {
        let result = execute_grep(json!({ "pattern": "" })).await;
        assert!(result.is_error);
        assert!(result.content.contains("empty"));
    }

    #[tokio::test]
    async fn b9_head_limit_truncates() -> Result<(), Box<dyn Error>> {
        if !rg_available() {
            return Ok(());
        }
        let dir = temp_dir("b9")?;
        let lines = (0..50).map(|i| format!("match_{i}\n")).collect::<String>();
        fs::write(dir.join("a.txt"), lines)?;

        let result = execute_grep(json!({
            "pattern": "match_",
            "path": dir.to_string_lossy(),
            "head_limit": 5,
        }))
        .await;
        let _ = fs::remove_dir_all(&dir);

        assert!(!result.is_error);
        assert!(result.content.contains("truncated"));
        // Verify at most ~5 result lines before the truncation marker.
        let visible: usize = result
            .content
            .lines()
            .take_while(|l| !l.contains("truncated"))
            .filter(|l| l.contains("match_"))
            .count();
        assert!(visible <= 6, "got {visible} visible match lines");
        Ok(())
    }

    /// mu-yyi: GrepTool with a bogus rg_path injected via
    /// `with_rg_path` — no process-env mutation, so this test no
    /// longer races with parallel `#[tokio::test]`s reading
    /// `MU_RG_BINARY`. Replaces the previously-`#[ignore]`d variant.
    #[tokio::test]
    async fn rg_unavailable_returns_clean_error() {
        let tool = GrepTool::new().with_rg_path("/no/such/binary/rg");
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        let result = tool.execute(json!({ "pattern": "x" }), cancel_rx).await;
        assert!(result.is_error);
        // Either "failed to spawn rg" (bad binary path) or
        // "ripgrep not found" depending on which arm of locate_rg
        // produced the error. Accept any of the diagnostic strings.
        assert!(
            result.content.contains("rg")
                || result.content.contains("ripgrep")
                || result.content.contains("grep:"),
            "expected diagnostic about rg failure; got: {}",
            result.content
        );
    }
}
