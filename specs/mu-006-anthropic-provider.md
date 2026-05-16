# Spec: Anthropic API Provider

| field      | value                                          |
| ---------- | ---------------------------------------------- |
| spec_id    | mu-006                                         |
| status     | ready                                          |
| created    | 2026-05-10                                     |
| updated    | 2026-05-10                                     |
| authors    | tcovert + claude-personal (claude-opus-4.7)    |
| supersedes | none                                           |

## Why

First real LLM integration. After this lands, `mu serve` running
with `AnthropicProvider` + a real `ANTHROPIC_API_KEY` produces actual
Claude responses — not echoed input from `FauxProvider`. From the
user's perspective, that's the moment mu becomes a working agent.

Scoped tight: text-only responses, no tool calls, no extended
thinking. Tool support is a follow-up amendment after we have at
least one Tool implementation (mu-007).

## Scope

- **In:**
  - **`crates/mu-ai/src/providers/mod.rs`** — module root for
    concrete Provider implementations.
  - **`crates/mu-ai/src/providers/anthropic.rs`** — `AnthropicProvider`
    struct implementing `Provider`. Direct API access via
    `ANTHROPIC_API_KEY`. POSTs to `/v1/messages` with `stream: true`.
    Parses SSE events. Translates to `ProviderEvent`.
  - **`crates/mu-ai/src/providers/sse.rs`** — minimal SSE event-stream
    parser. Provides a `read_event` async fn that consumes from a
    `Stream<Item = Result<Bytes>>` and yields `SseEvent { event:
    Option<String>, data: String }`. ~80 LOC.
  - **`crates/mu-ai/src/lib.rs`** — `pub mod providers;` and
    `pub use providers::anthropic::AnthropicProvider;`.
  - **`crates/mu-ai/Cargo.toml`** — already has `reqwest`. No new deps.
  - Mock-driven unit tests for SSE parsing and event translation
    (no network).
  - One **live integration test** gated behind the
    `MU_LIVE_ANTHROPIC=1` env var. Defaults to skip; runs only when
    explicitly enabled. CI-friendly.

- **Out:**
  - Tool support (`tools` field in the API request, tool_use SSE
    events in the response). Future spec — adds on top of this once
    mu-007 has at least one Tool to test with.
  - Extended thinking (the `thinking` parameter, `thinking_delta`
    events). Future spec.
  - Image/document content blocks. Future spec.
  - Anthropic OAuth (Max5 plan via subprocess). Different spec —
    that path shells out to `claude --print` per the no-token-holding
    rule in AGENTS.md.
  - Bedrock variant. Future spec.
  - Retry/backoff on transient errors. v1 errors out on first failure.
  - Full message-history Compaction handling. v1 sends the whole
    `messages` slice the loop provides; the daemon's session
    accumulates context across turns naturally.

- **Non-goals:**
  - 1:1 fidelity with the Anthropic API spec. We map enough of the
    surface to support text generation and feed the right shape
    into mu-003's loop. Edge cases (refusals, content filtering,
    multi-modal) are best-effort or deferred.

## Invariants

- **INV-1 (no token holding via subprocess):** This Provider HOLDS
  an API key (in env). That's NOT the no-third-party-OAuth-token
  guardrail — API keys are a different thing. The OAuth guardrail
  applies to subprocess-wrapper providers (anthropic-oauth,
  openai-oauth), which are separate specs.
- **INV-2 (no unsafe, no unwrap/expect/panic outside tests):**
  Standard.
- **INV-3 (no new workspace deps):** `reqwest` and its
  `rustls-tls` + `json` + `stream` features are already on. `bytes`
  comes in transitively via reqwest. `eventsource-stream` is NOT
  added — we hand-roll the SSE parser (it's ~80 lines and avoids
  yet another dep).
- **INV-4 (file size):** Each file under 800 lines.
- **INV-5 (live test gated):** The integration test that hits real
  Anthropic skips silently unless `MU_LIVE_ANTHROPIC=1`. CI must
  not accidentally rack up spend.

## Interfaces

### `crates/mu-ai/src/providers/anthropic.rs` (sketch)

