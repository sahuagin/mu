# Design: mu-slat — Claude Code session hosting via fork+exec

| field      | value                                       |
| ---------- | ------------------------------------------- |
| spec_id    | mu-slat (design doc, not a wire spec)       |
| status     | draft                                       |
| created    | 2026-05-25                                  |
| authors    | tcovert + claude-personal (claude-opus-4.7) |
| bead       | mu-slat                                     |
| depends_on | mu-037 (mailbox, Phase 1 — implemented)     |

## Motivation

Anthropic's 2026-06-15 Agent SDK billing change (memory `1c0cd76d`)
splits subscription quota by **entrypoint classification**:

| Entrypoint      | Classification | Billing path                |
| --------------- | -------------- | --------------------------- |
| `cli`           | Interactive    | Subscription pool (unchanged, uncapped) |
| `sdk-cli`       | Agent SDK      | Credit pool ($100 Max5 / $200 Max20)    |
| `sdk-ts`        | Agent SDK      | Credit pool                 |
| `mcp`           | Agent SDK      | Credit pool                 |

The `cli` entrypoint is the **default** — it's what gets written when
none of the four explicit `CLAUDE_CODE_ENTRYPOINT` assignments in the
binary fire. The `-p` flag is the **sole** `sdk-cli` trigger (memory
`1cab7615`). TTY presence/absence is irrelevant.

**Consequence:** mu's existing orchestration path (`agent-spawn-v2` →
`claude -p`) moves to the credit pool post-6/15. For tcovert's usage
shape (orchestration + philosophical sessions), the $100/mo credit
pool exhausts in roughly a week.

**Solution:** mu becomes the unified UI surface; Claude Code subprocess
sessions run as `cli` entrypoint (subscription-billed). The user's
subscription stays load-bearing. mu's event-log observability extends
to Claude subprocess sessions too.

### Secondary motivation: credential security

mu doesn't handle or pass Anthropic credentials directly. The spawned
Claude binary handles auth itself via its own keychain/OAuth. Reduces
credential blast radius if mu is misconfigured. Same pattern as VS
Code and JetBrains integrations (memory `54f977fe`).

---

## Q1: Fork+exec shape to spawn claude-code

### The entrypoint constraint

The spawn shape is dictated by the billing classification:

| Invocation | Entrypoint | Billing | Programmatic? |
| ---------- | ---------- | ------- | ------------- |
| `claude -p "prompt"` | `sdk-cli` | Credit pool | Yes — structured JSON on stdout |
| `claude` (no `-p`, on a TTY) | `cli` | Subscription | No — interactive TUI on terminal |
| `claude` (no `-p`, piped stdin, no TTY) | `cli` | Subscription | Partially — accepts input but renders for terminal |

To stay on subscription: **do not use `-p`.**

### Option A: pty-bridged interactive (RECOMMENDED for in-mu-UI)

mu spawns `claude` as a child process under a **pty** (pseudo-terminal).
The pty makes claude believe it's running interactively → entrypoint =
`cli` → subscription billing.

```
mu daemon
  └─ pty master ←→ pty slave
       └─ claude (interactive TUI, cli entrypoint)
```

**Process spawn:**
```rust
// Conceptual — actual pty crate TBD (portable-pty, nix::pty, etc.)
let pty_pair = openpty()?;
let mut cmd = Command::new(claude_binary_path());
cmd.stdin(pty_pair.slave.try_clone()?)
   .stdout(pty_pair.slave.try_clone()?)
   .stderr(pty_pair.slave.try_clone()?)
   .env("TERM", "xterm-256color")
   .env("COLUMNS", "120")
   .env("ROWS", "40")
   .current_dir(&working_dir);
// DO NOT set -p. DO NOT set CLAUDE_CODE_ENTRYPOINT.
let child = cmd.spawn()?;
```

**Communication:**
- mu writes user messages to the pty master fd as keystrokes
- claude's TUI output (ANSI sequences, text, tool-call renders)
  appears on the pty master fd
