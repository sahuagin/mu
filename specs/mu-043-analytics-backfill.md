# Spec: analytics backfill — documentary historical inserts

| field      | value                                       |
| ---------- | ------------------------------------------- |
| spec_id    | mu-043                                      |
| status     | proposed                                    |
| created    | 2026-05-16                                  |
| updated    | 2026-05-16                                  |
| authors    | tcovert + claude                            |
| supersedes | none                                        |
| beads      | mu-mk9l (this), mu-5g7i (envelope, landed), mu-8alb (classifier, landed), mu-8ypx (sink, landed) |

## Why

The analytics sink (mu-8ypx) populates from live `TaskTelemetry` events
forward in time. The first real qualitatively-rich dataset — the 2026-05-16
overnight worker run that produced PRs #49–#53 — predates the sink. Without
backfill, the analytics CLI starts with no signal until enough new tasks
accumulate; with backfill, day 1 of the analytics CLI has all the major
failure modes (clean_success, bug_in_output, hollow_commit, lying_state,
narrative_no_action) already visible.

This spec adds a `mu analytics backfill` subcommand that inserts
**pre-classified historical entries** into the sink. Unlike `compact`, it
does NOT call `classify_task` — the caller specifies the outcome class
directly. Outcomes for historical entries are determined by the operator
(who knows ground truth from PR review, commit content, and recall), not
by a classifier running over partial data.

## What this is NOT

- Not for live data — `compact` is the live path.
- Not a classifier replacement — backfill is the "we know what this was"
  path, classification is the "compute outcome from observable evidence"
  path. Each lives in its own context.
- Not freeform — backfill entries are TOML-described with a fixed schema.

## CLI

```
mu analytics backfill --preset <NAME> [--db PATH]
mu analytics backfill --input <PATH>  [--db PATH]
```

Exactly one of `--preset` / `--input` is required.

Presets are TOML files embedded in the binary via `include_str!`. v1 ships
exactly one preset: `overnight-2026-05-16` (described below). New presets
land as separate beads when new historical datasets are worth ingesting.

## TOML schema

```toml
description = "free-form description of the dataset"

[[tasks]]
task_id        = "backfill-overnight-2026-05-16-r94"   # caller picks; must be unique
session_id     = "overnight-2026-05-16"                # logical group
provider       = "anthropic_oauth"                     # plain string; not validated
model          = "claude-opus-4-7"                     # plain string; not validated
exit_reason    = "done"                                # required, must parse to TaskExitReason
outcome_class  = "clean_success"                       # required, must parse to Outcome
outcome_confidence = "definite"                        # default: "definite"; values: definite|probable|inferred
rationale      = "PR #49 merged"                       # optional; lands in tasks.rationale

# Optional envelope fields — populated when ground-truth data exists,
# null otherwise per the "documentary; don't fabricate" discipline.
parent_task_id     = "backfill-overnight-2026-05-16-supervisor"   # optional
model_version      = "claude-opus-4-7"                            # optional
started_at_unix_ms = 1778762257696                                # optional
ended_at_unix_ms   = 1778762447323                                # required (sink schema NOT NULL)
wall_clock_ms      = 189627                                       # optional
prompt_tokens      = 12345                                        # optional
completion_tokens  = 678                                          # optional
cache_read_tokens  = 1024                                         # optional
cache_write_tokens = 256                                          # optional
```

## Discipline

- **Documentary only.** Backfill entries describe historical reality as
  the operator understood it at PR/commit-review time. Fabricated fields
  are not better than null. When wall_clock_ms isn't recoverable, leave
  it null.
- **Pre-classified.** The operator (or the dataset author) decides
  `outcome_class` and `outcome_confidence` based on the artifacts they
  reviewed. The classifier is NOT invoked.
- **UPSERT semantics match `compact`.** Re-running backfill produces the
  same rows; the task_id is the primary key.

## Acceptance (mu-mk9l)

- [x] `mu analytics backfill --preset overnight-2026-05-16 --db PATH` works
  end-to-end against an empty sink
- [x] After running the preset, `mu analytics summary` shows: 2
  clean_success, 2 bug_in_output, 1 hollow_commit, 1 lying_state, 1
  narrative_no_action = 7 total
- [x] `--input <toml>` works for an external file (test exercises this path)
- [x] Invalid TOML / unknown outcome_class produces a clear error, not a
  silent skip
- [x] UPSERT is idempotent — re-running the same preset gives the same
  rows, not duplicates
- [x] Tests for: TOML parsing, row conversion, end-to-end CLI smoke,
  preset content matches the bead-table outcomes
- [x] `cargo test --workspace` green
- [x] `scripts/pre-pr-check.sh` green

## Out of scope (deferred)

- Per-task tool-surface fields (`tools_granted`, `tools_actually_called`)
  — sink schema doesn't carry them yet; orthogonal to backfill mechanism
- Cross-reference back to GitHub PRs (a `pr_url` field on backfill entries
  could land later for richer reporting)
- Multiple presets in one binary — current design embeds one; a registry
  + `--list-presets` is straightforward to add when we have a second
