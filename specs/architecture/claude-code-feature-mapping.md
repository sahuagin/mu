# Claude Code → mu: feature inventory and mapping

**Date**: 2026-05-21
**Audience**: claude-c137, future mu sessions, bead-creation prompts
**Status**: research complete; bead breakdown pending

This document is the result of a feature-by-feature walk through the Claude Code
documentation, scoped to "what should mu model, and what should mu skip." It
exists so future sessions (and other claude instances) don't have to redo the
research.

## How to use this document

- **Each numbered item below is a candidate bead.** The Tier indicates priority;
  the "Next bite" suggests the smallest useful implementation chunk.
- **Every item has a source URL** at `code.claude.com/docs/en/...`. When in doubt
  about behavior, re-fetch the source page — these docs are revised frequently
  and "what was true in May 2026" may not be the spec by the time you read this.
- **The deep sections (§A–§D)** are implementation specs, not just summaries.
  Cache discipline (§A) is essentially the contract for mu-bn4's
  `AnthropicCacheStrategy`. The worktree hook recipe (§B) is wire-it-up-today
  scope. OTel signal architecture (§C) and the supervisor pattern (§D) are
  architectural decisions with concrete starting points.

## Source documents consulted

Direct URLs (re-fetch if details matter for an implementation decision):

- `/en/overview`
- `/en/context-window`
- `/en/features-overview`
- `/en/how-claude-code-works`
- `/en/memory`
- `/en/sessions`
- `/en/permissions`
- `/en/permission-modes`
- `/en/routines`
- `/en/checkpointing`
- `/en/commands`
- `/en/agent-sdk/observability`
- `/en/tools-reference`
- `/en/agent-view`
- `/en/prompt-caching`
- `/en/worktrees`
- `/en/llms.txt` (full doc index)

Pages NOT yet read but likely high-value next reads:

- `/en/agent-sdk/agent-loop`
- `/en/agent-sdk/file-checkpointing`
- `/en/agent-sdk/session-storage`
- `/en/agent-sdk/sessions`
- `/en/hooks-reference` (404'd at time of research — try `/en/hooks` or
  `/en/hooks-guide` instead)
- `/en/agents` (overview of parallel-execution surfaces)
- `/en/channels` and `/en/channels-reference`
- `/en/monitoring-usage` (full attribute/metric reference for OTel)

---

# Part 1 — Tiered feature inventory

## Tier 1 — Build this; the docs essentially write the spec

### 1. Context-window explorer

**Source**: `/en/context-window`

The page ships a fully data-driven interactive timeline. The data model is
worth modeling exactly:

- **Event record**: `{t: 0.0–1.0, kind, label, tokens, color, vis, desc, link, tip, noSurviveCompact?}`
  - `kind` ∈ `{auto, user, claude, hook, sub, compact}`
  - `vis` ∈ `{hidden, brief, full}` — drives a ●/◐/○ visibility indicator
  - `noSurviveCompact: true` flag on skills listing (only thing that doesn't
    reload after `/compact`)
- **11-color legend**: System / CLAUDE.md / Memory / Skills / MCP / Rules /
  Files / Output / Claude / You / Hooks
- **"Gate" events**: prompts, `!` bang commands, `/` slash commands, and
  `/compact` pause playback and require a button click. This is how the
  interactive "video that you advance with buttons" works.

**mu fit**: maps cleanly onto `RetainedRope` / `ProviderStatusSnapshot` work
(mu-nat, mu-di4). Rope spans already are an event stream — add
`(token_cost, visibility, survives_compact)` tags to each and render. The
live-loop integration point is mu-fb0.

**Next bite**: extend rope span metadata with the three tags above. A renderer
can come later; the data model is the load-bearing part.

### 2. Memory system — three distinct layers

**Source**: `/en/memory`

Claude Code distinguishes three layers mu currently collapses:

| Layer | Who writes | When loads | Truncation |
|---|---|---|---|
| **CLAUDE.md** (managed/user/project/local) | Human | Every session, full | None |
| **`.claude/rules/`** with `paths:` frontmatter | Human | When matching file is read | None |
| **MEMORY.md + topic files** (auto memory) | Claude | First 200 lines / 25KB of MEMORY.md | Topic files load on demand |

Design moves to copy:
- **Path-scoped rules**: YAML frontmatter `paths: ["src/api/**/*.ts"]` →
  rule fires only when matching file is touched. Lazy load is the feature.
- **MEMORY.md as index, topic files for body**: already mu's `agent memory`
  pattern via SQLite; the markdown surface is for in-tree discoverability.
- **`@path/to/file` imports** with 5-hop recursion limit and first-encounter
  approval dialog.
- **`claudeMdExcludes`** in settings for monorepos.
- **Hierarchy**: managed > user > project > local. CLAUDE.md is *additive*;
  skills/subagents/MCP servers *override by name*.

