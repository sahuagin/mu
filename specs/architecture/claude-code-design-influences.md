# Claude Code → mu: protocol, runtime, and operational shape

**Date**: 2026-05-21
**Audience**: future mu sessions; other claude instances picking up the thread
**Status**: synthesis notes from a working session, not a formal spec
**Companion**: `claude-code-feature-mapping.md` (the tiered feature inventory). This document is complementary, not duplicative. Where the feature mapping walks the Claude Code docs feature-by-feature and proposes beads, this document records design-shape conclusions reached by working through specific subsystems in conversation. Read both.

---

## Why this doc exists

A working session with a senior systems engineer walked through Claude Code's runtime in enough operational depth to surface design conclusions that aren't visible in the docs themselves — partly because the docs describe each feature in isolation, partly because some of the most useful insights only appear when you trace how the features compose under load and how their economics map to a Max5/Max20 plan vs. an API account vs. a third-party client.

This document captures those conclusions. It is opinionated, sometimes terse, and assumes a reader who has at least skimmed the feature-mapping document. Where this document and the feature-mapping document disagree, this one is the later working draft and probably has the better synthesis.

## Reading map

1. **§1 Hooks and the subprocess+JSON contract** — small section; complements feature-mapping §7
2. **§2 Deferred tools and tool-catalog economics** — the lazy-import-for-AI-tools pattern, why it matters, how it composes with prompt caching
3. **§3 Push primitives and the inversion** — Monitor, PushNotification, RemoteTrigger, remoteControlAtStartup, agentPushNotifEnabled. The shape: CC is bidirectional in a way most CLIs aren't
4. **§4 MCP protocol** — JSON-RPC envelope, capability handshake, three surfaces (Tools/Resources/Prompts), resource subscriptions as protocol-native push, transport choices
5. **§5 Mailbox architecture** — Maildir + etcd watches + filter design + drop semantics, with reference to mu-037
6. **§6 Auth futures** — OAuth 2.1 PKCE for human-loop, client_credentials for agent-loop, mTLS for intra-trust-zone, self-host vs. SaaS, YubiKey across protocols
7. **§7 Billing topology** — MCP is not a billing axis; client → model is; April 2026 third-party-client surcharge
8. **§8 Anti-patterns and CC-specific shapes mu should not copy** — the sandbox/namespace assumption, remote-control-bus, SOC2-shaped audit posture, embedded LLM in tool servers
9. **§9 Cross-references to existing mu specs** — what touches what

---

# 1. Hooks and the subprocess+JSON contract

The feature-mapping document covers the eight named events. Two design conclusions about the *shape* of the hook system are worth marking separately because they bear on choices mu has to make and hasn't fully made.

## 1.1 Subprocess + JSON-on-stdin/stdout is a load-bearing choice, not an accident

Claude Code's hooks are not an in-process plugin API. The harness spawns a child process per event, hands it a JSON blob on stdin, reads JSON or text from stdout, and treats the exit code as policy signal. This has three consequences mu should internalize:

1. **Hooks can be written in any language.** A user can write a hook in shell, Python, Rust, Go, Awk, sed — anything that reads stdin and writes stdout. Nothing in the harness has to know what language; nothing in the harness has to be rebuilt to swap a hook implementation. The barrier to wiring up a new hook is whatever the user's PATH already contains.

2. **A misbehaving hook cannot bring down the harness.** Subprocess isolation gives you an exception boundary for free. The hook can panic, segfault, run out of memory, hang — the worst that happens is the harness times it out and moves on. An in-process API would not have this property; one bad plugin would kill the whole runtime.

3. **Per-event spawn cost is real but small.** A subprocess spawn on Linux/FreeBSD is on the order of 10–30 ms depending on the binary's startup cost. Multiple hooks per event multiplied by multiple events per turn gives total per-turn hook overhead in the 50–250 ms range. This is invisible against a multi-second model call. **It would not be invisible against a sub-second tool call**, so hooks should not be wired into hot paths where they'd dominate. mu should respect this: per-tool-call hooks are fine; per-token hooks are not.

mu currently uses a similar shape (hooks defined per-event, executed out-of-process). The lesson here is to keep it. Don't be tempted by "an in-process Lua/Wasm plugin API would be faster." It would be faster *and* fragile. The cost is paid in the right place.

## 1.2 The `additionalContext` injection convention is the protocol mu should mirror

CC's hook output protocol distinguishes:

- **Plain stdout on exit-0** → debug log only; not seen by the model
- **`{"hookSpecificOutput": {"additionalContext": "..."}}` JSON on stdout** → text is inserted into the model's context at the appropriate location

This separation matters. Plain stdout is for the *operator* (visible in CLI logs, useful for debugging). The JSON channel is for the *model* (visible in context, costs tokens, requires careful authoring). Conflating them — i.e., "anything the hook prints goes into the model's view" — would either flood the context with debug spam or force operators to choose between debugging and silence.

mu should adopt the same distinction explicitly. A hook's `stdout` should be operator-only by default. Model-visible injection should require an explicit JSON envelope. The exact field name doesn't matter; the discipline does. Suggested convention for mu:

```json
{
  "mu": {
    "additionalContext": "...",
    "additionalContextRole": "system" | "user_visible_only" | "model_visible_only",
    "additionalContextLifetime": "this_turn" | "session" | "until_compact"
  }
}
```

The lifetime field is mu-specific and addresses a problem CC hasn't solved cleanly: hook-injected context that should fade vs. context that should persist. If the hook injects "tomorrow is a deadline" once, that probably should fade after one turn. If the hook injects "this codebase uses Diesel, not SQLx," that should persist as long as the session does.

## 1.3 Hooks that bookend a turn

A specific pattern worth naming, because it generalized in our discussion and isn't in the feature-mapping document: **paired UserPromptSubmit / Stop hooks**.

CC has scripts (`claude-mid-turn-tracker`, `verify-claim`, `stop-slop`) that run on *both* prompt-submit and stop. The shape:

```
UserPromptSubmit → record state at turn-start
   ⋮
   (turn happens)
   ⋮
Stop → compare against state-at-turn-start; fire if divergent
```

