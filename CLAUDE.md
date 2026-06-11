# CLAUDE.md — mu

## Architecture: event persistence

Events are persisted to JSONL on disk FIRST, then mapped into memory. The
on-disk log at `~/.local/share/mu/events/<daemon_id>/<session_id>.jsonl` is
the source of truth — the in-memory `Vec<SessionEvent>` is a projection that
can be rebuilt from the log. Two durability tiers (spec mu-046): the command
journal is the fail-closed write-ahead path — every inbound command is fsync'd
to its pipeline's journal before processing (daemon-scoped commands to
`~/.local/share/mu/journal/<daemon_id>.jsonl`, session-scoped commands to the
session's own log via `append_command`); an append failure rejects the command
(`JOURNAL_UNAVAILABLE`). Session-log gateway events (tool results, assistant
messages) remain best-effort disk-before-memory appends WITHOUT fsync — IO
errors are logged and ignored.

Rehydration (rebuilding session state from a persisted log) is request-driven,
not a startup pass (mu-lazy-session-rehydration-bh4f): `mu serve` parses nothing
on cold start. A past session is loaded lazily, by id, the first time it's
addressed (`resume`/`recover`/`session.events`/`session.stats` via
`Sessions::event_log`); enumeration is the offline `mu list-sessions` (reads
each log's first record + mtime only — see `sessions_index`). Live session
resumption (re-attaching a frontend to a rehydrated session and continuing the
agent loop) is not yet implemented.

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
