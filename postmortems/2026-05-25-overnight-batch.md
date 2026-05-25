# Overnight goal batch — 2026-05-25 postmortem

**Authors:** claude-personal (claude-opus-4.7)
**Last reviewed:** 2026-05-25
**Scope:** Four overnight autonomous /goal sessions dispatched 2026-05-25 ~02:08 UTC.

## Executive summary

Four autonomous goal sessions were dispatched overnight. Two produced useful output (mu-0q44 commit, mu-slat 645-line design doc). One succeeded but its changes were invisible in the commit graph (mu-gdwd — code landed in working copy, not a separate commit). One failed after 28 turns to an API error (mu-sy02 — `redacted_thinking` block corruption). The operator didn't learn any of this until asking ~8 hours later. Every one of these outcomes was preventable with infrastructure we already had and lessons we'd already learned.

## Scope and impact

- **Affected:** primary mu repo working copy, 4 budget allocations ($42 total cap)
- **Blast radius:** no data loss, no repo corruption. mu-gdwd's code is recoverable from the working copy. mu-sy02's work is lost (3 lines changed, negligible).
- **Cost:** ~$32 estimated across all four sessions. mu-sy02's ~$8 was largely wasted.
- **Opportunity cost:** overnight compute time that should have produced 4 clean PRs instead produced 1 commit + 1 design doc + 1 invisible working-copy change + 1 failure.

## Timeline

```
2026-05-25T06:08Z — Batch launcher started. mu-gdwd begins on host working copy.
2026-05-25T07:08Z — mu-gdwd finishes (timeout). ToolArgs implemented, tests pass,
                     bead closed. Changes in working copy, no separate commit.
2026-05-25T07:08Z — mu-sy02 begins on SAME working copy (now dirty with mu-gdwd's
                     uncommitted changes).
2026-05-25T07:09Z — mu-sy02 reads the goal text, begins reading skill files.
~2026-05-25T07:20Z — mu-sy02 hits API Error: 400 redacted_thinking block invalid.
                      Only 28 assistant turns. 3 lines changed in viewport.rs.
2026-05-25T08:08Z — mu-sy02 timeout fires. Exit 0.
2026-05-25T08:08Z — mu-0q44 begins on SAME working copy (now dirty with both
                     mu-gdwd + mu-sy02 residue).
2026-05-25T08:19Z — mu-0q44 creates a new jj change, implements the fix, commits
                     as tcovert-c137[bot]. Clean commit at llwtvrrw.
2026-05-25T08:38Z — mu-0q44 timeout fires. Exit 0.
2026-05-25T08:38Z — mu-slat begins. Research only — reads specs, writes design doc.
2026-05-25T09:38Z — mu-slat timeout fires. Exit 0. Design doc at specs/mu-slat-design.md.
~2026-05-25T14:00Z — Operator asks "what landed?" First awareness of outcomes.
```

## Contributing factors

### 1. Host execution instead of pot isolation (known-solved problem)

The launch scripts used `claude-code --print` directly on the host instead of `agent-spawn-v2` in isolated pots. We have validated pot infrastructure (agent-spawn-v2, agent-runtime template, slot pool, all verified working 2026-05-24 with 5 provider paths). We used pots for the code-index LSP research worker launched 30 minutes later in the same session. The host path was chosen for "simplicity" despite the pot path being equally available and operationally superior.

**Why this happened:** The orchestrator (me) took the path of least resistance. Writing a `claude-code --print` launch script is fewer lines than wiring `agent-spawn-v2` with the right env vars. The pots were mentioned by the operator ("if there are conflicts, you can review and decide to switch to agent-spawn-v2. I fixed things yesterday and added some features.") but I didn't use them from the start.

**Prior art ignored:** Every prior overnight worker dispatch (spline-connect Tier 1, mu-nbi0 attempts, the code-index research worker) used pots. The lesson "pots for isolation" was already learned and documented in the freebsd-jails skill. I didn't apply it.

### 2. Shared working copy with no workspace isolation

All four workers ran sequentially on the same jj working copy (`default@`). No `jj new`, no `jj workspace add`, no branch creation. Worker N inherited worker N-1's uncommitted changes as ambient state. This directly caused:

- mu-sy02's `redacted_thinking` error (inherited mu-gdwd's large diff as context)
- mu-gdwd's invisible commit (changes accumulated in working copy instead of a named revision)

**Why this happened:** The launch scripts `cd /home/tcovert/src/public_github/mu` and invoked claude-code without any jj setup. The goal texts said "use bot-jj for commits" but didn't say "first create a clean workspace" or "run jj new before starting work."

**What should have happened:** Each worker should have either:
- Run in a pot (inherits a clean clone via ZFS snapshot), OR
- Run `jj new main -m "wip: <bead-id>"` as its first action, OR
- Used `jj workspace add` to get an isolated workspace

### 3. No pre-work clean-state verification

None of the goal texts included "verify the working copy is clean before starting." The jj-runbook skill (which we converted to native format hours earlier in this same session) has this as a standing rule: first command in a surprising state is `jj op log`. But the goal texts didn't reference the jj-runbook skill or require clean-state verification.

**Why this happened:** The goal text template focuses on what to implement, not on workspace hygiene. The goal-protocol skill's "Standing rules of engagement" say "one bead per commit" and "atomic commits" but don't say "verify clean working copy at session start."

### 4. No model ever commits the work independently

mu-0q44 was the only worker that created a separate commit. The others left changes in the working copy. This is a recurring pattern with autonomous claude-code sessions — the model reads the instruction to "use bot-jj for commits" but doesn't reliably execute the commit step, especially when it's the last action before timeout.

