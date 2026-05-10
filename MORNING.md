# Morning summary — overnight session

Wake-up readout. Skim, decide what to keep / tweak, delete this file
when read (or rename to `MORNING-2026-05-10.md` if you want to keep
the history).

## What landed

Three specs, four commits on `main`:

```
83fe68e  docs(delegations): add mu-007 row to ledger
0c24b8f  feat(mu-coding): mu-007 — read tool, first concrete Tool impl
2cddb14  docs(readme): update Status section to reflect MVP working through mu-006
3f4bde9  feat(mu-ai): mu-006 — Anthropic API Provider (text-only, v1)
f779752  feat(mu-coding): mu-005 — mu ask one-shot CLI mode
```

(Plus three spec commits: mu-005, mu-006, mu-007 + their delegation
prompts where applicable.)

## What works now

```sh
mu versions          # workspace smoke
mu serve             # JSON-RPC daemon over stdio
mu ask "hello"       # one-shot CLI; spawns mu serve, echoes via FauxProvider
```

Live Anthropic API verified: `MU_LIVE_ANTHROPIC=1 cargo test -p mu-ai
b7_live_anthropic_smoke` calls real Claude (haiku-4-5), got "hello"
back, all green.

## Tests

70/70 workspace tests pass. Breakdown by crate:
- mu-core: 47 (protocol, transport, agent loop)
- mu-ai: 16 (faux, sse, anthropic — live test skipped by default)
- mu-coding: 7 unit + integration tests across serve_smoke and ask_smoke

## Delegation ledger highlights

WC fix held for ALL FOUR night delegations. Pattern is reliable. Two
new positive routing rules surfaced:
1. **Treat §Invariants as normative, §Interfaces sketches as
   illustrative.** Evidence: mu-004a (gpt-5.5 mapped mutex poisoning
   to ProviderError instead of using the spec's literal `expect`).
2. **Implementers can sometimes avoid a dep the spec implies.**
   Evidence: mu-007 (gpt-5.5 desugared `Tool::execute` rather than
   add `async-trait` to mu-coding's deps).

Both rules logged in `specs/delegations.md` "Routing implications"
section.

## Memory — for context

While working on mu-006 you mentioned the parallel-session work on
LLM internals + memory dreams. I searched memory and saved a
forward-looking note (`agent memory show ee639a12`) about wiring mu's
future memory integration through the existing `agent memory` MCP
primitives (show / events / patch-log / apply-plan) rather than
re-implementing storage. Not started; just flagged for whichever spec
incorporates memory into mu (probably mu-009 or similar).

## What's next, in order of value

These are CANDIDATES, not commitments — your call:

1. **mu-006 extension: AnthropicProvider tool support.** Adds the
   `tools` field to the API request and parses `tool_use`
   content blocks from the response. Once this lands, `read`
   (mu-007) can be wired into the daemon's tool list and the agent
   can actually invoke it. ~2-3 hr; probably half delegation, half
   claude.

2. **mu-008 candidate: Wire `read` tool + provider selection into
   `mu serve`.** Adds CLI flags / config so you can do
   `mu serve --provider anthropic-api --model claude-haiku-4-5
   --tools read`. ~1-2 hr; mostly claude (config plumbing has
   judgment calls).

3. **First TUI prototype (`mu tui`).** ratatui-backed interactive
   client that connects to `mu serve`. Bigger; ~half-day at minimum;
   architecturally interesting (event-driven UI on top of the same
   transport).

4. **Memory integration (mu-009 candidate).** Per the parallel-session
   work — wire `agent memory` (show/recent/search/apply-plan) as a
   built-in MCP-server-equivalent in `mu serve`. Per memory
   `ee639a12` saved overnight.

5. **OpenAI provider (mu-010 candidate).** Symmetrical to mu-006 but
   for the OpenAI API. Mostly mechanical translation. Good
   delegation candidate.

## Things to review

- **README.md**: I updated the Status section to claim "MVP working."
  Worth a sanity check.
- **specs/mu-007-delegation.md** & **mu-007-read-tool.md**: spec
  + prompt for the delegation. The spec is opinionated about the
  `spawn_blocking + select!` pattern (§OOC-3) — opinionated for
  good reason but worth knowing in case future tools want a
  different shape.
- **`agent memory show ee639a12`** if you want to read the memory
  integration note.

## Stopping rationale

Hit my stopping condition: three specs landed cleanly. Could have
kept going (mu-006 extension is right there) but didn't want to
rack up a fifth delegation or get into architectural decisions
without your input on mu-008's shape.

Welcome back.
