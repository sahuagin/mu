# Spec: Queue-driven agent loop

| field      | value                                          |
| ---------- | ---------------------------------------------- |
| spec_id    | mu-003                                         |
| status     | ready                                          |
| created    | 2026-05-09                                     |
| updated    | 2026-05-09                                     |
| authors    | tcovert + claude-personal (claude-opus-4.7)    |
| supersedes | none                                           |

## Why

The actual point of mu. Until this lands, `mu serve` is a JSON-RPC echo
that knows what its protocol looks like but can't do anything. After this
lands, `mu serve` can run an agent — call an LLM (via a Provider trait
implementation), execute tools, stream events back to the frontend.

This spec uses a **queue-driven event loop** instead of pi_ts's two-loop
control structure. Rationale: pi_ts has a *de facto* queue
(`pendingMessages` + `getFollowUpMessages()`) but doesn't model it as
the central abstraction; the result is two nested control loops where
one event-driven loop would be cleaner. The queue model also handles
peer messages from a future mailbox MCP server uniformly with frontend
input — both push `UserMessage` actions onto the same queue.

## Scope

- **In:**
  - A new module at `crates/mu-core/src/agent/`, organized as multiple
    files because a single agent.rs would push 700+ lines and start the
    accretion problem we're trying to avoid.
  - **`agent/types.rs`** (~150 lines): `AgentMessage`, `AssistantMessage`,
    `ContentBlock`, `ToolCall`, `StopReason`. The data carried in the
    conversation context.
  - **`agent/provider.rs`** (~80 lines): `Provider` trait,
    `ProviderEvent` enum, `ProviderError`. The LLM abstraction. No
    concrete impls — those are mu-ai's job in a future spec.
  - **`agent/tool.rs`** (~80 lines): `Tool` trait, `ToolSpec`,
    `ToolResult`. Tool abstraction. No concrete impls — those are
    mu-coding's job in a future spec.
  - **`agent/loop.rs`** (~350 lines including tests): the queue-driven
    `run` function, `AgentLoop` handle, `AgentInput`, `AgentEvent`,
    `Action` (private), `AgentConfig`, `Outcome`. The state machine.
  - **`agent/mod.rs`** (~30 lines): module root + public re-exports.
  - **`pub mod agent;`** added to `crates/mu-core/src/lib.rs`.
  - Tests in each file using a `MockProvider` and `MockTool` defined in
    `agent/loop.rs`'s test module. Loop tests cover at least the seven
    behaviors (B-1..B-7) below.

- **Out:**
  - Real Provider impls (anthropic, openai, openrouter) — mu-ai's job.
    For mu-003, only `MockProvider` exists (in test code).
  - Real Tool impls (read, bash, edit, etc.) — mu-coding's job. For
    mu-003, only `MockTool` exists.
  - Streaming partial tool results during execution (`tool_execution_update`
    in pi_ts). Future spec; tools return their final result.
  - Hooks (`prepareNextTurn`, `shouldStopAfterTurn`, `beforeToolCall`,
    `afterToolCall`). Defer all — none are MVP-blocking.
  - Parallel tool execution. v1 is sequential always; parallel is a
    future spec.
  - Steering / follow-up message *queues* in the pi_ts sense — replaced
    by the queue model. External callers push `UserMessage` to the
    front (steer) or back (follow-up); no separate primitives needed.
  - Compaction. Big future spec.
  - Session persistence. mu-coding's job.
  - Wiring `run` into `mu serve` (the JSON-RPC handler). Future spec
    after mu-003 lands; for now, the loop is testable in isolation.

- **Non-goals:**
  - `session.message_update` event variant (carry both delta + running
    partial). Listed in mu-001's "future amendment" candidates;
    deliberately not pulled in here. mu-003 emits `AgentEvent::TextDelta`
    only. We can amend mu-001 + mu-003 later if/when consumers want
    the richer primitive.

## Invariants