**mu fit**: agent.sqlite already covers the auto-memory dimension. The gap is
**path-scoped rules** under `.claude/rules/*.md` — small, high-value add.

**Next bite**: implement path-scoped rule loading. Define the frontmatter
schema, wire it to mu's file-read pipeline, surface in mu's equivalent of
`/memory`.

### 3. Session management — resume / branch / rewind

**Source**: `/en/sessions`, `/en/checkpointing`

Three primitives, two of which are subtle:

**Resume** (`--continue`, `--resume`, `--resume <name>`, `--from-pr <num>`):
- Sessions stored as JSONL at `~/.claude/projects/<project>/<session-id>.jsonl`
- Picker shows sessions from current worktree; `Ctrl+W` widens to all worktrees,
  `Ctrl+A` to all projects, `Ctrl+B` filters to current branch
- Each row: `name | summary-or-first-prompt | time-since | message-count | git-branch`
- Pasting a PR URL into search finds the session that created it

**Branch** (`/branch [name]`, `--fork-session`):
- Copies history into new session ID; original unchanged
- **Permissions approved "for this session" do NOT carry over to the fork**
  (subtle gotcha worth replicating)

**Rewind** (`/rewind`, double-Esc on empty input, aliases `/checkpoint` /`/undo`):
- Per-prompt picker with **six** actions:
  1. Restore code AND conversation
  2. Restore conversation only (keep code)
  3. Restore code only (keep conversation)
  4. **Summarize FROM here forward** (compress recent context)
  5. **Summarize UP TO here** (compress early setup)
  6. Never mind
- Summarize keeps you in the same session; messages preserved in transcript
- "Restore" reverts state; "Summarize" compresses without changing files

**The dual rewind/summarize axis is the gem.** Restore is destructive (undo
state). Summarize is non-destructive (compress context). Both target the same
prompt-checkpoint.

**Checkpointing storage and triggers**:
- Snapshot taken before each Claude file-edit tool call
- Each user prompt creates a new checkpoint marker
- **Bash-modified files NOT tracked**
- External edits NOT tracked
- 30-day retention via `cleanupPeriodDays`
- File contents only — not conversation. Explicitly "local undo, not Git."

**mu fit**: you journal conversations already. Adding the 6-action picker is
mostly UX work. Pre-edit file snapshotting keyed by `(session_id, prompt_id, file_path)`
is straightforward to hook into mu's edit-tool dispatcher.

**Next bite**: implement pre-edit snapshots + the 6-action rewind picker as
two separate beads. The picker can land first with stub "restore code" no-op
if checkpoint storage isn't ready.

### 4. Permission modes + permission rules (two orthogonal systems)

**Source**: `/en/permissions`, `/en/permission-modes`

**Modes** (Shift+Tab cycles): `default` → `acceptEdits` → `plan` → `auto`.
Admin-only: `bypassPermissions`, `dontAsk`. Modes are session-level UX.

**Rules** (`/permissions` UI + `settings.json`):
- 3-tier: `allow` / `ask` / `deny`. Evaluated **deny → ask → allow**; first match wins.
- Pattern syntax: `Bash(npm run *)`, `Read(./.env)`, `WebFetch(domain:github.com)`,
  `mcp__puppeteer__*`, `Agent(Explore)`
- **Compound-command awareness**: `Bash(safe-cmd *)` does NOT cover
  `safe-cmd && other-cmd`. Each subcommand must match independently.
  Recognized separators: `&& || ; | |& & newline`.
- **Process-wrapper stripping**: `timeout`, `time`, `nice`, `nohup`, `stdbuf`,
  bare `xargs` stripped before matching. Devbox-style runners (`devbox run`,
  `npx`, `docker exec`) explicitly NOT stripped — they consume their args.
- **Read-only command allowlist baked in**: `ls`, `cat`, `head`, `tail`, `grep`,
  `find`, `wc`, `which`, `diff`, `stat`, `du`, `cd`, read-only `git`. Never prompts.
- **Gitignore-style path patterns**: `//absolute`, `~/home`, `/project-root`,
  `./cwd`, bare names match at any depth.
- **Symlink rule**: allow needs both source AND target to match;
  deny triggers if either matches.

**mu fit**: mu-stw (AWS capability runtime enforcement) is the enforcement analog
of *rules*. The four design moves to copy:
1. Add a **mode** abstraction over the rule set (`plan` mode = locked read-only,
   most useful for autonomous loops)
2. **Compound-command splitting** before policy check (pi-runtime uses pipes)
3. **Wrapper-stripping list** as a denylist of "things that look safe but
   execute their argument"
4. Distinguish **session-scoped grants** from persistent grants

**Next bite**: implement permission modes as a discrete bead (the rule system
exists; modes are a thin layer). Wrapper-stripping is a separate small bead.
Compound-command splitting is third.