This is the canonical pattern for "did the model do what it said it would do" detection. The verify-claim and stop-slop hooks use this shape. mu can adopt the same primitive: hooks should be able to declare a "pair" relationship (run at both ends of a turn, share state via filesystem or environment).

A specific implementation note: in CC, paired hooks share state by writing to a known scratch file (often under `~/.cache/claude/`). mu could formalize this with a `hook_state` directory passed to the hook via env var, so paired hooks don't have to invent their own coordination convention.

---

# 2. Deferred tools and tool-catalog economics

This is the section the feature-mapping document barely touches. It matters disproportionately because every additional MCP server connected to mu trades against context-window cost, and the design of the deferred-tools mechanism is the difference between "carrying 5 MCPs is cheap" and "carrying 5 MCPs is unaffordable."

## 2.1 The problem

Tool definitions live in the model's system prompt. Each definition is a JSON Schema fragment — name, description, input schema. For tools with rich input shapes (think: `mcp__orchestrator__run_task` with 8 nested fields each with their own descriptions), the schema is 1–2 KB of tokens.

A working mu environment might carry:

- ~10 built-in tools (Read, Edit, Write, Bash equivalent, etc.)
- ~30 task/agent tools (TaskCreate family, Monitor, ScheduleWakeup, etc.)
- ~20 MCP tools across 3–5 active servers
- ~10 more from any specialty MCP (mailbox, future rabbitmq, future memory facade)

Eagerly loaded, that's 15,000–30,000 tokens of tool catalog **in every system prompt**. Even at prompt-cache-hit cost (~10% of base), you're paying for it on every turn. For a long session that's significant.

## 2.2 The mechanism

Claude Code splits tools into two tiers:

1. **Eagerly loaded** — schemas in the system prompt from turn 1; callable directly. Minimal set: the tools the harness expects to need on every conversation (Read, Edit, Write, Bash, Skill, ToolSearch, etc.).

2. **Deferred** — only the **names** appear in a system-reminder block. The schemas are *not* in context. To call a deferred tool, the model invokes `ToolSearch` first; the matched tool's schema arrives as a tool result, lands in conversation history (not system prompt), and the tool becomes callable for the rest of the session.

This is structurally equivalent to **lazy-import for AI tool catalogs**. The same insight that drove Python's `importlib.util.LazyLoader`, JavaScript's dynamic `import()`, and JVM class-on-first-reference loading, applied to LLM context engineering: catalog metadata is cheap, full schemas are expensive, fetch on demand.

## 2.3 Why this composes with prompt caching specifically

The clever part is *where* the schema lands when fetched. When `ToolSearch` returns a schema, it goes into **conversation history**, not the system prompt. Two consequences:

- **Loading a deferred tool mid-session does NOT invalidate the system prompt cache.** You only pay the schema's tokens once (as a tool result), and from then on it rides cheap inside the cached conversation prefix.
- **Different sessions can have wildly different active tool catalogs without affecting each other's caches.** A session that never loads `mcp__snake__step` literally never pays for snake's schema, even though the snake MCP server is registered globally.

If schemas were retroactively inserted into the system prompt every time a new tool was loaded, the cache invalidation would defeat half the savings. By putting them in conversation history (cached at coarser boundaries, re-cached cheaply), the deferred-tools mechanism *cooperates with* prompt caching instead of fighting it. The CC harness team thought this through and the choice is load-bearing.

## 2.4 The search interface

`ToolSearch` takes three query shapes:

- `select:Name1,Name2` — exact-name fetch (the "I know what I want" path)
- `notebook jupyter` — keyword search ranked by relevance
- `+slack send` — require `slack` in name; rank remaining terms

The keyword path is what makes the system discoverable. The model doesn't need to memorize every tool name; it can describe what it wants and get back the closest matches. This works because tool *descriptions* (the text in the schema's `description` field) are themselves indexable, and the matcher does relevance ranking over them.

## 2.5 What mu should adopt

1. **A two-tier tool catalog.** Eagerly loaded set should be the minimum needed for any reasonable mu session: file ops, shell, spec-author/spec-read, mailbox basics, capability-check primitives. Everything else should be deferred.

