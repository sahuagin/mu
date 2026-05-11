//! `edit` tool — string-replacement file editing.
//!
//! Replaces a unique occurrence of `old_string` with `new_string` in
//! a file. With `replace_all = true`, replaces every occurrence and
//! reports the count. Errors when `old_string` is not found, is
//! ambiguous (multiple matches and `replace_all` is false), is empty,
//! or equals `new_string`. See spec mu-022.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use mu_core::agent::{Tool, ToolResult, ToolSpec};
use serde_json::{json, Value};
use tokio::sync::oneshot;

pub struct EditTool;

impl EditTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EditTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for EditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".to_owned(),
            description: "Edit a file by replacing a unique occurrence of `old_string` with `new_string`. \
                          When `old_string` appears multiple times, the call fails unless `replace_all` is true. \
                          The match is exact (no whitespace normalization) — include enough surrounding context \
                          for the match to be unique."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative path to the file to edit."
                    },
                    "old_string": {
                        "type": "string",
                        "description": "Exact substring to replace. Must be unique in the file unless replace_all=true."
                    },
                    "new_string": {
                        "type": "string",
                        "description": "Replacement substring. May be empty (to delete the matched region)."
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "When true, replace every occurrence; report count. Defaults to false.",
                        "default": false
                    }
                },
                "required": ["path", "old_string", "new_string"]
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
        Box::pin(async move {
            let path = match path_argument(&arguments) {
                Ok(p) => p,
                Err(r) => return r,
            };
            let old_string = match string_argument(&arguments, "old_string") {
                Ok(s) => s,
                Err(r) => return r,
            };
            let new_string = match string_argument(&arguments, "new_string") {
                Ok(s) => s,
                Err(r) => return r,
            };
            let replace_all = arguments
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            // Cheap pre-validation that doesn't touch the file.
            if old_string.is_empty() {
                return ToolResult {
                    content: "edit: `old_string` must not be empty".to_owned(),
                    is_error: true,
                };
            }
            if old_string == new_string {
                return ToolResult {
                    content: "edit: `old_string` == `new_string`; nothing to do".to_owned(),
                    is_error: true,
                };
            }

            let path_for_task = path.clone();
            let task_handle = tokio::task::spawn_blocking(move || -> std::io::Result<(String, std::path::PathBuf)> {
                let contents = std::fs::read_to_string(&path_for_task)?;
                Ok((contents, path_for_task))
            });

            let (contents, path_owned) = tokio::select! {
                join = task_handle => match join {
                    Ok(Ok(pair)) => pair,
                    Ok(Err(err)) => {
                        return ToolResult {
                            content: format!("edit: read error for {}: {err}", path.display()),
                            is_error: true,
                        };
                    }
                    Err(err) => {
                        return ToolResult {
                            content: format!("edit: read task failed for {}: {err}", path.display()),
                            is_error: true,
                        };
                    }
                },
                _ = cancel_rx => {
                    return ToolResult {
                        content: "edit cancelled before read".to_owned(),
                        is_error: true,
                    };
                }
            };

            let occurrences = contents.matches(&old_string).count();
            if occurrences == 0 {
                return ToolResult {
                    content: format!(
                        "edit: `old_string` not found in {}",
                        path.display()
                    ),
                    is_error: true,
                };
            }
            if occurrences > 1 && !replace_all {
                return ToolResult {
                    content: format!(
                        "edit: `old_string` appears {occurrences} times in {}; \
                         include more surrounding context to make the match unique, \
                         or set replace_all=true",
                        path.display()
                    ),
                    is_error: true,
                };
            }

            let new_contents = if replace_all {
                contents.replace(&old_string, &new_string)
            } else {
                // Exactly one occurrence — replace by find+splice for clarity.
                let idx = contents.find(&old_string).expect("checked count > 0");
                let mut s = String::with_capacity(
                    contents.len() - old_string.len() + new_string.len(),
                );
                s.push_str(&contents[..idx]);
                s.push_str(&new_string);
                s.push_str(&contents[idx + old_string.len()..]);
                s
            };

            let write_handle = tokio::task::spawn_blocking(move || {
                std::fs::write(&path_owned, new_contents.as_bytes())
                    .map(|_| new_contents.len())
            });

            match write_handle.await {
                Ok(Ok(bytes_written)) => ToolResult {
                    content: if replace_all {
                        format!(
                            "edited {} ({} replacements, {bytes_written} bytes written)",
                            path.display(),
                            occurrences
                        )
                    } else {
                        format!(
                            "edited {} (1 replacement, {bytes_written} bytes written)",
                            path.display()
                        )
                    },
                    is_error: false,
                },
                Ok(Err(err)) => ToolResult {
                    content: format!("edit: write error for {}: {err}", path.display()),
                    is_error: true,
                },
                Err(err) => ToolResult {
                    content: format!("edit: write task failed for {}: {err}", path.display()),
                    is_error: true,
                },
            }
        })
    }
}

