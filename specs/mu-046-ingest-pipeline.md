# Spec: ingest pipeline — disruptor-style command journal, receipts, per-session pipelines

| field      | value                                                          |
| ---------- | -------------------------------------------------------------- |
| spec_id    | mu-046                                                         |
| status     | proposed                                                       |
| created    | 2026-06-10                                                     |
| updated    | 2026-06-10                                                     |
| authors    | tcovert + claude                                               |
| supersedes | amends the dispatch model of mu-004 (handlers stay; entry changes) |
| beads      | mu-ingest-pipeline-umbrella-n3yy (.1–.7)                       |

## Why

mu's documentation commits to "every view of the session is a projection of that
log" and CLAUDE.md claims write-ahead persistence. The implementation inverted
this at the daemon boundary: inbound commands are never journaled
(`handle_ask_session` drops the message into the agent loop's mpsc and returns
`accepted: true` with nothing on disk), responses and rejections leave no log
trace, and the forwarder's own comment calls the event log a "durable
projection" of the in-memory `AgentEvent` stream — process-then-log, the
opposite of the stated architecture. There is no fsync anywhere in
`event_log.rs`; IO failures are swallowed. Config is TOML+env at startup, not a
message. The MCP socket surface (`serve/mcp.rs::dispatch_tool`) calls handlers
directly, bypassing `dispatch()` — a second, unaudited entry.

This spec makes the border real. The named pattern (tcovert): **disruptor +
event sourcing, with the core treated like a matching engine.** Adapters at the
edges, a sequenced durable journal in the middle, deterministic state machines
consuming it, receipts out.

mu controls its border, not the world. External UIs bring themselves (ACP /
JSON-RPC 2.0); MCP is foreign. Everything crossing the border becomes a
journaled command before anything processes it. Replay, test, and audit run
out-of-band off the logs.

## What this is NOT

- Not a wire-protocol change — JSON-RPC methods, params, and responses are
  unchanged. Frontends notice nothing except a new error code.
- Not runtime-mutable config — `config.set` / `ConfigAmended` are deferred;
  this spec only makes the *resolved* startup config a journaled message that
  rides the same path they later will.
- Not capability/biscuit gating — the extremities stay open (MVP). The
  adapter/route seam carries an `AuthSnapshot` and a gate hook so gating can
  land later without re-architecting.
- Not a session-log durability overhaul — only *command* appends get the
  strict fsync path. Gateway-result events (tool results, assistant messages)
  keep best-effort appends.
- Not log compaction or retention — logs stay append-only and unbounded;
  tombstones (mu-mh4) remain the only repair mechanism.

## Architecture

```
stdio-jrpc adapter ─┐                      ┌→ daemon control-plane pipeline
MCP adapter        ─┼→ parse → route by ───┤   (own journal + queue: session.create,
(ACP, control: later)┘  session_id         │    daemon.*, config, peer.*, mailbox)
                                           └→ session pipeline (one per session)
                                               own journal + queue + state machine

each pipeline: journal CommandReceived (fsync) → enqueue   [single writer; seq = order]
               └ fail-closed: append error ⇒ reject, never enqueue
               consumer = deterministic state machine (matching-engine style);
               provider/tool work = async gateways whose results re-enter as
               sequenced inputs; on completion journal CommandSucceeded/Failed
               wrapping the original command → emit to outbound

outbound: one daemon-wide stream, tagged envelopes (origin connection id, request
id, command seq), spmc — transport writers filter; notifications fan out
```

**One pipeline per session.** The daemon hosts many sessions; each has its own
queue, its own pipeline, its own log. Concurrency is across pipelines, never
within one. "Single port of entry" does not mean one socket — it means every
listener is an adapter-producer into the same ingest/route seam, and nothing
reaches a handler any other way.

## Shapes

### Daemon control-plane journal (new file family)

Location: `~/.local/share/mu/journal/<daemon_id>.jsonl` — a sibling of
`events/`, so the two session-log scanners (`sessions_index`,
`discovery/file_backend`) never see it. New module
`mu-core::command_journal`:

```rust
pub struct JournalRecord {
    pub seq: u64,                 // monotonic per journal, from 1; THE command id
    pub daemon_id: String,
    pub timestamp_unix_ms: u64,
    pub payload: JournalPayload,
}

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JournalPayload {
    JournalOpened   { mu_version: String, pid: u32 },
    ConfigLoaded    { sources: Vec<String>, config: serde_json::Value }, // redacted
    CommandReceived {
        request_id: serde_json::Value,  // JSON-RPC id (client-chosen, NOT unique)
        method: String,
        params: serde_json::Value,      // secret-redacted
        session_id: Option<String>,
        auth: AuthSnapshot,             // authenticated | unauthenticated | denied
        origin: Origin,                 // connection/transport identity
    },
    CommandSucceeded { command_seq: u64, command: CommandEcho, result: serde_json::Value, elapsed_ms: u64 },
    CommandFailed    { command_seq: u64, command: CommandEcho, code: i32, message: String, elapsed_ms: u64 },
    CommandRejected  { command_seq: u64, command: CommandEcho, code: i32, message: String, stage: RejectStage },
    Tombstone        { target_seq: u64, reason: String },
}

/// Guideline 5's "success object WRAPPING the original command".
pub struct CommandEcho { pub request_id: serde_json::Value, pub method: String, pub params: serde_json::Value }

#[serde(rename_all = "snake_case")]
pub enum RejectStage { AuthGate, Validation, Routing }
```

`CommandJournal::append` returns `io::Result<u64>` and fsyncs (`sync_data`)
before returning — **errors propagate**, unlike `SessionEventLog::append`.
This journal is load-bearing.

### Session pipelines journal into the session's own event log

Session-scoped commands (`session.ask`, `session.cancel`,
`session.cancel_outstanding`, `session.close`,
`session.respond_to_input_required`, `session.set_route`, mailbox posts
addressed to the session) are journaled into the session's existing JSONL log —
already append-only, tombstoned, and projected. New `EventPayload` variants:

```rust
CommandReceived  { request_id: Value, method: String, params: Value, auth: AuthSnapshot, origin: Origin },
CommandSucceeded { command_event_id: u64, command: CommandEcho, result: Value, elapsed_ms: u64 },
CommandFailed    { command_event_id: u64, command: CommandEcho, code: i32, message: String, elapsed_ms: u64 },
CommandRejected  { command_event_id: u64, command: CommandEcho, code: i32, message: String, stage: RejectStage },
```

plus a strict append on `SessionEventLog`:

```rust
/// Like append(), but for commands: fsync before return, errors propagate.
pub fn append_command(&self, actor: EventActor, payload: EventPayload) -> io::Result<u64>;
```

`command_event_id` is the session-log event id of the `CommandReceived` —
same correlation scheme as the daemon journal's `seq`.

### Outbound

```rust
pub struct OutboundEnvelope {
    pub origin: Origin,              // which connection this belongs to (None ⇒ broadcast)
    pub request_id: Option<Value>,   // response correlation
    pub command_seq: Option<u64>,    // journal correlation
    pub item: Outbound,              // existing Response/Notification union
}
```

One daemon-wide stream (`tokio::sync::broadcast`), spmc: each transport writer
subscribes and forwards envelopes whose `origin` matches its connection (or
broadcasts). `NotificationWriter` and `write_loop` port onto this; there is no
other way to emit bytes outward.

### Wire surface

One addition: error code `JOURNAL_UNAVAILABLE = -32003`, returned when a
command cannot be made durable. New `[journal]` config section:

```toml
[journal]
fsync = "always"          # "always" | "never" — default always
journal_queries = true    # default true: read-only queries are journaled too
# dir = "..."             # override location (tests/ephemeral daemons)
```

## Invariants

- **INV-1 (durable before processed).** A command is appended to its
  pipeline's journal — fsync'd per policy — before it enters that pipeline's
  queue. No handler, no state machine, no side effect sees a command that is
  not on disk.
- **INV-2 (fail closed).** If the journal append errors, the command is
  rejected with `JOURNAL_UNAVAILABLE` and is never processed. A daemon that
  cannot open its journal at boot does not serve.
- **INV-3 (seq is the sequencer).** Within a pipeline, journal order == queue
  order == processing order, maintained by a single writer per pipeline.
  Per-session replay is deterministic: re-feed the journal, get the same
  state. Concurrency exists only across pipelines.
- **INV-4 (at most one receipt per command).** Every `CommandReceived`
  eventually gains exactly one `CommandSucceeded`/`Failed`/`Rejected` — or
  none, and an orphaned `CommandReceived` IS the legible crash marker.
  Recovery may tombstone orphans; it never erases them.
- **INV-5 (receipts wrap the original).** Success and failure records embed
  the original command (`CommandEcho`), not just a reference. The receipt is
  self-contained evidence of what was asked and what came of it.
- **INV-6 (secrets never hit a journal).** `peer.auth_initiate` params and
  `auth.tokens` (and any future secret-shaped field) are redacted before
  append. Verified by grepping raw journal bytes for the live token in tests.
