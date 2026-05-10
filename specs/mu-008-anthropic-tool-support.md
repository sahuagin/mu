# Spec: AnthropicProvider tool support

| field      | value                                          |
| ---------- | ---------------------------------------------- |
| spec_id    | mu-008                                         |
| status     | ready                                          |
| created    | 2026-05-10                                     |
| updated    | 2026-05-10                                     |
| authors    | tcovert + claude-personal (claude-opus-4.7)    |
| supersedes | none                                           |

## Why

Vertical-slice prerequisite. After mu-008 lands, `AnthropicProvider`
can both **request** tool use (sending tool descriptors in the
request) and **receive** tool calls (parsing `tool_use` content
blocks from the response stream). mu-009 then wires the `read` tool
(mu-007) into `mu serve`'s session creation; mu-010 verifies the
full pipe — `mu ask --provider anthropic-api --tools read "what's in
/etc/hostname?"` → real Claude → tool call → mu reads file → result
back to Claude → final answer.

Today (mu-006 v1), `AnthropicProvider` ignores its `tools` argument
and never produces `ContentBlock::ToolCall` in the final assistant
message. mu-008 closes both loops.

## Scope

- **In:**
  - `crates/mu-ai/src/providers/anthropic.rs` — extend:
    - `translate_tool_spec(&ToolSpec) -> Value` for the request side.
    - `translate_messages(&[AgentMessage]) -> Vec<Value>` replacing
      internal use of `translate_message`. This handles the
      consecutive-`ToolResult`-grouping that Anthropic's API expects:
      multiple tool results from one assistant turn must be batched
      into a single user message with multiple `tool_result` blocks.
    - Update `build_request_body` to include `tools` when non-empty
      (omit the field entirely when empty — Anthropic accepts that).
    - Add `tool_result` content-block handling to message translation
      so `AgentMessage::ToolResult` round-trips back to Anthropic.
    - Refactor `StreamState` from a single `accumulated_text: String`
      to a per-block builder (`BlockBuilder` enum tracking either
      text or tool_use accumulation, indexed by Anthropic's block
      index). Required because text and tool_use blocks can interleave.
    - Update `next_event` to handle `content_block_start` with
      `content_block.type = "tool_use"`, `content_block_delta` with
      `delta.type = "input_json_delta"`, and `content_block_stop`.
    - Build the final `AssistantMessage::content` from accumulated
      blocks in document order.
  - Tests covering: tool spec translation, message-grouping on
    consecutive tool results, request body shape with tools, SSE →
    ContentBlock::ToolCall translation, mixed text+tool_use response.
  - Optionally split tests to `anthropic_tests.rs` if file approaches
    800 lines (per CONVENTIONS).
  - Live integration test extension: also gated on
    `MU_LIVE_ANTHROPIC=1`. Sends a tool spec; verifies real Claude
    returns a tool_use block we can parse.

- **Out:**
  - `ProviderEvent::ToolCallDelta` emission during streaming. The
    `ProviderEvent` variant exists, but mu-003's loop ignores it
    (the loop only consumes the complete tool calls in
    `ProviderEvent::Done`). Don't emit incremental tool-call events
    in v1; future spec adds this when a frontend (TUI) wants the
    streaming view.
  - Anthropic's `tool_choice` parameter (forcing a specific tool,
    forcing any tool, forcing none). Future spec.
  - Prompt caching (`cache_control` field). Future spec.
  - Image / document content in tool results. v1 supports text
    content only.
  - Multiple parallel tool calls per turn from the loop side. Loop
    handles them sequentially (mu-003 §Out / §B-2).

- **Non-goals:**
  - 1:1 fidelity with Anthropic's full content-block surface. We map
    the subset needed to support text + tool_use. New surfaces
    arrive as their own specs.

## Invariants

- **INV-1 (CONVENTIONS apply).** Per
  `specs/delegations/CONVENTIONS.md`. Output envelope, verification
  ritual, no new workspace deps, etc.
- **INV-2 (file size).** Per CONVENTIONS Rule 1. If
  `anthropic.rs` would exceed ~800 lines, extract tests to
  `anthropic_tests.rs` (using
  `#[cfg(test)] #[path = "anthropic_tests.rs"] mod tests;`).
