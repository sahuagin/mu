# Spec: OpenRouter provider

| field      | value                                          |
| ---------- | ---------------------------------------------- |
| spec_id    | mu-017                                         |
| status     | ready                                          |
| created    | 2026-05-10                                     |
| updated    | 2026-05-10                                     |
| authors    | tcovert + claude-personal (claude-opus-4.7)    |
| supersedes | mu-015 long-term (text-only OpenAI-OAuth path) |

## Why

Third concrete Provider impl, second HTTP-based one. After this lands:

- Mu has a pi-free path to GPT, Gemini, Llama, and many others —
  OpenRouter routes to most major-model providers behind one API
  with one key.
- The OpenAI-Codex-via-pi path (mu-015) becomes optional rather than
  the only OpenAI option. Long-term, mu-017 supersedes it for
  delegate use cases that don't need OpenAI Pro budget specifically.
- The Provider abstraction gets validated in a third instance.
  Anthropic's wire format (content blocks, tool_use blocks) is
  different enough from OpenAI's (delta.content, delta.tool_calls)
  that any leaks in the abstraction will surface here.

OpenRouter's API is OpenAI-compatible, so this Provider doubles as a
template for a future OpenAI-direct provider (different endpoint and
auth header, same wire format).

CONVENTIONS apply.

## Scope

- **In:**
  - `crates/mu-ai/src/providers/openrouter.rs` — `OpenRouterProvider`
    struct implementing `Provider`. HTTP + API key auth via
    `OPENROUTER_API_KEY`. POSTs to `/api/v1/chat/completions` with
    `stream: true`. Parses OpenAI-format SSE. Translates to
    `ProviderEvent`.
  - `crates/mu-ai/src/lib.rs` — `pub use OpenRouterProvider`.
  - `crates/mu-ai/src/providers/mod.rs` — module declaration.
  - `crates/mu-coding/src/serve/factory.rs` — `"openrouter"` arm in
    `build_provider`.
  - Tool support included (tools field in request, tool_calls
    parsing in response). The Provider abstraction goes end-to-end.
  - Mock-driven unit tests for OpenAI SSE parsing and tool-call
    accumulation.
  - Live integration test gated on `MU_LIVE_OPENROUTER=1`.

- **Out:**
  - Direct OpenAI API (api.openai.com). Different endpoint and auth
    method, but same wire format. Future spec, maybe ~50 LOC of
    delta given mu-017's bones.
  - Model routing strategies (OpenRouter's `route` parameter, fallback
    chains, etc.). v1 sends a fixed model id.
  - OpenRouter-specific extensions (provider preference, cost limits,
    `transforms`, etc.). v1 ignores them.
  - Image content blocks. v1 supports text + tool_use only.
  - `tool_choice: required` / forced-tool selection. v1 leaves
    OpenRouter's default ("auto").

- **Non-goals:**
  - Reusing AnthropicProvider's response state machine. The wire
    formats are structurally different — Anthropic uses content
    blocks with explicit start/delta/stop events; OpenAI uses
    deltas with implicit ordering by index. A single shared parser
    would be more abstract than helpful. v1 has two providers; if
    we add a third HTTP provider with yet another shape, we
    revisit.

## Invariants

- **INV-1 (CONVENTIONS apply).**
- **INV-2 (Same INVs as mu-006/mu-008 for HTTP).** API key holding
  is fine (this is NOT OAuth; the no-token-holding rule applies to
  OAuth tokens specifically). No `unsafe`, no
  `unwrap`/`expect`/`panic` outside `#[cfg(test)]`. Module under
  800 lines.
- **INV-3 (live test gated).** `MU_LIVE_OPENROUTER=1` env var to
  run; CI never spends.
- **INV-4 (text+tools end-to-end).** Unlike mu-015, this provider
  supports tool calls in both directions: tools sent in the request,
  `tool_calls` parsed from the streaming response. The vertical
  slice for OpenRouter+read should work the same as for
  Anthropic+read after this lands.

## Interfaces

```rust
pub struct OpenRouterProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    api_base: String,
}

impl OpenRouterProvider {
    pub fn new(api_key: String, model: String) -> Self;
    pub fn from_env(model: String) -> Result<Self, ProviderError>;
    pub fn with_api_base(mut self, base: String) -> Self;
}
```

Endpoint: `{api_base}/api/v1/chat/completions`. Default
`api_base` = `https://openrouter.ai`.

