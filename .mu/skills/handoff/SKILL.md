---
name: handoff
description: Generate a compact session handoff file for restarting or switching sessions
when_to_use: Only when the operator explicitly invokes /handoff or asks for a session handoff; do not suggest proactively.
disable-model-invocation: true
runtime: ["mu"]
---

Generate a compact session handoff block for switching to a different session.

**Write the block to a file — do not paste it into the terminal.** The operator
runs fullscreen TUIs inside zellij, where alt-screen content is invisible to the
multiplexer's scrollback, so copy-pasting a printed block is painful and unreliable
across the session restart a handoff requires. A file sidesteps the terminal entirely:
the next session reads it with an `@`-reference. (For ad-hoc extraction that does
NOT cross a restart, `/copy` and `/export` are simpler — they push straight to the
X11 clipboard via xclip/OSC52, fullscreen or not. The file path is specifically for
the handoff, which the clipboard may not survive.)

## Pre-handoff hygiene

Before generating the block, do these in order:

1. **Update stale tracker state.** If any bead, spec, or doc you touched this session has a description that no longer matches actual code/state, fix it now (`br update <id> --description ...` or edit the spec). The handoff is a poor place to flag staleness — the source-of-truth is.
2. **Write durable memories.** Anything worth surviving past this session goes to `agent memory add` BEFORE the handoff is produced. Categories: feedback (corrections/validations from operator), project (multi-session initiative state), reference (external resource pointers). Don't save: code patterns, debug recipes, conversation history.
3. **Reach a clean breakpoint.** Either land the current work (merged or abandoned) or explicitly mark it `in_progress` with a "what's left" note in the handoff. No dangling subprocess, no half-applied edits.
4. **Release every claim this session holds — describe first.** (Policy decided 2026-06-05: a held claim that never gets reclaimed is a SILENT failure — the bead looks taken and work stalls invisibly. A released claim loses nothing under jj: commits live in the shared repo store, workspaces are disposable views, so a successor — same operator restarting, or a different actor — re-claims and finds the commits.) For each bead claimed by THIS session's actors (check `br list --status in_progress`; never touch other actors' claims):
   - In its sprint workspace: `jj describe` anything in flight. A described change is a safe change; working-copy-only state is the ONLY thing release can lose. sprint-end refuses on a dirty tree, which enforces this mechanically.
   - `sprint-end` from the workspace (default = unclaim + teardown; `--close` only if the work actually landed).
   - Record in the handoff block: bead id, the change-ids of in-flight commits, one line of "what's left".
   Crash caveat: this step only runs on graceful exits. Stale claims from crashed sessions need liveness-bound claims (agent-slot lease + takeover) — a tooling gap, not a handoff-discipline gap.

## Steps

1. Run in parallel:
   - `date`
   - In the primary working directory: `jj log --limit 5 --no-graph -T 'change_id.short(8) ++ " " ++ description.first_line() ++ "\n"'` (fall back to `git log --oneline -5` if not a jj repo)
   - `jj st` (or `git status --short`)
   - `jj diff --stat` (or `git diff --stat`)
   - `agent memory recent --n 5` (memories saved this session)
   - `task_log query --cwd $PWD --days 1 --limit 5`

2. Pull live memory context: `agent memory context --cwd $PWD` — read the project-relevant memories so the handoff can name standing policy, recent learnings, and stable conventions accurately.

3. Fill every field of the block below. Write "none" if a field doesn't apply — do not omit fields. Be specific over polished: the next-me needs operational facts, not narrative.

4. Write the completed block to a file:
   - `mkdir -p ~/handoffs`
   - Write the full block to `~/handoffs/handoff-{YYYY-MM-DD-HHMM}.md` (timestamp from the `date` call), then ALSO write the identical content to `~/handoffs/latest.md` (a plain copy, not a symlink — keep it trivially readable by either account).
   - Print ONLY this pointer to the terminal (nothing else, no block, no summary).
     Use the CONCRETE timestamped filename, not latest.md — when multiple handoffs
     land in a short window, latest.md gets overwritten and races the resume; the
     stamped name is unambiguous. latest.md remains the fallback for "lost my place":
     ```
     Handoff written → ~/handoffs/handoff-{stamp}.md  (latest.md also updated)
     Resume in the new session:  @~/handoffs/handoff-{stamp}.md   then say "Continue from the handoff."
     ```

---

```
## Session Handoff — {DATE}

**Switching:** {current session/runtime} → {next session/runtime}
Runtime/accounts: {e.g. mu-solo, claude-personal, claude/work, or other relevant shell/runtime}

**Working dir:** {absolute path}

**Standing operating policy:**
{Long-lived goals/priorities the operator has stated explicitly this thread, or that come from memory and are still active. E.g., "Axis 1 (highest): Claude efficiency + orchestration. Axis 2: mu-tui daily driver." If none stated, write "none — operator drives task selection."}

**Recent commits:**
{3-5 lines: short-hash description}

**Uncommitted changes:**
{file list or "none"}

**Claims released:**
{Per bead this session held: `<bead-id> — released; in-flight commits: <change-ids>; what's left: <one line>`. The successor re-claims via sprint-start and finds the commits. "none held" if no claims.}

**Last action:**
{one sentence}

**Next task:**
{one sentence — what the operator asked for, or the explicit on-deck item. If task selection is open, say so.}

**Key files:**
{up to 5 paths, one per line}

**Surprises / plan changes:**
{Anything you learned this session that contradicted prior assumptions — stale beads found, scope larger/smaller than expected, dead-end explorations to NOT redo. One line each. "none" if uneventful.}

**Memories written this session:**
{Memory IDs + 1-line gist for each, so next-me knows to re-ground. E.g., "55e68ae9 — feedback: detect stop-suggestion pattern as context-load signal". Pull from `agent memory recent`.}

**Open questions / blockers:**
{Things explicitly deferred or unresolved. Distinct from next task. "none" if clean.}

**Don't re-do:**
{Investigations, refactors, or scopes the prior session ruled out — saves next-me from relitigating. "none" if nothing dead-ended.}

**Project context:**
{2-3 sentences from memory: what this codebase is, what problem we're solving, any constraints or current state. Operational, not narrative.}

**To resume:**
1. cd {working dir}
2. Start the next session/runtime (mu-solo, claude, claude-personal, etc.). Session-start hook auto-injects `agent memory context`; if it doesn't, run `agent memory context --cwd $PWD` manually.
3. Type `@~/handoffs/handoff-{stamp}.md` then say "Continue from the handoff." (No copy-paste — the new session reads this file directly. This stamped name is THIS handoff specifically; `@~/handoffs/latest.md` works as a fallback but races when several handoffs land close together.)
```
