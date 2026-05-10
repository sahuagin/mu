# Delegation: implement mu-007 (`read` tool)

This file is the prompt sent to a sub-agent (currently `agent-router
--auth codex-oauth`) to implement spec `mu-007`.

---

## Workspace hygiene (READ FIRST)

Working directory: `/home/tcovert/src/public_github/mu`. A parallel
claude-code session may be editing concurrently. If you see files in
the working copy that are NOT in your deliverable list, **leave them
alone**. Do NOT `jj restore`, `jj abandon`, `git checkout --`, or
otherwise revert files that aren't yours.

This rule has held for three prior delegations (mu-002 → mu-003a →
mu-004a) since being added. Past violation deleted in-flight work via
`jj restore`.

## If the spec and this prompt disagree

Make the call your judgment supports and SURFACE the deviation in your
output envelope's `notes` field. §Invariants are normative; §Interfaces
sketches are illustrative.

## Read first (REQUIRED)

1. `specs/mu-007-read-tool.md` — full specification.
2. `crates/mu-core/src/agent/tool.rs` — the `Tool` trait you're
   implementing. Three items: `Tool::spec`, `Tool::execute`, plus
   the `ToolResult` and `ToolSpec` types.
3. `crates/mu-core/src/agent/loop_tests.rs` — `MockTool` is the
   structural reference. Yours is the public, non-test version.
4. `crates/mu-coding/Cargo.toml` — confirm `mu-core` is already in
   dependencies (it is). No new deps needed.
5. `AGENTS.md` — project-wide rules.

## Deliverable

Three files, two new + one modified:

- **`crates/mu-coding/src/tools/mod.rs`** (new) — module root with
  `pub mod read;` and `pub use read::ReadTool;`.
- **`crates/mu-coding/src/tools/read.rs`** (new) — `ReadTool` impl +
  tests B-1..B-5 (B-6 best-effort cancel test optional).
- **`crates/mu-coding/src/lib.rs`** (modified) — add `pub mod tools;`.
  One line. Don't reorder.

## Verification

```sh
cd /home/tcovert/src/public_github/mu
cargo build -p mu-coding
cargo nextest run -p mu-coding
wc -l crates/mu-coding/src/tools/read.rs    # under 400
grep -nE '\bunsafe\b|\.unwrap\(\)|\.expect\(|\bpanic\!|\btodo\!|\bunimplemented\!' \
  crates/mu-coding/src/tools/*.rs \
  | grep -v '^[[:space:]]*//' \
  | grep -v 'cfg(test)'
# ^ should print nothing outside test modules
```

All checks must pass. Existing 63 tests must remain green; your new
tests bring the total to 68+ depending on how many B-* you implement.

## What NOT to do

- Don't add new dependencies. Use `std::fs`, `std::path`,
  `serde_json::Value`, plus what's already in mu-coding's
  `Cargo.toml` (tokio, serde, serde_json, etc.).
- Don't add `tempfile` or any other test-helper crate. Use
  `std::env::temp_dir()` + `std::fs` for temp files.
- Don't return `Err(_)` from `execute`. The trait returns
  `ToolResult` directly; errors are expressed via `is_error: true`.
  See §INV-2 of the spec.
- Don't replace `tokio::task::spawn_blocking + select!` with
  `tokio::fs::read`. The explicit pattern is load-bearing for cancel
  support — see §OOC-3.
- Don't wire `ReadTool` into mu-coding's `dispatch::handle_create_session`.
  That's a future spec (gated on Provider tool support landing).

## Output protocol

```json
{
  "status": "complete",
  "files_changed": ["crates/mu-coding/src/tools/mod.rs",
                    "crates/mu-coding/src/tools/read.rs",
                    "crates/mu-coding/src/lib.rs"],
  "tests_added": ["tests::<list>"],
  "verification_results": {
    "build_p_mu_coding": "clean",
    "tests_passed": <N>,
    "module_lines": <N>,
    "no_new_deps": true,
    "grep_unsafe_unwrap_outside_tests": "empty"
  },
  "design_notes": "<any judgment calls>",
  "notes": "<flag any spec/prompt disagreements; flag if you saw working-copy files you respected>"
}
```

If you hit a blocker, use `status: "blocked"` and explain in `notes`.
