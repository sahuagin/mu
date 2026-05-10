# Delegation: implement mu-013 (`ls` tool)

Universal rules: read `specs/delegations/CONVENTIONS.md`. This prompt
covers only spec-specific content.

## Spec to read first

`specs/mu-013-ls-tool.md` — full spec. Mirrors mu-007 (ReadTool) and
mu-011 (WriteTool) almost literally. The §Interfaces block shows the
exact function bodies.

Reference reading: `crates/mu-coding/src/tools/read.rs` and
`crates/mu-coding/src/tools/write.rs` are the structural templates.
Your `LsTool` follows the same pattern: desugared async fn,
spawn_blocking + select!, errors via is_error: true, no async-trait
dep.

## Deliverable

Two files, one new + one modified:

- **`crates/mu-coding/src/tools/ls.rs`** (new) — `LsTool` per
  §Interfaces. Six tests (B-1..B-5 + B-6 best-effort).
- **`crates/mu-coding/src/tools/mod.rs`** (modified) — add
  `pub mod ls;` and `pub use ls::LsTool;`. Don't reorder existing
  entries.

## Spec-specific don'ts

- **Don't add async-trait** — see read.rs / write.rs for the
  desugared pattern.
- **Don't wire `LsTool` into the factory.** mu-014 does that.
- **Don't use `tokio::fs::read_dir`.** Same reasoning as read/write
  — doesn't compose with `tokio::select!` cleanly. Use
  `tokio::task::spawn_blocking` + `std::fs::read_dir`.
- **Don't try to be clever about formatting.** v1 is just newline-
  joined names with `/` for directories. Don't add columns,
  alignment, headers, etc.

## Verification

Per CONVENTIONS Rule 7:

```sh
cargo build -p mu-coding
cargo nextest run -p mu-coding
wc -l crates/mu-coding/src/tools/ls.rs    # under 400
grep -nE '\bunsafe\b|\.unwrap\(\)|\.expect\(|\bpanic\!|\btodo\!|\bunimplemented\!' \
  crates/mu-coding/src/tools/ls.rs \
  | grep -v '^[[:space:]]*//' \
  | grep -v 'cfg(test)'
```

Existing 95 + ~6 new tests = 101+ passing workspace-wide.

## Output envelope

Per CONVENTIONS Rule 5.
