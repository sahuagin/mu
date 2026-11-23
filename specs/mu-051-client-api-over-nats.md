# mu-051 — Client API over NATS (remote control of mu sessions)

**Status:** DRAFT, revised after the operator's rulings on §16 (2026-10-14). Design only. No code until he approves this revision.
**Bead:** mu-client-api-nats-spec-mw91s
**Date:** 2026-10-14

## 1. Why this spec exists

The operator's original requirement for mu, restated 2026-10-14:

> A client talks to the daemon only through messages on a queue, in both
> directions. The queue abstracts the transport: the same machine is a
> convenience, never a requirement, and swapping the queue's producer/consumer
> for a bus reader or a simple socket protocol should be nearly trivial. AI
> clients (mu-solo first) use that API, and what they need is added to the API
> rather than worked around in the client.

The goal behind that requirement is **remote control of mu sessions**: away
from the desk, see which sessions exist and what each is doing, watch its
conversation, give it the next prompt, answer its approval prompts, stop it.
The same goal was written down in `specs/architecture/claude-code-design-influences.md`
§3.5/§8.2 (May), bead `mu-o7e2a` (Sep 10: IRC as "a remote operator console",
with `mu approve`/`mu deny`), and epic `mu-irc-remote-session-zgbdz` (Sep 16).
Each time the build narrowed: `specs/plans/mu-irc-gateway-v0.md` excluded "daemon
changes and IRC awareness in mu-coding" on day one, which made remote control
structurally impossible, and a month of work produced an IRC bridge for mesh
DMs instead.

An audit (2026-10-14) found the daemon's core matches the requirement:
`serve::pipeline::ingest` is one way in, `mu_core::transport::Router` is one way
out. Nearly every edge violates it: the daemon dies with its stdin; a session's
events go only to the connection that created it; auth is a token handed to a
spawned child through the environment; policy is spawn flags; clients read logs,
config and pricing locally. The daemon API already HAS nearly every control a
remote client needs (§3) — they are reachable only by the one client that spawned
the daemon over stdio. This spec makes them reachable, over NATS, to any client
that speaks the API.

## 2. Scope

**In:** mu sessions. Any client that implements this API: mu-solo, mu-tui,
`mu ask`, a web page, an IRC adapter, scripts.

**Out:** Claude Code sessions (Claude Code's own remote control covers them;
the operator: "I probably won't feel compelled"). Agent-to-agent messaging
(mesh DMs, mu-dialogue, mailbox) is a separate concern and is not changed here.
Multi-user tenancy: one operator, one NATS server he alone uses.

**Decided by the operator (2026-10-14):** NATS is the transport, with JetStream ON
(it is disabled today on NATS 2.14.4; the operator enables it). The API may extend
the existing protocol or redesign it. The rest of his rulings are in §16.

## 3. Requirements, and what already exists

Derived from Claude Code's remote control (code.claude.com/docs/en/remote-control)
and mu's needs. "Exists" is the daemon API today, reachable only by the spawning
client unless noted.

| # | Requirement | Exists | Missing |
|---|---|---|---|
| R1 | List live sessions across machines, with a human name | etcd presence keys, `$SRV` (no status, no name) | name; status; cross-host list |
| R2 | Each session's state: working / idle / waiting for me / done | `provider_status`, `SessionStatusSummary` | published anywhere off the box |
| R3 | Watch the conversation live (text, tool calls) | `session.*` notifications | any subscriber but the creator |
| R4 | Catch up after (re)connecting | `session.events` paging over the durable log | reachable remotely |
| R5 | Several devices at once | Router lanes support many connections | events fan out to one |
| R6 | Send a prompt as a real turn, mid-turn too | `ask_session` (+ interjection) | reachable remotely |
| R7 | Answer approval prompts | `respond_to_input_required` | prompt persisted + listable; timeout suited to an absent human (180 s compiled today) |
| R8 | Interrupt / stop | `cancel_outstanding`, `cancel_session` | stop an autonomous run; wake a parked one |
| R9 | Change model / effort | `session.set_route`, `effort` | reachable remotely |
| R10 | Context controls (clear, usage, config) | `clear_context`, `get/set_config`, `stats` | reachable remotely |
| R11 | Start a session remotely | `create_session` | policy + credentials as messages, not spawn flags |
| R12 | Resume a past session | `session.resume` (strict) | reachable remotely |
| R13 | Session outlives any client | — | daemon lifetime independent of stdio |
| R14 | Be notified when needed or done | — | attention events |
| R15 | Attach files / images | — | deferred (§11) |
| R16 | Authenticated, no inbound ports | NATS (outbound), biscuit capabilities | capability per request, not spawn-env token |

