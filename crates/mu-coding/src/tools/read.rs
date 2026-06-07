use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use mu_core::agent::{Tool, ToolResult, ToolSpec};
use serde_json::{json, Value};
use tokio::sync::oneshot;

pub struct ReadTool;

impl ReadTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ReadTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for ReadTool {
    fn spec(&self) -> ToolSpec {
        // mu-cvm5: ReadOnly + Allow is now an EXPLICIT opt-in via
        // `.read_only()` — the default fails closed (Mutating + Ask).
        // verbatim_result: read output is the model's belief about
        // disk truth — exact-match `edit` builds on it, so the tier-1
        // ingestion filter (mu-2e0h) must never collapse/cap/truncate
        // it.
        ToolSpec::new(
            "read",
            "Read a file. Returns the file's contents as text. Use for inspecting source code, configs, or any text file the agent needs to consider.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative path to the file."
                    }
                },
                "required": ["path"]
            }),
        )
        .read_only()
        .with_verbatim_result()
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

            let path_for_task = path.clone();
            let read_handle = tokio::task::spawn_blocking(move || std::fs::read(&path_for_task));

            tokio::select! {
                res = read_handle => match res {
                    Ok(Ok(bytes)) => match String::from_utf8(bytes) {
                        Ok(content) => ToolResult { content, is_error: false },
                        Err(_) => ToolResult {
                            content: format!("file is not valid UTF-8: {}", path.display()),
                            is_error: true,
                        },
                    },
                    Ok(Err(err)) => ToolResult {
                        content: format!("read error for {}: {err}", path.display()),
                        is_error: true,
                    },
                    Err(err) => ToolResult {
                        content: format!("read task failed for {}: {err}", path.display()),
                        is_error: true,
                    },
                },
                _ = cancel_rx => ToolResult {
                    content: "read cancelled".to_owned(),
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
            "mu-read-tool-{name}-{}-{nanos}",
            std::process::id()
        )))
    }

    async fn execute_read(path: &Path) -> ToolResult {
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        ReadTool::new()
            .execute(json!({ "path": path.to_string_lossy() }), cancel_rx)
            .await
    }

    #[test]
    fn spec_describes_read_tool() {
        let spec = ReadTool::new().spec();

        assert_eq!(spec.name, "read");
        assert!(spec.description.contains("Read a file"));
        assert_eq!(spec.input_schema["required"], json!(["path"]));
    }

    #[tokio::test]
    async fn b1_reads_real_file() -> Result<(), Box<dyn Error>> {
        let path = temp_path("real-file")?;
        fs::write(&path, "hello\nworld\n")?;

        let result = execute_read(&path).await;
        let _ = fs::remove_file(&path);

        assert!(!result.is_error);
        assert_eq!(result.content, "hello\nworld\n");
        Ok(())
    }

    #[tokio::test]
    async fn b2_nonexistent_file_is_error() -> Result<(), Box<dyn Error>> {
        let path = temp_path("missing")?;

        let result = execute_read(&path).await;

        assert!(result.is_error);
        assert!(result.content.contains(&path.to_string_lossy().to_string()));
        Ok(())
    }

    #[tokio::test]
    async fn b3_missing_path_argument_is_error() {
        let (_cancel_tx, cancel_rx) = oneshot::channel();

        let result = ReadTool::new().execute(json!({}), cancel_rx).await;

        assert!(result.is_error);
        assert!(result.content.contains("missing required `path` argument"));
    }

    #[tokio::test]
    async fn b4_directory_path_is_error() -> Result<(), Box<dyn Error>> {
        let path = temp_path("directory")?;
        fs::create_dir(&path)?;

        let result = execute_read(&path).await;
        let _ = fs::remove_dir(&path);

        assert!(result.is_error);
        assert!(result.content.contains("read error"));
        Ok(())
    }

    #[tokio::test]
    async fn b5_invalid_utf8_is_error() -> Result<(), Box<dyn Error>> {
        let path = temp_path("invalid-utf8")?;
        fs::write(&path, [0xff, 0xfe, 0x00])?;

        let result = execute_read(&path).await;
        let _ = fs::remove_file(&path);

        assert!(result.is_error);
        assert!(result.content.contains("not valid UTF-8"));
        Ok(())
    }

    #[tokio::test]
    async fn b6_cancel_before_read_does_not_hang() -> Result<(), Box<dyn Error>> {
        let path = temp_path("cancel")?;
        fs::write(&path, "content")?;
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let _ = cancel_tx.send(());

        let result = ReadTool::new()
            .execute(json!({ "path": path.to_string_lossy() }), cancel_rx)
            .await;
        let _ = fs::remove_file(&path);

        assert!(result.is_error || result.content == "content");
        Ok(())
    }
}
