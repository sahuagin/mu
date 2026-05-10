# Spec: `mu serve` end-to-end + FauxProvider

| field      | value                                          |
| ---------- | ---------------------------------------------- |
| spec_id    | mu-004                                         |
| status     | ready                                          |
| created    | 2026-05-10                                     |
| updated    | 2026-05-10                                     |
| authors    | tcovert + claude-personal (claude-opus-4.7)    |
| supersedes | none                                           |

## Why

End-to-end smoke. After this lands, `mu serve` is a real JSON-RPC
daemon that accepts `create_session` / `ask_session` / `cancel_session`
/ `close_session`, runs an agent loop per session, and streams
`session.*` notifications back. With FauxProvider, the whole pipeline
is testable without an LLM key — and FauxProvider stays useful as the
default integration-test fixture for every later spec.

This is also the first time mu-ai becomes a real crate (it's been a
stub) and the first time mu-coding's `serve` subcommand does anything.

## Scope

- **In:**
  - **`crates/mu-ai/src/faux.rs`** — `FauxProvider` (concrete
    `Provider` impl) with two modes: `scripted` (a `VecDeque` of
    pre-built `ProviderEvent` sequences, one per `stream()` call) and
    `echo` (returns the most recent user message back as the assistant
    text). ~150 lines including tests.
  - **`crates/mu-ai/src/lib.rs`** — `pub mod faux;` + `pub use
    faux::FauxProvider;`. The `version()` fn stays.
  - **`crates/mu-coding/src/serve/mod.rs`** — entry point:
    `pub async fn run(provider: Arc<dyn Provider>) -> Result<()>`.
    Sets up the JSON-RPC handler closure, calls
    `mu_core::transport::serve_stdio`. ~80 lines.
  - **`crates/mu-coding/src/serve/sessions.rs`** — in-memory session
    map: `Sessions` struct wrapping `Arc<Mutex<HashMap<String,
    SessionState>>>`. Methods: `create`, `get_sender`, `remove`.
    ~100 lines.
  - **`crates/mu-coding/src/serve/dispatch.rs`** — JSON-RPC method
    dispatch: matches on `request.method`, calls the appropriate
    `Sessions` operation, builds responses. ~150 lines.
  - **`crates/mu-coding/src/serve/forwarder.rs`** — translates
    `AgentEvent` → `mu_core::protocol::*` notifications, sends via
    `NotificationWriter`. Spawns one task per session. ~80 lines.
  - **`crates/mu-coding/src/lib.rs`** — `pub mod serve;`.
  - **`crates/mu-coding/src/bin/mu.rs`** — `Command::Serve` arm calls
    `mu_coding::serve::run(provider).await` with a hardcoded
    `FauxProvider::echo()` for v1. The `main` becomes async via
    `#[tokio::main]`.
  - **mu-core small amendment**: `AgentLoop::sender(&self) ->
    mpsc::Sender<AgentInput>` — needed so `serve` can briefly hold a
    sync lock, clone the sender out, drop the lock, and `await` the
    send without holding the lock across await. ~3 lines.
  - Integration test in `crates/mu-coding/tests/serve_smoke.rs` (new
    integration-test file): drive the JSON-RPC surface end-to-end via
    `tokio::io::duplex`, with `FauxProvider::echo()` as the LLM. Send
    `create_session`, `ask_session`, observe `session.text_delta`
    and `session.done` notifications, verify shape.

- **Out:**
  - Real Provider impls (anthropic-api, anthropic-oauth, openai-api,
    openai-oauth, openrouter). All deferred to per-provider specs
    later. v1's `mu serve` runs hardcoded with `FauxProvider`.
  - Real Tool impls (read, bash, edit, write). Sessions run with no
    tools in v1. Adding tools is a small future spec.
  - Session persistence to sqlite. In-memory only.
  - Provider selection from config or `ProviderSelector` field of
    `CreateSessionRequest`. In v1 we ignore the
    `CreateSessionRequest.provider` field — every session uses the
    daemon-wide provider. Honoring per-session provider is a small
    future spec.
  - `mu ask`, `mu tui`, `mu orchestrate` — those subcommands stay
    with their pre-MVP "not implemented" message.
  - `session.input_required` (in-band agent-asks-for-input) and the
    bidir-comm patterns from earlier discussion. Future specs.
  - Translating the `AgentEvent` lifecycle events `AgentStart`,
    `TurnStart`, `MessageStart`, `MessageEnd`, `TurnEnd` into
    JSON-RPC notifications. mu-001's notification surface is smaller
    than mu-003's `AgentEvent` enum on purpose — those events are
    silently dropped at the forwarder. We'll amend mu-001 to add
    them when a frontend (e.g., a TUI) actually needs them.

