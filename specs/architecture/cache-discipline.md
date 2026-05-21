# Cache discipline

A coherent contract for mu's prompt-cache behavior. Captures the conceptual model, the operational invariants, the current implementation state (post-mu-yqeq.8), and the file-as-memmap direction that may inform future bead work.

This document is reference material — not a spec for an unbuilt feature. The pieces that are implemented are noted; the pieces that aren't are flagged.

## Relationship to `claude-code-feature-mapping.md`

The sibling doc [`claude-code-feature-mapping.md`](claude-code-feature-mapping.md) (c137 research output, 2026-05-21, memory anchor `f3c61b6b`) contains in §A a comprehensive **cache-discipline contract** derived from Claude Code's documentation. That §A is the canonical source for the invariants, invalidator/non-invalidator tables, TTL details (including the `ENABLE_PROMPT_CACHING_1H` / `FORCE_PROMPT_CACHING_5M` env-var controls), and the operating rules for live-loop adoption.

**This document complements that one rather than duplicates it.** It adds:

- The **memory-segmentation mental model** (tcovert's framing of program/tools/conversation regions analogous to stack/heap/static) that motivated mu's rope structure
- **Hierarchical marker strategies** (using markers 3 and 4 of the 4-marker budget for patterns like append-tool-without-invalidating)
- **The file-as-memmap direction** — v1 hash-check, v1.5 watchman, future AST via code_index — sketched as a multi-tier roadmap
- A **mu-specific implementation checklist** distinguishing what landed in mu-yqeq.8 from what's still open
- Cross-references to mu's actual code, commits, and beads

When the two documents agree (the invalidator table, TTL discipline, observability metrics), `claude-code-feature-mapping.md` §A is the authoritative source — it has the Claude Code doc URLs as provenance. When they cover different ground (the mental model, file-as-memmap, mu's state), this document is the source.

## Motivation

Prompt caching is the load-bearing optimization that makes long agent sessions economically viable. Anthropic's caching ([docs](https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching)) lets a request mark prefix boundaries via `cache_control: ephemeral`; subsequent requests with the same prefix get a 10× cost reduction on those tokens and a substantial latency win. Used well, it turns multi-turn coding sessions from "every turn pays for the whole history" into "every turn pays for cache_read of the history plus a small delta."

Used poorly, it does nothing — or worse, you keep rebuilding the cache because the prefix drifts.

mu needs a coherent discipline for: where to place markers, what stays stable, what's allowed to mutate, and how to recover when something invalidates the cache.

## Conceptual model — memory segmentation

The mental model that produced mu's rope structure (per tcovert 2026-05-21):

```
                          ┌──────────────────────────┐
top of "stack"  (volatile)│  Conversation turns      │  ← LRU operates here
                          │  Tool results            │  ← drop / summarize candidates
                          │  Memory injections       │
                          │  (Recent assistant text) │
                          ├──────────────────────────┤
                          │                          │
boundary 2  (cache mark)  │ ← Marker on last span    │
                          │   in stable prefix       │
                          ├──────────────────────────┤
                          │  Tool schemas            │  ← stable per-session
                          │  (Tools array contents)  │
                          ├──────────────────────────┤
boundary 1  (cache mark)  │ ← Marker on system span  │
                          ├──────────────────────────┤
                          │  System prompt           │  ← most stable
bottom (program region)   │  (Project context,       │
                          │   skill activations,     │
                          │   memory header)         │
                          └──────────────────────────┘
```

The analogy to program memory:

- **"Program region"** = system prompt + tool schemas. Loaded once at session start. Effectively static for the session's life.
- **"Stack"** = conversation + tool results + injections. Grows during the session. Subject to LRU / compaction.
- **Cache markers** are placed at boundaries between regions, so cache lookups can match prefixes at the appropriate stability granularity.

This is not just an analogy — it dictates that **anything which mutates after session start should be in the conversation region**. Anything in the program region is treated as immutable for the session. Mid-session changes to "program-region" content (a new tool, an updated system prompt, a memory hot-reload) **MUST be handled either as a session reset OR as appended-to-conversation content**, never as mid-stream rewrites of the cached prefix.

## Cache-key invariants

Anthropic's cache is keyed by the literal bytes of the prefix-through-marker. In practice this means the cache key includes:

| Element | Where it lives in the wire | Notes |
|---|---|---|
| Model ID | `body.model` | A model switch = total cache miss |
| Tool name set | `body.tools[].name` | New tool name added → cache miss past last unchanged marker |
| Tool descriptions / schemas | `body.tools[].description, input_schema` | Schema rewrites invalidate downstream cache |
| System prompt text | `body.system[].text` | Any byte change invalidates everything |
| Working directory | Often embedded in system prompt | Two worktrees of the same repo have different cache keys |
| (Provider-specific extras) | e.g. `anthropic-beta` header | Beta-feature opt-in changes routing; behave as invalidator |

Cache lookups are byte-exact through the marker. Two prefixes that differ by a single whitespace character are two distinct cache entries.

## Markers — Anthropic's limits and how mu uses them

Anthropic allows **up to 4 `cache_control` markers per request**. They are hierarchical: a request with markers at A, B, C, D caches four prefixes (`[0..A]`, `[0..B]`, `[0..C]`, `[0..D]`), and on a subsequent request the longest matching prefix wins.

mu's `AnthropicCacheStrategy` (after mu-yqeq.8) uses **2 of 4 markers**:

1. **System span boundary** — caches the system prompt alone. Survives even if tools change.
2. **Last stable-and-cacheable span boundary** — typically the last tool-schema span. Caches system + tools as a single prefix.

The 2-marker shape mirrors the pre-mu-yqeq.8 live-loop annotation (which tagged system + last tool unconditionally). The shape isn't a maximum; it's the minimum that doesn't regress vs the legacy behavior. **There's real headroom to use markers 3 and 4 for more advanced caching strategies** — see the "Hierarchical marker strategies" section below.

## Hierarchical marker strategies (future direction, not implemented)

With 4 markers available, several patterns become viable:

### Pattern: append-tool-without-invalidating-old-tools

When the agent discovers a new tool mid-session, marking the new tool's position **in addition to** the old last-tool keeps both caches warm:

```
Old request:  system[M1] | tool1, tool2, tool3[M2] | conv
New request:  system[M1] | tool1, tool2, tool3[M2], tool4[M3] | conv, new
```

On the new request:
- Cache lookup through `[M2]` HITS — system + tools 1-3 stays cached.
- Cache lookup through `[M3]` MISSES, pays `cache_creation` for just the bytes of tool4.
- The conversation appended after pays normal input rates.

Without this pattern, adding tool4 would require `cache_creation` on the entire system + tools prefix again — expensive on a tools array that's grown large.

### Pattern: rolling-conversation-checkpoint

Mark a stable point in the conversation prefix (e.g., the last span before active work began). System + tools + early-stable-conversation all stay cached across turns; only the active-work portion pays fresh input rates. This is the "/rewind cheaper than /compact" pattern — discarding a side-track means truncating back to a marked conversation checkpoint that's still warm.

### Pattern: program-region cache survives compaction

Compaction rewrites the conversation but should never touch the program region. Markers 1 and 2 (system, tools) stay valid through compaction; only markers 3 and 4 (conversation checkpoints) get invalidated. The agent restarts post-compaction with the program-region cache warm — no re-paying for system prompt or tool schemas.

## Invalidators and non-invalidators

Adapted from the Claude Code prompt-caching documentation (per tcovert's research 2026-05-21). The discipline applies to mu equally.

### Cache invalidators (force recompute on next turn)

| Action | Why |
|---|---|
| Model switch | Cache keyed by model |
| Tool name set changes | Tool names in cached prefix |
| MCP server connect/disconnect | Tool definitions change |
| System prompt mid-session edit | The cached prefix is the system prompt |
| Working directory change | Embedded in system prompt typically |
| Compaction that rewrites the conversation prefix | The cached portion changes |
| Anthropic API version / beta-header changes | Routing changes, behave as invalidators |

### Non-invalidators (cache stays warm)

| Action | Why |
|---|---|
| File edit in repo | Captured as new `read` tool result (appended to conversation); not in cached prefix |
| User message appended | Appended after cache marker |
| Tool call + result appended | Same — appended after marker |
| Skill / command invocation | Appended as user message |
| Spawning a subagent (mu's warden/pi/agent-spawn primitives) | Subagent runs in its own daemon → has its own cache; parent unaffected |
| Permission mode change | Not in prompt text (mu enforces at tool-dispatch time) |

The non-invalidator behavior is what makes mu's session model economically viable. Compaction is the **one** operation in mu that intentionally invalidates the conversation-prefix cache; everything else either appends or operates outside the cached region.

## TTL and timing

Anthropic's ephemeral cache TTL is 5 minutes by default. Each cache-hitting request resets the timer (sliding window). Sustained work keeps the cache warm; idle sessions older than 5 minutes lose the cache.

Other TTL options observed in Claude Code (may or may not be available on direct API):

- 1-hour cache via `anthropic-beta: prompt-caching-extended-2025-04-09` or similar (Anthropic's docs are the canonical source)
- 1-hour TTL is automatic for Claude subscription users (per Claude Code research)
- Subagents always use 5-minute TTL even on subscription

mu currently uses the default 5-minute ephemeral TTL. Going to 1-hour for long-running sessions is a known optimization that's been left on the table.

## Observability — what F5 should show

Per-turn metrics from the API response:

- `cache_creation_input_tokens` — bytes that wrote new cache entries this turn (billed at cache-write rate, ~1.25× input rate)
- `cache_read_input_tokens` — bytes that hit cached prefix this turn (billed at ~0.1× input rate)

The **ratio of `cache_read` to `cache_creation` over turns** is the discipline-violation alarm:

- High cache_read, low cache_creation → cache is working. Sustained reads of a stable prefix.
- High cache_creation, low cache_read → cache is broken or prefix is drifting. Each turn pays to rebuild.
- Both low → session is short or prefix is small.

F5 currently shows aggregate `cache%` (= cache_read / total_input). This is correct for "is caching paying off overall" but misses the **drift signal**: a sustained-creation-no-reads pattern over consecutive turns means the prefix is being invalidated repeatedly — bug, not UX choice.

A future F5 enhancement: per-turn cache_creation/cache_read breakdown, exposing drift over time rather than just aggregate. (Filed informally; not blocking.)

## What mu has, what mu doesn't

### Implemented (post-mu-yqeq.8)

- ✅ Two-marker `AnthropicCacheStrategy`: system + last-stable-cacheable
- ✅ `cache_marker` field on `ProviderMessage` carrying per-message Ephemeral hints
- ✅ Wire emission reads `cache_marker` and tags wire positions accordingly (Anthropic adapter)
- ✅ Rope structure that puts system + tool-schemas before conversation spans
- ✅ Compaction policies (`SpanFamilyDropPolicy`, `HashAndSummaryPolicy`) that operate on conversation spans, leaving system + tools untouched
- ✅ Background-async compaction (`HashAndSummaryPolicy::is_async = true`) so live-judge calls don't block turns
- ✅ F5 aggregate cache% display

### Not implemented / open questions

- ⬜ **Layer-ordering invariant as a runtime assertion**: assemble_rope happens to put system first, but it's not an enforced constraint. A future bead inserting a User span into the program region would silently kill cache. Could be a `#[cfg(test)]` invariant check or a richer span-typing surface.
- ⬜ **Hierarchical marker strategies** (markers 3 and 4): the "append-tool-without-invalidating" and "rolling-conversation-checkpoint" patterns aren't implemented. Real wins on long sessions; not blocking shorter ones.
- ⬜ **`/rewind` primitive**: dropping conversation spans after a marked checkpoint to recover budget without compacting. mu has the cache marker on the rope; this would just consume that marker to know where to truncate. Probably 1-2 hour bead.
- ⬜ **Drift observability**: F5 doesn't currently expose per-turn cache_creation/cache_read trends. Small renderer change to surface this.
- ⬜ **Session-config freeze contract**: mu currently freezes `system_prompt` at session creation by accident (no mid-session update path). Formalizing this as "session config items are immutable; write to next-session config" prevents future foot-guns.
- ⬜ **1-hour TTL opt-in**: long-running mu sessions could use the extended-cache beta if available. Cost calculation needs care (cache-creation at 1h has a higher write rate per Anthropic's pricing).
- ⬜ **File-as-memmap region**: see next section.

## File-as-memmap direction

(From tcovert's 2026-05-21 thread.) Files read into context could be treated as memory-mapped regions rather than arbitrary tool-result content. The mental model: each file → a `FileLoad` span with `(path, content_hash, mtime)` identity. The substrate is partly there — `SpanKind::FileLoad` already exists, and `SpanFamilyDropPolicy` targets it as Tier 1 of its drop ladder.

### v1 shape (file-level granularity, no watcher)

When the `read` tool is invoked with path `P`:

1. Look up existing FileLoad span with `path == P` in the current rope.
2. If found: hash the on-disk file.
   - Hash matches the span's stored hash → reuse the span. No new content added to rope; the tool call is satisfied from existing cache.
   - Hash differs → invalidate the old span, create a new FileLoad span with current content. Old span goes to the event log forever; new one enters the rope.
3. If not found: normal read; new FileLoad span enters the rope.

Benefits:
- The "agent re-reads to confirm state" pattern stops paying re-cache cost when the file hasn't changed.
- The "same file referenced from multiple turns" pattern dedups naturally.
- No new infrastructure needed (no watcher).

### v1.5 shape (watchman integration)

When watchman is available (registered via config like jj does it):

- At session start: register a watchman subscription for the working directory.
- On change notification: mark matching FileLoad spans as `stale: true` (don't remove — assistant turns referencing the file by line number still need the position-in-rope to be coherent).
- Renderer surfaces stale spans to the model as something like `<file X has changed since last read; re-read to see current content>`.
- Without watchman: hash-check-at-read-time still catches drift; you just don't get proactive notification. Graceful degradation.

mu would follow jj's pattern here: watchman is an optional accelerator, not a hard dependency. The `watchman-client` Rust crate handles the protocol.

### Granularity options for future iteration

- **File-level** (v1 above): simplest. Works for most coding-agent workflows where users specify whole files.
- **Segment-level (line ranges)**: invalidate just the changed hunk. Useful when files are huge and changes are localized. Requires diff-tracking but doesn't need language awareness.
- **Semantic segments (AST-aware)**: invalidate just the changed function/class. The existing `~/src/agent_tools/code_index` tool already does AST chunking via tree-sitter for Rust + Python. Integration would consume those chunks as `SpanKind::IndexedChunk` spans. Tracked as bead mu-ks1f.

For v1, file-level is enough. The substrate to extend later is already there.

## Cross-references

### Specs
- `specs/measurements/compaction-2026-05-14.md` — Opus auto-compaction baseline
- `specs/measurements/compaction-2026-05-21.md` — mu policy ladder measurements
- `specs/measurements/compaction-quality-2026-05-21.md` — qualitative side-by-side
- `specs/measurements/compaction-recovery-test-2026-05-21.md` — recovery test, including v2 with tools enabled
- `specs/mu-044-provider-messages-cutover.md` — mu-yqeq epic spec (post-cutover context)

### Code
- `crates/mu-ai/src/context/anthropic.rs` — `AnthropicCacheStrategy` (post-mu-yqeq.8)
- `crates/mu-ai/src/providers/anthropic.rs` — `build_request_body_from_projection` (cache-marker-driven cache_control)
- `crates/mu-core/src/agent/loop_/mod.rs:818` — cutover call site (passes `MessageInput::Projected`)
- `crates/mu-core/src/context/compaction/heuristic.rs` — `SpanFamilyDropPolicy` (drops conversation-region spans, leaves program region)
- `crates/mu-core/src/context/compaction/hash_summary.rs` — `HashAndSummaryPolicy` with `is_async = true`

### Beads
- `mu-yqeq` (closed): the Phase-A through Phase-D cutover that made the rope live
- `mu-gs13` (open, P3): live-API cache_creation_input_tokens fixture for Phase D acceptance
- `mu-ks1f` (open, P3): code_index research test — does pre-loading chunks reduce agent turns/tokens?
- `mu-slat` (open, P2): host Claude Code subprocess sessions inside mu via Zed protocol

### Memories
- `e97a09e0` (feedback): jj auto-snapshot can silently rewrite earlier commits — relevant to maintaining cache-stable session prefixes across bead work
- `b49d681c` (project): mu-yqeq.3 bead-spec divergence resolution
- `be72d65e` (project): compaction recovery test v2 finding — mu's verbatim user-prompt preservation enables tool-based forensic recovery
- `f60fef3a` (project): compaction meta-judge research thread (workload-aware policy selection)
- `c90d163c` (reference): tcovert's local LLM proxy setup
- `ff466a97` (reference): Agent SDK billing change 2026-06-15
