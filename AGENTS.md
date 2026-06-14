# AGENTS.md — mu

The canonical agent/cc context for this repository: what mu is, how it's laid
out, how to build and test it, the architecture invariants you must not break,
and how work flows here. Universal operating conduct (VCS discipline, work
claiming, tone) comes from the operator's environment-level config; this file is
mu-specific.

## What mu is

A coding agent and standalone agent runtime, built as a Rust workspace. The core
model: **`mu serve` is the JSON-RPC core daemon; everything else is a frontend**
to it — the TUIs, the one-shot `ask`, the web console. Sessions are
event-sourced (see the architecture invariants below).

## Workspace layout

Nine crates (`Cargo.toml` `[workspace]`):

| crate | role |
|---|---|
| `mu-core` | agent loop, JSON-RPC protocol, transport, session state |
| `mu-ai` | LLM provider abstraction |
| `providers/mu-anthropic` | Anthropic Messages API wire protocol as typed Rust (standalone; no `mu-core` dep) |
| `providers/mu-anthropic-py` | thin pyo3 binding over `mu-anthropic` |
| `mu-coding` | the coding agent; **owns the `mu` binary** (`src/bin/mu.rs`) |
| `mu-tui` | terminal UI for `mu serve` |
| `mu-solo` | standalone single-pane chat TUI for `mu serve` |
| `mu-bridge` | Claude-code JSONL → mu event format (pyo3) |
| `t4c` | tools4claude — capability/tool discovery |

The `mu` CLI subcommands: `serve` (daemon), `ask` (one-shot), `resume`, `tui`,
`orchestrate`, `console`, `login`/`logout`, `mark`, `list-sessions`,
`analytics`, `capabilities`, `audit`, `versions`.

## Build & test

- Toolchain: **stable** with `rustfmt` + `clippy` (`rust-toolchain.toml`).
- **`just ci` is the gate** — `fmt-check` → `clippy` → `test`, fail-fast in that
  order; it mirrors `.github/workflows/ci.yml` verbatim. A green `just ci` is the
  local proxy for green CI. Run it before pushing. The three steps are:
  - `cargo fmt --all -- --check` — **check-only, never rewrites files** (use
    `just fmt` to actually format)
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  - `cargo test --workspace --all-features --no-fail-fast` — plain `cargo test`,
    not nextest
- `just check` runs the superset `scripts/pre-pr-check.sh` (the `just ci` checks
  plus `verify-claims`). `scripts/gh-wrapper` runs it automatically at
  `gh pr create` / `gh pr ready`, so the gate holds even if you skip the manual
  run.
- `just check-quick` — fmt + clippy only (fast inner loop).
- `just ci-aipr` — local-only cross-provider AI review of the diff
  (`scripts/ai-review.sh`); not a CI step.

## Running it

- `just smoke` — faux-provider `mu ask` (no API key needed); the fastest
  end-to-end smoke test.
- `just ask "…"`, `just serve …`, `just tui …`, `just solo …` — pass-throughs.
- Direct: `cargo run -p mu-coding --bin mu -- <subcommand>`.

## Architecture invariants — do not break these

1. **The on-disk event log is the source of truth.** Events persist to JSONL at
   `<state_dir>/events/<daemon_id>/<session_id>.jsonl` (`state_dir` defaults to
   `~/.local/share/mu`). In-memory session state is a *projection* rebuildable
   from the log: write to disk first, then map into memory.
2. **Durability is two-tier (spec mu-046).** The command journal is the
   fail-closed write-ahead path — an inbound command is journaled before
   processing, and a failed append *rejects* the command
   (`JOURNAL_UNAVAILABLE = -32003`). Session-log gateway events (tool results,
   assistant messages) are best-effort disk-before-memory appends: IO errors are
   logged and ignored, not fatal.
3. **Rehydration is lazy and request-driven**
   (`mu-lazy-session-rehydration-bh4f`). `mu serve` parses nothing on cold start;
   a past session is loaded by id the first time it's addressed. Enumeration is
   the offline `mu list-sessions` (reads each log's first record + mtime only).
4. **Deep design lives in `specs/`** — the `architecture/` subdir, the numbered
   `mu-NNN` specs, and `specs/plans/`. Read it for the *why*; put new design docs
   there, **not** in crate roots.

## How work flows here

- **VCS is `jj`** over a colocated git+jj repo. **`main` is protected and is
  production** → branch + PR for everything. Local commits are ungated; push / PR
  is the reviewed, ask-first step.
- **Force-push and direct push to `main` are disabled for agents —
  intentionally and permanently.** The branch ruleset requires every change to go
  through a PR and its bypass list is empty. Do **not** try to force-push or
  re-grant a bypass to "work around" a rejected push; that guardrail is
  deliberate. The forward path is: a normal forward commit → PR → a human admin
  merges.
- **Work is tracked in beads (`br`).** Canonical DB: `.beads/beads.db`;
  `.beads/issues.jsonl` is the exported mirror, reconciled onto `main` by
  `just beads-sync` (run from the backing repo after a merge wave). Claim a bead
  before editing its code. The `mu-NNN` / `mu-<slug>` ids that pepper code
  comments and spec filenames are the durable link from a line back to its
  rationale.
- **Code search:** prefer semantic code-index recall (`code_recall`) for
  orientation and concept-location when it's configured; fall back to `rg` for
  literal / regex matches.
