# `orchestrate.sh` architecture-only dogfood — 2026-08-12

| field | value |
| --- | --- |
| scope | read-only architecture consensus for session-context orchestration |
| pipeline | manual reproduction of `orchestrate.sh` early phases |
| implementation requested | no |
| tracking | `mu-orchestrate-pipeline-refinement-gjr6` |

## Why the pipeline was reproduced manually

`scripts/orchestrator/orchestrate.sh` currently runs a fixed sequence:

```text
SPEC-CRITIC -> ARCHITECT -> PLAN -> IMPLEMENT -> REVIEW -> ADJUDICATE
```

It can skip the first two gates, but cannot stop before implementation. The task required multiple architecture reviews, synthesis, a spec, and a bead graph without runtime implementation. The session therefore dispatched three read-only workers manually and reconciled their output.

## Outcome

The multi-role reasoning pattern materially improved the architecture:

- the skeptical reviewer identified the existing N=1 worker-return durability gap as a prerequisite to `JoinAll` and pointed to mu-046's command/receipt discipline as the pattern to reuse;
- the architecture critic separated existing substrate from new work, corrected overclaims about loop determinism, distinguished `SessionRef` routing from sub-session content addressing, and made adoption information-only;
- the implementation planner connected the proposal to existing `mu-x9j`, `mu-m7x`, epoch-capsule retention, `capability-event-log-trust.md`, and the decided NATS/Router/Biscuit direction.

The resulting spec is `specs/architecture/session-context-orchestration.md`; rollout is tracked by `mu-session-context-orchestration-rurq`.

## What worked

1. **Independent roles found different defects.** The reviews were complementary rather than three paraphrases.
2. **Read-only critics could inspect broad terrain cheaply.** Existing specs and beads prevented invention of parallel mechanisms.
3. **Late reconciliation remained possible.** The planner completed after the first synthesis; its substantive additions could still be folded into the spec and graph.
4. **The spec-first boundary held.** No runtime implementation files were changed.

## What failed or required manual recovery

### No architecture-only mode

The early pipeline phases are independently useful, but the script has no mode/preset for them. A useful surface should include:

```text
--mode=architecture
--mode=all                 # default/backward-compatible
```

and a composable phase mechanism such as `--through`, `--phases`, or individual phase flags. Invalid combinations must be rejected: for example, REVIEW without an implementation artifact. The resolved phase graph should appear in the run summary.

Individual `--architecture` / `--implementation` flags are attractive only if their semantics are graph-based rather than boolean soup. Presets plus an explicit phase selector give both the common operation and the escape hatch.

### Worker result loss at the abstraction boundary

The substantive planner output existed in a Claude-specific transcript. The durable parent mailbox received only:

> Delivered: the plan (5 tracks / 19 atomic commits...)

Recovering the plan required locating and parsing `~/.claude/projects/...jsonl`. Provider-specific transcript archaeology must not be part of orchestrator correctness. A worker terminal result must contain or durably reference the actual result artifact, with provider/model/session provenance and typed terminal status.

This repeats a goal-protocol lesson: verify the artifact/diff, not the worker's narrative that it delivered one.

### Duplicate barriers and wake storm

Several overlapping watches were registered while waiting for the same workers. When the final worker exited, every watch emitted a completion wake. The runtime had no named idempotent dispatch barrier that each assignment could satisfy exactly once.

A dispatch needs one durable identity, membership, typed terminal outcomes, deadline, reconstructable join projection, and one barrier wake.

### Weak progress classification

For several minutes the only useful status was that a PID remained alive. The coordinator could not distinguish:

- legitimate slow work;
- provider wait/stall;
- tool or nested-subagent work;
- completed work whose result delivery failed;
- dead process.

Typed progress/status and explicit deadlines are required before unattended consensus runs are trustworthy.

### Late result policy was improvised

The first synthesis proceeded after two workers while the planner appeared stuck; the planner completed shortly afterward. Architecture consensus needs an explicit policy: strict join, deadline then partial synthesis, or early synthesis with a later amendment phase. Timing should not silently decide membership.

## Proposed refinement

Architecture mode should be a first-class graph:

```text
SPEC-CRITIC
  -> N read-only architecture critics/planners
  -> one named join barrier
  -> SYNTHESIZE (preserve evidence and disagreements)
  -> operator review
```

Properties:

- no implementation workspace is created;
- workers receive read-only tools;
- every worker writes a complete provider-neutral result artifact;
- the join barrier is durable and idempotent;
- failures/timeouts are members of the joined result, not missing messages;
- synthesis cites terrain and records unresolved forks;
- the operator can promote the accepted architecture into a later implementation run.

## Goal-protocol lineage

The predecessor methodology is preserved on `10.1.1.172` under:

- `~tcovert/.claude-personal/skills/goal-protocol/`;
- `~tcovert/.claude-personal/experiments/goal-*.md`;
- `goal-2026-06-16-arch-bench-and-goal-protocol.md`;
- `postmortem-2026-06-16-arch-bench-goal-protocol.md`.

The current repo also has `specs/process/goal-protocol.md`.

Load-bearing lessons to retain while reconciling stale substrate details:

- preregister goals, success conditions, abort conditions, auth, spend, and work graph;
- smoke-test the actual dispatch path before committing the full budget;
- isolate and claim worker workspaces;
- verify diffs/artifacts/tests rather than terminal self-reports;
- distinguish semantic stop criteria from outer budget/time caps;
- checkpoint durable state before timeout;
- produce a briefing and postmortem, then refine the protocol from evidence;
- route subscription-covered and metered models deliberately.

Several old goal-protocol statements describe tooling that has since changed. The history is evidence, not a current runbook; terrain-check it before adoption.

## Beads

Epic `mu-orchestrate-pipeline-refinement-gjr6` tracks:

1. phase selection and architecture/all presets;
2. multi-review consensus and synthesis;
3. complete provider-neutral worker result artifacts;
4. one durable idempotent join barrier;
5. actionable progress/stall status;
6. architecture-only regression dogfood;
7. consolidation of goal-protocol history into the current runbook.
