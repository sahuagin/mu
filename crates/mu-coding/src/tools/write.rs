use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use mu_core::agent::tool_call_cut::suggested_part_kib;
use mu_core::agent::{
    PermissionLevel, RetryPolicy, SideEffects, Tool, ToolPolicy, ToolResult, ToolSpec,
};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::tools::path::expand_leading_tilde;

pub struct WriteTool {
    /// mu-c9b2l: the effective `[session].max_tool_call_bytes`, so the
    /// schema states the size limit this session actually enforces. `None`
    /// (the cap disabled, or a caller with no config in hand) advises the
    /// default's part size.
    max_tool_call_bytes: Option<usize>,
}

impl WriteTool {
    pub fn new() -> Self {
        Self {
            max_tool_call_bytes: None,
        }
    }

    /// Build the tool against a resolved `[session].max_tool_call_bytes`.
    /// The daemon's tool factory has the value; the schema is the only
    /// surface a model reads the limit from, so it has to be the real one.
    pub fn with_max_tool_call_bytes(max_tool_call_bytes: Option<usize>) -> Self {
        Self {
            max_tool_call_bytes,
        }
    }
}

impl Default for WriteTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for WriteTool {
    fn spec(&self) -> ToolSpec {
        // mu-c9b2l: the size limit belongs HERE. A call whose arguments run
        // past the output ceiling is cut mid-JSON and executes nothing, and
        // the tool list is the surface these models actually read — the
        // limit and the way around it (append) have to be in the schema,
        // not in prose. The number is half the EFFECTIVE cap this tool was
        // built with, so lowering `[session].max_tool_call_bytes` moves the
        // figure the model is told rather than leaving it at the default.
        // mu-c9b2l: no output budget here — the tool list is built once at
        // daemon startup, before any session picks a model, so the cap is
        // the only ceiling this surface can state. The cut refusal, which
        // runs with a request in hand, quotes the smaller of the two.
        let part_kib = suggested_part_kib(self.max_tool_call_bytes, None);
        let description = format!(
            "Write a file. Overwrites if the file exists, or appends when `append` is true. \
             Returns confirmation on success or an error message if the write fails. \
             Keep `content` under about {part_kib} KB per call: a larger call is \
             cut off mid-argument and nothing is written. For a larger file, write the first \
             part, then call `write` again for each remaining part with `append: true`."
        );
        ToolSpec::new(
            "write",
            description,
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative path to the file."
                    },
                    "content": {
                        "type": "string",
                        "description": format!(
                            "UTF-8 text to write. Overwrites any existing file at that path \
                             unless `append` is true. Keep this under about \
                             {part_kib} KB per call and write larger files in parts."
                        )
                    },
                    "append": {
                        "type": "boolean",
                        "description": "Append to the file instead of overwriting it \
                                        (creating it if missing). Default false. Use this to \
                                        write a large file in parts."
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
            // mu-c9b2l: false since `append` exists. One static bit covers
            // the whole tool, and a repeated append doubles the file, so the
            // honest value is the one that holds for every call rather than
            // just for overwrites. Nothing reads the flag today, so an
            // overwrite retry a caller chooses to make is unaffected.
            idempotent: false,
            ends_turn_on_success: false,
        })
    }

    fn execute<'life0, 'async_trait>(
        &'life0 self,
        arguments: Value,
        mut cancel_rx: oneshot::Receiver<()>,
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
            // mu-c9b2l: the second bite. Absent or false leaves the
            // overwrite path byte-identical to what it was.
            let append = arguments
                .get("append")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            let path_for_task = path.clone();
            let content_for_task = content.clone();
            let write_handle = tokio::task::spawn_blocking(move || {
                if append {
                    append_bytes(&path_for_task, content_for_task.as_bytes())
                } else {
                    std::fs::write(&path_for_task, content_for_task.as_bytes()).map(|()| None)
                }
            });

            // mu-c9b2l: a blocking task cannot be aborted, so the old
            // `select!` that dropped this handle on cancel stopped nothing —
            // it reported "write cancelled" while the bytes landed anyway,
            // and a caller acting on that by re-issuing the same
            // `append: true` call doubled the content. A file write is
            // short: wait for it, then report what actually happened. The
            // cancel is read afterwards because that is all it can be now —
            // an annotation on a completed write, not a way to stop one.
            let res = write_handle.await;
            let cancelled = cancel_rx.try_recv().is_ok();
            let landed_after_cancel = if cancelled {
                " (cancel arrived after the write landed)"
            } else {
                ""
            };

            // `Some(total)` is the appended case carrying the new file size;
            // `None` is the overwrite case.
            match res {
                Ok(Ok(Some(total))) => ToolResult {
                    content: format!(
                        "appended {} bytes to {} (now {total} bytes){landed_after_cancel}",
                        content.len(),
                        path.display()
                    ),
                    is_error: false,
                },
                Ok(Ok(None)) => ToolResult {
                    content: format!(
                        "wrote {} bytes to {}{landed_after_cancel}",
                        content.len(),
                        path.display()
                    ),
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
            }
        })
    }
}