- **INV-7 (no side doors).** Every transport — stdio JSON-RPC, the MCP
  socket, future ACP/control listeners — is an adapter producing into
  ingest/route. Nothing calls handlers directly.
- **INV-8 (one way out).** All responses and notifications leave through the
  tagged outbound stream. No writer bypasses it.
- **INV-9 (config is a message).** TOML+env resolution stands, but the
  resolved config enters the control-plane pipeline as `ConfigLoaded` —
  journaled, sequenced, applied — before adapters accept traffic. Future
  `config.set` rides the same path.
- **INV-10 (gateways re-enter, never mutate sideways).** Slow external work
  (provider streams, tool execution) happens in async gateways; their results
  come back into the pipeline as sequenced inputs. No gateway mutates pipeline
  state from the side.

## Receipt semantics

For accept-async commands the receipt records *processing outcome*, written by
the owning pipeline at completion:

- `session.ask`: wire response `accepted: true` stays immediate;
  `CommandSucceeded` (wrapping the original ask) is written when the turn
  completes (`Done`), `CommandFailed` on `Error`. The existing
  `Done`/`TaskTelemetry` events are unchanged.
- Synchronous commands (`session.list`, `daemon.stats`, …): receipt written
  when the response is produced.
- Rejections (auth gate, validation, unknown method, session-not-found) are
  receipts too — `CommandRejected{stage}` — and are journaled even though no
  handler ran. The journal append happens *before* the auth gate.

## Work packages (beads mu-ingest-pipeline-umbrella-n3yy.1–.7)

| WP | bead | scope |
|----|------|-------|
| 1 | .1 | `mu-core/src/command_journal.rs`; `event_log.rs` command/receipt variants + `append_command`; `config.rs` `[journal]` + `redact_config` |
| 2 | .2 | `OutboundEnvelope`; daemon-wide broadcast stream; port `NotificationWriter`/`write_loop`; per-connection filtering |
| 3 | .3 | `serve/pipeline.rs`: `Command` envelope, route-by-session_id, control-plane queue + consumer (absorbs daemon-scoped `dispatch()` arms + auth gate), receipts; stdio transport becomes adapter #1 |
| 4 | .4 | session-scoped commands journal via `append_command` before the session input queue; completion receipts; gateway re-entry formalized |
| 5 | .5 | MCP adapter: `dispatch_tool` → `Command`s (`method = "mcp.<tool>"`) through ingest/route |
| 6 | .6 | boot sequence: resolve config (provenance tracked) → `JournalOpened` → `ConfigLoaded` through the control plane before adapters accept traffic |
| 7 | .7 | doc closure: `specs/architecture/event-sourced-context.md`, CLAUDE.md write-ahead correction, forwarder comment fix |

Order: WP1 → WP2 → WP3 → {WP4 ∥ WP5} → WP6 → WP7.

## Migration / compat

- The daemon journal is a new file family in a new directory; existing session
  logs replay unchanged.
- New `EventPayload` variants are additive on an internally tagged enum
  (`kind`); old readers skip-and-count unknown kinds (`from_jsonl` malformed
  counter). Mixed-version fleets see the counter tick, nothing breaks.
- Pre-mu-046 daemons have no journal — absence is the legible era marker. No
  backfill.

## Acceptance criteria (umbrella)

- [ ] Crash test: a handler that panics after ingest leaves exactly one
  `CommandReceived` on disk, no receipt; replay surfaces it as an orphan.
- [ ] Fail-closed test: journal append error ⇒ `-32003` response, handler
  never runs.
- [ ] Property test: journal seq order == processing order per pipeline.
- [ ] Auth-rejection test: unauthenticated protected call yields
  `CommandReceived` + `CommandRejected{stage: auth_gate}`; raw journal bytes
  do not contain the bearer token.
- [ ] Boot test: journal record 1 = `JournalOpened`, record 2 = `ConfigLoaded`;
  `--bare` reflected; secrets absent.
- [ ] MCP tool call appears in the journal as `mcp.<tool>` with receipt.
- [ ] `mu telemetry compact` still rebuilds analytics from session logs.
- [ ] `just ci` green at every WP boundary.

## Out of scope (deferred)

- `config.set` / `config.get` RPCs and `ConfigAmended` (the path is built;
  the verbs are not).
- Capability/biscuit gating at the adapter seam (the `AuthSnapshot` + gate
  hook reserve the spot).
- ACP and control-port adapters (the adapter trait is the extension point).
- Journaling pre-parse wire garbage (transport-level parse failures) — noted
  follow-up.
- `CapabilityAmended` events — reserved here, lands under mu-nqn5.
- fsync for non-command session events — best-effort append stays.
