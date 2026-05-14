use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use mu_core::agent::{
    PermissionLevel, RetryPolicy, SideEffects, Tool, ToolPolicy, ToolResult, ToolSpec,
};
use serde_json::{json, Value};
use tokio::sync::oneshot;

pub struct WriteTool;

impl WriteTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for WriteTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "write",
            "Write a file. Overwrites if the file exists. Returns confirmation on success or an error message if the write fails.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative path to the file."
                    },
                    "content": {
                        "type": "string",
                        "description": "UTF-8 text to write. Overwrites any existing file at that path."
                    }
                },
                "required": ["path", "content"]
            }),
        )
        .with_policy(ToolPolicy {
            side_effects: SideEffects::Mutating,
            permission: PermissionLevel::Allow,
            retry: RetryPolicy::ModelDecides,
            required_aws_capability: None,
            idempotent: true, // same path + same content = same end state
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
        Box::pin(async move {
            let path = match path_argument(&arguments) {
                Ok(path) => path,
                Err(result) => return result,
            };
            let content = match content_argument(&arguments) {
                Ok(content) => content,
                Err(result) => return result,
            };

            let path_for_task = path.clone();
            let content_for_task = content.clone();
            let write_handle = tokio::task::spawn_blocking(move || {
                std::fs::write(&path_for_task, content_for_task.as_bytes())
            });

            tokio::select! {
                res = write_handle => match res {
                    Ok(Ok(())) => ToolResult {
                        content: format!("wrote {} bytes to {}", content.len(), path.display()),
                        is_error: false,
                    },
                    Ok(Err(err)) => ToolResult {
                        content: format!("write error for {}: {err}", path.display()),
                        is_error: true,
                    },
                    Err(err) => ToolResult {
                        content: format!("write task failed for {}: {err}", path.display()),
                        is_error: true,
                    },
                },
                _ = cancel_rx => ToolResult {
                    content: "write cancelled".to_owned(),
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
            content: "missing required `path` argument".to_owned(),
            is_error: true,
        })
}

fn content_argument(arguments: &Value) -> Result<String, ToolResult> {
    arguments
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ToolResult {
            content: "missing required `content` argument".to_owned(),
            is_error: true,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> Result<PathBuf, Box<dyn Error>> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(std::env::temp_dir().join(format!(
            "mu-write-tool-{name}-{}-{nanos}",
            std::process::id()
        )))
    }

    async fn execute_write(path: &Path, content: &str) -> ToolResult {
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        WriteTool::new()
            .execute(
                json!({ "path": path.to_string_lossy(), "content": content }),
                cancel_rx,
            )
            .await
    }

    #[test]
    fn spec_describes_write_tool() {
        let spec = WriteTool::new().spec();

        assert_eq!(spec.name, "write");
        assert!(spec.description.contains("Write a file"));
        assert_eq!(spec.input_schema["required"], json!(["path", "content"]));
    }

    #[tokio::test]
    async fn b1_writes_new_file() -> Result<(), Box<dyn Error>> {
        let path = temp_path("b1")?;

        let result = execute_write(&path, "hello").await;
        let written = fs::read_to_string(&path)?;
        let _ = fs::remove_file(&path);

        assert!(!result.is_error);
        assert!(result.content.contains("wrote 5 bytes to"));
        assert!(result.content.contains(&path.to_string_lossy().to_string()));
        assert_eq!(written, "hello");
        Ok(())
    }

    #[tokio::test]
    async fn b2_overwrites_existing_file() -> Result<(), Box<dyn Error>> {
        let path = temp_path("b2")?;
        fs::write(&path, "first")?;

        let result = execute_write(&path, "second").await;
        let written = fs::read_to_string(&path)?;
        let _ = fs::remove_file(&path);

        assert!(!result.is_error);
        assert_eq!(written, "second");
        Ok(())
    }

    #[tokio::test]
    async fn b3_missing_path_argument_is_error() {
        let (_cancel_tx, cancel_rx) = oneshot::channel();

        let result = WriteTool::new()
            .execute(json!({ "content": "x" }), cancel_rx)
            .await;

        assert!(result.is_error);
        assert!(result.content.contains("missing required `path` argument"));
    }

    #[tokio::test]
    async fn b4_missing_content_argument_is_error() {
        let (_cancel_tx, cancel_rx) = oneshot::channel();

        let result = WriteTool::new()
            .execute(json!({ "path": "/tmp/x" }), cancel_rx)
            .await;

        assert!(result.is_error);
        assert!(result
            .content
            .contains("missing required `content` argument"));
    }

    #[tokio::test]
    async fn b5_nonexistent_parent_dir_is_error() -> Result<(), Box<dyn Error>> {
        let dir = temp_path("no-such-dir")?;
        let path = dir.join("file.txt");
        let _ = fs::remove_dir_all(&dir);

        let result = execute_write(&path, "content").await;

        assert!(result.is_error);
        assert!(result.content.contains("write error"));
        assert!(result.content.contains(&path.to_string_lossy().to_string()));
        Ok(())
    }

    #[tokio::test]
    async fn b6_cancel_before_write_does_not_hang() -> Result<(), Box<dyn Error>> {
        let path = temp_path("cancel")?;
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let _ = cancel_tx.send(());

        let result = tokio::time::timeout(
            Duration::from_millis(500),
            WriteTool::new().execute(
                json!({ "path": path.to_string_lossy(), "content": "content" }),
                cancel_rx,
            ),
        )
        .await?;
        let _ = fs::remove_file(&path);

        assert!(result.is_error || result.content.contains("wrote 7 bytes to"));
        Ok(())
    }
}