fn path_argument(arguments: &Value) -> Result<PathBuf, ToolResult> {
    arguments
        .get("path")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| ToolResult {
            content: "edit: missing required `path` argument".to_owned(),
            is_error: true,
        })
}

fn string_argument(arguments: &Value, name: &str) -> Result<String, ToolResult> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ToolResult {
            content: format!("edit: missing required `{name}` argument"),
            is_error: true,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> Result<PathBuf, Box<dyn Error>> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(std::env::temp_dir().join(format!(
            "mu-edit-tool-{name}-{}-{nanos}",
            std::process::id()
        )))
    }

    async fn execute_edit(
        path: &Path,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> ToolResult {
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        EditTool::new()
            .execute(
                json!({
                    "path": path.to_string_lossy(),
                    "old_string": old_string,
                    "new_string": new_string,
                    "replace_all": replace_all,
                }),
                cancel_rx,
            )
            .await
    }

    #[test]
    fn spec_describes_edit_tool() {
        let spec = EditTool::new().spec();
        assert_eq!(spec.name, "edit");
        assert!(spec.description.contains("Edit a file"));
        assert_eq!(
            spec.input_schema["required"],
            json!(["path", "old_string", "new_string"])
        );
    }

    #[tokio::test]
    async fn b1_replaces_unique_occurrence() -> Result<(), Box<dyn Error>> {
        let path = temp_path("b1")?;
        fs::write(&path, "foo bar baz")?;

        let result = execute_edit(&path, "bar", "BAR", false).await;
        let after = fs::read_to_string(&path)?;
        let _ = fs::remove_file(&path);

        assert!(!result.is_error, "got: {}", result.content);
        assert_eq!(after, "foo BAR baz");
        assert!(result.content.contains("1 replacement"));
        Ok(())
    }

    #[tokio::test]
    async fn b2_not_found_is_error() -> Result<(), Box<dyn Error>> {
        let path = temp_path("b2")?;
        fs::write(&path, "foo bar baz")?;

        let result = execute_edit(&path, "xyz", "abc", false).await;
        let after = fs::read_to_string(&path)?;
        let _ = fs::remove_file(&path);

        assert!(result.is_error);
        assert!(result.content.contains("not found"));
        // File unchanged.
        assert_eq!(after, "foo bar baz");
        Ok(())
    }

    #[tokio::test]
    async fn b3_ambiguous_is_error() -> Result<(), Box<dyn Error>> {
        let path = temp_path("b3")?;
        fs::write(&path, "x x x")?;

        let result = execute_edit(&path, "x", "y", false).await;
        let after = fs::read_to_string(&path)?;
        let _ = fs::remove_file(&path);

        assert!(result.is_error);
        assert!(result.content.contains("appears 3 times"));
        assert!(
            result.content.contains("replace_all=true")
                || result.content.contains("more surrounding context")
        );
        // File unchanged.
        assert_eq!(after, "x x x");
        Ok(())
    }

    #[tokio::test]
    async fn b4_replace_all_replaces_every_occurrence() -> Result<(), Box<dyn Error>> {
        let path = temp_path("b4")?;
        fs::write(&path, "x x x")?;

        let result = execute_edit(&path, "x", "y", true).await;
        let after = fs::read_to_string(&path)?;
        let _ = fs::remove_file(&path);

        assert!(!result.is_error);
        assert_eq!(after, "y y y");
        assert!(result.content.contains("3 replacements"));
        Ok(())
    }

    #[tokio::test]
    async fn b5_empty_old_string_is_error() -> Result<(), Box<dyn Error>> {
        let path = temp_path("b5")?;
        fs::write(&path, "anything")?;

        let result = execute_edit(&path, "", "x", false).await;
        let _ = fs::remove_file(&path);

        assert!(result.is_error);
        assert!(result.content.contains("`old_string` must not be empty"));
        Ok(())
    }

    #[tokio::test]
    async fn b6_no_op_same_string_is_error() -> Result<(), Box<dyn Error>> {
        let path = temp_path("b6")?;
        fs::write(&path, "foo")?;

        let result = execute_edit(&path, "foo", "foo", false).await;
        let _ = fs::remove_file(&path);

        assert!(result.is_error);
        assert!(result.content.contains("nothing to do"));
        Ok(())
    }

    #[tokio::test]
    async fn b7_nonexistent_path_is_error() -> Result<(), Box<dyn Error>> {
        let path = temp_path("b7-nope")?;
        // Don't create the file.
        let result = execute_edit(&path, "a", "b", false).await;
        assert!(result.is_error);
        assert!(result.content.contains("read error"));
        assert!(result.content.contains(&path.to_string_lossy().to_string()));
        Ok(())
    }

    #[tokio::test]
    async fn b8_missing_arguments_are_errors() {
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        let result = EditTool::new()
            .execute(json!({ "old_string": "a", "new_string": "b" }), cancel_rx)
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("missing required `path`"));

        let (_cancel_tx, cancel_rx) = oneshot::channel();
        let result = EditTool::new()
            .execute(json!({ "path": "/tmp/x", "new_string": "b" }), cancel_rx)
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("missing required `old_string`"));

        let (_cancel_tx, cancel_rx) = oneshot::channel();
        let result = EditTool::new()
            .execute(json!({ "path": "/tmp/x", "old_string": "a" }), cancel_rx)
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("missing required `new_string`"));
    }

    #[tokio::test]
    async fn b9_cancel_before_completion() -> Result<(), Box<dyn Error>> {
        let path = temp_path("b9")?;
        fs::write(&path, "foo")?;

        let (cancel_tx, cancel_rx) = oneshot::channel();
        // Fire cancel immediately — the edit may or may not have
        // started the read yet. Either way, the result should be an
        // error indicating cancel, OR a clean success if the read
        // finished first (we can't deterministically guarantee
        // which on every machine).
        let _ = cancel_tx.send(());
        let result = EditTool::new()
            .execute(
                json!({
                    "path": path.to_string_lossy(),
                    "old_string": "foo",
                    "new_string": "bar",
                }),
                cancel_rx,
            )
            .await;
        let _ = fs::remove_file(&path);

        // Either cancelled (preferred) or succeeded — both are
        // acceptable depending on race. NOT acceptable: hung or
        // panicked.
        if result.is_error {
            assert!(
                result.content.contains("cancelled") || result.content.contains("read error"),
                "unexpected error message: {}",
                result.content
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn delete_via_empty_new_string_works() -> Result<(), Box<dyn Error>> {
        // Empty `new_string` is allowed (delete the matched region).
        let path = temp_path("delete")?;
        fs::write(&path, "before-MIDDLE-after")?;

        let result = execute_edit(&path, "-MIDDLE-", "", false).await;
        let after = fs::read_to_string(&path)?;
        let _ = fs::remove_file(&path);

        assert!(!result.is_error);
        assert_eq!(after, "beforeafter");
        Ok(())
    }

    #[tokio::test]
    async fn multiline_replacement_works() -> Result<(), Box<dyn Error>> {
        let path = temp_path("multiline")?;
        let content = "fn old_name() {\n    body\n}\n";
        fs::write(&path, content)?;

        let result = execute_edit(
            &path,
            "fn old_name() {\n    body\n}",
            "fn new_name() {\n    new_body\n}",
            false,
        )
        .await;
        let after = fs::read_to_string(&path)?;
        let _ = fs::remove_file(&path);

        assert!(!result.is_error, "got: {}", result.content);
        assert!(after.contains("fn new_name()"));
        assert!(after.contains("new_body"));
        Ok(())
    }
}