- mu needs an **ANSI parser** (e.g., `vte` crate) to extract
  structured content from the terminal output stream

**Pros:**
- Subscription billing (the whole point)
- Claude handles its own auth, hooks, CLAUDE.md, plugins — full
  feature surface
- Same integration model as Zed's terminal-hosted Claude

**Cons:**
- **Fragile.** Claude's TUI is not a stable API. Rendering changes
  across versions break the parser. Anthropic can ship a UI change
  any week.
- **Parsing cost.** Extracting tool calls, assistant text, and
  structured events from ANSI output is substantially harder than
  reading JSON
- **pty on FreeBSD.** Works (`posix_openpt(3)` is standard) but
  library support varies. `portable-pty` targets Linux/macOS
  primarily; may need custom FreeBSD glue

**Verdict:** Viable for "display claude's TUI in a mu pane" (the
mu-solo split-pane model). Not viable for programmatic orchestration
where mu needs to parse tool calls and results reliably.

### Option B: `--print` + stream-json (for headless/pot work)

```sh
claude --print \
  --output-format stream-json --verbose \
  --input-format stream-json \
  --dangerously-skip-permissions \
  --model claude-opus-4-7 \
  "prompt text"
```

**Wire format (stdout, one JSON object per line):**
```jsonc
{"type":"assistant","message":{"role":"assistant","content":[...],"usage":{...}}}
{"type":"system","subtype":"tool_use_start","tool_name":"Read","tool_id":"..."}
{"type":"result","subtype":"success","result":"...","duration_ms":42}
// ... etc
```

**Pros:**
- Structured JSON — trivially parseable
- Full programmatic control (tool results, usage, errors)
- Already validated in agent-spawn-v2 (memory `b7532871`)
- `--input-format stream-json` enables bidirectional structured
  communication

**Cons:**
- Entrypoint = `sdk-cli` → credit pool post-6/15
- Acceptable for bounded headless tasks where credit-pool spend is
  intentional and budgeted

**Verdict:** The right choice for pot-based orchestration, autonomous
goal-protocol workers, and any case where mu drives claude
programmatically and the credit-pool cost is acceptable.

### Option C: hybrid (RECOMMENDED overall)

Use **both** shapes, for different purposes:

| Use case | Shape | Billing |
| -------- | ----- | ------- |
| Interactive "philosophical" sessions in mu's UI | Option A (pty) | Subscription |
| Headless workers (goal-protocol, pot dispatch) | Option B (`-p`) | Credit pool |
| mu-internal tool execution (e.g., "ask claude to summarize this") | Option B with `--max-budget-usd` | Credit pool, bounded |

The session registry gains a new variant:

```rust
enum SessionKind {
    MuNative {
        provider: Arc<dyn Provider>,
        agent_loop: JoinHandle<()>,
    },
    ClaudeSubprocess {
        child: tokio::process::Child,
        pty: Option<PtyMaster>,     // Some for Option A shape
        stdin: Option<ChildStdin>,  // Some for Option B shape
        stdout_reader: JoinHandle<()>,
        mode: ClaudeMode,           // Interactive | Headless
    },
}
```

### Option D: future — `--ide` / ACP / headless-interactive flag

If Anthropic ships a structured protocol for IDE integrations that
produces `entrypoint=cli`, mu could use it for programmatic +
subscription-billed orchestration. The `--ide` flag exists in 2.1.146
but currently means "connect to an existing IDE" — not "speak a
structured protocol on stdio."

The Zed Agent Context Protocol (ACP) is the most likely candidate.
If/when claude-code exposes ACP as a stdio transport with `cli`
entrypoint classification, mu should adopt it immediately — it would
collapse Options A and B into a single clean path.

**Not available today.** Design should not depend on it. But the
adapter layer (§6) should be structured so swapping the wire protocol
is a contained change.

### Binary discovery

```rust
fn claude_binary_path() -> PathBuf {
    // 1. MU_CLAUDE_BINARY env var (explicit override)
    // 2. ~/.local/bin/claude (standard symlink chain per
    //    reference_claude_binary memory)
    // 3. `which claude` fallback
}
```