Headers:
- `Authorization: Bearer {api_key}`
- `Content-Type: application/json`
- (optional) `HTTP-Referer` and `X-Title` per OpenRouter docs for
  attribution. v1 sets `X-Title: mu` so the user's OpenRouter
  dashboard shows mu's traffic separately.

Request body (OpenAI chat-completions shape):

```json
{
  "model": "anthropic/claude-haiku-4.5",
  "messages": [...],
  "tools": [...],
  "stream": true,
  "max_tokens": 4096
}
```

Message translation (mu's AgentMessage → OpenAI):

- `User { content }` → `{"role": "user", "content": <string>}`
- `Assistant(AssistantMessage)` →
  `{"role": "assistant", "content": <text>, "tool_calls": [...]}`
  where text is concatenated text blocks and tool_calls are
  `{"id": ..., "type": "function", "function": {"name": ..., "arguments": <string>}}`
  with arguments serialized as a JSON string.
- `ToolResult { call_id, content, is_error }` → 
  `{"role": "tool", "tool_call_id": call_id, "content": <content>}`
  (OpenAI doesn't have an explicit error field — error info goes in
  the content text).

Tool spec translation (mu's ToolSpec → OpenAI):

```json
{
  "type": "function",
  "function": {
    "name": "read",
    "description": "Read a file...",
    "parameters": {...input_schema...}
  }
}
```

Response SSE format (one chunk per `data: ...` line, terminated by
`data: [DONE]\n\n`):

```json
{
  "id": "...",
  "object": "chat.completion.chunk",
  "choices": [
    {
      "index": 0,
      "delta": {
        "content": "...",          // text incremental
        "tool_calls": [             // tool call incremental, by index
          {
            "index": 0,
            "id": "call_abc",       // first chunk only
            "type": "function",
            "function": {
              "name": "read",       // first chunk only
              "arguments": "..."     // partial JSON, accumulated
            }
          }
        ]
      },
      "finish_reason": "stop" | "tool_calls" | "length" | null
    }
  ]
}
```

Tool calls are streamed by `index`. The first chunk for an index
carries `id`, `function.name`, and the start of `function.arguments`.
Subsequent chunks for the same index append to `function.arguments`.
The arguments are streamed as a JSON string (not parsed JSON).

### Stream state

```rust
struct StreamState {
    sse: Pin<Box<dyn Stream<Item = SseEvent> + Send>>,
    accumulated_text: String,
    /// Indexed by OpenAI's per-call `index`. Each entry accumulates
    /// id, name, and args-JSON-string until the stream ends.
    tool_calls: HashMap<u32, ToolCallBuilder>,
    /// Insertion order of tool_call indexes — preserved so the
    /// final assistant message has tool calls in stream order.
    tool_call_order: Vec<u32>,
    finish_reason: Option<String>,
    cancel_rx: Option<oneshot::Receiver<()>>,
    finished: bool,
    emitted_done: bool,
}

struct ToolCallBuilder {
    id: String,
    name: String,
    args_json: String,
}
```

### Final assembly

On `data: [DONE]` (or stream end), emit `ProviderEvent::Done` with
an `AssistantMessage` whose content is:

- `Text { text: accumulated_text }` if non-empty
- `ToolCall { id, name, arguments }` for each entry in
  `tool_call_order` (in order), with `arguments` parsed from the
  accumulated JSON string

Stop reason mapping:

| OpenAI `finish_reason` | mu `StopReason` |
|------------------------|-----------------|
| `"stop"`               | `EndTurn`       |
| `"tool_calls"`         | `ToolUse`       |
| `"length"`             | `MaxTokens`     |
| any other / null       | `EndTurn` + `tracing::warn!` |

Same fallback pattern as Anthropic for malformed tool-call args
(empty object, `tracing::warn!`).

## Behaviors

1. **B-1 (translate user message):**
   `translate_message(User{"hi"})` → `{"role":"user","content":"hi"}`.

2. **B-2 (translate assistant text-only):**
   Assistant with one Text block → `{"role":"assistant","content":"hi"}`.

3. **B-3 (translate assistant with tool calls):**
   Assistant with one Text block + one ToolCall →
   `{"role":"assistant","content":"text","tool_calls":[{...}]}`.

4. **B-4 (translate tool result):**
   `ToolResult{call_id:"x",content:"out",is_error:false}` →
   `{"role":"tool","tool_call_id":"x","content":"out"}`.

5. **B-5 (translate_tool_spec):**
   `ToolSpec{name:"read",description:"...",input_schema:...}` →
   `{"type":"function","function":{"name":"read",...}}`.

6. **B-6 (build_request_body with tools):** Body includes
   `messages`, `tools` (only when non-empty), `stream: true`,
   `max_tokens`, `model`.

7. **B-7 (SSE → text deltas):** Mock SSE stream with text-only
   chunks → `[TextDelta("hello"), TextDelta(" world"),
   Done(AssistantMessage{content:[Text{"hello world"}], EndTurn})]`.

8. **B-8 (SSE → tool call accumulation):** Mock SSE chunks for one
   tool call streamed across multiple deltas → final ContentBlock
   array contains exactly one ToolCall with id+name+arguments
   correctly accumulated.

9. **B-9 (mixed text + tool_call):** Text streams first, then a
   tool call streams. Final content has both blocks (text first,
   tool_call second) — matches OpenAI's typical ordering.

10. **B-10 (malformed tool args fall back to empty object):** If the
    accumulated tool call's `function.arguments` doesn't parse as
    JSON, fall back to `{}` per the same pattern as AnthropicProvider.

11. **B-11 (HTTP error → ProviderError):** Mock server returning
    HTTP 401 produces `ProviderError::Other` containing the status.

12. **B-12 (live API smoke — gated):** With `MU_LIVE_OPENROUTER=1`
    and `OPENROUTER_API_KEY` set, send a single user message
    `"Reply with the word 'hello' and nothing else."` to a cheap
    model. Verify response contains `"hello"`.

13. **B-13 (live tool round-trip — gated):** Same env gating. Send
    a tool spec for an "echo" function, prompt model to use it.
    Verify response contains a ToolCall with the right name and
    parsed arguments.

## Acceptance

- New file: `crates/mu-ai/src/providers/openrouter.rs`.
- Modified: `crates/mu-ai/src/lib.rs`, `providers/mod.rs`,
  `mu-coding/src/serve/factory.rs`.
- `cargo build` clean.
- `cargo nextest run` passes — every existing test plus B-1..B-11
  (B-12, B-13 skipped without env var).
- With `MU_LIVE_OPENROUTER=1` and `OPENROUTER_API_KEY` set, B-12 and
  B-13 also pass.
- `mu serve --provider openrouter --model anthropic/claude-haiku-4.5`
  works (verified manually).
- Module under 800 lines.

## Out-of-circuit warnings

- **OOC-1:** OpenAI's `function.arguments` is a STRING containing
  JSON, not a JSON object. The model picks a JSON shape and stringifies
  it. So our deserialization is `serde_json::from_str(&accumulated)`,
  not `Value::from(accumulated)`. Same pattern as Anthropic; just
  a reminder.

- **OOC-2:** The `data: [DONE]\n\n` line is a sentinel, not JSON.
  Our SSE parser sees it as `SseEvent { event: None, data: "[DONE]" }`.
  Recognize it specifically and end the stream; don't try to parse
  it as JSON.

- **OOC-3:** OpenRouter's docs recommend setting `HTTP-Referer` for
  attribution. We set `X-Title: mu` so traffic is identifiable in
  the user's OpenRouter dashboard. `HTTP-Referer` is optional;
  v1 omits it (we don't have a meaningful referrer URL).

- **OOC-4:** Tool call streaming order in OpenAI: chunks for index
  0 may interleave with chunks for index 1 if the model emits two
  tool calls in parallel. The accumulator HashMap-by-index handles
  this; just don't assume sequential. The `tool_call_order` Vec
  preserves the order in which indexes first appeared.

- **OOC-5:** `finish_reason` arrives ON THE FINAL CHUNK alongside
  an empty delta, not as its own event. Make sure to record it
  during normal chunk processing, not in a special `[DONE]`
  handler.

## Prior work / context

- mu-006 — AnthropicProvider (the structural template; HTTP +
  SSE + state machine).
- mu-008 — Anthropic tool support (tool spec translation + parsing
  the streaming response, different shape).
- mu-015 — OpenAI-Codex via pi subprocess (the path this supersedes
  for non-Pro-budget OpenAI use).
- OpenAI Chat Completions docs + OpenRouter docs.

## Changelog

- 2026-05-10 — initial draft (claude-personal).
