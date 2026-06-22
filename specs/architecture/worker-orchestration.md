# Worker Orchestration — as built

| field | value |
| --- | --- |
| status | **as-built** — reflects shipped reality as of 2026-06-22 |
| beads | mu-slat, mu-037 mailbox, mu-ktj0 non-POT spawn rewrite |
| supersedes | the "what is true now" question for `specs/mu-slat-design.md` (historical design-space record) |

This is the current reference for how mu spawns and coordinates worker sessions.
POTs/jails are no longer part of the runtime path. `mu-spawn` is now a small
non-POT dispatch wrapper over current agent runtimes:

- `claude -p` for `claude-oauth`
- `mu ask --bare` for all other providers

Provider/model choice comes from `scripts/agent-role` unless explicitly
overridden.

## Thesis

A worker is a child agent process launched by `mu-spawn`, registered by mu as a
`SubprocessSession`, and monitored by the daemon. The worker receives the task on
stdin, prints its result to stdout, and the daemon posts that stdout back to the
calling session's mailbox. If the worker also has dialogue/MCP tools available,
it is instructed to register/communicate as `mu:<daemon_id>:<session_id>`.

```
calling session (agent loop)
  │  spawn_worker{prompt, model?, timeout_secs?}
  ▼
spawn_worker tool ──► spawn_worker() ──► mu-spawn ──► claude -p | mu ask --bare
  (per-session,         registers worker,   role/model       (non-POT child
   stamps reply_to)     records task,       dispatch          agent process)
                        monitors child
                                                                        │ stdout
   calling session   ◄── AgentInput::MailboxMessage ◄── mailbox task_result append
   (loop wakes)          (try_send if live)       to reply_to session
```

## The pieces

- **`spawn_worker` tool** — `crates/mu-coding/src/tools/spawn_worker.rs`.
  Inputs: `prompt` (required), `model`, `timeout_secs` (default 3600). The tool
  instance is scoped to its calling session: `build_config()` stamps
  `parent_session_id` into the worker's `reply_to`. Per-session injection happens
  at session creation in `serve/handlers/session.rs` (gated on `events_dir` —
  production only). Without that scoping, results have no reliable path back to
  the caller.

- **Spawn mechanics** — `spawn_worker()` in
  `crates/mu-coding/src/serve/worker.rs` registers a `SubprocessSession`, records
  the task in the worker's mailbox/event log for observability, launches
  `mu-spawn`, writes the task to child stdin, and monitors process exit.

- **Dispatch wrapper** — `scripts/mu-spawn` resolves a role/rank through
  `scripts/agent-role` unless `--provider` / `--model` or `MU_SPAWN_PROVIDER` /
  `MU_SPAWN_MODEL` override it. `claude-oauth` routes to `claude -p`; other
  providers route to `mu ask --bare` with useful built-in tools. The wrapper
  passes worker identity in the system prompt, including the dialogue peer id
  `mu:<daemon_id>:<session_id>` when known.

- **Result return** — the monitor captures stdout/stderr. On success it appends a
  `MailboxMessagePosted { message_kind: "task_result" }` to the `reply_to`
  session's event log for auditability, then wakes a live parent with an inline
  `AgentInput::WatchCompleted` summary containing the worker stdout/stderr. The
  inline wake is deliberate: worker completion should not depend on the model
  having `mu_mailbox_read` or on polling mcp-dialogue. If the child has dialogue
  tools and posts its own result, the normal mailbox path in
  `serve/handlers/mailbox.rs` still works.

- **Deadline** — `monitor_worker` races the child against `timeout_secs`; on
  timeout it kills the child, emits `WorkerTimeout`, and notifies the parent.

- **Mailbox RPCs (mu-037)** — `peer.hello`, `mailbox.post`, `mailbox.list`,
  `mailbox.read`, and `mailbox.consume` are in `serve/handlers/mailbox.rs`. See
  `specs/mu-037-peer-discovery-mailbox.md`.

## Known gap: no dead-letter

If a worker's result targets a session that is gone (not in the registry), the
monitor has nowhere durable to deliver the result. The loop relies on `reply_to`
being correct at spawn time and the calling session still being live when the
result lands. There is no re-delivery or dead-letter queue.

## Relationship to goal-protocol and host workers

The old distinction between mu-native `spawn_worker` and host-side
`agent-spawn-v2` narrowed: both now use non-POT child agent processes. The
mu-native path still differs in one important way: the daemon registers the
worker as a session and routes completion back through the caller's mailbox.
Host-side goal-protocol workers are still ordinary shell-spawned processes whose
supervisor drains stdout/exit code directly.

| | **mu `spawn_worker`** | **host goal-protocol worker** |
| --- | --- | --- |
| Trigger | in-loop tool call by a mu session | host shell / loop-guard wrapper |
| Process | `mu-spawn` → `mu ask --bare` or `claude -p` | host-selected agent command |
| Result path | stdout captured by daemon → mailbox wakeup | stdout drain + exit code |
| Registry | `SubprocessSession` in mu daemon | external process table / run dir |
| Use for | mu-native agent-spawns-agent | autonomous `/goal` host workers |

## Historical notes

`specs/mu-slat-design.md` and older versions of this document describe a POT +
pty/vt100 architecture. That path was removed by mu-ktj0 because POTs are no
longer the worker isolation mechanism. The historical docs remain useful for why
mailbox routing exists, but not for current launch mechanics.
