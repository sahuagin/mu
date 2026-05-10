# Spec: `write` tool

| field      | value                                          |
| ---------- | ---------------------------------------------- |
| spec_id    | mu-011                                         |
| status     | ready                                          |
| created    | 2026-05-10                                     |
| updated    | 2026-05-10                                     |
| authors    | tcovert + claude-personal (claude-opus-4.7)    |
| supersedes | none                                           |

## Why

Second concrete `Tool` impl. Same vertical-slice template that
mu-007/mu-009/mu-010 worked through for `read`, applied to `write`.
After this lands, mu-012 (separate small spec) registers `write` in
the factory and adds an end-to-end integration test mirroring
mu-010.

CONVENTIONS apply.

## Scope

- **In:**
  - `crates/mu-coding/src/tools/write.rs` — `WriteTool` struct
    implementing `Tool`. Takes `arguments: { path, content }`,
    writes the file, returns `"wrote N bytes to <path>"` (success) or
    a descriptive error message via `is_error: true`.
  - `crates/mu-coding/src/tools/mod.rs` — `pub mod write;` and
    `pub use write::WriteTool;`.
  - Tests in `tools/write.rs`'s `#[cfg(test)] mod tests`: write a
    new file, overwrite, missing path arg, missing content arg,
    write to nonexistent parent dir, cancel before write completes.

- **Out:**
  - Wiring `write` into the factory and CLI flags. mu-012 does that.
  - Append mode (`{ append: true }`). Future spec adds the optional flag.
  - Atomic writes (write-and-rename). v1 uses plain `std::fs::write`.
  - Sandbox / path filtering. v1 writes wherever the daemon's process
    has permission. Agent-side trust is the user's job for v1.
  - Binary content via base64. v1 takes a UTF-8 string only.

- **Non-goals:**
  - Permission-prompt UX (mu hasn't built a permission system yet).
  - Concurrent-write coordination across sessions.

## Invariants

- **INV-1 (CONVENTIONS apply).**
- **INV-2 (errors via is_error, not Err).** Same as `ReadTool`.
- **INV-3 (cancel honored).** `tokio::task::spawn_blocking` +
  `tokio::select!` against `cancel_rx`. Same shape as `ReadTool`.
- **INV-4 (file size).** Module under 400 lines including tests.

## Interfaces

```rust
// crates/mu-coding/src/tools/write.rs

use std::path::PathBuf;

use serde_json::{json, Value};
use tokio::sync::oneshot;

use mu_core::agent::{Tool, ToolResult, ToolSpec};

pub struct WriteTool;

impl WriteTool {
    pub fn new() -> Self { Self }
}

impl Default for WriteTool { fn default() -> Self { Self::new() } }

impl Tool for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".to_string(),
            description: "Write a file. Overwrites if the file exists. \
                          Returns confirmation on success or an error \
                          message if the write fails."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative path to the file."
                    },
                    "content": {
                        "type": "string",
                        "description": "UTF-8 text to write. Overwrites any \
                                        existing file at that path."
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        cancel_rx: oneshot::Receiver<()>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = ToolResult> + Send + 'a>,
    > {
        Box::pin(async move {
            // Parse path + content.
            let path: PathBuf = match arguments.get("path").and_then(Value::as_str) {
                Some(s) => PathBuf::from(s),
                None => return ToolResult {
                    content: "missing required `path` argument".to_string(),
                    is_error: true,
                },
            };
            let content: String = match arguments.get("content").and_then(Value::as_str) {
                Some(s) => s.to_string(),
                None => return ToolResult {
                    content: "missing required `content` argument".to_string(),
                    is_error: true,
                },
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
                    Ok(Err(e)) => ToolResult {
                        content: format!("write error for {}: {e}", path.display()),
                        is_error: true,
                    },
                    Err(e) => ToolResult {
                        content: format!("write task failed: {e}"),
                        is_error: true,
                    },
                },
                _ = cancel_rx => ToolResult {
                    content: "write cancelled".to_string(),
                    is_error: true,
                },
            }
        })
    }
}
```

(Note the desugared `execute` signature — same trick gpt-5.5 used for
`ReadTool` to avoid pulling `async-trait` into `mu-coding`'s deps.)

## Behaviors

1. **B-1 (write a new file):** Use `std::env::temp_dir().join("mu_011_b1.txt")`.
   Call `WriteTool.execute({path, content: "hello"}, rx)`. Result:
   `is_error: false`, `content` mentions byte count and path. Then
   read the file via `std::fs::read_to_string` and assert it equals
   `"hello"`.

2. **B-2 (overwrite existing file):** Same path, write twice with
   different content. Second result: `is_error: false`. File contents
   match the second write.

3. **B-3 (missing path argument):** Pass `{content: "x"}`. Result:
   `is_error: true`, `content` mentions "missing required `path`".

4. **B-4 (missing content argument):** Pass `{path: "/tmp/x"}`.
   Result: `is_error: true`, `content` mentions "missing required
   `content`".

5. **B-5 (write to nonexistent parent dir):** Pass a path under a dir
   that doesn't exist (e.g., `/tmp/mu_011_no_such_dir/file.txt`).
   Result: `is_error: true`, `content` includes the OS error.

6. **B-6 (cancel before write completes):** Like read's B-6, this is
   best-effort given fast local writes. Fire cancel before calling
   execute. Result: either Cancelled (`is_error: true`, content
   mentions cancel) OR Ok depending on race. The test's contract:
   no panic, no hang, returns within 500ms.

## Acceptance

- New file: `crates/mu-coding/src/tools/write.rs`.
- Modified: `crates/mu-coding/src/tools/mod.rs` (+2 lines).
- `cargo build` clean.
- `cargo nextest run` passes — every existing test plus B-1..B-5
  (B-6 best-effort).
- Module under 400 lines.

## Out-of-circuit warnings

- **OOC-1:** Same `spawn_blocking + select!` pattern as `ReadTool`.
  Reuse the shape. (We could extract a shared helper at some point;
  for v1 it's fine to copy.)
- **OOC-2:** `std::fs::write` overwrites unconditionally. That's the
  documented v1 behavior. If the caller wanted append, that's a
  future spec.

## Prior work

- mu-007 — `ReadTool` (the structural template).

## Changelog

- 2026-05-10 — initial draft (claude-personal).
