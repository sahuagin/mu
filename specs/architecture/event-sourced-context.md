# Architecture: event-sourced context and session substrate

| field      | value                                          |
| ---------- | ---------------------------------------------- |
| doc_id     | architecture/event-sourced-context             |
| status     | architecture breadcrumb (no immediate impl)    |
| created    | 2026-05-10                                     |
| updated    | 2026-05-10                                     |
| authors    | tcovert + claude (cooperating sessions)        |
| supersedes | none                                           |

## Framing

**This is not a new direction.** It is the application of an
existing events-not-mutations discipline — the same one that gives
observability, audit, and testability in other systems we work on —
to mu's agent runtime state.

Three product directions that were already on the board converge on
one substrate:

- **TUI session tree** (memory `7e44f7ad`) — sessions branch from
  specific points in their parent's history; the tree is a view over
  event lineage.
- **Per-session accounting** (memory `b0e06d20`, mu-021 work) —
  usage is a projection over model/tool/session events.
- **Memory/compaction** (this doc's core) — context is a projection
  over event spans, not a mutable blob.

Doing those three as separate storage problems would be wasteful.
They share an envelope, a provenance model, and a replay mechanism.
Name the substrate; the projections fall out cheaply.

## Thesis

> The transcript is not the session. The prompt is not the context.
> The memory is not the store. All three are projections over the
> event log.

A `mu` session is a typed event log. Events share an envelope
(id, session_id, parent_event_ids, timestamp, actor, kind,
payload). Payloads are typed — they do *not* share a schema.
Different consumers (transcript renderer, prompt assembler, accounting
view, session-tree UI) materialize different projections from the
same log.

## Goals

- **Inspectability.** What did the model know at turn N? Answerable.
- **Provenance.** Every injected prompt/memory segment points back
  to source events.
- **Replay / rewind / fork.** A session is a reproducible trace.
- **Continuous context management.** Working set evolves gradually
  via promote/demote/summarize events, not panic-summarize at a
  watermark.
- **Testing/postmortem.** Many "model failures" are context-assembly
  failures; prompt-assembly events let us stop guessing.
- **Session-tree UI support.** Sub-sessions inherit an explicit
  context view at a point in the event tree, not an opaque blob.
- **Accounting/usage correlation.** Per-event usage rolls up to
  per-session, per-subtree, per-provider.

## Non-goals

- All event payloads sharing one schema.
- Implementing full storage immediately.
- Replacing current protocol types in one pass.
- Making model calls deterministic.

## Event envelope

```text
id
session_id
parent_event_ids        // causal links (lineage, not just order)
seq                     // local ordering
timestamp
actor                   // user, agent, tool, provider, system
kind                    // discriminator for payload
payload_schema          // version tag
payload_json
hash/checksum           // later, if useful
```

Same envelope across kinds. Different kinds participate in different
projections via interface-style markers; see "Projections" below.

## Typed payload families

```text
UserMessage
AssistantMessage
ToolCall
ToolResult
ModelCallStarted / ModelCallCompleted
PromptAssembly / ContextAssembly
MemoryWrite
MemoryRefinement
Compaction
ProviderUsage
SessionCreated / SessionBranched
SessionMessage / MailboxMessage
ErrorEvent
```

Not all events are memories. Not all are transcript items. They
opt in via interfaces:

```rust
trait HasMemorySpans {
    fn memory_spans(&self) -> Vec<MemorySpanRef>;
}

trait HasTranscriptItem {
    fn transcript_item(&self) -> Option<TranscriptItem>;
}

trait HasUsage {
    fn usage(&self) -> Option<Usage>;
}

trait HasContextFragments {
    fn context_fragments(&self) -> Vec<ContextFragmentRef>;
}
```

Participation map (illustrative):

```text
UserMessage          transcript + context fragment + maybe memory candidate
AssistantMessage     transcript + context fragment + maybe memory candidate + usage
ToolCall             transcript + tool audit + context fragment
ToolResult           transcript + tool audit + context fragment
MemoryWrite          memory
MemoryRefinement     memory
ContextAssembly      prompt provenance
ProviderUsage        accounting
SessionMessage       coordination/mailbox
CompactionEvent      context/memory transformation
```

## Projections

The durable store stays simple. Sophistication lives in projections.

```text
Event store
  JSONL / SQLite / both
  append-oriented durable record
  typed envelope + typed payload

Indexes / projections
  transcript view
  memory rope view
  active prompt/context view
  session tree view
  accounting view
  tool audit view
  observability timeline

Prompt assembly
  consumes selected projections/spans
  builds model-specific message payload
```

## Prompt assembly as an event (load-bearing)

For every model call, emit a `ContextAssembly` event that records
what was included in the model payload and why.

The mental model: **the assembled context is a versioned virtual
document with a source map back to events.** Analogous to LSP /
compiler source maps — the provider receives a flat
messages/content-blocks payload, but mu retains coordinates
(`message_index`, `block_index`, `byte_or_token_range`) that map
each region of the payload back to its `source_event_id`,
`source_span`, `source_kind`, `reason_included`, and retention
metadata. Not literally LSP; just the mental model of ranges over a
logical document/view.

For each included segment/span:

```text
segment_id
source_event_id
source_span
priority / retention class
token_count
reason_included
valid_until / relevance score, if applicable
// coordinates in the virtual document:
prompt_message_index
prompt_block_index
prompt_range_start
prompt_range_end
```

Also record omitted candidates when useful:

```text
omitted span F because token budget
omitted span G because superseded
omitted memory H because relevance below threshold
```

The source-map framing is what enables real product surfaces on top
of `ContextAssembly`:

- **`/context why <position>`** — point at a region of the assembled
  prompt and ask which event(s) put it there.
- **Diagnostics on spans** — attach lints to context regions
  ("superseded memory still active", "project instruction omitted
  due to budget", "pinned startup context", "child-session handoff
  included"). Diagnostics layer on the same coordinates the prompt
  is laid out in.
- **Provenance highlighting** in the TUI — hover a region, see its
  source event and reason.
- **Diffing** — two assemblies of the same logical session
  (different policies, different models, different
  rewinds) can be diffed because both have coordinates over the same
  underlying event set.
- **Replay with edits** — modify retention metadata or evict a span,
  re-assemble, see what the model would have received.

This is the single highest-leverage event in the design. Without
it, the harness has the ingredients but not the actual meal. With
it, "what did the model know?" is answerable — and editable.

## Memory as rope/projection

Memory is a projection over selected event spans, not a separate
blob of text.

```text
MemoryRope
  span(event: startup_instruction, bytes/tokens 0..420)
  span(event: user_preference, bytes/tokens 10..210)
  span(event: prior_decision, bytes/tokens 0..180)
  synthetic_span(event: compaction_summary, bytes/tokens 0..300)
```

Externally, a memory reads as a contiguous buffer. Internally, every
piece remains addressable, removable, provenance-preserving.

Retention classes:

```text
startup / always
pinned
hot
warm
cold
archived
superseded
```

The harness maintains a prompt working set continuously, not at a
watermark.

## Compaction as refinement, not overwrite

Conventional:

```text
old transcript -> summary -> discard old transcript
```

Preferred:

```text
CompactionEvent {
  input_spans: [...]
  output_summary_span: ...
  actions: [
    demote span A from active -> archival,
    keep span B pinned,
    replace spans C-D with summary S,
    leave tool result E retrievable but not prompt-active,
  ]
}
```

Compaction is auditable and partially reversible. We can ask:

- what got removed?
- what summary replaced it?
- which original spans support this summary?
- did a user instruction get compacted away?
- can a branch rehydrate the original detail?

## Real-time context management

Context pressure is gradual, not cliff-like. Events the harness emits:

```text
context.promoted
context.demoted
context.summarized
context.pinned
context.evicted_from_prompt
context.rehydrated
```

The UI shows the active context set changing over time. "Agent
attention" becomes a runtime object, not invisible.

## Session tree / branching semantics

A child session does not inherit an opaque parent blob. It inherits
an explicit context view at a point in the event tree.

```text
child_session {
  parent_session_id
  branched_at_event_id
  inherited_context_view_id
  additions: role prompt, task spec, tool budget, provider
}
```

This composes with the TUI session-tree direction (memory
`7e44f7ad`):

- parent chain of turns
- child session branches from a specific turn/event
- subtree accounting rollups
- context diff between parent and child
- rewind and re-fork from an earlier event/view

## Cooperating sessions / mailbox

The same substrate supports cooperating live sessions. Typed
mailboxes, not automatic mind-meld. Sessions publish messages to
named roles; recipients choose to incorporate, ignore, ack, or
respond.

Kinds:

```text
status
question
handoff
observation
```

Potential JSON-RPC shape:

```json
{
  "method": "session.message",
  "params": {
    "from": "design",
    "to": "impl",
    "kind": "handoff",
    "body": "...",
    "requires_response": false,
    "context_refs": ["specs/architecture/event-sourced-context.md", "memory:a15e6caa"]
  }
}
```

Key distinction: direct channel, but no implicit context bleed.
Messages should reference durable artifacts where possible.

## Replay, testing, and post-analysis

When a session's event log has full coverage — input context
assembly, provider request/response, tool calls/results, usage —
the session is a reproducible-ish trace, not a chat transcript.

```text
Replay exact
  same prompt assembly, same provider/model/settings,
  cached response if available

Replay live
  same prompt assembly, call current provider again,
  compare behavior

Replay with modified context
  remove memory X, pin doc Y, change compaction policy,
  rerun from event N

Replay with different model
  same assembled context, different provider/model,
  compare output/tool choices

Unit-test harness
  feed event sequence, assert emitted events / tool calls /
  final state

Postmortem
  inspect what was visible to the model, what it chose,
  what tool data it saw, what memory/compaction policy contributed
```

Many "model failures" are actually context assembly failures:

- stale memory injected
- project instruction missing
- compaction removed a relevant constraint
- tool result omitted or transformed badly
- child session inherited the wrong slice
- provider adapter normalized something incorrectly
- cheap model used where frontier model was required
- hidden system prompt conflicted with project prompt

Prompt assembly events let us stop guessing.

Testing layers this enables:

```text
Provider adapter tests
  provider SSE/API payload -> normalized AgentEvent sequence

Agent loop tests
  AgentEvent + tool registry -> next actions

Context policy tests
  event history + retrieval policy + token budget -> prompt rope

End-to-end trace tests
  fixture event log -> expected final events/state

Regression tests
  bug session trace -> replay after fix -> assert no recurrence
```

## Accounting

This composes directly with the per-turn and (future)
per-session/per-subtree usage tracking. Post-analysis examples:

```text
This delegation cost $0.43.
60% was context resend.
Two tool loops were unnecessary.
The model called read three times for the same file.
The child session inherited 18k tokens it never used.
OpenRouter model A got the same answer as Codex at 1/5 cost.
```

Agent work becomes observable, replayable, comparable, optimizable.

## Product surface examples

Potential CLI/TUI commands the substrate enables:

```text
/context show active
/context why <segment>
/context diff <session-a> <session-b>
/context pin <span>
/context demote <span>
/context rehydrate <span>
/context explain-last-turn
/memory rope show
/memory refine <span>
/session replay-from <event>
/session inbox
/session ack <message-id>
```

## Mapping to current mu state (2026-05-10)

What's already event-shaped (good — minimal rework when persisting):

- `AgentEvent` (mu-core `agent::loop_`) is already a typed enum
  emitted by the agent loop. It maps cleanly to event kinds.
- `AssistantMessage.usage` (mu-021) is already a per-turn usage
  payload — becomes a `ProviderUsage` event or rolls into the
  `AssistantMessage` event payload.
- `session.text_delta` / `tool_call_started` / `tool_call_completed`
  / `callout` / `done` JSON-RPC notifications are already a projection
  of agent loop events to wire format.
- Per-session provider via factory (mu-020) means a
  `SessionCreated` event carries the provider selector as
  provenance.

What we deliberately did *not* lock in (room to grow):

- The current `Sessions` registry holds only in-memory runtime
  state. No flat-blob commitment. The natural evolution is a
  per-session event log that the registry holds (or references in a
  store) plus derived views.

What's missing for the architecture to bite:

- A durable event store (JSONL/SQLite). v1 can be in-memory
  per-session `Vec<SessionEvent>`.
- `ContextAssembly` events. Today the agent loop assembles messages
  for each provider call and forgets the slice. This is the highest-
  leverage missing event.
- Memory-rope projection. We have a separate `agent.sqlite` knowledge
  store (claude-personal); the mu agent-runtime memory is different
  and is what this doc means by "memory."
- Compaction events. mu doesn't compact yet (no context-pressure
  case has hit), so this is greenfield.

## Guidance for current implementation

Do not derail active work to build this whole substrate now. The
work shipped tonight (mu-018 through mu-021) is compatible — the
typed-event shape is already there in the agent loop.

When touching:

- session storage
- context assembly / prompt building
- compaction
- memory (agent runtime, not user-knowledge)
- delegation / session-branching
- TUI / session-tree foundations

…**avoid locking in flat transcript/blob assumptions.** Leave room
for typed event log + projections.

Specifically, the next planned session-cumulative usage tracking
should be implemented as a derived view over per-session events
(or a `Vec<DoneEvent>` proto-log on `SessionState`), not as a flat
counter mutated on every Done.

## Suggested follow-up specs

Not blockers. Filed as discoverable breadcrumbs.

```text
ContextAssembly event records prompt provenance
SessionEventLog in-memory + JSONL backing
Memory rope projection
Compaction/refinement events
Session branch parent/child model
Replay trace harness
Context inspector TUI view
Mailbox / cooperating-session SessionMessage protocol
```

## Skills, tools, and the active context as a retained pointer set

The existing rope model handles memory and transcript spans. This amendment extends the same model to two additional span categories:

- **Skills** — `SKILL.md` + reference files become addressable spans. Skill metadata (frontmatter), section headers, and reference files each address as separate spans. Activating a skill = adding pointers to the retained set; deactivating = dropping them. There is no separate "skill loader" mechanism — skill activation IS pointer-set membership.
- **Tool schemas** — registered tools' descriptions and parameter schemas are spans. The active tool set IS a retained pointer set over tool-schema spans. Capability attenuation, subagent dispatch, and `--tools` filtering all become pointer-set operations.

```text
ActiveContext (RetainedPointerSet)
  span(event: startup_instruction, …)       // pinned, always
  span(event: skill_activation:goal-protocol, file: SKILL.md, lines 1-50)
  span(event: skill_activation:goal-protocol, file: stop-criteria.md, lines 1-150)
  span(event: tool_schema:Edit, version: v3)
  span(event: tool_schema:Bash, version: v3, capability_filtered: true)
  span(event: user_turn_N, …)                // volatile
  span(event: tool_result_M, …)              // volatile
```

The active context is a *materialization function* over the retained pointer set. Renderers turn the set into provider messages (for the agent) or TUI artifacts (for the operator). Provenance is preserved: every byte in any rendered output traces back to a source span and the event that introduced it.

### Implications

- **No separate skill-loading code path.** The skill manager emits `skill.activated { skill_id, span_ids }` events; the rope picks up the spans into its retained set. Deactivation is the inverse.
- **Tool registration becomes scope-local.** A subagent's tool set is its own retained subset over tool-schema spans, distinct from the parent's. No more "register globally, filter per-call."
- **Capability changes are span-set changes.** Attenuating a delegate's capability = filtering the tool-schema span set the delegate inherits. The capability's "tool allowlist" maps directly to a pointer-set predicate.

## Cache-boundary alignment

Provider prompt caching (Anthropic's `cache_control`, equivalents in other providers) is currently expressed at message granularity. Under this model it is *derived* from span retention/stability:

```text
RopeRenderer
  for each retained span, ordered by stability:
    if span.cacheable && span.retention >= hot:
       emit before cache boundary
    else:
       emit after cache boundary
  attach cache_control at the boundary span (provider-specific rendering)
```

The cache boundary falls at the first volatile retained span (or the first non-cacheable stable span — a span can be stable but marked uncacheable for other reasons, e.g., contains time-stamps the model shouldn't rely on).

### Why this is the right inversion

Today: developer or harness annotates each message with `cache_control: ephemeral` at hand-picked boundaries.

Under this model: each span carries metadata (`stable`, `cacheable`, `retention_class`). The renderer derives cache boundaries from that metadata. The annotation moves *from the per-message rendering layer* to the *per-span source layer* — closer to ground truth.

### Independent validation: claude-code 2.1.139

The `--exclude-dynamic-system-prompt-sections` flag (claude-code 2.1.139, observed 2026-05-12) does this manually: pushes `cwd`/`env-info`/`memory-paths`/`git-status` out of the system prompt into the first user message so the system prefix stays cacheable. Under mu's rope model, the same outcome is declarative — those spans are tagged `volatile`, the renderer places them after the cache boundary automatically. No flag needed.

The `--bare` flag is the same idea expressed differently — a different *initial retention set* (drop pointers to hooks/LSP/CLAUDE.md/auto-memory/plugin-sync). Not a different code path; just a different policy.

### Composition with compaction (mu-kgu.6)

Compaction policies operate on the rope and produce a new rope. The cache strategy then re-runs on that post-compaction rope. The two surfaces compose under a single load-bearing invariant:

> **Compaction-cache composition invariant.** After any `CompactionPolicy::compact` returns, running `CacheStrategy::boundaries` on the resulting rope places its boundary AT OR AFTER the position of the last span in the post-compaction rope that is either (a) kept verbatim from the pre-rope AND itself stable + cacheable, or (b) a newly-inserted `SpanKind::CompactionSummary` span. Compaction NEVER shrinks the cacheable prefix below the post-rope's kept-stable span set; it MAY EXTEND the prefix when a Pinned summary span replaces a volatile span that previously truncated it.

The structural reason it holds: `HashAndSummaryPolicy::surgery` (mu-kgu.3) emits `CompactionSummary` spans with `RetentionClass::Pinned`, and the `Span::new` convenience constructor sets `cacheable = retention.is_stable()`, so summary spans are stable + cacheable by construction. A volatile span in the middle of the pre-rope that used to truncate the cacheable prefix becomes a Pinned summary in the post-rope, healing the hole.

Consequence: compaction has a second job beyond saving tokens. It can transform a rope whose cacheable prefix was fractured by interior volatile spans into one whose entire kept tail is a fresh cache prefix — so the *next* ask pays one cache-creation cost and then enjoys cache hits.

Regression coverage lives in `crates/mu-ai/src/context/compaction_cache_tests.rs` and runs under `cargo test -p mu-ai`. The tests assert: drop-tail extends boundary to summary; absorb-volatile-prefix CREATES a cacheable prefix where none existed; absorb-interior heals the truncation; keep-all leaves boundary unchanged; absorb-all yields a single Pinned summary with boundary at index 0. A property sweep asserts the invariant across all five shapes.

## Pluggable cache and provider strategies

Two orthogonal extensibility points emerge from the cache-boundary section:

```text
trait ProviderRenderer
  fn render(rope: &RetainedRope, target: ProjectionTarget) -> ProviderMessages

trait CacheStrategy
  fn boundaries(rope: &RetainedRope) -> Vec<CacheBoundary>
  fn annotate(messages: &mut ProviderMessages, boundaries: &[CacheBoundary])
```

- **`ProviderRenderer`** is per-provider: Anthropic, OpenAI, FauxProvider. Translates the rope into the provider's message format.
- **`CacheStrategy`** is composable with renderer: where to put cache boundaries, given the rope. Different strategies can be tried over the same rope.

### Why this matters

- **Provider differences are explicit.** Anthropic supports `cache_control`; OpenAI does not (currently); FauxProvider is a no-op. Each gets its own strategy implementation. No coupling between rope semantics and provider quirks.
- **Strategies are A/B testable.** Run the same rope through two cache strategies, log hit rates, compare. The rope is the controlled variable; the strategy is the experimental variable.
- **Provider migration is mechanical.** Switching a session from Anthropic to OpenAI = swap the `ProviderRenderer`; the rope is unchanged.

## Agent-view vs operator-view projections

The same retained pointer set materializes into two distinct projections:

```text
ProjectionTarget {
  AgentView,       // provider messages — what the model sees
  OperatorView,    // TUI / log rendering — what the human sees
}
```

These share source spans but render *differently*:

| Span kind | AgentView | OperatorView |
|---|---|---|
| Tool result (large JSON) | raw JSON, full | structured table or summary |
| Skill activation | full SKILL.md + references | one-line badge "skill X loaded" |
| Tool schema | full schema | tool name + one-line description |
| Conversation turn | verbatim | verbatim (typically) |
| Memory injection | full content | "memory X recalled" + collapsible |
| Compacted span | summary only | summary + collapsible to original |

### Why this separation matters

LLM tools today conflate the agent view and the operator view. The human sees the *transcript*, which is also what the model saw. This is convenient but wrong: humans and models have different attention budgets, different relevance criteria, and different debugging needs.

Separating the projections:
- **Operator gets a usable surface.** Tool results are skimmable; agent context is summarizable; non-essential context can be collapsed.
- **Agent gets full fidelity.** No display-driven truncation creeping into what the model sees.
- **Debugging is now possible.** Operator can drill into "what did the agent actually see at turn N?" — that's an `AgentView(at_turn=N)` render of the rope at that time.
- **The transcript is no longer load-bearing.** It becomes one of several projections; if a richer operator view is more useful, build it without affecting the agent.

## Subagent context handoff via shared events + initial pointer-set

A subagent does not inherit an opaque parent blob. It inherits:

1. **Read-only access to the parent's event log** (entire history, queryable).
2. **An initial pointer-set** — a subset of the parent's retained spans, plus optionally synthetic spans summarizing parent context (e.g., "your task is X, here's the file you should focus on").

The subagent then maintains its own retained pointer-set (over BOTH parent events and its own events) and emits its own events into its own event log.

```text
ChildSession {
  parent_session_id
  parent_events_view: ReadOnlyHandle  // shared
  initial_pointer_set: Vec<PointerToParentSpan>
  own_event_log: WriteableLog
  own_retained_set: RetainedPointerSet (over parent and own spans)
}
```

### Eviction semantics

The subagent's pointer-set is INDEPENDENT of the parent's. When the parent's rope compacts and drops a pointer to span S:
- The parent's retained set no longer points at S
- The parent's event log STILL HAS S (events are append-only)
- If the subagent retained its OWN pointer to S, the subagent's rope is unaffected
- "Eviction from parent = forgetting that pointer" — the data isn't destroyed, the reference is dropped

This means parent compaction never strands the subagent. A long-running parent can prune its working set freely; a subagent that took an early snapshot keeps its view.

### Why this is the right shape

- **Decouples parent compaction pressure from subagent context.** Parent can be aggressive about working-set reduction without affecting children.
- **Subagent context is auditable.** "What did the subagent see when it made decision X?" answers from the subagent's retained set at that time, traceable through both parent and own events.
- **Event log sharing is cheap.** Read-only access to an append-only log is a stable reference, not a copy. Multiple subagents can share the same parent event log without contention.

## Span source-change detection via OS file watches

When a span is loaded from a file (skill, tool schema definition, system prompt template), the harness registers a file watch:

- FreeBSD: `kqueue` (`EVFILT_VNODE`, `NOTE_WRITE | NOTE_DELETE | NOTE_RENAME`)
- Linux: `inotify` (`IN_MODIFY | IN_DELETE | IN_MOVE`)
- macOS: `kqueue` or `FSEvents`
- Cross-platform abstraction via `notify` crate or feature-gated

On change notification, emit:

```text
SourceChangedEvent {
  span_id
  source_path
  change_kind: Modified | Deleted | Renamed
  detected_at
}
```

### Policy on the signal (separate from the signal itself)

The default policy is **don't auto-reload**. The rope's retained pointer continues to address the *version of the file at activation time* — preserving reproducibility and ensuring the model never sees a context change it didn't observe via an event.

The operator (or the agent, with permission) can issue `rehydrate(span_id)` to pick up the new version. Rehydration emits a `span.rehydrated` event in the log, making the version change observable.

### Why signal/policy separation matters

- **Reproducibility by default.** "What did the model know at turn N?" is answerable in the same way before and after a file change.
- **Reload is an explicit, audited action.** Not a silent surprise.
- **Multiple policies can coexist.** A development-mode session might auto-rehydrate skills on change (fast iteration); a production session never auto-rehydrates (stability).
- **The watch is cheap regardless of policy.** Registering watches and emitting events doesn't force any particular reaction.

## Related memories

- `a15e6caa` — event-sourced context + rope memory architecture
- `3cc7a18c` — skills/cache/tools rope extension (the source for this amendment)
- `5b69bcdf` — cooperating-sessions mailbox architecture
- `f5a03e25` — mu evening state 2026-05-10
- `b0e06d20` — per-turn vs session-cumulative accounting split
- `7e44f7ad` — TUI session tree design
- `17e4a19d` — accounting requirement

## Related beads (implementation work)

- `mu-qk8` — this amendment (the architecture-doc landing)
- `mu-ktq` — Pluggable `ProviderRenderer` + `CacheStrategy` traits
- `mu-nat` — Skills and tool schemas as rope spans (eliminate parallel loading mechanisms)
- `mu-ovl` — Agent-view vs operator-view projections from the rope
- `mu-x9j` — Subagent context handoff via shared read-only events + initial pointer-set
- `mu-56p` — kqueue/inotify watches on file-loaded spans, with signal/policy separation

## Live-loop adoption (mu-fb0)

The agent loop's per-call sequence now projects session state into
a `RetainedRope` and runs it through the provider's declared
`ProviderRenderer` + `CacheStrategy` before each model call:

```text
assemble_rope(system_prompt, messages, tool_specs)
  → provider.renderer().render(rope, AgentView)
  → provider.cache_strategy().boundaries(rope) + annotate(messages)
  → emit ContextAssembly { renderer, cache_strategy, span_count,
                           cache_boundary_count, first_span_ids, … }
  → provider.stream(system_prompt, &messages, &tool_specs, cancel)
```

The `Provider` trait gained three additive methods (mu-fb0):

```rust
fn renderer(&self) -> Arc<dyn ProviderRenderer>;
fn cache_strategy(&self) -> Arc<dyn CacheStrategy>;
fn provider_label(&self) -> &'static str;
```

All three carry default impls (`FauxProviderRenderer` /
`NoCacheStrategy` / `"faux"`), so existing `Provider` impls
compile unchanged. `AnthropicProvider` overrides to return
`AnthropicProviderRenderer` + `AnthropicCacheStrategy` + `"anthropic"`.

### Resolved design questions

1. **Rope storage** — per-turn projection from `messages`. The
   `messages: Vec<AgentMessage>` field remains the canonical
   session state (external input lands there, the wire `stream()`
   signature still consumes it). The rope is a *projection function*
   applied per model call, matching the spec's thesis that context,
   transcript, and memory are projections. Storing the rope as a
   field is correct in the long term when events become first-class;
   for the mu-fb0 transition, building from the source of truth keeps
   the new path equivalence-preserving with the existing wire body
   byte-for-byte.

2. **`CacheStrategy` dispatch** — trait method on `Provider`. Each
   provider self-declares its renderer + strategy pair, eliminating
   any match-on-`provider_kind` site. The renderer and strategy are
   independent traits, so a future A/B test could pair the Anthropic
   renderer with a *different* strategy by overriding only
   `cache_strategy()`.

3. **`ContextAssembly` event shape** — extended with five optional
   fields (`renderer`, `cache_strategy`, `span_count`,
   `cache_boundary_count`, `first_span_ids[<=5]`). All carry
   `#[serde(default, skip_serializing_if)]` so pre-mu-fb0 fixtures
   serialize byte-for-byte to the same wire shape. `first_span_ids`
   is capped at 5 to keep the event-log row size bounded — the full
   rope is reconstructable from the `SessionEventLog` walk per
   spec lines 167-228; the breadcrumb is for the common "which spans
   entered this call?" question.

4. **Cutover** — single commit. The trait additions are
   default-impl backward-compatible, so all `Provider` impls
   (`FauxProvider`, `OpenRouterProvider`, `OpenaiCodexProvider`,
   `AnthropicProvider`) keep compiling; only Anthropic overrides
   to take advantage of the real renderer + strategy. The
   `Provider::stream()` signature is unchanged — the rope-projected
   `ProviderMessages` are observed (their content + cache markers
   describe what the model will see), but the wire request still
   goes through the AgentMessage path. This preserves stop-
   criterion #9 byte-for-byte; threading `ProviderMessages` into
   `stream()` is left as a separate bead once the operator wants
   the wire-shape change.

### Equivalence guarantee

`crates/mu-ai/src/providers/anthropic_tests.rs` adds five
fb0-tagged tests asserting the rope projection's role sequence,
content surfaces, span count, and cache-boundary placement match
what `build_request_body` produces for the wire body. These
augment the existing 100+ Anthropic and agent-loop tests, which
remain green byte-for-byte (the wire path is untouched).

## Changelog

- 2026-05-10 — initial doc, promoted from cross-session handoff
  prose. Authored across two cooperating Claude sessions (one
  drafting architecture, one orchestrating implementation tonight).
  Framed as application of existing events-not-mutations discipline
  rather than novel invention.
- 2026-05-13 — amendment (mu-qk8): six new sections extending the
  rope model to skills, tool schemas, cache boundaries, projection
  targets, subagent handoff, and source-change file watches.
  Independent validation from claude-code 2.1.139's
  `--exclude-dynamic-system-prompt-sections` and `--bare` flags
  (memory `3cc7a18c`, `fb5cd5be`).
- 2026-05-14 — amendment (mu-fb0): "Live-loop adoption" section
  recording the `Provider` trait extension, the per-call rope
  projection pipeline, the resolved design questions, and the
  equivalence guarantee preserving the existing wire request body.
