# Spec: projection queries + pluggable session discovery

| field      | value                                       |
| ---------- | ------------------------------------------- |
| spec_id    | mu-038                                      |
| status     | proposed                                    |
| created    | 2026-05-11                                  |
| updated    | 2026-05-11                                  |
| authors    | tcovert + claude-personal (claude-opus-4.7) |
| supersedes | none                                        |

## Why

The TUI wire slice (jj `qrpzxpuq`, 2026-05-11 morning) immediately hit three missing surfaces:

- **`session.list`** — no way to enumerate sessions. The TUI was starting empty every time and inventing a client-side list of sessions it had created during that run, which goes away on reconnect and never sees sessions another client/daemon created. That's exactly the "UI owns runtime state" antipattern the design doc warns against.
- **`session.events`** — no way to read a session's history. Without it, the TUI can render *live* notifications into a firehose but can't show what happened before it attached. Reattach + transcript replay is a daily-driver requirement.
- **`daemon.stats`** — no way to populate the Command Center header (uptime, event count, in-flight calls, cost). The mockup assumes this surface exists.

All three are projection queries over state mu already has — they just aren't exposed. mu-038 adds them.

The interesting one is `session.list`. There are at least three places sessions might live:

1. **In-process** — the `Sessions` registry in `mu-coding/src/serve/sessions.rs` (today).
2. **Cross-daemon, same machine** — sessions in another `mu serve` running for the same user.
3. **Cross-machine** — sessions in a daemon on another host (the mu-037 peer-discovery direction).

If we hard-code `session.list` to read only (1), we'll have to retrofit a different surface when we want (2) and (3). Better: design a **pluggable `SessionDiscovery` trait** now, ship the in-process backend for v1, and slot file-based and etcd backends in later without changing the wire shape.

A file-based default makes "I have two `mu serve` instances on this box, show me all their sessions" work with no extra infrastructure. etcd makes the same thing work across machines AND adds liveness detection via lease TTLs (a daemon that crashes loses its registration automatically). The same trait drops mu-037 phase 2's peer-discovery story onto this substrate cleanly.

After mu-038:

- `session.list`, `session.events`, `daemon.stats` are real RPCs the TUI can call.
- `session.list` reads through a `SessionDiscovery` trait; the default backend is in-process (v1), with file-based and etcd backends sketched as follow-ups.
- The TUI loses three client-side workarounds.

CONVENTIONS apply.

## Scope

### In

- **Wire types** in `mu-core/src/protocol.rs`:
  - `SessionListRequest { filter: SessionListFilter? }` → `SessionListResponse { sessions: Vec<SessionInfo>, snapshot_at_unix_ms: u64 }`
  - `SessionListFilter { include_remote?: bool, parent_session_id?: String, status?: SessionStatusFilter, active_since_unix_ms?: u64, limit?: u32 }`
  - `SessionInfo { session_id, daemon_id, is_remote, parent_session_id?, provider_kind, model, status: SessionStatusSummary, started_at_unix_ms, last_activity_unix_ms, ask_count, tool_call_count, cumulative_usage? }`
  - `SessionStatusSummary` enum: `Idle, Asking, Streaming, ToolExecuting, AwaitingInputRequired, Done, Errored` — derived from the session's event log.
  - `SessionEventsRequest { session_id, after_event_id?: u64, limit?: u32, kinds_filter?: Vec<String> }` → `SessionEventsResponse { events: Vec<EventRecord>, next_event_id?: u64, end_of_log: bool }`
  - `EventRecord { event_id, ts_unix_ms, actor: EventActor, payload_kind: String, payload: serde_json::Value }` — a JSON-friendly projection of `EventPayload` (the enum already exists in `event_log.rs`; we just expose its serialised form).
  - `DaemonStatsRequest {}` → `DaemonStatsResponse { daemon_id, started_at_unix_ms, uptime_ms, session_count, active_session_count, total_events, total_tool_calls, total_input_tokens, total_output_tokens, in_flight_calls_count, version }`