**Why this happened:** The timeout is a hard kill. If the model is mid-thought when the timeout fires, the commit never happens. Additionally, `bot-jj` had a known TOML parse error with the `[bot]` suffix (mu-gdwd's log shows this), which may have discouraged the model from retrying.

### 5. Exit code 0 is meaningless for goal sessions

`timeout 3600 claude-code --print ...` exits 0 whether the goal succeeded, failed, hit budget, or timed out. The batch script's `|| true` further masks failures. The batch log shows `exit: 0` for all four, conveying zero information about outcomes.

**Why this happened:** The launch script was written for "don't crash the batch" safety, not for "report outcomes." There's no post-run step that checks what actually happened.

### 6. No notification or structured outcome reporting

The only way to learn outcomes was to parse multi-megabyte stream-json logs. No MORNING briefing was written (despite the goal-protocol requiring one). No mailbox message posted (despite mailboxes being implemented). No result.json sidecar. No push notification.

**Why this happened:** The batch runner is a 20-line shell script that runs commands and logs output. It has no awareness of goal-protocol conventions, no post-run hooks, and no integration with mu's mailbox system.

## What went well

- **mu-0q44 landed cleanly.** The worker created its own jj change, implemented the fix, committed via bot-jj, and produced a well-described commit. This proves the pattern CAN work.
- **mu-slat produced a substantial design doc.** 645 lines, well-structured, directly useful for the 6/15 planning. Research-only tasks are good overnight candidates.
- **mu-gdwd completed its implementation.** The ToolArgs newtype, Eq restoration, and all 833 tests passing is real work. The failure was in commit hygiene, not implementation quality.
- **The batch runner didn't crash.** Sequential execution, per-task timeout, log capture all worked mechanically.
- **The operator explicitly offered the better path.** "if there are conflicts, you can review and decide to switch to agent-spawn-v2" — the information was available; I failed to act on it.

## How it was resolved

- mu-gdwd's changes are in the working copy and can be extracted into a proper commit
- mu-0q44 is committed and clean
- mu-slat design doc is written and useful
- mu-sy02 needs to be re-run (in a pot, with a clean workspace)

## Counterfactuals

- **If pots had been used from the start,** each worker would have had an isolated ZFS clone. No cross-contamination, no shared working copy, no `redacted_thinking` error from inherited context. mu-sy02 would likely have succeeded.
- **If the goal texts had included `jj new main` as a first step,** mu-gdwd would have its own commit instead of invisible working-copy changes.
- **If mailbox integration existed in the batch runner,** the operator would have seen 4 structured status messages in the morning instead of parsing logs.
- **If the timeout had a pre-kill hook** (e.g., 5-minute warning → force commit → exit), mu-gdwd would have committed its work before the hard kill.

## Remediation

### Tactical: this week

- [ ] Rewrite the overnight batch launcher to use `agent-spawn-v2` per worker — owner: claude-personal — by: next overnight batch
- [ ] Add `jj new main -m "wip: <bead-id>"` to the goal text template as a mandatory first step — owner: claude-personal — by: next goal session
- [ ] Add a post-run step to the launch script that writes a structured `result.json` (beads touched, files changed, exit reason) — owner: claude-personal — by: next overnight batch
- [ ] Extract mu-gdwd's working-copy changes into a proper commit — owner: claude-personal — by: today
- [ ] Re-run mu-sy02 in a pot with a clean workspace — owner: claude-personal — by: next overnight batch

### Strategic: prevent the category

- [ ] Add "verify clean working copy" to the goal-protocol skill's standing rules — owner: tcovert — by: next skill update
- [ ] Build a `goal-dispatch` script that wraps agent-spawn-v2 + goal-protocol pre-flight + structured outcome reporting + mailbox post-completion notification — owner: tcovert/claude-personal — by: 2026-06-01
- [ ] Add the `redacted_thinking` API error to tool-conventions.md as a known failure mode with mitigation (retry with fresh context) — owner: claude-personal — by: next session
- [ ] Update the goal-protocol skill to require MORNING briefing as the FIRST action on session start (record "I am starting work on X") not just on stop — bookend pattern

### Visionary

- [ ] mu-solo `/goal` command that handles all of this: pot isolation, clean workspace, structured reporting, mailbox notifications, budget tracking, live status via throbber
- [ ] Fleet monitor TUI (memory `0671083d`) — overnight workers appear as live-status rows, not log files to parse

## Open questions

- **Was mu-sy02's `redacted_thinking` error caused by inherited context from mu-gdwd, or would it have happened anyway?** Gates whether pot isolation alone fixes this, or whether the Anthropic API has a deeper issue with `--print` + `/goal` + extended thinking.
- **Why did mu-0q44 create a separate commit but mu-gdwd didn't?** The goal texts were structurally identical. Examining the logs would reveal whether mu-0q44 explicitly ran `jj new` or whether it was coincidental.
- **Should the goal-protocol skill mandate pot-based execution for overnight/unattended work?** Currently it's agnostic about execution environment. The evidence says unattended = isolated.

## Provenance

- Batch log: `~/.claude-personal/experiments/logs/overnight-batch-2026-05-25.log`
- Per-task logs: `~/.claude-personal/experiments/logs/goal-2026-05-25-mu-{gdwd,sy02,0q44,slat-exploratory}.log`
- Experiment docs: `~/.claude-personal/experiments/goal-2026-05-25-mu-{gdwd,sy02,0q44,slat-exploratory}.md`
- Launch scripts: `~/.claude-personal/experiments/goal-2026-05-25-*.launch.sh`
- Working copy with mu-gdwd changes: jj revision `yqllnzmx`
- mu-0q44 commit: jj revision `llwtvrrw` (7614135c)
- mu-slat design doc: `specs/mu-slat-design.md`
