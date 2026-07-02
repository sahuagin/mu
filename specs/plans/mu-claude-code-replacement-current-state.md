# mu Claude-Code-replacement current state

| field | value |
| --- | --- |
| status | operational snapshot / roadmap map |
| last terrain check | 2026-07-02 |
| starting revision | `wuoxsolx` / git `6f476383` (`main`, `refactor(dialogue): rip the client-side inbound poller from mu sessions`) |
| checks run | `jj status`, `just smoke`, `just ci`, targeted `mu-core` tests for `mu-rb4u` / `mu-4n8u` / `mu-kgpg`, targeted `mu-coding` spawn-worker tests, OpenAI canary replay/unit/spec checks, local `spawn_worker` dogfood with parent `openai-codex` and child `faux`, targeted file reads / `code_recall`, selected bead inspection; 2026-07-02 follow-up: `just ci` over the metered-env scrub + kx prompt hints + parked-autonomy ask diagnostics stack |
| tracker | central beadsd `mu` remote (`http://10.1.1.172:7771/mcp`) |

This document is the fast map for agents continuing the long-term goal: make
`mu` good enough as the daily coding environment that Claude Code is no longer
needed. It is **not** an architecture invariant. Treat it as a repo-versioned
snapshot: useful for orientation, but always terrain-check before closing beads
or changing code.

## First ten minutes for a future agent

1. Read `AGENTS.md`, then this file.
2. Inspect workspace state:
   ```sh
   pwd
   jj status
   jj log -r 'parents(@) | @' --no-graph
   ```
3. Check the live tracker before editing:
   ```sh
   beads --url http://10.1.1.172:7771/mcp ready --limit 30
   ```
4. Prefer semantic code recall for orientation, then file reads / grep for exact
   verification.
5. Run the cheap runtime smoke before trusting broad claims:
   ```sh
   just smoke
   ```

## Hard invariants to keep in view

Do not duplicate the architecture specs here. The load-bearing sources remain:

- `AGENTS.md` — repo-specific build/test/workflow rules and invariant summary.
- `specs/architecture/event-sourced-context.md` — event-log substrate,
  context/prompt projections, `ContextAssembly` as source map.
- `specs/mu-046-ingest-pipeline.md` — command journal, receipts, pipeline
  invariants, no side doors.
- `specs/architecture/session-lifecycle.md` — liveness is a projection,
  per-turn provider identity, explicit handoff semantics.
- `specs/architecture/worker-orchestration.md` — as-built worker spawning path.
- `specs/architecture/agent-context-taxonomy.md` — when to use AGENTS, skills,
  hooks, MCP, workers, and policy.

Short form: the on-disk event log is source of truth; commands are durable
before processing and fail closed; session rehydration is lazy; frontends are
hats over `mu serve`; context is a projection/rope; authority narrows, never
widens.

## Provider / protocol terrain

What terrain showed in this survey:

- **Anthropic direct API** uses the clean `mu-anthropic` wire crate from
  `crates/providers/mu-anthropic`.
- **OpenAI public API and OpenAI Codex/OAuth** use the clean `mu-openai` wire
  crate through `mu-ai/src/providers/openai.rs`.
- **Ollama** now speaks the Anthropic-compatible `/v1/messages` path by
  composing `AnthropicProvider`; it deliberately does not read
  `ANTHROPIC_API_KEY` and disables Anthropic cache markers for the ollama
  backend.
- **OpenRouter** is still its own OpenAI-compatible chat-completions provider,
  not a consumer of `mu-openai`'s Responses API types. This may be the right
  endpoint choice, but it is still an explicit design/implementation decision,
  not already-settled terrain.
- The agent loop now calls providers with `MessageInput::Projected`, so the
  rope/provider projection path is live rather than observational.
- Thinking/reasoning effort is threaded through session creation, `ask_session`,
  mu-solo, and the provider trait. Exact valid effort levels are still a daemon
  authority problem for frontends to consume, not infer.

## Context / compaction terrain

- `ContextAssembly` events are emitted before provider calls and are the best
  answer to “what did the model know?”
