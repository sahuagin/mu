# Spec: `session.provider_status` — observability for in-flight model calls

| field      | value                                       |
| ---------- | ------------------------------------------- |
| spec_id    | mu-035                                      |
| status     | proposed                                    |
| created    | 2026-05-11                                  |
| updated    | 2026-05-11                                  |
| authors    | tcovert + claude-personal (claude-opus-4.7) |
| supersedes | none                                        |

## Why

mu's existing wire surface emits notifications **only when the agent loop has something concrete to share** — `session.text_delta` (when tokens stream), `session.tool_call_started/completed`, `session.callout`, `session.done`. Between these events, the wire is silent.

That silence is structurally indistinguishable from a hang. A 30 s reasoning pass (no text yet), a slow first-token (provider warming a cold prefix), a stuck SSE stream, a stalled tool call, and a dead connection all look identical to a watching client: nothing on the wire. The orchestrator (mu_client.py, TUI, web UI, whatever) has no way to ask "is mu thinking or is it broken?"

This bit on 2026-05-11: the openai-codex backend stalled tonight, leaving sessions awaiting first-token for >100 s with zero client-side signal. The (already-deployed) mu_client.py defensive layer surfaces this as a *client-side* timeout with rich `waiting_for` context. mu-035 closes the gap on the *wire side* so the signal is authoritative, real-time, and shared across all clients (TUI, web UI, agent orchestrators, OTEL exporters).

CONVENTIONS apply.

After mu-035:

- A client subscribed to a session receives periodic `session.provider_status` notifications whenever the agent loop is in a non-streaming wait state. The notification names the wait kind (awaiting first token, thinking, tool executing, etc.), how long it has been in that state, and — when meaningful — how many bytes have arrived from the provider.
- A new RPC `session.cancel_outstanding { session_id, reason? } → { canceled: bool }` lets a client kill the current provider call **without ending the session**. The agent loop then decides what to do (retry on the same provider, fall over, surface an error, ask the human).
- A new daemon-level RPC `daemon.outstanding_calls → { calls: OutstandingCall[] }` enumerates every in-flight provider call across all sessions, with per-call elapsed_ms and provider/model identity. Critical for the TUI command-centre view (per `mu-ui-mockups-2026-05-10.md`).

## Scope

### In

- **Protocol additions** in `mu-core/src/protocol.rs`:
  - `ProviderStatusEvent { session_id: String, kind: ProviderStatusKind, started_at_unix_ms: u64, elapsed_ms: u64, bytes_received: Option<u64>, tool_call_id: Option<String> }` — emitted on the wire as `session.provider_status` notifications.
  - `ProviderStatusKind` enum: `AwaitingFirstToken`, `Streaming`, `Thinking`, `ToolExecuting`, `AwaitingToolResult`, `Idle`. Tag stable; future additions are additive.
  - `CancelOutstandingRequest { session_id: String, reason: Option<String> }` and `CancelOutstandingResponse { canceled: bool, was_in: ProviderStatusKind }` — wire method `session.cancel_outstanding`.
  - `OutstandingCall { session_id: String, kind: ProviderStatusKind, provider_kind: String, model: String, started_at_unix_ms: u64, elapsed_ms: u64 }` — element of `daemon.outstanding_calls` response.
- **Agent loop state machine** in `mu-core/src/agent/loop_.rs`:
  - New `ProviderStatusTracker` owned by the agent loop; transitions on every significant boundary (InvokeLlm start, first SSE byte, first content token, tool execute start, tool result received, etc.).
  - A tokio interval timer fires every `status_emit_interval_ms` (default 1000) while the tracker is in any non-streaming state and the current state's elapsed has exceeded a configurable floor (default 500 ms — avoid spam for short waits). Emits one `AgentEvent::ProviderStatus` per tick.
  - `Streaming` is treated as self-evidence: every `session.text_delta` already implies progress, so the tracker switches off periodic emission there. Resumes when a quiet gap exceeds `idle_threshold_ms` (default 2000 ms) mid-stream.
- **Forwarder** in `mu-coding/src/serve/forwarder.rs`:
  - Maps `AgentEvent::ProviderStatus` → wire `session.provider_status` notification. Also appends a compact event-log entry (`EventPayload::ProviderStatus`) so replays preserve the wait pattern.
- **Cancellation surface:** `session.cancel_outstanding` is a NEW dispatch handler (`handle_cancel_outstanding`) that signals a per-session `cancel_outstanding_tx` channel the agent loop watches alongside its existing `cancel_rx`. The agent loop treats this as a narrow cancel: aborts the in-flight provider stream, then surfaces a `ProviderEvent::CancelOutstanding { reason }` to the loop which records and continues to the next decision point (typically asks the model to recover or returns control to the human).
- **`daemon.outstanding_calls`** in `mu-coding/src/serve/dispatch.rs`:
  - Reads each `Sessions` entry's `ProviderStatusTracker` snapshot and returns the list. Lock-then-clone-then-drop pattern matches existing helpers (`input_sender`, `event_log`).
