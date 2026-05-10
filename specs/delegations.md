# Delegation Ledger

Append-only record of sub-agent delegations. The point of this file is
not bookkeeping — it's evidence for the routing policy. Patterns
across rows tell us when to delegate, to whom, and what kind of spec
makes a delegation likely to succeed.

## Failure-mode taxonomy

| code | name                       | meaning                                                                                          | fix path                                                                          |
|------|----------------------------|--------------------------------------------------------------------------------------------------|-----------------------------------------------------------------------------------|
| SA   | Spec Ambiguity             | we under-specified; sub-agent made a reasonable choice we disagree with                          | patch spec template / future-spec invariants                                       |
| SC   | Sub-agent Comprehension    | spec was clear; sub-agent paraphrased / reordered / dropped a constraint                          | tighten delegation prompt phrasing                                                 |
| WC   | Workspace Cleanup          | sub-agent modified or restored files unrelated to its task ("helpfully" cleaning up)             | add explicit "don't touch files you didn't author" rule to delegation prompt      |
| CM   | Capability Mismatch        | task is the kind this sub-agent reliably gets wrong (rust async lifetimes, send bounds, etc.)    | update routing rule; route this category elsewhere                                 |
| IC   | Iteration Cap              | sub-agent ran out of iterations mid-implementation                                                | break the spec into smaller pieces; or re-route to a backend with a higher cap     |
| BL   | Blocked (returned cleanly) | sub-agent recognized it couldn't continue and exited with `status: "blocked"` and a useful note  | this is the WORKING case for a hard task — extend with the unblocking info         |
| —    | Success                    | result matches the spec; verification passed; no rework                                          | note what made the spec / prompt / routing work, so we repeat it                   |

A delegation can have multiple failure modes (e.g., SA + SC). Record
all that apply.

## Ledger

| spec_id | attempt | sub-agent           | outcome   | iters | wall_time | files_changed_match | tests_passed | failure_modes | lesson |
|---------|---------|---------------------|-----------|-------|-----------|---------------------|--------------|---------------|--------|
| mu-001  | 1       | codex-oauth/gpt-5.5 | success   | ?     | ~?m       | yes (2/2)           | 11/11        | —             | spec was tight enough that §Invariants / §OOC blocks caught the predictable mistakes (extra derives, wrong rename_all). Reusable: §OOC section is load-bearing for mechanical translation tasks. |
| mu-002  | 1       | codex-oauth/gpt-5.5 | success†  | ?     | ~?m       | yes (2/2)           | 19/19        | WC            | Spec implementation was correct (concurrent tokio code with mpsc + writer task, all 8 §Behaviors covered). BUT pi-rust ran `jj restore specs/delegations.md` mid-task, wiping an unrelated file I had created in the working copy. Lesson: when the working copy contains files from a parallel claude-code session, the sub-agent will "clean them up." Spec said "don't touch other files"; sub-agent interpreted that as "restore other files to their parent state." Different operation, same intent — but it's destructive to in-flight work. See `delegations/mu-002-attempt-1-postmortem.md`. |

`†` = "spec passed acceptance, but operational issue captured separately."

Columns:
- **iters**: iteration count if known (codex-oauth doesn't always
  surface this; leave `?` if not in the output envelope).
- **wall_time**: rough order of magnitude. Helps spot "fast and right"
  vs "slow and right" vs "fast but garbage."
- **files_changed_match**: did the diff touch only the files the spec
  said it would? `yes (N/M)` means N of M expected files, no extras.
  `no` with notes if it touched things it shouldn't have.
- **tests_passed**: P/T or just `pass` if everything green.
- **failure_modes**: zero or more codes from the taxonomy table.
- **lesson**: one sentence. What does this row teach us about
  delegation in general?

## Per-attempt post-mortems

When `failure_modes` is non-empty, also create a short markdown file
under `specs/delegations/` named
`<spec_id>-attempt-<N>-postmortem.md`. The post-mortem has four sections:

1. **What we expected** — restate the spec acceptance.
2. **What we got** — actual diff stat / output envelope.
3. **Why it diverged** — failure mode(s) plus root cause.
4. **What we changed** — spec patch, prompt patch, or routing change,
   with a link to the commit that made the change.

Successes don't need a post-mortem unless something surprising
happened (e.g., "succeeded but in 2x expected wall time" is worth
flagging).

## Routing implications (live)

This section is updated as patterns emerge. Each rule cites the rows
it's based on so it can be revisited when the evidence changes.

- **Delegation prompts MUST include a "don't touch files you didn't
  create" rule.** Evidence: mu-002 attempt 1 (WC). The standard "don't
  touch any other file" wording is insufficient — sub-agents may
  interpret restoring an unrelated file as a benign "clean up your
  workspace" action. Future delegation prompts should say explicitly:
  "If you see files in the working copy that are not in your
  deliverable list, DO NOT modify, restore, or `jj abandon` them. They
  belong to a parallel session."

## Cross-references

- Memory `2da785e5` — current `agent-router` routing policy. Updated
  when this ledger surfaces enough evidence.
- AGENTS.md — multi-agent build flow section.
- task_log entries tagged `mu,delegation`.
