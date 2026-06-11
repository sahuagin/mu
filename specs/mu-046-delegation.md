# Delegation: mu-046 — ingest pipeline work packages

Common worker contract for beads `mu-ingest-pipeline-umbrella-n3yy.1–.7`.
The orchestrator (claude, interactive session) dispatches one worker per WP
as a native subagent **in the orchestrator's own workspace** — sequential
landing, one mutating worker at a time (per tcovert 2026-06-10: agent-router /
delegate.sh assumed unavailable; ollama down for repair).

Routing: native Claude Code subagents (flat-rate). Review: orchestrator diff
review against the mu-046 invariants + `just ci-aipr` with review models
pointed at openrouter/openai while ollama is down.

---

## Worker rules (every WP)

1. **Read the spec first**: `specs/mu-046-ingest-pipeline.md`. Your WP's scope
   is the matching row in its work-package table. Stay inside it.
2. **The invariants are the acceptance criteria.** If your implementation
   can't satisfy an INV that touches your scope, STOP and report — do not
   reinterpret the invariant.
3. Scope discipline: do not modify specs, AGENTS.md, CLAUDE.md, or crates
   outside your WP's file list unless genuinely forced — and surface any
   deviation explicitly in your output envelope.
4. Verification before exit, in order:
   - `cargo fmt --all -- --check`
   - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
   - `cargo test --workspace --all-features --no-fail-fast`
   (this is `just ci`; run that if you prefer.)
5. Commit message MUST end with a `## Files` block
   (`<STATUS> <PATH> +<added> -<deleted>` per line, per
   `scripts/verify-claims.sh`). Get counts from
   `git diff-tree -r --numstat --no-commit-id <commit>` /
   `jj diff --stat`.
6. Output envelope at exit: commit short id, verification results verbatim,
   notes on any deviation or surprise (codebase reality vs spec assumption —
   map-vs-terrain findings are wanted, not punished).

## Per-WP notes

- **WP1 (.1)** — pure mu-core; no serve wiring. The strict `append_command`
  must NOT change existing `append()` behavior (best-effort stays for
  gateway events).
- **WP2 (.2)** — transport only; dispatch untouched. Prove per-connection
  filtering + broadcast fan-out with transport-level tests.
- **WP3 (.3)** — the big one: `serve/pipeline.rs`, route-by-session_id,
  control-plane consumer, receipts, stdio adapter conversion. The crash test,
  fail-closed test, and seq==order property test live here.
- **WP4 (.4)** — session-scoped command journaling + completion receipts.
  Wedged-loop test: command durable even when the session input channel is
  full/closed.
- **WP5 (.5)** — MCP adapter. `serve/mcp.rs::dispatch_tool` stops calling
  handlers; everything becomes `Command`s through ingest/route.
- **WP6 (.6)** — boot ordering: `JournalOpened` → `ConfigLoaded` → adapters
  accept traffic. Redaction proven against raw journal bytes.
- **WP7 (.7)** — docs only. The forwarder "Durable projection" comment and
  the CLAUDE.md write-ahead paragraph are load-bearing corrections.
