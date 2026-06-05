# mu-console: web operator console (first slice)

Bead: `mu-mu-console-operator-web-jg9p.1`

## Purpose

`mu-solo` should stay the keyboard cockpit: prompt, stream, approve,
navigate, recover orientation. It should not become a bad browser.

`mu-console` is the richer read-only operator projection over the same mu
substrate: sessions, event timelines, context assemblies, compaction audits,
and cost/cache economics. The web console is also the reference projection for
views that may later get small TUI affordances.

First principle split:

- `mu-solo`: show what changes the operator's next action.
- `mu-console`: show enough to explain what happened.

## V1 constraints

- Read-only. No chat/control endpoints in the first slice.
- Local-first: default bind should be loopback only.
- Event log / analytics are truth; pages are projections.
- Prefer boring server-rendered HTML first. Add HTMX/light JS only where it
  pays rent.
- Do not require a live daemon for historical inspection; raw JSONL and the
  analytics DB are sufficient for v1.
- Nginx-compatible: support a configurable base path so this can live under a
  route such as `/mu-console/` or `/mu-stats/console/`.

## Placement recommendation

Add a `mu console` subcommand in `crates/mu-coding` rather than a new crate for
v1.

Why:

- `mu-coding` already owns CLI modes, default event paths, analytics DB path,
  and the main `mu` binary.
- The first console is a projection over existing `mu-coding`/`mu-core` data,
  not a reusable frontend crate yet.
- A later split to `mu-console` crate is easy once the route/data model proves
  itself.

Suggested module layout:

```text
crates/mu-coding/src/console/
  mod.rs              # run(ConsoleOptions)
  routes.rs           # route construction / handlers
  data.rs             # read raw JSONL + analytics summaries
  render.rs           # server-rendered HTML helpers/templates
  views/
    sessions.rs
    session.rs
    cost.rs
    context.rs
    compaction.rs
    compare.rs
```

CLI shape:

```text
mu console \
  --bind 127.0.0.1:8765 \
  --base-path /mu-console/ \
  --events-dir ~/.local/share/mu/events \
  --analytics-db ~/.local/share/mu/telemetry.sqlite
```

`--events-dir` and `--analytics-db` should default to the same locations used
by `mu analytics compact` today.

## Data sources

### Raw event JSONL

Primary source for:

- session existence when analytics has not been compacted
- transcript projection
- event timeline
- `ContextAssembly`
- `CompactionAssembly`
- tool calls/results
- provider status events
- memory/callout events
- worker events

Existing API:

- `mu_core::event_log::SessionEventLog::from_jsonl(path)` parses one session
  JSONL into typed `SessionEvent`s.
- `SessionEventLog` already exposes useful projections such as
  `provider_info`, `started_at_unix_ms`, `last_activity_unix_ms`,
  `ask_count`, `context_assembly_count`, `tool_call_count`, `live_usage`, and
  `cumulative_usage`.

The console should add filesystem scanning on top:

```text
~/.local/share/mu/events/<daemon_id>/<session_id>.jsonl
```

For v1, scanning can be synchronous at request time; optimize later if needed.

### Analytics SQLite

Primary source for:

- task/session cost summaries when compacted
- provider/model/outcome grouping
- normalized task-level usage/cost-ish telemetry
- cross-runtime imported data if already projected there

Existing API:

- `mu_coding::analytics::default_db_path()`
- `mu_coding::analytics::sink::open(path)`
- `mu_coding::analytics::query::{summary, rate_hallucination, ...}`

Current schema is task-level (`tasks`) rather than full provider-call/session
comparison. V1 console should use it opportunistically and clearly mark missing
or stale analytics. Do not block raw event-log inspection on analytics being
present.

### Future data-source likely needed

The cost/cache page and compare page will eventually want a call-level
projection table, because task-level `TaskTelemetry` is too coarse to explain
cache behavior per provider call.

Candidate future projection:

```text
provider_calls(
  source_runtime,
  daemon_id,
  session_id,
  model_call_id,
  provider_kind,
  model,
  started_at_unix_ms,
  prompt_tokens,
  output_tokens,
  cache_read_tokens,
  cache_write_tokens,
  cache_write_5m_tokens,
  cache_write_1h_tokens,
  uncached_input_tokens,
  estimated_cost_usd,
  context_assembly_event_id,
  compaction_assembly_event_id,
  prefix_hash
)
```

Do not build that first unless terrain says it is already almost present.
Start by rendering what the events already contain.

## Route shape

All routes live under configurable `base_path`.

```text
GET /                         redirect/index
GET /sessions                 session index
GET /sessions/:daemon/:id     session detail: transcript + event timeline
GET /sessions/:daemon/:id/events
GET /sessions/:daemon/:id/cost
GET /sessions/:daemon/:id/context
GET /sessions/:daemon/:id/context/:model_call_id
GET /sessions/:daemon/:id/compactions
GET /sessions/:daemon/:id/compactions/:model_call_id
GET /compare?left=...&right=...
GET /healthz
```

Session handles should include daemon id because session ids are not globally
unique.

For imported Claude Code sessions, use a synthetic source/runtime namespace if
the data source exposes one. If not yet available, the first console can be
mu-event-log-only and the comparison bead can extend it.

## First shippable vertical slice

### Slice 1: server + session index

- `mu console` starts an Axum server.
- `/healthz` returns OK.
- `/sessions` scans event logs and renders a table.
- Each row links to `/sessions/:daemon/:id`.
- Missing/malformed logs are skipped with a visible warning count.

This proves placement, routing, config, and event-log scanning.

### Slice 2: session detail

- Transcript projection from typed events.
- Event timeline with event id, timestamp, actor, payload kind.
- Expandable raw JSON/details.
- Links to context/cost/compaction routes where data exists.

### Slice 3: cost/cache

- Render session-level usage from events.
- Render per-`Done` and/or `AssistantMessageEvent` usage rows where available.
- Distinguish provider-reported vs estimated fields.
- Show cache-read/write/tier fields when present.
- Warn when usage semantics are missing/pre-`mu-rf9x`.

### Slice 4: ContextAssembly explorer

- List all context assemblies by `model_call_id`.
- Show token estimate, span count, tool count, renderer, cache strategy,
  prefix hash, cache boundary count.
- Show token breakdown table.
- Link to matching compaction if present.

### Slice 5: CompactionAssembly explorer

- List compactions by `model_call_id`.
- Show policy, tokens before/after, wall-clock, decision counts.
- Expand full decisions with truncation for large content.

## Follow-on bead adjustments

The existing child beads are still the right sequence:

1. `.2` Axum skeleton + nginx-friendly local serving
2. `.3` read-only session index page
3. `.4` session detail with transcript + event timeline
4. `.5` cost/cache economics page
5. `.6` ContextAssembly explorer
6. `.7` CompactionAssembly explorer
7. `.8` compare two replay/session runs
8. `.9` mu-solo links/commands to open web-console views

Recommended tweak: `.5` should first use raw event usage and current analytics;
if call-level comparison proves necessary, file a separate analytics projection
bead rather than burying schema work in the web UI.

## Open questions

- Should `mu console` auto-run `mu analytics compact` or only read existing
  projections? V1 recommendation: do not auto-compact; expose stale/missing
  analytics clearly.
- Should the console read imported Claude Code sessions directly, or only via a
  normalized analytics/projection DB? V1 recommendation: only support whatever
  normalized source already exists; don't make web routing own import logic.
- HTML rendering choice: hand-built strings, `maud`, or `askama`? V1
  recommendation: inspect cached dependencies / existing style, then choose the
  smallest dependency that keeps escaping safe.
- Authentication: v1 loopback-only. Remote access requires a separate AAA bead
  (biscuits/macaroons/bootstrap), not ad hoc web auth.