- **INV-3 (tool-result grouping).** Multiple consecutive
  `AgentMessage::ToolResult` messages MUST be batched into a single
  Anthropic user message with multiple `tool_result` content blocks.
  Why: Anthropic's API rejects (or behaves unexpectedly with) a
  sequence of separate user messages where each contains a single
  tool result. Test §B-3 pins this.
- **INV-4 (block ordering preserved).** The final
  `AssistantMessage::content` order matches the order in which
  Anthropic emitted `content_block_start` events. Tests §B-6 pin
  this for mixed text+tool_use responses.
- **INV-5 (no JSON parse panics).** If `input_json_delta` accumulates
  to invalid JSON (provider bug, network glitch, etc.), the
  resulting tool call's `arguments` field becomes
  `Value::Object({})` (empty object) and the tool call still appears
  in `AssistantMessage::content` with the raw accumulated string in
  a `tracing::warn!` log. Don't emit a `ProviderEvent::Error`; let
  the agent loop attempt to call the tool and handle the failure
  there.
- **INV-6 (request omits empty tools).** When the `tools` argument
  is empty, the request body MUST NOT include a `"tools"` field.
  Why: Anthropic's API accepts both omission and `[]`, but omission
  is the documented "no tools" form and avoids any chance of
  edge-case behavior with `[]`.

## Interfaces

### Request side: tool spec translation

```rust
pub(crate) fn translate_tool_spec(spec: &ToolSpec) -> Value {
    json!({
        "name": spec.name,
        "description": spec.description,
        "input_schema": spec.input_schema,
    })
}
```

### Request side: message translation with tool-result grouping

```rust
pub(crate) fn translate_messages(messages: &[AgentMessage]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut tool_result_buf: Vec<Value> = Vec::new();

    for m in messages {
        match m {
            AgentMessage::ToolResult { call_id, content, is_error } => {
                tool_result_buf.push(json!({
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": content,
                    "is_error": is_error,
                }));
            }
            other => {
                if !tool_result_buf.is_empty() {
                    out.push(json!({
                        "role": "user",
                        "content": std::mem::take(&mut tool_result_buf),
                    }));
                }
                if let Some(translated) = translate_message_single(other) {
                    out.push(translated);
                }
            }
        }
    }

    if !tool_result_buf.is_empty() {
        out.push(json!({
            "role": "user",
            "content": tool_result_buf,
        }));
    }

    out
}

/// Single-message translation for non-ToolResult variants. ToolResult
/// is handled by `translate_messages` because of the grouping
/// requirement (INV-3).
fn translate_message_single(m: &AgentMessage) -> Option<Value> {
    match m {
        AgentMessage::User { content } => Some(json!({
            "role": "user",
            "content": content,
        })),
        AgentMessage::Assistant(a) => {
            let blocks: Vec<Value> = a.content.iter().filter_map(|b| match b {
                ContentBlock::Text { text } => Some(json!({ "type": "text", "text": text })),
                ContentBlock::ToolCall(tc) => Some(json!({
                    "type": "tool_use",
                    "id": tc.id,
                    "name": tc.name,
                    "input": tc.arguments,
                })),
                ContentBlock::Thinking { .. } => None, // future
            }).collect();
            if blocks.is_empty() {
                None
            } else {
                Some(json!({ "role": "assistant", "content": blocks }))
            }
        }
        AgentMessage::ToolResult { .. } => None, // handled in translate_messages
    }
}
```

The existing public `translate_message` function (currently called
only by the in-file `build_request_body`) is replaced by these.
Outside callers don't reference it (it's `pub(crate)`); the rename
is internal. Tests that referenced `translate_message` directly
update to call `translate_messages`.

### Request side: build_request_body

```rust
pub(crate) fn build_request_body(
    model: &str,
    messages: &[AgentMessage],
    tools: &[ToolSpec],
) -> Value {
    let api_messages = translate_messages(messages);
    let mut body = json!({
        "model": model,
        "max_tokens": 4096,
        "stream": true,
        "messages": api_messages,
    });
    if !tools.is_empty() {
        body["tools"] = json!(tools.iter().map(translate_tool_spec).collect::<Vec<_>>());
    }
    body
}
```

