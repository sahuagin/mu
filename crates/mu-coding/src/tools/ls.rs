use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use mu_core::agent::{Tool, ToolResult, ToolSpec};
use serde_json::{json, Value};
use tokio::sync::oneshot;

pub struct LsTool;

impl LsTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LsTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for LsTool {
    fn spec(&self) -> ToolSpec {
        // mu-cvm5: explicit read-only opt-in (default now fails closed).
        ToolSpec::new(
            "ls",
            "List the contents of a directory (one level only). Directories are suffixed with '/'. Returns names one per line. Defaults to the current directory if no path is given.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path. Defaults to '.' if omitted."
                    }
                },
                "required": []
            }),
        )
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
            let path = path_argument(&arguments);
            let path_for_task = path.clone();
            let read_handle = tokio::task::spawn_blocking(move || list_dir(&path_for_task));

            tokio::select! {
                res = read_handle => match res {
                    Ok(Ok(listing)) => ToolResult { content: listing, is_error: false },
                    Ok(Err(err)) => ToolResult {
                        content: format!("ls error for {}: {err}", path.display()),
                        is_error: true,
                    },
                    Err(err) => ToolResult {
                        content: format!("ls task failed for {}: {err}", path.display()),
                        is_error: true,
                    },
                },
                _ = cancel_rx => ToolResult {
                    content: "ls cancelled".to_owned(),
                    is_error: true,
                },
            }
        })
    }
}

fn path_argument(arguments: &Value) -> PathBuf {
    arguments
        .get("path")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn list_dir(path: &Path) -> std::io::Result<String> {
    let entries = std::fs::read_dir(path)?;
    let mut names = Vec::new();

    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let suffix = if entry
            .file_type()
            .map(|file_type| file_type.is_dir())
            .unwrap_or(false)
        {
            "/"
        } else {
            ""
        };
        names.push(format!("{name}{suffix}"));
    }

    Ok(names.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use std::fs;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> Result<PathBuf, Box<dyn Error>> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(std::env::temp_dir().join(format!("mu-ls-tool-{name}-{}-{nanos}", std::process::id())))
    }

    async fn execute_ls(path: &Path) -> ToolResult {
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        LsTool::new()
            .execute(json!({ "path": path.to_string_lossy() }), cancel_rx)
            .await
    }

    fn sorted_lines(content: &str) -> Vec<String> {
        let mut lines = content.lines().map(str::to_owned).collect::<Vec<_>>();
        lines.sort();
        lines
    }

    #[test]
    fn spec_describes_ls_tool() {
        let spec = LsTool::new().spec();

        assert_eq!(spec.name, "ls");
        assert!(spec
            .description
            .contains("List the contents of a directory"));
        assert_eq!(spec.input_schema["required"], json!([]));
    }

    #[tokio::test]
    async fn b1_lists_directory_entries() -> Result<(), Box<dyn Error>> {
        let dir = temp_path("b1")?;
        fs::create_dir(&dir)?;
        fs::write(dir.join("alpha.txt"), "alpha")?;
        fs::create_dir(dir.join("subdir"))?;
        fs::write(dir.join("omega.txt"), "omega")?;

        let result = execute_ls(&dir).await;
        let _ = fs::remove_dir_all(&dir);

        assert!(!result.is_error);
        assert_eq!(
            sorted_lines(&result.content),
            vec!["alpha.txt", "omega.txt", "subdir/"]
        );
        Ok(())
    }

    #[tokio::test]
    async fn b2_missing_path_defaults_to_current_dir() {
        let (_cancel_tx, cancel_rx) = oneshot::channel();

        let result = LsTool::new().execute(json!({}), cancel_rx).await;

        assert!(!result.is_error);
        assert!(!result.content.is_empty());
    }

    #[tokio::test]
    async fn b3_nonexistent_directory_is_error() -> Result<(), Box<dyn Error>> {
        let path = temp_path("missing")?;

        let result = execute_ls(&path).await;

        assert!(result.is_error);
        assert!(result.content.contains(&path.to_string_lossy().to_string()));
        Ok(())
    }

    #[tokio::test]
    async fn b4_file_path_is_error() -> Result<(), Box<dyn Error>> {
        let path = temp_path("file")?;
        fs::write(&path, "content")?;

        let result = execute_ls(&path).await;
        let _ = fs::remove_file(&path);

        assert!(result.is_error);
        assert!(result.content.contains("ls error"));
        assert!(
            result.content.contains("directory")
                || result.content.contains("Directory")
                || result.content.contains("not a directory")
        );
        Ok(())
    }

    #[tokio::test]
    async fn b5_suffixes_only_directories_with_slash() -> Result<(), Box<dyn Error>> {
        let dir = temp_path("b5")?;
        fs::create_dir(&dir)?;
        fs::write(dir.join("file.txt"), "file")?;
        fs::create_dir(dir.join("child"))?;

        let result = execute_ls(&dir).await;
        let _ = fs::remove_dir_all(&dir);

        assert!(!result.is_error);
        let lines = sorted_lines(&result.content);
        assert!(lines.iter().any(|line| line == "child/"));
        assert!(lines.iter().any(|line| line == "file.txt"));
        assert!(!lines.iter().any(|line| line == "file.txt/"));
        Ok(())
    }

    #[tokio::test]
    async fn b6_cancel_before_ls_does_not_hang() -> Result<(), Box<dyn Error>> {
        let dir = temp_path("cancel")?;
        fs::create_dir(&dir)?;
        fs::write(dir.join("file.txt"), "content")?;
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let _ = cancel_tx.send(());

        let result = tokio::time::timeout(
            Duration::from_millis(500),
            LsTool::new().execute(json!({ "path": dir.to_string_lossy() }), cancel_rx),
        )
        .await?;
        let _ = fs::remove_dir_all(&dir);

        assert!(result.is_error || result.content.contains("file.txt"));
        Ok(())
    }
}