- **INV-1 (file size):** Each file under 400 lines (including tests).
  `agent/loop.rs` is the biggest and may approach 350; that's the
  natural ceiling. Splits if needed: extract test module into
  `agent/loop_tests.rs`.
- **INV-2 (Provider trait is async):** No sync `Provider`. The LLM
  ecosystem is async-first. Use `#[async_trait]` (already a workspace
  dep).
- **INV-3 (single source of external input):** External callers push
  `AgentInput` via the loop's `mpsc::Sender`. Internal state-machine
  transitions (`Action::InvokeLlm`, `Action::ExecuteTools`,
  `Action::MaybeFinish`) live in a private `VecDeque` inside the run
  function. Callers cannot push `Action`s. This prevents broken
  state-machine transitions.
- **INV-4 (iteration cap):** `AgentConfig::max_turns` defaults to 20;
  counts assistant-message turns, not loop iterations. When a turn
  would exceed the cap, the loop emits `AgentEvent::Done` with
  `stop_reason: StopReason::EndTurn` and returns
  `Outcome::IterationCap`. Configurable per spawn.
- **INV-5 (cancellation):** Cancel propagates via a `oneshot` channel
  per provider call AND per tool execution. The loop is also
  cancellable via `AgentInput::Cancel` which short-circuits the queue
  and returns `Outcome::Cancelled`. Reason for two paths: in-flight
  provider streams need to be told to stop (oneshot), but the loop's
  state machine also needs a recognizable termination event (Cancel
  enum variant).
- **INV-6 (no token holding):** The Provider trait does NOT include
  any field for OAuth tokens or refreshable credentials. Auth lives
  inside Provider impls (in mu-ai), opaque to the loop. This matches
  the AGENTS.md "no third-party-OAuth-token holding" guardrail —
  mu-core's loop never sees a token.
- **INV-7 (no unsafe, no unwrap/expect outside tests):** Standard.
- **INV-8 (no new dependencies):** Use `tokio` (full), `serde`,
  `serde_json`, `thiserror`, `tracing`, `async-trait`, `futures`, plus
  stdlib. All already in the workspace deps. Specifically:
  - **No `tokio-util`** for `CancellationToken` — use `oneshot`
    channels directly. (We can add `tokio-util` later as a separate
    cleanup if cancel patterns proliferate.)
  - **No `tokio-stream`**. The `futures` crate provides `StreamExt`.
- **INV-9 (no panics):** No `panic!`, no `unimplemented!`, no `todo!`.
  Branches that "shouldn't happen" return `Outcome::Error` with a
  descriptive message.

## Interfaces

### `agent/types.rs`

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One message in an agent's conversation context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum AgentMessage {
    User { content: String },
    Assistant(AssistantMessage),
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
}

/// The model's response on one turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    ToolCall(ToolCall),
    /// Reasoning trace (Anthropic extended thinking, OpenAI reasoning).
    Thinking { text: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Model stopped naturally; no tool calls in the response.
    EndTurn,
    /// Model emitted tool calls; loop should execute them and continue.
    ToolUse,
    /// Model hit a token limit mid-response.
    MaxTokens,
    /// Provider errored; assistant message may be partial.
    Error,
    /// Cancel was requested (via AgentInput::Cancel or via cancellation
    /// signal from outside).
    Aborted,
}
```

### `agent/provider.rs`

```rust
use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::oneshot;

use super::tool::ToolSpec;
use super::types::{AgentMessage, AssistantMessage};

/// Events from a provider's streaming response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProviderEvent {
    /// Streaming text chunk.
    TextDelta(String),
    /// Streaming reasoning chunk (Anthropic extended thinking, OpenAI o1).
    /// Optional — providers without reasoning never emit this.
    ThinkingDelta(String),
    /// Streaming partial tool call. Provider may emit multiple deltas
    /// before the call is finalized in the Done payload.
    ToolCallDelta {
        id: String,
        name_delta: Option<String>,
        arguments_delta: Option<String>,
    },
    /// Stream ended successfully. Final assistant message attached.
    Done(AssistantMessage),
    /// Stream errored. Caller should map this to Outcome::Error.
    Error(String),
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("provider: {0}")]
    Other(String),
}

