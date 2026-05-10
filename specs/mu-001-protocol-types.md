# Spec: JSON-RPC 2.0 protocol types for `mu-core`

| field      | value                                          |
| ---------- | ---------------------------------------------- |
| spec_id    | mu-001                                         |
| status     | ready                                          |
| created    | 2026-05-09                                     |
| updated    | 2026-05-09                                     |
| authors    | tcovert + claude-personal (claude-opus-4.7)    |
| supersedes | none                                           |

## Why

`mu-core` is the JSON-RPC daemon. Every frontend (`mu tui`, `mu ask`,
`mu orchestrate`) is going to talk to it over this protocol. The protocol
*is* the contract between the daemon and the rest of the system; if it's
ambiguous or grows by accretion, every later subsystem inherits the
ambiguity. Define it once, deliberately, before any transport or agent-
loop code goes near it.

Smaller motivation: this is the first delegation to a sub-agent (gpt-pro
via `agent-router`). The spec is also a calibration of how tightly we can
hand structured work to non-claude agents and get clean output back.

## Scope

- **In:**
  - The JSON-RPC 2.0 envelope: `Request`, `Response`, `Error`, `Notification`.
  - Domain method types (request / response pairs) for: `ping`,
    `create_session`, `ask_session`, `cancel_session`, `close_session`.
  - Event-notification types emitted by the daemon during `ask_session`:
    `session.text_delta`, `session.tool_call_started`,
    `session.tool_call_completed`, `session.done`, `session.error`.
  - Serde derives + serialization tests for every shape above.
  - One file, `crates/mu-core/src/protocol.rs`, plus `pub mod protocol;`
    re-export in `lib.rs`.

- **Out:**
  - The transport (stdio framing, unix sockets — separate spec).
  - The agent loop / state machine (separate spec).
  - Provider trait / actual LLM calls (separate crate, `mu-ai`).
  - Tool execution. Tools run server-side; the protocol only surfaces
    `tool_call_started` / `tool_call_completed` events, never a "frontend
    executes tool" round-trip. Permission/approval is deferred.
  - Session persistence schema (sqlite tables — separate spec).

- **Non-goals:**
  - Streaming framing semantics. Notifications come over the same channel
    as request/response per JSON-RPC 2.0; *how* they're delimited on the
    wire is the transport's concern.
  - Compaction, attachments, multi-modal. Listed only so a reasonable
    agent doesn't try to add fields for them speculatively.

## Invariants

- **INV-1:** All public types derive `Serialize`, `Deserialize`, `Debug`,
  `Clone`, and `PartialEq`. Even types that only flow one direction
  derive both Serialize *and* Deserialize so tests can round-trip them.
- **INV-2:** Every method's request type is named `<Method>Request`, every
  response type `<Method>Response`. No `*Params` / `*Result` aliasing —
  prefer one name per concept.
- **INV-3:** JSON-RPC `id` field is `serde_json::Value` (per spec: number
  or string allowed). Do not narrow the type. Notifications have no `id`.
- **INV-4:** The `method` field on requests/notifications is a
  `&'static str` constant on each type via an inherent `const METHOD: &str`,
  *not* an enum-of-strings. This lets the wire-name and the type stay in
  lockstep without an extra round of serde glue.
- **INV-5:** No `unsafe`. No `unwrap` / `expect` outside test modules.
- **INV-6:** Module length < 800 lines including tests. Per AGENTS.md
  ("no 27k-line files") — splits earlier than necessary, but late-binding
  splits are the canonical bug source.

## Interfaces

