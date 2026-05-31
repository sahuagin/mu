# Worker Orchestration — as built

| field    | value                                                     |
| -------- | --------------------------------------------------------- |
| status   | **as-built** — reflects shipped reality as of 2026-05-31  |
| beads    | mu-slat (P1), mu-037 (mailbox), mu-k4i3 (this reconcile)  |
| supersedes | the "what is true now" question for `specs/mu-slat-design.md` (which remains the design-*space* record) |

This is the single current reference for how mu spawns, coordinates, and reaps
worker sessions. It exists because the same material was scattered across the
design doc, the code, several memories, and the bead worklist — forcing a full
re-read every session. Read **this** first; follow the cross-references only
for depth.

> **Design doc vs reality.** `mu-slat-design.md` (draft, 2026-05-25) explored
> the option space and *recommended* Option B (`claude -p` + stream-json) for
> programmatic orchestration, calling the pty path "not viable for programmatic
> orchestration." **What actually shipped is the opposite:** the pty/vt100
> interactive path (Option A) coordinated via the **mailbox** (Mechanism 3),
> not stdout stream-json parsing. The driver was billing — the interactive
> `cli` entrypoint stays on the subscription pool; `-p` (`sdk-cli`) moves to
> the metered credit pool after 2026-06-15. mu chose subscription billing and
> paid for it with a mailbox round-trip instead of a stdout parser.

## Thesis

A worker is an **interactive Claude Code process** that mu drives under a pty,
spawned in-loop by the `spawn_worker` tool. The worker does its task and
**posts its result back through the mailbox** to the session that spawned it;
that post wakes the calling session's agent loop directly (event-driven, not
polling). The **host reaps** the worker when the result lands, because an
interactive Claude under the FreeBSD linuxulator cannot reliably self-kill.

```
calling session (agent loop)
  │  spawn_worker{prompt, model?, timeout_secs?}
  ▼
spawn_worker tool ──► spawn_worker() ──► mu-spawn (pot clone) ──► pty/vt100 spawn
  (per-session,         registers in       agent-runtime pot       (interactive
   stamps reply_to)     registry; posts    + linux-fs mounts        claude, cli
                        task to mailbox)                            entrypoint)
                                                                        │ works
   calling session   ◄── AgentInput::MailboxMessage ◄── mailbox.post{kind:"result"}
   (loop wakes)          (try_send if live)              to reply_to session
                                                              │ and on kind=result:
                                                         host-side reap_worker()
```

## The pieces (file:line anchors)

- **`spawn_worker` tool** — `crates/mu-coding/src/tools/spawn_worker.rs:19-144`.
  Inputs: `prompt` (required), `model`, `timeout_secs` (default 3600). The tool
  instance is **scoped to its calling session**: `build_config()`
  (`spawn_worker.rs:46-62`) stamps `parent_session_id` into the worker's
  `reply_to`. Per-session injection happens at session creation in
  `serve/handlers/session.rs:187-206` (gated on `events_dir` — production only).
  *This per-session scoping is the `03015a51` fix — without it, results had no
  reliable path back to the caller.*

- **Spawn mechanics** — `spawn_worker()` at `crates/mu-coding/src/serve/worker.rs:51`
  registers the worker, posts the task to its mailbox, and spawns it. The spawn
  is a Rust-owned pty via `crates/mu-coding/src/serve/pty_spawn.rs:79-189`
  (`portable-pty`), which **replaces the old `script(1)` + stdin-pipe kickstart
  hack**. It waits for prompt readiness by polling the vt100 screen for the
  `❯` marker / permission footer (45s timeout), then types the kickstart with
  human cadence (40–90ms jitter) to dodge the TUI's paste-burst debounce.

- **Pot + mounts** — `scripts/mu-spawn` (zsh) clones the `agent-runtime` pot
  template and launches `jexec ... claude`. The linux-fs mounts
  (linprocfs / linsysfs / tmpfs `/dev/shm` / fdescfs+linrdlnk — the CPU-visibility
  fix) live in the **template's `fscomp.conf`**, not in mu; `mu-spawn` only does
  pot lifecycle + MCP-config placement + launch.

- **Result return** — the worker posts `kind="result"` to its `reply_to` session
  via `mailbox.post` (`crates/mu-coding/src/serve/handlers/mailbox.rs:83-225`).
  The handler appends a `MailboxMessagePosted` event and, **if the target is
  live**, injects `AgentInput::MailboxMessage` into its input channel via
  `try_send` (`mailbox.rs:203-210`) — a direct channel push, *not* polling. The
  agent loop (`crates/mu-core/src/agent/loop_/mod.rs`) turns that into a
  synthetic user message and queues an `InvokeLlm`, so the caller wakes
  immediately.

