# mu

`mu` is an agent runtime built around one commitment: **every fact about a
session is a typed event in a durable log, and everything you look at is a
projection of that log.**

A session is not a chat transcript. It is an append-only JSONL event log —
model calls, tool calls, the exact context assembled for each call (span by
span, with token counts), what compaction kept and ejected, approvals, token
accounting, delegations. The transcript is one projection of that log. The
status bar is another. The analytics database is another. When something
goes wrong — or surprisingly right — the log answers *what did the model
see, what did it do, what did it cost, and why* after the fact, without
having had the foresight to enable debugging first.

One workspace, several entry points to the same runtime contract:

- **`mu serve`** — the JSON-RPC daemon. Everything else is a client of it.
- **`mu-solo`** — standalone single-pane TUI; spawns its own daemon. The
  daily driver.
- **`mu ask`** — one-shot CLI prompts with full tool access; scriptable.
- **`mu tui`** — multi-dashboard client (session tree, firehose, context
  inspector) for watching a daemon from the outside.

`mu` is the answer the agent gives when the question's premise is wrong.

## Status: self-hosting

As of 2026-06-03, mu's own development happens inside `mu-solo` sessions:
agents read mu's code through mu's MCP-imported code search, edit it with
mu's tools, gate shell access through mu's approval flow, and the sessions
that did the work land in mu's event log, where they get audited for
regressions and behavior. The architecture sections below are not
aspiration — they are the machinery those sessions run on, and the event
logs they leave behind are how problems in mu get found (several of the
open issues cite the exact session and event that exposed them).

## Why event-sourcing pays: compaction