---

## Q2: Auth inheritance

**mu does not manage Claude's credentials.** Three cases:

### Host-spawned (in-process)

Claude subprocess inherits the user's filesystem. It reads its own
keychain (`~/.claude/`, OAuth tokens, etc.) exactly as it would in a
standalone terminal. mu sees nothing.

**Requirement:** the user must have authenticated claude interactively
at least once (`claude auth login` or first-run OAuth flow) before mu
can spawn subprocess sessions.

### Pot-spawned (agent-spawn-v2)

Already validated (memory `b7532871`). The flow:

1. Host reads headless OAuth token from
   `~/.config/claude-code/headless-oauth-token.disabled`
2. Base64-encodes it
3. Passes via `CLAUDE_CODE_OAUTH_TOKEN` env into the pot
4. **Unsets** `ANTHROPIC_API_KEY` and `ANTHROPIC_BASE_URL` to prevent
   silent-billing-trap (memory `ac0cc306`)
5. Claude inside the pot uses the token for Max5 OAuth billing
6. Per-pot ephemeral HOME (`/root`) — no host `~/.claude` mount
   (POSIX flock deadlock on nullfs)

**mu's role:** pass the token env var when spawning via agent-spawn-v2.
mu doesn't decode, validate, or store the token.

### Cross-machine (Phase 2+, not in scope)

Would require token forwarding or a federated auth model. Deferred to
mu-037 Phase 2 (cross-daemon over unix sockets) + §6 Auth futures in
the claude-code design influences doc.

---

## Q3: Work delivery (how the subprocess receives tasks)

### Mechanism 1: spawn-time prompt (primary, both modes)

The simplest path: the prompt is an argument or stdin at spawn time.

**Headless (`-p`):**
```sh
claude -p --dangerously-skip-permissions "prompt text"
```

**Interactive (pty):**
After spawn, mu writes the prompt as keystrokes to the pty master fd,
followed by Enter. Claude receives it as user input.

### Mechanism 2: `--input-format stream-json` (headless only)

For multi-turn headless sessions, mu can stream user messages to
claude's stdin:

```jsonc
{"type":"user_message","content":"first task"}
// ... claude works, emits stream-json on stdout ...
{"type":"user_message","content":"follow-up task"}
```

This enables mu to drive a headless claude session through multiple
turns without re-spawning. The session retains context across turns.

### Mechanism 3: mailbox delivery (coordination layer)

mu-037's mailbox surface is already implemented. For multi-session
coordination:

1. mu spawns claude subprocess (either mode)
2. mu creates a mu session as a **companion** alongside the subprocess
3. The companion session posts `mailbox.post` messages to coordinate
4. The subprocess reads coordination via MCP tools or file artifacts

**Important:** the subprocess claude session has its own context
window, tools, and capabilities. It does NOT share mu's internal
session state. The mailbox is the coordination channel.

### Mechanism 4: CLAUDE.md / system prompt injection

For persistent context that should be available from turn 1:

- `--system-prompt` or `--append-system-prompt` flags
- `--add-dir` to make additional directories (and their CLAUDE.md
  files) visible
- `--mcp-config` to attach MCP servers (including mu's own mailbox
  MCP, if built)

### Recommended approach

**Phase 1:** spawn-time prompt (mechanism 1) for single-task dispatch.
The caller specifies the full task in the prompt.

**Phase 2:** stream-json bidirectional (mechanism 2) for multi-turn
orchestration from mu's daemon.

**Phase 3:** mailbox coordination (mechanism 3) for peer-like
multi-session patterns where the subprocess and a mu-native session
cooperate.

---

## Q4: Result reporting

### From headless (`-p`) sessions

