# Spec: `edit` tool — string-replacement file editing

| field      | value                                          |
| ---------- | ---------------------------------------------- |
| spec_id    | mu-022                                         |
| status     | ready                                          |
| created    | 2026-05-10                                     |
| updated    | 2026-05-10                                     |
| authors    | tcovert + claude-personal (claude-opus-4.7)    |
| supersedes | none                                           |

## Why

`mu` currently has `read`, `write`, and `ls` tools. It can read code,
write fresh files, and list directories — but it can't *modify*
existing files without rewriting them whole. The whole-file rewrite
pattern is brittle (loses unrelated content, fights file watchers,
re-renders the entire blob in transcripts) and uneconomical
(re-sending a 2000-line file to change 5 characters is profligate).

The `edit` tool is the smallest useful step toward real code-editing:
substitute a unique substring with a new substring. Behavior is
modeled on the same surface this Claude tool offers — proven UX,
proven failure modes, no novelty to debug.

CONVENTIONS apply.

## Scope

- **In:**
  - `mu-coding/src/tools/edit.rs` — `EditTool` implementing `Tool`.
  - Arguments: `path` (string), `old_string` (string),
    `new_string` (string), optional `replace_all` (bool, default
    false).
  - Atomic-ish replace: read whole file → string-replace → write
    whole file. (Not atomic across crash; v1 doesn't promise
    durability semantics beyond `std::fs::write`.)
  - Uniqueness check: when `replace_all` is false, `old_string` must
    occur exactly once. Zero → "not found"; >1 → "ambiguous;
    include more context or set replace_all=true".
  - Same-string check: `old_string == new_string` is rejected (no-op
    that wastes context).
  - Empty-old check: empty `old_string` is rejected (replacing
    "nothing" everywhere doesn't make sense).
  - `cancel_rx` honored — file-read and file-write happen on
    `tokio::task::spawn_blocking` and race with cancel.
  - Wired in `mu-coding/src/serve/factory.rs::build_tools`.
- **Out:**
  - Multi-edit-in-one-call. (`edit` does one replacement; if the
    agent needs multiple, it calls the tool multiple times. Future
    spec for batch-edit if churn proves real.)
  - Regex / line-range / patch-format edits. Future tools (`edit-regex`,
    `apply-patch`).
  - Backup / undo. Mu's event-sourced architecture (per
    `specs/architecture/event-sourced-context.md`) will make undo
    fall out naturally; for now, the model can re-edit to revert.
  - Permission prompts. mu currently runs tools unsupervised; a
    `session.input_required` approval flow is a future spec.
  - Whitespace-normalization, smart-indent matching. v1 is exact
    string match. The agent has to include enough context for the
    match to be unique.

## Invariants

- **INV-1 (CONVENTIONS apply).**
- **INV-2 (uniqueness).** When `replace_all` is false, exactly one
  replacement happens, or the tool errors. Never silently mutate
  multiple occurrences.
- **INV-3 (read-modify-write atomicity within process).** The
  read and write happen on the same thread (`spawn_blocking`), so a
  concurrent edit on the same path from *within mu* would race only
  at the cooperative-task level. External writers can still race.
  v1 accepts this.
- **INV-4 (cancel cleanly).** Cancel before write means the file is
  unchanged. Cancel after write may leave the file modified.

## Interfaces

```rust
// mu-coding/src/tools/edit.rs
pub struct EditTool;

impl Tool for EditTool {
    fn spec(&self) -> ToolSpec {
        // name: "edit"
        // input_schema:
        //   path: string
        //   old_string: string
        //   new_string: string
        //   replace_all: bool (default false)
    }

    async fn execute(&self, arguments: Value, cancel_rx: oneshot::Receiver<()>)
        -> ToolResult;
}
```

Factory wiring (`mu-coding/src/serve/factory.rs::build_tools`):

```rust
"edit" => Ok(Arc::new(EditTool::new()) as Arc<dyn Tool>),
```

## Behaviors

1. **B-1 (replace unique occurrence):** File has "foo bar baz",
   edit("bar", "BAR"); file now has "foo BAR baz". Result `is_error
   = false`.
2. **B-2 (not found):** edit("xyz", "abc") on a file without "xyz"
   returns is_error=true with a message naming the missing string.
3. **B-3 (ambiguous):** File has "x x x", edit("x", "y") returns
   is_error=true mentioning the count.
4. **B-4 (replace_all replaces every occurrence):** Same file, but
   replace_all=true → "y y y", is_error=false, result text reports
   how many replacements.
5. **B-5 (empty old_string):** edit("", "x") returns is_error=true.
6. **B-6 (no-op same string):** edit("foo", "foo") returns
   is_error=true with "old_string == new_string".
7. **B-7 (path doesn't exist):** edit on a nonexistent path returns
   is_error=true with the path in the message.
8. **B-8 (missing required argument):** absent `path`,
   `old_string`, or `new_string` returns is_error=true.
9. **B-9 (cancel before completion):** cancel_tx fires; the edit
   bails out and reports cancelled.
10. **B-10 (factory wiring):** `build_tools(&["edit"])` succeeds.

## Acceptance

- New file: `crates/mu-coding/src/tools/edit.rs`.
- Modified: `crates/mu-coding/src/tools/mod.rs` — `pub use
  edit::EditTool`.
- Modified: `crates/mu-coding/src/serve/factory.rs::build_tools` —
  arm for "edit".
- `cargo build` clean.
- `cargo nextest run` passes (172 → ~182).
- Manual: `mu ask --provider openai-codex --tools edit "edit
  /tmp/foo.txt to change X to Y"` round-trips a tool call and the
  file is modified.

## Out-of-circuit warnings

- **OOC-1 (exact match).** No whitespace normalization. If the model
  writes `foo  bar` (two spaces) and the file has `foo bar` (one
  space), the edit fails. This is a feature, not a bug — fuzzy
  matching is the road to silently editing the wrong region. The
  model learns to copy substrings exactly.

- **OOC-2 (UTF-8 only).** Files are read as UTF-8 via
  `String::from_utf8`. Binary files or non-UTF-8 text will fail at
  read with a clear error.

- **OOC-3 (whole-file rewrite).** Even a 1-character change rewrites
  the entire file. For files >100KB this is wasteful but tolerable.
  A future `apply-patch` tool will be diff-oriented.

- **OOC-4 (no perms/ACL preservation beyond `fs::write` defaults).**
  Standard Rust `fs::write` preserves file mode on most platforms
  but doesn't promise it. If a tool surface needs explicit mode
  preservation (e.g., shell scripts being edited), that's a future
  hardening item.

## Prior work / context

- `mu-007-read-tool.md`, `mu-011-write-tool.md`, `mu-013-ls-tool.md` —
  existing tool specs; edit follows the same `Tool` trait shape.
- `specs/architecture/event-sourced-context.md` — once
  `ContextAssembly` / event log is built, edits become reviewable
  events with full provenance, and undo falls out naturally.

## Changelog

- 2026-05-10 — initial draft (claude-personal).
