# mu-dialogue push mailbox v1

| field | value |
| --- | --- |
| status | plan / implementation slice |
| created | 2026-06-30 |
| bead | `mu-dialogue-push-mailbox-v1-3sl2` |
| related | `mu-rkhj`, `mu-nld2`, `mu-q0oe`, `mu-1f27`, `mu-ieb5`, `mu-qddr`, `mu-mcp-status-audit-mu-solo-naqm` |

This is the KISS v1 map for making mu-native dialogue/mailbox delivery
**event-driven** without promoting MCP long-polling into the core agent loop.
It intentionally does not implement the full typed messaging substrate from
`mu-q0oe`; that remains the reserve/future transport epic.

## Terrain checked

- `code-index` outbound MCP works end-to-end: a live `mu ask` session imported
  `code_status` and the model called it successfully.
- `mu-dialogue` works for Claude Code peer messaging today, but its presence
  semantics are unacceptable for mu-native coordination: `dialogue_say` /
  `dialogue_poll` activity creates/refreshes peer rows, and stale session ids
  do not expire.
- A dogfood test showed why model-visible `dialogue_poll` is dangerous in mu:
  after `spawn_worker`, the parent model called `dialogue_poll` for five
  minutes instead of relying on the daemon's worker result/wakeup path.
- The old client-side receive poller has been removed from `mu-coding`; the
  transport-agnostic `AgentInput::DialogueMessage` wake seam still exists in
  `mu-core` and should be preserved for v1 push delivery.

## Non-negotiable constraints

- The session event log is source of truth.
- External transports are adapters into the ingest/session pipeline; they must
  not mutate agent-loop state through side doors.
- Native receive is wake-on-notification only. No client-side or model-visible
  long-poll loop is the mu-native path.
- Tool calls return promptly. Asynchronous delivery/failure is represented by a
  later correlated event/input, not by holding an LLM tool call open across
  turns.
- Presence is explicit and lease-backed; `poll == register` is compatibility
  behavior, not the mu-native registration model.
- MCP remains an edge/compatibility adapter. It may expose legacy tools for
  Claude Code, but it is not the substrate.

## V1 architecture

### 1. Presence: etcd lease registry

Each live daemon/session that opts into dialogue registers its own mailbox under
an etcd lease. The lease is the liveness proof and expires without graceful
shutdown.

Suggested keyspace:

```text
/mu/dialogue/v1/daemons/<daemon_id>
/mu/dialogue/v1/sessions/<daemon_id>/<session_id>
/mu/dialogue/v1/peers/<peer_id>
```

`peer_id` keeps the existing scheme:

```text
mu:<daemon_id>:<session_id>
cc:<claude-code-session-id>
```

Example value:

```json
{
  "peer_id": "mu:abcd:session-1",
  "role": "mu",
  "daemon_id": "abcd",
  "session_id": "session-1",
  "endpoint": "ws://10.1.1.11:49152/mu-dialogue/v1",
  "protocol": "mu-dialogue-push-v1",
  "capabilities": ["mailbox.notify", "mailbox.fetch"],
  "started_at_unix_ms": 1782800000000,
  "version": "0.0.1"
}
```

No secrets belong in etcd. Authentication/authorization for push/fetch is a
separate transport concern.

### 2. Durability: central mailbox store first

For the first reliable slice, keep message durability centralized in the
mailbox/dialogue service. Push is a wake hint, not the only copy.

Minimum durable message fields:

```text
message_id        monotonic or ULID; echoed in every related event
task_id           send-side operation id returned to the model
from_peer
to_peer
thread_id
created_at_unix_ms
state             queued | pushed | fetched | acked | failed
content
attempt_count
last_error
```

UDP/gossip can later carry hints, but message content must remain retrievable
idempotently by `message_id` until acknowledged or expired by policy.

### 3. Send path: immediate ack, later delivery result

Model-facing `dialogue_send` / `mailbox_send` must return immediately:

```json
{
  "task_id": "dlg-task-...",
  "message_id": "msg-...",
  "state": "queued"
}
```

The eventual delivery outcome is a separate event/input:

```text
DialogueDeliveryResult { task_id, message_id, to_peer, state, error? }
```

This preserves provider tool-call pairing: the original LLM tool call already
has its tool result. No provider-specific deferred-tool-call semantics are
needed for v1.

### 4. Receive path: push into daemon, then event log

The dialogue service resolves `to_peer` through the lease registry and pushes a
small notification to the recipient daemon:

```json
{
  "kind": "mail_available",
  "message_id": "msg-...",
  "from_peer": "cc:...",
  "to_peer": "mu:abcd:session-1",
  "thread_id": "...",
  "preview": "..."
}
```

The daemon must route this through the normal ingest/session path:

1. network push arrives at a daemon dialogue adapter;
2. adapter validates the recipient/session and enqueues a control/session input;
3. session event log records the arrival/fetch result before in-memory state is
   projected;
4. the session wakes via `AgentInput::DialogueMessage` (or a deliberately
   versioned successor after a separate design) with inline content or a
   bounded fetch result.

The v1 implementation should reuse `AgentInput::DialogueMessage` as the wake
seam. It already has tests proving an idle session wakes and gets one LLM turn.
Update its comments to remove references to the old per-session poller.

### 5. Fetch/ack path

A pushed `mail_available` may include enough inline content for small messages.
For larger messages or operator policy, the daemon fetches by `message_id` using
a bounded request:

```json
{ "message_id": "msg-..." }
```

Fetch is not a long-poll; it either returns the persisted message quickly or a
bounded error/task status. After successful event-log append, the daemon may ACK
so the central store can mark the message `acked` or retain it for history.

### 6. Model-visible tools

Native mu sessions should not see `dialogue_poll` as the default mailbox path.
For compatibility with Claude Code, the existing MCP `dialogue_poll` can remain
available at the edge, but mu should expose/gate a safer surface:

- `dialogue_send` / `mailbox_send`
- `dialogue_fetch` / `mailbox_fetch`
- `dialogue_ack`
- `dialogue_status`

If the old `dialogue_poll` remains configured, it should be visibly marked as
compatibility/long-poll and omitted from normal mu profiles unless explicitly
requested.

### 7. Operator-visible status

The operator needs to see which of these surfaces are real in the current
daemon. The first implementation slice is `mu-solo /mcp`, which should show:

- daemon MCP socket path and whether the session-status subscription has
  produced updates;
- configured outbound `[[mcp.servers]]` entries, allowlists, side-effect
  classification, and known long-poll footguns such as `dialogue_poll`;
- explicit gaps: live imported-tool health and dialogue lease/push state are
  not daemon-authoritative until a future `daemon.mcp_status` /
  `dialogue.status` RPC exists.

A later daemon-authoritative status RPC should report import success/failure,
imported tool names, last error, lease id/TTL, push connection state, pending
messages, and stale compatibility peers.

## Recommendation for current `mu-dialogue`

Short term: **wrap and quarantine**.

- Keep the existing `mu-dialogue` MCP server for Claude Code compatibility and
  known working peer messaging.
- Do not treat its poll-derived presence as authoritative.
- Do not use model-visible `dialogue_poll` as mu-native receive.
- Build the lease-backed push path alongside/around it. Replace the service
  later if wrapping it forces invariant violations.

## Later layers, explicitly deferred

- Gossip can be valuable for resilient dissemination of small facts
  (presence/status/config/lifecycle). If added, it should run in its own
  event loop/sidecar, use enumerated fact/message ids for dedupe, and touch the
  local daemon only for relevant inbound/outbound traffic. Do not make gossip
  part of the v1 mailbox critical path.
- The typed substrate (`mu-q0oe`: envelope, schema/catalog, Aeron/ZeroMQ/SBE /
  Cap'n Proto, MCP edge adapter) remains the long-term architecture. V1 should
  not block on it.
- RabbitMQ/AMQP remains an honest alternative if durability/routing/fanout/DLQ
  requirements outgrow the thin mailbox service. Do not reimplement a broker
  one accident at a time.

## Implementation beads to split/follow

1. `mu-dialogue-presence-etcd`: implement lease-backed presence and make
   `dialogue_peers` show lease-live vs activity-derived compatibility peers.
2. `mu-dialogue-push-daemon-adapter`: daemon listening/push adapter that routes
   through control/session ingest and wakes via `AgentInput::DialogueMessage`.
3. `mu-dialogue-send-async-result`: model-facing send returns `{task_id,
   message_id, queued}` and later emits correlated delivery/failure events.
4. `mu-dialogue-quarantine-poll`: hide/gate `dialogue_poll` from normal mu tool
   imports while preserving CC compatibility.
5. `mu-mcp-daemon-status-rpc`: daemon-authoritative MCP/dialogue status RPC for
   mu-solo and CLI.