2. **A `mu_tool_search` (or whatever name) primitive** that takes the same three query shapes. The implementation is a small text search over registered tool descriptions; SQLite FTS5 is the obvious backend (mu's agent.sqlite already uses it for memory).

3. **Schemas-in-conversation-history, not in system prompt.** When a tool is fetched, its schema should land as a tool result with the special role "tool catalog entry," and the model should be told once that future invocations of that tool are valid.

4. **`listChanged` notifications** (borrowed from MCP — see §4): when an MCP server's tool set changes mid-session, the harness should notify the model so the deferred-tools list updates. CC has this; mu should too.

5. **A "tool catalog cost" telemetry signal** — operators should be able to see how much of a session's context is spent on tool definitions. This guides whether to defer more aggressively.

## 2.6 The architectural pressure deferred-tools creates is healthy

A subtle design effect worth naming: deferred-tools lets MCP server authors be **generous with capabilities** — expose 15 specialized tools instead of 1 swiss-army one — without imposing a context-window tax on agents that don't use them. That's a meaningful design freedom. The `pi_prompt`/`memory_recall`/`spawn_agent`/`list_tasks`/`watch_task`/etc. proliferation in orchestrator+warden is **exactly** what deferred-tools makes affordable.

mu's spec authors should know this: when designing an MCP server's tool surface, prefer many specific tools over one general tool with a switch parameter. Specific tools are more discoverable (each has its own description), more cacheable (the model can decide to load just the few it needs), and easier to deprecate (you can mark one tool removed without breaking other call paths).

---

# 3. Push primitives and the inversion

## 3.1 The traditional CLI shape

A traditional CLI is unidirectional in attention: the user drives it, types commands, reads output, types more commands. The CLI never reaches out to the user. If the user walks away, the CLI sits idle. If a long task finishes, the user has to come back and check.

Claude Code breaks this. With the right settings (`remoteControlAtStartup: true`, `agentPushNotifEnabled: true`) and the right primitives (`Monitor`, `PushNotification`, `RemoteTrigger`), the relationship becomes **bidirectional**. The user can be pinged on their phone when something interesting happens; an external script can fire a prompt into a running session; an idle session can be woken by a scheduled trigger.

This is qualitatively different from "a CLI with notifications bolted on." It's closer to **ambient compute** — a process that runs continuously, reaches the user when warranted, and accepts inputs from outside its terminal.

mu has the architectural ingredients to do the same thing. The question is which primitives to expose and how to compose them.

## 3.2 The three primitives in CC, mapped to mu

| CC primitive | What it does | mu equivalent | Notes |
|---|---|---|---|
| `Monitor` w/ `persistent: true` | Runs a background script; every stdout line becomes a chat-visible event | `mu-037 mailbox` + a watcher | The event stream IS the mailbox in mu's design |
| `PushNotification` | Desktop notification + phone push if remote-control connected | `mu-029 input_required` + escalation transport | Already in mu's design space |
| `RemoteTrigger` | Schedules a prompt to fire into a remote session at a future time | `mu-037 peer.hello` from an external scheduler | Composes cleanly with capability handles |
| `Bash` w/ `run_in_background` | Single-shot: command runs, exits, fires one completion event | `mu task spawn --detached` or similar | The one-shot version of Monitor |

## 3.3 Monitor's design is the most generalizable

The Monitor tool's "every stdout line is an event" pattern is structurally important. Three properties make it powerful:

1. **Unix-philosophy composability.** Any existing tool that emits events on stdout (`tail -f`, `inotifywait`, `etcdctl watch`, `socat`, custom Python) becomes a Claude-visible event source with zero glue code. The interface is the same interface every Unix tool already uses.

2. **Filter discipline at the source.** The watcher script is responsible for emitting only events worth surfacing. The model doesn't see raw logs; it sees what the watcher decided to emit. This is the right place for the filter — close to the data, in a language the operator can iterate in.

3. **"Silence is not success."** From Monitor's own description, this is the genuinely important engineering doctrine: a monitor that only matches the happy path is structurally identical to no monitor when things go sideways. Every filter must include the failure signatures, not just success markers. If you can't enumerate failure modes, broaden the grep alternation rather than narrow it. This rule generalizes far beyond Monitor.

mu should adopt the same model:
- The mailbox watcher emits structured events (one JSON object per line, or one preview-line per arrival)
- The watcher decides what's interesting; the agent decides what to do with it
- Watchers must be paranoid about coverage — every failure mode an operator would want to act on must be matched

## 3.4 The cost of the inversion

Every push event has a token cost. The mailbox-watcher pattern that emits a one-line preview per arrival is cheap; a watcher that emits 5 KB per event will starve the context window in a long-running session.

Design rule: the watcher's stdout should carry **only what the agent needs to decide whether to fetch more**. Full message bodies stay in the MCP server; the watcher emits previews. The agent calls `mcp__agent_mail__read_message` if the preview was interesting. This mirrors how `gh pr list` and `gh pr view` work — list is cheap, view is paid.

## 3.5 RemoteTrigger and the "wake from outside" shape

CC's `RemoteTrigger` exposes claude.ai's trigger API. The structurally interesting part is the *bus* it runs over: any session with `remoteControlAtStartup: true` is registered on a bus that the mobile app, the web UI, and the trigger API can all reach. From the user's phone, you can push a prompt into a specific terminal session miles away.

For mu, the equivalent is: peer.hello-able sessions can be discovered by external schedulers, and an authorized scheduler can post into the mailbox as a `task` kind that escalates appropriately. The trust model for this is harder than CC's (which leans on Anthropic's authentication infrastructure); mu would need its own — see §6.

## 3.6 The discipline of when to actually push

CC's `PushNotification` schema includes a remarkably blunt warning in its description: "err toward not sending one." Notification fatigue is real. The cost of an unwanted notification is multiplied across every future notification the user might dismiss without reading.

mu should adopt the same discipline:
- Push only when the user would want to know **now**
- Default to silent; require an explicit signal to escalate
- Bundle multiple events when possible; don't fire N separate pushes if one summary push will do
- Make it easy to silence noisy channels per-context

---

# 4. MCP protocol

## 4.1 The protocol is small

Despite its prominence, the Model Context Protocol is structurally tiny:

- **JSON-RPC 2.0** over a transport (stdio, streamable HTTP, WebSocket)
- A **capability handshake** at session start (`initialize` → `result` → `notifications/initialized`)
- Three **capability surfaces**: Tools, Resources, Prompts
- **Notifications** for one-way server-pushed updates

That's the whole thing. The spec lives at `https://spec.modelcontextprotocol.io/`. Reference SDKs exist in TypeScript, Python, Go, Rust.

## 4.2 The handshake is IMAP CAPABILITY, basically

```jsonc
// client → server
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
  "protocolVersion":"2024-11-05",
  "capabilities":{"tools":{},"resources":{"subscribe":true}},
  "clientInfo":{"name":"mu","version":"..."}}}

// server → client
{"jsonrpc":"2.0","id":1,"result":{
  "protocolVersion":"2024-11-05",
  "capabilities":{"tools":{"listChanged":true},"resources":{"subscribe":true}},
  "serverInfo":{"name":"mu_agent_mail","version":"0.3.1"}}}

// client → server (notification, no id)
{"jsonrpc":"2.0","method":"notifications/initialized"}
```

Both sides advertise what they support; the intersection defines the session. The `listChanged: true` flag means "I may push you `notifications/tools/list_changed` later" — the same shape as IMAP NOTIFY.

mu's RPC layer should adopt this pattern. The current mu spec leans toward "capabilities are everything" (see mu-033), which is correct, but the capability *negotiation* at session start is a small explicit step that's worth doing rather than inferring capability availability from probe-and-fail.

## 4.3 Three surfaces, three different roles

| Surface | What it is | IMAP analog | mu equivalent |
|---|---|---|---|
| **Tools** | Callable functions with JSON Schema input; model emits `tools/call` | Custom commands | Already in mu via tool registry |
| **Resources** | URI-addressable content the client can fetch / subscribe to | Mailbox + messages | mu-037 mailbox URIs + `watch` |
| **Prompts** | Server-supplied parameterized prompt templates the user invokes as slash-commands | Sieve scripts | mu skills via slash-commands |

