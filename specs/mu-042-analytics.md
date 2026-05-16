# Spec: analytics sink + `mu analytics` subcommand

| field      | value                                       |
| ---------- | ------------------------------------------- |
| spec_id    | mu-042                                      |
| status     | proposed                                    |
| created    | 2026-05-16                                  |
| updated    | 2026-05-16                                  |
| authors    | tcovert + claude                            |
| supersedes | none                                        |
| beads      | mu-8ypx (this), mu-5g7i (envelope, landed), mu-8alb (classifier, landed), mu-mk9l (backfill, follow-up) |

## Why

mu-5g7i emits `TaskTelemetry` events at task end. mu-8alb classifies them.
Neither makes that data actionable — it sits in per-session JSONL event-log
files. This spec adds the **sink** (queryable SQLite projection) and the
**CLI** (`mu analytics`) that surfaces patterns: how often does each
provider/model hallucinate, what's the wall-clock distribution, what shifted
this week from baseline.

This spec is intentionally narrow. Pattern detection (drift, correlation) is
deferred until enough data accumulates to make the queries useful.

## Sink

Location: `~/.local/share/mu/telemetry.sqlite` (sibling to the existing
`events/` directory). Created on first compact; rusqlite is already a
workspace dep.

Schema (denormalized for query convenience):

```sql
CREATE TABLE tasks (
  task_id              TEXT PRIMARY KEY,
  session_id           TEXT NOT NULL,
  parent_task_id       TEXT,
  provider             TEXT NOT NULL,
  model                TEXT NOT NULL,
  model_version        TEXT,
  started_at_unix_ms   INTEGER,
  ended_at_unix_ms     INTEGER NOT NULL,
  wall_clock_ms        INTEGER,
  prompt_tokens        INTEGER,
  completion_tokens    INTEGER,
  cache_read_tokens    INTEGER,
  cache_write_tokens   INTEGER,
  exit_reason          TEXT NOT NULL,
  outcome_class        TEXT NOT NULL,
  outcome_confidence   TEXT NOT NULL,
  rationale            TEXT
);
CREATE INDEX idx_tasks_provider_model ON tasks(provider, model);
CREATE INDEX idx_tasks_ended_at ON tasks(ended_at_unix_ms);
CREATE INDEX idx_tasks_outcome ON tasks(outcome_class);
```

Tool-surface columns (`files_added`, `lines_added`, etc. from the bead) are
deferred — the v1 TaskTelemetry envelope doesn't carry them yet (mu-5g7i §MVP
slice). When upstream fields gain values, this schema gains columns via
`ALTER TABLE`; downstream queries treat missing columns as `NULL`.

## Projection model

A standalone subcommand reads event-log JSONLs and projects:

```
mu analytics compact [--events-dir PATH] [--db PATH] [--since UNIX_MS]
```

For each `TaskTelemetry` event encountered:
1. Build a `TaskTelemetrySnapshot` from envelope fields.
2. Call `forensics::classify_task` with `commit=None, pr=None` (v1 limitation
   — commit/PR enrichment is a follow-up bead; today's classifier produces
   accurate classes for all terminal exits and `NarrativeNoAction` for
   Done-with-no-commit-info).
3. UPSERT into `tasks` by `task_id` (idempotent — re-running compact is safe).

This deliberately does NOT run inline in `mu serve`. Reasons:
- mu serve stays untouched (we touched it twice already this week).
- Backfill story works the same as live: compact reads JSONLs, doesn't care
  whether the events were written 5s ago or 5d ago.
- Re-running compact after a classifier change re-classifies all history.

## CLI

```
mu analytics compact [--events-dir PATH] [--db PATH] [--since UNIX_MS]
mu analytics summary [--db PATH] [--since UNIX_MS]
mu analytics rate    [--db PATH] [--metric METRIC] [--by FIELDS] [--since UNIX_MS]
```

Defaults: `--db ~/.local/share/mu/telemetry.sqlite`, `--events-dir
~/.local/share/mu/events`, `--since 7d-ago` (parsed liberally; for v1 accept
only absolute unix-ms, document the 7d default in `--help`).

### `summary`

Totals + breakdown by exit_reason / provider+model / outcome_class. Tabular
text output. Example:

```
mu analytics summary --since 1778000000000
total tasks: 47

by exit_reason:
  done                  42
  error                  3
  cancelled              2

by provider/model:
  openrouter/deepseek/deepseek-v4-flash    38
  anthropic_api/claude-opus-4-7             9

by outcome:
  narrative_no_action  Definite  35
  error_exit           Definite   3
  operator_intervention Definite  2
  unclassified         Inferred   7
```

### `rate --metric hallucination`

Hallucination rate = (`hollow_commit` + `lying_state` + `narrative_no_action`)
/ total `Done` tasks, grouped by `--by` (default `provider,model`):

```
mu analytics rate --metric hallucination --by provider,model
provider             model                        rate    (hallu / done)
openrouter           deepseek/deepseek-v4-flash   92.1%   35 / 38
anthropic_api        claude-opus-4-7               0.0%    0 / 9
```

(In v1 most "hallucinations" will be `narrative_no_action` since commit info
isn't enriched yet. The follow-up bead that adds git/gh shell-out makes the
discrimination useful.)

`--metric` is an enum (v1: only `hallucination`). `--by` accepts comma-
separated field names from the schema. Validation rejects unknown fields.

## Discipline

- All queries are PRESET. No freeform SQL CLI; that's a maintenance trap.
- Output is text-tabular by default; JSON output is a follow-up if scripts
  need it.
- Defaults are READ-ONLY. `compact` writes, but only to the analytics DB —
  never mutates event-log JSONLs.

## Acceptance (mu-8ypx)

- [x] `crates/mu-coding/src/analytics/` module with sink/compact/query split
- [x] `mu analytics {compact, summary, rate}` CLI working end-to-end
- [x] Sink schema created on first compact (idempotent)
- [x] Compact is idempotent (re-run produces the same rows; UPSERT by task_id)
- [x] `summary` and `rate --metric hallucination` work on synthetic data
- [x] Tests for the projection logic, sink writes, and both queries
- [x] `cargo test --workspace` green
- [x] `scripts/pre-pr-check.sh` green

## Out of scope (deferred)

- Inline projection in `mu serve` — compact is the only writer in v1
- Commit/PR enrichment via git/gh — separate bead (call site: compact's
  classify call); landing this makes `rate --metric hallucination` discriminate
  between `hollow_commit` / `lying_state` / `narrative_no_action`
- Additional metrics (`rate --metric error`, `rate --metric timeout`)
- `drift`, `correlation`, `export` subcommands
- Tool-surface columns in schema (waiting on telemetry envelope to grow them)
- mu-mk9l backfill (separate bead, consumes this sink)
