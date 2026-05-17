# Delegation: mu-u8a — TUI color polish (moonfly/github_dark family)

This file is the prompt sent to a sub-agent to implement bead
`mu-u8a`. It is the FIRST overnight-prep test of the verify-claims
gate (mu-b5kl) on a real delegated worker.

Routing target: `agent-router --auth codex-oauth` (flat-rate OpenAI
subscription; not API-token-billed).

---

## Workspace hygiene (READ FIRST)

You are working in your OWN jj workspace at
`/home/tcovert/src/public_github/mu/.delegations/mu-u8a-attempt-1/`.
Stay in that workspace. Do not `jj restore`, `jj abandon`, or `git
checkout --` files you didn't author. Do not delete untracked files.

---

## The bead, verbatim

> Reference memory 269e44ce (user color preferences). Current TUI uses
> some saturated colors (yellow streaming block, bright Green status)
> that feel slightly louder than the moonfly/github_dark aesthetic
> Thaddeus prefers.
>
> Pass:
> - Yellow `Color::Yellow` for streaming block / awaiting-first-token
>   → muted teal or soft amber
> - Green `Color::Green` for running session glyph → cooler/desaturated
> - Active-tab Black-on-Cyan is closest to the editors and can stay
> - Budget warning red threshold can stay (signals real danger)
>
> Test by running TUI alongside helix open in another terminal —
> colors should feel of-a-piece, not jarring.

## Scope

In: `crates/mu-tui/src/main.rs` (color constants + Color uses).

Out: Any other crate, any other file. Specifically:
- Do NOT modify `crates/mu-tui/src/` files other than `main.rs`
  unless absolutely necessary (and surface the deviation in your
  envelope notes if so).
- Do NOT modify other crates.
- Do NOT modify specs or AGENTS.md.

## Color choices

You pick the actual replacement values. Guidance:

- The moonfly/github_dark family uses muted, slightly cool/grey
  tones. Saturated primaries (pure Yellow, pure Green) are out of
  family.
- `Color::Yellow` → either `Color::Rgb(204, 153, 102)` (soft amber)
  or `Color::Rgb(115, 178, 159)` (muted teal). Pick one. Your call
  based on which reads better against a dark terminal background.
- `Color::Green` for "running" → `Color::Rgb(122, 162, 122)` or
  similar muted/desaturated green. Cooler than `Color::Green` but
  still readable as green.
- `Color::Cyan` for active tab → keep as-is.
- `Color::Red` for budget warning → keep as-is.

Tools: `rg -n "Color::Yellow|Color::Green" crates/mu-tui/src/main.rs`
will find the call sites. There may be 4-10 of them.

## Verification (required)

Before exit:

1. `cargo fmt --check`
2. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
3. `cargo test --workspace --all-features --no-fail-fast` (no
   regressions; existing tests pass)

## Commit + the `## Files` block (LOAD-BEARING)

Your commit message MUST end with a `## Files` block listing every
file you changed. Format per line:

```
<STATUS> <PATH> [+<added>] [-<deleted>]
```

Where `STATUS` is `A` (added) / `M` (modified) / `D` (deleted) /
`R` (renamed). Example:

```
## Files
M crates/mu-tui/src/main.rs +12 -8
```

The `scripts/verify-claims.sh` gate (mu-b5kl) runs at your exit and
will FAIL if your claim list does not match `git diff-tree`. Don't
guess LOC counts wildly — within 20% drift is fine, beyond that
warns. Path mismatches are fatal.

Get the actual counts via `git diff-tree -r --numstat --no-commit-id
<commit>` once you've made your commit.

## Output envelope (write to stdout at the end)

A short summary including:

- The bookmark you committed on (`delegate/mu-u8a-attempt-1`)
- Your commit's short sha
- Verification results (the three commands above)
- Notes — any color choice rationale or deviation from this prompt
- Anything surprising about the codebase, the bead text, or the
  acceptance criteria

## What this delegation is testing

Beyond the bead itself, this is the **first overnight-prep
gate-validation test**. The orchestrator (claude) wants to learn:

- Did you emit a valid `## Files` block?
- Did the gate (`scripts/verify-claims.sh`) pass at your exit?
- Did you hallucinate any file claims?

Don't game these — answer honestly even if the gate failed.
