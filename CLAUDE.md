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