- **Non-goals:**
  - Multi-provider sessions in one daemon. The daemon has one
    provider; choose it at startup.
  - Tool approval / permission gates.
  - Compaction. Sessions can grow unbounded for v1; restart to clear.

## Invariants

- **INV-1 (file size):** Each file under 800 lines including tests.
  Same cap as mu-002 / mu-003 (post-amendment). Splits if approaching
  1000.
- **INV-2 (session lock never held across await):** All
  `Sessions::*` methods that need to mutate the map do so under a
  `std::sync::Mutex` (or `parking_lot::Mutex` if profiling shows
  contention — but `parking_lot` is not in our deps so v1 uses
  `std::sync`). Lock acquisition is brief; no `await` calls happen
  while the lock is held. Pattern: lock → clone what you need (a
  `Sender`, an `Arc`) → drop lock → await on the clone.
- **INV-3 (notification path is one-way):** The forwarder task
  reads `AgentEvent`s from the loop's events channel and writes
  notifications via `NotificationWriter::emit`. It NEVER sends
  responses; only the dispatch handler does that. This keeps the
  JSON-RPC request/response correlation clean.
- **INV-4 (forwarder dies when its session does):** When the loop's
  events channel closes (loop terminated), the forwarder's `recv`
  loop exits; the task ends naturally. We don't need to abort it;
  storing the `JoinHandle` in `SessionState` keeps it from being
  dropped (which would NOT abort it, but storing it documents intent).
- **INV-5 (provider is shared):** `Arc<dyn Provider>` is cloned per
  session at create time. Provider impls must be `Send + Sync` (the
  trait already requires this) and re-entrant — multiple concurrent
  `stream()` calls from different sessions must be safe.
- **INV-6 (no unsafe, no unwrap/expect/panic outside tests):**
  Standard.
- **INV-7 (no new workspace deps):** Use what's already in the
  workspace `[workspace.dependencies]`. As with mu-003: per-crate
  `Cargo.toml` files MAY add a workspace-listed dep to their own
  `[dependencies]` section.

## Interfaces

### `mu_core::agent::AgentLoop` amendment

Add one method:

```rust
impl AgentLoop {
    /// Clone the sender so external code can drive the loop without
    /// holding the AgentLoop handle. Used by mu-coding's session
    /// manager to avoid locking across await.
    pub fn sender(&self) -> tokio::sync::mpsc::Sender<AgentInput> {
        self.tx.clone()
    }
}
```