Most MCP servers only implement Tools. The Resources surface is underused, partly because client support is uneven and partly because the use cases (RAG, contextual file injection, live data feeds) overlap with tools in ways that make it tempting to model everything as a tool.

But Resources has one property tools don't have: **subscribe**. A client can subscribe to a resource URI, and the server pushes `notifications/resources/updated` when the underlying content changes. The client then decides whether to call `resources/read` to fetch the new content.

This is **protocol-native push**. It's what the mailbox subsystem actually wants. The fact that mu is currently planning to do push via Monitor-style watcher scripts is a workaround for the fact that not all MCP clients implement resource subscriptions well. As client support matures, the mu mailbox should expose itself **as a resource** with subscribe semantics, and the Monitor-watcher path should become the fallback for clients that don't support subscriptions.

## 4.4 The tool-call shape

```jsonc
// list (called once at session start; may be re-called when listChanged fires)
{"method":"tools/list"}
→ {"result":{"tools":[
   {"name":"send_mail",
    "description":"Send a message to another agent's mailbox.",
    "inputSchema":{"type":"object","properties":{
      "to":{"type":"string"},
      "subject":{"type":"string"},
      "body":{"type":"string"}},
    "required":["to","subject","body"]}}
   ]}}

// invoke
{"method":"tools/call","params":{
  "name":"send_mail",
  "arguments":{"to":"mu-gpt","subject":"task","body":"..."}}}
→ {"result":{"content":[
   {"type":"text","text":"Message queued. id=01HXYZ"}],
   "isError":false}}
```

The `content` field is an array of typed parts (`text`, `image`, `embedded_resource`). A single tool call can return multiple parts — a result blurb plus an image plus a JSON blob. mu should keep this multi-part shape; it's more flexible than the single-string-result alternative and the cost is negligible.

The `isError: true` convention is for tool-domain errors that don't blow up the protocol. Network-layer or protocol-layer errors use standard JSON-RPC error codes. This is the same distinction SMTP makes between 4xx/5xx (protocol-level) and application-level rejections; it's worth preserving.

## 4.5 Resource subscriptions in detail

```jsonc
// subscribe (or unsubscribe via resources/unsubscribe)
{"method":"resources/subscribe","params":{"uri":"mailbox://mu/inbox/"}}

// server-pushed notification when something changes
{"method":"notifications/resources/updated","params":{
  "uri":"mailbox://mu/inbox/msg-01HXYZ"}}

// client reads on demand
{"method":"resources/read","params":{"uri":"mailbox://mu/inbox/msg-01HXYZ"}}
→ {"result":{"contents":[{"uri":"...","mimeType":"application/json","text":"..."}]}}
```

The subscribe call can target a URI prefix (in the example above, the whole inbox) rather than a specific resource. The server fires `notifications/resources/updated` for any URI within the subscribed prefix.

For mu, this maps onto the mailbox design with no contortion: a mu agent subscribes to `mailbox://self/inbox/`, the mailbox MCP fires updates as messages arrive, and the agent reads the body only when the preview was interesting. The implementation should support **prefix subscriptions** explicitly — `mailbox://self/inbox/from/specific-peer/` should also be subscribable, so agents can filter at the source.

## 4.6 Transports

| Transport | Use | How it works |
|---|---|---|
| **stdio** | Local subprocess (every mu MCP today) | JSON-RPC on stdin/stdout; stderr is logs |
| **Streamable HTTP** (2025) | Remote / hosted | POST + SSE on a single endpoint |
| **HTTP+SSE** (legacy) | Older remote | Two endpoints — POST for requests, SSE for server pushes |
| **WebSocket** | Newer remote | Full duplex; less standardized in MCP than streamable-HTTP |

Critical stdio gotcha: **logs go to stderr, never stdout**. Mixing log output into stdout corrupts the JSON-RPC stream. mu MCP servers must follow this; a single `print()` call to stdout in the wrong place will break the protocol in ways that are extremely confusing to debug. A linter rule for "no bare prints in MCP server code" is worth setting up early.

The streamable-HTTP transport replaced the original SSE-based transport in 2025. It's the path forward for any future jail-hosted mu MCP that wants to be reached from outside its local trust zone. The OAuth 2.1 auth (see §6) lives at this transport layer.

## 4.7 Error handling

JSON-RPC defines standard error codes:

- `-32700` Parse error
- `-32600` Invalid Request
- `-32601` Method not found
- `-32602` Invalid params
- `-32603` Internal error

MCP adds codes in the `-32000` to `-32099` range for server-defined errors. mu MCP servers should:

- Use protocol-level errors (JSON-RPC `error` field) for malformed requests, missing capabilities, auth failures
- Use tool-result `isError: true` for tool-domain failures (file not found, validation failed, etc.)
- Never confuse the two — protocol errors interrupt the session; tool errors don't

---

# 5. Mailbox architecture

This is the most architecturally interesting section because it spans MCP design, OS primitives, etcd integration, and the cost economics of push systems. mu-037 is the spec; this section records design conclusions that should inform its implementation.

## 5.1 Maildir as the no-locking primitive

Bernstein's Maildir scheme (qmail, 1996) is the right base. Each message is its own file (or in mu's variant, its own folder containing message + metadata). Delivery is `rename(2)` from `tmp/` to `new/` — atomic at the inode level. No locking needed anywhere; concurrent readers and writers don't conflict because they're touching different inodes.

Properties this gives you:
- **Atomic delivery.** The message either fully exists or doesn't. No partial-write window.
- **Crash safety.** Process crashes during write leave a junk file in `tmp/`; cleanup is trivial.
- **Idempotency.** Re-delivery of the same message-ID is detectable; agents can be at-least-once with confidence.
- **Concurrent everything.** Multiple writers, multiple readers, no shared mutable state.

Mu should NOT invent a custom storage format for the mailbox. Use Maildir-style (or its near-equivalent: per-message JSON file + per-mailbox SQLite index). The hardest problem in mail-systems design — concurrent durable message delivery — was solved 30 years ago; don't re-solve it.