```rust
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use mu_core::agent::{
    AgentMessage, AssistantMessage, ContentBlock, Provider, ProviderError,
    ProviderEvent, StopReason, ToolSpec,
};

const ANTHROPIC_API_BASE: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    api_base: String,
}

impl AnthropicProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model,
            api_base: ANTHROPIC_API_BASE.to_string(),
        }
    }

    /// Convenience: API key from `ANTHROPIC_API_KEY`. Fails if unset.
    pub fn from_env(model: String) -> Result<Self, ProviderError> {
        let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
            ProviderError::Other("ANTHROPIC_API_KEY not set".into())
        })?;
        Ok(Self::new(api_key, model))
    }

    /// Test hook: override the API base URL.
    pub fn with_api_base(mut self, base: String) -> Self {
        self.api_base = base;
        self
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn stream(
        &self,
        messages: &[AgentMessage],
        _tools: &[ToolSpec],  // v1: ignored; tool support is a future amendment
        cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let body = build_request_body(&self.model, messages);
        let resp = self
            .client
            .post(format!("{}/v1/messages", self.api_base))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Other(format!("anthropic request: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Other(format!(
                "anthropic returned {status}: {text}"
            )));
        }

        let byte_stream = resp.bytes_stream();
        Ok(events_stream(byte_stream, cancel_rx))
    }
}

fn build_request_body(model: &str, messages: &[AgentMessage]) -> Value {
    let api_messages: Vec<Value> = messages
        .iter()
        .filter_map(translate_message)
        .collect();
    json!({
        "model": model,
        "max_tokens": 4096,
        "stream": true,
        "messages": api_messages,
    })
}

fn translate_message(m: &AgentMessage) -> Option<Value> { /* user/assistant only for v1 */ }

fn events_stream(
    bytes: impl futures::Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
    cancel_rx: oneshot::Receiver<()>,
) -> BoxStream<'static, ProviderEvent> {
    /* See §Behaviors B-2..B-4 for the parsing/translation logic. */
}
```

### `crates/mu-ai/src/providers/sse.rs` (sketch)

```rust
use bytes::Bytes;
use futures::Stream;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Parse a stream of bytes as SSE events. Returns a stream of
/// SseEvents. Handles partial chunks (events spanning multiple
/// `Bytes`).
///
/// SSE format: each event is a sequence of `field: value\n` lines
/// terminated by a blank line. Fields we care about: `event` and
/// `data`. Multi-line `data` is concatenated with `\n`.
pub fn parse_sse<S>(bytes: S) -> impl Stream<Item = SseEvent>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
{
    /* implementation */
}
```

## Behaviors

1. **B-1 (translate user message):** `translate_message(User { content: "hi" })` produces `{"role": "user", "content": "hi"}`. Tested via direct call.

2. **B-2 (translate assistant message):** `translate_message(Assistant(AssistantMessage { content: [Text { "hi" }], ... }))` produces `{"role": "assistant", "content": [{"type":"text","text":"hi"}]}`. Tested via direct call.

3. **B-3 (SSE parser handles multi-chunk events):** Feed
   `parse_sse` two `Bytes` chunks where the boundary splits an event
   in the middle (e.g., first chunk: `event: foo\ndata: par`, second
   chunk: `tial\n\n`). Yields exactly one event with
   `event: Some("foo")`, `data: "partial"`.

4. **B-4 (SSE event translation: text deltas):** Given a sequence of
   SSE events matching Anthropic's documented shape:
   - `content_block_start` with `content_block.type = "text"`
   - `content_block_delta` with `delta.type = "text_delta"` and
     `delta.text = "hello"`
   - `content_block_delta` with `delta.text = " world"`
   - `content_block_stop`
   - `message_delta` with `delta.stop_reason = "end_turn"`
   - `message_stop`
   The translator yields, in order:
   `[TextDelta("hello"), TextDelta(" world"),
   Done(AssistantMessage { content: [Text { "hello world" }],
   stop_reason: EndTurn })]`.

5. **B-5 (HTTP error → ProviderError):** A test using
   `wiremock` (NOT a new dep — actually we'll just use a
   manually-controlled mock-server pattern with `reqwest` and a
   `tokio::net::TcpListener`) returns HTTP 401. The Provider's
   `stream()` returns `Err(ProviderError::Other(...))` containing
   the status code. Skip the wiremock dep — we'll mock by spinning
   up a one-shot `tokio::net::TcpListener` that serves the response
   we want for one connection. ~30 lines of test infra.

6. **B-6 (cancel during stream terminates promptly):** When
   `cancel_rx` fires mid-stream, the resulting BoxStream drops in
   bounded time (test uses `tokio::time::timeout(500ms)`).

