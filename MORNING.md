# Status: 2026-05-10 late evening

Latest readout. Picks up from this afternoon's note.

## What landed since the afternoon refresh

### mu-016: callout primitive ✅

Catch-all "agent says something notable" notification. Free-form
`kind`, optional `theme`, `context_refs` to durable artifacts.
Future-extensible: peer messages, memory recalls, observations,
warnings all become callouts without protocol changes.

No emitters yet — the surface is ready; future specs hook real
events into it. Saved memories `d22f391a` (cooperating sessions)
and `ee639a12` (memory integration) both reference this primitive
as the consumption point.

### Refactor pass: forwarder + agent-loop planners ✅

The "queue-input → pure-logic → queue-output" pattern, applied:

- **forwarder**: extracted `translate_event(session_id, event) →
  Option<(method, value)>` as a pure function. The IO loop becomes
  thin glue. 6 new unit tests target the translation directly.

- **agent loop**: extracted `plan_post_invoke_llm`,
  `plan_post_execute_tools`, `should_push_invoke_llm` as pure
  functions. The loop's match arms are now ~half their previous
  size; the planning logic gets edge-case tests without spawning
  mock providers/tools. 9 new tests.

All existing behavior tests still pass — refactor is observation-
preserving. The user's framing crystallized: *queue-mediated logic
should be pure, with explicit inputs and outputs; the queue
topology gets tested separately at a coarser grain.*

### mu-017: OpenRouter provider ✅

Third Provider impl, fourth provider total. Pi-free path to GPT,
Gemini, Llama, Claude (all via OpenRouter), with full tool support
and streaming. OpenAI-compatible wire format.

Verified live end-to-end:

```sh
$ mu ask --provider openrouter --model anthropic/claude-haiku-4.5 --tools read \
    "Read mu's Cargo.toml. What's the resolver version? Just the number."
2
```

Same vertical slice that Anthropic-direct supports, this time
through a different provider. Validates the Provider abstraction
generalizes — Anthropic's content-block format and OpenAI's
delta-by-index format are different enough that any abstraction
leaks would have surfaced. None did.

Long-term, mu-017 supersedes mu-015 (OpenAI-Codex via pi
subprocess) for delegate use cases that don't specifically need
OpenAI Pro budget.

## What `mu` does now

```sh
# Four providers
mu serve --provider faux                # echo (default; tests)
mu serve --provider anthropic-api       # real Claude (mu-006)
mu serve --provider openai-codex        # OAuth-via-pi-subprocess (mu-015)
mu serve --provider openrouter          # Multi-model HTTP+key (mu-017) ← NEW

# Three tools
mu serve --tools read,write,ls

# End-to-end CLI
mu ask --provider <p> [--model <m>] [--tools <csv>] "..."
```

Per the user's stated MVP provider list:

| Stated MVP | Spec | Status |
|------------|------|--------|
| anthropic_key | mu-006/008 | ✅ full (text + tools + streaming) |
| openai oauth | mu-015 | ✅ stepping stone (text-only, leaky) |
| openrouter-many | mu-017 | ✅ full (text + tools + streaming) |
| anthropic oauth | (deferred) | not started; low priority |

OpenRouter unlocks GPT for tool-using sessions if needed, despite
the openai-oauth path's v1 limitations.

## Tests

- **142/142 unit/integration tests pass** without live env vars.
- **5/5 live tests pass** with appropriate env vars set:
  - mu-006 Anthropic text smoke
  - mu-008 Anthropic tool round-trip
  - mu-010 read tool e2e via Anthropic
  - mu-012 write tool e2e via Anthropic
  - mu-014 ls tool e2e via Anthropic
  - mu-015 OpenAI-Codex text smoke (gated separately on MU_LIVE_OPENAI_CODEX)
  - mu-017 OpenRouter text smoke + tool round-trip (gated on MU_LIVE_OPENROUTER)

Note: that's actually 7 live tests now; my count drifted. Several
gated only on the per-provider env var.

## Architectural observations from this stretch

1. **The pure-translation pattern is the right abstraction for
   queue-mediated logic.** Identifying it during the forwarder
   refactor and then applying it to the agent-loop's planners both
   produced cleaner code and ~15 new edge-case tests we wouldn't
   have written otherwise. Worth applying anywhere else queue-IO
   wraps decision logic.

2. **The Provider abstraction holds across HTTP + content-block
   (Anthropic) and HTTP + delta-by-index (OpenAI/OpenRouter)
   formats.** Three providers in, no abstraction leaks visible.
   The `BoxStream<'static, ProviderEvent>` pattern works across
   substantially different wire formats.

3. **Two architectural questions still open** that we discussed
   tonight but didn't formalize:
   - Cooperating sessions / mailbox primitive (memory `d22f391a`)
   - `session.input_required` for in-band agent-asks-user
   Both will become specs when there's a concrete first consumer.
   Callout's existence reduces pressure on both — observations
   that don't need a response just become callouts.

## What's next

In rough priority order:

1. **First real callout emitter.** Pick a plausible site (iteration
   cap warning, tool error context) and wire it through. Validates
   mu-016's surface end-to-end. ~50 LOC, ~30 minutes.

2. **Bash tool spec** — when you've reviewed `specs/recon-bash.md`
   and picked a phase-1 direction (most likely: allowlist with a
   small curated default). 2 specs, ~3 hours total.

3. **Memory integration as callouts.** Wire `agent memory`
   recalls as `kind: "memory"` callouts. Per the design we
   discussed and memory `ee639a12`. Probably a future spec when
   we know what the trigger surface is.

4. **More tools** — edit (line-based modification), find, grep.
   Each follows the read/write/ls template; mostly mechanical.

5. **TUI prototype** — still defer. None of the above need it.

6. **anthropic-oauth via subprocess** — symmetric to mu-015 but
   wrapping `claude --print`. Useful for using Max5 budget. Probably
   skip given anthropic-api is full-featured.

## Stopping here

No in-flight delegations. Workspace clean. Most recent commit:
`77924d91`.

Welcome back when you're back.
