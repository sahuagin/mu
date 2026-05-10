# Post-mortem: mu-002 attempt 1 — workspace cleanup false positive

| field          | value                                |
| -------------- | ------------------------------------ |
| spec_id        | mu-002                               |
| attempt        | 1                                    |
| sub-agent      | codex-oauth / gpt-5.5                |
| date           | 2026-05-09                           |
| outcome        | success on spec, WC failure mode     |

## What we expected

Spec acceptance from `mu-002-stdio-transport.md`:
- File created at `crates/mu-core/src/transport.rs`
- `pub mod transport;` line added to `crates/mu-core/src/lib.rs`
- Build clean, 19+ tests passing, module under 800 lines, no new
  deps, no unsafe/unwrap outside tests
- Diff touches *exactly* those two files

The delegation prompt's "What NOT to do" section explicitly said:
> Don't touch `crates/mu-core/src/protocol.rs`. It's the contract;
> changes there require a new spec or an amendment to mu-001.

It did NOT say anything about other files in the working copy that
weren't part of the spec — because at spec-writing time we didn't
know there'd be one.

## What we got

The spec deliverable: **clean.** All 8 §Behaviors green, 19/19 tests
pass, 420 lines, no new deps, no unsafe outside tests. Correct
concurrent dispatch (verified by the 50ms-sleep ordering test), correct
EOF drain handling, correct line framing. Quality of work was high.

But pi-rust's op log (run as the codex-oauth backend's runtime) shows:

```
op 78995aab: jj restore crates/mu-core/src/protocol.rs
op cd18cc13: jj restore crates/mu-core/src/protocol.rs specs/delegations.md
```

The first restore is *correct* behavior: gpt-5.5 evidently edited
protocol.rs at some point during its work, then realized the spec
forbade that, and rolled back. Self-correction working as intended.

The second restore is *not* — `specs/delegations.md` was a file I had
just created in the parallel claude-code session before the
delegation fired. It was in the working copy when gpt-5.5 started.
Since it wasn't in gpt-5.5's deliverable list and wasn't tracked in
the parent commit, gpt-5.5 (or pi-rust on its behalf) treated it as
detritus and restored it to its parent state — which had no such
file. Result: the file was wiped.

## Why it diverged

Two compounding factors:

1. **The instruction "don't touch any other file" is ambiguous about
   *restore* vs *modify*.** Spec readers (human or LLM) interpret
   "don't touch" to mean "don't edit." It doesn't naturally extend to
   "don't restore" because restore feels like cleanup, not editing.

2. **The concurrent-claude-code-session-modifies-working-copy pattern
   wasn't on our radar at delegation time.** This is the actual
   architectural surprise: in a multi-agent build, one agent is
   working on its task in the same working copy that another agent
   is using for *unrelated* work. The naive assumption is "if it's
   not yours, leave it alone" — but the heuristic LLMs use for
   workspace hygiene is "if it's not tracked, it's probably scratch
   to clean up."

This is a **WC** (Workspace Cleanup) failure: not a comprehension
miss, not a capability mismatch, not a spec ambiguity in the strict
sense. It's a class of error that wouldn't happen in single-agent
work and only becomes visible because we're running multiple sessions
concurrently in the same directory.

## What we changed

1. **Added `WC` failure mode to the taxonomy** in `specs/delegations.md`.
2. **Added a routing rule** under "Routing implications": delegation
   prompts must include an explicit "don't restore files you didn't
   create" sentence.
3. **(Pending — to apply on next delegation)** Update
   `mu-NNN-delegation.md` template so future prompts have:
   ```
   ## Workspace hygiene
   If you see untracked files in the working copy that aren't in
   your deliverable list, leave them alone. Do NOT `jj restore`,
   `jj abandon`, `git checkout --`, or otherwise revert them. They
   belong to a parallel session. Your "don't touch any other file"
   rule extends to restore operations, not just edits.
   ```
4. **Recovered** delegations.md from claude's message history (the
   file content was in conversation context). No data loss.

## Recovery cost

~5 minutes: noticed the missing file when I tried to update the
ledger row, traced via `jj op log`, rewrote the file from
conversation memory.

## Generalizable lesson

Multi-agent workspaces have a "blast radius" issue. A sub-agent's
authority is implicit (the working copy it sees) but its responsibility
is explicit (the spec deliverable). The gap is where collateral damage
happens. Either:
- Tighten authority via process isolation (give the sub-agent a
  worktree, not the same working copy)
- Tighten responsibility via prompt rules (the WC rule above)

For now, prompt rules. If WC failures recur, we should bias toward
isolation — `jj workspace add` per delegation, possibly auto-managed
by `agent-router`.

## Cross-references

- Ledger row: `specs/delegations.md` mu-002 row
- Op log: `jj op log` shows ops `78995aab` (correct restore) and
  `cd18cc13` (false-positive restore)
- Recovery commit: TBD (will reference the commit that lands this
  post-mortem and the recovered ledger)
