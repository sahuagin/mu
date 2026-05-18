# mu

`mu` is a local-first coding-agent runtime written in Rust.

It is a single binary with multiple frontends: `mu serve` runs the JSON-RPC daemon, `mu ask` runs one-shot prompts, and `mu tui` is the interactive terminal UI. They all attach to the same runtime contract.

`mu` is the answer the agent gives when the question's premise is wrong.

## Why this architecture pays off

The architectural thesis (events as substrate, context as projection, capability-bounded delegation) is below in [Architectural bet](#architectural-bet). This section is the concrete cash-out: one place where the architecture has already produced a measurable win you can reproduce.

### Compaction: structural beats LLM-summary

**Claim.** mu represents context as a typed event log + retained rope of spans, not as a transcript blob. That makes compaction a *structural transform* over typed spans instead of a model-mediated summarization. For the apples-to-apples case where both approaches spend an LLM call on semantic summarization, mu's compaction is approximately **700× faster and ~40× cheaper** than Anthropic's beta `compact_20260112` pathway in our measurement. For workloads that can accept structural-only compaction (no semantic summary), mu's heuristic tier runs at microsecond scale.

**Mechanism.** `crates/mu-core/src/context/compaction/` defines a policy ladder over the same `RetainedRope` interface:

| Policy | What it does | Cost surface |
|---|---|---|
| `SpanFamilyDropPolicy` (heuristic) | Structural: drop low-value span families (verbose tool results, superseded states) by event-type rules | CPU only — no LLM call |
| `HashAndSummaryPolicy` (mock judge) | Hash-and-replace with deterministic stub summaries | CPU only |
| `HashAndSummaryPolicy` (live judge) | Same shape, but a configurable provider (e.g. Haiku) generates the summary spans — this is the apples-to-apples comparison with Anthropic compaction | One small LLM call per compaction |

Each tier is selected by config (`[context.compaction.policy]`), not by a code-path rewrite.

**Measurement.** Against a real 124,091-token mu session corpus, 5 runs each, median wall-clock:

| | Wall-clock | Cost per event | Reduction |
|---|---|---|---|
| Anthropic Opus 4.7 auto-compaction (beta) | 38.18 s | $2.03 | 124k → 2.3k tokens (~98%) |
| mu `HashAndSummaryPolicy` (live Haiku judge, estimated) | ~50 ms | ~$0.05 | mid-tier semantic |
| mu `SpanFamilyDropPolicy` (heuristic, structural-only) | 19 µs | $0.00 | structural drop, no semantic summary |

The 2M× headline ratio between Anthropic and the heuristic is not a fair single-axis comparison — the heuristic preserves structure, not semantics, and its measurement does not include a real tokenization pass. The ~700× / ~40× numbers for the live-judge case are the more defensible "same trade-off, much cheaper" claim.

**Reproduce locally:**

```sh
cargo run --example compaction-bench -p mu-ai
```

**Methodology (5 runs, real corpus, isolated wall-clock):** [`specs/measurements/compaction-2026-05-14.md`](specs/measurements/compaction-2026-05-14.md).

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

Launch the TUI against a running daemon:

```sh
cargo run -p mu-tui -- --mu-binary "$(which mu)" --provider anthropic_api --model claude-haiku-4-5
# or, after `cargo install`:
mu tui
```

## What works today

### Runtime

- **Stdio JSON-RPC daemon** (`mu serve`) — the architectural contract. Frontends, dashboards, and tools talk to this; they do not reach into implementation internals.
- **One-shot CLI** (`mu ask`) — single-prompt invocations with full tool access, useful for both interactive use and `agent-spawn`-style automation.
- **Provider abstraction** — Anthropic Messages API, OpenRouter, OpenAI Codex (OAuth + direct backend), and a faux provider for tests. Provider selection is per-session.
- **Real tool execution** — `read`, `write`, `ls`, `edit`, `grep`, `glob`, and `bash` (with strict allowlist, per-call approval mode, and yolo mode).
- **Capability-oriented policy** — every tool call is gated by an explicit policy. The runtime only dispatches what the session is authorized to use. Delegation attenuates authority; it never widens.
- **Approval primitive** — `session.input_required` suspends work until an attached UI, parent session, orchestrator, or policy answers. The same primitive backs the bash-prompt modal, capability prompts, and orchestrator-mediated approvals.
- **OAuth login** for providers that need it (currently `openai-codex` via the standard PKCE flow); tokens persist under `~/.config/mu/auth/` mode 0600.

### Terminal UI (`mu tui`)

`mu-tui` is a fully implemented ratatui-based interactive client (~4,500 LOC in [`crates/mu-tui/`](crates/mu-tui/)). It is the most-developed frontend, not aspirational scaffolding. `mu tui` execs the `mu-tui` binary directly (mu-yvvz); arguments after `tui` pass through.

What's wired up today:

- F1–F9 dashboards: command center, session tree, transcript, context inspector, raw firehose, provider stats, approvals queue, etc.
- Streaming response rendering with throbbers per provider call.
- Modal approval flows for bash-prompt and capability gates.
- `$EDITOR` handoff for long-form prompt editing (KKP push/pop, alternate-screen save/restore, post-edit reflow).
- Tool-call inspector with arguments, results, and timing.
- Per-session cost and token-usage panel (mu-fqvc pricing table).

### Observability

- **Typed session event log** — `~/.local/share/mu/events/<session-id>/` is the durable substrate. Sessions, tool calls, model calls, context assemblies, approvals, usage, and delegations are all events.
- **`ContextAssembly` records** — for each model call, the runtime is moving toward an answer to "what did the model know, and why?" — included spans, omitted spans, cacheable spans, and what the provider actually received.
- **TaskTelemetry + analytics** — `mu analytics` projects the event log into a queryable SQLite at `~/.local/share/mu/analytics.sqlite`. Preset queries: per-session cost, per-policy compaction stats, outcome classification (clean / dirty / hollow / lying / narrative).
- **Live firehose** in the TUI (F4): every protocol frame in/out of the daemon, scrollable, with anchor on streaming.

### What's still pre-MVP

- **`mu orchestrate`** — multi-daemon coordination is sketched in specs but not implemented; the subcommand bails with a pointer to `mu tui`.
- **Web/external dashboards** — the JSON-RPC contract supports them, but nothing's been built yet.
- **Persistent session re-hydration across daemon restarts** — events are durable, but the rehydration path is in progress ([mu-u1ld](https://github.com/sahuagin/mu/issues?q=mu-u1ld)).

## Architectural bet

Most agent tools look like chat wrapped around tool calls. `mu` is built around a different center:

> agent work should be inspectable, replayable, accountable, and capability-bounded.

The runtime is event-sourced: sessions, model calls, tool calls, context assemblies, usage records, approvals, delegations, and observability facts are typed events. Transcripts, dashboards, session trees, context inspectors, and accounting reports are projections over that event log.

The goal is not another opaque coding chatbot. The goal is a toolkit where you can see what the agent knew, which tools it could use, what it did, what it cost, why it paused for approval, and how a delegated sub-session inherited authority.

### Workspace shape

```text
crates/
  mu-core/     protocol types, agent loop, event log, transport, tool/provider traits, compaction
  mu-ai/       LLM provider implementations and provider translation layers
  mu-coding/   the `mu` binary: CLI modes, serve frontend, tools, sessions, config, analytics
  mu-tui/      the `mu-tui` binary: ratatui interactive client (4,500+ LOC)
```

The architectural contract is the protocol and event model, not a particular UI. The same daemon supports a terminal UI, one-shot CLI calls, future orchestrated multi-agent runs, and external observability tooling — all over the same JSON-RPC interface.

### Design principles

**Context is a projection, not a blob.** The transcript is not the session. The prompt is not the context. The memory is not the store. All three are projections over the event log. A model call should eventually be explainable by a `ContextAssembly` record: which spans were included, where they came from, why, what was omitted, what was cacheable, and what the provider actually received.

**Tools are capabilities.** A model can ask for any tool name it wants. The runtime only dispatches tools the session is authorized to use. Delegation should attenuate authority, never widen it.

**Streaming beats whole-state replacement.** When a consumer might want deltas or whole state, the primitive should be deltas. A UI can buffer deltas into a whole view; it cannot recover deltas from an opaque replacement.

**Observability is a product feature.** Usage, cache behavior, tool calls, router/proxy state, approvals, session lineage, and context assembly should be visible. The agent runtime should not require users to guess what happened.

### Architecture deep-dives

- [`specs/architecture/mu-capability-substrate.md`](specs/architecture/mu-capability-substrate.md)
- [`specs/architecture/event-sourced-context.md`](specs/architecture/event-sourced-context.md)
- [`specs/architecture/capability-delegation.md`](specs/architecture/capability-delegation.md)
- [`specs/architecture/os-enforced-agent-sandboxing.md`](specs/architecture/os-enforced-agent-sandboxing.md)
- [`specs/measurements/compaction-2026-05-14.md`](specs/measurements/compaction-2026-05-14.md)
- [`specs/delegations.md`](specs/delegations.md)

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

Project specs live under [`specs/`](specs/). Most features are developed from small specs, with mechanical work sometimes delegated to sub-agents in isolated jj workspaces under `.delegations/`.

## Contributing

`mu` is early. Issues, design notes, and patches are welcome, but the project is still discovering its core shape.

Before contributing, read:

- [`LICENSE`](LICENSE)
- [`LICENSING.md`](LICENSING.md)
- [`AGENTS.md`](AGENTS.md)
- the relevant spec under [`specs/`](specs/)

If you are building commercial products, hosted agent services, proprietary agent runtimes, model-training pipelines, or agent-evaluation systems from `mu`, read the license carefully. Commercial production use requires a separate commercial license until the delayed-open change date.

## License

`mu` is source-available under a delayed-open license: Business Source License 1.1 with an Additional Use Grant for personal, educational, research, noncommercial, internal-evaluation, and open-source-project use. This version converts to BSD-3-Clause on the change date.

See [`LICENSE`](LICENSE) for the controlling terms and [`LICENSING.md`](LICENSING.md) for the project licensing philosophy.
