# INTEGRATION-PLAN.md — wiring mu-anthropic into mu

The executable plan to replace mu's hand-rolled Anthropic provider with the
`mu-anthropic` crate. Read `INTEGRATION.md` (the seam map + the *why*) and
`AGENTS.md` (the DAG rule) first — this is the *how*, step by step.

**Done-signal:** the old hand-crafted functions in
`crates/mu-ai/src/providers/anthropic.rs` (`build_request_body*`, `translate_*`,
the inline `AnthropicEvent`/`AnthropicUsage`/`BlockBuilder`) are **deleted**, and
mu's request bytes + SSE parsing come from `mu-anthropic` types. Delete, don't
refactor-in-place. mu stays on the old path until the single cutover.

## Ground rules (do not violate — they're why the crate exists)

1. **The `From` lives MU-SIDE, never in mu-anthropic** (DAG: `mu-core → mu-anthropic`,
   never back). `mu-anthropic` imports nothing from mu.
2. **Parity tests are tripwires, not cages.** `anthropic_tests.rs` pins the
   current wire bytes (esp. the `yqeq4_parity_*` Legacy≡Projected invariants). The
   new path must produce the **same wire JSON** on those scenarios; if you change
   the bytes, change them *deliberately* and update the test with a note — never
   silently.
3. **Old file untouched until cutover.** It keeps mu running. No incremental edits
   in place (that approach is what produced the mess).

## The seam (concrete, from the map)

**Stays in mu** (agent-loop + transport — NOT protocol shape):
- `Provider::stream` impl, HTTP POST, headers, cancel channel — `providers/anthropic.rs:110-169`
- SSE byte→line framing — `providers/sse.rs`
- `next_event()` → `ProviderEvent` emission (the agent-loop event contract) — `anthropic.rs:878-1044`
- rope render → `ProviderMessages`, cache strategy — `context/anthropic.rs`
- the call site — `crates/mu-core/src/agent/loop_/invoke.rs:54-59` (`provider.stream(system, MessageInput::Projected(proj), tools, cancel)`)

**Moves to mu-anthropic types** (the wire shapes):
- request build (`build_request_body*`, `translate_*`) → `MessagesRequest` + serialize
- SSE event enum (`AnthropicEvent`) → `mu_anthropic::StreamEvent`
- block accumulation (`BlockBuilder`/`assemble_content`) → `mu_anthropic::accumulate()`
- usage merge (`AnthropicUsage`) → `mu_anthropic::Usage`
- tool input parse (`parse_tool_input`) → `mu_anthropic::JsonValue`

**The mu types the `From` reads** (`mu-core/src/context/renderer.rs`):
`ProviderMessages { messages: Vec<ProviderMessage>, target }`,
`ProviderMessage { role: ProviderRole, content: Arc<str>, cache_marker: Option<CacheMarker>, blocks: Option<Vec<mu ContentBlock>> }`,
`ProviderRole` = System|User|Assistant|ToolResult, `CacheMarker::Ephemeral`, `CacheTtl`.
mu's `ContentBlock` (`mu-core/src/agent/types.rs`) = `Text|ToolCall|Thinking`.
Tools: `ToolSpec { name, description, input_schema: Value }`.

## Phase 0 — dependency

- Add to `crates/mu-ai/Cargo.toml`: `mu-anthropic = { path = "../providers/mu-anthropic" }`.
- Confirm `just ci` still green (no behavior change yet).

## Phase 1 — outbound (request)

Goal: mu's request bytes come from `MessagesRequest`, byte-identical to today on
the parity scenarios.

1. Write the mapping **mu-side** (e.g. a new `providers/anthropic_convert.rs`):
   `fn to_messages_request(pmsgs: &ProviderMessages, tools: &[ToolSpec], ttl: CacheTtl, model: &str, max_tokens: u32, system: Option<&str>) -> mu_anthropic::MessagesRequest`
   (or an idiomatic `From`/`TryFrom` over a small input tuple/struct).
   Port the *logic*, not the JSON, from `translate_provider_messages`
   (anthropic.rs:393-477) + `build_request_body_from_projection` (:560-596) +
   `detect_cache_targets` (:612-642). Carry over these wire facts (already
   encoded as mu-anthropic types/tests — see INTEGRATION.md §6 scar list):
   - **system hoisting**: system-role spans (except tool-schema) → the envelope `system`.
   - **tool-result batching**: consecutive `ToolResult` → one `user` message of `tool_result` blocks.
   - **thinking stripped outbound** (never echo reasoning back).
   - **cache_control per-BLOCK**: a marked bare-string user message becomes a
     `[{type:text,text,cache_control}]` block array; some markers map to envelope
     positions (system / last tool spec) — port `detect_cache_targets`.
   - `ToolSpec` → `mu_anthropic::Tool::new(name, description, input_schema)`;
     `ContentBlock::ToolCall` → `ContentBlock::ToolUse`.