- **Per-session knob, daemon-wide default** for `status_emit_interval_ms` and `idle_threshold_ms`: settable via `create_session.options` (additive optional field) or daemon CLI flags `--status-interval-ms` / `--idle-threshold-ms`.
- **Tests:**
  - Unit: tracker transitions on a scripted sequence of agent events; verifies emission cadence and idle-threshold behaviour.
  - Integration: a forwarder test that runs the FauxProvider in a "delay 5s before first token" mode, asserts ≥3 `session.provider_status` notifications arrive during the wait, and asserts the final stream completes normally.
  - End-to-end smoke: dispatch a `session.cancel_outstanding` mid-stream; verify a `ProviderEvent::CancelOutstanding` lands in the event log and the session stays alive for a subsequent ask.

### Out

- **`session.cancel_outstanding` retry policy** — the agent loop's decision after a narrow cancel (retry once, fall over to another provider, surface to human) is governed by existing `RetryPolicy`/Capability mechanisms; mu-035 only provides the surgical-cancel primitive. Recovery policy is a separate spec when we need it.
- **OTEL / metrics export** — `daemon.outstanding_calls` is the primitive query; exporters (Prometheus, OTEL) layered on top come later (mu-038ish).
- **Cross-session deadline scheduling** — if a daemon decides that any call older than X should be auto-cancelled, that's a watchdog policy, not a primitive. Not in mu-035.
- **Streaming pause heuristics** — distinguishing "the model is *thinking* mid-stream" from "the connection has died mid-stream" is a richer state machine; v1 uses a simple `idle_threshold_ms` and lumps both as `Thinking`. Refinement is a follow-up.

## Invariants

- **INV-1 (clients are not required to handle `session.provider_status`).** Notification is purely additive. Clients that don't recognise the method per JSON-RPC 2.0 forward-compat rules ignore it. mu does not retry-on-no-ack; this is a fire-and-forget periodic.
- **INV-2 (no status emission during active streaming).** While `session.text_delta` notifications are arriving, the tracker is silent. Periodic emission resumes only after `idle_threshold_ms` of no text events.
- **INV-3 (cancel_outstanding does not end the session).** The session remains addressable. A subsequent `ask_session` or `respond_to_input_required` is valid. The cancel is recorded as an event so the audit trail is intact.
- **INV-4 (status emission survives a stuck provider).** Periodic emission runs on a tokio timer independent of the provider stream future. If the provider stream is blocked in a syscall, the timer still fires and the client still sees status. **This is the load-bearing property of the whole spec.**
- **INV-5 (one outstanding call per session).** mu's agent loop is serial within a session. `OutstandingCall` is therefore singular per session. `daemon.outstanding_calls` exposes the per-session value, fanned out.
- **INV-6 (cumulative elapsed survives idle states).** `started_at_unix_ms` and `elapsed_ms` reset on each state transition. A long sequence (awaiting → first byte → thinking → streaming → tool) emits multiple `ProviderStatusEvent`s with their own elapsed counters; cumulative wall-clock per call is computable from the event log.

## Wire surface

### Notification

```jsonc
// notification (no id)
{
  "jsonrpc": "2.0",
  "method": "session.provider_status",
  "params": {
    "session_id": "session-7",
    "kind": "awaiting_first_token",
    "started_at_unix_ms": 1763456789012,
    "elapsed_ms": 4500,
    "bytes_received": null,       // populated once an SSE byte arrives
    "tool_call_id": null          // populated only when kind=ToolExecuting / AwaitingToolResult
  }
}
```

### Cancel

```jsonc
// request
{
  "jsonrpc": "2.0",
  "id": 42,
  "method": "session.cancel_outstanding",
  "params": { "session_id": "session-7", "reason": "user cancelled via TUI" }
}

// response
{ "jsonrpc": "2.0", "id": 42, "result": { "canceled": true, "was_in": "awaiting_first_token" } }
```

If the session has no outstanding call (`was_in` would be `Idle`), `canceled` is `false` and the call is a no-op.

### Daemon-level enumeration

```jsonc
// request
{ "jsonrpc": "2.0", "id": 43, "method": "daemon.outstanding_calls", "params": {} }

// response
{
  "jsonrpc": "2.0", "id": 43,
  "result": {
    "calls": [
      {
        "session_id": "session-7",
        "kind": "awaiting_first_token",
        "provider_kind": "openai_codex",
        "model": "gpt-5.5",
        "started_at_unix_ms": 1763456789012,
        "elapsed_ms": 4500
      }
    ]
  }
}
```

## Implementation sketch

### Agent loop state machine

