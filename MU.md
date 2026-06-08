# MU.md

mu's own project-context file. mu reads it at session start via the
project-file recall provider (`crates/mu-core/src/context/recall/project_files.rs`):
`./MU.md`, `./AGENTS.md`, then `<config-dir>/mu/{MU,AGENTS}.md`. This is
deliberately separate from `CLAUDE.md` (which serves claude-code / pi) — mu
reads *this* file, and it evolves on its own. Keep it small and legible: it is
the startup context, so everything here is something a mu session is meant to
carry.

## Architecture: event persistence (write-ahead)

Events are persisted to JSONL on disk FIRST, then mapped into memory. The
on-disk log at `~/.local/share/mu/events/<daemon_id>/<session_id>.jsonl` is the
source of truth; the in-memory `Vec<SessionEvent>` is a projection that can be
rebuilt from the log. Write-ahead, not write-back: no event exists in memory
that isn't already durable on disk.

## Code search

Use `code_recall` (the code-index MCP) for symbol/concept/pattern lookups
instead of grep/ripgrep — hybrid semantic + lexical retrieval returning ranked
chunks with file paths and line numbers. Fall back to grep only for a literal
regex match or when `code_recall` returns nothing.

## Local CI gate

`just ci` mirrors `.github/workflows/ci.yml` verbatim: `cargo fmt --all -- --check`,
then `cargo clippy --workspace --all-targets --all-features -- -D warnings`, then
`cargo test --workspace --all-features --no-fail-fast` — fail-fast in that order.
A green `just ci` is the local proxy for green CI; run it before pushing. The fmt
step is check-only and never edits files. `scripts/gh-wrapper` runs the superset
`pre-pr-check.sh` (the `just ci` checks plus verify-claims) at `gh pr create` /
`gh pr ready`, so the gate holds even if you skip the manual run.