- Tool-call and tool-result association must be by call id, not adjacency.
  Terrain now includes call-id extraction from tool-result span ids and
  compaction reconciliation tests that prevent orphaned tool exchanges.
- The Codex/openai reasoning-only empty-turn class has a bounded actionless-turn
  auto-continue guard in the agent loop. Future work should verify live behavior,
  but the small loop guard is no longer absent.
- Context limit handling has improved (`mu models sync`, generated model layers,
  no fabricated ollama placeholder driving compaction), but provider-aware
  pre-dispatch budget refusal/warning remains important for daily-driver trust.

## Orchestration / worker terrain

The mu-native `spawn_worker` path has been pulled onto the shared dispatch
substrate:

- `scripts/agent-role` still resolves provider/model by role and remains
  ollama-lease-aware when ranking targets.
- `scripts/lib/agent-dispatch.sh` is now the one dispatch function for review,
  orchestrator, and `mu-spawn`; it sends `claude-oauth` through `claude -p`,
  everything else through `mu ask --bare`, and acquires the shared ollama lease
  for ollama dispatches.
- `spawn_worker` registers a subprocess session, derives a child built-in tool
  grant from the parent session's delegable tools, narrows it with
  `Capability.allowed_tools` when that axis is populated, and passes it to
  `mu-spawn` as `MU_SPAWN_TOOLS`.
- `mu-spawn` no longer hardcodes `read,write,edit,glob,grep,ls,bash --bash-yolo`.
  It forwards the child grant into `agent_dispatch`, so read-only parents spawn
  read-only children and write/yolo parents keep their legitimate write/yolo
  worker path.

- Normal root sessions now populate live `Capability.allowed_tools` from the
  final launch-time session tool vector (base `--tools`, imported MCP tools,
  plus per-session control/discovery tools). Worker attenuation and discovery
  now see the same concrete parent authority instead of relying on a
  spawn-worker-only side grant.

## mu-solo / frontend terrain

- `mu-solo --profile/-p <name>` exists and selects `[profile.<name>]` from
  `~/.config/mu/solo.toml`, then resolves model aliases through the model
  catalog.
- Route/model discoverability improved after `fix(routes): expose generated
  model catalog entries`: `RouteCatalog::from_env()` imports
  `models.generated.<provider>.toml` entries, and mu-solo's provider/model
  picker consumes daemon `daemon.list_routes` metadata (provider/model labels,
  context, max output, configured status) instead of relying only on curated
  frontend lists.
- The TUI is usable for daily work but still thin. The big live UX work remains:
  viewport streaming, fullscreen transcript mode, context inspection, rewind,
  multi-session visibility, and route/model discoverability.
- Frontends should not be sources of provider truth. The model list picker now
  leans on daemon route data; remaining known authority leak: mu-solo still
  computes valid effort levels locally in places where the daemon should resolve
  and send authoritative values.

## MCP / discovery / ai-help terrain

- MCP is functional enough that `just smoke` imports tools from the configured
  code-index and mu-dialogue MCP servers and the daemon serves its MCP socket.
- `code_recall` is a mu builtin tool backed by libt4c. The `t4c` CLI exists
  primarily for Claude Code and host-side use, because Claude Code does not get
  mu builtins.
- The model-facing structured help convention is `--help-ai --json`; `t4c` and
  `agent` both speak it.
- The mu MCP surface implements negotiated `experimental.mu.aiHelp` and custom
  method `mu/aiHelp` to expose trimmed `--help-ai` nodes over MCP.
- Operator config on 2026-07-02 imports the full current `agent-mcp` memory /
  task / metrics / kx MCP surface (read and write tools with side-effect
  classifications) and enables the direct `mu-dialogue` MCP server tools. A live
  `mu serve --enable-mcp --bare` probe showed `dialogue_poll`,
  `dialogue_history`, `dialogue_peers`, and `dialogue_say` in
  `capabilities/discover`.
- mu now has opt-in prompt-time kx document hints (`[recall].kx = true`): the
  agent loop runs bounded `agent kx recall` for the current user intent and
  injects compact doc pointers as a transient span. It is off by default because
  it may call the configured embedder/API.

