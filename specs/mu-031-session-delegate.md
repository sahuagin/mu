# Spec: `session.delegate` — sub-session primitive

| field      | value                                          |
| ---------- | ---------------------------------------------- |
| spec_id    | mu-031                                         |
| status     | ready (v1 MVP)                                 |
| created    | 2026-05-10                                     |
| updated    | 2026-05-10                                     |
| authors    | tcovert + claude-personal (claude-opus-4.7)    |
| supersedes | none                                           |

## Why

The capability-delegation architecture
(`specs/architecture/capability-delegation.md`) and the TUI session-
tree memory (`7e44f7ad`) both assume that mu sessions form a tree:
a root session at the top, with children spawned by `session.delegate`
calls, each child potentially spawning further sub-sessions. Until
this spec lands, mu had no way to express that lineage at the
protocol level — every session was a sibling root.

This spec ships the **minimum useful primitive** for sub-sessions:

- New RPC `session.delegate { parent_session_id, provider,
  branched_at_parent_event_id? } → { child_session_id }`.
- Child sessions are fully independent at the runtime level — own
  agent loop, own event log, own pending-approvals registry — but
  carry a reference to their parent for audit, tree queries, and
  (future) biscuit attenuation.
- The child's `SessionCreated` event records `parent_session_id`
  and the optional branch point in the parent's log.

