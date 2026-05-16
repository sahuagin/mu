# mu

`mu` is a local-first coding-agent runtime written in Rust.

It is a single binary with multiple frontends: `mu serve` runs the JSON-RPC daemon, `mu ask` runs one-shot prompts, and future frontends
such as `mu tui` and `mu orchestrate` attach to the same runtime contract.

`mu` is the answer the agent gives when the question's premise is wrong.

## Why mu exists

Most agent tools look like chat wrapped around tool calls. `mu` is built around a different center:

> agent work should be inspectable, replayable, accountable, and capability-bounded.

The runtime is moving toward an event-sourced model where sessions, model calls, tool calls, context assemblies, usage records, approvals, delegations, and
observability facts are all typed events. Transcripts, dashboards, session trees, context inspectors, and accounting reports are projections over that event log.

The goal is not another opaque coding chatbot. The goal is a toolkit where you can see what the agent knew, which tools it could use, what it did, what it cost,
why it paused for approval, and how a delegated sub-session inherited authority.

## Highlights

- **One binary, multiple modes**: `serve`, `ask`, `tui`, `orchestrate`, `login`, `logout`, and `versions`.
- **JSON-RPC daemon core**: frontends talk to the daemon over a stable protocol instead of reaching into implementation internals.
- **Provider abstraction**: faux provider for tests, Anthropic API, OpenRouter, and OpenAI Codex OAuth/direct-backend work.
- **Real tool execution**: file and shell tools including `read`, `write`, `ls`, `edit`, `grep`, `glob`, and `bash`.
- **Capability-oriented tool policy**: tools are explicit capabilities; policy gates decide what a session can invoke and when approval is required.
- **Human-or-policy approval primitive**: `session.input_required` suspends work until an attached UI, parent session, orchestrator, or policy answers.
- **Session event log**: session state is increasingly represented as typed events with cumulative views and stats projections.
- **Context provenance**: `ContextAssembly` records are the beginning of an inspectable "what did the model know?" surface.
- **Delegation-aware runtime**: sub-sessions are becoming first-class instead of ad-hoc subprocess prompts.
- **Local-first by default**: state is backed by SQLite and the daemon runs locally.

## Quick start

`mu` is pre-release and not published to crates.io yet. Build from source:

```sh
git clone https://github.com/sahuagin/mu
cd mu
cargo build --workspace
cargo run -p mu-coding --bin mu -- versions
```

Run a smoke test with the faux provider, no API key required:

```sh
cargo run -p mu-coding --bin mu -- ask "hello"
# hello
```

Run against Anthropic with tools:

```sh
export ANTHROPIC_API_KEY=...
cargo run -p mu-coding --bin mu -- ask \
  --provider anthropic-api \
  --model claude-haiku-4-5 \
  --tools read \
  "Use the read tool to read /etc/hostname. Just the hostname, nothing else."
```

Enable multiple local tools:

```sh
cargo run -p mu-coding --bin mu -- ask \
  --provider anthropic-api \
  --tools read,ls,grep,glob \
  "Find the Rust crates in this repository and summarize them."
```

Use the bash tool in strict mode with per-call approval:

```sh
cargo run -p mu-coding --bin mu -- ask \
  --provider anthropic-api \
  --tools bash \
  --bash-prompt \
  "Run cargo check and tell me the result."
```

`--bash-yolo` exists for trusted sessions, but it deliberately bypasses the strict allowlist and approval path. Treat it like handing the prompt a shell.

## Current status

`mu` is young and moving quickly. The current implementation already has:

- a stdio JSON-RPC daemon;
- one-shot `mu ask`;
- per-session provider selection;
- Anthropic API, OpenRouter, and OpenAI Codex provider paths;
- OAuth login/logout support for providers that need it;
- read/write/list/edit/search/bash tools;
- tool policy, retry policy, session stats, approval suspension, and session delegation primitives;
- early event-log and context-assembly observability.

