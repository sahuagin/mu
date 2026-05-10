# Spec: `session.callout` primitive

| field      | value                                          |
| ---------- | ---------------------------------------------- |
| spec_id    | mu-016                                         |
| status     | ready                                          |
| created    | 2026-05-10                                     |
| updated    | 2026-05-10                                     |
| authors    | tcovert + claude-personal (claude-opus-4.7)    |
| supersedes | none                                           |

## Why

Existing typed events (`text_delta`, `tool_call_*`, `done`,
`error`) handle the load-bearing structured surfaces. They don't
cover the wider set of "the agent has something to say outside the
main response stream":

- agent observations ("I noticed a typo on line 5")
- memory recalls ("found in memory: …")
- system notices ("approaching context limit")
- peer-agent messages (cooperating-sessions design, memory `d22f391a`)
- hints, warnings, status updates, telemetry callouts

A typed event per category bloats the protocol; each new feature
needs a wire-format change. A single extensible **callout**
primitive collapses them — `kind` is a free string, frontends
render any callout uniformly, new categories are documented
without protocol changes.

The user's framing: "if we find something should be turned into a
formal message later, we always can, but until then, we have a
flexible way to add things, and try things out."

CONVENTIONS apply.

## Scope

- **In:**
  - `crates/mu-core/src/protocol.rs` — add `CalloutEvent` struct and
    `CalloutBody` enum. Notification METHOD: `"session.callout"`.
  - `crates/mu-core/src/agent/loop_.rs` — add `AgentEvent::Callout`
    variant.
  - `crates/mu-coding/src/serve/forwarder.rs` — translate
    `AgentEvent::Callout` → `CalloutEvent` notification.
  - Round-trip serde tests (in protocol's existing test module).
  - Forwarder test that demonstrates a callout flowing through.

- **Out:**
  - **Tool-emitted callouts.** The Tool trait doesn't currently have
    an event-emission channel. Adding one is a Tool trait change; future
    spec when there's a concrete tool that needs to emit progress.
  - **Specific callout-emission sites in the loop.** mu-016 adds the
    *surface*; emissions come as their own future specs (e.g.,
    "emit warning callout when approaching iteration cap").
  - **Frontend rendering.** No TUI exists yet; how callouts display
    is a future TUI spec.
  - **Persistence / replay.** Callouts are ephemeral notifications.
    If we want session-replay later, that's a session-store spec
    (which doesn't exist yet anyway).
  - **Required-response callouts.** The `session.input_required`
    spec — agent asks user a question and waits — is its own spec.
    Callout is one-way notification only.

## Invariants

- **INV-1 (CONVENTIONS apply).**
- **INV-2 (additive, not substitutive).** Existing typed events
  (`text_delta`, `tool_call_*`, `done`, `error`) STAY. Callout is
  for extensible miscellany; structured events keep their typed
  shape because the agent loop and replay code depend on it.
- **INV-3 (kind is a free string).** Don't enum-restrict it. New
  kinds are documented additively, not enforced. A starter set is
  documented in the spec body but isn't load-bearing in code.
- **INV-4 (theme is presentation hint, optional).** If absent, the
  frontend picks based on `kind`. We document a starter set
  ("info", "muted", "warning", "danger", "success") but a custom
  theme value isn't an error.
- **INV-5 (context_refs is the durable-reference seam).** Per the
  cooperating-sessions design (memory `d22f391a`), callouts SHOULD
  reference durable artifacts (specs, memory IDs, code-index paths)
  rather than embedding context blobs in `body`. v1 doesn't enforce
  this — it's a convention.

## Interfaces

### Wire types (mu-001 protocol module)

```rust
/// Catch-all "the agent is saying something notable" notification.
/// Free-form `kind` and optional `theme` lets new categories be
/// added without protocol changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalloutEvent {
    pub session_id: String,
    /// Free-form category. Documented starter set:
    /// "observation", "warning", "hint", "info", "status",
    /// "memory", "peer_message".
    pub kind: String,
    /// Short human-readable label (UI title).
    pub title: String,
    /// Body content. `Text` for simple cases; `Structured` for
    /// richer payloads that frontends may render specially.
    pub body: CalloutBody,
    /// Presentation hint. Documented starter set: "info", "muted",
    /// "warning", "danger", "success". Frontends may treat unknown
    /// values as "info".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// References to durable artifacts (spec IDs, memory IDs,
    /// code-index paths, beads). The body should be terse; the
    /// refs let consumers fetch full context.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CalloutBody {
    Text(String),
    Structured(serde_json::Value),
}

impl CalloutEvent {
    pub const METHOD: &'static str = "session.callout";
}
```

### AgentEvent variant (mu-003 loop)

```rust
// In agent/loop_.rs's AgentEvent enum, add:
Callout {
    kind: String,
    title: String,
    body: serde_json::Value,
    theme: Option<String>,
    context_refs: Vec<String>,
},
```

(The AgentEvent uses `body: serde_json::Value` directly — no
`CalloutBody` enum at the loop's level, since `Value::String("...")`
naturally serializes to the `Text(String)` untagged variant. The
protocol layer's `CalloutBody` enum is for consumer ergonomics.)

### Forwarder mapping (mu-coding)

```rust
AgentEvent::Callout { kind, title, body, theme, context_refs } => {
    let body_payload = match body {
        Value::String(s) => CalloutBody::Text(s),
        other => CalloutBody::Structured(other),
    };
    let _ = notif.emit(
        CalloutEvent::METHOD,
        CalloutEvent {
            session_id: session_id.clone(),
            kind, title,
            body: body_payload,
            theme,
            context_refs,
        },
    ).await;
}
```

## Behaviors

1. **B-1 (round-trip text body):** `CalloutEvent { kind: "observation",
   title: "spotted typo", body: CalloutBody::Text("line 5"), theme:
   Some("info"), context_refs: [] }` round-trips through `serde_json`
   preserving all fields. The encoded body field is just the string
   `"line 5"` (untagged).

2. **B-2 (round-trip structured body):** Same with `body:
   CalloutBody::Structured(json!({"line": 5, "kind": "typo"}))`.
   Encoded body is the JSON object.

3. **B-3 (skip empty context_refs in encoding):** With
   `context_refs: vec![]`, the encoded JSON does NOT contain a
   `context_refs` key (the `skip_serializing_if` covers this).
   Verifies via `serde_json::to_value(...).get("context_refs").is_none()`.

4. **B-4 (skip None theme in encoding):** With `theme: None`, no
   `theme` key in the encoded form.

5. **B-5 (CalloutEvent::METHOD constant):** Equals `"session.callout"`.
   Pinned by test, in line with other METHOD constants.

6. **B-6 (forwarder translates AgentEvent::Callout):** Push an
   `AgentEvent::Callout` through a test-mode loop's events channel,
   capture the resulting `NotificationWriter::emit` call, assert the
   emitted method is `"session.callout"` and the params include the
   right fields.

## Acceptance

- Modified files:
  - `crates/mu-core/src/protocol.rs` (+~50 lines)
  - `crates/mu-core/src/agent/loop_.rs` (+1 variant, +match arm in
    test code if needed)
  - `crates/mu-coding/src/serve/forwarder.rs` (+1 match arm)
- `cargo build` clean.
- `cargo nextest run` passes — every existing test plus B-1..B-6.
- No new workspace deps.

## Out-of-circuit warnings

- **OOC-1:** `CalloutBody` is `#[serde(untagged)]` so a Text body
  encodes as a bare string and a Structured body as an object. This
  is the right wire shape but it means deserializing a string-as-
  Structured is impossible (it'll always pick Text). That's
  intentional — strings ARE text in this design.

- **OOC-2:** Don't add `CalloutEvent` to mu-001's `*Request` /
  `*Response` family. It's a notification only — daemon → frontend.
  Same shape as `TextDeltaEvent` and friends.

- **OOC-3:** `AgentEvent::Callout`'s `body` is
  `serde_json::Value`, not a `CalloutBody`. Loop emitters pass the
  raw value; the forwarder coerces to `CalloutBody` at the wire
  layer. This avoids importing protocol types into the agent loop's
  module (mu-core's protocol and agent modules don't depend on each
  other today, and this preserves that).

## Documented starter set (informational, not enforced)

`kind` values we'll start with:

| kind | meaning | typical theme |
|------|---------|---------------|
| `info` | generic informational message | info |
| `status` | "I'm working on X" | muted |
| `observation` | something the agent noticed | info |
| `hint` | a suggestion that doesn't require action | info |
| `warning` | non-fatal issue worth flagging | warning |
| `memory` | memory recall surfaced for the user | muted |
| `peer_message` | message from a cooperating session | info |

`theme` values:

| theme | typical use |
|-------|-------------|
| `info` | default; neutral |
| `muted` | low-priority, dimmed |
| `warning` | yellow-ish; attention without alarm |
| `danger` | red-ish; something is wrong |
| `success` | green-ish; positive outcome |

These aren't enforced in code — frontends interpret. A `kind` of
`"my_custom_thing"` and `theme: "magenta"` are valid; the frontend
falls back to defaults.

## Prior work / context

- mu-001 — protocol notification types (siblings: `TextDeltaEvent`,
  `ToolCallStartedEvent`, etc.).
- mu-003 — `AgentEvent` enum (siblings: `TextDelta`,
  `ToolCallStarted`, etc.).
- mu-coding's `serve/forwarder.rs` — the typed-event-to-notification
  mapper.
- memory `d22f391a` — cooperating-sessions design, where peer
  messages would be callouts of `kind: "peer_message"`.
- memory `ee639a12` — mu memory integration candidate; recalls
  would emit callouts of `kind: "memory"`.

## Changelog

- 2026-05-10 — initial draft (claude-personal).
