# cc → mu-core `SessionEvent` mapping (full-fidelity unification)

**Bead:** `mu-cc-event-unification-lkma.1` (WS1) · part of epic
`mu-cc-event-unification-lkma`.

## Why this exists

claude-code (cc) sessions and mu-native sessions must sit on **one** event
schema so analytics (markers, ML, dashboards) read a single substrate. The
canonical schema is `mu_core::event_log::SessionEvent` / `EventPayload`
(`crates/mu-core/src/event_log.rs`). The emitter that converts cc transcripts
into that schema lives in `mu-analytics/cc_telemetry.py`.

**The bug this corrects:** `cc_telemetry.py` historically collapsed an entire cc
session into ONE `TaskTelemetry` event (a contentless summary) + bare `ToolCall`
stubs. Every behavioral marker reads `UserMessage` / `AssistantMessageEvent` /
`ToolResult` / `Done` — exactly the kinds that summary omitted.

## WS1 finding: the schema is already sufficient for turn-level fidelity

Auditing the real cc transcript shape (sampled across all three accounts) against
the mu-core types (`crates/mu-core/src/agent/types.rs`, `event_log.rs`):
**no mu-core schema change is required.** The contentless-summary problem was an
emitter gap, not a schema gap. cc maps onto existing kinds, following mu's own
existing normalizations.

## Field-by-field map (the contract the WS2 emitter implements)

| cc transcript datum | mu-core target | notes |
|---|---|---|
| first assistant `message.model` + cc account | `EventPayload::SessionCreated { provider_kind: "claude_code", model, usage_semantics: Anthropic }` | one per session |
| `type:"user"`, `message.content: String` | `EventPayload::UserMessage { content }` | the actual operator text — markers depend on this |
| `type:"user"`, `message.content: [tool_result …]` | one `EventPayload::ToolResult { call_id: tool_use_id, content, is_error }` per block | `is_error` absent ⇒ `false` |
| `type:"assistant"` | `EventPayload::AssistantMessageEvent { message: AssistantMessage { content, stop_reason, usage } }` | the whole turn |
| assistant block `text` | `ContentBlock::Text { text }` | |
| assistant block `thinking { thinking, signature }` | `ContentBlock::Thinking { text: thinking }` | **signature dropped — CONSISTENT with mu's own native handling**: `accumulate.rs` tracks the signature during streaming but drops it at `ContentBlock::Thinking` construction; `anthropic.rs:777` — thinking "carries display text only." Not a cc-specific loss. |
| assistant block `tool_use { id, name, input, caller }` | `ContentBlock::ToolCall(ToolCall { id, name, arguments: input })` **and** an `EventPayload::ToolCall { call_id: id, name, arguments }` event | `caller` (direct vs sub-agent) deferred → sub-agent sidechain bead |
| assistant `usage.{input_tokens, output_tokens, cache_read_input_tokens, cache_creation_input_tokens}` | `Usage.{input_tokens, output_tokens, cache_read_input_tokens, cache_creation_input_tokens}` | 1:1 |
| assistant `usage.cache_creation.{ephemeral_5m_input_tokens, ephemeral_1h_input_tokens}` | `Usage.{cache_creation_5m_input_tokens, cache_creation_1h_input_tokens}` | tier split maps 1:1 |
| assistant `stop_reason: tool_use / end_turn / max_tokens` | `StopReason::{ToolUse, EndTurn, MaxTokens}` | 1:1 |
| assistant `stop_reason: stop_sequence` | `StopReason::EndTurn` | **normalized — IDENTICAL to mu's own anthropic provider** (`anthropic.rs:678` maps `AnthropicStopReason::StopSequence => StopReason::EndTurn`). Adding a `StopSequence` variant would cascade across ~204 sites and fork mu's deliberate semantics. |
| ask round-trip (assistant turn boundary) | `EventPayload::Done { stop_reason, turn_count, usage, elapsed_ms }` | `elapsed_ms` from consecutive message `timestamp`s; `usage` = the turn's usage |
| session-summed | `EventPayload::TaskTelemetry { … }` | KEEP the existing summary event — now emitted **alongside** the rich stream, not instead of it (sink contract is unchanged → cost parity) |

## Deferred (documented — NOT silent drops)

- **thinking `signature`**: replay-fidelity crypto blob, zero analytical value;
  mu drops it for its own sessions too. Add only if a concrete replay-probe
  consumer needs it (would be a both-fleets schema change, out of this scope).
- **non-token `usage` metadata** (`service_tier`, `server_tool_use`, `speed`,
  `inference_geo`, `iterations`): no mu `Usage` slot; `Usage` derives `Copy` so a
  `String` field (`service_tier`) can't be added without a codebase-wide break.
  Not load-bearing — cc cost is flat-rate subscription (`cost_kind=subscription`),
  so `service_tier` doesn't change cc cost. Revisit if server-tool analytics or
  cost-tier accuracy becomes a requirement.
- **sub-agent (`Task` tool) `caller` + sidechain transcripts**: represent via
  `parent_task_id` / parent-session linkage. Follow-up bead.

## Accepted mu-only asymmetries (cc genuinely cannot produce)

`ContextAssembly`, `CompactionAssembly`, `RecallProvenance`,
`ProviderStatusUpdate`, `Command*` — cc's context-assembly, compaction, recall,
and command internals are opaque from the transcript. Analytics MUST treat their
absence for cc as **unobservable**, never as zero.