### Response side: StreamState refactor

```rust
enum BlockBuilder {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input_json: String,
    },
}

struct StreamState {
    sse: Pin<Box<dyn Stream<Item = SseEvent> + Send>>,
    /// Accumulating content blocks indexed by Anthropic's `index`.
    blocks: std::collections::HashMap<u32, BlockBuilder>,
    /// Order in which blocks first appeared. Used to assemble the
    /// final assistant message's content vec in the same order.
    block_order: Vec<u32>,
    stop_reason: Option<String>,
    cancel_rx: Option<oneshot::Receiver<()>>,
    finished: bool,
    emitted_done: bool,
}
```

### Response side: next_event handling

```rust
// content_block_start with text:
//   - blocks.insert(index, BlockBuilder::Text(empty)); block_order.push(index)
// content_block_start with tool_use { id, name }:
//   - blocks.insert(index, BlockBuilder::ToolUse { id, name, input_json: "" });
//     block_order.push(index)
// content_block_delta with text_delta { text }:
//   - append to blocks.get_mut(index).Text
//   - emit ProviderEvent::TextDelta(text)
// content_block_delta with input_json_delta { partial_json }:
//   - append to blocks.get_mut(index).ToolUse.input_json
//   - DO NOT emit ProviderEvent::ToolCallDelta in v1 (§Out)
// content_block_stop:
//   - no-op for v1; the block is finalized at message_stop
// message_stop:
//   - assemble final AssistantMessage from blocks in block_order
//   - emit ProviderEvent::Done(message)
```

When assembling the final message, ToolUse blocks parse their
accumulated `input_json` via `serde_json::from_str`. On parse error,
fall back to `Value::Object(Map::new())` per INV-5, log a warning.

## Behaviors

1. **B-1 (translate_tool_spec):** A `ToolSpec { name: "read", description: "...", input_schema: ... }` translates to `{"name":"read","description":"...","input_schema":...}`. Direct unit test on the function.

2. **B-2 (translate_messages preserves non-tool-result order):** A sequence `[User, Assistant, User, Assistant]` translates to four messages in the same order, each with the right role.

3. **B-3 (consecutive tool results group):** A sequence `[User, Assistant(tool_call A; tool_call B), ToolResult A, ToolResult B, Assistant]` translates to four Anthropic messages: user, assistant (with two tool_use blocks), **one** user (with two tool_result blocks in input order), assistant. Verify the third message has exactly two `tool_result` blocks and `role: "user"`.

4. **B-4 (build_request_body with tools):** With one ToolSpec and one user message, the request body has `body["tools"]` as a one-element array AND `body["messages"]` as a one-element array.

5. **B-5 (build_request_body without tools):** With empty `&[]` tools, the request body has NO `tools` key (`body.get("tools").is_none()`). Per INV-6.

6. **B-6 (SSE → mixed content blocks):** Feed an SSE sequence containing:
   - block 0: text "I will read it. "
   - block 1: tool_use with id="toolu_X", name="read", and input_json delta `'{"path":"/etc/hostname"}'`
   The final ProviderEvent::Done's AssistantMessage.content has TWO blocks in order: `Text { text: "I will read it. " }` then `ToolCall { id: "toolu_X", name: "read", arguments: {"path":"/etc/hostname"} }`.

7. **B-7 (SSE → tool_use only):** Without any text block, the final content has just the ToolCall.

8. **B-8 (input_json malformed → empty object, no panic):** Feed input_json_delta with content `'{not valid}'`. The final ToolCall's `arguments` is `{}` (empty object). No panic; a `tracing::warn!` is logged (asserting on log output is optional — the no-panic and empty-object behavior is the contract).

9. **B-9 (live API tool round-trip — gated on MU_LIVE_ANTHROPIC=1):** Send a tool spec for a fake "echo" tool with a simple schema, prompt "Use the echo tool with the text 'hi'". Receive a response. Verify: at least one ToolCall block in the final content; its name is "echo"; its arguments parse as a JSON object containing the text "hi" somewhere. Skips silently when the env var isn't set.

## Acceptance

- Modified file: `crates/mu-ai/src/providers/anthropic.rs` (and
  optionally `crates/mu-ai/src/providers/anthropic_tests.rs` if
  tests get split).