### 5. Routines / scheduling — three distinct tiers

**Source**: `/en/routines`, `/en/scheduled-tasks`, `/en/desktop-scheduled-tasks`

DO NOT collapse these three tiers:

| Tier | Where it runs | Triggers | Reach |
|---|---|---|---|
| **`/loop`** (in-session) | Your terminal, your session | Time interval OR model-self-paced | Whatever session has |
| **Desktop scheduled tasks** | Your machine, fresh session | Cron-like | Local files |
| **Routines** (cloud) | Anthropic VMs | Schedule + API webhook + GitHub events | Cloned repo + MCP connectors + env vars |

Routines design moves worth stealing:
- **Per-routine HTTP `/fire` endpoint with bearer token**: external systems
  POST `{"text": "<context>"}` to trigger
- **GitHub triggers with filter predicates**: author, title regex, base/head
  branch, labels, draft state, merged state
- **Network policy per-environment, not per-routine**: `Trusted` (default
  allowlist), `Custom` (your domains), `Full` (open)
- **Caches setup script result** — env doesn't re-run install on every fire
- **One-off scheduled runs** exempt from daily run cap

**mu fit**: you already have `/loop` analog and external cron. The pieces worth
stealing: per-route HTTP webhook for API triggers, filter-predicate model for
external events.

**Next bite**: HTTP webhook endpoint per scheduled mu task. Smaller, more useful
than the full Routines surface.

## Tier 2 — High-value, not on the original short list

### 6. "What survives compaction" — the semantics table

**Source**: `/en/context-window`

Each context source declares its compaction behavior:

| Source | Behavior |
|---|---|
| System prompt + output style | Unchanged (not in message history) |
| Project-root CLAUDE.md + unscoped rules | Re-injected from disk |
| Auto memory | Re-injected from disk |
| **Path-scoped rules** | **Lost until matching file read again** |
| **Nested CLAUDE.md** | **Lost until subdirectory file read again** |
| **Invoked skill bodies** | Re-injected; capped at 5K/skill and 25K total; oldest dropped first |
| Hooks | N/A (they run as code) |

**Mu fit**: publish the same table for mu's own context sources. This is the
spec for how compaction interacts with everything else.

**Next bite**: a doc, not a code change. Write `specs/architecture/compaction-semantics.md`
defining each mu context source's compaction behavior. Implementation flows
from the doc.

### 7. Hook lifecycle event set

