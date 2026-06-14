# INTEGRATION.md — how mu-anthropic plugs into mu (and how we build it)

This doc is the seam map and the development methodology. It exists so a new
session does NOT have to re-derive (a) where this crate connects to mu, or
(b) why we are building greenfield instead of editing the existing code.

Read alongside `PLAN.md` (what to build) and `AGENTS.md` (the rules).

---

## 1. What we are replacing

mu's current Anthropic support lives in **one file**:
`crates/mu-ai/src/providers/anthropic.rs` (~1040 lines). It does FOUR jobs.
mu-anthropic re-implements three of them as typed wire protocol; the fourth
(the agent-loop-coupled stream driver) stays in mu.

| Job in the old file | mu-anthropic equivalent | Keep in mu? |
|---|---|---|
| **Outbound request build** — `build_request_body*`, `translate_messages`, `translate_provider_*`, `cache_control_value`, `detect_cache_targets` | typed request structs (`MessagesRequest`, `Message`, `ContentBlock`, …) + `Serialize` | **replace** |
| **Inbound wire types** — `AnthropicEvent`/`Block`/`Delta`/`Usage` (already `#[serde(tag="type")]`) | typed response + streaming-event structs + `Deserialize` | **replace** |
| **Async accumulator** — folds SSE deltas into a final `Message` (the SDK's `get_final_message()` shape) | a clean **async, streaming** accumulator over typed `StreamEvent`s | **provide here**; mu wraps it |
| **Stream driver + transport** — `stream()`, `StreamState`, `next_event`, headers (`x-api-key`, `anthropic-version`), cancel, `ProviderEvent` emission, `DegradedEof` | — | **stays in mu** (agent-loop + transport concern, not wire format) |

The old file is **not deleted and not edited** while we build. It keeps mu
running. It dies later, all at once, when mu's call site is rewired to
mu-anthropic (see §5).

---

## 2. The seam (both directions)

```
OUTBOUND
  mu RetainedRope
    └─ AnthropicProviderRenderer (mu-ai/src/context/anthropic.rs)   [KEEP: mu policy]
    └─ AnthropicCacheStrategy   (places <=4 ephemeral markers)      [KEEP: mu policy, cost-measured]
         ▼  ProviderMessages { role, content, cache_marker, blocks: Vec<ContentBlock> }
    ── From<&ProviderMessages>  (lives MU-SIDE, not here) ──▶  mu_anthropic::MessagesRequest
                                                                     │ serialize
                                                                     ▼  wire JSON
INBOUND
  wire SSE line ── deserialize ──▶ mu_anthropic::StreamEvent
                                       │  (mu's stream driver folds these;
                                       │   or mu-anthropic's async accumulator
                                       │   assembles the final Message)
                                       ▼  ProviderEvent / AssistantMessage   [mu narrows]
  wire (non-stream) ── deserialize ──▶ mu_anthropic::Message ── [mu narrows] ──▶ AssistantMessage
```

**mu-anthropic owns:** typed `MessagesRequest` (+serialize), typed `Message` /
`StreamEvent` (+deserialize), the async accumulator, the protocol constants
(`anthropic-version`, the `cache_control` / `ttl` wire shapes, stop-reason
strings, content-block `type` tags).

**mu-anthropic does NOT own and MUST NOT import:** `RetainedRope`, `Span`,
`CacheStrategy`/`CacheMarker`, `ProviderMessages`/`ProviderMessage`, mu's
`ContentBlock`, the transport, the agent loop. (See AGENTS.md: no-import-mu
DAG. The conversions live mu-side; the consumer owns the mapping.)

### The integration contract types (read for convertibility, not dependency)

mu-anthropic's types should be shaped so the mu-side `From` is MECHANICAL.
The two mu types the `From` reads:

- `ProviderMessage` = `{ role: ProviderRole, content: Arc<str>,
  source_span_ids, cache_marker: Option<CacheMarker>,
  blocks: Option<Vec<ContentBlock>> }`
  — `mu-core/src/context/renderer.rs`
- mu's `ContentBlock` = `Text{text} | ToolCall(ToolCall{id,name,arguments}) |
  Thinking{text}` — `mu-core/src/agent/types.rs`

Mapping notes (so the type layout leaves room):
- mu's `ContentBlock::ToolCall` → wire `tool_use {id,name,input}`. 1:1.
- mu `Thinking` is **stripped outbound** (never echo the model's reasoning
  back as input — old file does this; keep the behavior).
- mu carries **tool results at the message level** (`ProviderRole::ToolResult`
  + `content`), NOT as a block. Outbound, consecutive tool results batch into
  one `user` message of `tool_result` blocks. mu-anthropic's types need a
  `tool_result` block; the batching can live in the `From`.
- **`cache_control` is PER-BLOCK** on the wire (confirmed by the spec's
  request-idempotency table AND the old file's hoist logic). mu's
  `cache_marker` is per-MESSAGE; the `From` translates message-marker →
  block-level `cache_control`. Some markers map to envelope positions
  (`body.system`, last tool spec) — see old `detect_cache_targets`.

---

## 3. Streaming is mandatory and async-only

Anthropic's Messages API **always streams** for non-trivial `max_tokens`
(their SDK warns large non-streaming requests risk the ~10-min timeout; the
long-request guidance is "you must stream"). So:

- mu-anthropic models the **streaming** event shape as first-class typed
  events (`message_start`, `content_block_start`, `content_block_delta`,
  `content_block_stop`, `message_delta`, `message_stop`, `ping`, `error`).
- It provides an **async accumulator** (the `get_final_message()` shape): a
  streaming consumer that reads events and yields the assembled `Message`
  once `message_stop` arrives — never blocking, never a synchronous API.
- The non-streaming single `Message` response is ALSO modeled (smaller, used
  for the first conformance proofs and for the count-tokens endpoint), but
  the production path is streamed.

No synchronous interfaces. Async + streaming wherever the protocol offers it.

---

## 4. How we build it — "reference, don't inherit"

Editing the old file in place has failed before: an agent absorbs the bad
patterns (the stringly-typed `json!{}` mutation — magic `"system"`/`"type"`/
`"text"` keys, no type that says "a valid request looks like THIS") and
"fix in place" has no clean done-state, so it drifts. The APPROACH is the bug,
not individual lines. So:

**The old file is a SPEC SOURCE, read-only. We extract facts, not code.**

1. **Behavioral oracle.** The old file encodes hard-won wire facts (see §6
   scar list). We re-express those as mu-anthropic types + tests. We do NOT
   copy its code.
2. **Parity target (prior art, not ground truth).** mu's canonical wire
   outputs are a useful cross-check: does our typed serializer produce the
   same bytes the shipping code does on the canonical scenarios? Capture the
   old outputs as **fixtures** (don't call the old `pub(crate)` fns across the
   crate boundary). But: prior art can be WRONG — ground truth is wire traces
   (§ below), not the old tests.
3. **Scar list → regression tests.** Each `mu-XXXX` fix-comment becomes a
   named test in mu-anthropic, so the bug can't recur in the new crate.

### Ground truth = wire traces

The authority for "what the wire actually looks like" is **captured real
traffic** (socat in front of claude-code, or the mu proxy with full logging),
NOT the spec text and NOT the old tests. Spec informs; old tests are prior
art; **traces adjudicate.** The pinned `specifications/llms-full.txt.xz` is the
documented contract; the traces are the observed contract; golden tests pin
both (PLAN tier 2 = spec-conformance, tier 3 = trace-conformance).

### Contract-break awareness gate

Where mu-anthropic intersects a mu seam that HAS a test, and our rewrite would
break that contract (e.g. the `parity_compare` Legacy==Projected invariant,
`assemble_content` block ordering, the usage-merge-across-two-events
semantics):

> **Lift a sibling test into mu-anthropic so the break is visible and CHOSEN.**

Breaking the contract is allowed. Breaking it *silently* is not. The test is a
tripwire that forces a deliberate decision, not a cage. (Same principle as the
rest of this work: a protection you can VERIFY beats one you have to TRUST.)

### The development loop (vertical slices)

Build leaf-first, one complete slice per commit. A slice = **one type + its
serde + its tests (round-trip, spec-conformance, parity-vs-fixture where it
applies)**. It compiles, tests pass, it's committed — that's the done-gate.
No half-types, no 1000-line file to edit; the crate is always coherent and
small enough to hold in head.

Slice order (each builds only on already-green slices):

1. `ContentBlock` (text / tool_use / tool_result / thinking; optional
   `cache_control`) — internally `#[serde(tag="type")]`.
2. `Message` (role + Vec<ContentBlock>).
3. `MessagesRequest` (the envelope: model, max_tokens, system, messages,
   tools, stream, cache knobs) — envelope/payload split per PLAN.
4. Response `Message` (non-streaming) + `Usage`.
5. `StreamEvent` enum (the SSE event shapes).
6. Async accumulator (StreamEvent stream → final `Message`).

The commit chain IS the slice sequence — which is also the "followable
coherent steps" history the operator wants. Same artifact, two goals.

---

## 5. Migration (later, mu-side, operator's call)

When the slices are green and trace-validated, mu's `providers/anthropic.rs`
gets rewired: `build_request_body*` call sites construct a
`mu_anthropic::MessagesRequest` (via the mu-side `From`) and serialize that;
the SSE parse uses mu-anthropic's `StreamEvent`. The old hand-crafted
functions are then **deleted, not refactored** — their deletion is the
migration's done-signal. mu stays on the old path until that single swap, so
the rewrite never percolates through the rest of mu.

---

## 6. Scar list (each becomes a regression test here)

Wire facts the old file learned the hard way — mu-anthropic must encode these
as types + named tests from day one:

- **mu-yz48:** `usage` sits at the TOP LEVEL of the `message_delta` event,
  sibling to `delta` — NOT nested in `delta`. Reading `delta.usage` returns
  None and freezes `output_tokens` at the message_start baseline (1–5).
- **mu-cache-write-tier-split-umq6:** `usage.cache_creation` is a per-tier
  breakdown object (`ephemeral_5m_input_tokens`, `ephemeral_1h_input_tokens`),
  separate from the flat `cache_creation_input_tokens` total.
- **Tool-result batching:** consecutive `tool_result`s MUST be grouped into a
  single `user` message (Anthropic's tool-use protocol requires it).
- **cache_control needs a BLOCK:** a marked plain-string `user` message must be
  rewritten into a `[{type:text, text, cache_control}]` block array — the
  marker has nowhere to attach on a bare string.
- **Thinking is outbound-stripped:** never echo the model's own reasoning trace
  back as input.
- **`#[serde(other)]` fallbacks:** unknown block/delta `type`s must deserialize
  to an `Other` variant, not error — forward-compat against unmodeled wire
  additions.
- **stop_reason mapping:** `stop_sequence` → end-turn; unknown → warn + treat
  as end-turn. Wire strings: `end_turn`, `tool_use`, `max_tokens`,
  `stop_sequence`.
- **Protocol constant:** `anthropic-version: 2023-06-01`.