- **Host-side reap** — on a `kind="result"` post, the same handler calls
  `reap_worker` (`mailbox.rs:217-219` → `serve/sessions.rs:497-514`), which uses
  the host-side pty `ChildKiller` set at `worker.rs:174` to SIGTERM the child.
  **Why host-side:** an interactive Claude under linuxulator is `pkill`-blind to
  itself; the FreeBSD host pty owner is the only authority that can kill it.

- **Supervisor fallback** — a well-known `supervisor` session is registered at
  daemon startup (`serve/mod.rs`, gated on `events_dir`). If a worker's
  `parent_session_id` is `None`, its result `reply_to` falls back to
  `supervisor` (`worker.rs:114-117`). The supervisor is a log-only ghost (no
  agent loop) — results land in its event log but wake nothing.

- **Deadline** — `monitor_worker` (`worker.rs:240-275`) races the child against a
  `timeout_secs` deadline; on timeout it kills the pty, emits `WorkerTimeout`,
  and notifies the parent.

- **Mailbox RPCs (mu-037)** — `peer.hello` (handshake → opaque token allowing
  `mailbox.post`), `mailbox.post` / `mailbox.list` / `mailbox.read` /
  `mailbox.consume`, all in `serve/handlers/mailbox.rs`. See `specs/mu-037-peer-discovery-mailbox.md`.

## Known gap: no dead-letter

If a worker's result targets a session that is gone (not in the registry),
`mailbox.post` returns `INVALID_PARAMS` and **the result is lost** — not queued,
not retried, not appended anywhere recoverable. The loop relies on `reply_to`
being correct at spawn time *and* the calling session still being live when the
result lands. There is no re-delivery or dead-letter queue. (Memory `f5ba1b62`
called this "dead-letter detection"; what exists is detection-at-post, not
durable handling.)

## What shipped, when

- **2026-05-27** — full mailbox round-trip validated: supervisor posts task →
  worker reads → works → posts result back. Commit `e52a885b` (memory `f0aefefb`).
- **2026-05-28** — Phase 2, event-driven: `AgentInput::MailboxMessage` wakeup,
  `spawn_worker` tool, worker deadline. Shipped end-to-end across 4 commits on
  main + the `mu-spawn` CPU-visibility mount fix (memories `f5ba1b62`, `521c83c2`).

## Open follow-ups (bead worklist)

- **Drive from mu-solo** — wire the operator's mu-solo session as the `reply_to`
  so spawns from the daily driver route results home (memory `521c83c2`).
- **`/btw` concurrent streaming** — demux concurrent worker turns by `session_id`
  in the live viewport (**mu-d04a**, P1, open; WIP lineage lives off-main).
- **Pot mount leak** — spawned pots leak a mount on teardown (memory `521c83c2`).
- **`mu-sabe`** (P3) — `mu-worker` pot capability: Rust template + safe
  memory/task_log mount strategy, so walk-away goal-protocol runs can target mu.
- **`mu-pr6r`** (P2) — observer architecture: process-layer auditors over worker
  event streams (the "invariants are checked, not trusted" triage layer).

## Relationship to goal-protocol and agent-spawn-v2

Two distinct worker paths exist; don't conflate them:

| | **mu-slat `spawn_worker`** (this doc) | **agent-spawn-v2 + `claude -p`** (goal-protocol) |
| --- | --- | --- |
| Trigger | in-loop tool call by a mu session | host shell / loop-guard wrapper |
| Process | interactive Claude under a pty | headless `claude -p` stream-json |
| Billing | subscription (`cli` entrypoint) | credit pool (`sdk-cli`) post-6/15 |
| Result path | mailbox → caller's loop (auto-reap) | stdout drain + exit code |
| Use for | mu-native agent-spawns-agent | autonomous `/goal` host workers, pots |

The `~/.claude-personal/skills/goal-protocol` skill documents the **right-hand**
column; this doc is the **left-hand** column. The goal-protocol skill points
here for the mu-native path.

## Cross-references

| Reference | Provides |
| --- | --- |
| `specs/mu-slat-design.md` | the design-space exploration (Options A–D, phasing) — historical rationale |
| `specs/mu-037-peer-discovery-mailbox.md` | the mailbox + peer.hello wire spec |
| Memory `f0aefefb` | round-trip validated 2026-05-27 (commit `e52a885b`) |
| Memory `f5ba1b62` | Phase-2 event-driven orchestration shipped |
| Memory `521c83c2` | open follow-ups (mu-solo reply_to, /btw, pot mount leak) |
| Memory `b7532871` | validated claude-in-pot recipe (agent-spawn-v2) |
| Memory `a033efde` | 3-layer orchestration pattern (workers / reviewers / supervisor) |
