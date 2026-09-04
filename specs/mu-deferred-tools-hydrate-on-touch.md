# mu-t4l5e — deferred tool schemas with hydrate-on-touch

Status: implemented 2026-09-04 (see Implementation notes). Bead mu-t4l5e. Evidence and survey: agentic-bench
`harness/battery/plan-ab/research/` (PR #6), battery results in that README.

## Problem

Every session ships every tool's full schema on every request (`loop_/mod.rs`
`tool_specs = tools.iter().map(|t| t.spec())`), ~9.4 KB for the default twelve,
plus 11.6 KB of MU.md/AGENTS.md. Twenty-one runs across qwen3.8-27b and gpt-5.5
with that context made zero `discover` calls: a tool that is always available is
not thereby used, and prose telling the model to discover first has no measured
effect on either model. The only discovery mechanisms with evidence on weak
models are ones where the harness selects (RAG-MCP, Less-is-More) or where the
need is created at the point the model acts (a call to a not-yet-loaded tool).

mu already has the harness-side selector: `[index].discover_injection`
(`context/capability_hints.rs`, mu-uz0n / mu-0x5i) ranks the last user message
through the `discover` ranker each turn and injects an anchored ~100-token
pointer span. What it cannot do is change what is in the tool list, because
nothing is deferred.

## Design

Three tiers of session tools, decided at session creation and mutable only by
loading (never unloading) during the session:

- **core** — always in the tool list with full schemas. Default:
  `read, write, edit, ls, grep, glob, bash, final_answer, discover`.
- **deferred** — granted (in `Capability.allowed_tools`) but the schema is
  withheld. The model sees one compact System span, byte-stable until a load:
  `Deferred tools (call one by name to load it, or ask discover): watch,
  spawn_worker, mailbox, memory_recall, aws_recon, code_recall …` at ~10-20
  tokens per tool. Default: every non-core tool, including MCP-imported and
  mesh-consumed tools.
- **loaded** — deferred tools promoted for the rest of the session (or the
  compaction epoch); their schemas join `tool_specs` from the next request.

Two ways a deferred tool gets loaded:

1. **Pre-selection at turn start.** Run the existing `capability_hints` ranker
   (`RankOptions` with `min_score_ratio`, dedup against context) on the last
   user-role message; deferred tools that qualify, up to `preselect_limit`, are
   loaded before the request is built. This is the RAG-MCP shape: the model
   never has to ask.
2. **Hydrate-on-touch.** In `execute_tools.rs`, the `tools.iter().find` miss
   branch checks the deferred set before the near-miss suggestion. A hit loads
   the tool and executes the call in the same round: `Tool::validate` runs on
   the model's arguments; valid → normal execution; invalid → an error result
   that carries the tool's schema ("`watch` is now loaded; arguments did not
   validate: …; schema: …"), so the retry is informed. No round trip is spent on
   a bare "not loaded" refusal, which is the failure Claude Code's deferred
   tools show (models skip the search step because they know the name).

`discover` marks deferred entries `(deferred — call by name to load)` in its
result text. Authority is unchanged: deferral is presentation, the capability
grant and the effects gate keep their full sets.

## Config (keys, not flags)

```toml
[session]
defer_tools = false          # opt-in until the battery shows no regression
core_tools  = ["read", "write", "edit", "ls", "grep", "glob", "bash", "final_answer", "discover"]

[index]
preselect       = true       # only meaningful with defer_tools; reuses the discover ranker
preselect_limit = 5
```

`--bare` sessions keep today's behaviour unless `defer_tools` is set.

## Event log

Loading is state, so it is an event first (invariant 1): `ToolLoaded { name,
reason: "preselect" | "touch" }` appended before the in-memory loaded set
changes; rehydration replays it. The deferred-manifest span is derived, not
stored.

## Cache

The tool list is part of the provider prefix; each load is one cache miss for
that session. Loads happen only at turn boundaries (pre-selection) or as the
last step of a tool round (touch), never mid-request, and never unload, so the
prefix is stable between loads. The manifest span sits after the system prompt
and before recall spans so a load rewrites the smallest possible suffix.

Provider specifics:

- **Anthropic.** `mu-ai`'s Anthropic adapter attaches `cache_control` to the
  LAST tool, so the tool list is a cached-prefix segment: any change to it
  between turns is a prompt-cache miss unless the request carries the
  `mid-conversation-tool-changes-2026-07-01` beta header (Fable 5 / Mythos 5 /
  Opus 4.8 / Opus 5). That header is being added by a separate change; with it,
  loads are cache-free on those models precisely because they happen only at
  turn boundaries and never unload. The loop therefore rebuilds the list only
  at turn start; a touch during a tool round takes effect from the next
  request. Under the header the one rewrite a load still makes is the manifest
  span itself (it names only what is still withheld, so it shrinks, and it
  lives in the system block). A session-static manifest (every deferred name,
  loaded or not) would make a load fully cache-free; left as a follow-up until
  the battery says the redundancy is harmless.