/// mu-c9b2l: append `bytes` to `path`, creating it if missing, and report
/// the resulting file size. The size is read back from the handle after the
/// write so the model can see the parts adding up — the whole point of
/// chunking is knowing where you are.
fn append_bytes(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<Option<u64>> {
    use std::io::Write as _;

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    Ok(Some(file.metadata()?.len()))
}

fn path_argument(arguments: &Value) -> Result<PathBuf, ToolResult> {
    arguments
        .get("path")
        .and_then(Value::as_str)
        .map(expand_leading_tilde)
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

    async fn execute_append(path: &Path, content: &str) -> ToolResult {
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        WriteTool::new()
            .execute(
                json!({ "path": path.to_string_lossy(), "content": content, "append": true }),
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

    /// mu-c9b2l: the tool list is the surface these models read, so the
    /// per-call size limit and the append route have to be stated there.
    #[test]
    fn spec_states_the_size_limit_and_the_append_route() {
        let spec = WriteTool::new().spec();

        assert!(
            spec.description.contains("under about 16 KB per call"),
            "description should state the limit; got: {}",
            spec.description
        );
        assert!(
            spec.description.contains("cut off mid-argument"),
            "description should say what happens past the limit; got: {}",
            spec.description
        );
        assert!(
            spec.description.contains("`append: true`"),
            "description should name the append route; got: {}",
            spec.description
        );
        assert_eq!(spec.input_schema["properties"]["append"]["type"], "boolean");
        // `append` stays optional — an existing overwrite call is unchanged.
        assert_eq!(spec.input_schema["required"], json!(["path", "content"]));
    }

    /// mu-c9b2l: the schema quotes the EFFECTIVE cap. Lowering
    /// `[session].max_tool_call_bytes` has to move the figure the model is
    /// told, or the advice sends it straight back over the limit.
    #[test]
    fn spec_size_limit_tracks_the_configured_cap() {
        let lowered = WriteTool::with_max_tool_call_bytes(Some(8 * 1024)).spec();
        assert!(
            lowered.description.contains("under about 4 KB per call"),
            "got: {}",
            lowered.description
        );
        assert!(
            lowered.input_schema["properties"]["content"]["description"]
                .as_str()
                .expect("content description")
                .contains("under about 4 KB per call"),
            "got: {}",
            lowered.input_schema["properties"]["content"]["description"]
        );

        // Cap disabled: the default's part size stands in.
        let uncapped = WriteTool::with_max_tool_call_bytes(None).spec();
        assert!(
            uncapped.description.contains("under about 16 KB per call"),
            "got: {}",
            uncapped.description
        );
    }

    /// mu-c9b2l: `append` makes a repeat call additive, so the tool cannot
    /// advertise idempotency for the whole surface.
    #[test]
    fn spec_is_not_idempotent_because_append_exists() {
        assert!(!WriteTool::new().spec().policy.idempotent);
    }

    #[tokio::test]
    async fn mu_c9b2l_append_extends_the_file_and_reports_both_sizes() -> Result<(), Box<dyn Error>>
    {
        let path = temp_path("append")?;

        let first = execute_write(&path, "part one\n").await;
        let second = execute_append(&path, "part two\n").await;
        let written = fs::read_to_string(&path)?;
        let _ = fs::remove_file(&path);

        assert!(!first.is_error);
        assert!(!second.is_error);
        assert_eq!(written, "part one\npart two\n");
        assert!(
            second.content.contains("appended 9 bytes to"),
            "got: {}",
            second.content
        );
        assert!(
            second.content.contains("(now 18 bytes)"),
            "the running total is what makes chunking navigable; got: {}",
            second.content
        );
        Ok(())
    }

    #[tokio::test]
    async fn mu_c9b2l_append_creates_a_missing_file() -> Result<(), Box<dyn Error>> {
        let path = temp_path("append-create")?;
        let _ = fs::remove_file(&path);

        let result = execute_append(&path, "fresh").await;
        let written = fs::read_to_string(&path)?;
        let _ = fs::remove_file(&path);

        assert!(!result.is_error, "got: {}", result.content);
        assert_eq!(written, "fresh");
        assert!(
            result.content.contains("(now 5 bytes)"),
            "got: {}",
            result.content
        );
        Ok(())
    }

    #[tokio::test]
    async fn mu_c9b2l_absent_and_false_append_still_overwrite() -> Result<(), Box<dyn Error>> {
        let path = temp_path("append-false")?;
        fs::write(&path, "first")?;

        // Explicit false.
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        let explicit = WriteTool::new()
            .execute(
                json!({ "path": path.to_string_lossy(), "content": "second", "append": false }),
                cancel_rx,
            )
            .await;
        assert_eq!(fs::read_to_string(&path)?, "second");
        assert!(
            explicit.content.contains("wrote 6 bytes to"),
            "got: {}",
            explicit.content
        );

        // Absent — the pre-mu-c9b2l shape, byte-identical result text.
        let absent = execute_write(&path, "third").await;
        let written = fs::read_to_string(&path)?;
        let _ = fs::remove_file(&path);

        assert_eq!(written, "third");
        assert!(
            absent.content.contains("wrote 5 bytes to"),
            "got: {}",
            absent.content
        );
        assert!(!absent.content.contains("appended"));
        Ok(())
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

        // mu-c9b2l: the budget covers the write itself now — a cancel no
        // longer short-circuits it — so it is sized to catch a hang, not to
        // hold the tool to a latency target. (`spawn_blocking` dispatch on a
        // loaded host stalls for the better part of a second often enough to
        // make a sub-second budget a coin flip.)
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            WriteTool::new().execute(
                json!({ "path": path.to_string_lossy(), "content": "content" }),
                cancel_rx,
            ),
        )
        .await?;
        let written = fs::read_to_string(&path)?;
        let _ = fs::remove_file(&path);

        assert!(!result.is_error, "got: {}", result.content);
        assert!(
            result.content.contains("wrote 7 bytes to"),
            "got: {}",
            result.content
        );
        assert_eq!(written, "content");
        Ok(())
    }

    /// mu-c9b2l: a cancel cannot stop a `spawn_blocking` write. The old
    /// cancel arm dropped the handle and answered "write cancelled" while the
    /// bytes landed regardless — and a caller acting on that by re-issuing the
    /// same `append: true` call doubled the file. The result now reports what
    /// actually happened, so a second append is a decision made on the truth.
    ///
    /// The cancel is armed BEFORE dispatch, so it is pending for the whole
    /// write — the case the old arm got wrong.
    #[tokio::test]
    async fn mu_c9b2l_cancel_during_append_reports_the_bytes_that_landed(
    ) -> Result<(), Box<dyn Error>> {
        let path = temp_path("cancel-append")?;
        fs::write(&path, "head:")?;
        let payload = "tail";

        let (cancel_tx, cancel_rx) = oneshot::channel();
        let _ = cancel_tx.send(());
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            WriteTool::new().execute(
                json!({
                    "path": path.to_string_lossy(),
                    "content": payload,
                    "append": true,
                }),
                cancel_rx,
            ),
        )
        .await?;
        let after = fs::read_to_string(&path)?;
        let _ = fs::remove_file(&path);

        assert!(!result.is_error, "the write landed: {}", result.content);
        assert!(
            result
                .content
                .starts_with(&format!("appended {} bytes to", payload.len())),
            "got: {}",
            result.content
        );
        assert!(
            !result.content.contains("write cancelled"),
            "the write landed, so the result must not claim a cancellation; got: {}",
            result.content
        );
        assert!(
            result
                .content
                .contains("cancel arrived after the write landed"),
            "the cancel is still worth naming; got: {}",
            result.content
        );
        // Appended exactly once, and the reported total is the file's real
        // size — a second identical append is now the caller's choice, not a
        // surprise.
        assert_eq!(after, format!("head:{payload}"));
        assert!(
            result
                .content
                .contains(&format!("now {} bytes", after.len())),
            "got: {}",
            result.content
        );
        Ok(())
    }
}