```rust
// crates/mu-core/src/protocol.rs

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ===== JSON-RPC 2.0 envelope =====

pub const JSONRPC_VERSION: &str = "2.0";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request<P> {
    pub jsonrpc: String,        // always "2.0"
    pub id: Value,              // number or string per JSON-RPC 2.0
    pub method: String,
    pub params: P,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Response<R> {
    Ok { jsonrpc: String, id: Value, result: R },
    Err { jsonrpc: String, id: Value, error: ErrorObject },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification<P> {
    pub jsonrpc: String,        // always "2.0"; no `id` field
    pub method: String,
    pub params: P,
}

// ===== Methods =====

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PingRequest;

impl PingRequest {
    pub const METHOD: &'static str = "ping";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PingResponse {
    pub pong: bool,                    // always true on success
    pub server_version: String,        // mu-core's CARGO_PKG_VERSION
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    pub provider: ProviderSelector,
    /// Optional system prompt override. None → daemon default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

impl CreateSessionRequest {
    pub const METHOD: &'static str = "create_session";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateSessionResponse {
    pub session_id: String,            // uuid-shaped, but serialize as String
}

/// Provider selection at session-create time. Tagged enum so the wire
/// format is `{ "kind": "anthropic_api", "model": "claude-..." }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderSelector {
    AnthropicApi   { model: String },
    AnthropicOauth { model: String },   // wraps `claude --print`
    OpenaiApi      { model: String },
    OpenaiOauth    { model: String },   // wraps `codex` CLI
    Openrouter     { model: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AskSessionRequest {
    pub session_id: String,
    pub user_message: String,
}

impl AskSessionRequest {
    pub const METHOD: &'static str = "ask_session";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AskSessionResponse {
    /// Acknowledgement that the request was accepted; the actual content
    /// is delivered via `session.*` notifications. Final terminator is
    /// the `session.done` notification.
    pub accepted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelSessionRequest {
    pub session_id: String,
}

impl CancelSessionRequest {
    pub const METHOD: &'static str = "cancel_session";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelSessionResponse {
    pub cancelled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloseSessionRequest {
    pub session_id: String,
}

impl CloseSessionRequest {
    pub const METHOD: &'static str = "close_session";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloseSessionResponse {
    pub closed: bool,
}

// ===== Event notifications (daemon → frontend) =====

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextDeltaEvent {
    pub session_id: String,
    pub delta: String,
}

impl TextDeltaEvent {
    pub const METHOD: &'static str = "session.text_delta";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallStartedEvent {
    pub session_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: Value,              // raw JSON; tools own their schema
}

impl ToolCallStartedEvent {
    pub const METHOD: &'static str = "session.tool_call_started";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallCompletedEvent {
    pub session_id: String,
    pub tool_call_id: String,
    /// `Ok(result)` or `Err(message)` — both shapes serialize as a
    /// tagged enum so the frontend can render them differently.
    pub outcome: ToolOutcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolOutcome {
    Ok  { result: Value },
    Err { message: String },
}

impl ToolCallCompletedEvent {
    pub const METHOD: &'static str = "session.tool_call_completed";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoneEvent {
    pub session_id: String,
    /// Optional usage metadata. None means provider didn't report.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageInfo {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

impl DoneEvent {
    pub const METHOD: &'static str = "session.done";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorEvent {
    pub session_id: String,
    pub message: String,
    /// Optional structured detail; provider-specific.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

impl ErrorEvent {
    pub const METHOD: &'static str = "session.error";
}
```

## Behaviors

1. **B-1 (envelope round-trip)**: Each of `Request<PingRequest>`,
   `Response<PingResponse>::Ok`, `Response<()>::Err`, and
   `Notification<TextDeltaEvent>` round-trips through `serde_json` (encode
   → decode) preserving all fields exactly.
2. **B-2 (jsonrpc field is always "2.0")**: Constructors / encoded form
   always emit `"jsonrpc": "2.0"`. A test checks raw `serde_json::Value`
   has `obj["jsonrpc"] == "2.0"` for both request and notification samples.
3. **B-3 (notification has no id)**: Encoding a `Notification<_>` to
   `serde_json::Value` produces an object that does NOT contain an `id`
   key. (Test by `to_value()` then `obj.get("id").is_none()`.)
4. **B-4 (id type is preserved)**: `Request` with `id = json!(7)` and
   `id = json!("a-uuid")` both round-trip. (Test both shapes.)
5. **B-5 (provider tagged enum wire format)**: Encoding
   `ProviderSelector::AnthropicApi { model: "x" }` produces JSON
   `{"kind":"anthropic_api","model":"x"}`. Test all 5 variants.
6. **B-6 (error response with optional data)**: `Response::Err` with
   `error.data = None` does NOT include a `"data"` key in the encoded
   form; with `error.data = Some(...)` it does. Verifies the
   `skip_serializing_if` is wired correctly.
7. **B-7 (METHOD constants match wire names)**: For every type with
   `const METHOD`, a test asserts the constant matches the spec table:
   `"ping"`, `"create_session"`, `"ask_session"`, `"cancel_session"`,
   `"close_session"`, `"session.text_delta"`,
   `"session.tool_call_started"`, `"session.tool_call_completed"`,
   `"session.done"`, `"session.error"`.

## Acceptance

- File created at `crates/mu-core/src/protocol.rs`.
- `pub mod protocol;` added to `crates/mu-core/src/lib.rs`.
- `cargo build -p mu-core` succeeds.
- `cargo nextest run -p mu-core` passes (all of B-1..B-7 plus the
  existing `version_is_nonempty`).
- Module file length under 800 lines.
- No `unsafe`, no `unwrap`/`expect` outside `#[cfg(test)]`.
- Diff touches exactly: `crates/mu-core/src/protocol.rs` (new),
  `crates/mu-core/src/lib.rs` (+1 line). No other files.

## Iteration-aware handoff protocol

Codex CLI's iteration budget is unknown to us; assume 50 like pi-rust.
At ~40 iterations:
1. Commit current state to a branch named `spec/mu-001-handoff` with a
   commit message describing what was completed.
2. Write `next_agent_starting_position` summarizing which §Behaviors are
   green and which §Behaviors are missing tests or types.
3. Exit cleanly. The next agent picks up by re-reading this spec and the
   handoff branch.

This task should not realistically need this — it's mechanical translation
of an §Interfaces block into Rust with predictable test patterns. The
handoff protocol is documented because the convention is load-bearing for
larger specs that will follow.

## Open questions

- [ ] OQ-1: Should `ProviderSelector` carry credentials at all, or only
  references to credentials stored in mu's config? — owner: defer —
  resolution: deferred to spec mu-002 (config + credentials). For this
  spec, treat credentials as out-of-band; `ProviderSelector` is purely
  "which backend + which model".
- [ ] OQ-2: Should events carry a sequence number for ordering across
  reconnect? — owner: defer — resolution: deferred until the transport
  spec; for now the protocol assumes ordered delivery.
- [ ] OQ-3: Do we need a `session.user_message_received` event so the
  frontend can echo the user's own message back via the same event
  stream? — owner: defer — resolution: probably no, the frontend already
  knows what it sent; revisit if multi-frontend-on-one-session becomes
  real.

## Out-of-circuit warnings

- **OOC-1**: serde tagged enums with `#[serde(tag = "kind")]` — agents
  sometimes reach for `#[serde(rename_all = "camelCase")]` on the enum
  variants by reflex. The spec specifies `snake_case`. If the implementation
  emits `anthropicApi` instead of `anthropic_api`, B-5 will catch it but
  the spec is the source of truth.
- **OOC-2**: `#[serde(skip_serializing_if = "Option::is_none")]` is
  load-bearing on `error.data`, `done.usage`, `error_event.detail`, and
  `create_session.system_prompt`. Without it the encoded form contains
  `"data": null` which some JSON-RPC clients treat as an error. B-6
  pins this for `error.data`; the implementation must apply the same
  attribute to the others.
- **OOC-3**: `#[derive(Default)]` on unit structs — `PingRequest` is a
  unit struct. Some agents will helpfully add `Default`. It's not in the
  invariants. Don't add unrequested derives; it bloats the public API
  surface.

## Prior work / context

- pi_ts's `modes/rpc/rpc-types.ts` (and `rpc-server.ts`) for the same
  surface in TypeScript. Reference, not copy. Located at
  `~/src/public_github/pi/packages/coding-agent/src/modes/rpc/`.
- pi_agent_rust's `acp.rs` is the closest analogue in pi_rs. We are
  *not* reproducing its shape; we are starting fresh with a smaller
  surface and JSON-RPC 2.0 framing instead of pi-rust's bespoke protocol.
- mu's AGENTS.md — read it. The "no third-party-OAuth-token holding"
  rule is reflected in `ProviderSelector` having `*_oauth` variants that
  imply subprocess wrappers (no token fields).

## Changelog

- 2026-05-09 — initial draft (claude-personal w/ tcovert review).
