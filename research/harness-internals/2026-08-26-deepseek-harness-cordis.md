# DeepSeek Harness / Cordis internals — what transfers to mu

*Research writeup, 2026-08-26 (session dd3b3009). Source: shallow clone of
`github.com/deepseek-ai/deepseek-harness` (MIT, v0.1.1-rc.2), kept at
`~claude/src/public_github/deepseek-harness`. mu claims below were
terrain-checked against mu `main` the same day; file anchors are to that state.
Beads filed from this study: `mu-eyfd8` (rope span epoch invalidation),
`mu-0xhja` (per-session disposer tree).*

## Context: why this repo mattered

DeepSeek released Harness (`dsh`) on 2026-08-13 alongside DeepSeek-V4-Pro. It
hit ~90k GitHub stars in 28 hours and ~135k in four days. The stir was not
about open-sourcing an agent CLI (Codex CLI, Gemini CLI, OpenCode, Pi already
exist); it was about the architecture and the positioning: a *framework* whose
agent loop is itself a replaceable plugin, with a formally-grounded event model
and full trace replay, from the company that commoditized frontier models. The
press narrative ("micro-kernel", "everything is a plugin") traces to a real
mechanism, examined below.

## Finding 1: Cordis is not theirs, and it is tiny

`vendor/cordis` is the pre-existing cordisjs plugin framework from the Koishi
chatbot ecosystem, vendored and rescoped to `@deepseek-ai/cordis`, with
upstream SHAs tracked in a vendor manifest. The entire kernel is ~2,700 lines
of TypeScript across eight files (`context.ts` 146, `events.ts` 352,
`fiber.ts` 754, `reflect.ts` 418, `registry.ts` 337, `service.ts` 115, plus
utils/logger). Everything the press calls revolutionary sits in those files
plus a thick discipline layer on top. The leverage is in the conventions, not
the kernel code.

## How the kernel works

**Context.** A `Context` is a proxied dependency container. Child contexts are
prototype-inherited: `extend()` adds metadata, `isolate(name)` scopes a service
name to a fresh label so a subtree can get a different implementation, and
`intercept(name, config)` layers config for plugins loaded below. None of
these mutate the parent.

**Fibers.** Each `ctx.plugin()` call creates a `Fiber` — the plugin's runtime
instance. Lifecycle (`PENDING → LOADING → ACTIVE → UNLOADING → DISPOSED`,
plus `FAILED`) is driven entirely by dependency state: a plugin declares
`inject: ['bash', 'session']` and its fiber pends until every named service is
provided by an ACTIVE fiber. The load-bearing trick is the **epoch**: a
fiber's epoch is the concatenation of its providers' uids. When a provider
reloads (new uid), every dependent's epoch changes and it automatically
unloads and reloads in cascade. Startup ordering, hot-swap, and teardown are
one mechanism — there is no separate init sequence to maintain.

**Effects.** Every registration (event listener, provided service, timer,
anything) goes through `ctx.effect()` and returns a disposer. Disposers are
collected per-fiber, run in reverse registration order at unload, and form a
labeled diagnostics tree (`getEffects()`). Leaked handlers are structurally
impossible. In-flight load/unload transitions are joinable promises
("inertia"), so concurrent dispose-vs-reload cannot race.

**The cost.** `fiber.ts` is 754 lines and most of it is re-entrancy defense
("a synchronous observer may dispose either this fiber or its parent").
That is the price of hot-reloading in-process plugins, and it is where the
community's over-engineering critique bites. Third-party reviewers also
measured ~10x token overhead per turn vs a minimalist harness (Pi: ~4.5k
input tokens vs dsh ~47.6k) — the cost of every capability shipping its own
prompt section.

## The loop is a plugin — extension via typed events with chosen dispatch modes

`packages/core/agent-loop` (~1,600 lines) is the only concrete loop, and their
architecture note is explicit that nothing outside it may depend on it; the
event vocabulary lives in contract packages, so a different loop can be
dropped in. Extension points are typed events with *deliberately chosen*
dispatch modes:

