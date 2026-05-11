# Spec: `session.input_required` — approval flow for tool dispatch

| field      | value                                          |
| ---------- | ---------------------------------------------- |
| spec_id    | mu-029                                         |
| status     | ready (v1: Approve/Deny; AskOnce remember reserved for v2) |
| created    | 2026-05-10                                     |
| updated    | 2026-05-10                                     |
| authors    | tcovert + claude-personal (claude-opus-4.7)    |
| supersedes | none                                           |

## Why

The capability-delegation architecture
(`specs/architecture/capability-delegation.md`) introduced
`PermissionLevel::Ask` as a policy posture for tools that need
human approval before each invocation. mu-028 added the type but
left the runtime gate stubbed. This spec activates it:

- A tool whose policy is `PermissionLevel::Ask` causes the
  AgentLoop to emit a `session.input_required` notification and
  block dispatch until the client sends a matching
  `session.respond_to_input_required`.
- Decision = Approve → tool dispatches normally.
- Decision = Deny → synthesized `is_error: true` `ToolResult`
  explains the denial; tool does NOT run.

This is the primitive needed for Claude-Code-style "agent is
about to run X; allow it?" interactions, and it's the runtime
half of what Phase 2 of bash needs (memory `b27e6b4a` and the
`recon-bash.md` Phase-2 column).

CONVENTIONS apply.

## Scope

- **In:**
  - Protocol additions (mu-001):
    `InputRequiredEvent` (daemon→client notification, method
    `session.input_required`), `RespondToInputRequiredRequest`/
    `Response` (client→daemon, method
    `session.respond_to_input_required`), `ApprovalDecision`
    (`Approve` | `Deny`).
  - `AgentEvent::InputRequired` variant on the agent-loop side.
  - `Sessions::pending_approvals` registry (per-session
    `HashMap<request_id, oneshot::Sender<ApprovalDecision>>`).
  - Agent-loop gate in `handle_execute_tools`: when a tool's
    `PermissionLevel` is `Ask` or `AskOnce`, the loop generates a
    fresh `request_id`, inserts a oneshot in the pending registry,
    emits `AgentEvent::InputRequired`, then awaits the oneshot
    (racing against cancel/user-message-interrupt).
  - Dispatch handler `handle_respond_to_input_required` takes the
    oneshot out of the registry and sends the decision back.
  - Forwarder translates `AgentEvent::InputRequired` to
    `InputRequiredEvent` on the wire.

- **Out:**
  - **AskOnce remembering.** v1 treats `AskOnce` identically to
    `Ask` (prompts every time). v2 will persist "approved this
    tool for this session" so subsequent calls skip the prompt.
  - **Persisted approvals across sessions.** No `PermissionLevel::
    Always` honoring; v2 spec when there's a real consumer.
  - **Approval timeout.** v1 waits indefinitely for the response
    (or cancels via `session.cancel_session`). Per-prompt timeout
    is a future hardening item.
  - **Argument-scoped approval.** v1 grants per call_id (one
    prompt → one tool invocation). Future: "approve this command
    for the rest of this session" / "approve this exact path".
  - **Per-tool config file** (per-project `.mu/permissions.toml`).
    Deferred; daemon-startup CLI configuration is enough for v1.
  - **Default tools** declaring `Ask`. No existing tool changes
    its policy in this commit; this only activates the
    infrastructure. Future spec wires bash strict to `Ask`.

## Invariants

- **INV-1 (CONVENTIONS apply).**
- **INV-2 (denial is total).** A `Deny` decision means the tool
  body literally does not execute. The synthesized
  `ToolCallCompleted` event carries `is_error: true` and content
  that names the denial; the model sees this and can adjust.
- **INV-3 (cancel pre-empts approval).** If the agent loop is
  waiting on a `oneshot::Receiver<ApprovalDecision>` and a
  `Cancel` arrives, the loop drops the pending registry entry,
  returns `Outcome::Cancelled`, and exits — no leaked oneshot,
  no half-state.
- **INV-4 (request_id is opaque).** The format
  (`ask-<call_id>-<counter>`) is an implementation detail.
  Clients should round-trip it as an opaque string.
- **INV-5 (stale responses fail closed).** A
  `session.respond_to_input_required` for a `request_id` that
  isn't in the registry (already answered, expired, never
  existed) returns `accepted: false`. No partial-state success.

## Interfaces

### Protocol additions

```rust
// mu-001 protocol extensions
pub struct InputRequiredEvent {
    pub session_id: String,
    pub request_id: String,        // opaque token
    pub tool_call_id: String,      // ties to ToolCallStartedEvent
    pub tool_name: String,
    pub arguments: Value,
    pub summary: String,           // human-readable one-line
}
// METHOD: "session.input_required"

pub struct RespondToInputRequiredRequest {
    pub session_id: String,
    pub request_id: String,
    pub decision: ApprovalDecision,
}
// METHOD: "session.respond_to_input_required"

pub enum ApprovalDecision { Approve, Deny }  // snake_case on wire
```

### Agent-loop variant

```rust
AgentEvent::InputRequired {
    request_id: String,
    tool_call_id: String,
    tool_name: String,
    arguments: Value,
    summary: String,
}
```

### Sessions registry

```rust
pub type PendingApprovals = Arc<Mutex<
    HashMap<String, oneshot::Sender<ApprovalDecision>>
>>;

impl Sessions {
    pub fn take_pending_approval(
        &self, session_id: &str, request_id: &str,
    ) -> Option<oneshot::Sender<ApprovalDecision>>;
}
```