- **`SessionDiscovery` trait** in `mu-coding/src/serve/discovery/mod.rs` (new module):
  ```rust
  #[async_trait]
  pub trait SessionDiscovery: Send + Sync {
      /// Enumerate sessions visible to this daemon.
      ///
      /// LocalRegistry returns only this daemon's sessions; File and
      /// Etcd backends additionally enumerate peer daemons. Filter
      /// determines what's included (status, parent, recency, remote-or-not).
      async fn list(&self, filter: SessionListFilter) -> Result<Vec<SessionInfo>>;

      /// Announce a new session to the discovery layer. Called when
      /// `Sessions::insert` adds a session. LocalRegistry is a no-op
      /// (the in-memory map IS the source of truth); File/Etcd write
      /// a registry entry.
      async fn announce(&self, info: LocalSessionInfo) -> Result<()>;

      /// Note a session's departure. Called when `Sessions::remove`
      /// fires. LocalRegistry is a no-op; File/Etcd delete the entry.
      async fn withdraw(&self, session_id: &str) -> Result<()>;

      /// Optional reactive change stream. v1 backends MAY return
      /// `DiscoveryWatch::Pending` (no event source); file backend uses
      /// inotify/kqueue, etcd uses watch RPCs. Clients that don't
      /// support reactive updates can poll `list()` periodically.
      fn watch(&self) -> DiscoveryWatch;
  }
  ```
- **Backends** (one ships in v1, two are sketched-only):
  - **`LocalRegistryBackend`** (v1, shipped) — wraps `Arc<Mutex<HashMap<String, SessionState>>>` already in `sessions.rs`. `list` derives `SessionInfo` from session state + event log snapshots. `announce` / `withdraw` are no-ops. `watch` returns `Pending`. No external dependencies, no IO; always works.
  - **`FileBackend`** (sketched, not in v1) — writes TOML files under `~/.local/share/mu/daemons/<daemon_id>/sessions/<session_id>.toml` on announce; deletes on withdraw. `list(include_remote=true)` enumerates files across all daemon directories. Liveness via mtime + a per-daemon heartbeat file (daemons touch `daemon.heartbeat` every N seconds; readers consider entries from a daemon with stale heartbeat as "may be dead"). `watch` uses platform-specific filesystem watching.
  - **`EtcdBackend`** (sketched, not in v1) — sessions are keys under `/mu/sessions/<session_id>` with a lease attached to the daemon's lease. Daemon death automatically expires the lease, sessions disappear without manual cleanup. `watch` uses etcd watch RPCs for live updates. Requires an etcd cluster in reach; perfect fit when running under warden (etcd already in the stack per memory `project_warden`).
