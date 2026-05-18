# AGENTS.md — operating context for `mu`

This file is loaded by claude-code (all accounts) and pi-rust at session
start. It captures things future-agents need that aren't reconstructible
from the code or git history.

## What this is

`mu` is a coding agent toolkit. Pre-MVP. Architecture is event-sourced
(typed events as substrate, context as projection, capability-bounded
delegation); see `specs/architecture/` and the top-level README for the
shape.

The name: μ (Greek small mu, U+03BC) — the response the agent gives
when the question's premise is wrong. Logo is a cow. Don't fight it.

## Architecture in two sentences

One binary `mu` with multiple subcommands; `mu serve` is the JSON-RPC
core daemon, every other subcommand (`tui`, `ask`, `orchestrate`) is a
frontend that owns one or more daemons. The protocol — `mu-core`'s
serde request/response types — is the contract; everything else
conforms.

## Operating rules (project-specific)

These are *additions* to the global rules in `~/.pi/agent/AGENTS.md` —
not replacements. Read that file first; this file extends it.

- **No 27k-line files.** If a module is approaching ~1000 lines, that's
  the point to split. The tags.scm patterns won't accidentally produce
  a 27k-line file, but human-and-agent decisions can.
- **No async-ness leaking through traits unnecessarily.** Use
  `async fn` in trait methods (Rust 1.75+) only when the implementation
  actually does I/O. Pure-compute trait methods stay sync.
- **Errors per crate.** Each library crate uses `thiserror` for its own
  error type. The binary (`mu-coding/src/bin/mu.rs`) is the only place
  `anyhow::Result` appears.
- **Per-provider OAuth posture.** Each provider's OAuth flow gets
  evaluated on its own merits, not lumped under a blanket rule:
  - **Anthropic** (claude-code OAuth): Anthropic's ToS appears to
    discourage third-party clients reimplementing their flow.
    `anthropic-oauth` provider, if/when added, should subprocess-
    wrap `claude --print`.
  - **OpenAI Codex**: open-source CLI, public flow parameters,
    multiple legit third-party implementations. mu implements
    directly via `oauth2` crate. See mu-018 for the OAuth flow,
    mu-019 (planned) for the API integration. Tokens stored at
    `~/.config/mu/auth/openai-codex.json` with `0600` perms; opt
    out via `--ephemeral` for memory-only.
  - **Other providers**: evaluate per-provider when added.
  Earlier versions of this file lumped these as "no third-party
  OAuth token holding ever." That was overgeneralized; the actual
  concern is Anthropic-specific.
## Where to look

- `~/src/agent_tools/code_index` — semantic recall over the codebase;
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

Delegations run in **isolated jj workspaces** under `.delegations/`.
Use `scripts/delegate.sh <spec-id> <attempt> <auth>` to set one up;
the script handles workspace creation, branch setup, and surfacing
the diff for review. Workspace isolation replaces an earlier prompt-
level "don't touch parallel-session files" rule (see
`specs/delegations/CONVENTIONS.md` for the full rule set).

Per-spec delegation prompts (`specs/mu-NNN-delegation.md`) reference
`CONVENTIONS.md` for universal rules and add only spec-specific
content (deliverable list, what NOT to do that's tied to the spec,
verification commands particular to the work). Earlier prompts
(mu-001 through mu-007) restate the universal rules inline; future
prompts should not.
