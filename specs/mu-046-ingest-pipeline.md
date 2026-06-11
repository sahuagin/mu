# Spec: ingest pipeline — disruptor-style command journal, receipts, per-session pipelines

| field      | value                                                          |
| ---------- | -------------------------------------------------------------- |
| spec_id    | mu-046                                                         |
| status     | implemented                                                    |
| created    | 2026-06-10                                                     |
| updated    | 2026-06-10                                                     |
| authors    | tcovert + claude                                               |
| supersedes | amends the dispatch model of mu-004 (handlers stay; entry changes) |
| beads      | mu-ingest-pipeline-umbrella-n3yy (.1–.9)                       |

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

outbound: Router — tagged envelopes (origin connection id, request id, command
seq) route to per-connection ordered lanes (one egress queue per connection;
durable never dropped, ephemeral evicted under pressure, slow consumers
disconnected — INV-11); origin-less envelopes broadcast to every lane
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

Session-scoped commands (`ask_session`, `cancel_session`,
`session.cancel_outstanding`, `close_session`,
`session.respond_to_input_required`, `session.set_route`, mailbox posts
addressed to the session — note the wire mixes legacy `*_session` and newer
`session.*` method names; the protocol METHOD constants are authoritative) are
journaled into the session's existing JSONL log —
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
    pub origin: Option<Origin>,      // which connection this belongs to (None ⇒ broadcast)
    pub request_id: Option<Value>,   // response correlation
    pub command_seq: Option<u64>,    // journal correlation
    pub item: Outbound,              // existing Response/Notification union
}
```

One daemon-wide `Router` (WP9; originally a `tokio::sync::broadcast` — see the
WP9 amendments for why that was replaced): each connection registers one
ordered egress lane at accept; producers route envelopes to the addressed
origin's lane (or every lane for `origin: None`), non-blocking always.
`NotificationWriter` and `write_loop` ride this; there is no other way to emit
bytes outward.

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
- **INV-11 (two-tier outbound — WP9).** Durable outbound (responses, receipts,
  lifecycle notifications) is never dropped while its connection lives;
  ephemeral outbound (live-feed ticks: text deltas, provider-status updates)
  may be evicted under pressure — announced by a one-shot
  `connection.lagged { dropped }` notice when pressure clears; a connection
  that cannot keep up with its own durable stream is disconnected — the
  journals remain the recovery path.

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
- Query-class receipts elide their result (WP8): for methods on the
  `QUERY_METHODS` allowlist a success receipt's `result` is replaced by the
  compact marker `{"elided": true, "result_bytes": <serialized length>}` —
  the `CommandEcho` still wraps the full command — because embedding a
  `session.events` result into the very log it just read would compound log
  growth under polling. Non-query receipts and wire responses are unchanged.

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

- [x] Crash test: a handler that panics after ingest leaves exactly one
  `CommandReceived` on disk, no receipt; replay surfaces it as an orphan.
  (`crash_after_ingest_leaves_orphaned_command_received_inv1_inv4`)
- [x] Fail-closed test: journal append error ⇒ `-32003` response, handler
  never runs. (`poisoned_seam_fails_closed_with_journal_unavailable_inv2`,
  `journal_open_failure_aborts_serve_inv2`,
  `broken_journal_fails_closed_with_no_effect` for MCP)
- [x] Journal seq order == processing order per pipeline — landed as an
  integration assertion over a batched 8-command run
  (`daemon_commands_process_in_seq_order_inv3`), not a proptest.
- [x] Auth-rejection test: unauthenticated protected call yields
  `CommandReceived` + `CommandRejected{stage: auth_gate}`; raw journal bytes
  do not contain the bearer token.
  (`auth_rejection_journaled_and_token_redacted_inv6`)
- [x] Boot test: journal record 1 = `JournalOpened`, record 2 = `ConfigLoaded`;
  `--bare` reflected; secrets absent.
  (`boot_journals_config_loaded_before_any_adapter_command_inv9`)
- [x] MCP tool call appears in the journal as `mcp.<tool>` with receipt.
  (`daemon_info_round_trips_and_is_journaled_with_receipt`,
  `mailbox_post_journals_in_session_log_before_effect`)
- [x] `mu telemetry compact` still rebuilds analytics from session logs
  (analytics/compact.rs tests green in the full suite; new command/receipt
  event kinds ride the skip-and-count path).
- [x] `just ci` green at every WP boundary.

## Landed notes / deviations (WP7, 2026-06-10)

- **Wire method names are the protocol constants** — the wire mixes legacy
  `*_session` (`ask_session`, `cancel_session`, `close_session`) and newer
  `session.*` names; journals record whichever constant the protocol type
  declares. No rename happened under this spec.
- **MCP auth posture**: MCP connections now derive the same initial
  connection state as stdio (`auth::initial_connection_state`) — root when no
  `[auth]` mechanism enforces, `Unauthenticated` when bearer tokens are
  configured. Since the MCP surface has no auth handshake yet, a
  token-configured daemon makes MCP tools `AUTH_REQUIRED`-unusable (-32001).
  Handshake/capability gating is future work (see Out of scope).
- **Cancel mid-ask**: the agent loop closes out pending receipt tickets on
  termination-without-Done by emitting a synthetic terminal `Done(Aborted)`
  (`Done(Error)` for error outcomes) carrying the pending command receipts;
  the forwarder writes each as `CommandFailed`. Asks queued but never started
  remain orphans — the INV-4 crash marker.
- **Queries journal by default**: `[journal].journal_queries = false` skips
  journaling read-only queries but mutations always journal
  (`journal_queries_false_skips_reads_but_journals_mutations`).
- **Receipts use the strict path too**: session-slot receipts go through
  `append_command` (fsync'd) just like intake — see
  `pipeline.rs::append_receipt` and the forwarder's Done handling. Receipt
  append *failures* are logged, never fatal: the command is already durable
  and the orphaned `CommandReceived` is the legible marker. Daemon-slot
  receipts use `CommandJournal::append` (fsync per policy), same
  logged-orphan-on-failure semantics.
- **Ask receipts without a disk-backed session log** fall back to the daemon
  journal with the immediate accepted-receipt shape (pinned by
  `receipts_wrap_the_original_command_inv5`); the session-log Done-time
  receipt path is `ask_receipt_lands_in_session_log_at_done_wp4`.

### WP8–WP9 amendments (adversarial review, 2026-06-11)

- **Per-session FIFO dispatch** — the control-plane consumer originally
  spawned one independent task per session-scoped command, so two commands
  pipelined to the SAME session could reach its input channel out of journal
  order (an INV-3 violation within the session pipeline). The consumer now
  routes session-scoped commands (post-journal, in seq order) into a
  per-session dispatcher: one task per addressed session id, strict FIFO
  within a session, concurrency only across sessions. Dispatchers are
  created lazily on first command and torn down when the session is no
  longer live and the lane is drained; a wedged session wedges only its own
  dispatcher. See the `serve/pipeline.rs` module doc ("Per-session FIFO
  dispatch") for the lifecycle, including the sweep-on-send-failure path.
- **Two-tier outbound (WP9, superseding the WP8 lossy-outbound note).** WP8
  had documented the daemon-wide `tokio::sync::broadcast` as lossy under lag
  — responses included, with one connection stalled on its own socket having
  its RESPONSES evicted by OTHER sessions' token deltas advancing the shared
  ring. Wrong semantics; replaced (bead n3yy.9) by the exchange answer:
  `transport::Router`, per-consumer egress queues with an explicit
  slow-consumer policy. Each connection registers ONE ordered lane (a single
  queue, so per-connection wire ordering is exactly emission ordering);
  envelopes are tier-classified at push — responses and every notification
  not on the ephemeral allowlist (`session.text_delta`,
  `session.provider_status`; unknown methods fail safe to durable) are
  DURABLE and never evicted while the connection lives. Past
  `EPHEMERAL_PRESSURE_CAP` (1024) a push evicts the oldest queued ephemeral
  envelope; when pressure clears, a one-shot durable
  `connection.lagged { dropped: n }` notification (new wire method,
  additive) tells the client how many ticks it missed. A lane past
  `LANE_HARD_CAP` (65536) with nothing ephemeral left to shed is poisoned:
  the writer logs the drop counters and terminates — the slow consumer is
  DISCONNECTED. Nothing is lost from the system of record: every durable
  item is derivable from the command journal / session logs (the recovery
  path on reconnect), and durable growth is otherwise self-limiting because
  responses are 1:1 with commands the client itself sent. MCP connections
  register their lane at accept; a per-connection demux task routes response
  envelopes to per-invocation waiters by synthetic request id and drops
  notifications at trace (the MCP tool surface has no notification channel);
  the old per-call `Lagged` error is gone — the failure modes are now lane
  closed (shutdown) and lane poisoned (slow-consumer disconnect; journal =
  source of truth).
- **INV-8 scope cut** — MCP resource reads (`mu://...` status reads) and
  `mu/session_status` subscription pushes bypass the tagged outbound
  stream; INV-8 currently holds for the tool/command surface only. Porting
  the MCP resource/subscription surface onto the outbound stream is the
  named follow-up.
- **Redaction is schema-keyed** — INV-6's mechanism redacts params of
  methods on the redaction map (`SECRET_PARAM_FIELDS`,
  `SECRET_KEY_DENYLIST`). Value-shaped secrets — e.g. a human pasting a
  credential into `session.respond_to_input_required` or a user message —
  are not detected; they are the same exposure class as user messages
  generally. Future work alongside capability gating.

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
