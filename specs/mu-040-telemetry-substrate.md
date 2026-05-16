# Spec: TaskTelemetry event variant — telemetry substrate foundation

| field      | value                                       |
| ---------- | ------------------------------------------- |
| spec_id    | mu-040                                      |
| status     | proposed                                    |
| created    | 2026-05-16                                  |
| updated    | 2026-05-16                                  |
| authors    | tcovert + claude                            |
| supersedes | none                                        |
| beads      | mu-5g7i (this), mu-8alb, mu-8ypx, mu-mk9l   |

## Why

mu's `EventPayload::Done` records turn-level termination but not the full envelope
needed for cross-task analytics (provider/model attribution, wall-clock budget,
tool surface granted vs used, exit-reason classification). The mu-fvy0 umbrella
calls this the "blotter / compliance" axis — every task end must produce a
queryable record sufficient for outcome classification and pattern detection.

This spec adds the **envelope** + **emission rule**. Classification (mu-8alb),
storage (mu-8ypx), and pattern detection (mu-sfmd) are separate beads that build
on this foundation.

## What this is NOT

- Not a wire-protocol notification — `session.done` already exists for that.
- Not a classifier — the variant carries raw facts; mu-8alb infers
  outcome categories from them.
- Not a sink/analytics surface — mu-8ypx adds the `mu analytics` subcommand.
- Not a budget-enforcement mechanism — `actual_spend_usd` and `max_budget_usd`
  are reported when known; enforcement is its own bead (part of mu-fvy0).

## Envelope

Added to `mu-core::event_log::EventPayload`:

```rust
TaskTelemetry {
    /// Time-sortable task identifier. Distinct from session_id because a
    /// session may run many tasks (Done events).
    task_id: String,
    /// Session this task ran in.
    session_id: String,
    /// Parent task id when this is a delegate-spawn child. None for root tasks.
    parent_task_id: Option<String>,

    /// Provider + model identification (best-effort from the SessionCreated
    /// event recorded earlier in this session's log).
    provider_kind: String,
    model: String,
    /// Provider may not expose model version; carry when available.
    model_version: Option<String>,

    /// Time-of-task instrumentation. wall_clock_ms is the canonical duration;
    /// started_at_unix_ms / ended_at_unix_ms give absolute placement.
    started_at_unix_ms: Option<u64>,
    ended_at_unix_ms: u64,
    wall_clock_ms: Option<u64>,

    /// Aggregated usage from the Done event being projected. None when the
    /// provider didn't report usage (faux, transient errors).
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,

    /// Tool surface — what was authorized + what actually ran. Populated
    /// from session state when accessible; otherwise empty Vec (which means
    /// "not captured", not "no tools").
    tools_granted: Vec<String>,
    tools_actually_called: Vec<(String, u32)>,

    /// Why the task ended. Mirrors AgentEvent terminal categories.
    exit_reason: TaskExitReason,

    /// Budget axis. None when budget tracking isn't wired (v1: always None
    /// pending the budget-ledger bead).
    max_budget_usd: Option<f64>,
    actual_spend_usd: Option<f64>,

    /// Time-of-day instrumentation for pattern analysis. local_hour is
    /// 0..=23; day_of_week is Mon=0..Sun=6 (ISO); tz is IANA name when
    /// derivable, else fixed-offset string.
    local_hour: Option<u8>,
    day_of_week: Option<u8>,
    tz: Option<String>,
}
```

New companion enum:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskExitReason {
    Done,
    Error,
    Cancelled,
    BudgetCap,
    Timeout,
    OperatorStopped,
}
```

## Emission rule

**Every task termination emits exactly one `TaskTelemetry` event.** This is a
hard rule, not a feature toggle. The forwarder (which already projects
`AgentEvent` → `EventPayload`) synthesizes the envelope at the same boundary
where it emits `EventPayload::Done` / `EventPayload::Error`.

When a field is unknowable at emission time (e.g. provider didn't expose
token counts), emit with the field set to `None` / empty `Vec` rather than
suppressing the event. Downstream consumers must be tolerant of nulls.

## MVP slice (what v1 actually populates)

Today the forwarder has straightforward access to:

| Field                  | v1 population                                          |
| ---------------------- | ------------------------------------------------------ |
| `task_id`              | UUID v4 (replace with v7 once that dep is added)       |
| `session_id`           | from the forwarder's session_id                        |
| `parent_task_id`       | None (delegate parent-task lineage not wired yet)      |
| `provider_kind`/`model`| from `event_log.provider_info()` (SessionCreated event)|
| `model_version`        | None                                                   |
| `started_at_unix_ms`   | from the most-recent `UserMessage`/turn start          |
| `ended_at_unix_ms`     | now (`SystemTime::now()`)                              |
| `wall_clock_ms`        | from Done.elapsed_ms when present, else computed       |
| `prompt_tokens`        | from Done.usage when present                           |
| `completion_tokens`    | from Done.usage when present                           |
| `cache_read_tokens`    | from Done.usage when present                           |
| `cache_write_tokens`   | from Done.usage when present                           |
| `tools_granted`        | empty Vec (session-state plumbing is follow-up work)   |
| `tools_actually_called`| empty Vec (likewise; counts can be derived from log)   |
| `exit_reason`          | from AgentEvent (Done/Error/Cancelled mapped)          |
| `max_budget_usd`       | None                                                   |
| `actual_spend_usd`     | None                                                   |
| `local_hour`/`dow`/`tz`| None (chrono not in workspace; follow-up bead)         |
| `task_id`              | `format!("task-{}", SystemTime::now-as-nanos)` (sortable; UUID v7 once that dep lands) |

Fields left at `None`/empty are NOT a bug. They are the contract that follow-up
beads (tool-surface plumbing, budget ledger, delegate parent linkage) will fill
without needing to renegotiate the envelope shape.

## Acceptance criteria (mu-5g7i)

- [x] `EventPayload::TaskTelemetry { ... }` variant defined and serde-compatible.
- [x] `TaskExitReason` enum defined.
- [x] Forwarder emits exactly one `TaskTelemetry` per task termination.
- [x] Tests verify emission shape across at least three exit paths
  (Done, Error, plus one more — Aborted maps to Cancelled in v1).
- [x] No breaking changes to existing `EventPayload` consumers (only an additive
  variant; serde tagged-enum is forward-compat).
- [x] `cargo test --workspace --all-features --no-fail-fast` green.
- [x] `scripts/pre-pr-check.sh` green.

## Out of scope (deferred to dependent beads)

- mu-8alb: outcome classifier (clean/dirty/hollow/lying/narrative) reads
  TaskTelemetry events + the surrounding event log.
- mu-8ypx: telemetry sink + `mu analytics` subcommand reads them too.
- mu-mk9l: backfill the 2026-05-16 overnight as inaugural dataset.
- mu-sfmd: pattern-detected pre-emptive judge invocation (closes the
  forensics loop).
- Tool-surface field population (`tools_granted` / `tools_actually_called`)
  beyond empty Vec — needs session-state plumbing.
- Budget ledger (`max_budget_usd` / `actual_spend_usd`) — separate axis under
  mu-fvy0.
- Delegate parent-task linkage (`parent_task_id`) — needs session
  delegation-tree threading.