The architectural thesis (events as substrate, context as projection,
capability-bounded delegation) is below in
[Architectural bet](#architectural-bet). This section is the concrete
cash-out: a measurable win you can reproduce.

**Claim.** mu represents context as a typed event log + retained rope of
spans, not as a transcript blob. That makes compaction a *structural
transform* over typed spans instead of a model-mediated summarization. On a
real-workload corpus, mu's structural-drop policy reduces context by **97%**
in **~62 ms** with **zero LLM cost** — within a point of Anthropic Opus's
98% reduction but ~600× faster and free. The judge-backed policy
(Haiku-summary) is ~7× faster and ~17× cheaper than Opus auto-compaction at
the same corpus size.

**Mechanism.** `crates/mu-core/src/context/compaction/` defines a policy
ladder over the same `RetainedRope` interface:

| Policy | What it does | Cost surface |
|---|---|---|
| `SpanFamilyDropPolicy` (heuristic) | Structural: drop low-value span families (verbose tool results, superseded states) by event-type rules, keeping tool-call/result/assistant clusters intact | CPU only — no LLM call |
| `HashAndSummaryPolicy` (mock judge) | Hash-and-replace with deterministic stub summaries | CPU only |
| `HashAndSummaryPolicy` (live judge) | Same shape, but a configurable judge model (e.g. Haiku) generates the summary spans — the apples-to-apples comparison with Anthropic auto-compaction | One small LLM call per compaction |

Live sessions select the policy via config:

```toml
[compaction]
default_policy = "heuristic"        # or "no-compaction"
trigger_threshold_tokens = 150000
```

Honest caveat: `heuristic` is the only tier live on the serve path today.
Requesting `hash-and-summary` warns and falls back to heuristic — the
judge-backed policy is implemented and benchmarked but not yet wired into
live session creation ([mu-8bkf]). When a compaction runs, the event log
records the full per-span decision audit (kept / dropped+reason /
summarized) as a durable `CompactionAssembly` event.

**Measurement.** Two measurement points were run against real mu session
corpora:

*Anthropic Opus baseline* — 5 runs against a 124,091-token session through
Anthropic's beta `compact_20260112` API, median wall-clock and direct
token-cost calculation.
[`specs/measurements/compaction-2026-05-14.md`](specs/measurements/compaction-2026-05-14.md).

*mu policy ladder* — 5 sessions ranging 91k–235k tokens (727k total) run
through each policy in `compaction-bench --judge live`, median wall-clock
from the in-process timer, Haiku cost calculated from measured per-call
token usage.
[`specs/measurements/compaction-2026-05-21.md`](specs/measurements/compaction-2026-05-21.md).

| | Wall-clock (median) | Cost per event | Reduction |
|---|---|---|---|
| Anthropic Opus 4.7 auto-compaction (beta) | 38.18 s | $2.03 | 124k → 2.3k tokens (~98%) |
| mu `HashAndSummaryPolicy` (live Haiku judge) | 6.0 s | ~$0.16 | 727k → 235k tokens (~67%) |
| mu `SpanFamilyDropPolicy` (heuristic, structural-only) | 62 ms | $0.00 | 727k → 16k tokens (~97%) |

Against the closest single-session corpus match to the Opus baseline (a
122,478-token mu session), the live-Haiku policy ran in **5.12 s** at
**~$0.12** — a **~7.5× speed and ~17× cost win** over Opus on the same
corpus size. The structural-drop policy ran in **62 ms** at **$0.00** — a
**~616× speed win** while landing within a percentage point of Opus's
reduction ratio.

**Reproduce locally:**

```sh
cargo run --release --example compaction-bench -p mu-ai -- \
  --judge live --max-sessions 20 --format json
```

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

`--bash-yolo` exists for trusted sessions, but it deliberately bypasses the
strict allowlist and approval path. Treat it like handing the prompt a shell.

Launch the daily-driver TUI (spawns and manages its own daemon):

```sh
cargo run -p mu-solo --bin mu-solo
```

Or the dashboard client against a running daemon:

```sh
cargo run -p mu-tui -- --mu-binary "$(which mu)" --provider anthropic_api --model claude-haiku-4-5
# or, after `cargo install`:
mu tui
```

## What works today

### Runtime

- **Stdio JSON-RPC daemon** (`mu serve`) — the architectural contract.
  Frontends, dashboards, and tools talk to this; they do not reach into
  implementation internals.
- **One-shot CLI** (`mu ask`) — single-prompt invocations with full tool
  access, useful for both interactive use and `agent-spawn`-style
  automation.
- **Provider abstraction** — Anthropic Messages API, OpenRouter, OpenAI
  Codex (OAuth + direct backend), and a faux provider for tests. Provider
  selection is per-session; each provider self-declares its renderer, cache
  strategy, and compaction hooks rather than being special-cased.
- **Real tool execution** — `read`, `write`, `ls`, `edit`, `grep`, `glob`,
  and `bash` (with strict allowlist, per-call approval mode, and yolo mode).
- **Outbound MCP client** — `[[mcp.servers]]` entries in config are
  connected at daemon startup over Streamable HTTP; their tools are
  imported as first-class, capability-gated session tools. This is how mu
  sessions get semantic code search (`code_recall`) in-loop.
- **Capability-oriented policy** — every tool call is gated by an explicit
  policy. The runtime only dispatches what the session is authorized to
  use. Delegation attenuates authority; it never widens.
- **In-loop discovery** — a native `discover` tool ranks the session's own
  capabilities against a free-text intent, so an agent that doesn't know
  the toolset can find it without shelling out or guessing.
- **Approval primitive** — `session.input_required` suspends work until an
  attached UI, parent session, orchestrator, or policy answers. The same
  primitive backs the bash-prompt modal, capability prompts, and
  orchestrator-mediated approvals.
- **OAuth login** for providers that need it (currently `openai-codex` via
  the standard PKCE flow); tokens persist under `~/.config/mu/auth/` mode
  0600.

### Frontends

**`mu-solo`** ([`crates/mu-solo/`](crates/mu-solo/)) is the daily driver: a
standalone single-pane TUI that spawns its own daemon, with a
claude-code-inspired command surface, scrollback pager, `$EDITOR` handoff,
per-session token/cost metrics, and skill/slash-command support. This is
the frontend mu's own development runs in.

**`mu tui`** ([`crates/mu-tui/`](crates/mu-tui/)) is the multi-dashboard
client (~4,500 LOC): F1–F9 dashboards — command center, session tree,
transcript, context inspector, raw protocol firehose, provider stats,
approvals queue. Use it to watch a daemon from outside the session.

### Observability

- **Typed session event log** —
  `~/.local/share/mu/events/<daemon_id>/<session_id>.jsonl` is the durable
  record. Sessions, tool calls, model calls, context assemblies, compaction
  decisions, approvals, usage, and delegations are all events. Logs are
  written before the in-memory projection is updated.
- **`ContextAssembly` records** — for each model call: which spans entered
  the prompt, the renderer's token estimate, and a per-section token
  breakdown (system / user / tool results / file loads / memory injections
  / …), so "where is my context going?" is answerable from the log.
- **`CompactionAssembly` records** — when compaction runs, the full
  per-span decision audit (kept / dropped+reason / summarized) lands in the
  same log, joined to its `ContextAssembly` by model-call id.
- **Read-only rehydration** — on restart the daemon scans the events
  directory and registers past sessions as read-only entries:
  `session.list` / `session.events` / `session.stats` work across daemon
  restarts. (Live resumption — re-attaching and continuing a rehydrated
  session — is not implemented yet.)
- **TaskTelemetry + analytics** — `mu analytics` projects the event log
  into a queryable SQLite at `~/.local/share/mu/analytics.sqlite`. Preset
  queries: per-session cost, per-policy compaction stats, outcome
  classification (clean / dirty / hollow / lying / narrative).
- **Live firehose** in `mu tui` (F4): every protocol frame in/out of the
  daemon, scrollable, with anchor on streaming.

### Known gaps

- **Live session resumption** — rehydrated sessions are read-only; you
  cannot yet re-attach a frontend and continue the agent loop.
- **`hash-and-summary` on the live path** — benchmarked, not yet selectable
  for live sessions ([mu-8bkf]).
- **`mu orchestrate`** — multi-daemon coordination is sketched in specs but
  not implemented; the subcommand bails with a pointer.
- **Token accounting normalization** — provider usage conventions differ
  (OpenAI's `input_tokens` includes cache reads; Anthropic's buckets are
  disjoint), and not every consumer normalizes correctly yet (mu-rf9x).

## Architectural bet

Most agent tools look like chat wrapped around tool calls. `mu` is built
around a different center:

> agent work should be inspectable, replayable, accountable, and
> capability-bounded.

The runtime is event-sourced: sessions, model calls, tool calls, context
assemblies, usage records, approvals, delegations, and observability facts
are typed events. Transcripts, dashboards, session trees, context
inspectors, and accounting reports are projections over that event log.

The goal is not another opaque coding chatbot. The goal is a runtime where
you can see what the agent knew, which tools it could use, what it did,
what it cost, why it paused for approval, and how a delegated sub-session
inherited authority — from the log, after the fact.

### Workspace shape

```text
crates/
  mu-core/     protocol types, agent loop, event log, transport, tool/provider traits, compaction
  mu-ai/       LLM provider implementations and provider translation layers
  mu-coding/   the `mu` binary: CLI modes, serve frontend, tools, sessions, config, analytics
  mu-solo/     the `mu-solo` binary: standalone single-pane TUI (daily driver)
  mu-tui/      the `mu-tui` binary: multi-dashboard ratatui client
  mu-bridge/   claude-code JSONL → mu event format (PyO3), for importing external session corpora
  t4c/         tools4claude: intent-based tool discovery surface (find / help / run)
```

The architectural contract is the protocol and event model, not a
particular UI. The same daemon supports both TUIs, one-shot CLI calls,
future orchestrated multi-agent runs, and external observability tooling —
all over the same JSON-RPC interface.

### Design principles

**Context is a projection, not a blob.** The transcript is not the session.
The prompt is not the context. The memory is not the store. All three are
projections over the event log. Every model call is explainable by its
`ContextAssembly` record: which spans were included, where they came from,
what they cost in tokens, and what the provider actually received.

**Tools are capabilities.** A model can ask for any tool name it wants. The
runtime only dispatches tools the session is authorized to use — built-in
and MCP-imported tools alike. Delegation should attenuate authority, never
widen it.

**Streaming beats whole-state replacement.** When a consumer might want
deltas or whole state, the primitive should be deltas. A UI can buffer
deltas into a whole view; it cannot recover deltas from an opaque
replacement.

**Observability is a product feature.** Usage, cache behavior, tool calls,
context assembly, compaction decisions, approvals, and session lineage
should be visible — and durable, so the question can be asked after the
session is gone. The agent runtime should not require users to guess what
happened.

### Architecture deep-dives

- [`specs/architecture/mu-capability-substrate.md`](specs/architecture/mu-capability-substrate.md)
- [`specs/architecture/event-sourced-context.md`](specs/architecture/event-sourced-context.md)
- [`specs/architecture/capability-delegation.md`](specs/architecture/capability-delegation.md)
- [`specs/architecture/os-enforced-agent-sandboxing.md`](specs/architecture/os-enforced-agent-sandboxing.md)
- [`specs/measurements/compaction-2026-05-14.md`](specs/measurements/compaction-2026-05-14.md)
- [`specs/delegations.md`](specs/delegations.md)

## Development

A top-level `justfile` collects the common workflows. `just --list` for the
menu; recipes wrap the underlying `cargo` and `scripts/` invocations.
`just ci` mirrors [`.github/workflows/ci.yml`](.github/workflows/ci.yml)
verbatim — a green `just ci` means a green CI, so run it before pushing.

```sh
just ci            # exactly what CI runs: fmt-check + clippy + test (fail-fast)
just check         # full pre-PR check: the ci checks + verify-claims, with timing
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

Live provider tests are gated so CI and routine local runs do not spend API
credits. Enable them explicitly with the relevant environment variables,
for example:

```sh
MU_LIVE_ANTHROPIC=1 cargo nextest run -p mu-ai
```

Project specs live under [`specs/`](specs/). Most features are developed
from small specs, with mechanical work sometimes delegated to sub-agents in
isolated jj workspaces under `.delegations/`.

## Contributing

`mu` is early. Issues, design notes, and patches are welcome, but the
project is still discovering its core shape.

Before contributing, read:

- [`LICENSE`](LICENSE)
- [`LICENSING.md`](LICENSING.md)
- [`AGENTS.md`](AGENTS.md)
- the relevant spec under [`specs/`](specs/)

If you are building commercial products, hosted agent services, proprietary
agent runtimes, model-training pipelines, or agent-evaluation systems from
`mu`, read the license carefully. Commercial production use requires a
separate commercial license until the delayed-open change date.

## License

`mu` is source-available under a delayed-open license: Business Source
License 1.1 with an Additional Use Grant for personal, educational,
research, noncommercial, internal-evaluation, and open-source-project use.
This version converts to BSD-3-Clause on the change date.

See [`LICENSE`](LICENSE) for the controlling terms and
[`LICENSING.md`](LICENSING.md) for the project licensing philosophy.

[mu-8bkf]: https://github.com/sahuagin/mu/issues?q=mu-8bkf
