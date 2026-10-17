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
 subscribe ◀─ mu.api.v1.<d>.<s>.evt.<kind> ◀── publish ──────  forwarder → Router
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

Event (published): `{ "v": 1, "daemon": "<d>", "session": "<s>", "seq": <n>,
"event_id": "<log id or null>", "kind": "<method>", "params": { ... } }`. `seq` is
per-session monotonic over the events on the `evt.` tree (text deltas are not
counted; §9), so a client detects gaps; it is not a replay position. `event_id`
ties durable events to the log for `session.events` catch-up.

Method names and params are **mu-001's**, unchanged where they exist. Over stdio
the same envelope travels as one JSON line (the current framing gains `v`,
`capability`, and `seq`).

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
| `mu.api.v1.<daemon>.<session>.evt.<kind>` | publish | every `session.*` notification except text deltas; one subject per kind so a client can subscribe to `evt.>` or only `evt.input_required` |
| `mu.api.v1.<daemon>.<session>.delta` | publish | `session.text_delta` only: outside the `evt.` tree, so `MU_EVENTS` never stores it (§9) |
| `mu.api.v1.<daemon>.evt.<kind>` | publish | daemon events (`daemon.started`, `daemon.stopping`, `session.created`, `session.closed`) |
| KV `mu_daemons`, key `<daemon>` | JetStream KV | fleet registry: one entry per live daemon (§7) |
| KV `mu_sessions`, key `<daemon>.<session>` | JetStream KV | session registry + status (§7) |
| KV `mu_approvals`, key `<daemon>.<session>.<request_id>` | JetStream KV | open approval prompts (§8) |
| Stream `MU_EVENTS` over `mu.api.v1.*.*.evt.>` | JetStream stream | replay/catch-up (§9) |

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
registry. Status transitions are written immediately; the tick catches anything
missed, including spawned workers. Both buckets are created with `max_age` =
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

**Reader.** `mu sessions [--watch]`, a client of the two buckets, prints one line per
session: daemon, name, status, model, time in the current phase, and cost. It is
the check for this chunk, not the operator's surface; the IRC status view (§13)
reads the same buckets later.

**Names (R1).** `session.set_name {session_id, name}` (new) sets the human label;
`create_session` takes an optional `name`. Default: `<role or cwd basename>-<n>`.

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

Live: a client subscribes to `mu.api.v1.<d>.<s>.evt.>`, and to `…<s>.delta` if it
renders streaming text. Any number of clients. Text deltas are published on
`delta`, outside the `evt.` tree, so the `MU_EVENTS` filter never stores them; the
final assistant message (an `evt.` event) carries the whole text, so catch-up loses
nothing but the streaming.

Catch-up: an ordered consumer on `MU_EVENTS` filtered to
`mu.api.v1.<d>.<s>.evt.>`. Its start position is a STREAM sequence, which is global
to `MU_EVENTS` and unrelated to the per-session `seq`. A client keeps the stream
sequence of the last message it consumed (from the message's JetStream metadata)
and resumes at that + 1; a client with no cursor starts from "last N" or a time.
The per-session `seq` is used only to detect gaps and drop duplicates. The stream
has limits (age, bytes) set in config; the durable log remains the source of truth,
and `session.events` (exists) pages it for anything older than the stream holds.
JetStream is required (ruling 1); there is no non-JetStream mode to maintain.

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
before it is checked at the desk with the `nats` CLI or `mu sessions`.

| # | Concept | Proves | Check | Wrong-path signal |
|---|---|---|---|---|
| P1 | Daemon identity on NATS: `[mesh] registry`, the `mu_daemons` entry keyed by daemon id with heartbeat + TTL, fail-fast/fail-loud rules (§7) | the daemon can own fleet-visible state with clean liveness | watch `mu_daemons` while starting, killing, restarting a daemon | TTL/heartbeat can't give a clean "gone" signal; names collide across instances (found: they would; §6 now keys by daemon id) |
| P2 | Session presence: `mu_sessions` entries with status from the forwarder, the reconciliation tick, `mu sessions` (§7) | the status model maps cleanly from today's `provider_status` | `mu sessions --watch` across a real turn, an approval and a close | status needs states the forwarder can't see; the entry churns too much to write per transition |
| P3 | Session names: `session.set_name`, the `create_session` `name` param, defaults (§7) | names belong to the daemon, not the client | set over stdio, see it in `mu sessions` | — (small; may fold into P2 if it is) |
| P4 | Events out: every session's notifications published to `…<s>.evt.<kind>` with `seq`, beside today's delivery to the creator (§9) | fan-out off the box without changing stdio clients | `nats sub 'mu.api.v1.<d>.<s>.evt.>'` during a mu-solo turn | the forwarder's origin binding (`pipeline.rs:933`) can't be widened without touching ingest |
| P5 | Command envelope: `v`, `capability`, `id` on the stdio framing; all in-tree clients updated (§5) | one envelope serves both adapters | `just ci` + each client's smoke | the envelope forces per-client special cases |
| P6 | Commands in: a NATS request/reply adapter over `ingest`, read-only methods first (`session.list`, `session.events`, `stats`) (§6) | a non-spawning client reaches `ingest` with journaling intact | `nats req` from a second machine | `ingest` assumes a connection lifetime it shouldn't |
| P7 | Prompting over NATS: `ask_session` (+ interjection) on the adapter | a remote prompt is a real turn | `nats req` a prompt, watch P4 events | interjection semantics differ per adapter |
| P8 | Approvals as state: persisted prompt, `mu_approvals`, the declared `approver`, `wait` default for interactive, configurable bound otherwise, `input_resolved`, attention event (§8, §10) | approvals can outlive the 180 s oneshot | raise one, answer via `nats req` from another box | the oneshot is load-bearing elsewhere |
| P9 | IRC client v0: status view from P2 | the IRC client's shape on `mu-irc-core` | the phone shows sessions | — |
| P10 | IRC: a query buffer per session rendering P4 events | rendering rules (§13) | watch a turn on the phone | — |
| P11 | IRC: prompt + approve from the buffer | the loop closes | answer an approval from the phone | — |

After P11 (the v1 bar is in §16, ruling 7), each of these is one increment:
stopping and waking (`stop_autonomous`, `wake`); per-request auth (§11); daemon
lifetime (§12, mu-weg); catch-up from the `MU_EVENTS` stream; the `mu-client` crate
and the client-side gaps (§13); and moving policy and credentials into the daemon.
Until per-request auth lands, NATS connection creds on the operator's own server
are the only auth. That is the operator's call (§16, still open). Stop is in the v1
bar, so it is the first of these.

## 16. Rulings (operator, 2026-10-14)

1. **JetStream: on.** The operator enables it. No non-JetStream mode (§9).
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

Still open:

- Until per-request capabilities land (after P11), is NATS connection creds enough
  auth for commands arriving over NATS (P6, P7, P8)?
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
