# Delegation: implement mu-011 (`write` tool)

Universal rules: read `specs/delegations/CONVENTIONS.md`. This prompt
covers only spec-specific content.

## Spec to read first

`specs/mu-011-write-tool.md` — full spec. Mirrors the shape of
mu-007's `ReadTool`. Key shape: §Interfaces shows the exact
function bodies (you're translating spec into code, not designing).

Reference reading: `crates/mu-coding/src/tools/read.rs` is the
structural template. Your `WriteTool` follows the same pattern
(desugared async fn, spawn_blocking + select!, error-via-is_error).

## Deliverable

Two files, one new + one modified:

- **`crates/mu-coding/src/tools/write.rs`** (new) — `WriteTool`
  per §Interfaces. Six tests B-1..B-6 (B-6 best-effort).
- **`crates/mu-coding/src/tools/mod.rs`** (modified) — add
  `pub mod write;` and `pub use write::WriteTool;`. Don't reorder
  existing entries.

## Spec-specific don'ts

- **Don't add `async-trait` as a dep.** Use the desugared
  `execute` signature exactly like `ReadTool` does. Look at
  `crates/mu-coding/src/tools/read.rs` for the pattern.
- **Don't wire `WriteTool` into the factory.** mu-012 does that.
- **Don't use `tokio::fs::write`.** The same reason mu-007 OOC-3
  cited for read: doesn't compose with `tokio::select!` for cancel.
  Use `tokio::task::spawn_blocking` + `std::fs::write` + `select!`.
- **Don't atomically write (write-and-rename).** v1 is plain
  `std::fs::write`; atomic semantics is a future spec.

## Verification

Per CONVENTIONS Rule 7:

```sh
cargo build -p mu-coding
cargo nextest run -p mu-coding
wc -l crates/mu-coding/src/tools/write.rs    # under 400
grep -nE '\bunsafe\b|\.unwrap\(\)|\.expect\(|\bpanic\!|\btodo\!|\bunimplemented\!' \
  crates/mu-coding/src/tools/write.rs \
  | grep -v '^[[:space:]]*//' \
  | grep -v 'cfg(test)'
```

Expect existing 86 + ~5 new tests = 91+ passing workspace-wide.

## Output envelope

Per CONVENTIONS Rule 5. Note in `design_notes` if you find any spec
ambiguity (the §Interfaces sketch is meant to be near-literal; small
deviations OK if you flag them).