## 4. Shape

```
 client (any)                NATS (+JetStream)                 mu daemon
 ───────────                 ─────────────────                 ─────────
 request  ── mu.api.v1.<d>[.<s>].cmd ──────────────────────▶  ingest (journal, auth, FIFO)
          ◀──────────────────────────────── reply ──────────  dispatch
 subscribe ◀─ mu.api.v1.<d>.<s>.evt.<kind> ◀── publish ──────  session log append
 subscribe ◀─ mu.api.v1.<d>.<s>.live.<kind> ◀─ publish ──────  forwarder → Router
 read/watch ◀─ KV mu_sessions / mu_approvals ◀── put ────────  status + approvals
```

The daemon gains ONE adapter (NATS) beside stdio. Both carry the same envelopes
(§5), so `ingest` and `Router` are unchanged in role. Same-machine stdio remains a
convenience: the spawned-child path is the same API over a pipe.

## 5. Envelopes

Command (request):
```json
{ "v": 1, "id": "<ulid>", "method": "ask_session",
  "params": { ... }, "capability": "<base64 biscuit>" }
```
Reply: `{ "v": 1, "id": "<same>", "result": { ... } }` or `{ ..., "error": { "code", "message", "data?" } }`.
Error codes keep mu-001's (-32700…-32603, AUTH_REQUIRED, AUTH_DENIED, JOURNAL_UNAVAILABLE).

Log line (published on `evt.`): the session-log record itself, byte for byte as
written to `<session>.jsonl` and as `session.events` returns it:
`{ "id": <n>, "session_id": "<s>", "timestamp_unix_ms": <t>, "actor": {...},
"payload": { "kind": "<kind>", ... } }`. `id` is the only number: per session,
starting at 1, contiguous, so a gap is a loss and the same id is the cursor for
`session.events`. The daemon is in the subject. The log's own format is the
versioning; there is no wrapper.

