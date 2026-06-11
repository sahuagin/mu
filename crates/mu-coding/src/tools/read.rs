use std::future::Future;
use std::io::BufRead;
use std::path::PathBuf;
use std::pin::Pin;

use tokio::sync::oneshot;

use crate::tools::path::expand_leading_tilde;
use mu_core::agent::{Tool, ToolResult, ToolSpec};
use serde_json::{json, Value};

/// Hard cap on bytes a single `read` call may return
/// (mu-mu-solo-loop-terminate-5ek5). The 2026-06-07 incident: an
/// uncapped read slurped a 1.88 GB file into the agent loop, and the
/// resulting span wedged/killed the session (multi-GB copies through
/// the event path, then quadratic tiktoken work in the inline
/// compaction). A read over the cap is a TOOL ERROR — a normal turn
/// event the model can react to — never a loop-threatening slurp.
const DEFAULT_MAX_READ_BYTES: u64 = 5 * 1024 * 1024;

pub struct ReadTool {
    /// Byte cap for a single read (full or ranged). Injectable so
    /// tests exercise the over-cap paths without multi-MB fixtures.
    max_bytes: u64,
}

impl ReadTool {
    pub fn new() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_READ_BYTES,
        }
    }

    /// Override the byte cap. Test-only ergonomics (same pattern as
    /// `GrepTool::with_rg_path`); production callers keep the default.
    #[cfg(test)]
    fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
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
            "Read a file. Returns the file's contents as text. Use for inspecting source code, configs, or any text file the agent needs to consider. Large files must be read in ranges via offset/limit.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative path to the file."
                    },
                    "offset": {
                        "type": "integer",
                        "description": "1-based line number to start reading from. Use with `limit` to read large files in ranges."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to read (from `offset`, or from the start)."
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
        let max_bytes = self.max_bytes;
        Box::pin(async move {
            let path = match path_argument(&arguments) {
                Ok(path) => path,
                Err(result) => return result,
            };
            let range = match range_arguments(&arguments) {
                Ok(range) => range,
                Err(result) => return result,
            };

            let path_for_task = path.clone();
            let read_handle =
                tokio::task::spawn_blocking(move || read_file(&path_for_task, range, max_bytes));

            tokio::select! {
                res = read_handle => match res {
                    Ok(result) => result,
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

/// Optional line range: (1-based start line, optional line count).
/// `None` means a full read.
type LineRange = Option<(u64, Option<u64>)>;

fn range_arguments(arguments: &Value) -> Result<LineRange, ToolResult> {
    let offset = arguments.get("offset").and_then(Value::as_u64);
    let limit = arguments.get("limit").and_then(Value::as_u64);
    match (offset, limit) {
        (None, None) => Ok(None),
        (Some(0), _) => Err(ToolResult {
            content: "`offset` is 1-based; use offset >= 1".to_owned(),
            is_error: true,
        }),
        (offset, limit) => Ok(Some((offset.unwrap_or(1), limit))),
    }
}

/// Blocking read body (runs on the blocking pool). Stats first: a
/// full read of a file over `max_bytes` is refused up front — never
/// slurped — with a pointer at ranged reads. Ranged reads stream
/// line-by-line (terminators preserved, so the output stays verbatim
/// disk bytes for exact-match `edit`) and stop with an error if the
/// requested range itself exceeds the cap.
fn read_file(path: &std::path::Path, range: LineRange, max_bytes: u64) -> ToolResult {
    let (start_line, line_limit) = match range {
        None => {
            // Full read: stat-gate the size BEFORE touching content.
            match std::fs::metadata(path) {
                Ok(meta) if meta.is_file() && meta.len() > max_bytes => {
                    return ToolResult {
                        content: format!(
                            "file is {} — over the {} read cap; read it in ranges with \
                             offset/limit (1-based start line + line count), or grep it \
                             for the parts you need: {}",
                            human_bytes(meta.len()),
                            human_bytes(max_bytes),
                            path.display()
                        ),
                        is_error: true,
                    };
                }
                // Missing file / permission errors fall through to the
                // read below for the existing error shape; directories
                // keep their "read error" result from fs::read.
                _ => {}
            }
            return match std::fs::read(path) {
                Ok(bytes) => utf8_result(bytes, path),
                Err(err) => ToolResult {
                    content: format!("read error for {}: {err}", path.display()),
                    is_error: true,
                },
            };
        }
        Some((start, limit)) => (start, limit),
    };

    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(err) => {
            return ToolResult {
                content: format!("read error for {}: {err}", path.display()),
                is_error: true,
            }
        }
    };
    let mut reader = std::io::BufReader::new(file);
    let mut line: Vec<u8> = Vec::new();
    let mut line_no: u64 = 0;

    // Skip to the requested start line. read_until keeps the
    // terminator, so skipped bytes are counted exactly.
    while line_no + 1 < start_line {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => {
                return ToolResult {
                    content: format!(
                        "offset {start_line} is past the end of {} ({line_no} lines)",
                        path.display()
                    ),
                    is_error: true,
                };
            }
            Ok(_) => line_no += 1,
            Err(err) => {
                return ToolResult {
                    content: format!("read error for {}: {err}", path.display()),
                    is_error: true,
                };
            }
        }
    }

    let mut out: Vec<u8> = Vec::new();
    let mut lines_read: u64 = 0;
    loop {
        if let Some(limit) = line_limit {
            if lines_read >= limit {
                break;
            }
        }
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(n) => {
                if out.len() as u64 + n as u64 > max_bytes {
                    return ToolResult {
                        content: format!(
                            "range starting at line {start_line} exceeds the {} read cap \
                             after {lines_read} lines — narrow the range (smaller limit): {}",
                            human_bytes(max_bytes),
                            path.display()
                        ),
                        is_error: true,
                    };
                }
                out.extend_from_slice(&line);
                lines_read += 1;
            }
            Err(err) => {
                return ToolResult {
                    content: format!("read error for {}: {err}", path.display()),
                    is_error: true,
                };
            }
        }
    }

    utf8_result(out, path)
}

fn utf8_result(bytes: Vec<u8>, path: &std::path::Path) -> ToolResult {
    match String::from_utf8(bytes) {
        Ok(content) => ToolResult {
            content,
            is_error: false,
        },
        Err(_) => ToolResult {
            content: format!("file is not valid UTF-8: {}", path.display()),
            is_error: true,
        },
    }
}

/// Human-readable byte size for cap/error messages ("1.9 GB", "5.0 MB").
fn human_bytes(n: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const KB: f64 = 1024.0;
    let n = n as f64;
    if n >= GB {
        format!("{:.1} GB", n / GB)
    } else if n >= MB {
        format!("{:.1} MB", n / MB)
    } else if n >= KB {
        format!("{:.1} KB", n / KB)
    } else {
        format!("{n} B")
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct EnvVarGuard {
        name: &'static str,
        old_value: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(name: &'static str, value: &std::ffi::OsStr) -> Self {
            let old_value = std::env::var_os(name);
            std::env::set_var(name, value);
            Self { name, old_value }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.old_value {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }

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

    async fn execute_with(tool: ReadTool, args: Value) -> ToolResult {
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        tool.execute(args, cancel_rx).await
    }

    #[test]
    fn spec_describes_read_tool() {
        let spec = ReadTool::new().spec();

        assert_eq!(spec.name, "read");
        assert!(spec.description.contains("Read a file"));
        assert_eq!(spec.input_schema["required"], json!(["path"]));
        assert!(spec.input_schema["properties"]["offset"].is_object());
        assert!(spec.input_schema["properties"]["limit"].is_object());
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

        let result = tokio::time::timeout(
            Duration::from_millis(500),
            ReadTool::new().execute(json!({ "path": path.to_string_lossy() }), cancel_rx),
        )
        .await?;
        let _ = fs::remove_file(&path);

        assert!(result.is_error || result.content == "content");
        Ok(())
    }

    #[tokio::test]
    async fn b7_expands_home_shorthand() -> Result<(), Box<dyn Error>> {
        let home = temp_path("home")?;
        fs::create_dir(&home)?;
        let path = home.join("tilde-file.txt");
        fs::write(&path, "from home")?;

        let _lock = ENV_LOCK.lock().await;
        let _home_guard = EnvVarGuard::set("HOME", home.as_os_str());
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        let result = ReadTool::new()
            .execute(json!({ "path": "~/tilde-file.txt" }), cancel_rx)
            .await;
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&home);

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, "from home");
        Ok(())
    }

    // ── size cap + ranged reads (mu-mu-solo-loop-terminate-5ek5) ────────

    /// The incident path: a full read of an over-cap file must be a
    /// TOOL ERROR (a normal turn event), not a slurp. The message must
    /// point the model at ranged reads.
    #[tokio::test]
    async fn c1_full_read_over_cap_is_tool_error() -> Result<(), Box<dyn Error>> {
        let path = temp_path("over-cap")?;
        fs::write(&path, "x".repeat(4096))?;

        let result = execute_with(
            ReadTool::new().with_max_bytes(1024),
            json!({ "path": path.to_string_lossy() }),
        )
        .await;
        let _ = fs::remove_file(&path);

        assert!(result.is_error, "{}", result.content);
        assert!(result.content.contains("read cap"), "{}", result.content);
        assert!(
            result.content.contains("offset/limit"),
            "{}",
            result.content
        );
        // The error is a message about the file, never its content.
        assert!(result.content.len() < 1024);
        Ok(())
    }

    #[tokio::test]
    async fn c2_ranged_read_returns_requested_lines_verbatim() -> Result<(), Box<dyn Error>> {
        let path = temp_path("ranged")?;
        fs::write(&path, "l1\nl2\nl3\nl4\nl5\n")?;

        let result = execute_with(
            ReadTool::new(),
            json!({ "path": path.to_string_lossy(), "offset": 2, "limit": 3 }),
        )
        .await;
        let _ = fs::remove_file(&path);

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, "l2\nl3\nl4\n");
        Ok(())
    }

    /// Ranged reads work on over-cap files — that's their purpose. The
    /// returned slice stays bounded by the cap.
    #[tokio::test]
    async fn c3_ranged_read_on_over_cap_file_succeeds() -> Result<(), Box<dyn Error>> {
        let path = temp_path("ranged-over-cap")?;
        let mut body = String::new();
        for i in 0..200 {
            body.push_str(&format!("line {i} padding padding padding\n"));
        }
        fs::write(&path, &body)?;

        let result = execute_with(
            ReadTool::new().with_max_bytes(1024),
            json!({ "path": path.to_string_lossy(), "offset": 5, "limit": 2 }),
        )
        .await;
        let _ = fs::remove_file(&path);

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            result.content,
            "line 4 padding padding padding\nline 5 padding padding padding\n"
        );
        Ok(())
    }

    /// A range whose bytes exceed the cap errors instead of slurping.
    #[tokio::test]
    async fn c4_ranged_read_over_cap_is_error() -> Result<(), Box<dyn Error>> {
        let path = temp_path("ranged-too-big")?;
        let mut body = String::new();
        for i in 0..100 {
            body.push_str(&format!("line {i} padding padding padding\n"));
        }
        fs::write(&path, &body)?;

        let result = execute_with(
            ReadTool::new().with_max_bytes(256),
            json!({ "path": path.to_string_lossy(), "offset": 1, "limit": 100 }),
        )
        .await;
        let _ = fs::remove_file(&path);

        assert!(result.is_error, "{}", result.content);
        assert!(
            result.content.contains("narrow the range"),
            "{}",
            result.content
        );
        Ok(())
    }

    #[tokio::test]
    async fn c5_offset_past_eof_is_error() -> Result<(), Box<dyn Error>> {
        let path = temp_path("past-eof")?;
        fs::write(&path, "only\ntwo\n")?;

        let result = execute_with(
            ReadTool::new(),
            json!({ "path": path.to_string_lossy(), "offset": 10 }),
        )
        .await;
        let _ = fs::remove_file(&path);

        assert!(result.is_error, "{}", result.content);
        assert!(
            result.content.contains("past the end"),
            "{}",
            result.content
        );
        assert!(result.content.contains("2 lines"), "{}", result.content);
        Ok(())
    }

    /// Verbatim contract: CRLF terminators survive a ranged read
    /// byte-identically (exact-match `edit` anchors on read output).
    #[tokio::test]
    async fn c6_ranged_read_preserves_crlf() -> Result<(), Box<dyn Error>> {
        let path = temp_path("ranged-crlf")?;
        fs::write(&path, "a\r\nb\r\nc\r\n")?;

        let result = execute_with(
            ReadTool::new(),
            json!({ "path": path.to_string_lossy(), "offset": 2, "limit": 1 }),
        )
        .await;
        let _ = fs::remove_file(&path);

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, "b\r\n");
        Ok(())
    }

    #[tokio::test]
    async fn c7_zero_offset_is_error() -> Result<(), Box<dyn Error>> {
        let path = temp_path("zero-offset")?;
        fs::write(&path, "x\n")?;

        let result = execute_with(
            ReadTool::new(),
            json!({ "path": path.to_string_lossy(), "offset": 0 }),
        )
        .await;
        let _ = fs::remove_file(&path);

        assert!(result.is_error, "{}", result.content);
        assert!(result.content.contains("1-based"), "{}", result.content);
        Ok(())
    }
}