/// LLM provider abstraction.
///
/// Concrete implementations live in mu-ai. mu-core only knows the
/// trait. This is the seam for cancel propagation: callers pass a
/// `oneshot::Receiver<()>`; the provider awaits it via `select!` and
/// terminates the stream when it fires.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Open a streaming response.
    ///
    /// Implementations OWN `cancel_rx`. When the matching sender
    /// fires, the implementation must terminate the stream
    /// promptly — emit `ProviderEvent::Done` with
    /// `stop_reason: StopReason::Aborted` if a partial message is
    /// available, otherwise `ProviderEvent::Error`.
    async fn stream(
        &self,
        messages: &[AgentMessage],
        tools: &[ToolSpec],
        cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError>;
}
```

### `agent/tool.rs`

```rust
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;

/// Public description of a tool, sent to the provider so the model
/// knows what tools exist.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema describing the arguments. The provider feeds this
    /// to the model.
    pub input_schema: Value,
}

/// Tool execution result. Errors are EXPRESSED via `is_error: true`
/// rather than propagated — the LLM expects to see the error text and
/// react to it, not get a "the tool failed" rejection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

#[async_trait]
pub trait Tool: Send + Sync {
    /// What the model sees about this tool.
    fn spec(&self) -> ToolSpec;

    /// Execute the tool. The Tool impl owns `cancel_rx` and must
    /// abort when it fires.
    async fn execute(
        &self,
        arguments: Value,
        cancel_rx: oneshot::Receiver<()>,
    ) -> ToolResult;
}
```

### `agent/loop.rs` (and `agent/mod.rs`)

```rust
// agent/mod.rs

pub mod loop_;
pub mod provider;
pub mod tool;
pub mod types;

pub use loop_::{AgentConfig, AgentEvent, AgentInput, AgentLoop, Outcome};
pub use provider::{Provider, ProviderError, ProviderEvent};
pub use tool::{Tool, ToolResult, ToolSpec};
pub use types::{AgentMessage, AssistantMessage, ContentBlock, StopReason, ToolCall};
```

(We name the module `loop_` because `loop` is a Rust keyword. The
`pub use` re-export makes the module name invisible to outside
consumers, who write `mu_core::agent::AgentLoop`.)

```rust
// agent/loop_.rs

use std::collections::VecDeque;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use futures::StreamExt;

use super::provider::{Provider, ProviderEvent};
use super::tool::{Tool, ToolResult};
use super::types::{AgentMessage, AssistantMessage, ContentBlock, StopReason, ToolCall};

/// External inputs callers can push to a running agent loop.
#[derive(Debug, Clone)]
pub enum AgentInput {
    /// Add a message to the conversation. Loop will invoke the LLM
    /// after processing.
    UserMessage(AgentMessage),
    /// Stop. In-flight provider stream and tool execution are
    /// cancelled; loop returns Outcome::Cancelled.
    Cancel,
}

/// Output events emitted by the loop. Mirrors the `session.*`
/// notifications in mu-001's protocol module, but is the *internal*
/// shape — wiring to the JSON-RPC notification surface is mu-coding's job.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    AgentStart,
    TurnStart,
    MessageStart { message: AgentMessage },
    TextDelta { delta: String },
    ToolCallStarted {
        tool_call_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },
    ToolCallCompleted {
        tool_call_id: String,
        content: String,
        is_error: bool,
    },
    MessageEnd { message: AgentMessage },
    TurnEnd,
    Done { stop_reason: StopReason, turn_count: u32 },
    Error { message: String },
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Cap on assistant-message turns. Default 20. The loop emits
    /// AgentEvent::Done(EndTurn) and returns Outcome::IterationCap
    /// when this is reached.
    pub max_turns: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self { max_turns: 20 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Loop ended naturally (LLM said no more tool calls).
    Done(StopReason),
    /// Hit max_turns.
    IterationCap,
    /// AgentInput::Cancel received, or input channel closed.
    Cancelled,
    /// Provider or tool errored unrecoverably.
    Error(String),
}