```rust
pub struct ProviderStatusTracker {
    state: ProviderStatusKind,
    state_started_at: Instant,
    bytes_received: u64,
    tool_call_id: Option<String>,
    // For periodic emission.
    last_emit_at: Instant,
    emit_interval: Duration,
    idle_threshold: Duration,
}

impl ProviderStatusTracker {
    pub fn transition(&mut self, new: ProviderStatusKind, now: Instant) -> Option<ProviderStatusEvent> {
        if new == self.state {
            return None;
        }
        // Always emit on transition into a non-streaming wait state.
        self.state = new;
        self.state_started_at = now;
        self.last_emit_at = now;
        if new != ProviderStatusKind::Streaming {
            Some(self.snapshot(now))
        } else {
            None
        }
    }

    pub fn tick(&mut self, now: Instant) -> Option<ProviderStatusEvent> {
        // No emit during active streaming.
        if matches!(self.state, ProviderStatusKind::Streaming | ProviderStatusKind::Idle) {
            return None;
        }
        if now.duration_since(self.state_started_at) < Duration::from_millis(500) {
            return None;  // too short to spam
        }
        if now.duration_since(self.last_emit_at) >= self.emit_interval {
            self.last_emit_at = now;
            Some(self.snapshot(now))
        } else {
            None
        }
    }

    fn snapshot(&self, now: Instant) -> ProviderStatusEvent { /* ... */ }
}
```

### Agent loop integration

```rust
// In the agent loop's main select! over (provider_event, cancel_rx, cancel_outstanding_rx, timer):
loop {
    tokio::select! {
        Some(ev) = provider_stream.next() => {
            tracker.handle_provider_event(&ev, Instant::now());
            // ... existing logic
        }
        _ = cancel_rx.recv() => {
            // Whole-session cancel — existing behavior.
        }
        Some(reason) = cancel_outstanding_rx.recv() => {
            provider_stream.abort();
            event_log.append(EventPayload::CancelOutstanding { reason });
            tracker.transition(ProviderStatusKind::Idle, Instant::now());
            // Loop continues; agent decides next action.
        }
        _ = tokio::time::sleep(emit_tick) => {
            if let Some(ev) = tracker.tick(Instant::now()) {
                emit(AgentEvent::ProviderStatus(ev));
            }
        }
    }
}
```

The crucial property: the `sleep(emit_tick)` arm runs even if `provider_stream.next()` is blocked. That is what makes status visible during a stalled provider call.

### Daemon registry

`Sessions` already stores per-session state under `Arc<Mutex<...>>`. Add an `Arc<Mutex<ProviderStatusTracker>>` field; the agent loop holds a clone, the dispatch handlers read snapshots for `daemon.outstanding_calls`.

## Tests

1. **Unit (tracker.rs):** scripted sequence — `AwaitingFirstToken` → tick (no emit, too short) → 600 ms → tick (emit) → 1.6 s → tick (emit) → `Streaming` (silence) → 3 s of no text → tick triggers `Thinking` emit. Asserts the exact emission sequence.
2. **Integration (forwarder.rs):** uses FauxProvider with a configurable first-token delay; runs an ask_session; asserts ≥3 `session.provider_status` notifications during the delay, then a `session.done` after.
3. **Integration (cancel_outstanding):** ask_session → wait 500 ms → `session.cancel_outstanding` → expect `was_in: AwaitingFirstToken`, `canceled: true`, the session stays alive, the event log contains a `CancelOutstanding` event, and a subsequent ask_session on the same session completes normally.
4. **Smoke (daemon.outstanding_calls):** spawn 2 sessions, dispatch a long ask on each (faux provider with delay), call `daemon.outstanding_calls`, assert both appear with the right `kind` and provider identity.

## Risks and follow-ups

- **Notification volume.** A periodic-every-1s tick across many concurrent sessions can flood the wire. Mitigation: clients that don't care can filter; daemon-side rate limits are added if real workloads expose pain.
- **Tracker accuracy depends on provider hookpoints.** Today only the agent loop knows "first byte received." Provider impls (`OpenaiCodexProvider`, `AnthropicProvider`, `OpenRouterProvider`) must emit a hint event when their SSE stream starts. The trait can grow an optional method, or — simpler — provider events already carry enough signal (`ProviderEvent::TextDelta` is the first-token boundary). Use the latter to avoid trait churn.
- **`Idle` is a soft state.** Between asks, the tracker reports `Idle`. `daemon.outstanding_calls` should filter `Idle` sessions out of the response (they aren't "outstanding").
- **OTEL / Prometheus exporters.** Not in scope, but the wire shape is designed to feed straight into them — `kind` is a low-cardinality label, `elapsed_ms` is a gauge, `bytes_received` is a counter. A future `mu-038` can wire OTEL on top.
- **TUI integration.** Per `mu-ui-mockups-2026-05-10.md`, the Command Center's "Live sessions" panel needs exactly this signal to render "phase: tool call: edit" or "phase: awaiting first token (4s)." The mockup already assumes the data is available — mu-035 is the spec that makes the assumption true.
- **Companion: mu_client.py defensive layer.** As of 2026-05-11 the orchestrator already has client-side timeouts + progress callbacks that *anticipate* `session.provider_status` semantically (the `waiting_for` string is the same idea as `ProviderStatusKind`). Once mu-035 lands, mu_client.py can switch from inferring state from "what we last received" to authoritative provider-status notifications, with the client-side timeouts as belt-and-suspenders.