The TUI and orchestrator commands are part of the intended surface but are not yet the primary daily-driver interface.

## Architecture

The workspace is split into three crates:

```text
crates/
  mu-core/     protocol types, agent loop, event log, transport, tool/provider traits
  mu-ai/       LLM provider implementations and provider translation layers
  mu-coding/   the binary: CLI modes, serve frontend, tools, sessions, config
```

The architectural contract is the protocol and event model, not a particular UI. The same daemon should be able to support:

- a terminal UI;
- a web dashboard;
- one-shot CLI calls;
- orchestrated multi-agent runs;
- external observability and replay tooling.

See also:

- [`specs/architecture/mu-capability-substrate.md`](specs/architecture/mu-capability-substrate.md)
- [`specs/architecture/event-sourced-context.md`](specs/architecture/event-sourced-context.md)
- [`specs/architecture/capability-delegation.md`](specs/architecture/capability-delegation.md)
- [`specs/architecture/os-enforced-agent-sandboxing.md`](specs/architecture/os-enforced-agent-sandboxing.md)
- [`specs/delegations.md`](specs/delegations.md)

## Design principles

### Context is a projection, not a blob

The transcript is not the session. The prompt is not the context. The memory is not the store. All three are projections over the event log.

A model call should eventually be explainable by a `ContextAssembly` record: which spans were included, where they came from, why they were included, what was
omitted, what was cacheable, and what the provider actually received.

### Tools are capabilities

A model can ask for any tool name it wants. The runtime only dispatches tools that the session is authorized to use. Delegation should attenuate authority, never
widen it.

### Streaming beats whole-state replacement

When a consumer might want deltas or whole state, the primitive should be deltas. A UI can buffer deltas into a whole view; it cannot recover deltas from an opaque
replacement.

### Observability is a product feature

Usage, cache behavior, tool calls, router/proxy state, approvals, session lineage, and context assembly should be visible. The agent runtime should not require
users to guess what happened.

### Reference, do not fork

[`pi_ts`](https://github.com/earendil-works/pi) is the architectural blueprint. `pi_agent_rust` is a Rust implementation reference. `mu` reads both for shape and
lessons, but the code here is written fresh.

## Development

A top-level `justfile` collects the common workflows. `just --list` for the menu; recipes wrap the underlying `cargo` and `scripts/` invocations.

```sh
just check         # full pre-PR check (fmt + clippy + test)
just smoke         # mu ask against the faux provider — no API key needed
just pr <branch>   # push current jj @ as <branch> and open a PR
```

Or the underlying commands directly:

```sh
cargo build --workspace
cargo check --workspace
cargo nextest run
cargo run -p mu-coding --bin mu -- versions
```

Live provider tests are gated so CI and routine local runs do not spend API credits. Enable them explicitly with the relevant environment variables, for example:

```sh
MU_LIVE_ANTHROPIC=1 cargo nextest run -p mu-ai
```

Project specs live under [`specs/`](specs/). Most features are developed from small specs, with mechanical work sometimes delegated to sub-agents in isolated jj
workspaces under `.delegations/`.

## Contributing

`mu` is early. Issues, design notes, and patches are welcome, but the project is still discovering its core shape.

Before contributing, read:

- [`LICENSE`](LICENSE)
- [`LICENSING.md`](LICENSING.md)
- [`AGENTS.md`](AGENTS.md)
- the relevant spec under [`specs/`](specs/)

If you are building commercial products, hosted agent services, proprietary agent runtimes, model-training pipelines, or agent-evaluation systems from `mu`, read
the license carefully. Commercial production use requires a separate commercial license until the delayed-open change date.

## License

`mu` is source-available under a delayed-open license: Business Source License 1.1 with an Additional Use Grant for personal, educational, research,
noncommercial, internal-evaluation, and open-source-project use. This version converts to BSD-3-Clause on the change date.

See [`LICENSE`](LICENSE) for the controlling terms and [`LICENSING.md`](LICENSING.md) for the project licensing philosophy.