## 5.2 etcd watches as production futexes

For multi-jail / multi-host coordination, etcd's `watch` API is the right primitive for the push side. It is structurally a *futex with crash recovery and consistency guarantees*: a watcher blocks on a key prefix; the watcher unblocks when something changes; the change is delivered with a sequence number that lets the watcher resume from where it left off after a crash.

Compared to flock-based or inotify-based alternatives:
- **flock** is advisory on every OS (Linux, FreeBSD, macOS) and unreliable on networked filesystems
- **inotify** is Linux-only; FreeBSD analog is kqueue+`kqwait` from ports
- **etcd watch** works the same across jails and hosts; consistent ordering; recoverable

The cost of etcd is operational complexity. But mu already pays it (warden/orchestrator era), so the marginal cost of adding mailbox watches to the existing etcd is low.

Recommended composition for mu-037:
- Mailbox MCP writes new messages to Maildir as files
- Mailbox MCP also writes a notification key to etcd (`/mu/mailboxes/<addr>/last_msg_id`)
- Watcher script does `etcdctl watch --prefix /mu/mailboxes/<addr>/` and pipes preview lines to stdout
- Agent sees the preview via Monitor-style event stream OR via MCP resource subscription
- Agent fetches full message body via `mcp__mailbox__read` if the preview warrants it

## 5.3 Filter-in-server vs. filter-in-watcher

A real design choice: where does the filter run?

| Location | Pros | Cons |
|---|---|---|
| MCP server (filter pushed down to the source) | Cheapest — unfiltered messages never cross the process boundary; filter applies before notification | Filter rules live in MCP server config; harder to iterate on; redeploy required to change |
| Watcher script (filter in shell/jq/Python near the chat) | Easy to iterate; rules visible in the script; user controls policy | Every message crosses the boundary even if dropped; some work is wasted; doesn't scale to high-volume mailboxes |
| Hybrid (coarse in server, fine in script) | Best of both | More moving parts |

Recommended default: **hybrid**. Server applies coarse subscription filters (`subscribe(from: "peer-x", priority_gte: "info")` — basically what messages should ever appear in this subscription stream); script applies fine policy (drop heartbeats, summarize routine status pings, escalate the rest).

This mirrors how PostgreSQL `WHERE` clauses interact with application-side filtering. Push cheap structural filters down to the source. Keep expressive policy near where you can debug it.

Sieve (RFC 5228) is the vocabulary worth borrowing for the server-side filter language. `if header :contains`, `discard`, `fileinto`, `redirect` — these primitive operations are well-understood and battle-tested. You don't need full Sieve compliance, but the primitive operations Sieve identified are the right ones for any agent-mail filter language.

## 5.4 Drop ranges (UID vs sequence)

A real operational need: "drop the last 10 messages" or "drop messages with IDs in this range." Two flavors worth distinguishing because they have different semantics:

- **Positional ranges** (drop the last 10, drop messages 5–15 in the current view). Cheap, mutable, no persistent identity. IMAP calls these "sequence numbers."
- **ID ranges** (drop messages with ULID range X..Y). Stable, idempotent (drop on already-dropped = no-op). IMAP calls these "UIDs."

mu should expose both because the use cases differ:
- Positional for "clean up the noise from the last hour" — operator-initiated, doesn't need to be exact
- ID-based for "I already processed up through msg-01HXYZ, drop everything before it" — agent-initiated, must be idempotent

Implementation: each Maildir message gets a ULID at delivery time (this is the UID). The current view ordering provides positional indices. Drop operations take either form and translate to the underlying ULID set before doing the delete.

## 5.5 The "every push has a token cost" rule

Repeating from §3 because it's load-bearing for mailbox design: every notification the agent sees costs tokens. The mailbox must be **selective about what it pushes**.

Concrete rules:
- Preview lines should be ~100 tokens, not ~1000
- Push only the metadata + a body preview that's enough to decide whether to read more
- Default: don't push, fetch on demand. Reverse the default only when the agent is actively in a "I'm waiting for this" state

This is also why server-side filters matter so much: at high mailbox volume, the cheapest filter is the one that drops messages before they ever reach the agent.

## 5.6 RabbitMQ behind the mailbox MCP, when the time comes

The mailbox MCP starts simple — Maildir + etcd watches. When feature pressure grows beyond what Maildir can handle (durable routing, fanout exchanges, dead-letter queues, multi-consumer patterns with strict ordering, persistent queues that survive across restarts), the right move is to plug RabbitMQ behind the MCP facade without changing the agent-facing interface.

This is exactly the "MCP as RPC facade" pattern paying off architecturally. The MCP schema stays stable (`send_mail`, `read_message`, `subscribe`, `drop_range`); the implementation evolves from "Maildir + etcd" to "RabbitMQ + etcd for discovery" to whatever's next. Agents don't notice.

Implementation note: when this migration happens, preserve the Maildir filesystem layout as an export format. Even if RabbitMQ is the runtime store, the ability to dump a mailbox to disk in a well-understood format is useful for debugging, backup, and the very real possibility that you want to migrate again later.

---

# 6. Auth futures

mu currently runs entirely within ambient-uid-trust (every process is your uid; the trust boundary is the user account). That's fine for today. It will not be fine forever — specifically, the moment you have:

1. Jails that should not trust each other fully
2. Inter-host mailbox traffic
3. Cooperating mu instances under different user accounts (one yours, one a colleague's)
4. Any kind of "untrusted MCP server I want to try without giving it my whole filesystem"

This section records design conclusions for the auth layer that will eventually have to exist.

## 6.1 OAuth at the boundary, mTLS in the trust zone

The right architectural split is **OAuth for cross-trust-boundary auth, mTLS for intra-trust-zone auth**. These answer different questions:

- **OAuth** is the right answer when you don't control the client. You issue tokens; clients present them; you verify. Standard, well-understood, lots of off-the-shelf implementations.
- **mTLS** is the right answer when you control both ends. The cert IS the identity. No token expiry/rotation pain. No phone-home for verification (if you trust your own CA).

For mu: jail-to-jail traffic on the same host = mTLS. mu-to-internet-MCP traffic = OAuth. The two coexist; they're answering different questions.

## 6.2 OAuth 2.1 + PKCE for human-in-the-loop

MCP standardized on OAuth 2.1 with PKCE for its streamable-HTTP transport. The relevant requirements for a self-hosted OAuth provider serving MCP:

- `/authorize` + `/token` endpoints
- **PKCE (S256) mandatory** — the security mechanism for public clients without a shared secret
- **No implicit grant, no password grant** (removed in 2.1)
- **Refresh token rotation** (each refresh invalidates the previous RT)
- **Discovery endpoint** (`/.well-known/oauth-authorization-server` per RFC 8414)
- **Dynamic Client Registration** (RFC 7591) if you want clients to self-register

This is the flow for any human-driven mu session that needs to reach an MCP server across a trust boundary: the user does an OAuth dance (touch the YubiKey for the WebAuthn step), gets a token, the mu session uses it to call the MCP.

## 6.3 client_credentials for agent-to-agent

Agent-to-agent traffic (mu peer talking to mu peer, no human in the loop) should NOT do the Authorization Code + PKCE dance. The right grant is `client_credentials`:

```
POST /token
  grant_type=client_credentials
  client_id=mu-peer-alpha
  client_secret=...
  scope=mailbox:read mailbox:write
```

No browser, no user consent, no PKCE. Just "agent X is authorized to do Y." Drastically simpler. Hydra (and any compliant OAuth 2.1 provider) supports this trivially.

For mu's likely architecture, every peer mu instance gets a `client_id` + `client_secret`, each scoped to specific capability sets. Token rotation can be automatic and short-lived (15-minute tokens with refresh) because there's no human to interrupt.

## 6.4 Self-hosting: Ory Hydra over Keycloak

For a single-user / small-team mu deployment, the options for self-hosted OAuth providers are:

| Project | Lang | Shape | mu fit |
|---|---|---|---|
| **Ory Hydra** | Go | Headless OAuth2/OIDC; bring your own login UI | **Probably best** — small, composable, production-grade |
| **Keycloak** | Java | Full IdP w/ UI, federation, SAML+OIDC | Battle-tested but heavy; JVM in a jail uses real memory |
| **Authentik** | Python | Modern full-stack IdP | Easy to spin up, more "appliance-y" |
| **Zitadel** | Go | Multi-tenant SaaS-shaped | Newer; more than mu needs |
| **Dex** | Go | OIDC federator | Wrong tool — Dex is for delegating to other providers, not being one |

Hydra is the analog of qmail in the mail world: small, single-purpose, does one thing well, composes cleanly. mu should probably pick it. Pair it with Ory Kratos (or a minimal hand-rolled login UI) for the identity-store/login-flow component.

Operational note: Hydra runs cleanly on FreeBSD. JVM-based options work too but use more memory and have less native-feeling integration with the jail/pot tooling mu is built around.

## 6.5 YubiKey across protocols

A YubiKey is a Swiss Army knife of auth primitives. The same physical device speaks:

- **U2F / WebAuthn (FIDO2)** — for browser-based 2FA on the OAuth login flow
- **PIV / smart card** — holds X.509 client certs (i.e., this is the path to mTLS-with-hardware-bound-keys)
- **OpenPGP** — code signing, gpg-agent-as-ssh-agent
- **OATH (TOTP/HOTP)** — for code-input flows where WebAuthn isn't supported

For mu's likely future architecture, one key, three roles:
1. **ssh into jails** (PIV-backed ssh key — already in place)
2. **OAuth 2FA** for the Hydra login flow (WebAuthn)
3. **mTLS CA root** if mu grows a jail mesh (PIV-backed CA cert; touch-to-sign for cert minting)

The deep operational win of hardware tokens is **the private key never leaves the device**, even under root compromise of the host. A compromised jail can use the key (via challenge-response) but cannot exfiltrate it. This changes the threat model for compromised-jail scenarios: an attacker who roots a jail can do what the jail was already authorized to do, but can't move the key elsewhere. Same property TPM-bound disk encryption gives you against offline attacks.

## 6.6 Capability-revocation (the hard problem)

mu-037 explicitly flags capability revocation as an open risk. OAuth doesn't help here as much as you'd think — short-lived tokens are the standard answer, but they require the issuer to be reachable for refresh. In a partitioned-network scenario, an agent might hold a still-valid token past the point you wanted it revoked.

The biscuit-auth direction the spec mentions is the right path: macaroon-style attenuated capabilities with offline verification. The mu trust model should converge on biscuits (or an equivalent) for fine-grained capability delegation, with OAuth/Hydra handling only the initial bootstrap.

This is a longer arc and probably not phase-1 work. But the design should leave room for it: don't bake "OAuth token is the only authorization primitive" into the protocol surface.

---

# 7. Billing topology

This section records operational/financial conclusions from working through how MCP usage actually bills under various client → model relationships. It is not a feature description; it is the economic ground truth that should inform mu's design choices.

## 7.1 The principle most people get wrong

**MCP server location and transport don't determine billing. The `client → model` relationship does.**

The MCP server is just a tool dispatcher. It doesn't talk to Anthropic (or OpenAI, or any model provider). The dollars are spent on the *model's* tokens: system-prompt tool definitions, output tokens for the `tool_use` block, input tokens on the next turn carrying the `tool_result`.

Whether your MCP server runs in your shell, in a jail, on Cloudflare, or on Mars, **is irrelevant to billing**. What matters is which `(client, model)` pair owns the conversation.

## 7.2 The (client × model) matrix as of 2026-05

| Client | Auth path | Model | MCP tokens billed as |
|---|---|---|---|
| Claude Code (OAuth → Max5 or Max20) | First-party OAuth | Claude | **Max plan quota** |
| Claude Code with `ANTHROPIC_API_KEY` set | API key | Claude | **per-token API** |
| Claude.ai web | OAuth → Max | Claude + Anthropic-hosted MCP | **Max quota** |
| Cline / Roo Code / aider pointed at Anthropic creds | API key | Claude | **per-token API + April 2026 third-party surcharge if extra-usage enabled** |
| Custom Python script using `anthropic` SDK | API key | Claude | **per-token API** |
| mu via Claude API direct | API key | Claude | **per-token API** |
| mu via openrouter | OpenRouter key | Whatever | **OpenRouter per-token** |
| mu via local Ollama | None | Local | **electricity only** |

## 7.3 The April 2026 third-party-client change

In April 2026, Anthropic started billing tokens from non-first-party Claude API clients (Cline, Roo Code, aider, similar agentic frameworks) at per-token API rates **on top of** any Max plan the underlying account holds, contingent on the user enabling "extra usage" in account settings.

Mechanically, Anthropic detects which client is making the API call via:
- **OAuth client_id** — Max OAuth tokens are minted with a `client_id` tied to Claude Code; other apps get a different `client_id`
- **User-Agent and endpoint surface** — third-party SDKs identify themselves; bare API key calls identify as `python-sdk`/`node-sdk`/etc.

The implication for mu: if mu makes Claude API calls, it is a third-party client by Anthropic's classification. **mu's Claude API usage cannot ride on the user's Max plan.** It needs an explicit API account, and (depending on the user's "extra usage" settings) may carry the surcharge.

