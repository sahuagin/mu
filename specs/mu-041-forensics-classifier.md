# Spec: forensics classifier — outcome categorization

| field      | value                                       |
| ---------- | ------------------------------------------- |
| spec_id    | mu-041                                      |
| status     | proposed                                    |
| created    | 2026-05-16                                  |
| updated    | 2026-05-16                                  |
| authors    | tcovert + claude                            |
| supersedes | none                                        |
| beads      | mu-8alb (this), mu-5g7i (envelope, landed), mu-8ypx (sink, follow-up) |

## Why

`EventPayload::TaskTelemetry` (mu-5g7i) captures the raw facts of every task
end. The forensics axis needs to turn those raw facts plus contextual evidence
(commit content, PR/CI status) into discrete **outcome classes** that analytics
can aggregate over.

This spec adds a pure classifier function. **No I/O, no emission, no CLI**:
those are the caller's responsibility (today: tests; future: mu-8ypx's
`mu analytics` subcommand).

## Outcome classes

From the 2026-05-16 overnight incident analysis:

| Class                  | Discriminator                                                |
| ---------------------- | ------------------------------------------------------------ |
| `clean_success`        | exit Done + claimed files ⊆ diff + CI green                  |
| `cosmetic_failure`     | exit Done + CI fmt-only-fail                                 |
| `bug_in_output`        | exit Done + CI test/clippy fail                              |
| `hollow_commit`        | exit Done + commit claims files NOT in diff (mu-nk3 mode)    |
| `lying_state`          | exit Done + claimed PR # doesn't exist on origin (mu-2jg)    |
| `narrative_no_action`  | exit Done + zero file modifications + narrative completion (codex mu-el3c) |
| `budget_halted`        | `TaskExitReason::BudgetCap`                                  |
| `error_exit`           | `TaskExitReason::Error`                                      |
| `timeout`              | `TaskExitReason::Timeout`                                    |
| `operator_intervention`| `TaskExitReason::Cancelled` or `OperatorStopped`             |
| `unclassified`         | none of the above match (escape hatch — never silently mis-classify) |

## Confidence levels

Classification is best-effort. Each result carries a confidence:

- `Definite` — discriminator is unambiguous (e.g. exit_reason == BudgetCap)
- `Probable` — discriminator inferred from heuristics that have known false-
  positive shapes (e.g. claim-parser regex extracted file names from a commit
  message)
- `Inferred` — fallback class, no discriminator strongly matched

## Inputs

```rust
pub struct ClassificationInputs<'a> {
    pub telemetry: &'a TaskTelemetry,
    /// Commit info — None if no commit was produced (narrative_no_action case).
    pub commit: Option<&'a CommitInfo>,
    /// PR + CI state — None if no PR was claimed or PR lookup hasn't run.
    pub pr: Option<&'a PrInfo>,
}

pub struct CommitInfo {
    pub sha: String,
    pub message: String,
    /// Files that actually changed in the commit. Set on disk.
    pub diff_files: Vec<String>,
}

pub struct PrInfo {
    /// PR number claimed in state.json or commit message.
    pub claimed_number: Option<u32>,
    /// Whether the claimed PR exists on origin. None when not yet checked.
    pub exists_on_origin: Option<bool>,
    /// CI status when present.
    pub ci: Option<CiStatus>,
}

pub struct CiStatus {
    pub fmt_pass: bool,
    pub clippy_pass: bool,
    pub test_pass: bool,
}
```

`TaskTelemetry`, `CommitInfo`, `PrInfo`, `CiStatus` live in `mu_core::forensics`.
`TaskTelemetry` is re-exported from `event_log` for ergonomics.

## API

```rust
pub fn classify_task(inputs: &ClassificationInputs) -> Classification;

pub struct Classification {
    pub outcome: Outcome,
    pub confidence: Confidence,
    /// One-line human-readable rationale (for debugging false positives).
    pub rationale: String,
}
```

## Discriminator order

The classifier tries discriminators in order, returning the first match:

1. Terminal-state exits (BudgetCap, Error, Timeout, Cancelled, OperatorStopped) —
   `Definite` confidence, directly from `exit_reason`.
2. `narrative_no_action` — `Done` exit, no commit at all OR commit with empty
   diff_files. `Definite` when no commit; `Probable` when commit exists but
   diff is empty (theoretically possible with empty-but-non-bogus commit).
3. `lying_state` — Done + commit + `pr.claimed_number.is_some()` +
   `pr.exists_on_origin == Some(false)`. `Definite`.
4. `hollow_commit` — Done + commit + commit message claims files (per the
   claim-parser) that are NOT in `commit.diff_files`. `Probable` (claim parser
   has false positives).
5. CI-discriminated classes (clean_success / cosmetic_failure / bug_in_output) —
   require `pr.ci` to be set. Otherwise fall through to `unclassified`.
6. `unclassified` — escape hatch, `Inferred` confidence.

## Claim parser (hollow_commit detection)

Narrow regex set, expanded based on real false positives:

- `\bcrates/[a-z][a-z0-9_-]*/(?:src|tests)/[a-zA-Z0-9_/.-]+\.rs\b` — Rust files in workspace crates
- `\bspecs/[a-zA-Z0-9_-]+\.md\b` — spec markdown
- `\bscripts/[a-zA-Z0-9_-]+\.sh\b` — shell scripts
- `\bjustfile\b`, `\bREADME\.md\b`, `\bCargo\.toml\b` — top-level files

Pattern set lives as a `const` so additions are easy and visible.

False-positive notes:
- Quoted file names in prose: caught (good, often a legit claim)
- Plural file references (e.g. "the parser files"): not caught (no path, no match)
- Old file paths in a rename: false positive — currently accepted; if it becomes
  noisy, add a renamed-files denylist sourced from `git log --name-status`

## Acceptance (mu-8alb)

- [x] `mu_core::forensics` module with the types + function above
- [x] `Outcome`, `Confidence`, `Classification` enums/structs serde-compatible
- [x] `classify_task` pure function — no I/O, no panics on adversarial input
- [x] Tests for each outcome class with synthetic fixtures shaped after the
  2026-05-16 incident modes
- [x] `cargo test --workspace` green
- [x] `pre-pr-check.sh` green

## Out of scope (deferred)

- `mu-8ypx` — sink + `mu analytics` CLI that *calls* classify_task
- `mu-mk9l` — backfill 2026-05-16 overnight (also calls classify_task)
- Git/gh I/O to populate `CommitInfo` / `PrInfo` from disk — caller's job
- Inline emission of classification results into the event log
- Renamed-files denylist (deferred until false-positive data warrants it)
