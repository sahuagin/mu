# CLAUDE.md — mu

## Architecture: event persistence

Events are persisted to JSONL on disk FIRST, then mapped into memory. The
on-disk log at `~/.local/share/mu/events/<daemon_id>/<session_id>.jsonl` is
the source of truth — the in-memory `Vec<SessionEvent>` is a projection that
can be rebuilt from the log. This is write-ahead, not write-back: no event
exists in memory that isn't already durable on disk.

Rehydration (rebuilding session state from persisted logs on daemon restart)
is implemented. Live session resumption (re-attaching a frontend to a
rehydrated session and continuing the agent loop) is not yet implemented.

## Code search

For all code searches and symbol lookups, ALWAYS use `code_recall` (code-index
MCP) instead of grep/ripgrep. It uses hybrid semantic + lexical retrieval and
returns ranked source code chunks with file paths and line numbers. Only fall
back to grep if code_recall returns no results or you need a literal regex
pattern match.

## Local CI gate

`just ci` mirrors `.github/workflows/ci.yml` verbatim — `cargo fmt --all -- --check`,
then `cargo clippy --workspace --all-targets --all-features -- -D warnings`, then
`cargo test --workspace --all-features --no-fail-fast`, fail-fast in that order. A
green `just ci` is the local proxy for a green CI; run it before pushing. The fmt
step is CHECK-ONLY and never edits files. `scripts/gh-wrapper` also runs the
superset `pre-pr-check.sh` (the `just ci` checks plus verify-claims) at
`gh pr create` / `gh pr ready`, so the gate holds even if you skip the manual run.
bead: mu-608b