- **Daemon wiring** in `mu-coding/src/serve/`:
  - `dispatch.rs` grows `handle_session_list`, `handle_session_events`, `handle_daemon_stats`.
  - `sessions.rs` gains an `Arc<dyn SessionDiscovery>` field. Insert/remove call `announce`/`withdraw` after the in-memory map is updated. (Failures are logged but don't block — discovery is non-load-bearing.)
  - A new daemon-level struct `DaemonInfo { daemon_id, started_at, version }`. `daemon_id` is generated once at startup (UUID v4) and stable for the daemon's lifetime; used in `SessionInfo.daemon_id` and (for FileBackend) in the registry directory name.
  - `serve/main.rs` CLI grows `--discovery <local|file|etcd>` (default `local`) and `--etcd-endpoints <url>` / `--registry-root <path>` follow-on flags. The `serve` command instantiates the chosen backend and threads it through `Sessions::new_with_discovery`.

- **Tests:**
  - Unit: `LocalRegistryBackend::list` against a constructed `Sessions` map; verifies filter semantics (status, parent, since, limit).
  - Unit: `session_status_summary_from_log` — given an event-log sequence, returns the right `SessionStatusSummary`. (Idle if last event was Done/SessionClosed; Streaming if last event is mid-asking with TextDelta events; etc.)
  - Integration: round-trip session.list via dispatch — create 3 sessions, list, filter by parent, filter by status.
  - Integration: session.events round-trip — replay a session's full event sequence; verify pagination via after_event_id.
  - Integration: daemon.stats reflects post-cancel state — error counts, tool-call counts, token counts.

### Out

- **FileBackend implementation.** Sketched only — concrete code is a follow-up (one of mu-038a or mu-039). The trait shape MUST be chosen with FileBackend in mind so we don't rev the trait when it lands.
- **EtcdBackend implementation.** Same — sketched. Add a feature flag `discovery-etcd` so the etcd dep is optional.
- **Cross-daemon authentication.** A FileBackend or EtcdBackend exposes sessions across daemons; the question of "should I trust the other daemon's claims" is a security question (biscuit-auth direction). mu-038 sets up the surface; trust is a separate spec.
- **Live tail (`event.tail` from the mockup).** Different shape from `session.events` — a long-lived subscription rather than a query. Worth a separate spec once the projection queries land.
- **Usage / cache projections (`usage.summary`, `usage.by_provider`, `cache.ledger`).** Listed in the mockup as future surfaces. Same shape as `daemon.stats` (read-only projection over event log) but bigger; defer.

## Invariants

- **INV-1 (queries are read-only).** None of the three new RPCs mutate session state, event logs, or capabilities. Safe to call from any client without authorization beyond "can talk to this daemon."
- **INV-2 (in-process backend always works).** Discovery degradations (file unwritable, etcd unreachable) MUST NOT prevent `session.list` from returning local sessions. The trait's `list` is allowed to log failures to peers and still return local results. (The `DiscoveryError::PartialFailure { local, failed_peers }` variant carries this.)
- **INV-3 (announce/withdraw are best-effort).** A failed announce does not prevent `Sessions::insert` from succeeding. The session is still in-memory and addressable through `ask_session`; it's just temporarily not visible to peer daemons. The next `announce` retry (on a heartbeat tick or a new session insert) catches up.
- **INV-4 (SessionStatusSummary is derived).** Not stored. Computed from the session's event log on each `list`. Cheap because the log is in memory; expensive if the log is huge (>10k events) — future optimization can cache the last-known summary, but v1 derives every time.
- **INV-5 (daemon_id is stable for daemon lifetime).** Generated at startup, unchanged until daemon exits. Restart → new daemon_id. This is what lets the discovery layer detect "the daemon I knew is gone, this is a fresh one."
- **INV-6 (forward-compat additive).** SessionInfo, EventRecord, DaemonStatsResponse all gain fields over time; existing clients must tolerate unknown fields. (serde's `#[serde(deny_unknown_fields)]` is off on response types — same convention as the existing protocol.)

## Wire surface

### session.list

```jsonc
// request
{ "jsonrpc": "2.0", "id": 60, "method": "session.list",
  "params": { "filter": { "include_remote": false, "status": "any", "limit": 100 } } }

// response
{ "jsonrpc": "2.0", "id": 60, "result": {
  "snapshot_at_unix_ms": 1763500000000,
  "sessions": [
    {
      "session_id": "session-3",
      "daemon_id": "8f2c4a…",
      "is_remote": false,
      "parent_session_id": null,
      "provider_kind": "anthropic_api",
      "model": "claude-haiku-4-5-20251001",
      "status": "streaming",
      "started_at_unix_ms": 1763499880000,
      "last_activity_unix_ms": 1763499999500,
      "ask_count": 2,
      "tool_call_count": 11,
      "cumulative_usage": { "input_tokens": 14200, "output_tokens": 980, "cache_read_input_tokens": 0 }
    }
  ]
} }
```

### session.events

```jsonc
// request: page from beginning
{ "jsonrpc": "2.0", "id": 61, "method": "session.events",
  "params": { "session_id": "session-3", "limit": 200 } }

// response
{ "jsonrpc": "2.0", "id": 61, "result": {
  "events": [
    {
      "event_id": 1,
      "ts_unix_ms": 1763499880000,
      "actor": "system",
      "payload_kind": "session_created",
      "payload": { "provider_kind": "anthropic_api", "model": "claude-haiku-4-5-20251001" }
    },
    {
      "event_id": 2,
      "ts_unix_ms": 1763499881020,
      "actor": "user",
      "payload_kind": "user_message",
      "payload": { "text": "..." }
    }
  ],
  "next_event_id": 3,
  "end_of_log": false
} }
```

`kinds_filter` allows clients to request only specific payload kinds (e.g. `["text_delta", "tool_call"]`) — useful for the TUI to render a transcript view without pulling every status notification.

### daemon.stats

```jsonc
// request
{ "jsonrpc": "2.0", "id": 62, "method": "daemon.stats", "params": {} }

// response
{ "jsonrpc": "2.0", "id": 62, "result": {
  "daemon_id": "8f2c4a…",
  "version": "0.0.1",
  "started_at_unix_ms": 1763490000000,
  "uptime_ms": 10000000,
  "session_count": 7,
  "active_session_count": 2,
  "total_events": 18213,
  "total_tool_calls": 312,
  "total_input_tokens": 1820000,
  "total_output_tokens": 14400,
  "in_flight_calls_count": 2
} }
```

`in_flight_calls_count` overlaps with `daemon.outstanding_calls` from mu-035 — but where the latter returns per-call detail, this is a scalar gauge for the header. Both are useful.

## Implementation sketch

### Trait location and instantiation

```rust
// mu-coding/src/serve/discovery/mod.rs
mod local_registry;
pub use local_registry::LocalRegistryBackend;

// Feature-flag the others to keep deps optional.
#[cfg(feature = "discovery-file")]
mod file;
#[cfg(feature = "discovery-file")]
pub use file::FileBackend;

#[cfg(feature = "discovery-etcd")]
mod etcd;
#[cfg(feature = "discovery-etcd")]
pub use etcd::EtcdBackend;

#[async_trait]
pub trait SessionDiscovery: Send + Sync { /* ... */ }
```

In `serve/main.rs`, parse `--discovery` and construct:

```rust
let discovery: Arc<dyn SessionDiscovery> = match args.discovery {
    DiscoveryKind::Local => Arc::new(LocalRegistryBackend::new(sessions.clone())),
    DiscoveryKind::File => Arc::new(FileBackend::new(args.registry_root)?),
    DiscoveryKind::Etcd => Arc::new(EtcdBackend::new(args.etcd_endpoints).await?),
};
let sessions = Sessions::new_with_discovery(discovery.clone());
```

### SessionStatusSummary derivation

```rust
pub fn status_from_log(log: &SessionEventLog) -> SessionStatusSummary {
    match log.last_event_kind() {
        Some(EventKind::SessionClosed) | Some(EventKind::Done) => Idle,
        Some(EventKind::Error) => Errored,
        Some(EventKind::InputRequired) => AwaitingInputRequired,
        Some(EventKind::ToolCall) if !log.last_tool_call_completed() => ToolExecuting,
        Some(EventKind::TextDelta) | Some(EventKind::AssistantMessage)
            if recently_active(log) => Streaming,
        Some(EventKind::UserMessage) => Asking,
        _ => Idle,
    }
}
```

(`recently_active` uses last-event-ts vs now; threshold ~5s. Post-mu-035 this can prefer the live `ProviderStatusTracker` snapshot when the session is local, falling back to log derivation for remote sessions.)

### Sessions registry integration

```rust
impl Sessions {
    pub fn insert(&self, id: String, /* ... */, discovery: &Arc<dyn SessionDiscovery>) {
        // existing in-memory insert
        // ...
        // best-effort announce
        let discovery = discovery.clone();
        let info = LocalSessionInfo { /* derived */ };
        tokio::spawn(async move {
            if let Err(e) = discovery.announce(info).await {
                tracing::warn!("discovery.announce failed: {e}");
            }
        });
    }
}
```

(spawn is fire-and-forget — INV-3 says announce must not block session creation.)

## Tests

1. **`LocalRegistryBackend::list` filter coverage:**
   - 5 sessions of varying status; filter by `status=streaming` returns exactly the streaming ones.
   - Filter by `parent_session_id=X` returns only children of X.
   - Filter by `active_since_unix_ms` excludes sessions older than threshold.
   - `limit=2` returns at most 2 sessions; ordering is `last_activity_unix_ms desc`.

2. **`session.events` pagination:**
   - 50-event session, `limit=20` returns first 20, `next_event_id=21`.
   - Second call with `after_event_id=20` returns events 21-50, `end_of_log=true`.
   - `kinds_filter=["text_delta"]` returns only those payload kinds.

3. **`daemon.stats` reflects state:**
   - Fresh daemon: `session_count=0, total_events=0`.
   - Spawn 3 sessions, run a few asks: counters advance accordingly.

4. **Announce/withdraw best-effort:**
   - `MockFailingBackend` whose `announce` always errors. Inserting a session still succeeds; the session appears in `Sessions` and is addressable via `ask_session`. A warning is logged.

5. **End-to-end smoke (FauxProvider):**
   - Daemon with default `LocalRegistryBackend`, 2 sessions, dispatch `session.list` → returns both with correct status; dispatch `session.events` on one → returns the recorded sequence; dispatch `daemon.stats` → reflects both sessions.

## Risks and follow-ups

- **SessionStatusSummary may lag.** A session that just transitioned to `ToolExecuting` 1ms ago might still report `Asking` until the next event is logged. Acceptable for v1; post-mu-035, the `ProviderStatusTracker` is the authoritative live source and the log derivation becomes the fallback for remote sessions.
- **FileBackend liveness detection.** If a daemon crashes without removing its session entries, those entries stay in the file registry forever. Heartbeat-mtime is the v1 answer: readers treat entries from a daemon with stale heartbeat (>30s old) as "may be dead." Cleanup is opportunistic when a new daemon starts.
- **EtcdBackend dependency.** Adds `etcd-client` crate + connection requirements. Feature-flagged to keep zero-config setups dep-free.
- **`session.list` performance.** Per INV-4, `SessionStatusSummary` derives from the log on each call. For daemons with hundreds of sessions and long logs this can be expensive. v1 ignores; if real workloads expose pain, cache the last-derived summary per session and invalidate on event append.
- **Cross-daemon trust.** A FileBackend reader trusts whatever any other daemon wrote to its directory. Locally that's usually fine (single user); cross-machine or multi-user setups need auth. Out of scope; documented as a known limitation in the FileBackend module docs.
- **Composition with mu-037 phase 2.** mu-037 needs cross-daemon discovery for the peer.hello handshake. mu-038's `SessionDiscovery` IS that primitive — peer discovery can be a thin layer on top of `SessionDiscovery::list(include_remote=true)`. Concretely: mu-037 phase 2 implementation reuses the FileBackend or EtcdBackend chosen here.
- **Composition with mu-035.** mu-035's `daemon.outstanding_calls` and mu-038's `daemon.stats` overlap in spirit — both are daemon-level read RPCs. Implementation can share a `DaemonProjections` helper struct that computes both from the same registry snapshot.
- **TUI integration.** Once mu-038 lands, the TUI:
  - On startup: `session.list` populates the Live sessions pane authoritatively.
  - On reattach (selecting an existing session): `session.events` fills the transcript pane.
  - On every tick: `daemon.stats` updates the header pane.
  - All three are short-poll queries (no streaming). Reactive updates via `event.tail` come in a later spec.