This is consistent with what was already true for pi-rust earlier in 2026 — the same enforcement mechanism. mu inherits the same constraints.

## 7.4 What this means for mu's design

Three operational design conclusions:

1. **Mu should NOT silently use the user's API key for "small calls" while using Max OAuth for "big calls".** This kind of mixed billing leaves money on the table on the Max side and inflates the bill on the API side. Pick one billing path per session and stay on it.

2. **Mu's default for Anthropic-model use should be: explicit API key, separate billing surface from Claude Code.** Don't reach for the OAuth token Claude Code uses; that token is scoped to Claude Code by `client_id` and won't work for mu anyway.

3. **For non-Anthropic models (OpenAI via Codex sub, OpenRouter, local Ollama), use the appropriate path natively.** The agent-router pattern is the right architectural answer here — explicit routing per task, explicit billing per route.

## 7.5 What mu's mailbox architecture does NOT do to billing

A small but important point: when mu agents send messages to each other via the mailbox, **no model tokens are billed for the message traffic itself**. The mailbox MCP is a pure relay. Model tokens are billed only when a model on either side reads the messages (input tokens for tool_result) or writes responses to them (output tokens for tool_use).

This means the mailbox is a billing-neutral coordination layer. Two agents (say, claude-c137 and mu-gpt) can sustain a long conversation through the mailbox, and the cost on each side is whatever each side's model provider charges for the tokens that flow through that side. The mailbox itself adds nothing.

## 7.6 Hybrid: MCP servers that internally call other models

If you ever build (or use) an MCP server that internally calls a model — say, a "judge" MCP server that prompts GPT-4 to grade outputs — the bill splits:

- **Claude side** (claude-c137 → tool_use → MCP server): bills against Max OAuth (if first-party) or API (if third-party)
- **GPT-4 side** (inside the MCP server's OpenAI SDK call): bills against the OpenAI account configured in the MCP server

The user pays both. Model providers on each side have no idea the other exists.

This pattern is powerful but the billing surface multiplies. Some MCP servers in the wild are this shape — agent-mail with auto-summarization, "researcher" servers that internally chain to a model — and they look free until you realize they're spending API credit behind the curtain. **Audit any third-party MCP server's source for embedded LLM calls** before deploying it in a billing-sensitive context.

mu's own MCP servers (the ones built for mu, by mu's authors) should default to **no embedded model calls**. If a future mu MCP server needs to do model-mediated processing, it should be explicit about which billing surface it uses — preferably the same one the calling agent is on, so the cost shows up in one place.

---

# 8. Anti-patterns and CC-specific shapes mu should not copy

Not everything in Claude Code is a model for mu. Some choices are CC-specific, some are environmental assumptions that don't apply to FreeBSD, some are operational shortcuts mu should avoid.

## 8.1 The Linux-namespace-tied sandbox

CC's `/sandbox` command opens a panel that configures OS-level sandboxing for Bash commands. It works on macOS (Seatbelt), Linux (bubblewrap + socat), and WSL2 (treated as Linux). **It does not work on FreeBSD.**

The reason is that bubblewrap depends on Linux user, mount, and network namespaces — kernel features that don't exist in FreeBSD. The FreeBSD linuxulator emulates Linux syscalls but not the namespace plumbing, so even running bubblewrap inside the linuxulator would fail.

mu's analog of `/sandbox` is the jail (or pot) — heavier weight, per-process-tree rather than per-bash-call, but functionally equivalent for the threat model. Mu should NOT try to port `/sandbox` to FreeBSD. The jail layer already exists and works; adding a per-Bash-call sandbox on top would be redundant and harder to maintain.

Implication: any mu doc that references "sandbox" should be specific about which layer — jail-level (the heavyweight per-process-tree boundary mu inherits from the OS) or hypothetical-per-call (which mu doesn't have and shouldn't build).

## 8.2 The Anthropic remote-control bus

CC's `remoteControlAtStartup: true` registers the session with Anthropic's remote-control service, putting it on a bus reachable from the Claude mobile app, claude.ai web UI, and the trigger API. From the user's phone, a prompt can be pushed into the terminal session.

This is genuinely useful and worth replicating in spirit, but the mechanism is Anthropic-specific. Mu cannot use Anthropic's bus; mu would need its own.

The mu equivalent should be: a small self-hosted relay that mu sessions register with at startup, accepting authenticated prompts and routing them into the correct session's mailbox as `task` kind. Plus a thin mobile or web client. This is a real piece of work (mobile-side too) and probably not phase-1. But the design should be: don't pretend mu can use Anthropic's remote-control surface; build the equivalent locally.

## 8.3 SOC2-shaped audit posture

CC's settings.json exposes various audit/compliance-shaped features: detailed logging, hook-based interception, opt-in telemetry that goes to Anthropic-managed dashboards. These exist because Anthropic-the-company has compliance customers who need them.

Mu does not. mu is built by and for the user. The audit needs are different: the user wants to reconstruct what mu did last Tuesday, not produce a SOC2 evidence package.