- `cargo build -p mu-ai` clean.
- `cargo nextest run -p mu-ai` passes — every existing mu-ai test
  plus B-1..B-8. (B-9 skipped without env var.)
- `cargo nextest run` passes — every workspace test still green.
- File sizes per INV-2 / CONVENTIONS.

## Iteration-aware handoff

This spec splits naturally into request-side (translate_*,
build_request_body) and response-side (StreamState, next_event)
work. If a sub-agent hits its budget mid-task, the natural break is
between the two sides. Request side first (it's mechanical); then
response side. Tests can be written progressively as each side
lands.

The split also maps to a delegation-vs-claude split:

- **Request side** (translate_tool_spec, translate_messages,
  build_request_body update, plus their unit tests) is mechanical
  and fits a delegation. ~150 LOC. gpt-pro candidate.

- **Response side** (StreamState refactor, next_event handler
  changes, mixed-block test, malformed-JSON test, live test) has
  real judgment calls (the StreamState shape, error handling
  policy). Claude does this.

I'm not splitting them into separate spec files because the request
and response sides reference the same types and conventions; one
spec, two phases of implementation makes the contract clearer.

## Open questions

- [ ] OQ-1: Should the live test (B-9) use a real provider tool the
  agent can actually invoke, or just a "describe a tool you would
  call but don't actually invoke" prompt? — owner: claude — resolution:
  the test only verifies tool_use parsing, not actual execution. The
  agent loop isn't involved. So describe a fake tool, prompt Claude
  to use it, parse the response. Real wiring is mu-009/mu-010.
- [ ] OQ-2: Should `is_error: false` be omitted from `tool_result`
  blocks (Anthropic accepts both)? — owner: defer — resolution: send
  it always. Explicit > implicit on the wire.
- [ ] OQ-3: What if the provider emits a `tool_use` block with
  `input_json` that's a valid JSON value but not an object (e.g., an
  array)? — owner: claude — resolution: per INV-5, fall back to empty
  object. The agent loop expects `arguments` to be an object that
  can be passed to `Tool::execute`; a non-object value would confuse
  Tool impls. Log a warning and proceed.

## Out-of-circuit warnings

- **OOC-1:** `serde_json::from_str(&accumulated_input)` requires
  `&str`, not `&String`. The accumulated input is a `String`; pass
  it as `accumulated.as_str()` or `&accumulated`. Compiler will
  catch but worth noting.
- **OOC-2:** Anthropic's tool_use blocks have `input` in the
  CONTENT BLOCK (the assistant's own format), but `tool_result`
  blocks (the user's response) refer to it via `tool_use_id`, not
  `id`. Different field names for the same concept. Easy to confuse
  in `translate_messages`. Pin via tests B-3.
- **OOC-3:** When `block_order` is empty at message_stop (e.g.,
  Anthropic sent zero content blocks somehow — shouldn't happen but
  network), the resulting AssistantMessage has empty content. Don't
  panic; emit a Done with empty content.
- **OOC-4:** The existing `translate_message` test in mu-006 calls
  `translate_message(&AgentMessage::ToolResult { ... })` and
  expects `None`. After mu-008, `translate_message_single` returns
  `None` for ToolResult (preserved behavior); the existing test
  still passes. But the test's name might suggest it's testing the
  whole-message translation — leave the test as-is, or rename to
  `translate_message_single_skips_tool_result_in_v1`. Implementer's
  choice.

## Prior work / context

- mu-006 — initial AnthropicProvider (text-only, this spec extends it).
- mu-007 — `read` tool, the first concrete Tool. mu-009 wires it
  through using mu-008's machinery.
- mu-003 — agent loop, especially the ProviderEvent enum and the
  ContentBlock::ToolCall variant we're now producing.
- Anthropic API tool-use docs:
  https://docs.anthropic.com/en/docs/agents-and-tools/tool-use/overview
- The `pi_agent_rust` codebase has tool-use parsing in
  `src/providers/anthropic.rs` if our SSE parsing turns out to have
  undocumented quirks; consult, don't copy.

## Changelog

- 2026-05-10 — initial draft (claude-personal).
