# Spec: `read` tool

| field      | value                                          |
| ---------- | ---------------------------------------------- |
| spec_id    | mu-007                                         |
| status     | ready                                          |
| created    | 2026-05-10                                     |
| updated    | 2026-05-10                                     |
| authors    | tcovert + claude-personal (claude-opus-4.7)    |
| supersedes | none                                           |

## Why

First concrete `Tool` implementation. With mu-006 the agent can talk
to real Claude; with mu-007 the agent can also read files. Together
they're the foundation of "agent that can do something with the
codebase." Tiny, mechanical, well-scoped. Good first delegation
chunk for gpt-pro.

This v1 deliberately doesn't wire tools into `mu serve` yet —
mu-coding's hardcoded `Vec::new()` tool list stays empty for now
because tool support in `AnthropicProvider::stream` is deferred
(mu-006 §Out). The `Tool` impl exists, has tests, and is ready to
plug in once Provider tool support lands.

## Scope

- **In:**
  - **`crates/mu-coding/src/tools/mod.rs`** — module root.
  - **`crates/mu-coding/src/tools/read.rs`** — `ReadTool` struct
    implementing `Tool`. Reads a file path from `arguments.path`,
    returns the file contents (or an error message on failure).
  - **`crates/mu-coding/src/lib.rs`** — `pub mod tools;`.
  - Tests in `tools/read.rs`'s `#[cfg(test)] mod tests`: read a
    real file (write one to `tempdir`), read a nonexistent file
    (gets is_error: true), read a directory (gets is_error: true),
    handle the `path` argument missing (gets is_error: true).

- **Out:**
  - Wiring this Tool into `mu serve`'s session creation. mu-coding's
    `dispatch::handle_create_session` still passes `Vec::new()` for
    tools. Wiring is a future spec, gated on Provider-side tool
    support landing in mu-006-extended.
  - Path security (chrooting to a workspace root, blocking `/etc/passwd`,
    etc.). v1 reads any path the daemon's process can access. Adding
    safety is a future spec.
  - Output truncation. v1 returns the whole file as `content`. Large
    files will produce large tool results — fine for now, future
    spec adds limits.
  - Binary file handling. v1 returns the bytes as a UTF-8 string;
    invalid UTF-8 produces an `is_error: true` result with a message.

- **Non-goals:**
  - Implementing other file tools (write, edit, ls, find, grep, bash).
    Each is its own spec.
  - Implementing the `tempfile` crate dep. v1 tests use
    `std::env::temp_dir()` + `std::fs` directly, no new dep.

## Invariants

- **INV-1 (Tool trait shape):** `ReadTool` implements
  `mu_core::agent::Tool` exactly per its existing definition. No
  changes to the trait.
- **INV-2 (errors via is_error):** Any failure (path missing, not a
  file, permission denied, invalid UTF-8) returns
  `ToolResult { content: <descriptive message>, is_error: true }`.
  Never panic, never `Err(_)` from execute (the trait returns
  `ToolResult` directly).
- **INV-3 (no unsafe, no unwrap/expect/panic outside tests):**
  Standard.
- **INV-4 (no new workspace deps):** Use `std::fs`, `std::path`,
  `serde_json::Value`. All available.
- **INV-5 (file size):** Module under 400 lines including tests.
- **INV-6 (cancel honored):** If `cancel_rx` fires before/during
  the read, the function should return promptly. For typical local-
  filesystem reads this is microseconds and the cancel path may
  never fire — but the implementation should still poll the cancel
  receiver (or use it via `select!`) rather than blocking on a
  potentially slow read.

## Interfaces

