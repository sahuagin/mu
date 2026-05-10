# Spec: Rewire OpenaiCodexProvider to use stored OAuth tokens

| field      | value                                          |
| ---------- | ---------------------------------------------- |
| spec_id    | mu-019                                         |
| status     | ready                                          |
| created    | 2026-05-10                                     |
| updated    | 2026-05-10                                     |
| authors    | tcovert + claude-personal (claude-opus-4.7)    |
| supersedes | partial — replaces mu-015 internals; preserves the public `OpenaiCodexProvider` type name |

## Why

mu-015 implemented `OpenaiCodexProvider` as a subprocess wrapper around
`pi --provider openai-codex` because mu didn't hold OAuth tokens
itself. mu-018 fixed that: tokens now live in
`~/.config/mu/auth/openai-codex.json` (0600) with a refresh path.
mu-019 cuts the pi subprocess loose and talks to OpenAI Codex
directly over HTTP, gaining streaming, tool support, and active
cancel — the same surface OpenRouter already has (mu-017).

After mu-019, `mu` is a fully standalone agent for OpenAI Codex.
The pi dependency in this codepath disappears. Anthropic remains
subprocess-wrapped (ToS, per AGENTS.md INV-2).

CONVENTIONS apply.

## Scope

- **In:**
  - Rewrite `crates/mu-ai/src/providers/openai_codex.rs` to call
    `https://chatgpt.com/backend-api/codex/responses` via `reqwest`,
    with `Authorization: Bearer <access_token>` and the required
    `chatgpt-account-id` / `originator` headers.
  - **JWT claim extraction.** The access_token from mu-018 is a JWT
    whose payload contains the
    `https://api.openai.com/auth.chatgpt_account_id` claim. mu-019
    decodes the middle segment and pulls that claim per-request. No
    signature verification — we're trusting our own stored token.
  - **Token loading.** `OpenaiCodexProvider::from_store()` reads the
    persisted bundle via `FileSystemTokenStore`. Fails clean ("not
    logged in; run `mu login --provider openai-codex`") if the file
    is missing.
  - **Token refresh.** A 401 response triggers exactly one refresh
    attempt: call `auth::openai_codex::refresh_access_token`, persist
    the new bundle (if not ephemeral), re-extract account-id from
    the new access_token, retry the request once. A second 401 gives
    up with a clear error.
  - **Responses API streaming.** Parse SSE events (reuses
    `providers::sse::SseStream`) and map to `ProviderEvent`:
    - `response.output_text.delta` → `TextDelta`
    - `response.output_item.added` (function_call type) → start a
      tool-call accumulator keyed by `item_id`
    - `response.function_call.arguments.delta` → `ToolInputDelta`
    - `response.output_item.done` (function_call type) → finalize
      that accumulator into a `ContentBlock::ToolUse`
    - `response.completed` → `Done(AssistantMessage)` with
      `StopReason::EndTurn` or `StopReason::ToolUse`
    - `error` event → `ProviderEvent::Error`
  - **Tool support.** Pass the supplied `&[ToolSpec]` through to the
    Responses API `tools` field in the OpenAI function-tool shape:
    `[{"type": "function", "name": ..., "description": ...,
    "parameters": <schema>}]`.
  - **Cancel.** `cancel_rx` aborts the in-flight reqwest stream —
    dropping the response future closes the connection (unlike the
    subprocess version's best-effort cancel).
  - **`--ephemeral` flag** on `mu ask` / `mu serve` for
    `openai-codex` provider: load token from store but don't persist
    refreshed tokens back. Useful for CI / one-off shells.
  - Rewrite mu-015's live smoke test against the new implementation.
    Same env var gate: `MU_LIVE_OPENAI_CODEX=1`.

- **Out:**
  - **`openai-api` provider** (direct API-key auth against
    `api.openai.com`). Separate future spec — different endpoint,
    different model availability, different billing surface.
  - **Multi-account selection.** Whatever account the JWT identifies
    is what we use. Account-switching is a future UX.
  - **Server-Sent Event reconnection.** If the connection drops
    mid-stream, mu-019 returns `ProviderEvent::Error`. Reconnect /
    resume is a future hardening spec.
  - **Reasoning summary events.** Codex emits
    `response.reasoning_summary.delta` events when reasoning is
    enabled. mu-019 ignores them. A future spec can surface them as
    `callout`s (mu-016) under kind `"thinking"`.
  - **Token introspection commands.** "`mu whoami`" / "`mu token
    info`" would be useful but is a future spec.
  - **`pi` subprocess fallback.** Hard cutover. If pi is desired,
    the user can still run pi standalone — mu won't reach for it.

## Invariants

- **INV-1 (CONVENTIONS apply).**
- **INV-2 (Anthropic stays subprocess-wrap per AGENTS.md).** mu-019
  does not touch the Anthropic provider.
- **INV-3 (token never in logs).** The redacting `Debug` from mu-018
  applies; mu-019 must not bypass it. `tracing::*` calls that
  reference the token go through `OAuthToken`'s `Debug`, not the raw
  string. Request bodies/responses logged at TRACE level must redact
  the Authorization header.
- **INV-4 (`chatgpt-account-id` header is non-optional).** Every
  request to `chatgpt.com/backend-api` carries this header. Missing
  it produces a 401 from the backend that no refresh will fix.
- **INV-5 (single refresh attempt per request).** 401 → refresh →
  retry once. No retry loops, no exponential backoff for auth
  errors. If the second attempt also 401s, return `ProviderError`
  with "credentials rejected after refresh; run `mu login` again".
- **INV-6 (refresh tokens may rotate).** The bundle returned from
  `refresh_access_token` MUST be persisted in full (assuming not
  ephemeral), not just the access_token. The refresh_token field in
  the persisted bundle is the source of truth for the next refresh.
- **INV-7 (cancel is prompt).** Drop-the-future cancel must close
  the TCP connection within ~100ms of `cancel_rx` firing. No leaving
  half-open SSE streams behind.

## Interfaces

### `mu-ai/src/providers/openai_codex.rs` — rewrite

```rust
use std::sync::Arc;
use crate::auth::{FileSystemTokenStore, OAuthToken, TokenStore};

pub struct OpenaiCodexProvider {
    model: String,
    thinking: String,
    token: tokio::sync::RwLock<OAuthToken>,
    store: Option<Arc<dyn TokenStore>>,  // None = ephemeral
    http: reqwest::Client,
}

impl OpenaiCodexProvider {
    /// Load the token from the default store. Fails if no token
    /// file exists (i.e., user hasn't run `mu login`).
    pub fn from_store(model: String) -> Result<Self, ProviderError>;

    /// Load from the default store, but don't persist refreshed
    /// tokens. The in-memory bundle still rotates on 401-refresh.
    pub fn from_store_ephemeral(model: String) -> Result<Self, ProviderError>;

    /// Use a caller-supplied token + store. For tests and embedders.
    pub fn from_parts(
        model: String,
        token: OAuthToken,
        store: Option<Arc<dyn TokenStore>>,
    ) -> Self;

    pub fn with_thinking(mut self, thinking: String) -> Self { ... }
}

#[async_trait]
impl Provider for OpenaiCodexProvider {
    async fn stream(
        &self,
        messages: &[AgentMessage],
        tools: &[ToolSpec],
        cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> { ... }
}
```

### JWT claim extraction

```rust
/// Pull `chatgpt_account_id` from the access_token JWT payload.
/// No signature verification — this is *our own* token, freshly
/// minted by OpenAI; we just want a claim out of it. Returns the
/// account id (a UUID-formatted string).
fn extract_chatgpt_account_id(access_token: &str) -> Result<String, ProviderError> {
    // 1. Split on '.', take segment[1]
    // 2. base64url-decode (pad as needed)
    // 3. parse JSON
    // 4. claim path: ["https://api.openai.com/auth"]["chatgpt_account_id"]
}
```

### Request body (Responses API)

```jsonc
POST https://chatgpt.com/backend-api/codex/responses
Authorization: Bearer <access_token>
chatgpt-account-id: <uuid-from-JWT>
originator: mu
Content-Type: application/json
Accept: text/event-stream

{
  "model": "gpt-5-codex",
  "input": [
    {"role": "user", "content": [{"type": "input_text", "text": "..."}]}
  ],
  "tools": [
    {"type": "function", "name": "read", "description": "...", "parameters": {...}}
  ],
  "reasoning": {"effort": "medium"},       // from `thinking`
  "stream": true
}
```

### `mu-coding/src/serve.rs` — provider construction

```rust
pub fn build_provider(
    name: &str,
    model: Option<&str>,
    ephemeral: bool,
) -> Result<Arc<dyn Provider>> {
    match name {
        "openai-codex" => {
            let model = model.unwrap_or("gpt-5-codex").to_owned();
            let provider = if ephemeral {
                OpenaiCodexProvider::from_store_ephemeral(model)?
            } else {
                OpenaiCodexProvider::from_store(model)?
            };
            Ok(Arc::new(provider))
        }
        // ... other providers
    }
}
```

### CLI changes

```rust
// mu serve / mu ask gain:
#[arg(long)]
ephemeral: bool,
```

Plumbed through `build_provider` for `openai-codex`. Ignored for
providers that don't use stored tokens.

## Behaviors

1. **B-1 (JWT claim extraction):** Build a synthetic JWT whose
   middle segment encodes
   `{"https://api.openai.com/auth": {"chatgpt_account_id": "abc-123"}}`;
   pass to `extract_chatgpt_account_id`, assert `"abc-123"`.

2. **B-2 (JWT bad format rejected):** Pass `"not-a-jwt"`,
   `"only.two"`, and `"a.b.c.d"`; assert `ProviderError` in each
   case.

3. **B-3 (`from_store` clean failure when not logged in):**
   `FileSystemTokenStore::with_base_dir(tempdir)`; call
   `OpenaiCodexProvider::from_store_at(tempdir, "gpt-5-codex")`
   (test-only constructor); assert error mentions "not logged in"
   and "mu login".

4. **B-4 (`from_store` happy path):** Write a token file at
   tempdir, construct via the test-only constructor, assert OK.

5. **B-5 (Request body shape):** Build a request body from
   `[AgentMessage::User { content: "hi" }]` + one tool + `thinking
   = "high"`; assert JSON contains `model`, `input[0].role == "user"`,
   `tools[0].type == "function"`, `reasoning.effort == "high"`,
   `stream == true`.

6. **B-6 (SSE → ProviderEvent mapping — text only):** Feed canned
   SSE bytes containing `response.output_text.delta` events and a
   final `response.completed`; assert stream yields
   `TextDelta("foo")`, `TextDelta("bar")`, `Done(...)` in order.

7. **B-7 (SSE → ProviderEvent mapping — tool call):** Feed canned
   SSE with `response.output_item.added` (function_call),
   `response.function_call.arguments.delta` chunks, and
   `response.output_item.done`; assert the stream yields one or more
   `ToolInputDelta` events and a `Done` whose `AssistantMessage`
   contains a `ContentBlock::ToolUse` with the accumulated input.

8. **B-8 (401 → refresh → retry succeeds):** Use a wiremock server
   for both OpenAI endpoints. First request to `/codex/responses`
   returns 401; refresh endpoint returns a new bundle; retry of
   `/codex/responses` returns 200 with one SSE event. Assert the
   stream yields the event AND that the stored token file was
   updated with the new bundle (non-ephemeral case).

9. **B-9 (401 after refresh gives up):** Same setup as B-8 but the
   retry also returns 401; assert `ProviderError` mentions
   "credentials rejected" and "`mu login`".

10. **B-10 (refresh ephemeral does not persist):** Same as B-8 but
    use `from_store_ephemeral`; assert the token file on disk is
    UNCHANGED after the refresh.

11. **B-11 (cancel mid-stream closes connection):** wiremock streams
    a slow SSE response; fire `cancel_rx`; assert the stream
    terminates within 100ms and (via mock server hook) that the
    connection was closed by the client.

12. **B-12 (live, gated `MU_LIVE_OPENAI_CODEX=1`):** Calls
    `chatgpt.com/backend-api/codex/responses` with a real token from
    the user's store; sends "Reply with the single word 'hello' and
    nothing else."; assert the response contains "hello" and yields
    a `Done` event.

13. **B-13 (live tool roundtrip, gated):** Same as B-12 but with
    one tool registered; the prompt asks the model to call the
    tool; assert the stream yields a `ToolInputDelta` and a `Done`
    whose `AssistantMessage` contains a `ToolUse` block.

## Acceptance

- Modified files:
  - `crates/mu-ai/src/providers/openai_codex.rs` — full rewrite
  - `crates/mu-ai/Cargo.toml` — add `base64`/`urlencoding` if not
    already present for JWT decode (both already present from
    mu-018)
  - `crates/mu-coding/src/serve.rs` — `build_provider` signature
    grows an `ephemeral: bool` arg
  - `crates/mu-coding/src/bin/mu.rs` — `--ephemeral` flag on
    `serve` / `ask`
  - `crates/mu-coding/src/ask.rs` — forward the flag
- Removed code: `flatten_messages`, `locate_pi`, `spawn_and_wait`,
  and the `tokio::process::Command` import — the pi subprocess path
  is gone.
- `cargo build` clean.
- `cargo nextest run` passes — every existing test plus B-1..B-11.
  B-12/B-13 are live-gated.
- Manual: with a fresh `mu login`, `mu ask --provider openai-codex
  --model gpt-5-codex "say hello"` streams text and exits cleanly.
- Manual: with `--tools read`, `mu ask --provider openai-codex
  --model gpt-5-codex --tools read "read /etc/hostname"` round-trips
  a tool call.

## Out-of-circuit warnings

- **OOC-1 (endpoint distinction).** `chatgpt.com/backend-api` is
  *not* `api.openai.com`. Codex-OAuth tokens won't work against
  `api.openai.com/v1/responses`, and API keys won't work against
  `chatgpt.com/backend-api`. If a future spec adds an `openai-api`
  provider, it goes in a separate module.

- **OOC-2 (Responses API ≠ Chat Completions).** mu-017's OpenRouter
  implementation uses Chat Completions semantics (deltas indexed by
  `choices[0]`, tool calls in `choices[0].delta.tool_calls`). The
  Responses API is a different shape — events are typed by event
  name (`response.output_text.delta`, etc.) and items have `item_id`s
  rather than choice indices. Don't try to share parsing code
  between the two; the mental model is genuinely different.

- **OOC-3 (`originator: mu` header).** pi sends `originator: pi`.
  We send `originator: mu`. This is a free string OpenAI uses to
  identify the client; mismatching it with what the JWT was issued
  for is *probably* fine (the JWT controls auth), but if Codex
  starts rejecting unknown originators, this is the place to look.

- **OOC-4 (account-id is per-token).** `chatgpt_account_id` lives
  in the JWT; if the user logs out and back in as a different
  account, the JWT changes and so does the account-id. Always
  re-extract from the *current* access_token, never cache.

- **OOC-5 (refresh token rotation).** `refresh_access_token` returns
  a full bundle whose `refresh_token` may differ from the one passed
  in. The persisted bundle MUST be the new one. Saving only the new
  access_token while keeping the old refresh_token leaves the user
  one refresh away from a dead account.

- **OOC-6 (no SSE parser sharing concerns).** `providers::sse` is
  generic — works for both Chat Completions and Responses API. The
  mapping from `SseEvent` to `ProviderEvent` is what differs.

- **OOC-7 (model defaults).** The current pi-subprocess provider
  has no default model — caller must pass one. Post-mu-019,
  consider defaulting to `gpt-5-codex` in `build_provider` for
  ergonomics. Decided per INV-5 of CONVENTIONS (no surprise
  defaults): we default *in the CLI*, not in the provider
  constructor, so library users still see explicit failures.

- **OOC-8 (live-test hygiene).** B-12/B-13 burn real tokens against
  the user's Codex quota. Keep them short ("say hello", tiny tool
  call). Don't add live tests that the user wouldn't want running
  on every `MU_LIVE_OPENAI_CODEX=1` invocation.

## Prior work / context

- mu-015 (`specs/mu-015-openai-codex-provider.md`) — the original
  pi-subprocess implementation we're replacing.
- mu-017 (`specs/mu-017-openrouter-provider.md`) — closest analog;
  HTTP+SSE+tools shape, different endpoint contract.
- mu-018 (`specs/mu-018-openai-oauth-login.md`) — token storage and
  refresh primitives mu-019 consumes.
- pi's `openai_responses.rs` (~2500 LOC) — Responses API event
  vocabulary and request shape are documented there. Read-only
  reference; we don't depend on pi.
- The decoded JWT structure verified on 2026-05-10:
  `https://api.openai.com/auth.chatgpt_account_id` is present in
  the access_token middle segment alongside `chatgpt_plan_type`
  (`"prolite"` for the test user) and `chatgpt_user_id`. mu-019's
  JWT decode only needs `chatgpt_account_id`.

## Changelog

- 2026-05-10 — initial draft (claude-personal).