**Primary: stdout stream-json.** The subprocess emits structured JSONL
events on stdout. mu reads these via a drainer task (same pattern as
`MuClient`'s stdout drainer in `mu_client.rs`):

```rust
async fn drain_claude_stdout(
    reader: BufReader<ChildStdout>,
    event_log: Arc<SessionEventLog>,
    notif: NotificationWriter,
) {
    // Parse each line as a stream-json event
    // Map to mu EventPayload variants
    // Append to event log + emit wire notifications
}
```

**Event mapping (claude stream-json → mu EventPayload):**

| Claude event type | mu EventPayload |
| ----------------- | --------------- |
| `assistant` (with content) | `AssistantMessageEvent` |
| `tool_use_start` | `ToolCall` |
| `result` (tool result) | `ToolResult` |
| `system` (subtype `turn_duration`) | `Done` with usage |
| `error` | `Error` |

**Secondary: exit code + final output.** For simple dispatch, just
read the final text output after the process exits. This is what
agent-spawn-v2 does today.

### From interactive (pty) sessions

The pty output stream contains ANSI-rendered terminal output. Two
approaches:

**Display-only (Phase 1):** render the raw terminal output in mu's UI
as a terminal pane. No parsing. The user sees exactly what they'd see
in a standalone terminal. mu records the raw byte stream in the event
log as opaque blobs.

**Parsed (Phase 2+):** apply an ANSI/VTE parser to extract structured
content. Map assistant text, tool calls, and results to mu's event
types. This is substantially harder and may not be worth the
investment if Option D (ACP) materializes.

### Cross-session result sharing

For orchestration patterns where a parent mu session needs results
from a claude subprocess:

1. **File artifacts:** subprocess writes to disk; parent reads. Works
   for code generation, spec drafting.
2. **Mailbox:** subprocess's companion mu session posts results back
   via `mailbox.post`. Requires wiring the subprocess to a mu MCP or
   using `--add-dir` to give it access to a shared coordination
   directory.
3. **Event log query:** parent calls `session.events` on the
   subprocess's mu session to read its event log. Works for
   observability; not ideal for real-time coordination.

---

## Q5: Pot integration via agent-spawn-v2

### Current state (validated, memory `b7532871`)

```sh
AGENT_SPAWN_RUNTIME=claude agent-spawn-v2 "prompt"
```

Works end-to-end. The flow:

1. `agent-spawn-v2` acquires an etcd-leased slot
2. Optionally builds a context manifest (memory injection)
3. Calls `agent-spawn` with the slot
4. `agent-spawn` (claude branch):
   - Reads headless OAuth token
   - Base64-encodes, passes via env
   - `jexec -l <pot> env -u ANTHROPIC_API_KEY ... claude -p --dangerously-skip-permissions "$PROMPT"`
5. Subprocess runs, produces text output
6. Slot released on exit

### What mu-slat changes

**Nothing in the pot path for headless work.** The existing
`agent-spawn-v2 RUNTIME=claude` path continues to work for credit-
pool-billed headless tasks. This is Phase 1 of mu-slat: mu's daemon
gains the ability to call `agent-spawn-v2` programmatically and track
the resulting session in its session registry.

```rust
// New RPC: session.spawn_claude
struct SpawnClaudeRequest {
    prompt: String,
    mode: ClaudeMode,  // Headless | Interactive
    working_dir: PathBuf,
    model: Option<String>,
    max_budget_usd: Option<f64>,
    timeout_secs: Option<u64>,
    // For pot dispatch:
    use_pot: bool,
    context_query: Option<String>,
}
```

**For interactive/pty sessions in mu's UI:** these run on the host
(not in a pot), since the user needs to see the TUI and the session
needs the user's keychain. No pot involvement.

### Model override gap

The current `agent-spawn` claude branch doesn't pass `--model` to
claude. It defaults to Sonnet 4.6. For substantive work needing Opus
4.7, the spawn script needs a `--model` pass-through. Filed as a gap
in memory `b7532871`.

### Timeout handling

`AGENT_SPAWN_TIMEOUT` defaults to 180s — insufficient for substantive
coding work. mu should set this based on the task shape (see
goal-protocol's budget table in SKILL.md).

---

## Q6: Composition with existing wire surface

### session.delegate (mu-031) — parent/child sessions

`session.delegate` creates a child mu-native session (provider +
agent loop + event log). A claude subprocess session is **not** a
delegate — it's a new session kind with a different lifecycle:

| Property | Delegate (mu-native) | Claude subprocess |
| -------- | -------------------- | ----------------- |
| Provider | mu's `Arc<dyn Provider>` | Claude binary's own |
| Agent loop | mu's `AgentLoop::spawn()` | Claude's internal loop |
| Tool execution | mu's tool registry | Claude's built-in tools |
| Capability | Attenuated from parent | Not governed by mu-033 |
| Event log | mu `SessionEventLog` | mu records mapped events |
| Auth | mu's provider factory | Claude's own keychain |

The two coexist: a mu daemon can host both mu-native sessions and
claude subprocess sessions simultaneously. The session registry
distinguishes them.

### mailbox.* (mu-037) — async coordination

Already implemented (Phase 1). Two composition patterns:

**Pattern A: mu session coordinates claude subprocess.**
A mu-native session (e.g., an orchestrator running on openai-codex)
spawns a claude subprocess for a specific task. The orchestrator posts
coordination messages to a shared mailbox; the subprocess reads them
via an MCP tool or file.

**Pattern B: claude subprocess posts results to mu session.**
The subprocess writes results to a file or posts via an MCP tool.
The mu session reads the results via `mailbox.list` or file I/O.

**Pattern C: peer handshake.**
The mu daemon performs `peer.hello` on behalf of the subprocess,
issuing it a peer handle scoped to `mailbox.post`. The subprocess
(via MCP or direct wire) can then post to other sessions' mailboxes.
This requires bridging the subprocess to mu's RPC surface.

### peer.hello (mu-037) — session discovery

Peer handles are issued per-session. A claude subprocess can
participate as a peer if:

1. mu registers it in the `Sessions` registry (as a
   `ClaudeSubprocess` variant)
2. mu handles `peer.hello` requests on behalf of the subprocess
3. The subprocess communicates with mu (its host) via a sidecar
   channel (MCP, file, or the pty)

### session.start_autonomous (mu-036) — autonomous loops

Not directly composable. Claude's own `/goal` mechanism is the
autonomous loop for subprocess sessions. mu's
`session.start_autonomous` applies to mu-native sessions only.

For subprocess sessions, mu's role is:
- Set a timeout (`AGENT_SPAWN_TIMEOUT` or shell `timeout`)
- Set a budget cap (`--max-budget-usd`, headless only)
- Monitor the process (exit code, output, event stream)
- Record telemetry in the event log

### session.list — unified view

The session list handler already supports both live and rehydrated
sessions. Adding subprocess sessions:

```rust
// In SessionListEntry:
struct SessionListEntry {
    session_id: String,
    kind: SessionKind,  // "mu_native" | "claude_subprocess"
    // ... existing fields
    subprocess_pid: Option<u32>,
    subprocess_mode: Option<String>,  // "interactive" | "headless"
}
```

The TUI's F2 session list shows all session kinds with a visual
marker distinguishing mu-native from claude-subprocess.

---

## Phased implementation

### Phase 0: design validation (this document)

- [x] Map the design space
- [ ] User review of billing strategy (subscription vs credit pool
  tradeoffs)
- [ ] Confirm Zed ACP timeline (is a structured `cli`-entrypoint
  protocol coming?)

### Phase 1: headless subprocess sessions

**Scope:** mu daemon can spawn `claude -p` as a tracked session.
Event stream parsed and recorded in mu's event log. Session appears
in `session.list`. Output visible via `session.events`.

**Wire surface:** `session.spawn_claude` RPC (or extend
`create_session` with a `ClaudeSubprocess` provider selector variant).

**Billing:** credit pool (acceptable for bounded tasks with
`--max-budget-usd`).

**Files touched:**
- `crates/mu-core/src/protocol/session.rs` — new request/response types
- `crates/mu-coding/src/serve/handlers/session.rs` — handler
- `crates/mu-coding/src/serve/sessions.rs` — `ClaudeSubprocess` in registry
- `crates/mu-coding/src/claude_subprocess.rs` — new: spawn + stdout drain + event mapping

### Phase 2: interactive pty sessions

**Scope:** mu-solo (or mu-tui) can host a claude TUI in a pane.
Raw terminal output displayed; no structured parsing.

**Billing:** subscription pool.

**Additional crates/deps:**
- pty crate (`portable-pty` or `nix::pty`)
- ANSI terminal renderer in mu's UI (already partly exists via
  `ansi-to-tui`)

### Phase 3: structured bidirectional (stream-json)

**Scope:** mu drives multi-turn headless sessions via
`--input-format stream-json` / `--output-format stream-json`.
Enables orchestration patterns (orchestrator mu session dispatches
work to claude subprocess, reads results, dispatches more).

**Billing:** credit pool.

### Phase 4: mailbox bridge

**Scope:** claude subprocess sessions can receive and send mailbox
messages. Requires either:
- An MCP server that bridges mu's mailbox to claude's MCP surface, or
- A file-based coordination protocol in a shared directory

### Phase 5: ACP / structured interactive (contingent)

**Scope:** if Anthropic ships a structured protocol for IDE
integrations with `cli` entrypoint classification, mu adopts it.
This collapses Phases 1-3 into a single clean path:
programmatic + subscription-billed.

---

## Open questions

1. **Anthropic's entrypoint classification stability.** The `cli` vs
   `sdk-cli` distinction is inferred from binary analysis (memory
   `1cab7615`), not from public documentation. Anthropic could change
   the classification criteria. The pty-based approach is our best
   current understanding of what stays on subscription.

2. **pty library on FreeBSD.** `portable-pty` (used by wezterm) has
   FreeBSD support but it's not the primary target. Needs a spike to
   confirm it works in mu's async context.

3. **Zed ACP timeline.** If Anthropic publishes ACP as a stable
   stdio protocol with `cli` entrypoint, it supersedes the pty
   approach. Worth monitoring the claude-code changelog.

4. **Credit pool exhaustion strategy.** Post-6/15, how much
   headless orchestration can fit in $100/mo (Max5) or $200/mo
   (Max20)? tcovert noted (memory `54f977fe`) he doesn't have a
   clear handle on current `-p` costs at API rates under
   subscription. The first few weeks post-rollout are a calibration
   period.

5. **Session cleanup.** When a claude subprocess crashes or hangs,
   mu needs to detect and clean up (kill child, release pot slot,
   update session registry). The existing `MuClient::close()` pattern
   (drop stdin → wait → SIGKILL) applies.

6. **MCP server for mailbox bridge.** Phase 4 requires either a
   standalone MCP server exposing mu's mailbox surface, or a creative
   use of `--mcp-config` to inject mu-aware tools into the subprocess.
   Design TBD.

---

## Cross-references

| Reference | What it provides |
| --------- | ---------------- |
| mu-037 spec (mailbox, implemented) | Wire surface for async coordination |
| Memory `54f977fe` (subprocess strategy) | The strategic rationale for binary-spawn-with-stdio |
| Memory `1cab7615` (entrypoint billing) | Empirical finding: `-p` = sdk-cli, no `-p` = cli |
| Memory `b7532871` (claude in pot) | Validated pot recipe for headless claude |
| Memory `1c0cd76d` (6/15 policy) | Credit pool caps by tier |
| Memory `53db9549` (Agent SDK patterns) | Subagent JSONL transcripts, hook events |
| `specs/architecture/claude-code-design-influences.md` | §1 hooks, §7 billing topology |
| `agent-spawn-v2` (warden/scripts) | Pot-based spawn with slot leasing |
| `crates/mu-coding/src/serve/handlers/mailbox.rs` | Implemented mailbox handlers |
| `crates/mu-coding/src/serve/handlers/session.rs` | Session creation + delegate patterns |
| `crates/mu-coding/src/serve/factory.rs` | Provider factory (new variant needed) |
| `crates/mu-tui/src/mu_client.rs` | Existing subprocess spawn pattern for `mu serve` |