7. **B-7 (live API smoke — gated):** `#[tokio::test]` that, when
   `MU_LIVE_ANTHROPIC=1` is set in env, calls
   `AnthropicProvider::from_env("claude-haiku-4-5-20251001")`,
   sends a single user message "Reply with the word 'hello' and
   nothing else.", drains the resulting stream, asserts the final
   `Done` payload's text contains "hello". Defaults to **skipped**
   when the env var isn't set; CI never spends.

## Acceptance

- New files at the paths in §Scope.
- Modified files: `crates/mu-ai/src/lib.rs` only (re-exports).
- `cargo build` clean.
- `cargo nextest run` passes — every existing test plus B-1..B-6
  (B-7 skips silently). 51 + ~6 = 57 minimum.
- With `MU_LIVE_ANTHROPIC=1 cargo nextest run --test ...`, B-7 also
  passes.
- No new workspace deps in `Cargo.toml` root.

## Iteration-aware handoff

Claude implements solo. If reqwest streaming has any FreeBSD-specific
quirk that can't be resolved in 2 attempts, fall back to a non-
streaming POST that buffers the whole response, parse SSE from a
`Bytes`, emit events all at once. Document the fallback as a future
optimization spec.

## Open questions

- [ ] OQ-1: `max_tokens: 4096` is hardcoded; should it be configurable
  per session? — owner: defer — resolution: hardcoded for v1; add
  to `AgentConfig` later.
- [ ] OQ-2: Should `system` prompt be a separate field on the
  `Provider::stream` call (mu-003's API doesn't currently take one)?
  — owner: defer — resolution: no for v1; system messages can be
  included in the messages array as the first user message (kludgy
  but works). Real fix: amend mu-003's Provider trait to take an
  optional `system: &str` parameter.
- [ ] OQ-3: How do we map Anthropic's `stop_reason: "max_tokens"`,
  `"stop_sequence"`, etc. to mu's `StopReason` enum? — owner:
  resolved in §B-4 by mapping `end_turn` → `EndTurn`,
  `tool_use` → `ToolUse`, `max_tokens` → `MaxTokens`, anything else
  → `EndTurn` with a `tracing::warn!`.

## Out-of-circuit warnings

- **OOC-1:** `reqwest::Response::bytes_stream()` returns
  `impl Stream<Item = reqwest::Result<Bytes>>`. The lifetime is tied
  to the `Response`, which we own. We need to consume the stream
  inside our own `BoxStream<'static, ProviderEvent>` — that means
  capturing the `Response` (and thus the `bytes_stream`) inside a
  closure and using `async-stream` or `futures::stream::unfold`. The
  spec uses `unfold` to avoid the `async-stream` dep.
- **OOC-2:** Anthropic's SSE format puts every event on its own
  line as `data: { ... json ... }`. The `event:` field tells you
  the event type (`content_block_start`, `content_block_delta`,
  `message_stop`, etc.); the `data` field is the JSON payload. Our
  parser must handle BOTH (`event` is metadata; `data` is the
  payload).
- **OOC-3:** Anthropic returns errors mid-stream as a special
  `error` event (with `data: {"type":"error","error":{...}}`). v1
  treats any `error` event as a terminal error: emit
  `ProviderEvent::Error("anthropic stream error: ...")` and end.

## Prior work / context

- mu-003 — `Provider` trait this implements.
- mu-004 — `FauxProvider` as the reference Provider impl.
- Anthropic API streaming docs:
  https://docs.anthropic.com/en/api/messages-streaming
- The `pi_agent_rust` codebase has a working Anthropic streaming
  impl in `src/providers/anthropic.rs` if the SSE parsing turns out
  to have undocumented quirks; consult, don't copy.

## Amendments

### A-1 (mu-r94): Distinguish clean message_stop from EOF-without-message_stop

Added `StopReason::DegradedEof` to signal SSE stream termination without a
terminal `message_stop` event. This distinguishes connection drops / upstream
truncations from normal completion.

**Behavior change:** When the SSE stream closes without `message_stop`, the
provider now emits `Done(AssistantMessage { stop_reason: DegradedEof, ... })`.
Previously, it would emit a mapped stop_reason from the last `message_delta`
event (often `EndTurn`), obscuring the degraded condition.

**Downstream impact:** Consumers of ProviderEvent now see `stop_reason:
DegradedEof` for truncated/dropped streams. This is observable in the event log
and can be used to surface warnings (TUI callout, metrics, retry policy).

## Changelog

- 2026-05-10 — initial draft (claude-personal).
- 2026-05-16 — amended (mu-r94): add DegradedEof signal for EOF-without-message_stop.