/// Internal action queue. NOT public — callers use AgentInput.
enum Action {
    External(AgentInput),
    InvokeLlm,
    ExecuteTools(Vec<ToolCall>),
    MaybeFinish,
}

/// Handle to a running agent loop.
pub struct AgentLoop {
    tx: mpsc::Sender<AgentInput>,
    handle: JoinHandle<Outcome>,
}

impl AgentLoop {
    /// Spawn a new agent loop on the current tokio runtime.
    pub fn spawn(
        provider: Arc<dyn Provider>,
        tools: Vec<Arc<dyn Tool>>,
        config: AgentConfig,
        events: mpsc::Sender<AgentEvent>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(32);
        let handle = tokio::spawn(run(provider, tools, config, events, rx));
        Self { tx, handle }
    }

    /// Push input to the running loop. Returns Err if the loop has
    /// terminated.
    pub async fn send(&self, input: AgentInput) -> Result<(), AgentInput> {
        self.tx.send(input).await.map_err(|e| e.0)
    }

    /// Wait for the loop to finish.
    pub async fn join(self) -> Outcome {
        self.handle
            .await
            .unwrap_or(Outcome::Error("loop task panicked".into()))
    }
}

async fn run(
    provider: Arc<dyn Provider>,
    tools: Vec<Arc<dyn Tool>>,
    config: AgentConfig,
    events: mpsc::Sender<AgentEvent>,
    mut input_rx: mpsc::Receiver<AgentInput>,
) -> Outcome {
    // ... see §Behaviors for the algorithm. Implementer chooses how
    // to interleave external input with internal queue draining
    // within the constraints of §INV-3 and §B-1..B-7 below.
}
```

The implementer's main judgment call inside `run`: how to interleave
external input (from `input_rx`) with internal queue draining. Options
include `tokio::select!` between the two, draining internal first
then awaiting external when empty, or polling both each iteration.
The §Behaviors section pins observable semantics (steering should
preempt; follow-up should be processed after the current turn);
within those, choose whatever keeps the code clearest.

## Behaviors

1. **B-1 (single-turn no-tools):** Given a `MockProvider` whose first
   `stream()` call yields `[TextDelta("hi"), Done(AssistantMessage {
   content: [Text("hi")], stop_reason: EndTurn })]`, sending
   `AgentInput::UserMessage(User { content: "hello" })` then awaiting
   `join()` produces `Outcome::Done(EndTurn)`. The events channel
   receives, in order: `AgentStart`, `MessageStart(user)`,
   `MessageEnd(user)`, `TurnStart`, `TextDelta("hi")`,
   `MessageStart(assistant)`, `MessageEnd(assistant)`, `TurnEnd`,
   `Done { stop_reason: EndTurn, turn_count: 1 }`.

2. **B-2 (single tool call):** Given a `MockProvider` that on first
   call yields a tool call `{id: "t1", name: "echo", args: {x: 1}}`
   then `Done`, and on second call yields `[TextDelta("done"), Done]`,
   AND a `MockTool` named "echo" that returns `ToolResult { content:
   "echoed", is_error: false }`, the loop produces (in order):
   `AgentStart`, `MessageStart/End(user)`, `TurnStart`,
   `MessageStart/End(assistant with tool call)`,
   `ToolCallStarted{t1, echo, ...}`,
   `ToolCallCompleted{t1, "echoed", is_error: false}`, `TurnEnd`,
   `TurnStart`, `TextDelta("done")`, `MessageStart/End(assistant)`,
   `TurnEnd`, `Done { turn_count: 2 }`.

3. **B-3 (iteration cap):** Given a `MockProvider` that ALWAYS yields
   tool calls and a tool that always succeeds, with
   `AgentConfig { max_turns: 3 }`, the loop emits
   `Done { turn_count: 3 }` and returns `Outcome::IterationCap`. Total
   `TurnStart` events: exactly 3.

4. **B-4 (cancel from external):** A loop in the middle of streaming
   (long mock provider that emits one TextDelta then awaits forever)
   receiving `AgentInput::Cancel` returns `Outcome::Cancelled` within
   reasonable time (test uses `tokio::time::timeout(500ms)`). The
   provider's `cancel_rx` MUST have fired (verify by having
   `MockProvider` record receipt and assert it).

5. **B-5 (tool error continues the loop):** A `MockTool` that returns
   `ToolResult { content: "boom", is_error: true }` does NOT terminate
   the loop. The loop emits
   `ToolCallCompleted { is_error: true, content: "boom" }`, then
   continues with another `InvokeLlm`. The test verifies a second
   provider call happens.

6. **B-6 (provider error terminates):** A `MockProvider` that yields
   `ProviderEvent::Error("rate limit")` causes the loop to emit
   `AgentEvent::Error { message: "rate limit" }` and return
   `Outcome::Error("rate limit")`. No further events follow.

7. **B-7 (UserMessage during turn pushes to back):** A loop that has
   queued an internal `Action::ExecuteTools(...)` receives
   `AgentInput::UserMessage(...)` from the external channel. The
   USER MESSAGE is processed AFTER the queued tool execution
   completes. (We choose this ordering because steering mid-tool-call
   is semantically tricky — the model has already committed to a tool
   call; injecting a user message before the tool result would
   violate the assistant↔user/toolResult alternation that providers
   require.) Test setup: prime the queue with a tool-call response
   from the provider, send a UserMessage during execution, verify
   the trace: `tool_started → tool_completed → user_message_start →
   user_message_end → turn_start (from invoke_llm)`.

## Acceptance

- Files created at exactly:
  - `crates/mu-core/src/agent/mod.rs`
  - `crates/mu-core/src/agent/types.rs`
  - `crates/mu-core/src/agent/provider.rs`
  - `crates/mu-core/src/agent/tool.rs`
  - `crates/mu-core/src/agent/loop_.rs`
- `pub mod agent;` added to `crates/mu-core/src/lib.rs`. Single line.
- `cargo build -p mu-core` succeeds, no warnings.
- `cargo nextest run -p mu-core` passes — every existing test plus
  B-1..B-7 (so 19 + 7 = 26 minimum, more if individual files have
  unit tests).
- Each agent module file under 400 lines including its tests.
- No `unsafe`, no `unwrap`/`expect`/`panic!`/`todo!`/`unimplemented!`
  outside `#[cfg(test)]`.