This means mu's logging should be:
- **Operator-driven, not compliance-driven** — the user decides what's logged and where it goes
- **Local-first** — no opt-in remote telemetry by default
- **Forensically useful, not audit-bureaucratic** — readable by humans tomorrow, not by auditors next quarter

Specifically: mu should NOT add hook-based logging interception that mirrors CC's logging surface unless the user explicitly wants that level of granularity. The current `task_log` + `agent memory` SQLite layer is the right altitude for mu.

## 8.4 Embedded LLMs in tool servers

Covered in §7.6 from the billing angle. Worth repeating from the architecture angle: **mu's MCP servers should not embed LLM calls**.

Reasons:
- Billing surface multiplication (covered)
- Reproducibility — model-mediated processing is inherently non-deterministic; if the MCP server's behavior depends on a model call, the same input may produce different outputs across runs
- Trust boundary blur — once an MCP server is "just a tool dispatcher," its security posture is small and reviewable. Add an LLM call and suddenly the server is making decisions, and those decisions can be influenced by anything in its context
- Debuggability — model calls fail in different ways than file/network calls; debugging an MCP server that depends on a model is harder than debugging one that doesn't

If a feature genuinely requires model-mediated processing, it should be done **in the calling agent**, not in the MCP server. The agent already has model access (that's the whole point); the agent can decide when and how to invoke it.

## 8.5 Unmatched `PostToolUse` logging at scale

CC's `PostToolUse` hook (in many users' configs, including the working session's example) fires on every tool call regardless of tool name. This is fine when the log destination is a small SQLite DB and tool calls are tens-per-turn.

It scales badly when:
- The log destination is over the network
- Tool calls are hundreds-per-turn (large refactors, batch operations)
- The log includes full tool inputs/outputs (which can be MB-scale for file reads)

Mu should expose the same hook surface but encourage matcher-filtered hooks for high-volume use cases. The "log everything" default is fine for development; production-grade hooks should narrow the matcher.

## 8.6 Slash-command output styles in the doc surface

CC has slash commands that affect output style ("explanatory mode," etc.). These are user-facing UX features. Mu has its own style choices and should not slavishly copy CC's style commands unless a specific use case demands it.

Conversely, the *mechanism* by which CC implements them — output styles as plugins / skills with `style: ...` frontmatter — is a clean architectural pattern worth borrowing. Just borrow the mechanism, not the specific styles.

---

# 9. Cross-references to existing mu specs

This section connects the design conclusions above to specific mu specs that are already in flight. Where a conclusion has implications for a spec, the spec is named.

## 9.1 mu-029 (input_required)

§3.2 (PushNotification analog): mu-029's escalation transport is the right place for the human-touch-required notifications. Design conclusion: escalations should be sparse, bundled where possible, and silenceable per-context. Default to silent; require explicit signal to escalate.

## 9.2 mu-031 (delegate)

§1.3 (paired hooks): the parent/child delegate relationship in mu-031 is the natural place to wire paired hooks. The parent records expectations at delegation time; the parent's Stop hook (or equivalent) compares actual outcome to recorded expectations.

§7.4 (billing topology): delegate sessions inherit billing from their parent unless explicitly routed. mu's delegate primitive should expose billing routing as an explicit parameter, not an implicit inheritance — so the user knows which budget each delegate spends from.

## 9.3 mu-033 (capability attenuation)

§6 (auth futures): the capability primitive in mu-033 is the right substrate for biscuit-style attenuated capabilities. When the OAuth/biscuit migration happens, mu-033's existing attenuation/intersection logic should be reused; biscuits should be the wire format for cross-jail/cross-host capability delegation, with mu-033's primitives handling in-process delegation.

## 9.4 mu-036 (autonomous)

§3 (push primitives): autonomous mu sessions running in `/loop`-like mode are the canonical case for needing external wake signals. mu-037 mailbox + mu-029 escalation + external trigger surface compose into the autonomous loop's input plane.

## 9.5 mu-037 (peer discovery + mailbox)

This whole document touches mu-037 heavily. The specific cross-references:

- §3.3 (Monitor's "stdout = event"): mailbox watchers should emit one structured line per event
- §4.5 (resource subscriptions): mailbox should be exposed as a resource with subscribe semantics, with Monitor-watcher as the fallback for clients that don't support subscriptions
- §5 (entire section): the mailbox architecture design
- §6 (auth): the trust model open-risk in mu-037 maps to OAuth client_credentials for peer-to-peer authentication
- §7.5 (billing-neutral coordination): mailbox traffic doesn't bill model tokens, so cooperating agents can sustain long exchanges through it cheaply

## 9.6 General: where deferred-tools applies

§2 (deferred tools and tool-catalog economics) doesn't have a specific mu spec yet. It probably should. Suggested spec name: `mu-NNN-deferred-tools-and-catalog-economics.md`. The bead breakdown:

- **Phase 1**: two-tier catalog (eager set + deferred set), basic search-by-name lookup
- **Phase 2**: SQLite FTS5 over tool descriptions for keyword search
- **Phase 3**: listChanged notifications when MCP server tool sets update mid-session
- **Phase 4**: telemetry signal for "tool catalog cost as fraction of context"

---

# 10. Provenance

This document is a synthesis of a working session on 2026-05-21 between Thaddeus and a Claude Code instance (the "claude-personal" account). The session covered:

- Claude Code feature exploration (sandbox, GitHub App, hooks, deferred tools, push primitives)
- MCP protocol mechanics and capability surfaces
- Mailbox architecture (Maildir + etcd + filter design)
- OAuth/mTLS/YubiKey futures
- Billing topology under Max plans, API accounts, and third-party clients
- The mu-CAT war story (out of scope for this document — see `career_book/cat-thesys-retrospective.md`)

The companion document `claude-code-feature-mapping.md` in this directory was written earlier the same day by a different Claude Code instance (claude-c137); it covers the doc-walk feature inventory in tier form. The two documents are complementary; this one captures synthesis conclusions, the other captures feature-by-feature implementation notes.

When in doubt about specifics, re-fetch the relevant Claude Code documentation page — the surface changes month to month. The conclusions here are about *shapes* and *economics*, which are stickier than feature surface.