- **OpenAI.** `mu-openai` already models `defer_loading` on tools
  (`crates/providers/mu-openai`, request types). Follow-up: on OpenAI lanes,
  harness-deferred tools could be sent with `defer_loading` instead of being
  omitted, so the provider-side search sees them. Harness-side deferral stays
  the provider-agnostic path — mu has no tool-search type for Anthropic.

## Attachment points

- `serve/handlers/session.rs` ~921-940: build core / deferred sets from config
  after `session_spawn_tools` and the `discover` push; pass a shared
  `Arc<Mutex<LoadedSet>>` (or add it to the capability handle) into the loop and
  `DiscoverTool`.
- `agent/loop_/mod.rs` ~2309: filter `tool_specs` by core ∪ loaded; emit the
  manifest span in assembly (`context/assembly.rs`) as a `System`-kind span.
- `agent/loop_/mod.rs` turn start: pre-selection through
  `capability_hints::rank_for_turn` (or its inner ranker) restricted to deferred
  names.
- `agent/loop_/execute_tools.rs` ~930-955: hydrate-on-touch before the
  near-miss branch.
- `config.rs`: the four keys above with defaults and doc comments.
- `mu ask`/`serve` plumbing: none new; `--tools` keeps naming the granted set.

## Implementation notes

- The handle is `mu_core::agent::deferred_tools::DeferredTools` (deferred set
  + loaded set + pre-selection knobs), built in `build_and_register_session`
  and shared by `AgentConfig::deferred_tools` and `DiscoverTool`. The
  manifest is `with_manifest` post-processing over the assembled rope — one
  `System` span, id `deferred-tools`, replaced in place on the
  compaction-baseline path so a stale copy never duplicates.
- Hydrate-on-touch hooks the FOUND-tool path in `execute_tools.rs`, not the
  not-found arm the design named: deferred tools stay in the loop's `tools`
  (authority is unchanged), so a call by name resolves. The touch loads and
  then falls through the ordinary gates; the validate arm formats the
  schema-carrying error when the call was a touch. A capability refusal takes
  precedence over the load.
- `ToolLoaded` travels like every other loop state event: `AgentEvent` on the
  loop's channel (sent BEFORE the set mutates), mapped by the forwarder to the
  durable `EventPayload::ToolLoaded { name, reason }`. `continuation::
  project_strict` collects the names into `Continuation::loaded_tools`; the
  resume handler seeds the new head's handle from it. Loads persist across a
  context clear.
- Pre-selection ranks lexically (`t4c_source::discover_view`, host catalog
  included) with the floor relative to the turn's best hit overall — a turn
  about `read` does not load whichever deferred tool is least irrelevant. It
  reuses `[index].discover_injection_min_score_ratio` as the ratio and runs
  once per user message per context epoch.

Review-board follow-ups (PR #600 panel):

- `preselect_candidates` takes the session capability and asks it exactly
  what the dispatch gate asks. `build_manifest_for_tools` drops what the
  grant (`check_allow`) denies; `discover_view_constrained` ranks under the
  session's `effective_constraints()`, so a capability the posture forbids
  comes back `allowed_by_session: false` and `select_candidates` drops it
  before computing the score floor (it cannot anchor the floor either); and
  `session_permits` — the predicate lifted out of the dispatch gate in
  `loop_/execute_tools.rs`, covering `check_allow` +
  `check_effects(derived_effects())` + the required-AWS grant — is re-run per
  surviving candidate. The first cut filtered on `check_allow` alone and
  ranked unconstrained, so a posture-restricted session could pre-load a
  schema that gate would then refuse (PR #600 panel, both dissenting seats).
- Resume writes one `ToolLoaded { reason: "inherited" }` row per inherited
  name onto the NEW head's log; the in-process seed alone died at the second
  generation, since resuming a resumed head projected no `ToolLoaded`.
- `final_answer` and `discover` stay core whatever `[session].core_tools`
  says (`DeferredTools::ALWAYS_CORE`) — withholding the turn's exit or the
  affordance the manifest points at breaks the tier rather than shrinking it.

## Measurement

Plan-ab rig (`agentic-bench harness/battery/plan-ab`, `ROLE=` lanes): a task
whose solution needs a deferred tool (e.g. `watch`, or an MCP tool) run with
`defer_tools` off vs on; metrics per run: `token_breakdown.tool_schema` from the
event log, ToolLoaded events by reason, wasted calls (invalid-args after touch),
pass rate. Expected: schema tokens fall from ~2.4K to ~1K on the default set;
the deferred tool is reached by touch or pre-selection with no discover prose.

## Non-goals

Skill bodies on demand (memo 01 attachment point 5), a plan tool, changes to
`discover_injection` defaults.