Live notification (published on `live.`): `{ "v": 1, "daemon": "<d>", "session":
"<s>", "method": "<method>", "params": { ... } }`, the notification a stdio client
gets (its subject's `<kind>` is the method without `session.`). Unnumbered: it is for rendering a turn as it happens, not for catching up.
The transport frames the length (NATS per message, stdio per line).

Method names and params are **mu-001's**, unchanged where they exist. Over stdio
the same envelope travels as one JSON line (the current framing gains `v` and
`capability`).

**Compatibility rule (ruling 4).** The framing may change. The operator is the only
consumer, so there is no deprecation window and no shim for old clients. What IS
required is that main works at every merge: a PR that changes the wire updates
every in-tree client that speaks it (mu-solo, mu-tui, `mu ask`, `mu console`, the
gateway's daemon calls, the scripts under `scripts/` that call `mu serve`) in the
same PR, and its smoke tests run each of them against the new daemon.

## 6. Subjects

| Subject | Kind | Carries |
|---|---|---|
| `mu.api.v1.<daemon>.cmd` | request/reply (queue group = the daemon) | daemon-scoped methods: `create_session`, `session.resume`, `session.list`, `daemon.*` |
| `mu.api.v1.<daemon>.<session>.cmd` | request/reply | session-scoped methods: `ask_session`, `session.respond_to_input_required`, `cancel_*`, `set_route`, `get/set_config`, `session.events`, … |
| `mu.api.v1.<daemon>.<session>.evt.<kind>` | publish | each line appended to the session's log, `<kind>` its payload kind (`user_message`, `tool_call`, …), so a client can subscribe to `evt.>` or to one kind (§9) |
| `mu.api.v1.<daemon>.<session>.live.<kind>` | publish | every `session.*` notification a stdio client gets (`text_delta`, `tool_call_started`, `input_required`, …), `<kind>` the method without `session.`; unnumbered and lossy (§9) |
| `mu.api.v1.<daemon>.evt.<kind>` | publish | daemon events (`daemon.started`, `daemon.stopping`, `session.created`, `session.closed`) |
| KV `mu_daemons`, key `<daemon>` | JetStream KV | fleet registry: one entry per live daemon (§7) |
| KV `mu_sessions`, key `<daemon>.<session>` | JetStream KV | session registry + status (§7) |
| KV `mu_approvals`, key `<daemon>.<session>.<request_id>` | JetStream KV | open approval prompts (§8) |

`<daemon>` is the daemon's existing per-process id, the same id that names its
event-log directory. An earlier draft asked for a stable name (hostname +
instance); P1 found that wrong: every client spawns its own daemon today, so a
host-based name collides at once, and with the fleet registry a client never needs
a name in advance. A restart registers under its new id, so nothing a previous run
left behind can block it. `<session>` is the daemon's session id.

**Fleet registry first (ruling 2).** A client never needs to know a daemon name in
advance. It lists `mu_daemons` and `mu_sessions`, and every entry carries the
subjects to address. The registry is the two KV buckets, not a service: there is
no registry process to run, and a daemon registers itself by writing its own keys.
The per-daemon subject tree stays, because it is where commands are routed.

## 7. Presence: the daemon and session lists, with status (R1, R2)

This is the first increment to build (ruling 7, §15).

Each daemon keeps one `mu_daemons` entry,
```json
{ "daemon": "...", "host": "...", "pid": n, "version": "...", "started_ms": n,
  "heartbeat_ms": n }
```
(P6 adds `cmd_subject` when there is a command subject to name.)
and one `mu_sessions` entry per live session,
```json
{ "daemon": "...", "session": "...", "name": "<label>", "status": "working|idle|awaiting_input|awaiting_approval|done|errored",
  "phase": "<provider_status kind>", "phase_since_ms": n, "awaiting": ["<request_id>", ...],
  "provider": "...", "model": "...", "last_activity_ms": n, "cost_usd": n, "autonomous": bool }
```
written on every status transition (the forwarder already recomputes status on
those events for the MCP status resource). A client lists sessions with a KV
key listing and watches changes with a KV watch — no polling, no subscription per
session. A session's entry is deleted when it closes.

**Liveness.** The writer copies the shape of today's etcd presence
(`serve/presence.rs`): one reconciliation task per daemon that, every TTL/3,
rewrites the daemon key and syncs the session keys against the live session
registry, spawned workers included. Each session's value is the `SessionInfo`
that `session.list` returns. P2 writes on the tick only, so status can be up to a
third of the TTL old (20 s by default); writing on each transition is a later
increment if that proves too slow. An open approval prompt is read from the live
session's approval registry, because the forwarder keeps `input_required` out of
the event log and the log-derived status cannot see it (P8 persists the prompt). Both buckets are created with `max_age` =
`[mesh].registry_ttl_secs` (default 60, minimum 6) and `limit_markers`, so a key not
rewritten within the TTL ages out and the server writes a marker for it. A client
watching a bucket treats a `Delete` (clean shutdown) or a `Purge` (aged out) as
gone. A cleanly deleted key is followed by a second `Purge` one TTL later, when the
delete marker itself ages out; that is harmless. If a bucket already exists with a
different `max_age`, the bucket's value governs and the daemon's heartbeat follows
it, with a warning. Rehydrated read-only sessions are not listed, as in etcd
presence.

**Config.** `[mesh] registry = true` under the existing `[mesh]` section (its
`enabled` master switch and `nats_url`), so there is one NATS address per daemon.
No NATS credentials yet: the operator's server has no auth, and auth is its own
increment (§11, §15).

**Failure.** A misconfiguration (`registry_ttl_secs` below 6) is a refusal to start,
naming the setting. A
NATS server that is unreachable, or one without JetStream or the buckets, is a
runtime fault: the daemon logs one error naming the cause and the remedy (for
example "JetStream is not enabled on the NATS server at <url>: add `jetstream {}`
to its nats-server.conf"),
keeps serving its sessions over stdio, retries on the tick, and logs again when it
recovers. Presence trouble never affects a session.

etcd presence stays as it is. mu-dialogue reads it for agent-to-agent peers, which
is out of scope here.

**Reader.** `RegistryReader` (library) reads both buckets as data, for any client.
`mu live-sessions [--watch] [--all]` prints them for a person: one line per
session, the ones waiting on the operator first, then working, then the rest,
each saying where it runs and what it is doing. Workers fold into their parent
as a count, and one-shot `mu ask` sessions are hidden unless `--all`. The IRC
status view (§13) is the next reader of the same data.

**Telling sessions apart (R1).** A fleet has many sessions, and a daemon id or
`session-N` identifies none of them to a person. Each `mu_sessions` value is
`SessionInfo` plus what the daemon already knows: `cwd` and `client` (a new
`SessionOrigin` log event, written when a session is built; `create_session` takes
an optional `client`, which `mu ask`, mu-solo and mu-tui now send), `role` (the
log's `FallbackArmed`), and the first prompt on one line. An explicit
`session.set_name` is left until these turn out not to be enough.

## 8. Approvals (R7)

`input_required` becomes durable state, not an in-memory oneshot that auto-denies
in 180 s:
- On raise: an `evt.input_required` event AND a `mu_approvals` entry
  `{request_id, tool_name, summary, arguments, raised_at_ms, expires_at_ms?}`, and
  the session's status becomes `awaiting_approval`.
- `session.respond_to_input_required` (exists) answers it; the entry is deleted and
  an `evt.input_resolved {request_id, decision, by}` is published, so every device
  clears it.
- **Who can approve is declared, not guessed.** Today's compiled 180 s
  (`APPROVAL_GATE_TIMEOUT`, `execute_tools.rs`) is a fail-closed floor: "no answer"
  can be a silent human, a dropped channel, or an approver that does not exist (the
  MCP `code_index` wedge), and a turn must never park forever. Its own comment
  names the refinement this spec adopts: a session-declared approver. `create_session`
  takes `approver: "interactive" | "none"`.
  - `none` (workers, autonomous runs, `mu ask`, scripts): an approval prompt is
    denied at once with today's typed refusal, so a model can route around it. No
    wait at all.
  - `interactive`: the policy is `wait` or `deny_after_secs = N`, per session
    (create param), with a daemon default in config.
  - Not declared: the bounded deny stays, now read from config
    (`[session] approval_timeout_secs`, default 180), not compiled.
- **Default for an interactive session (ruling 3): `wait`, with notification.** It
  waits and raises `evt.attention {reason: approval}` (§10), so the operator is told
  rather than timed out. Waiting is safe here because the prompt is durable state
  any client can answer, not a reply owed on one connection: a dropped channel
  loses nothing. `deny_after_secs` is available per session; it is not the default,
  because an auto-deny ends a turn the operator meant to come back to. A denial by
  timeout is recorded as `by: "timeout"` in `input_resolved`, so it reads as our
  limit firing.
- `session.approvals.list` (new) returns open prompts for clients without KV.
- The prompt is also written to the session log (today it is not), so replay shows it.

## 9. Watching and catching up (R3, R4, R5)

The session is the store; NATS carries traffic and keeps nothing. Each session has
two streams:

- `evt.<kind>`: the session log. Every line appended to a session's log is
  published as written, from inside the log's append lock, so lines go out in id
  order. The ids are contiguous per session, so a subscriber that sees a gap, or
  connects mid-session, asks the session for what it lacks with `session.events`
  (`after_event_id`, `limit`), which returns the same records. An id the log
  itself never filled (an append that failed after taking its id) comes back
  absent; that is not a loss. A client joining a long session reads the newest
  page first (`from_end: true`, `limit`) and pages back with `before_event_id`
  set to the response's `prev_event_id`; every page is in ascending id order.
- `live.<kind>`: the notifications a stdio client gets, deltas and finalized text
  included, so a client renders a turn the way mu-solo does. Unnumbered and
  lossy: what lasts is in the log. P4 found deltas are most of the traffic (31 of
  a one-word turn's 34 notifications were thinking deltas and status ticks), so
  they stay off the log stream.

Joining a session (planned with P6, operator 2026-11-18): a remote client's first
request to a session is a registration that carries its credentials (the §11
capability, `observe` at least). The capability is checked there, since events
are not individually authorized. A successful reply carries the most recent
information: the newest page of the log (what `from_end` returns) and the
session's current state. A client subscribes to `evt.>` before it registers, so
nothing falls between the reply and the stream; it drops any line whose id is
already in the reply, and pages back with `before_event_id` only if it wants
older history.

Built (P6b, 2026-11-23): the registration is `session.join` on the session's
command subject, `{session_id, limit?}`, needing `observe`. The reply is
`{session, events, prev_event_id?, last_event_id?}`: `session` is the
`session.list` entry for this session (status, model, usage, cost), derived after
the page was read so it is never older than the page; `events` is the `from_end`
page; `prev_event_id` is the `before_event_id` for the next older page; and
`last_event_id` is the newest id in the log when the page was read, so a client
drops streamed lines at or below it: each is in the page, or older than the page
(more than `limit` lines landed between subscribing and joining) and reachable by
paging back. `limit` 0 means the default, as for `session.events`. The join journals like any read, into the session's own log, so a
join is recorded where its session is (the page ends with its `command_received`,
and its receipt is the next line on the stream). Joining holds no state on the
daemon: the capability is checked on every later command as well.

An approval prompt (`input_required`) is only on `live.` until P8 logs it, so each
one dropped is warned about on its own: the session is still waiting, and the
client that created it can answer (with `[mesh].registry` on, `mu_sessions` also
shows it as awaiting input).

Publishing is `[mesh] events = true` (under `enabled`), core NATS, whether or not
the logs are also kept on disk (`[session].persist_events_to_disk`). While the client
is not connected, messages are dropped and counted, not queued. Each drop is logged
as its cause: not connected, a failed publish (the error says why, e.g. a payload
over `max_payload`), or our own `[mesh].events_call_timeout_ms` (default 1000)
firing. Core NATS does not acknowledge a publish, so a server refusal (a
permissions violation) arrives later as a connection event and is logged from
there. Log lines wait in a queue of `[mesh].events_queue_len` (default 4096); the
append never blocks, and a line that finds the queue full is dropped and counted (a
gap the subscriber fills from the session). A zero timeout or queue, or a
`nats_url` no client can be built from, is a refusal to start; an unreachable
server is not, since the client reconnects. A disconnect is warned about as a
possible loss of what was published just before it. If the live publisher falls
behind, the router first sheds its streaming notifications (logged with the count)
and, past its slow-consumer cap, the publisher re-registers its tap and carries on.

Why not a JetStream stream of events (the earlier plan, `MU_EVENTS`): it would be a
second copy of the session log, with its own retention, its own cursor (a stream
sequence unrelated to the log id) and its own limits to configure, for replay the
session already serves by id. The cost of serving from the session is that its
daemon must be up; a dead daemon has no live session to catch up with, and its logs
stay readable offline (`mu list-sessions`, `mu recover`).

The forwarder's "notifications to the creating origin" (`pipeline.rs:933`) becomes
"notifications to the session's publisher": the NATS adapter publishes every
session's events; stdio clients keep receiving the sessions they created (and may
request others' via the same subjects when connected to NATS).

## 10. Controls added (R8, R11, R14)

New methods:
- `session.stop_autonomous {session_id, reason?}` — ends an autonomous run (today
  `ask_session` is refused during one and nothing stops it).
- `session.wake {session_id}` — wakes a parked session now.
- `session.set_name` (§7), `session.approvals.list` (§8).

Attention events (R14), published by the daemon on the session subject:
`evt.attention {reason: input_required|approval|done|error|autonomy_ended, summary}`.
A client decides how to notify (an IRC highlight, a push, a desktop notification).
`specs/plans/mu-away-operator-notify.md` (PR #431) becomes one consumer of these.

`create_session` takes policy as params instead of spawn flags (`tools`,
`bash_policy`, `thinking`, `bare`, `ephemeral`, `mcp`), each gated by the caller's
capability. Provider credentials are resolved BY THE DAEMON from its config by
endpoint name; `api_key` stops travelling on the wire.

## 11. Auth (R16)

- **Connection:** NATS user credentials (nkey/creds), configured on the server; the
  operator's devices and the daemons each have one.
- **Per request:** the envelope's `capability` is a biscuit (the mesh's existing
  issuer/verifier, `mesh::verify…`), checked in `ingest` before dispatch. Rights:
  `observe` (subscribe/list/events), `prompt` (ask/interject), `approve`,
  `control` (cancel/stop/wake/set_route/config), `create` (create/resume). A client
  holds an attenuated token; the IRC adapter, for instance, gets
  `observe+prompt+approve+control` for the operator's sessions.
- Events are not individually authorized; NATS subject permissions restrict who can
  subscribe to `mu.api.v1.>`.
- Built so far (P6a, operator 2026-11-18): commands over NATS carry the capability
  from the start. `[mesh] serve` verifies it on every command against the
  `[mesh].issuer_key` public half, for the right the method needs (`observe` for the
  read-only methods), and the request enters `ingest` under the auth state that
  check produced, so a missing or failed capability is journaled and rejected by the
  auth gate (`AUTH_REQUIRED`, `AUTH_DENIED`). Peers on the same subscription share
  no auth, which is why the mesh can now serve under an enforcing `[auth]`. A
  daemon with `serve` on and no valid issuer key refuses to start (operator,
  2026-11-23): invariant 7 over keeping the daemon up without the mesh.
- The stdio spawn path keeps working: a spawning client is issued a root capability
  at spawn (today's trust-on-spawn), carried in the same envelope field.
- `granted_capability`, recorded and ignored today, becomes the enforced one.

## 12. Daemon lifetime (R13)

`mu serve` runs as a long-lived service (rc.d / supervisor), not a child of a
client. stdio becomes one optional adapter; its EOF closes that connection, not the
daemon. Shutdown is a signal or `daemon.shutdown` (capability `control`). Sessions
survive client disconnects; `create_session` from any client lands in the running
daemon. Spawning a private daemon remains a convenience for `mu ask` and tests.
(Bead mu-weg covers the lifetime half.)

## 13. Clients

- **Shared client crate** (`mu-client`): envelope types (from mu-001), a
  `Connector` trait with `stdio-spawn` and `nats` implementations, typed calls,
  an event stream per session, reconnect + catch-up. mu-solo, mu-tui and `mu ask`
  move onto it; their three hand-rolled JSON-RPC clients and three token
  generators go.
- **Gaps the clients currently fill locally move into the API:** effort levels and
  model lists (mu-uvuo), pricing, `context_assembly` as an event (mu-hs6h), skills
  via `capabilities/discover`, config errors as a notification. `mu console`
  writes `OperatorMark` through a method (`session.mark`) instead of into the log.
- **IRC** is one client. It needs: KV watch → a status view (one line per session)
  and one buffer per session; `evt.>` → rendered lines in that buffer; a line typed
  in the session's buffer → `ask_session`; `yes`/`no` (or `!approve <id>`) →
  `respond_to_input_required`; `evt.attention` → a highlight of the operator's
  nick (his IRC app notifies); catch-up on join from the stream. Ergo supplies
  history on reconnect (CHATHISTORY), several devices (multiclient), and
  notifications. It does NOT need puppets, RELAYMSG mirroring, label topics from
  etcd or the mesh-DM routing of `mu-irc-gateway`.

  **Session traffic is the operator's only (ruling 5).** Agents do not see each
  other's prompts and responses: that would fill their contexts with traffic they
  don't need. So each session gets a QUERY buffer between the IRC client's nick for
  that session and the operator, not a channel agents could join. NATS subject
  permissions give the same rule to every other client: agent credentials carry no
  subscribe right on `mu.api.v1.*.*.evt.>`. When agents need to share, the operator
  tells them to talk, and they use mesh DM / mu-dialogue, which stays (ruling 6).

  **What gets a line (proposed default; to settle when the IRC client is built):**
  the operator's prompt; the final assistant text of each turn, split to IRC line
  length; one line per tool call (`tool name: short argument summary`, e.g. the
  path or the first 60 characters of a bash command); a tool result only when it
  is an error; approvals with their request id; status changes. Thinking and text
  deltas get no lines. `!more` shows the full output of the last tool call.

## 14. What happens to the existing IRC work

`mu-irc-gateway` is frozen. Its IRC plumbing (adapter: registration, SASL, caps,
ISUPPORT; transport; framing; membership; channel reconciler) is reusable and moves
to a crate the new IRC client builds on (working name `mu-irc-core`). Its mesh-DM
bridge (routing, outbound addressing, puppets, topics from labels, SETNAME,
RELAYMSG) is agent messaging and keeps running as-is (ruling 6). mu-dialogue and the daemon's mesh DM / mailbox stay as they are
(agent-to-agent; out of scope).

## 15. Increments

Each increment is ONE functional concept, sized to review as that concept, with a
cheap check and a stated result that would mean we're on the wrong path. Only the
current increment is built; later increments are not designed in code ahead of
time, and the order below may change on what an earlier one teaches. Every merge
leaves main working (ruling 4). The operator's surface arrives at P9; everything
before it is checked at the desk with the `nats` CLI or `mu live-sessions`.

| # | Concept | Proves | Check | Wrong-path signal |
|---|---|---|---|---|
| P1 | Daemon identity on NATS: `[mesh] registry`, the `mu_daemons` entry keyed by daemon id with heartbeat + TTL, fail-fast/fail-loud rules (§7) | the daemon can own fleet-visible state with clean liveness | watch `mu_daemons` while starting, killing, restarting a daemon | TTL/heartbeat can't give a clean "gone" signal; names collide across instances (found: they would; §6 now keys by daemon id) |
| P2 | Session presence: `mu_sessions` entries with status and identity, the reconciliation tick, `mu live-sessions` (§7) | the status model maps cleanly from today's `provider_status` | `mu live-sessions --watch` across a real turn, an approval and a close | status needs states the forwarder can't see (found: open approvals; read from the live registry); sessions can't be told apart (found; identity folded in from P3) |
| P3 | Explicit names: `session.set_name`, if cwd/client/role/first prompt prove not enough (§7) | — | — | — (may not be needed) |
| P4 | Events out: every session's log published line by line to `…<s>.evt.<kind>`, and its notifications to `…<s>.live.<kind>`, beside today's delivery to the creator (§9) | fan-out off the box without changing stdio clients | `nats sub 'mu.api.v1.<d>.<s>.evt.>'` during a mu-solo turn | the forwarder's origin binding (`pipeline.rs:933`) can't be widened without touching ingest |
| P5 | Command envelope: `v`, `capability`, `id` on the stdio framing; all in-tree clients updated (§5) | one envelope serves both adapters | `just ci` + each client's smoke | the envelope forces per-client special cases |
| P6 | Commands in: a NATS request/reply adapter over `ingest`, read-only methods first (`session.list`, `session.events`, `stats`), and the join registration that returns the newest page (§6, §9). Split: P6a the adapter, speaking the §5 envelope with the §11 capability checked per command (built; stdio keeps its framing until P5); P6b the join registration, `session.join` (built, §9) | a non-spawning client reaches `ingest` with journaling intact | `nats req` from a second machine | `ingest` assumes a connection lifetime it shouldn't |
| P7 | Prompting over NATS: `ask_session` (+ interjection) on the adapter | a remote prompt is a real turn | `nats req` a prompt, watch P4 events | interjection semantics differ per adapter |
| P8 | Approvals as state: persisted prompt, `mu_approvals`, the declared `approver`, `wait` default for interactive, configurable bound otherwise, `input_resolved`, attention event (§8, §10) | approvals can outlive the 180 s oneshot | raise one, answer via `nats req` from another box | the oneshot is load-bearing elsewhere |
| P9 | IRC client v0: status view from P2 | the IRC client's shape on `mu-irc-core` | the phone shows sessions | — |
| P10 | IRC: a query buffer per session rendering P4 events | rendering rules (§13) | watch a turn on the phone | — |
| P11 | IRC: prompt + approve from the buffer | the loop closes | answer an approval from the phone | — |

After P11 (the v1 bar is in §16, ruling 7), each of these is one increment:
stopping and waking (`stop_autonomous`, `wake`); the rest of per-request auth (§11:
the other rights, attenuated tokens per client); daemon
lifetime (§12, mu-weg); the `mu-client` crate
and the client-side gaps (§13); and moving policy and credentials into the daemon.
Commands over NATS carry a capability from P6a on (operator, 2026-11-18). Stop is
in the v1 bar, so it is the first of these.

## 16. Rulings (operator, 2026-10-14)

1. **JetStream: on.** The operator enables it, for the KV buckets (§6, §7). No
   non-JetStream mode. Amended 2026-11-17: no JetStream stream of events; the
   session log is the only store, published line by line, and catch-up asks the
   session (§9).
2. **Fleet registry first.** The two KV buckets are the registry (§6, §7).
3. **Approvals wait by default** in a session that declares an interactive
   approver, with an attention event (sessions without one keep a fail-closed
   deny, §8); `deny_after_secs` is
   opt-in per session (§8).
4. **The stdio framing may change.** He is the only consumer. Every PR keeps main
   working by updating all in-tree clients with the wire (§5).
5. **Session traffic is private to the operator.** Agents should be able to talk to
   each other, but should not see each other's prompts and responses; when they
   need to share, he tells them to talk directly. IRC uses a query buffer per
   session (§13). Which tool output gets a line is still open; §13 has a proposed
   default.
6. **Keep the mesh-DM bridge** in `mu-irc-gateway` (§14).
7. **v1 = list, watch, prompt, approve, stop from the phone; presence first**
   (§15: P1–P11, then stop). Resume comes later. Later clarification (2026-10-15):
   the point of the order is small single-concept PRs that expose a wrong path
   early, not a first step that is useful on its own.
8. **Capabilities from the start (2026-11-18).** Commands over NATS carry the §11
   capability, checked on every command, from P6a on (this answers what was left
   open about NATS connection creds being enough). The NATS adapter speaks the §5
   envelope first; stdio keeps its framing until P5.

Still open:

- The tool-output line rules in §13, which can wait until P10.

## 17. Sources

- Audit and requirements map: memory `mu-client-daemon-queue-requirement-and-audit-2026-10-14`.
- mu-001 (protocol is the contract), mu-002 (stdio; other transports deferred),
  mu-005 (spawned child as a rule — superseded by §12), mu-029 (input_required),
  mu-035 (provider_status), mu-036 (autonomy), mu-037 (mailbox), mu-038 (projection
  queries, "reattach + transcript replay"), mu-046 (ingest pipeline, adapter seam).
- `specs/architecture/claude-code-design-influences.md` §3.5, §8.2.
- `specs/plans/mu-irc-gateway-v0.md`, `specs/plans/mu-irc-gateway-v1-puppets.md`,
  `specs/plans/mu-away-operator-notify.md` (PR #431).
- Beads: mu-o7e2a, mu-irc-remote-session-zgbdz(.1), mu-weg, mu-iqo8, mu-40oa,
  mu-cuf, mu-uvuo, mu-hs6h, mu-ki1f.1, mu-w9mi8, mu-5pn, mu-71q.