- **waterfall** — around-middleware that can transform or short-circuit:
  `agent/pre-step`, `agent/request`, `system-prompt/assemble`,
  `tools/pre-execute`, `tools/execute`, `tools/post-execute`, `llm/stream`.
- **serial** — awaited in listener order for ordered checkpoints:
  `agent/turn-stopping`.
- **parallel** — awaited fan-out where every listener must get an independent
  chance: the `session/flush` durability checkpoint.
- **emit** — synchronous fire-and-forget notifications: inbox transitions,
  lifecycle, errors.

They considered a koa-compose middleware stack and an explicit phase state
machine and rejected both because Cordis events already carry disposal and
reload semantics for free.

## Event-sourced sessions: "model-visible ⟺ logged"

A `Session` is an append-only log of typed `SessionEvent`s — the single source
of truth. The LLM message history is *derived* from the log
(`deriveMessages()`); raw stream chunks are logged for token-level replay
while the assembled `assistant/message` event is authoritative. Appends are
synchronous (hot path never blocks on I/O); persistence plugins buffer
write-behind and drain at the awaited turn-end `session/flush`. Their stated
repo-wide invariant: anything that reaches a model request must be
reconstructable from the session log, and a new model-visible input *requires*
a new session event type.

Versioning: one monotonic integer (`SESSION_FORMAT_VERSION`), bumped only for
structural changes, decided by what the *writer* emits. Ordinary vocabulary
growth is handled by an `ignorable` marker on the event envelope — a reader
refuses a log containing an unknown event type unless that event says it is
skippable, because "parses without error is not correctness."

## Capability seams: Service Definition / Service Provider / Consumer

A swappable capability is three roles: the Service Definition (the contract
and vocabulary, owns `ctx.<key>`), Service Providers (implementations), and
Consumers (what the model programs against — the tool schema). The bash trio
is the template: `dsh-shell` / `dsh-bash-local`+`dsh-bash-sandbox` /
`dsh-tool-bash`. The point is rates of change: swapping local execution for
sandboxed never touches the schema the model sees. They are explicit about
not splitting preemptively.

## Composition is data

A whole application is a `cordis.yml`: an ordered list of plugin ids with
config, `!!js` expressions allowed only in config values. The example ACP
agent assembles model adapter, sandbox policy, bash, approval, compaction,
subagents, workflows, and Claude Code / Codex hook bridges (they ship compat
with both ecosystems' hooks.json dialects) purely from that file. They also
ship subagent providers that literally run Claude Code and Codex binaries off
the host PATH.

## Process discipline worth stealing regardless of architecture

- `.agents/notes/` — 143 implemented architecture decision notes in
  problem / decision / alternatives / consequences form, **required in the
  same PR** for non-trivial changes, with statuses
  (proposed/implemented/rejected/archived) and a rule that factual drift is
  updated in place but a reversed *decision* needs a new note.
- Their AGENTS.md independently states several of our rules: "no hardcoded
  tunables in plugins — deployment-varying choices are validated Config
  fields; a `DEFAULT_*` constant is not configurability" (the spline
  risk-limits rule in spirit), "misconfiguration fails loud", branded ids for
  cross-boundary identifiers, "an empty catch names what it swallows."
- Keyless snapshot transcripts through a real runnable example are required
  evidence for every model-visible behavior change; per-file 100% coverage is
  the CI gate; doc budgets and generated catalogs are enforced by doc-sync
  gates.

## What transfers to mu — refined after terrain-checking mu

The first-pass list was cut down hard by checking mu's actual internals
(2026-08-26, with Thaddeus). What survived, what dissolved:

**1. "Model-visible means logged" — the invariant exists in mu; the gap has a
precise location.** Intake is solid: `SessionEventLog::append_command` holds
the append-order lock across assign-id → write → fsync → memory push and
refuses to run without a disk writer (spec mu-046 INV-1). The leak is prompt
assembly: the loop's history is an in-memory `AgentMessage` vec, the rope is
assembled per call, and several things are injected *at assembly time*
(discover hints, kx hints, project-context memory/file spans, capability
hints). What the log records about assembly is `ContextAssembly` — counts and
token breakdowns, not content. So the log answers "how big" but not "what
exactly did the model see" for content born at prompt time. dsh's fix is
structural, not disciplinary: history is derived from the log on every
request, so an unlogged input cannot reach the model — the log is the input
path, not a reporting side-channel. mu already does log-derivation in one
place (resume: `continuation::project_strict`), and the rope already keeps an
internal append-only `RopeEvent` log with a provenance map; the seam is that
RopeEvents are not SessionEvents. If rope assembly consumed only spans that
exist as durable events, the invariant would hold by construction.