2. Rewire `build_request_body_from_projection` to build the `MessagesRequest` via
   the mapping and `serde_json::to_value` it (keep the same return type for now).
3. **Make the parity tests pass unchanged** (`yqeq4_parity_*`, `build_request_body_*`,
   `translate_tool_spec_*`). They are the proof the new bytes == old bytes. If a
   diff is *correct* (old was wrong), change the fixture deliberately + note why.
4. Add tests: `ProviderMessages → MessagesRequest` mapping (the new seam).

## Phase 2 — inbound (response / streaming)

Goal: SSE parsing + accumulation come from mu-anthropic; the stream driver and
`ProviderEvent` contract stay in mu.

1. Replace the inline `AnthropicEvent` enum with `serde_json::from_str::<mu_anthropic::StreamEvent>`
   on each SSE `data:` line (keep `sse.rs` line framing).
2. Replace `BlockBuilder`/`assemble_content` with `mu_anthropic::accumulate()` —
   OR, if `next_event()`'s incremental `TextDelta` emission must stay (it streams
   deltas to the UI), map each `StreamEvent` to the existing `ProviderEvent`
   incrementally and use mu-anthropic's types only as the parsed shape. (Decide:
   full `accumulate()` vs. incremental mapping. The UI wants live deltas, so
   incremental mapping of `StreamEvent` → `ProviderEvent` is likely; `accumulate()`
   is the non-streaming/get-final-message convenience.)
3. Replace `AnthropicUsage` merge with `mu_anthropic::Usage` (the mu-yz48
   top-level-usage and cache-tier-split scars are already encoded there).
4. `to_usage()` → map `mu_anthropic::Usage` → `mu_core::agent::Usage`.
5. Keep the golden SSE fixtures passing (they pin event→ProviderEvent behavior).

## Phase 3 — delete (the done-signal)

Once Phases 1–2 are green and the smoke tests
(`crates/mu-coding/tests/anthropic_{read,write,ls}_smoke.rs`) pass:
delete `build_request_body*`, `translate_*`, `AnthropicEvent`, `AnthropicUsage`,
`BlockBuilder`, `assemble_content`, `parse_tool_input` from `anthropic.rs`. The
file shrinks to: the `Provider` impl, transport, `next_event` driver, cancel.
That deletion is the migration's done-signal.

## Critical files

| role | path |
|---|---|
| old provider (replace internals) | `crates/mu-ai/src/providers/anthropic.rs` |
| old provider renderer/cache (KEEP) | `crates/mu-ai/src/context/anthropic.rs` |
| parity/contract tests (tripwires) | `crates/mu-ai/src/providers/anthropic_tests.rs` |
| mu internal types (the `From` reads) | `crates/mu-core/src/context/renderer.rs`, `crates/mu-core/src/agent/types.rs` |
| call site (KEEP) | `crates/mu-core/src/agent/loop_/invoke.rs:54` |
| Provider trait / MessageInput / ProviderEvent (KEEP) | `crates/mu-core/src/agent/provider.rs` |
| smoke tests | `crates/mu-coding/tests/anthropic_*_smoke.rs` |
| new wire types | `crates/providers/mu-anthropic/src/{request,content,response,stream,accumulate}.rs` |

## Risks / gotchas

- **The `From` is where the real complexity lands** — system hoisting, tool-result
  batching, per-block cache_control, thinking-strip. Port it carefully from the
  old `translate_provider_*`; the parity tests are the safety net.
- **Don't pull mu types into mu-anthropic** to make the mapping easier. The mapping
  is mu-side, mechanical against mu-anthropic's public types.
- **Incremental deltas vs accumulate()**: the agent loop streams `TextDelta` to the
  UI, so you probably map `StreamEvent`→`ProviderEvent` incrementally rather than
  awaiting `accumulate()`. Decide this in Phase 2 step 2.
- **The drift canary** keeps guarding the wire during/after migration — if Anthropic
  changes something mid-migration, it surfaces as a bead, not a mystery.
