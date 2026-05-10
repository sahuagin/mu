# AGENTS.md — operating context for `mu`

This file is loaded by claude-code (all accounts) and pi-rust at session
start. It captures things future-agents need that aren't reconstructible
from the code or git history.

## What this is

`mu` is a coding agent toolkit. Pre-MVP. Architectural blueprint is
[pi_ts](https://github.com/earendil-works/pi) (`@earendil-works/pi-*`);
[pi_agent_rust](https://github.com/Dicklesworthstone/pi_agent_rust) is
consulted for Rust-specific implementation details only — neither is a
dependency.

The name: μ (Greek small mu, U+03BC) — the response the agent gives
when the question's premise is wrong. Also: µ (micro sign, U+00B5) →
"micro pi" — the lineage joke. Logo is a cow. Don't fight it.

## Architecture in two sentences

One binary `mu` with multiple subcommands; `mu serve` is the JSON-RPC
core daemon, every other subcommand (`tui`, `ask`, `orchestrate`) is a
frontend that owns one or more daemons. The protocol — `mu-core`'s
serde request/response types — is the contract; everything else
conforms.

## Operating rules (project-specific)

These are *additions* to the global rules in `~/.pi/agent/AGENTS.md` —
not replacements. Read that file first; this file extends it.

- **No 27k-line files.** This rule exists because the whole point of
  forking off pi_agent_rust was to avoid its monolithic structure. If a
  module is approaching ~1000 lines, that's the point to split. The
  tags.scm patterns won't accidentally produce a 27k-line file, but
  human-and-agent decisions can.
- **No async-ness leaking through traits unnecessarily.** Use
  `async fn` in trait methods (Rust 1.75+) only when the implementation
  actually does I/O. Pure-compute trait methods stay sync.
- **Errors per crate.** Each library crate uses `thiserror` for its own
  error type. The binary (`mu-coding/src/bin/mu.rs`) is the only place
  `anyhow::Result` appears.
- **No third-party-OAuth-token holding.** `mu` never holds Anthropic or
  OpenAI OAuth tokens directly. The `anthropic-oauth` and
  `openai-oauth` "providers" are subprocess spawns of the legitimate
  CLI clients (`claude --print`, `codex` resp.). This is a ToS
  guardrail; treat it as load-bearing.
- **Reference, don't copy.** When implementing a feature pi_ts has,
  *read* pi_ts for the shape and *consult* pi_agent_rust for Rust
  idioms — but write fresh code. Pasting either invites the structural
  problems of the source.

## Where to look

- `~/src/public_github/pi/packages/` — pi_ts, the blueprint.
  - `agent/` ≈ `mu-core`
  - `ai/` ≈ `mu-ai`
  - `coding-agent/` ≈ `mu-coding`
- `~/src/flywheel/pi_agent_rust/src/` — pi_rs, Rust-syntax cross-check.
- `~/src/agent_tools/code_index` — semantic recall over either tree;
  also the eventual built-in MCP code-search server.
- `~/src/agent_tools/agent` — the memory CLI, also the eventual
  built-in MCP memory server.

## Multi-agent build flow

`mu`'s build itself uses the multi-agent dispatch tools at
`~/src/claude-personal/scripts/`. Mechanical work (scaffolding,
boilerplate trait impls, test writing once shapes are stable) goes to
`agent-router --auth codex-oauth` (OpenAI Pro) or `--auth openrouter`
(misc cheap-tier models). Architectural / cross-cutting work stays with
the claude-code session. The routing policy lives in agent memory
`2da785e5`.