**2. Epoch-cascade invalidation → rope rehydration (bead `mu-eyfd8`).** Spans
with external sources (FileLoad, ToolSchema, SkillActivation) carry an epoch
derived from source identity (file hash, registry version). Source change →
span re-derives → dependent spans and cache-prefix placement invalidate
mechanically. Subsumes the deferred file-watch rehydration (mu-56p) as one
mechanism instead of per-span-kind watchers, and gives
`RetentionClass::is_stable()` an honest cache-bust signal.

**3. Disposer/ownership tree for daemon teardown (bead `mu-0xhja`).** Every
per-session registration (forwarder task, watch senders, live-state flags,
outstanding provider calls) returns a guard collected on a per-session owner;
teardown is reverse-order disposal; "what did this session leave running"
becomes mechanically answerable. Current terrain: the forwarder's
channel-close cleanup comments it is the "last chance to avoid wedging the
session as permanently busy" (`crates/mu-coding/src/serve/forwarder.rs`, end
of `forward_events`) — exactly the seam this makes structural. Rust shape:
RAII guards + owner-held list with async-joinable teardown; NOT the Cordis
epoch/hot-reload state machine.

**4. Loop extension points — a maybe, at the AgentLoop, not the
orchestrator.** The orchestrate pipeline is `scripts/orchestrator/` — a
process on top, not mu internals (a first-pass framing error, corrected). The
place the "plugins, not loop changes" observation lands inside mu is
`AgentConfig`: it accretes one field per feature (`discover_hints`,
`kx_hints`, `project_context`, `compaction_policy_override`,
`seed_messages`, `effort`, ...), each requiring a loop edit. The Rust
translation would be hook traits at the loop's natural seams (pre-request
assembly, post-tool). Whether the indirection pays in a single-team monorepo
is an open judgment call.

**5. Capability seams — dissolved.** mu already has all three roles: the
`Tool` trait as the model-facing contract, providers behind it,
`Capability.allowed_tools` / alias-level config attenuation, and the rope
models attenuation as `filter_tools`. The only thing dsh adds is npm package
boundaries so *external* plugin authors can version against the contract
alone — an ecosystem concern mu doesn't have.

**What NOT to import:** the in-process kernel itself. The Context proxy
magic, mixins, and the epoch/inertia state machine pay for hot-reload of
in-process plugins — a capability mu (multi-process, script-orchestrated)
doesn't need. The transferable layer is the contracts: dependency-declared
activation, everything-returns-a-disposer, typed events with chosen dispatch
modes, and the event-sourced log.

**Where mu is ahead:** dsh has nothing like the rope on the context axis. It
assembles the prompt through a waterfall of string-section contributors; no
retention classes, no provenance-addressable spans, no cache-placement model.
Traffic on that axis runs mu → dsh, not the other way.

## Fiber vs rope

They are different axes — a fiber is the lifecycle of a *code unit*, the rope
is the substrate of *context content* — but both are the same underlying
shape: a retained set + append-only membership events + provenance. Rope:
spans / RopeEvents / origins map. Fiber: disposables / EffectMeta tree. The
two adoptable mechanisms are exactly the beads above (epochs for
source-driven invalidation, disposer trees for ownership); the skill
activation symmetry (deactivating an activation disposes exactly the spans it
introduced) mu's pointer-set membership already handles.

## Fast tour of the source

- `vendor/cordis/src/fiber.ts` — the whole lifecycle mechanism.
- `.agents/notes/implemented/architecture/2026-06-11-microkernel-event-taxonomy.md`
  and `...-event-sourced-sessions.md` — the two decisions that define the
  system.
- `examples/acp-agent/cordis.yml` — a whole application as config.
- Root `AGENTS.md` — the conventions layer.
