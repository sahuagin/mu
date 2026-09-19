# Spec: usage-limit fallback — when a subscription lane caps out, the session continues on the next configured route

| field      | value                        |
| ---------- | ---------------------------- |
| spec_id    | mu-049                       |
| status     | proposed                     |
| created    | 2026-09-19                   |
| updated    | 2026-09-19                   |
| authors    | cc (claude-opus-5)           |
| supersedes | none                         |
| bead       | mu-cbmru (relates: mu-qmnoo) |

## Why

Operator, 2026-09-10 (merging #626): gpt-6-astra is expensive and rides
the codex subscription; a busy week can drain the plan's window early.
Not "don't use it" — watch it, and when the account rejects on usage,
fall back to another model. Since then the mu-analytics judge (role
`judge` = gpt-6-astra only) has gone a third of a day without verdicts
whenever that one lane refuses or answers in prose (mu-qmnoo): one lane
with no fallback zeroes the run.

Terrain today: the codex lane renders the cap as a non-retryable error
(`codex usage limit reached (plan X); resets in ~Nm — switch providers`,
from the 429 body's `usage_limit_reached` with `plan_type` and
`resets_in_seconds`; `invoke.rs` never retries it). `agent-role` ranks
fallbacks per role and `agent-dispatch.sh` walks the ranks for
dispatched workers. mu's own loop has no fallback: a cap ends the turn
with an error, and the session is stuck on the capped lane until someone
issues `set_route`.

## Decision: the fallback lives in the daemon, at the session

Three places were possible. The dispatcher (workers only) already walks
ranks, and it cannot help an interactive or `mu ask` session. The
provider adapter cannot: switching models is a session decision (context
limits, output budget, usage semantics, receipts all change), and the
adapter has none of that. The agent loop already owns the one operation
this needs — `AgentInput::SwitchProvider`, the mid-session route switch
`set_route` performs — plus the `ProviderSwitched` log event that makes
receipts say which model answered. So:

- The fallback is a **ranked chain of routes on the session**, resolved
  by the daemon at session creation from config, pre-built with the same
  provider factory `set_route` uses (a route that cannot be built is
  refused at creation, not discovered at the cap).
- It fires **only** on the usage-limit class (`usage_limit_reached`),
  never on a transport error, a 5xx, a refusal, a context overflow or a
  cap the operator set (mu-048's ceiling). Everything else keeps today's
  behaviour: retry where the retry policy says so, else end the turn.
- When it fires, the loop switches the session to the next unused route
  (exactly what `SwitchProvider` does: provider, kind, model, limits,
  output budget, usage semantics), logs `ProviderSwitched`, emits a
  `Callout` (`kind = "fallback"`) naming the capped lane, the reason (plan,
  resets-in) and the route now in force, and **re-issues the same model
  call**. The turn continues; the caller sees a switch, not an error.
- Each route in the chain is used at most once per session; when the
  chain is exhausted the cap surfaces as it does today. The session stays
  on the fallback route afterwards (a cap resets in hours, not turns);
  `set_route` can move it back.

Nothing about which models is compiled in (AGENTS.md invariant 6). The
chain is config.

## Config

```toml
# config.toml
[[fallback]]
# the lane this chain protects; a session whose route starts here gets it
from = "openai_codex"
# routes tried in order when that lane reports usage_limit_reached
to = [
  { provider = "anthropic-oauth", model = "claude-opus-4-8" },
  { provider = "openrouter",      model = "z-ai/glm-5.2" },
]
```

`from` is a provider kind (or a configured endpoint name); `to` uses the
same `provider`/`model` vocabulary as `mu ask --provider/--model` and
`set_route`, resolved through `resolve_configured_selector`. A session
created on `from` (or switched onto it) carries the chain; a session on
any other lane carries nothing. Absent section: no fallback anywhere —
today's behaviour, unchanged.

**Open question for the operator (roster source of truth).** The
dispatcher's ranks live in `~/.config/mu/agent_roles.toml` (the agent
CLI's file, read by `agent-role`); this section would be a second roster
in `config.toml`. Options: (a) keep both by hand — drifts; (b) mu reads
`agent_roles.toml` for a named role (`fallback = { role = "code_review" }`)
— couples mu to agent_tools' file format at runtime; (c) generate: a
`[[fallback]]` layer written from `agent-role` the way `mu models sync`
writes catalog layers — one source, no drift, no runtime coupling. The
mechanism below is the same under all three; the increment that lands
the config picks one. Recommendation: (c).

## Monitoring (the "watch it" half)

The cap is invisible until it fires. Two durable signals, both
log-derived (invariant 1):

- `EventPayload::ProviderUsageLimit { provider_kind, model, plan_type,
  resets_in_seconds }` appended when the cap is hit (before any fallback),
  so mu-analytics can count caps per lane per day and show the reset
  window — the alert the bead asks for is a query over this event, not a
  new daemon.
- `ProviderSwitched` (existing) with the fallback callout's reason, so
  receipts and the session log say which model answered after a cap.

A weekly-window rollup of codex usage belongs to mu-analytics (it already
keeps per-model usage); this spec gives it the cap events to anchor on.

## What the caller sees

- Interactive (mu-solo): a callout line `usage limit on openai_codex
  (plan pro, resets in ~2h10m) — continuing on anthropic_oauth /
  claude-opus-4-8`; `/status` shows the route in force.
- `mu ask`: the same callout on stderr; the answer arrives from the
  fallback model; exit 0. With no chain (or exhausted): today's error.
- Chain exhausted: the cap error ends the turn as today.

## Non-goals

- Falling back on anything but the usage-limit class.
- Round-robin, load-based or cost-based routing.
- Reading the subscription's remaining budget ahead of time (the API does
  not expose it; the cap response is the signal).

## Increments

1. **Capability** (mu-core + mu-ai): the usage-limit class typed rather
   than string-matched (`ProviderError::UsageLimit { plan_type,
   resets_in_seconds }` from the codex adapter; `invoke.rs` maps it to a
   distinct outcome), `ProviderUsageLimit` on the log, `FallbackConfig`
   (`[[fallback]]`) with validation, `AgentConfig.fallback_routes` and the
   loop's switch-and-retry on that outcome (bounded: each route once),
   tested against the faux provider. Nothing arms it yet.
2. **Integration** (mu-coding): the daemon resolves and pre-builds the
   chain at session creation (and on `set_route` onto a `from` lane),
   `mu ask`/mu-solo surface the callout, the example config, and the
   roster source per the open question.