No new test required for this — every mu-004 test exercises it
indirectly. (If you want one anyway: clone, send, observe via the
existing AgentLoop's events.)

### `crates/mu-ai/src/faux.rs`

```rust
use std::sync::Mutex;
use std::collections::VecDeque;

use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use tokio::sync::oneshot;

use mu_core::agent::{
    AgentMessage, AssistantMessage, ContentBlock, Provider, ProviderError,
    ProviderEvent, StopReason, ToolSpec,
};

/// What a single FauxProvider::stream() call should produce.
#[derive(Debug, Clone)]
pub enum FauxResponse {
    /// Emit these events, in order.
    Script(Vec<ProviderEvent>),
    /// Echo the most recent user message back as a single TextDelta
    /// followed by Done(text + EndTurn).
    Echo,
}

/// Concrete Provider impl for testing and dev mode.
///
/// Two construction patterns:
/// - `scripted([resp1, resp2, ...])`: each `stream()` call pops the
///   next response off a FIFO queue. Out of responses → empty stream.
/// - `echo()`: every `stream()` call uses Echo mode.
pub struct FauxProvider {
    responses: Mutex<VecDeque<FauxResponse>>,
    /// If non-None, this is used when the queue is empty (instead of
    /// the empty-stream fallback).
    fallback: Option<FauxResponse>,
}

impl FauxProvider {
    pub fn scripted(responses: Vec<FauxResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            fallback: None,
        }
    }

    /// Echo always. Convenient default for `mu serve` smoke tests.
    pub fn echo() -> Self {
        Self {
            responses: Mutex::new(VecDeque::new()),
            fallback: Some(FauxResponse::Echo),
        }
    }
}

#[async_trait]
impl Provider for FauxProvider {
    async fn stream(
        &self,
        messages: &[AgentMessage],
        _tools: &[ToolSpec],
        _cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let response = {
            let mut q = self.responses.lock().expect("mutex poisoned");
            q.pop_front().or_else(|| self.fallback.clone())
        };
        let events = match response {
            None => Vec::new(),
            Some(FauxResponse::Script(es)) => es,
            Some(FauxResponse::Echo) => echo_events(messages),
        };
        Ok(Box::pin(stream::iter(events)))
    }
}

fn echo_events(messages: &[AgentMessage]) -> Vec<ProviderEvent> {
    let text = messages
        .iter()
        .rev()
        .find_map(|m| match m {
            AgentMessage::User { content } => Some(content.clone()),
            _ => None,
        })
        .unwrap_or_default();
    vec![
        ProviderEvent::TextDelta(text.clone()),
        ProviderEvent::Done(AssistantMessage {
            content: vec![ContentBlock::Text { text }],
            stop_reason: StopReason::EndTurn,
        }),
    ]
}
```

Tests in the same file (`#[cfg(test)] mod tests`):
- `echo_returns_last_user_message`
- `scripted_drains_in_fifo_order`
- `out_of_responses_returns_empty_stream`

### `crates/mu-coding/src/serve/sessions.rs`

```rust
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use mu_core::agent::{AgentInput, AgentLoop};

/// Per-session state held by the daemon.
struct SessionState {
    input_tx: mpsc::Sender<AgentInput>,
    /// Forwarder task — reads from the agent loop's events channel
    /// and translates to JSON-RPC notifications. We store the handle
    /// to keep it alive (storage is what keeps it scheduled; dropping
    /// JoinHandle in tokio detaches but does not abort).
    _forwarder: JoinHandle<()>,
    /// AgentLoop's JoinHandle. Stored for the same reason as above.
    _agent: JoinHandle<()>,
}

/// In-memory session registry.
#[derive(Clone)]
pub struct Sessions {
    inner: Arc<Mutex<HashMap<String, SessionState>>>,
}

impl Sessions {
    pub fn new() -> Self {
        Self { inner: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// Generate a unique session id. Counter-based, no UUID dep.
    pub fn next_id() -> String {
        static C: AtomicU64 = AtomicU64::new(1);
        format!("session-{}", C.fetch_add(1, Ordering::Relaxed))
    }

    /// Insert a new session. Caller has already spawned the agent
    /// loop and forwarder; this just stores their handles.
    pub fn insert(
        &self,
        id: String,
        input_tx: mpsc::Sender<AgentInput>,
        forwarder: JoinHandle<()>,
        agent: JoinHandle<()>,
    ) {
        // Wrap AgentLoop's JoinHandle<Outcome> into JoinHandle<()> via
        // tokio::spawn(async move { let _ = h.await; }) at the call site.
        self.inner
            .lock()
            .expect("sessions mutex poisoned")
            .insert(
                id,
                SessionState {
                    input_tx,
                    _forwarder: forwarder,
                    _agent: agent,
                },
            );
    }

    /// Get a clone of a session's input sender, briefly locking.
    /// Returns None if no such session.
    pub fn input_sender(&self, id: &str) -> Option<mpsc::Sender<AgentInput>> {
        self.inner
            .lock()
            .expect("sessions mutex poisoned")
            .get(id)
            .map(|s| s.input_tx.clone())
    }

    /// Remove a session. Dropping its SessionState drops the input_tx
    /// and the JoinHandles; the agent loop sees its input channel
    /// close and terminates naturally.
    pub fn remove(&self, id: &str) -> bool {
        self.inner
            .lock()
            .expect("sessions mutex poisoned")
            .remove(id)
            .is_some()
    }
}
```

### `crates/mu-coding/src/serve/forwarder.rs`

```rust
use tokio::sync::mpsc;

use mu_core::agent::AgentEvent;
use mu_core::protocol::{
    DoneEvent, ErrorEvent, TextDeltaEvent, ToolCallCompletedEvent,
    ToolCallStartedEvent, ToolOutcome,
};
use mu_core::transport::NotificationWriter;

/// Read AgentEvents from the loop, translate to JSON-RPC notifications,
/// emit via the writer. Exits when the events channel closes.
pub async fn forward_events(
    session_id: String,
    mut events_rx: mpsc::Receiver<AgentEvent>,
    notif: NotificationWriter,
) {
    while let Some(event) = events_rx.recv().await {
        match event {
            AgentEvent::TextDelta { delta } => {
                let _ = notif
                    .emit(
                        TextDeltaEvent::METHOD,
                        TextDeltaEvent { session_id: session_id.clone(), delta },
                    )
                    .await;
            }
            AgentEvent::ToolCallStarted { tool_call_id, tool_name, arguments } => {
                let _ = notif
                    .emit(
                        ToolCallStartedEvent::METHOD,
                        ToolCallStartedEvent { session_id: session_id.clone(), tool_call_id, tool_name, arguments },
                    )
                    .await;
            }
            AgentEvent::ToolCallCompleted { tool_call_id, content, is_error } => {
                let outcome = if is_error {
                    ToolOutcome::Err { message: content }
                } else {
                    ToolOutcome::Ok {
                        result: serde_json::Value::String(content),
                    }
                };
                let _ = notif
                    .emit(
                        ToolCallCompletedEvent::METHOD,
                        ToolCallCompletedEvent { session_id: session_id.clone(), tool_call_id, outcome },
                    )
                    .await;
            }
            AgentEvent::Done { stop_reason: _, turn_count: _ } => {
                let _ = notif
                    .emit(
                        DoneEvent::METHOD,
                        DoneEvent { session_id: session_id.clone(), usage: None },
                    )
                    .await;
            }
            AgentEvent::Error { message } => {
                let _ = notif
                    .emit(
                        ErrorEvent::METHOD,
                        ErrorEvent { session_id: session_id.clone(), message, detail: None },
                    )
                    .await;
            }
            // Lifecycle events not in mu-001's notification surface; drop.
            AgentEvent::AgentStart
            | AgentEvent::TurnStart
            | AgentEvent::TurnEnd
            | AgentEvent::MessageStart { .. }
            | AgentEvent::MessageEnd { .. } => {}
        }
    }
}
```

### `crates/mu-coding/src/serve/dispatch.rs`

The handler closure for `serve_stdio`. Takes `Request<Value>` +
`NotificationWriter`, returns `Response<Value>`. Branches on
`request.method`:

```rust
// Pseudocode shape — implementer fills in.

pub async fn dispatch(
    request: Request<Value>,
    notif: NotificationWriter,
    sessions: Sessions,
    provider: Arc<dyn Provider>,
) -> Response<Value> {
    match request.method.as_str() {
        PingRequest::METHOD => {
            ok_response(request.id, serde_json::to_value(PingResponse {
                pong: true,
                server_version: env!("CARGO_PKG_VERSION").into(),
            }).unwrap_or(Value::Null))
        }
        CreateSessionRequest::METHOD => {
            // Ignore params for v1 — the daemon's hardcoded provider
            // is used regardless of `request.params.provider`.
            let session_id = Sessions::next_id();
            let (events_tx, events_rx) = mpsc::channel(64);
            let agent = AgentLoop::spawn(provider.clone(), vec![], AgentConfig::default(), events_tx);
            let input_tx = agent.sender();
            // Detach AgentLoop into a JoinHandle<()> so we can store it.
            let agent_handle = tokio::spawn(async move {
                let _ = agent.join().await;
            });
            let forwarder = tokio::spawn(forward_events(session_id.clone(), events_rx, notif.clone()));
            sessions.insert(session_id.clone(), input_tx, forwarder, agent_handle);

            ok_response(request.id, serde_json::to_value(CreateSessionResponse {
                session_id,
            }).unwrap_or(Value::Null))
        }
        AskSessionRequest::METHOD => {
            // Parse params, look up session, send UserMessage.
            // ... see Behaviors.
        }
        CancelSessionRequest::METHOD => { /* ... */ }
        CloseSessionRequest::METHOD => { /* ... */ }
        _ => err_response(request.id, codes::METHOD_NOT_FOUND, format!("unknown method: {}", request.method)),
    }
}
```

The `unwrap_or(Value::Null)` calls in the success paths exist because
`serde_json::to_value` of a `serde::Serialize` struct can technically
fail (e.g., if a field's `Serialize` impl errors). For our types, it
won't — but the type signature requires we handle it. The fallback is
fine because this branch doesn't actually fire in practice and the
client gets a syntactically-valid (if semantically odd) response.

If you'd rather avoid the `unwrap_or`, change the return type to
`Result<Response<Value>, Response<Value>>` and propagate; same
behavior, more verbose.

### `crates/mu-coding/src/serve/mod.rs`

```rust
use std::sync::Arc;
use mu_core::agent::Provider;

mod dispatch;
mod forwarder;
mod sessions;

pub use sessions::Sessions;

pub async fn run(provider: Arc<dyn Provider>) -> anyhow::Result<()> {
    let sessions = Sessions::new();
    mu_core::transport::serve_stdio(move |req, notif| {
        let sessions = sessions.clone();
        let provider = provider.clone();
        async move {
            dispatch::dispatch(req, notif, sessions, provider).await
        }
    })
    .await
    .map_err(Into::into)
}
```

### `crates/mu-coding/src/bin/mu.rs` changes

```rust
use std::sync::Arc;
use anyhow::Result;
use clap::{Parser, Subcommand};
use mu_ai::FauxProvider;
use mu_core::agent::Provider;

// ... Cli, Command unchanged ...

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt() /* ... */ .init();
    let cli = Cli::parse();
    match cli.command {
        Command::Versions => { /* unchanged */ }
        Command::Serve => {
            // v1: hardcoded FauxProvider::echo. Real provider selection
            // is a future spec.
            let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
            mu_coding::serve::run(provider).await
        }
        Command::Ask { .. } | Command::Tui | Command::Orchestrate { .. } => {
            anyhow::bail!("this subcommand is not yet implemented; see specs/.")
        }
    }
}
```

Note: `mu-coding/Cargo.toml` needs `mu-ai = { path = "../mu-ai" }` added
under `[dependencies]` — that's a per-crate dep (per INV-7 / mu-003
amendment, this is NOT a "new workspace dep" since `mu-ai` is already a
workspace member; we're just declaring `mu-coding` depends on it).

## Behaviors

1. **B-1 (FauxProvider echo round-trip):** A `FauxProvider::echo()`
   given messages `[User { content: "hello" }]` returns a stream
   yielding `TextDelta("hello")` then `Done(AssistantMessage {
   content: [Text { text: "hello" }], stop_reason: EndTurn })`.

2. **B-2 (FauxProvider scripted FIFO):** A `FauxProvider::scripted(
   [Script(vec1), Script(vec2)])` returns `vec1` on the first call,
   `vec2` on the second, empty stream on the third.

3. **B-3 (FauxProvider out-of-responses):** A `FauxProvider::scripted(
   [])` returns an empty stream on every call (which the agent loop
   will treat as `Outcome::Error("provider stream ended without
   Done")` — that's correct error propagation, verified at the
   AgentLoop layer in mu-003 B-6).

4. **B-4 (serve smoke: ping):** Drive the JSON-RPC surface via
   `tokio::io::duplex`. Send a `Request<()>` with method `"ping"`,
   `id: 1`. Expect a `Response::Ok` with `id: 1` and `result.pong:
   true`. Verify via `serde_json::to_value` on the captured stdout
   line.

5. **B-5 (serve smoke: create + ask + done):** Spawn the serve task
   on duplex pipes with `FauxProvider::echo()`. Send
   `create_session` (with a `ProviderSelector::Anthropic_api` that
   gets ignored). Receive a response with `session_id: "session-N"`.
   Send `ask_session { session_id, user_message: "hello" }`. Receive
   the response (`accepted: true`) plus three notifications, in
   order, on the same channel:
   - `session.text_delta` with `delta: "hello"` and the matching
     session_id
   - `session.done` with the matching session_id
   - (Implementation-detail order: the `ask_session` response is
     emitted via the same outbound channel as the notifications, so
     it can be interleaved. The contract: `accepted: true` arrives,
     and EVENTUALLY the notifications. Tests assert the set of
     emitted lines, not strict ordering between response and
     notifications.)

6. **B-6 (serve cancel):** With a `FauxProvider` that returns
   `stream::pending()` (or the `MockProvider::pending` shape from
   mu-003 — bring it back as a public test helper or build inline
   in this test), send `create_session`, `ask_session`, then
   `cancel_session`. Within 500ms, observe a `session.error` (with
   message containing "cancel") OR a clean exit — implementer's
   choice but document it. The test asserts: cancellation completes
   within 500ms.

7. **B-7 (serve close cleans up):** After `close_session`, sending
   another `ask_session` for the same session returns an error
   response (`code: codes::INVALID_PARAMS`, message including
   "session not found"). This proves the session was actually
   removed from the map.

## Acceptance

- New files at exactly:
  - `crates/mu-ai/src/faux.rs`
  - `crates/mu-coding/src/serve/mod.rs`
  - `crates/mu-coding/src/serve/sessions.rs`
  - `crates/mu-coding/src/serve/dispatch.rs`
  - `crates/mu-coding/src/serve/forwarder.rs`
  - `crates/mu-coding/tests/serve_smoke.rs`
- Modified files at exactly:
  - `crates/mu-ai/src/lib.rs` (+2 lines: `pub mod faux;` and
    re-export)
  - `crates/mu-ai/Cargo.toml` (add per-crate deps: `mu-core`,
    `futures`)
  - `crates/mu-coding/src/lib.rs` (+1 line: `pub mod serve;`)
  - `crates/mu-coding/src/bin/mu.rs` (Serve arm + tokio::main)
  - `crates/mu-coding/Cargo.toml` (add per-crate dep: `mu-ai`,
    `futures` if needed by tests)
  - `crates/mu-core/src/agent/loop_.rs` (one-line `sender()` method)
- `cargo build` succeeds, no warnings.
- `cargo nextest run` passes — every existing test plus B-1..B-7
  (so 36 + 7 = 43 minimum).
- All new files under 800 lines.
- No `unsafe`, no `unwrap`/`expect`/`panic!`/`todo!`/`unimplemented!`
  outside `#[cfg(test)]`.

## Iteration-aware handoff protocol

Same shape as prior specs. Natural break points (also the
delegation-split boundary):

- **Part A (delegation candidate)**: `mu-ai/src/faux.rs` only,
  including its three behavior tests. ~150 lines, no architectural
  judgment beyond "follow the §Interfaces". Mechanical.
- **Part B (claude)**: serve wiring across mu-coding. Real
  judgment calls (lock granularity, task spawning, response/
  notification interleaving). The spec leaves the concrete order of
  ops in `dispatch.rs` open.

## Open questions

- [ ] OQ-1: Does `cancel_session`'s response come BEFORE or AFTER the
  agent's last events? — owner: defer — resolution: doesn't matter
  for v1; tests don't pin ordering. If a frontend cares, that's a
  future tightening.
- [ ] OQ-2: Should `close_session` await the agent loop's actual
  termination (via the JoinHandle) before responding? — owner:
  defer — resolution: no for v1. close_session is fire-and-forget
  removal from the map; the loop terminates async. If a caller wants
  synchronous cleanup, that's a different verb (e.g.,
  `terminate_session`) in a future spec.
- [ ] OQ-3: Should the daemon emit a `session.error` notification
  when an unknown method arrives? — owner: tcovert — resolution: no.
  Unknown method is per-request error (METHOD_NOT_FOUND in the
  Response), not a session event. `session.error` is reserved for
  errors WITHIN a session.

## Out-of-circuit warnings

- **OOC-1:** `Sessions::insert` takes `JoinHandle<()>` for both the
  forwarder and the agent. `AgentLoop::join` returns `Outcome`, not
  `()`. Wrap the `AgentLoop::join` call in a `tokio::spawn(async
  move { let _ = agent.join().await; })` to get a `JoinHandle<()>`.
  This is documented in the §Interfaces dispatch sketch but easy to
  miss.
- **OOC-2:** The `mu-coding/tests/` directory is a Cargo
  *integration-test* directory, not a unit-test module. Files there
  can `use mu_coding::*` like any other consumer. Don't put
  `#[cfg(test)] mod tests` inside `serve/dispatch.rs` for these
  end-to-end tests — those are unit tests' shape. Use
  `tests/serve_smoke.rs` for end-to-end and per-module unit tests
  for the smaller-scope checks.
- **OOC-3:** `serde_json::to_value(struct).unwrap_or(Value::Null)`
  is the documented fallback. Don't replace with `.unwrap()` (INV-6)
  or `.expect("infallible")` (also INV-6). The `unwrap_or` is the
  load-bearing pattern.
- **OOC-4:** `mu-coding`'s `Cargo.toml` adding `mu-ai` as a dep
  is the right call — but `mu-ai` doesn't already have its own
  per-crate `mu-core` dep. Add that too as part of this spec
  (`mu-ai` references `mu_core::agent::*` types).

## Prior work / context

- Spec mu-001 — JSON-RPC protocol types (PingRequest, etc.).
- Spec mu-002 — stdio transport (serve_stdio, NotificationWriter).
- Spec mu-003 — agent types, traits, and queue-driven loop.
- task_log entries tagged `mu,delegation`.
- memory `b22fd2c3` — streaming-primitive preference.
- The mu-002-attempt-1 post-mortem at
  `specs/delegations/mu-002-attempt-1-postmortem.md` — the WC fix
  rule applies to part-A delegation here too.

## Changelog

- 2026-05-10 — initial draft (claude-personal w/ tcovert review).
