# Spec: `ls` tool

| field      | value                                          |
| ---------- | ---------------------------------------------- |
| spec_id    | mu-013                                         |
| status     | ready                                          |
| created    | 2026-05-10                                     |
| updated    | 2026-05-10                                     |
| authors    | tcovert + claude-personal (claude-opus-4.7)    |
| supersedes | none                                           |

## Why

Third concrete `Tool` impl. Same vertical-slice template as read /
write. Lists directory contents so the agent can navigate the
filesystem without reading every file. mu-014 (separate small spec)
wires it into the factory and adds an end-to-end live test.

CONVENTIONS apply.

## Scope

- **In:**
  - `crates/mu-coding/src/tools/ls.rs` — `LsTool` struct
    implementing `Tool`. Takes `arguments: { path }` (path defaults
    to `"."` if missing). Returns a newline-joined list of entries
    in the directory, with a trailing `/` suffix on directories so
    the model can distinguish without a separate type field.
  - `crates/mu-coding/src/tools/mod.rs` — `pub mod ls;` and
    `pub use ls::LsTool;`.
  - Tests covering: list a known directory, list with trailing-slash
    distinction, missing path defaults to ".", nonexistent path
    error, file-not-directory error, cancel-before-completes.

- **Out:**
  - Recursive listing (`-R` style). v1 lists one level only.
    Future spec adds depth.
  - Hidden-file toggle. v1 always lists hidden files (anything
    `readdir` returns, including dotfiles). Future spec adds an
    optional `hidden: false` flag.
  - File metadata (size, mtime, permissions). v1 returns names only.
    Future spec adds a `details: true` flag that returns
    `name<TAB>kind<TAB>size` per line.
  - Sort order. v1 returns entries in whatever order the OS gives
    them (typically not sorted). Future spec adds an optional
    `sort: name|mtime|size` flag.

- **Non-goals:**
  - Glob/pattern matching (`ls *.rs`). That's `find` or `grep`.

## Invariants

- **INV-1 (CONVENTIONS apply).**
- **INV-2 (errors via is_error, not Err).** Same as read/write.
- **INV-3 (cancel honored).** `tokio::task::spawn_blocking` +
  `tokio::select!` against `cancel_rx`. Same shape as ReadTool/WriteTool.
- **INV-4 (file size).** Module under 400 lines including tests.
- **INV-5 (default path is `.`).** A missing `path` argument is NOT
  an error — it lists the current working directory. (Differs from
  read/write where path is required; ls with no args is a sensible
  default.)

## Interfaces

```rust
// crates/mu-coding/src/tools/ls.rs

use std::path::PathBuf;

use serde_json::{json, Value};
use tokio::sync::oneshot;

use mu_core::agent::{Tool, ToolResult, ToolSpec};

pub struct LsTool;

impl LsTool {
    pub fn new() -> Self { Self }
}

impl Default for LsTool { fn default() -> Self { Self::new() } }

impl Tool for LsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ls".to_string(),
            description: "List the contents of a directory (one level only). \
                          Directories are suffixed with '/'. Returns names \
                          one per line. Defaults to the current directory \
                          if no path is given."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path. Defaults to '.' if omitted."
                    }
                },
                "required": []
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
            let path: PathBuf = arguments
                .get("path")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));

            let path_for_task = path.clone();
            let read_handle = tokio::task::spawn_blocking(move || {
                list_dir(&path_for_task)
            });

            tokio::select! {
                res = read_handle => match res {
                    Ok(Ok(listing)) => ToolResult { content: listing, is_error: false },
                    Ok(Err(e)) => ToolResult {
                        content: format!("ls error for {}: {e}", path.display()),
                        is_error: true,
                    },
                    Err(e) => ToolResult {
                        content: format!("ls task failed: {e}"),
                        is_error: true,
                    },
                },
                _ = cancel_rx => ToolResult {
                    content: "ls cancelled".to_string(),
                    is_error: true,
                },
            }
        })
    }
}

/// Read directory entries, format as one-per-line text. Directories
/// get a trailing `/`. Returns an io::Error if the path doesn't exist
/// or isn't a directory.
fn list_dir(path: &std::path::Path) -> std::io::Result<String> {
    let entries = std::fs::read_dir(path)?;
    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let suffix = if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
            "/"
        } else {
            ""
        };
        names.push(format!("{name}{suffix}"));
    }
    Ok(names.join("\n"))
}
```

## Behaviors

1. **B-1 (list a directory):** Create a temp dir with three known
   entries (a file, an empty subdir, another file). `LsTool.execute({path: tmp_path}, rx)`.
   Result: `is_error: false`. Content lines are exactly the three
   entry names. Subdir entry has trailing `/`.

2. **B-2 (default path is current dir):** Pass `{}` as arguments.
   Result: `is_error: false`. Content is non-empty (assumes the test
   binary's cwd has at least one entry, which it does — at minimum
   the cargo target dir or test binary itself). Don't assert specific
   contents; just non-empty + no error.

3. **B-3 (nonexistent directory):** Pass a path that doesn't exist.
   Result: `is_error: true`, content mentions the path.

4. **B-4 (path is a file, not a directory):** Pass a known file's
   path. Result: `is_error: true`, content mentions a directory-like
   error.

5. **B-5 (trailing slash on directories):** Verify for B-1 that the
   subdir's entry literally ends with `/` and the file entries don't.

6. **B-6 (cancel before completes):** Best-effort; same shape as
   read/write B-6.

## Acceptance

- New file: `crates/mu-coding/src/tools/ls.rs`.
- Modified: `crates/mu-coding/src/tools/mod.rs` (+2 lines).
- `cargo build` clean.
- `cargo nextest run` passes — every existing test plus B-1..B-5
  (B-6 best-effort).
- Module under 400 lines.

## Out-of-circuit warnings

- **OOC-1:** Same `spawn_blocking + select!` pattern as ReadTool /
  WriteTool. Reuse the shape; this is the third instance and a
  refactor candidate, but explicitly OUT of mu-013's scope (will
  evaluate after this slice lands).
- **OOC-2:** `entry.file_type()` returns `io::Result<FileType>`.
  On platforms where it requires a syscall and that fails, fall
  back to "no slash suffix" rather than erroring the whole listing.
  The §Interfaces sketch shows this: `entry.file_type().map(...).unwrap_or(false)`.
- **OOC-3:** `entry.file_name()` returns OsString. Convert via
  `.to_string_lossy().into_owned()`. Some filenames may contain
  invalid UTF-8 on FreeBSD; `to_string_lossy` substitutes U+FFFD
  for invalid sequences. That's correct for v1 — the agent wants
  a human-readable string, and lossy substitution is better than
  failing.

## Prior work

- mu-007 — `ReadTool` (structural template).
- mu-011 — `WriteTool` (also follows the template).

## Changelog

- 2026-05-10 — initial draft (claude-personal).
