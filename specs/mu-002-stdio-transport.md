# Spec: stdio JSON-RPC transport for `mu-core`

| field      | value                                          |
| ---------- | ---------------------------------------------- |
| spec_id    | mu-002                                         |
| status     | ready                                          |
| created    | 2026-05-09                                     |
| updated    | 2026-05-09                                     |
| authors    | tcovert + claude-personal (claude-opus-4.7)    |
| supersedes | none                                           |

## Why

`mu-001` defined the protocol *types*. This spec defines how those types
move over a real wire. Specifically: newline-delimited JSON over
stdin/stdout, async via tokio, with concurrent request dispatch and a
notification path that doesn't block the read loop.

`mu serve` becomes a real (if useless) JSON-RPC daemon after this
lands. Frontends can connect (`mu ask` will spawn it as a child
subprocess and speak JSON-RPC over the child's stdio). Tests can drive
the same code paths without a subprocess by using in-memory pipes.

## Scope

- **In:**
  - `crates/mu-core/src/transport.rs` — newline-delimited JSON-RPC
    transport.
  - `pub mod transport;` re-export in `lib.rs` (one line).
  - Public API: `serve_stdio(handler)`, generic `serve(reader, writer,
    handler)` for tests, `NotificationWriter` for handlers to emit
    notifications, `TransportError` for errors that escape the
    transport, helper constructors `ok_response`, `err_response`, and a
    `codes` module with the standard JSON-RPC 2.0 error codes.
  - Concurrent request dispatch: handlers run on `tokio::spawn`, so
    a slow `ask_session` doesn't block a subsequent `cancel_session`.
  - Notification ordering: notifications and responses share an
    outbound mpsc channel; whoever sends first writes first. No
    interleaving (each line is a complete JSON object).
  - Tests using `tokio::io::duplex` for in-memory request/response and
    notification-emission round-trips.

- **Out:**
  - Unix sockets, TCP, websockets — separate spec(s) when needed.
  - Authentication, capability negotiation, heartbeats.
  - Backpressure beyond the bounded mpsc channel.
  - Per-method handler trait. The caller dispatches by method string
    inside their single handler closure. Trait machinery is bigger
    than this spec; revisit if/when more than one binary speaks
    JSON-RPC.

- **Non-goals:**
  - "Out of band" comms (mcp_agent_mail-style peer mailboxes) —
    those are an MCP-tool concern, not a transport concern. Tracked
    for a future spec (`mu-NNN-mcp-client`).
  - In-band "agent asks parent for input" — needs a request type
    (`session.input_required` notification + `provide_input` request)
    that mu-001 doesn't yet define. Tracked for a future spec
    (`mu-NNN-input-required`).

## Invariants

- **INV-1:** Every line on the wire is exactly one JSON value followed
  by `\n`. No multi-line JSON. No CRLF — single LF only.
- **INV-2:** Reader is line-oriented and tolerant: a malformed line
  produces a `parse error` Response (-32700) for the offending request
  *if* an `id` can be recovered, or is logged and skipped if not.
  The transport DOES NOT crash on malformed input.
- **INV-3:** Notifications and responses both flow through a single
  bounded mpsc channel (capacity 64) to a single writer task. There
  is no other path that writes to the wire. This is the property
  that makes "no line interleaving" guaranteed without locks.
- **INV-4:** Handlers run on `tokio::spawn` — concurrent dispatch is
  required, not optional. A handler that holds the read loop blocks
  cancellation. (See §B-5.)
- **INV-5:** `NotificationWriter::emit` returns `Ok(())` even if the
  channel is closed — i.e., emission failures are logged via
  `tracing::warn!`, not propagated. Reason: from the handler's
  perspective, the daemon is shutting down or the client disconnected;
  neither is the handler's bug. The handler should keep running so it
  can clean up state.
- **INV-6:** Module length < 800 lines including tests.
- **INV-7:** No `unsafe`, no `unwrap`/`expect` outside `#[cfg(test)]`.
- **INV-8:** No new dependencies. The mu-core `Cargo.toml` already has
  `tokio`, `serde`, `serde_json`, `thiserror`, `tracing`, and
  `async-trait`. Use only those plus stdlib.

## Interfaces

```rust
// crates/mu-core/src/transport.rs

use std::sync::Arc;
use std::pin::Pin;
use std::future::Future;

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::protocol::{ErrorObject, Notification, Request, Response, JSONRPC_VERSION};

// ===== Public API =====

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    /// Outbound channel closed before all messages were flushed.
    #[error("outbound channel closed")]
    OutboundClosed,
}

/// Standard JSON-RPC 2.0 error codes.
pub mod codes {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
}

/// Build a successful Response<Value>. Caller has already serialized
/// the result type into a Value.
pub fn ok_response(id: Value, result: Value) -> Response<Value> {
    Response::Ok {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id,
        result,
    }
}

/// Build an error Response<Value>.
pub fn err_response(id: Value, code: i32, message: impl Into<String>) -> Response<Value> {
    Response::Err {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id,
        error: ErrorObject {
            code,
            message: message.into(),
            data: None,
        },
    }
}

/// Handle on a single shared outbound channel. Cheap to clone (Arc-y
/// internally). Pass into request handlers so they can emit
/// notifications mid-flight.
#[derive(Clone, Debug)]
pub struct NotificationWriter {
    tx: mpsc::Sender<Outbound>,
}

impl NotificationWriter {
    /// Emit a notification. Returns `Ok(())` even if the channel is
    /// closed — see §INV-5.
    pub async fn emit<P: Serialize>(&self, method: &str, params: P) -> Result<(), TransportError> {
        let params = serde_json::to_value(params)?;
        let notif = Notification {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.to_string(),
            params,
        };
        let value = serde_json::to_value(&notif)?;
        if self.tx.send(Outbound(value)).await.is_err() {
            tracing::warn!("notification dropped: outbound channel closed");
        }
        Ok(())
    }
}

/// Convenience: serve over the process's actual stdin/stdout.
pub async fn serve_stdio<F, Fut>(handler: F) -> Result<(), TransportError>
where
    F: Fn(Request<Value>, NotificationWriter) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<Value>> + Send + 'static,
{
    let stdin = BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    serve(stdin, stdout, handler).await
}

/// Generic transport: read newline-delimited JSON requests from
/// `reader`, dispatch each to `handler` on `tokio::spawn`, write
/// responses and notifications back to `writer`.
pub async fn serve<R, W, F, Fut>(
    reader: R,
    mut writer: W,
    handler: F,
) -> Result<(), TransportError>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    F: Fn(Request<Value>, NotificationWriter) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<Value>> + Send + 'static,
{
    // ... see §Behaviors for the algorithm; spec is the contract,
    // not the implementation. Implementer chooses how to structure
    // the read-task / writer-task / dispatcher within these constraints.
}

// ===== Internal =====

/// Anything destined for the outbound channel: a serialized Response
/// or Notification, already as a Value so it can be flushed without
/// re-borrowing the type. Pub(crate) only.
#[derive(Debug)]
pub(crate) struct Outbound(pub(crate) Value);
```

## Behaviors

1. **B-1 (line framing):** Each request is read as one line up to and
   including a single `\n`. Each response and notification is written
   as one line of compact JSON followed by exactly one `\n`. A test
   verifies this by feeding `serde_json::to_string(&request) + "\n"`
   on a `tokio::io::duplex` and asserting the response on the other
   side ends with exactly one `\n`.

2. **B-2 (round-trip request):** Given a handler that returns
   `ok_response(req.id, json!({"pong": true}))`, sending a
   `Request<Value> { jsonrpc: "2.0", id: 1, method: "ping", params: null }`
   in produces a `Response::Ok` out with the same id and the result
   payload. Verified via duplex stream + `from_str` round-trip.

3. **B-3 (notification emission):** A handler that calls
   `notif.emit("session.text_delta", json!({"session_id": "s", "delta": "hi"})).await`
   *before* returning its response causes the notification line to
   appear on the wire *before* the response line. Test by reading
   two lines from the writer side and asserting line 1 is the
   notification, line 2 is the response.

4. **B-4 (parse error → error response):** Sending a malformed line
   (`"{not valid json\n"`) produces an error response with
   `code: codes::PARSE_ERROR`, `id: null`. Sending a syntactically
   valid request that lacks `method` produces
   `code: codes::INVALID_REQUEST`. Neither crashes the transport;
   subsequent valid requests still process.

5. **B-5 (concurrent dispatch):** Two requests sent back-to-back
   where the first handler `tokio::time::sleep(Duration::from_millis(50))`s
   produce the SECOND response first. This proves §INV-4: handlers
   run concurrently. Test verifies by id ordering on the wire.

6. **B-6 (eof on read terminates serve, drains writes):** When the
   reader hits EOF, `serve` returns `Ok(())` after the writer task
   has drained any in-flight notifications/responses from the channel.
   Test by sending one request (with response) then closing the
   reader, asserting the response was written before `serve` returned.

7. **B-7 (NotificationWriter is Clone and Send):** Compile-time test
   `static_assertions::assert_impl_all!(NotificationWriter: Send, Sync, Clone);`
   — or hand-rolled if static_assertions not in deps:
   `fn assert_send<T: Send>() {} ; assert_send::<NotificationWriter>();`
   inside a test. Verifies handlers can clone the writer into spawned
   tasks for streaming.

8. **B-8 (id is preserved as Value):** A request with `id: "abc"`
   (string) gets a response with `id: "abc"` (string), not coerced
   to anything else. A request with `id: 7` gets `id: 7`. A request
   with `id: null` (which is technically a notification per the
   JSON-RPC spec — but pi_ts and we both accept a real id of `null`
   from peers that disregard the spec) gets `id: null` echoed back
   and a normal response is produced.

## Acceptance

- File created at `crates/mu-core/src/transport.rs`.
- `pub mod transport;` added to `crates/mu-core/src/lib.rs` (single
  line; the existing `pub mod protocol;` line stays put).
- `cargo build -p mu-core` succeeds, no warnings.
- `cargo nextest run -p mu-core` passes — all of B-1..B-8 plus the
  existing `protocol` tests plus `version_is_nonempty`.
- Module file length under 800 lines including tests.
- No `unsafe`, no `unwrap`/`expect` outside `#[cfg(test)]`.
- No new dependencies in `Cargo.toml`.
- Diff touches exactly: `crates/mu-core/src/transport.rs` (new),
  `crates/mu-core/src/lib.rs` (+1 line). No other files.

## Iteration-aware handoff protocol

Codex CLI iteration budget assumed at 50 (same as pi-rust). At ~40
iterations:
1. Commit current state to a branch named `spec/mu-002-handoff` with
   a commit message describing what was completed.
2. Write `next_agent_starting_position` summarizing which §Behaviors
   are green and which are missing.
3. Exit cleanly. Next agent re-reads this spec + handoff branch.

This task is meaningfully harder than mu-001 — concurrent tokio code
with a dispatch loop, a writer task, a channel, and tests using
`tokio::io::duplex`. If gpt-pro hits the cap, the partial transport
implementation likely already has the data structures in place; the
next-agent should be able to finish with the Behaviors block as a
checklist.

## Open questions

- [ ] OQ-1: Should `serve` accept an `AbortHandle`-style cancel
  primitive so the daemon can shut down cleanly mid-run? — owner:
  defer — resolution: deferred. v1 relies on closing stdin to
  terminate; explicit cancellation is `tokio::select!` over a
  `CancellationToken` and can be added without breaking the public
  surface.
- [ ] OQ-2: Should responses to notifications-disguised-as-requests
  (id: null per spec) be suppressed? — owner: tcovert — resolution:
  no, see §B-8. We're permissive about id: null and respond anyway,
  matching pi_ts's posture. Strict compliance is a v2 concern.
- [ ] OQ-3: Backpressure behavior when the outbound channel fills
  (capacity 64) — does `NotificationWriter::emit` block, or drop? —
  owner: tcovert — resolution: emit blocks (it's an `await` on
  `mpsc::Sender::send`). Channel full is a real backpressure signal,
  not a reason to drop. Logged a warning for visibility.

## Out-of-circuit warnings

- **OOC-1:** `serde_json::to_string` vs `serde_json::to_string_pretty`.
  Wire format is compact (no whitespace). Implementer must use
  `to_string`, NOT `to_string_pretty`. Hard to test directly (the
  pretty form is still parseable JSON), so check the line for
  newlines: a compact response is one line; the pretty form contains
  embedded `\n`. §B-1 catches this if the test verifies "exactly one
  `\n` at end."
- **OOC-2:** `tokio::io::stdout()` is buffered. After every line
  write, call `.flush().await` so frontends see output without
  waiting for a buffer fill. Forgetting flush is the #1 cause of
  "the daemon hung" reports.
- **OOC-3:** The `where` bounds on `serve` are dense. The handler
  closure must be `Fn` (not `FnMut`) because it'll be cloned into
  spawned tasks via `Arc<F>`. The future must be `Send + 'static`
  because it'll cross thread boundaries. Compiler errors for these
  are notoriously confusing; if implementer hits one, the answer is
  almost always "the handler is capturing a non-Send type" — fix by
  cloning the captured value before the `tokio::spawn` block.
- **OOC-4:** `BufReader::lines()` returns a `Lines` stream that
  consumes the reader. The spec says reader is `R: AsyncBufRead +
  Unpin`; implementer should use `reader.lines()` directly without
  a re-wrap. Calling `BufReader::new(reader)` on an already-buffered
  reader is allowed but wastes a layer.

## Prior work / context

- spec mu-001 (`specs/mu-001-protocol-types.md`) — the types this
  transport carries.
- pi_ts's `modes/rpc/jsonl.ts` (58 lines) — their newline-framed JSON
  parser. Reference, not copy. Located at
  `~/src/public_github/pi/packages/coding-agent/src/modes/rpc/jsonl.ts`.
- pi_ts's `modes/rpc/rpc-mode.ts` (754 lines) — their dispatcher
  loop. Much bigger surface than mu-002 because it owns ALL the
  command/response logic too; we're separating transport from
  dispatch (which lives in `mu-coding`).
- task_log entry b989f74f — the mu workspace scaffold.
- task_log entry from mu-001 delegation (TBD; will fill in after this
  spec lands).

## Changelog

- 2026-05-09 — initial draft (claude-personal w/ tcovert review).