```rust
// crates/mu-coding/src/tools/read.rs

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use mu_core::agent::{Tool, ToolResult, ToolSpec};

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

#[async_trait]
impl Tool for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read".to_string(),
            description: "Read a file. Returns the file's contents as text. \
                          Use for inspecting source code, configs, or any text \
                          file the agent needs to consider."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative path to the file."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(
        &self,
        arguments: Value,
        cancel_rx: oneshot::Receiver<()>,
    ) -> ToolResult {
        // Parse path argument.
        let path: PathBuf = match arguments.get("path").and_then(Value::as_str) {
            Some(s) => PathBuf::from(s),
            None => {
                return ToolResult {
                    content: "missing required `path` argument".to_string(),
                    is_error: true,
                };
            }
        };

        // Race the read against the cancel.
        // Spawn the read on a blocking task so cancel can preempt it.
        let path_for_task = path.clone();
        let read_handle = tokio::task::spawn_blocking(move || std::fs::read(&path_for_task));

        tokio::select! {
            res = read_handle => match res {
                Ok(Ok(bytes)) => match String::from_utf8(bytes) {
                    Ok(s) => ToolResult { content: s, is_error: false },
                    Err(_) => ToolResult {
                        content: format!("file is not valid UTF-8: {}", path.display()),
                        is_error: true,
                    },
                },
                Ok(Err(e)) => ToolResult {
                    content: format!("read error for {}: {e}", path.display()),
                    is_error: true,
                },
                Err(e) => ToolResult {
                    content: format!("read task panicked or was cancelled: {e}"),
                    is_error: true,
                },
            },
            _ = cancel_rx => ToolResult {
                content: "read cancelled".to_string(),
                is_error: true,
            },
        }
    }
}
```

## Behaviors

1. **B-1 (read a file):** Write a file under `std::env::temp_dir()` with
   known content. Call `ReadTool.execute(json!({"path": path}), rx)`.
   Result: `is_error: false`, `content` matches what was written.

2. **B-2 (nonexistent file):** Path that doesn't exist. Result:
   `is_error: true`, `content` mentions the path.

3. **B-3 (path argument missing):** Empty JSON object `{}` as
   arguments. Result: `is_error: true`, `content` mentions
   "missing required `path` argument".

4. **B-4 (directory not file):** Pass a directory path. Result:
   `is_error: true`, `content` mentions a read error.

5. **B-5 (invalid UTF-8):** Write a file with invalid UTF-8 bytes
   (e.g., `vec![0xff, 0xfe, 0x00]`). Result: `is_error: true`,
   `content` mentions "not valid UTF-8".

6. **B-6 (cancel before read completes):** Pass a cancel signal that
   fires before the read; result has `is_error: true`. Hard to
   make deterministic for fast local reads, so this test is
   best-effort: fire cancel BEFORE calling execute, and assert
   either outcome (Cancelled OR Ok) — what we're verifying is no
   panic / no hang.

## Acceptance

- New files at the paths in §Scope.
- Modified file: `crates/mu-coding/src/lib.rs` (+1 line).
- `cargo build` clean.
- `cargo nextest run` passes — every existing test (63) plus the
  new B-1..B-5 (B-6 best-effort) = 68+ minimum.
- `read.rs` module under 400 lines.

## Iteration-aware handoff

Mechanical and gpt-pro-able. Estimated 100-200 LOC. If gpt-pro
finishes well under iteration cap, the test count will be the
verification check.

## Open questions

- [ ] OQ-1: Should `read` accept an optional `lines: Range<usize>`
  argument like a more sophisticated read tool? — owner: defer —
  resolution: no for v1. Future spec adds it.

## Out-of-circuit warnings

- **OOC-1:** `tokio::task::spawn_blocking` returns `JoinHandle<T>`
  whose `.await` resolves to `Result<T, JoinError>`. The unwrap
  cascade in the matching pattern needs to handle BOTH the join
  error and the inner `io::Error`. The §Interfaces sketch shows the
  shape.
- **OOC-2:** Returning `Err(_)` from `execute` is forbidden — the
  trait returns `ToolResult` directly per mu-003 INV (errors via
  `is_error: true`).
- **OOC-3:** Don't try to use `tokio::fs::read` instead of
  `spawn_blocking + std::fs::read`. tokio's filesystem ops also use
  `spawn_blocking` internally, but they don't compose with
  `tokio::select!` for cancel. The explicit `spawn_blocking + select!`
  pattern is the load-bearing piece.

## Prior work / context

- mu-003 — `Tool` trait, `ToolResult`, `ToolSpec`.
- mu-004 — `MockTool` in mu-core's loop_tests.rs is the structural
  reference.
- pi_ts has `core/tools/read.ts` for cross-checking the JSON-schema
  shape (their schema includes optional offset/limit; we omit for
  v1).

## Changelog

- 2026-05-10 — initial draft (claude-personal).
