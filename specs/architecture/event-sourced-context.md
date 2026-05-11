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

## Related memories

- `a15e6caa` — event-sourced context + rope memory architecture
- `5b69bcdf` — cooperating-sessions mailbox architecture
- `f5a03e25` — mu evening state 2026-05-10
- `b0e06d20` — per-turn vs session-cumulative accounting split
- `7e44f7ad` — TUI session tree design
- `17e4a19d` — accounting requirement

## Changelog

- 2026-05-10 — initial doc, promoted from cross-session handoff
  prose. Authored across two cooperating Claude sessions (one
  drafting architecture, one orchestrating implementation tonight).
  Framed as application of existing events-not-mutations discipline
  rather than novel invention.
