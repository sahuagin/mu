//! `glob` tool — file-name pattern matching via `fd` subprocess.
//!
//! Complements `grep`: grep searches *content*, glob searches *paths*.
//! `fd` respects `.gitignore` and is fast. See spec mu-024.

use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;

use mu_core::agent::{Tool, ToolResult, ToolSpec};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::tools::path::expand_leading_tilde;

pub struct GlobTool;

impl GlobTool {
    pub fn new() -> Self {
        Self
    }

    fn locate_fd() -> Result<String, String> {
        if let Ok(p) = std::env::var("MU_FD_BINARY") {
            if !p.is_empty() {
                return Ok(p);
            }
        }
        let out = std::process::Command::new("which")
            .arg("fd")
            .output()
            .map_err(|e| format!("which fd failed: {e}"))?;
        if !out.status.success() {
            return Err("`fd` not found in PATH; install fd-find or set MU_FD_BINARY".to_owned());
        }
        let path = String::from_utf8(out.stdout)
            .map_err(|_| "fd path not utf-8".to_owned())?
            .trim()
            .to_owned();
        if path.is_empty() {
            return Err("fd path empty".to_owned());
        }
        Ok(path)
    }
}

impl Default for GlobTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for GlobTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "glob",
            "Find files by name pattern using `fd`. \
             Pattern is a glob by default (*.rs, src/**/*.ts); \
             pass `glob: false` to interpret pattern as a regex instead. \
             Respects .gitignore. Returns one path per line.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Name pattern. Glob by default; regex if `glob: false` is set."
                    },
                    "path": {
                        "type": "string",
                        "description": "Root directory to search. Defaults to the current working directory."
                    },
                    "glob": {
                        "type": "boolean",
                        "description": "Treat `pattern` as a glob (fd --glob). Default true; set false for regex.",
                        "default": true
                    },
                    "kind": {
                        "type": "string",
                        "enum": ["file", "directory", "any"],
                        "description": "Filter results by type. `any` = no filter. Default `file`."
                    },
                    "case_insensitive": {
                        "type": "boolean",
                        "description": "Pass -i. Default false."
                    },
                    "hidden": {
                        "type": "boolean",
                        "description": "Include hidden files/dirs (fd --hidden). Default false."
                    },
                    "head_limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Truncate output to the first N lines. Default 250."
                    }
                },
                "required": ["pattern"]
            }),
        )
        // mu-cvm5: explicit read-only opt-in (default now fails closed).
        .read_only()
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
                        content: "glob: empty `pattern` is not allowed".to_owned(),
                        is_error: true,
                    };
                }
                None => {
                    return ToolResult {
                        content: "glob: missing required `pattern` argument".to_owned(),
                        is_error: true,
                    };
                }
            };
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .map(expand_leading_tilde);
            // mu-wkn: a tool named "glob" should glob by default.
            // Legacy callers passing explicit `glob: true` still work;
            // regex behavior is preserved via explicit `glob: false`.
            let use_glob = arguments
                .get("glob")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let kind = arguments
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("file")
                .to_owned();
            let case_insensitive = arguments
                .get("case_insensitive")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let hidden = arguments
                .get("hidden")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let head_limit = arguments
                .get("head_limit")
                .and_then(Value::as_u64)
                .unwrap_or(250) as usize;

            let fd = match Self::locate_fd() {
                Ok(p) => p,
                Err(e) => {
                    return ToolResult {
                        content: format!("glob: {e}"),
                        is_error: true,
                    };
                }
            };

            let mut cmd = tokio::process::Command::new(&fd);
            if use_glob {
                cmd.arg("--glob");
            }
            match kind.as_str() {
                "file" => {
                    cmd.arg("--type").arg("f");
                }
                "directory" => {
                    cmd.arg("--type").arg("d");
                }
                _ => {} // "any" or unrecognized → no --type
            }
            if case_insensitive {
                cmd.arg("-i");
            }
            if hidden {
                cmd.arg("--hidden");
            }
            cmd.arg("--color").arg("never");
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
                        content: format!("glob: failed to spawn fd: {e}"),
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
                            content: format!("glob: wait failed: {e}"),
                            is_error: true,
                        };
                    }
                },
                _ = cancel_rx => {
                    return ToolResult {
                        content: "glob cancelled".to_owned(),
                        is_error: true,
                    };
                }
            };

            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();

            // fd: 0 = matches present (or none — fd doesn't distinguish
            // success-with-zero-results vs success-with-results, unlike rg),
            // non-zero = real error.
            if output.status.success() {
                if stdout.trim().is_empty() {
                    ToolResult {
                        content: format!(
                            "no matches for pattern {pattern:?}{}",
                            path.as_ref()
                                .map(|p| format!(" under {}", p.display()))
                                .unwrap_or_default()
                        ),
                        is_error: false,
                    }
                } else {
                    ToolResult {
                        content: truncate_head(&stdout, head_limit),
                        is_error: false,
                    }
                }
            } else {
                ToolResult {
                    content: format!(
                        "glob: fd exited with status {:?}: {}",
                        output.status.code(),
                        stderr.trim()
                    ),
                    is_error: true,
                }
            }
        })
    }
}

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
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> Result<PathBuf, Box<dyn Error>> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let p = std::env::temp_dir().join(format!(
            "mu-glob-tool-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&p)?;
        Ok(p)
    }

    async fn execute_glob(args: Value) -> ToolResult {
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        GlobTool::new().execute(args, cancel_rx).await
    }

    fn fd_available() -> bool {
        std::env::var("MU_FD_BINARY")
            .map(|p| !p.is_empty())
            .unwrap_or(false)
            || std::process::Command::new("which")
                .arg("fd")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
    }

    #[test]
    fn spec_describes_glob_tool() {
        let spec = GlobTool::new().spec();
        assert_eq!(spec.name, "glob");
        assert!(spec.description.to_lowercase().contains("fd"));
        assert_eq!(spec.input_schema["required"], json!(["pattern"]));
    }

    #[tokio::test]
    async fn b1_finds_files_by_regex() -> Result<(), Box<dyn Error>> {
        if !fd_available() {
            return Ok(());
        }
        let dir = temp_dir("b1")?;
        fs::write(dir.join("alpha.rs"), "")?;
        fs::write(dir.join("beta.rs"), "")?;
        fs::write(dir.join("gamma.txt"), "")?;

        // mu-wkn: glob is now the default; this test asserts the
        // regex path still works when explicitly opted into.
        let result = execute_glob(json!({
            "pattern": r"\.rs$",
            "path": dir.to_string_lossy(),
            "glob": false,
        }))
        .await;
        let _ = fs::remove_dir_all(&dir);

        assert!(!result.is_error, "got: {}", result.content);
        assert!(result.content.contains("alpha.rs"));
        assert!(result.content.contains("beta.rs"));
        assert!(!result.content.contains("gamma.txt"));
        Ok(())
    }

    #[tokio::test]
    async fn b2_glob_syntax_when_glob_flag() -> Result<(), Box<dyn Error>> {
        if !fd_available() {
            return Ok(());
        }
        let dir = temp_dir("b2")?;
        fs::write(dir.join("foo.rs"), "")?;
        fs::write(dir.join("foo.txt"), "")?;

        let result = execute_glob(json!({
            "pattern": "*.rs",
            "path": dir.to_string_lossy(),
            "glob": true,
        }))
        .await;
        let _ = fs::remove_dir_all(&dir);

        assert!(!result.is_error, "got: {}", result.content);
        assert!(result.content.contains("foo.rs"));
        assert!(!result.content.contains("foo.txt"));
        Ok(())
    }

    #[tokio::test]
    async fn b3_kind_directory_only() -> Result<(), Box<dyn Error>> {
        if !fd_available() {
            return Ok(());
        }
        let dir = temp_dir("b3")?;
        fs::create_dir_all(dir.join("subdir-target"))?;
        fs::write(dir.join("file-target.txt"), "")?;

        // mu-wkn: under default-glob the substring match needs `*target*`.
        let result = execute_glob(json!({
            "pattern": "*target*",
            "path": dir.to_string_lossy(),
            "kind": "directory",
        }))
        .await;
        let _ = fs::remove_dir_all(&dir);

        assert!(!result.is_error);
        assert!(result.content.contains("subdir-target"));
        assert!(!result.content.contains("file-target.txt"));
        Ok(())
    }

    #[tokio::test]
    async fn b4_no_matches_is_success() -> Result<(), Box<dyn Error>> {
        if !fd_available() {
            return Ok(());
        }
        let dir = temp_dir("b4")?;
        fs::write(dir.join("a.txt"), "")?;

        let result = execute_glob(json!({
            "pattern": "definitely_does_not_exist",
            "path": dir.to_string_lossy(),
        }))
        .await;
        let _ = fs::remove_dir_all(&dir);

        assert!(!result.is_error);
        assert!(result.content.contains("no matches"));
        Ok(())
    }

    #[tokio::test]
    async fn b5_missing_pattern_errors() {
        let result = execute_glob(json!({})).await;
        assert!(result.is_error);
        assert!(result.content.contains("missing required `pattern`"));
    }

    #[tokio::test]
    async fn b6_empty_pattern_errors() {
        let result = execute_glob(json!({ "pattern": "" })).await;
        assert!(result.is_error);
        assert!(result.content.contains("empty"));
    }

    #[tokio::test]
    async fn b7_case_insensitive() -> Result<(), Box<dyn Error>> {
        if !fd_available() {
            return Ok(());
        }
        let dir = temp_dir("b7")?;
        fs::write(dir.join("MyFile.txt"), "")?;

        // mu-wkn: under default-glob a bare word matches files
        // whose name is exactly that; widen with `*myfile*` for
        // the substring-case-insensitive behavior this test asserts.
        let result = execute_glob(json!({
            "pattern": "*myfile*",
            "path": dir.to_string_lossy(),
            "case_insensitive": true,
        }))
        .await;
        let _ = fs::remove_dir_all(&dir);

        assert!(!result.is_error);
        assert!(result.content.contains("MyFile.txt"));
        Ok(())
    }

    #[tokio::test]
    async fn wkn_default_is_glob_when_flag_omitted() -> Result<(), Box<dyn Error>> {
        // mu-wkn: invocation `glob {pattern: "*.rs"}` (no flag) should
        // glob, not regex — that's the foot-gun this bead fixes.
        if !fd_available() {
            return Ok(());
        }
        let dir = temp_dir("wkn-default-glob")?;
        fs::write(dir.join("alpha.rs"), "")?;
        fs::write(dir.join("beta.rs"), "")?;
        fs::write(dir.join("gamma.txt"), "")?;

        let result = execute_glob(json!({
            "pattern": "*.rs",
            "path": dir.to_string_lossy(),
        }))
        .await;
        let _ = fs::remove_dir_all(&dir);

        assert!(!result.is_error, "got: {}", result.content);
        assert!(result.content.contains("alpha.rs"));
        assert!(result.content.contains("beta.rs"));
        assert!(
            !result.content.contains("gamma.txt"),
            "default-glob should not match gamma.txt against *.rs; got: {}",
            result.content
        );
        Ok(())
    }

    #[test]
    fn wkn_spec_declares_glob_default_true() {
        let spec = GlobTool::new().spec();
        assert_eq!(spec.input_schema["properties"]["glob"]["default"], true);
    }
}