- No new dependencies in `Cargo.toml`.
- Diff touches exactly the six files above (5 new + 1 modified). No
  other files. No formatting changes to existing code.

## Iteration-aware handoff protocol

This spec is meaningfully bigger than mu-002. Estimated 700-1000 LOC
total across five files. If gpt-pro hits its iteration cap mid-task:

1. Commit current state to a branch named `spec/mu-003-handoff` with a
   commit message listing which files are complete and which are
   stubbed.
2. The natural break points are file boundaries:
   - `types.rs` first (no deps)
   - `tool.rs` next (depends on types — only ToolSpec uses Value)
   - `provider.rs` next (depends on types + tool)
   - `loop_.rs` last (depends on all of the above)
   - `mod.rs` and `lib.rs` re-exports last
3. If types/provider/tool are done but loop_.rs is stubbed, that's a
   completely valid handoff state. Next agent picks up at loop_.rs.

If a sub-agent realizes early it can't fit this spec in its budget,
returning `status: "blocked"` with `notes: "request smaller scope"`
is preferred over a half-done loop. We can split into mu-003a
(types+traits) and mu-003b (loop) if needed.

## Open questions

- [ ] OQ-1: Should AgentEvent be `pub` from mu-core, or kept internal
  and only the JSON-RPC `session.*` notifications (in mu-001) are the
  public surface? — owner: tcovert — resolution: PUB. The events are
  testable in isolation and consumers (e.g., a TUI in mu-coding) want
  the typed enum, not the JSON wire form. mu-coding does the typed-
  enum → JSON-RPC notification translation. This is also why
  AgentEvent has its own `serde_rename_all = "snake_case"` and matches
  the wire shape of mu-001's notifications: the translation is mostly
  trivial.