**Source**: `/en/features-overview`, `/en/hooks-guide` (the hooks-reference page
404'd at research time; try those alternates)

Named events: `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`,
`PreCompact`, `Stop`, `SubagentStop`, `Notification`.

Critical behaviors:
- **`hookSpecificOutput.additionalContext` JSON field** is the ONE way to feed
  hook output INTO the model's context. Plain stdout on exit-0 → debug log only.
- **`PreToolUse` exit-code 2 takes precedence over allow rules** — blocking
  hooks are stronger than allow rules but cannot override deny rules.
- **Hook output enters context without truncation** — keep it concise.

**Mu fit**: you use hooks for `post-compact-reread` and `SessionStart` already.
The `additionalContext` JSON convention should become mu's official hook-output
protocol.

**Next bite**: formalize mu's hook event names and JSON output protocol.
Should mirror Claude Code's event names for portability.

### 8. Plan mode — distinct from "read-only allowlist"

**Source**: `/en/permission-modes`

Plan mode = a permission mode that **locks the agent into read-only tools AND
produces a plan artifact you approve before any write**. Accepting a plan
**auto-names the session from the plan content**.

This is a tighter coupling between mode/session-naming/approval than mu currently
has. The "session-name-from-plan-content" detail is small but high-quality.

**Next bite**: implement plan mode as a discrete bead once #4 (permission modes)
lands. The session-naming side is independent of the mode itself.

### 9. Slash-command surface IS the skill system

**Source**: `/en/commands`, `/en/skills`

Many "commands" are actually bundled skills (marked **[Skill]** in the commands
table): `/simplify`, `/batch`, `/review`, `/security-review`, `/debug`, `/loop`,
`/verify`, `/run`, `/claude-api`, `/fewer-permission-prompts`. **The skill
mechanism IS the command mechanism.** Don't add CLI built-ins; add skills.

mu's `/goal-protocol`, `/handoff`, `/postmortem`, `/jj-runbook` already work
this way. The architectural rule: skills are the only extension mechanism for
user-facing commands.

**Next bite**: audit any mu built-ins that should be skills. Move them.

### 10. `disable-model-invocation: true` skill flag

**Source**: `/en/skills`

Pattern: skills with side effects (deploy, commit-push) opt out of
auto-invocation. They become **invisible to the model** until user types
`/<name>` explicitly. Zero context cost when not used. Settings-side
equivalent: `skillOverrides` lets you hide a skill you didn't write.

**Mu fit**: explicit opt-in vs opt-out default per skill. Side-effect skills
should default to manual-only.

**Next bite**: add the flag to mu's skill frontmatter parser and respect it
in the model-visible skill list.

### 11. `/insights` and `/team-onboarding`

**Sources**: `/en/commands` (entries for both)

- `/insights` generates a Claude-narrated report from your own session history.
  Different from a metrics dashboard. Complements your existing `ccmon` plan
  (memory `project_metrics_dashboard.md`).
- `/team-onboarding` analyzes 30 days of session history and generates a
  markdown guide a teammate can paste as a first message. Returns a share link.
  Your `/handoff` skill writ large.

**Next bite**: lower priority. Defer until at least one of the Tier-1 items
ships.

## Tier 3 — Worth considering, lower priority

### 12. `/background` / agent-view dashboard

**Source**: `/en/agent-view`

See §D below for the architecture deep-dive.

**Next bite**: see §D. This is "your current pot-orchestration architecture,
formalized."

### 13. `/btw` (side question without conversation pollution)

Tiny, useful. Side question doesn't add to history.

### 14. `/copy [N]` with code-block picker + `w`-to-write-to-file

For SSH workflows. Saves piping through xclip.

### 15. `/diff` with per-turn diffs

Arrow keys cycle between current git diff and individual Claude turns.
Better than what most agents expose.

### 16. `/focus` view

Show only last prompt + tool-call one-liner + final response. For watching
long autonomous runs without scroll fatigue. Especially relevant for mu's
long sessions (avg 8 hr per insights report).

### 17. `/effort [low|medium|high|xhigh|max]` session dial

Explicit effort dial as session-level UX. mu has model selection; effort dial
is a layer on top.

### 18. Bundled bash wrapper handling

The fixed denylist of `watch`, `setsid`, `ionice`, `flock`, `find -exec`,
`find -delete` that always prompt regardless of allow rules. Port into mu's
capability matcher.

### 19. `/sandbox` toggle

Permissions block intent; sandbox blocks execution. Defense-in-depth pattern
worth documenting.

### 20. `Channels` (`/en/channels`)

Different from API triggers — persistent inbound message streams from
Telegram/Discord/iMessage/webhooks. Lower priority for solo work.

## Tier 4 — Skip or already have

- Plugins / marketplaces (distribution isn't a bottleneck)
- `/install-github-app`, `/install-slack-app`, `/setup-bedrock`, `/setup-vertex`
  (auth wizards; BYO-credentials)
- `/passes`, `/stickers`, `/upgrade`, `/radio` (SaaS surface)
- `/teleport`, `/remote-control`, `/mobile`, `/desktop` (cross-surface only
  matters with multiple surfaces)
- `/voice` (input modality, separate problem)
- Multiple themes / `/color` / `/scroll-speed`

---

# Part 2 — Implementation specs

## §A. Cache-discipline contract (the mu-bn4 / AnthropicCacheStrategy spec)

**Source**: `/en/prompt-caching`

The prompt-caching page is essentially the spec for mu-bn4. Encode these as
invariants in `AnthropicCacheStrategy`:

### Three-layer prefix ordering (most-stable → most-volatile)

```
[1] System prompt   — instructions, tool defs, output style
[2] Project context — CLAUDE.md, auto memory, unscoped rules
[3] Conversation    — user msgs, model responses, tool results
```

Anything placed earlier than its volatility class warrants tanks the cache.
"A change to the system prompt invalidates everything." This is the literal
reason output-style and CLAUDE.md edits **silently fail to apply mid-session** —
loading them would invalidate everything that follows.

### Cache-key invariants

- **Model identifier** (model switch = total recompute)
- **Working directory** (embedded in system prompt — sibling worktrees of the
  same repo miss each other's cache)
- **Git branch + recent commits snapshot** (captured at session start)
- **Set of tool names** (not schemas — names suffice. `ToolSearch` exists
  precisely so MCP schemas can defer without changing the keyed prefix)

### Invalidators (force full recompute on next turn)

| Action | Why |
|---|---|
| `/model` switch | Cache keyed by model |
| MCP server connect/disconnect (incl. silent reconnects after stdio process exit, HTTP session expiry, server-initiated dynamic tool updates) | Tool name set changed in system prompt |
| Bare-tool deny rule added (`deny: ["Bash"]` or `deny: ["WebFetch"]`) | Tool removed from context |
| `/compact` | Replaces conversation layer with summary |
| Claude Code binary upgrade | System prompt likely changed |
| Resume after upgrade | Same |

### Non-invalidators (cache stays warm)

| Action | Why |
|---|---|
| File edit in repo | Appended as `<system-reminder>`; Claude re-reads if needed |
| **CLAUDE.md edit mid-session** | Doesn't invalidate AND doesn't apply — held in memory from session start |
| Output style change via `/config` | Same — doesn't apply until next `/clear` |
| Permission mode change | Not in prompt text. *Exception*: `opusplan` resolves to Opus/Sonnet on plan-mode toggle, so it IS a model switch |
| **Scoped** deny rule like `Bash(rm *)` | Tool still in context; checked at call site |
| Skill or command invocation | Injected as user message — appended, not prefixed |
| `/recap` | Appended as command output |
| **`/rewind`** | Truncates back to a prefix that's already cached. **Cheaper than `/compact`** |
| Spawning a subagent | Subagent has its OWN cache; parent prefix untouched |

### TTL discipline

- 5min default for API-key auth
- 1hr automatic for Claude subscription (no extra billing)
- 1hr opt-in via `ENABLE_PROMPT_CACHING_1H=1`
- 5min force-override via `FORCE_PROMPT_CACHING_5M=1`
- Each cache-hitting request **resets the timer** — sustained work keeps cache warm
- Subagents always use 5min, even on subscription

### Observability invariants

API response carries two metrics:
- `cache_creation_input_tokens` (this turn wrote to cache, billed at cache-write rate)
- `cache_read_input_tokens` (this turn read from cache, billed at ~10% of input rate)

mu's status renderer (mu-di4) should display the read:create ratio. Sustained
high creation count turn-after-turn means the prefix is drifting — that's a
bug, not a UX choice.

### Operating rules for mu-fb0 (live-loop adoption)

1. **Pin model and MCP set at session start.** If users swap models or hot-add
   MCP mid-session, charge them visibly for it.
2. **Never reload CLAUDE.md mid-session** even if disk changes. Pretend it's
   frozen until `/clear`.
3. **Prefer `/rewind` over `/compact`** when discarding a side-track. Rewind
   hits a still-warm prefix; compact rebuilds.

## §B. Worktree adapter contract (jj + pot recipe)

**Source**: `/en/worktrees`

`WorktreeCreate` and `WorktreeRemove` hooks are the explicit extension seam
for non-git SCMs. Hook contract:

```
INPUT (stdin):   {"name": "<session-name>"}
                 (WorktreeRemove also gets {"path": "..."} per the doc)
OUTPUT (stdout): <absolute-path-to-worktree-directory>  (WorktreeCreate only)
SIDE EFFECTS:    Whatever you want — create checkout, spin up container, etc.
```

The doc shows SVN as the example. jj+pot is the same shape.

### Hook registration

```json
{
  "hooks": {
    "WorktreeCreate": [{
      "hooks": [{
        "type": "command",
        "command": "/usr/local/bin/mu-worktree-create"
      }]
    }],
    "WorktreeRemove": [{
      "hooks": [{
        "type": "command",
        "command": "/usr/local/bin/mu-worktree-remove"
      }]
    }]
  }
}
```

### `/usr/local/bin/mu-worktree-create`

```sh
#!/bin/sh
set -eu
NAME=$(jq -r .name)
REPO_ROOT=$(jj root 2>/dev/null || git rev-parse --show-toplevel)
WORKTREE_DIR="$REPO_ROOT/.claude/worktrees/$NAME"
POT_NAME="mu-wt-$NAME"

# 1. Create jj workspace
jj --repository "$REPO_ROOT" workspace add \
    --name "$NAME" "$WORKTREE_DIR" >&2

# 2. Copy gitignored files (since the hook REPLACES default logic,
#    .worktreeinclude isn't auto-processed)
if [ -f "$REPO_ROOT/.worktreeinclude" ]; then
    while IFS= read -r pattern; do
        [ -z "$pattern" ] && continue
        case "$pattern" in '#'*) continue ;; esac
        find "$REPO_ROOT" -maxdepth 3 -name "$pattern" \
            -not -path '*/.jj/*' -not -path '*/.git/*' 2>/dev/null \
        | while read -r f; do
            rel="${f#$REPO_ROOT/}"
            mkdir -p "$WORKTREE_DIR/$(dirname "$rel")"
            cp "$f" "$WORKTREE_DIR/$rel"
        done
    done < "$REPO_ROOT/.worktreeinclude"
fi

# 3. Spin up a pot bound-mounted onto the worktree
pot clone -P spline-base -p "$POT_NAME" >&2
pot mount-in -p "$POT_NAME" -m /workspace -d "$WORKTREE_DIR" >&2
pot set-attribute -p "$POT_NAME" -A start-at-boot -V NO >&2
pot start -p "$POT_NAME" >&2

# 4. Emit the worktree path Claude Code / mu will cd into (host-side path).
echo "$WORKTREE_DIR"
```

### `/usr/local/bin/mu-worktree-remove`

```sh
#!/bin/sh
set -eu
NAME=$(jq -r .name)
PATH_=$(jq -r .path)
POT_NAME="mu-wt-$NAME"

# 1. Tear down pot first (releases bind mounts cleanly)
if pot show -p "$POT_NAME" >/dev/null 2>&1; then
    pot stop -p "$POT_NAME" >&2 || true
    pot destroy -F -p "$POT_NAME" >&2
fi

# 2. Forget the jj workspace
REPO_ROOT=$(jj --repository "$PATH_" root 2>/dev/null || echo "")
if [ -n "$REPO_ROOT" ]; then
    jj --repository "$REPO_ROOT" workspace forget "$NAME" >&2 || true
fi

# 3. Remove the worktree directory
rm -rf "$PATH_"
```

### Design notes

1. **Bind direction**. The hook above bind-mounts the worktree INTO the pot at
   `/workspace`. mu/claude-code runs outside; the pot is a containment boundary
   for builds, tests, and Bash. To run mu *inside* the pot (true ephemeral
   agent-in-jail), wrap the binary at session-launch level (not hook level),
   since the WorktreeCreate hook fires from outside the pot.
2. **`worktree.baseRef`**. Set to `"head"` in mu settings if the pot needs your
   unpushed jj revisions; default `"fresh"` branches from `origin/HEAD`.
   Per memory `feedback_jj_as_claude.md`, never run jj state-modifying commands
   as the claude UID inside the jail — the hook above runs as the user invoking
   mu (host side), which is correct.
3. **Auto-cleanup discipline**. Claude Code's default: subagent worktrees removed
   on finish-without-changes; orphaned ones swept after `cleanupPeriodDays`.
   Honor the same semantics — if `WorktreeRemove` is called with an "intact"
   worktree, keep the pot+workspace; if called with `--force` or with the
   worktree already merged, destroy. The hooks-reference page (which 404'd at
   research time) probably specifies a flag for this. Verify.

### Suggested `.worktreeinclude` for mu

(Add `.claude/worktrees/` to `.gitignore` / `.jjignore` first.)

```
.env
.env.local
.envrc
.direnv/
config/secrets.toml
```

## §C. Observability — copy this OTel stack wholesale

**Source**: `/en/agent-sdk/observability`, `/en/monitoring-usage` (full ref —
not yet read)

Three independent signals on independent enable flags:

| Signal | Enable | Contents |
|---|---|---|
| Metrics | `OTEL_METRICS_EXPORTER=otlp` | Token counters, cost, session count, lines-of-code, tool-decision counters |
| Logs | `OTEL_LOGS_EXPORTER=otlp` | Structured records per prompt / API request / API error / tool result |
| Traces | `OTEL_TRACES_EXPORTER=otlp` + `CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1` | Spans for interactions, model requests, tool calls, hooks |

### Span hierarchy (replicate exactly)

```
claude_code.interaction               (one full turn of the agent loop)
├── claude_code.llm_request           (model name, latency, token counts as attrs)
├── claude_code.tool                  (one tool invocation)
│   ├── claude_code.tool.blocked_on_user   (permission-prompt wait)
│   └── claude_code.tool.execution         (actual run)
└── claude_code.hook                  (one hook execution; detailed-beta flag)

When a subagent is spawned via Task tool:
claude_code.tool (parent's Task call)
└── claude_code.interaction (subagent's loop)
    └── claude_code.llm_request, claude_code.tool, ...    ← nest as children
```

Subagent spans nesting under parent's tool span means a complete delegation
chain appears as ONE trace. mu's parallel-workers pattern produces flame graphs
out of the box if you adopt this naming.

### Critical attribute: `session.id`

Every span carries `session.id`. **This is the join key** between traces,
`~/.local/share/task_log.sqlite`, and any UI. When mu writes to task_log,
include the same session.id so trace ↔ log joins are trivial.

### W3C trace context propagation

Two clever patterns:
1. **Injects `TRACEPARENT` into the CLI child process env** — so spans become
   children of the calling application's spans.
2. **Forwards `TRACEPARENT` into every Bash/PowerShell command** — so any
   subprocess that emits OTel nests under `claude_code.tool.execution`.

This composes with the jj+pot worktree adapter (§B): if pot-side build steps
emit OTel, the trace shows
`mu.tool.execution → pot exec → build span → cargo test span` as one chain.

### Sensitive-data tiering — opt-in by tier

| Variable | Adds |
|---|---|
| `OTEL_LOG_USER_PROMPTS=1` | User prompt text on `user_prompt` events and `interaction` spans |
| `OTEL_LOG_TOOL_DETAILS=1` | Tool input args (paths, commands, search patterns) on `tool_result` events |
| `OTEL_LOG_TOOL_CONTENT=1` | Full tool input AND output bodies as span events, truncated at 60KB |
| `OTEL_LOG_RAW_API_BODIES=1` or `file:<dir>` | Full Messages API request+response JSON. `file:` form writes untruncated to disk with `body_ref` path in event |

Design pattern: structural data always exported (token counts, durations, tool
names). Content requires explicit opt-in. The `file:` mode keeps your transcript
exporter aligned with your trace exporter using a `body_ref` pointer.

**Mu fit**: this is the model for `mu observe` / `mu logs`. Match the variable
names if possible — anyone who's run Claude Code at scale already knows them.

### Audit-trail event set (for SIEM)

Doc-flagged security-relevant events:
- `tool_decision` (allowed/asked/denied for each call)
- `tool_result`
- `mcp_server_connection`
- `permission_mode_changed`

Combined with `enduser.id` + `tenant.id` resource attributes (percent-encoded
per OTel spec), this is a complete per-user audit trail.

## §D. Background-agent / supervisor — your architecture, model the UI

**Source**: `/en/agent-view`

You have the architectural pieces (pot-based ephemeral agents, agent-router
dispatching, task_log journaling). What agent-view adds is the **dashboard
discipline**.

### Supervisor process model

| Concept | Claude Code | mu mapping |
|---|---|---|
| Per-user daemon | `~/.claude/daemon.log`, `~/.claude/daemon/roster.json` | Same shape; agent.sqlite already serves roster role |
| Per-session state | `~/.claude/jobs/<id>/state.json` | Map to per-pot state |
| Auto-stop after idle | ~1hr unattached → process exits, state on disk | Pots can already do this |
| Cold-restart | Next peek/reply/attach spawns fresh process from saved state | Matches "respawn from saved conversation" |
| Auto-update | Watches binary on disk, restarts into new version | Hot mu upgrades |
| Status introspection | `claude daemon status` | Add `mu daemon status` |

### Haiku-priced row summaries

Row summaries are generated by a Haiku-class model, refreshing every 15s + once
per turn-end. Each refresh is one short Haiku request through your normal
provider. So the dashboard pays its own way per-row instead of static
summarization.

**Mu fit**: when ProviderStatusSnapshot (mu-di4) extends to multiple parallel
sessions, use a small model for per-row summary. 15s refresh cap is the
rate-limit number.

### State icons — orthogonal axes

- **Activity** (color/animation): Working / Needs input / Idle / Completed / Failed / Stopped
- **Process** (shape): `✻`/`✽` alive, `∙` exited-restartable, `✢` /loop sleeping

Color says "what does it want from me." Shape says "is the runtime alive."
mu's throbber work (mu-di4) covers activity; process axis is missing.

### Keyboard model worth copying

```
↑↓     navigate
Space  peek (one-line excerpt + reply input, no full attach)
Enter  attach (full conversation takeover)
→      attach (alt)
←      detach back to view
Alt+1..9   attach by ordinal — fast multi-session switching
Ctrl+S grouping toggle (state vs. directory)
Ctrl+T pin to top
Ctrl+X stop; press again ≤2s to delete
Ctrl+G open prompt in $EDITOR
```

`Space` for peek (vs Enter for attach) is the move that makes a multi-session
dashboard usable.

### Filter syntax

```
a:<agent-name>       sessions running that agent
s:<state>            s:working, s:blocked (anything waiting on you)
#<num> or PR URL     session working on that PR
```

### Auto-worktree-isolation policy

> Before editing files, the bg session moves into `.claude/worktrees/<id>/`,
> so parallel sessions can read the same checkout but each writes to its own.

**Spawn ephemeral isolation only when the agent attempts a write, not at
session start.** Reads are cheap; isolate at first-write.

---

# Part 3 — Cross-references to mu's existing state

## Existing beads this maps to

(From the 2026-05-14 EOD snapshot, memory `7e62b34e`)

| Existing bead | Maps to item(s) above |
|---|---|
| **mu-bn4** (Anthropic provider + cache strategy, LANDED) | §A — cache discipline IS the spec for the cache strategy half |
| **mu-fb0** (live-loop adoption) | §A operating rules; item #1 (context-window) for runtime display |
| **mu-nat** (full RetainedRope skills/tools as spans) | Item #1 — rope spans are the event stream |
| **mu-di4** (throbber for ProviderStatusSnapshot, LANDED) | §D state-icon orthogonal axes; cache ratio display from §A |
| **mu-stw** (AWS capability runtime enforcement, LANDED) | Item #4 — rules half is done; modes half is the next bead |
| **mu-2w7** (TUI design-exploration researchers) | §D — agent-view layout |
| **mu-yc6** (MCP support) | Item #6 — MCP tools as a context source in compaction table |
| **mu-zh1** (TUI pattern reference shelf) | §D — agent-view as a reference |
| **mu-f6z** (semantic code tool — LSP + treesitter) | Tools-reference §LSP — code-intelligence plugin pattern |
| **mu-8mj** (web-fetch as MCP) | Tools-reference §WebFetch — lossy-by-design pattern |
| **mu-m80** (file-watching kqueue/inotify) | Tools-reference §Monitor — line-by-line model feedback |
| **mu-s6t** (diff/patch editing primitive) | Tools-reference §Edit — read-before-edit + uniqueness checks |
| **mu-3aa** (OpenAIProviderRenderer) | §A — cache rules are provider-agnostic; OpenAI provider needs its own analog |
| **mu-iwq** (P1, current task) | Possibly §A or §D — verify against bead body |
| **mu-26x** (PR-flow convention) | Item #9 — slash-command-IS-skill rule |

## Insights-report cross-references

(From `~/.claude-personal/usage-data/report-2026-05-21-044803.html`)

- **Friction #1 (output-token-limit blocking transcripts, 9+ sessions)**:
  addressed by Claude Code's Bash overflow-to-file pattern. Apply mu's tool-
  output handling AND to the model's own output.
- **Friction #2 (symptom-vs-root-cause, 31 incidents)**: addressable as a
  `PreToolUse` hook recipe that injects a hypothesis-before-action reminder.
- **Friction #3 (stale-base PR work, ≥2 corrections)**: hook on `git push *`
  that runs `git fetch && git log origin/main..HEAD`. Harden the existing
  CLAUDE.md guidance into enforcement.
- **Confirmed parallel pattern (11% of messages overlap with parallel
  sessions, 47 sessions involved in 38 overlaps)**: validates §D as not-optional.
- **Top-tool distribution (Bash 10122, Edit 1780, Read 1755)**: Bash dominates
  5.4x over Edit. Bash polish (overflow-to-file, wrapper stripping, compound
  command awareness, read-only allowlist) directly affects 67% of tool calls.

---

# Part 4 — Suggested bead breakdown

Recommended ordering when creating beads from this document. Items are sized
roughly small / medium / large.

## Wave 1 — Foundation (1-2 weeks)

1. **mu-cache-contract** (§A) — encode invariants from §A into
   `AnthropicCacheStrategy`. The doc IS the spec; little design work needed.
   Add `cache_creation_input_tokens` / `cache_read_input_tokens` ratio to
   ProviderStatusSnapshot. (medium)
2. **mu-worktree-hooks** (§B) — implement WorktreeCreate/Remove hook contract;
   ship jj+pot adapter as the reference implementation. Smallest scope, highest
   "this is now real" payoff. (small-medium)
3. **mu-otel-spans** (§C minimal slice) — emit `claude_code.interaction`-style
   spans with `session.id` attribute. Even without an exporter wired up, the
   span structure costs nothing and the join key to task_log is worth it day one.
   Use the prefix `mu.*` instead of `claude_code.*` if you want clean attribution. (small)

## Wave 2 — Memory + sessions (2-3 weeks)

4. **mu-rules-paths** (item #2) — path-scoped rules under `.claude/rules/`
   with `paths:` frontmatter. (medium)
5. **mu-rewind-picker** (item #3) — 6-action rewind picker UI; pre-edit
   snapshots underneath. Can split into two beads. (medium-large)
6. **mu-permission-modes** (item #4) — modes layer over existing rule set.
   Plan / acceptEdits / default at minimum. (medium)
7. **mu-compaction-semantics** (item #6) — write the compaction-behavior table
   as a spec doc first. Implementation follows. (small spec, medium impl)

## Wave 3 — Multi-session UX (3-4 weeks)

8. **mu-supervisor** (§D supervisor) — formalize per-user daemon + per-session
   state files. Likely largest single bead. (large)
9. **mu-agent-view** (§D dashboard) — TUI for multi-session visibility. Depends
   on supervisor. (medium-large)
10. **mu-context-window-explorer** (item #1) — rope-span renderer with event-
    kind tags. Depends on mu-nat. (medium)

## Wave 4 — Polish

11. Hook lifecycle event names + `additionalContext` JSON protocol (item #7)
12. Plan mode + session-name-from-plan (item #8)
13. Bash polish: overflow-to-file, wrapper stripping, compound-command splitting
14. Skill `disable-model-invocation` flag (item #10)
15. `/btw`, `/diff`, `/focus`, `/effort` UX commands (Tier 3 items)

## Notes for the bead-creator

When creating beads, each item above should reference back to this document by
path. The Wave-1 items already have implementation specs inline (§A, §B, §C);
Wave-2+ items will need spec drafts before implementation.

The cross-reference table in Part 3 shows several existing beads that overlap
with items here. **Don't duplicate — extend existing beads when the scope
matches.**

---

# Provenance

- Research session: 2026-05-21, claude-personal account
- Source: 16 pages from `code.claude.com/docs/en/...` listed above
- Insights report referenced: `~/.claude-personal/usage-data/report-2026-05-21-044803.html`
- Memory anchor: see agent.sqlite, search "claude-code-feature-mapping"