Out of scope here (future specs): biscuit minting + attenuation
(needs this primitive but is its own design), context-inheritance
from parent (copying parent's message history into the child),
parent-side `ChildSpawned` event in the parent's log, tree-query
RPCs (`session.children`, `session.subtree_stats`), TUI session-
tree view.

CONVENTIONS apply.

## Scope

- **In:**
  - Protocol: `DelegateSessionRequest { parent_session_id,
    provider, branched_at_parent_event_id? }` and
    `DelegateSessionResponse { child_session_id }`. METHOD =
    `"session.delegate"`.
  - `EventPayload::SessionCreated` gains two optional fields:
    `parent_session_id`, `branched_at_parent_event_id`. Both
    `None` for root sessions; `Some(...)` for delegates.
  - `SessionState` gains a `parent_session_id: Option<String>`
    field, populated for delegated sessions.
  - Dispatch: shared `build_and_register_session` helper called by
    both `create_session` (root path) and `session.delegate` (child
    path). The two RPCs differ only in their wire response shape
    and in whether they pass parent info.
  - Validation: delegating to a parent_session_id that doesn't
    exist in the Sessions registry returns `INVALID_PARAMS`.
  - Tests: protocol round-trip; end-to-end smoke via `serve_smoke`
    drives `create → delegate → stats(child) → delegate(bad)`.

- **Out:**
  - **Biscuit attenuation** (mu-032 or later). The child currently
    inherits the daemon's full tool set, just like any root
    session. Biscuit minting + per-tool-call verification at
    dispatch is the next big piece.
  - **Inherited message history.** v1 children start with an empty
    message history. Future work: `inherit_history: bool` field
    plus a way to specify "branch at parent's event N and seed
    child with messages 0..=N from parent's log."
  - **Parent-side `ChildSpawned` event.** The parent's log
    currently doesn't record that it spawned a child. Helpful for
    audit; not load-bearing for v1. Add when tree-rollup queries
    need it.
  - **Cross-daemon delegation.** v1 is same-daemon only. A future
    `session.delegate` extension might mint a JSON-RPC handle to
    a session in a peer daemon (paired with the cooperating-
    sessions / mailbox direction).
  - **Tree-query RPCs** (`session.children`, `session.subtree_*`).
    SessionState now carries `parent_session_id`; the rollup
    queries can be added when consumers exist.
  - **Branched-at validation.** v1 accepts any
    `branched_at_parent_event_id` value without verifying it
    points at a real event in the parent's log. The field is
    recorded as provided. Future hardening can validate.

## Invariants

- **INV-1 (CONVENTIONS apply).**
- **INV-2 (child runtime independence).** A child session has its
  own agent loop, event log, and pending-approvals registry. The
  parent's state is not mutated by child operations. Closing the
  parent does NOT close the child (closing a session removes it
  from the registry; the child stays).
- **INV-3 (parent must exist at delegate time).** `session.delegate`
  with a `parent_session_id` not in the registry returns
  `INVALID_PARAMS` immediately. The check is a snapshot — if the
  parent is closed mid-delegation (between the check and the
  child's registration), the child still gets created, since the
  parent-id is only used for audit at this point. Race-free
  parent verification (e.g., requiring an unbroken chain to
  root) is a future hardening concern.
- **INV-4 (provider is independent).** The child's provider is
  whatever the request specifies, not what the parent uses. The
  per-session provider work from mu-020 already supports this.
- **INV-5 (lineage is event-recorded).** The child's first event
  (`SessionCreated`) carries `parent_session_id` and optionally
  `branched_at_parent_event_id`. This is the durable record;
  `SessionState.parent_session_id` is the in-memory cache.

## Interfaces

### Protocol

```rust
pub struct DelegateSessionRequest {
    pub parent_session_id: String,
    pub provider: ProviderSelector,
    pub branched_at_parent_event_id: Option<u64>,
}
// METHOD: "session.delegate"

pub struct DelegateSessionResponse {
    pub child_session_id: String,
}
```

### Event log

```rust
// EventPayload::SessionCreated gains:
pub enum EventPayload {
    SessionCreated {
        provider_kind: String,
        model: String,
        parent_session_id: Option<String>,
        branched_at_parent_event_id: Option<u64>,
    },
    // ... existing variants unchanged
}
```

Both new fields are `#[serde(default, skip_serializing_if = "Option::is_none")]`
so legacy logs (events serialized before mu-031) decode cleanly
with `None`.

### Sessions registry

```rust
struct SessionState {
    // ... existing fields ...
    parent_session_id: Option<String>,
}

impl Sessions {
    pub fn insert(
        // ... existing args ...
        parent_session_id: Option<String>,
    );
}
```

### Dispatch

```rust
fn handle_delegate_session(...) -> Response<Value> {
    // Verify parent exists.
    // Call build_and_register_session with Some(parent_session_id).
    // Return { child_session_id }.
}

/// Shared helper used by create_session and delegate_session.
fn build_and_register_session(
    selector: &ProviderSelector,
    parent_session_id: Option<String>,
    branched_at_parent_event_id: Option<u64>,
    notif: NotificationWriter,
    sessions: Sessions,
    factory: ProviderFactory,
    tools: Arc<Vec<Arc<dyn Tool>>>,
) -> Result<String, String>;
```

## Behaviors

1. **B-1 (delegate creates child with parent reference):**
   create_session for parent → session.delegate with parent_id +
   provider → response carries child_session_id, distinct from
   parent. Both sessions queryable via `session.stats`. **Tested
   in `serve_smoke::b9_session_delegate_creates_child`.**

2. **B-2 (child provider is independent):** Parent uses
   `openrouter`; delegate request specifies `anthropic_api`.
   `session.stats(child)` returns `provider_kind = "anthropic_api"`.
   **Same test as B-1.**

3. **B-3 (delegate to missing parent fails clean):** Request with
   `parent_session_id = "session-does-not-exist"` returns
   `INVALID_PARAMS`. **Same test as B-1.**

4. **B-4 (protocol round-trips):** `DelegateSessionRequest`
   round-trips through serde. Optional `branched_at_parent_event_id`
   omitted from wire when None. **Tested in `protocol::tests::*`.**

## Acceptance

- New files:
  - `specs/mu-031-session-delegate.md` (this)
- Modified files:
  - `crates/mu-core/src/event_log.rs` —
    `EventPayload::SessionCreated` gains optional parent fields
  - `crates/mu-core/src/protocol.rs` — new types + tests
  - `crates/mu-coding/src/serve/sessions.rs` — `SessionState`
    parent field; `insert` arg; tests
  - `crates/mu-coding/src/serve/dispatch.rs` — `handle_delegate_session`;
    shared `build_and_register_session` helper; create_session
    refactored to call it
  - `crates/mu-coding/tests/serve_smoke.rs` — B-9 smoke test
- `cargo build` clean.
- `cargo nextest run` passes (255 → 259).

## Out-of-circuit warnings

- **OOC-1 (no biscuits yet means no enforcement).** A child
  session today can call any tool a root session could call —
  there's no capability narrowing. mu-032 (or later) wires
  biscuits in to enforce "child can use these tools, parent
  can't widen." Until then, delegation is purely an organizational
  primitive, not a security boundary.

- **OOC-2 (parent close + child orphan).** Closing the parent
  session doesn't close its children. This is intentional for v1
  — delegates may have work to finish independently. But a future
  consumer might want "close session and all its descendants"
  semantics; that's a separate spec.

- **OOC-3 (no message inheritance).** The child starts fresh.
  If you want the agent to "continue from where the parent was,"
  the caller has to manually copy the relevant context into the
  child's first user message. Future `inherit_history` flag will
  automate this.

- **OOC-4 (branched_at_parent_event_id is unverified).** v1
  accepts any u64 without checking that it corresponds to a real
  event in the parent's log. The audit record is "claimed branch
  point" — useful for replay tooling that the caller designed,
  not a verified causal link.

## Prior work / context

- `specs/architecture/capability-delegation.md` — the architecture
  doc that names sub-session as the missing primitive for biscuit
  attenuation.
- Memory `7e44f7ad` — TUI session-tree design depends on this
  primitive for any tree to render.
- Memory `12e112e9` — escalation-primitive memory; future
  PermissionLevel::Ask routing depends on the delegation chain
  shipped here.
- mu-025 — event log primitive; SessionCreated event extended
  here.
- mu-020 — per-session provider; the child's provider being
  independent of the parent's is "free" because of that work.

## Changelog

- 2026-05-10 — initial draft + impl (claude-personal).