- [ ] OQ-2: Should the loop snapshot the messages vector for the
  Provider call, or pass the live `&[AgentMessage]`? — owner: defer —
  resolution: pass `&messages` directly. Provider impls that need
  ownership can `.to_vec()`; cloning a few-message vec is cheap and
  the API surface is simpler. If real providers do something exotic
  (e.g., transforming the messages and needing the original), revisit.
- [ ] OQ-3: Should `AgentInput::Cancel` be `Cancel { reason: Option<String> }`
  for telemetry? — owner: defer — resolution: no for v1. Adding a
  field later is non-breaking via `#[non_exhaustive]` if we want.

## Out-of-circuit warnings

- **OOC-1:** `loop_.rs` is named with a trailing underscore because
  `loop` is a Rust reserved keyword. Don't try to name it `r#loop` or
  `loop_mod`; the trailing underscore is the canonical Rust pattern
  for this. The `pub use loop_::*;` in `mod.rs` makes it invisible to
  consumers.
- **OOC-2:** `BoxStream<'static, ProviderEvent>` lifetime is
  load-bearing in §Interfaces. Implementers may be tempted to write
  `BoxStream<'_, _>` or tie it to `&self`; that will compile in
  isolation but break when the loop tries to hold the stream across
  await points. Use `'static` exactly as written.
- **OOC-3:** `AgentMessage::ToolResult` carries `call_id` (the id
  from the tool call) and `content` (the result text). It does NOT
  carry the result-as-content-blocks array that providers like
  Anthropic use natively — that translation is the Provider impl's
  job, not the loop's. The loop is provider-agnostic and traffics in
  string content for tool results.
- **OOC-4:** Concurrent dispatch via `tokio::spawn` (which mu-002
  uses) is NOT what we want here. The loop is a single owner of its
  state machine; events flow through one task. Don't split the loop
  across multiple tasks.
- **OOC-5:** `is_error: true` in a ToolResult is NOT the same as the
  Tool returning `Err(_)`. The Tool trait's `execute` returns a
  `ToolResult` (not a `Result<ToolResult, _>`) precisely so tools can
  signal "I ran fine but my result is an error message" without
  collapsing into the unrecoverable-failure case. If a Tool needs to
  fail unrecoverably (e.g., panic), that becomes a panic — which the
  spawned task catches via `JoinHandle`. We don't propagate panics
  back to the loop in v1; they show up as `Outcome::Error("loop task
  panicked")` from `AgentLoop::join`.

## Prior work / context

- spec mu-001 (`specs/mu-001-protocol-types.md`) — the JSON-RPC
  notification types this loop's events translate into.
- spec mu-002 (`specs/mu-002-stdio-transport.md`) — the transport
  that will eventually carry the events. mu-coding wires it together
  in a future spec.
- pi_ts's `agent/src/agent-loop.ts` — the two-loop reference
  implementation. We deliberately diverge to a queue model.
- pi_ts's `agent/src/types.ts` — message/content/tool types reference.
  Our shapes are simpler (no thinking redaction, no image content
  yet, no document content).
- task_log entries tagged `mu,delegation`.
- memory `b22fd2c3` — the streaming-primitive preference. Honored by
  not pre-buffering text deltas inside the loop.

## Changelog

- 2026-05-09 — initial draft (claude-personal w/ tcovert review).
