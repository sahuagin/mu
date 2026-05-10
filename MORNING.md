# Status: 2026-05-10 evening

Latest readout. Picks up from this morning's note.

## What landed since the last refresh

### Track A: ls vertical slice ✅

- **mu-013** `LsTool` (delegated to gpt-5.5; same desugared async-fn
  pattern as ReadTool/WriteTool). 6 tests, 237 lines, no async-trait
  dep added.
- **mu-014** wired into factory + live integration test.
  `mu serve --tools ls` works.

Verified live:

```sh
$ mu ask --provider anthropic-api --tools ls \
    "Use the ls tool to list /tmp/mu_014_ls_test. Reply with the names."
mu_014_marker_b8d9c.txt
```

### Track B: OpenAI Codex provider ✅

- **mu-015** subprocess wrapper around `pi --provider openai-codex`.
  No OAuth tokens enter mu's address space — pi handles all token
  management. Matches AGENTS.md's no-token-holding rule.
- v1 limitations (intentional): no streaming, no tool support,
  cancel doesn't actively kill the subprocess. Future specs.

Verified live:

```sh
$ mu ask --provider openai-codex "Reply with just 'success'."
success
```

(Some pi metadata leaks through after the response text — known
v1 limitation, see commit message.)

### Track C: bash tool security recon ✅

- **`specs/recon-bash.md`** — research doc, not a numbered spec.
  Surveys five design options (refuse-to-ship, allowlist-only,
  prompt-for-approval, hybrid, containerization), threat model,
  per-decision questions, recommended phased path.
- Open questions for you in the doc to decide before formalizing
  into a numbered spec.

## What `mu` does now

```sh
# Three providers
mu serve --provider faux                # default; echo
mu serve --provider anthropic-api       # real Claude (mu-006)
mu serve --provider openai-codex        # OAuth-via-pi-subprocess (mu-015)

# Three tools
mu serve --tools read,write,ls          # all available

# End-to-end CLI
mu ask --provider <p> --tools <csv> "..."

# All flags forward through subprocess
```

## Tests

- 109/109 unit/integration tests pass (no live env vars).
- 4/4 Anthropic live tests pass with `MU_LIVE_ANTHROPIC=1`:
  text smoke, tool-use parsing, read e2e, write e2e, ls e2e.
- 1/1 OpenAI live test passes with `MU_LIVE_OPENAI_CODEX=1`:
  text smoke (only test for this provider; no tool support v1).

## Delegation ledger

One new row this stretch:
- **mu-013** (success): delegated cleanly through `scripts/delegate.sh`.
  Following the same shape as mu-007 / mu-011 — the structural pattern
  for tool implementations is now well-locked-in.

No new failure modes discovered. Pattern shake-out should slow now;
we've seen WC, TR, and the desugared-async-fn-vs-async-trait
flexibility. Future delegations are likely to be uneventful.

## A small infrastructure soft spot

Mid-flight: when I wrote the bash recon doc and then called
`jj rebase`, the doc disappeared from disk because my working-copy
commit got rewritten as part of moving the delegate's commit onto
main. I had to op-restore to find it (didn't), then rewrite from
context.

**Lesson for future me:** when working in the orchestrator's main
workspace, commit-then-rebase, not write-and-rebase. Or use
`jj split` to peel off intermediate work into its own commit before
any rebase shuffle.

I noted this in the recon's changelog.

## Spec list, current

| Spec | What | Status |
|------|------|--------|
| mu-001..mu-005 | foundation | ✅ |
| mu-006 | AnthropicProvider | ✅ |
| mu-007 | ReadTool | ✅ |
| mu-008 | Anthropic tool support | ✅ |
| mu-009 | --provider/--model/--tools flags | ✅ |
| mu-010 | read e2e test | ✅ |
| mu-011 | WriteTool | ✅ |
| mu-012 | wire write + e2e test | ✅ |
| mu-013 | LsTool | ✅ |
| mu-014 | wire ls + e2e test | ✅ |
| mu-015 | OpenaiCodexProvider | ✅ |
| recon-bash | bash security recon | ✅ research |

## What's next, ranked

1. **Bash spec** based on recon decisions. Once you pick a phase-1
   direction (probably allowlist-only with a curated default), we
   have ~2 numbered specs (the tool itself + factory wiring + e2e).
2. **OpenRouter provider** as a third HTTP-based provider. Easy,
   gives access to many models, exercises the Provider abstraction
   in a third dimension.
3. **More tools**: edit (line-based), find, grep — useful, similar
   shapes.
4. **`session.input_required` protocol extension** — needed for
   the bash phase-2 hybrid model AND for cooperating-sessions.
5. **Refactor study**: with read/write/ls all using the same
   spawn_blocking + select! pattern, there's a clear extraction
   target. Probably a `tool_io` helper module in mu-coding/src/tools/.
6. **TUI prototype** — still appropriate to defer.

## Stopping here

Three completion-points across the three tracks. No in-flight
delegations or workspaces. Most recent commit: `2b37c7e7`.

Welcome back when you're back.