### Flow

```text
Agent loop wants to call tool T with args A
  tool.spec().policy.permission == Ask
  → loop generates request_id
  → loop inserts (request_id, oneshot_tx) into Sessions.pending_approvals
  → loop emits AgentEvent::InputRequired
    → forwarder translates to session.input_required notification
    → wire emits to client
  → loop awaits oneshot_rx (racing against cancel/user input)

Client receives session.input_required
  → presents to human (or auto-approves per policy)
  → sends session.respond_to_input_required { request_id, decision }

Daemon dispatch::handle_respond_to_input_required
  → Sessions::take_pending_approval(session_id, request_id) → Some(sender)
  → sender.send(decision)
  → returns { accepted: true } to client

Agent loop's oneshot receives the decision
  → Approve: dispatches tool as normal
  → Deny: synthesizes is_error=true ToolResult, skips dispatch
  → emits ToolCallCompleted with the result
  → continues the loop
```

## Behaviors

1. **B-1 (Approve dispatches tool):** MockProvider scripted to
   issue one tool call. MockTool with
   `policy.permission = Ask`. Drive AgentLoop, wait for
   `AgentEvent::InputRequired`, send `Approve` via the registered
   oneshot, observe `ToolCallCompleted` with `is_error: false` and
   the tool's actual output content. **Implemented in
   `loop_tests::ask_permission_emits_input_required_and_dispatches_on_approve`.**

2. **B-2 (Deny synthesizes error, does not run tool):** Same
   setup, send `Deny`. Observe `ToolCallCompleted` with
   `is_error: true`, content contains "denied"; the tool's
   internal "should not appear" string is NOT in the result.
   **Implemented in
   `loop_tests::ask_permission_deny_synthesizes_error_result_without_running_tool`.**

3. **B-3 (Cancel mid-approval):** Loop is awaiting decision.
   Send `AgentInput::Cancel`. Loop returns `Outcome::Cancelled`;
   the pending-approvals entry is removed; no panic. *Not
   explicitly unit-tested in v1 — relies on the cancel path's
   existing test coverage.*

4. **B-4 (Stale response is no-op):** Client sends
   `respond_to_input_required` for a `request_id` not in the
   registry. Daemon returns `accepted: false`. **Implemented in
   `sessions::tests::take_pending_approval_round_trips_via_oneshot`.**

5. **B-5 (Protocol round-trip):** `InputRequiredEvent` +
   `RespondToInputRequiredRequest` + `ApprovalDecision` serde
   round-trip. **Implemented in `protocol::tests::*`.**

## Acceptance

- Modified files:
  - `crates/mu-core/src/protocol.rs` — new request/response/event
    types
  - `crates/mu-core/src/agent/loop_.rs` — `AgentEvent::InputRequired`
    variant; `PendingApprovals` type alias; Ask gate in
    `handle_execute_tools`
  - `crates/mu-core/src/agent/loop_tests.rs` — B-1, B-2 tests
  - `crates/mu-coding/src/serve/sessions.rs` — `pending_approvals`
    field; `take_pending_approval` accessor
  - `crates/mu-coding/src/serve/dispatch.rs` — pass approvals
    registry to AgentLoop; new `handle_respond_to_input_required`
  - `crates/mu-coding/src/serve/forwarder.rs` — translate
    `AgentEvent::InputRequired` → `InputRequiredEvent`; skip in
    log projection (transient prompt; outcome is recorded as
    ToolCall/ToolResult)
- `cargo build` clean.
- `cargo nextest run` passes (253 → 255+).
- No existing tool uses `PermissionLevel::Ask` yet — this commit
  ships the primitive; tool wiring is a follow-up spec.

## Out-of-circuit warnings

- **OOC-1 (model still sees the call started).** The agent loop
  emits `ToolCallStarted` BEFORE the gate. If a deny ultimately
  happens, the wire-level transcript shows "call started, then
  denied, then call completed (with error)". This is intentional
  — observers should see what the agent attempted, not just what
  succeeded. Frontends can dim or strike-through denied calls.

- **OOC-2 (no timeout means hung sessions on no UI).** If a
  client never responds and never cancels, the agent loop waits
  forever. v2 should add a per-prompt timeout that synthesizes a
  Deny after N minutes. For v1, the daemon's `cancel_session` RPC
  is the recovery path.

- **OOC-3 (AskOnce is currently Ask).** Marking a tool as
  `PermissionLevel::AskOnce` works but prompts every time in v1.
  The v2 spec will distinguish them by persisting "user approved
  this tool name during this session" in the Sessions registry.

- **OOC-4 (no auto-approve config).** v1 has no "default to
  approve" mode. For unattended use, mark tools as
  `PermissionLevel::Allow` (default) instead of `Ask`. Future
  config-file support will let users mark "auto-approve `Ask`
  tools by default" but that's a deliberate trust choice.

## Prior work / context

- `specs/architecture/capability-delegation.md` — defines the
  `PermissionLevel` enum this spec activates.
- mu-026 (bash phase 1) — recon doc identified
  `session.input_required` as phase 2 of bash.
- mu-028 — added `ToolPolicy` to `ToolSpec`; this spec is the
  first runtime consumer of `policy.permission`.
- Memory `b27e6b4a` — biscuit-direction; biscuits will eventually
  attenuate which tools a session has access to, but the
  per-call approval gate is a separate primitive that composes.

## Changelog

- 2026-05-10 — initial draft + impl (claude-personal).
