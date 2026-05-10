# Status: 2026-05-10 afternoon

Pickup readout. Update of `MORNING.md` from earlier today.

## What landed since you left

### Read tool vertical slice — fully verified live

- **mu-008** Anthropic Provider tool support (request + response).
  Part A delegated to gpt-5.5 via the new `scripts/delegate.sh`;
  part B (claude) refactored StreamState to per-block builders.
  Live test passes: real Claude calls a fake `echo` tool, parser
  extracts the `ToolCall` correctly.
- **mu-009** `--provider` / `--model` / `--tools` flag wiring on
  `mu serve` and `mu ask`. Includes a `factory` module mapping
  flag values to `Arc<dyn Provider>` / `Vec<Arc<dyn Tool>>`.
- **mu-010** integration test locking in the read end-to-end:
  `mu ask --provider anthropic-api --tools read "..."` works.

### Write tool vertical slice — fully verified live

- **mu-011** `WriteTool` (delegated to gpt-5.5; same desugared
  async-fn pattern as ReadTool to avoid pulling in async-trait as
  a mu-coding dep).
- **mu-012** wired into factory + live integration test.

### Refactor pass

- **`tests/common/`** — extracted shared `MU_BIN`,
  `live_anthropic_enabled()`, and `run_mu_ask(args)` helpers.
  Each integration test shrinks ~10-20 lines.
- **README** bumped to "as of mu-010" (and is ahead of where it
  was when you left).

## What `mu` does now (live verified)

```sh
$ mu ask --provider anthropic-api --tools read \
    "Read /home/tcovert/src/public_github/mu/Cargo.toml. \
     Tell me what the workspace's resolver version is. Just the number."
2

$ mu ask --provider anthropic-api --tools write \
    "Write 'hello' to /tmp/foo.txt. Reply 'done'."
done   # and /tmp/foo.txt now contains 'hello'
```

Two tools (`read`, `write`) are end-to-end against real Anthropic
with all 95 unit/integration tests + 2 live tests passing.

## Tests

- 95/95 workspace tests pass without `MU_LIVE_ANTHROPIC`.
- 3/3 live tests pass with the env var set
  (mu-006 text smoke, mu-008 tool round-trip, mu-010 read e2e,
  mu-012 write e2e).

## Delegation ledger

Two new rows since this morning's `MORNING.md`:

- **mu-008a** (success): first delegation through `scripts/delegate.sh`.
  Workspace isolation worked; the new shorter prompt format (~70
  lines instead of ~130) was followed cleanly.
- **mu-011** (success††): **NEW failure mode discovered.** Wrapper
  exited 1 due to an SSE truncation on the response stream, but the
  work itself was complete. New ledger taxonomy entry: TR
  (Transport / response truncation). Routing rule:
  > Non-zero exit from `delegate.sh` is NOT sufficient evidence of
  > failure. Always inspect the workspace and run verification first.
  > The diff is the source of truth, not the wrapper's exit code.

## Memory note

While working on mu-008 you flagged the design-session note about
**cooperating sessions** (typed mailboxes/direct channels between
live mu sessions, no automatic mind-meld, message kinds:
status/question/handoff/observation, context_refs to durable
artifacts). Saved as memory `d22f391a` for whenever we spec the
coordination plane. Not a current blocker.

## What's next, ranked

These are CANDIDATES. Pick the order you want when you're back.

1. **mu-013 / mu-014: `ls` tool slice.** Simplest remaining tool.
   Lists directory contents. Same shape as read/write. ~1 hr,
   delegate-able.
2. **mu-015 / mu-016: `bash` tool slice.** Higher value but
   higher security risk — runs arbitrary shell commands as the
   daemon's user. Probably warrants explicit approval design
   (allow/deny lists, prompt-on-dangerous?). Defer to discuss.
3. **mu-017+: OpenAI Codex provider.** Symmetric to Anthropic but
   uses your Pro account's OAuth via the codex CLI as a subprocess
   (per the AGENTS.md no-token-holding rule). After this, mu can
   choose between Claude and GPT.
4. **Refactor study** as you proposed — look at what's emerged
   across the read/write slices, identify abstractions worth
   pulling out before adding more. The tools/read.rs and
   tools/write.rs files share a lot of structure; might be a real
   abstraction there.
5. **Cooperating sessions spec** (memory `d22f391a`). Probably
   waits until `mu orchestrate` is real — there need to be N live
   sessions for the messaging primitive to matter.
6. **TUI prototype.** Per your earlier guidance, only stub enough
   to validate the architecture supports push/pull. Not on the
   critical path yet.

## Things to review

- **`specs/delegations.md`** — new TR failure-mode taxonomy entry,
  new routing rule about exit codes. Worth a sanity check.
- **`tests/common/mod.rs`** — first shared test helper. Pattern is
  pretty light; if you want a different shape, the change cost is
  low.
- **`scripts/delegate.sh` + `scripts/delegate-cleanup.sh`** —
  these worked on mu-008a but the mu-011 case showed how the
  wrapper-vs-workspace distinction matters. Might want to add a
  hint in delegate.sh's output saying "exit code is informational;
  inspect the workspace to confirm work state."

## Stopping point

Stopping deliberately at "two tools complete + refactor pass" so
you have a reviewable checkpoint. No new specs/delegations fired
since mu-012's commit. Workspace is clean (no in-flight tasks).
Most recent commit: `4ef4347e`.

Welcome back.