Do not treat `t4c find` missing a mu builtin as evidence that mu lacks that
builtin. Inside mu, prefer the native tool surface (`discover`, `code_recall`,
etc.); use `t4c` for Claude Code compatibility or host-side checks.

## Current high-leverage next sequence

1. **Finish dialogue receive the right way.** The direct MCP tools are now
   usable, but automatic inbound mu-session wakeup is still unresolved. Do not
   resurrect the old client-side long-poll side door blindly; terrain/memory say
   the desired architecture is event-driven delivery through the mu-046 ingest /
   sequenced-input path.
2. **Autonomous-run operator notifications.** Parked asks now reject with an
   explicit wake time/reason instead of silently buffering. Remaining work:
   long tool waits, parks, errors, and `input_required` need to reach an away
   operator through a visible notification surface.
3. **Decide OpenRouter/OpenAI-compat extraction.** Either extract a shared
   chat-completions compatible layer or explicitly document why OpenRouter stays
   separate from the typed Responses crate.
4. **Move remaining frontend effort authority to the daemon.** Model lists now
   come from daemon routes; effort levels/options should follow the same pattern
   rather than being recomputed by mu-solo.
5. **Keep improving mu-solo's daily-driver UX.** Prioritize issues that make
   Thaddeus stay in mu rather than reaching for Claude Code.

## Stale-bead ledger from this survey

These were stale or misdiagnosed by terrain. The first four were closed during
this cleanup pass after targeted verification; keep the remaining entries as
live orientation until their own terrain-checks say otherwise.

| bead | terrain finding / action |
| --- | --- |
| `mu-odtc` | Closed 2026-07-02: `claude-oauth` dispatch now scrubs `ANTHROPIC*`, Bedrock, and Vertex env before invoking Claude Code, and `.mu/emu` no longer silently hydrates Anthropic/OpenRouter keys into default `mu-solo` sessions. |
| `mu-kx-prompt-hints-jn1o` | Closed 2026-07-02: prompt-time kx hint injection exists behind `[recall].kx`, with bounded subprocess recall and loop tests. |
| `mu-autonomous-sleep-operability-f4ib.2` | Closed 2026-07-02: `ask_session` against a parked autonomous run rejects immediately with explicit wake time/reason and says the message was not queued; receipts stay non-orphaned through the normal rejection path. |
| `mu-rb4u` | Closed: actionless/empty Codex turn guard exists in the agent loop; `cargo test -p mu-core rb4u_` passed. |
| `mu-4n8u` | Closed: compaction tool-pair reconciliation by call id exists; targeted orphan-prevention tests passed. |
| `mu-kgpg` | Closed: `SessionEventLog::append` and `append_command` share an append-order critical section; mixed append-path ordering test passed. |
| `mu-openai-protocol-canary-drift-slld` | Closed as misdiagnosed drift: log showed live checks ran from `/home/tcovert` and failed to find `Cargo.toml`; `scripts/openai-protocol-canary.sh` now uses `--manifest-path "$repo/Cargo.toml"` for live checks. |
| `mu-ktj0` | Closed with the launcher fix: `mu-spawn` is non-POT, carries dialogue identity guidance, and now routes through `agent_dispatch`; `mu-lqa0` closed the remaining hardcoded full-power grant. |

## Update triggers

Update this snapshot when any of these change materially:

- provider routing or wire-protocol ownership changes;
- context assembly, compaction, or event-log invariants change;
- worker spawning / orchestration / agent dispatch semantics change;
- mu-solo becomes materially more capable;
- a P1 blocker in the sequence above is fixed, split, or demoted;
- stale-bead reconciliation changes the roadmap;
- `AGENTS.md` architecture invariants change.

## Update checklist

- [ ] Re-run `jj status` and inspect parent/current changes.
- [ ] Run `just smoke` or a more relevant smoke.
- [ ] Check ready P0/P1 beads from the central mu beadsd remote.
- [ ] Verify every “landed” claim against code/tests/CLI behavior.
- [ ] Update the stale-bead ledger.
- [ ] Update the next recommended sequence.
