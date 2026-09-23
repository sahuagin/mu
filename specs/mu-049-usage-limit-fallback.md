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
  `set_route` can move it back. The used-routes budget is a projection of
  the log (invariant 1): every fallback is the `fallback` callout the loop
  records (`SessionEventLog::fallback_routes_used`), and a continuation
  hands that list to the loop (`AgentConfig.fallback_routes_used`) so a
  resume never replenishes the chain.
- Inputs the operator sent while the capped call was in flight ride into
  the re-issued call, and survive a re-issued call that is refused before
  dispatch (the turn cap, an over-window prompt): they open the next ask.
- A cap that arrives after the call already streamed output the client
  saw is not answered by a fallback — a re-issue would repeat the output
  (the retry policy's first-token rule). It is recorded, the ask ends as
  an error, and the route is kept for a clean cap. A route named twice in
  a chain is one route.
- Under a spend ceiling (mu-048): an in-stream cap came from a request the
  provider accepted and that returned no usage — unknown money, so the
  meter locks (`SpendUnaccounted`) and no fallback is taken (its call would
  be refused at preflight). A request-time cap (the 429 body, a parsed
  error response) is a rejection the server stated: not a bill, no lock,
  and the fallback proceeds.

Nothing about which models is compiled in (AGENTS.md invariant 6). The
chain is config.

## Config

There is no new config section. The roster is the one that already
exists: `~/.config/mu/agent_roles.toml`, role → ranked targets, read by
`scripts/agent-role`. Operator, 2026-09-22: *"I don't want another
heading and another set of stuff. I just want the whole system to not
come to a halt if the #2 slot happens to be out of tokens. It should
just use the next available one."*

The ranks are a **circular list** for a caller that asks for one:
`agent-role --wrap <role> <rank>` resolves rank 3 of a two-rank role to
rank 1 rather than erroring. The wrap is OPT-IN, because the plain form's
error is a probe existing callers rely on (`ai-review.sh` reads a failing
`code_review_leaf 1` as "no second leaf seat, use the hosted default"),
and a `--wrap` caller must bound its own walk by the rank count —
`agent-role <role>` prints every rank, which is what the walkers iterate.

"Unavailable" is carried by EXIT CODES, never by parsing output. `mu ask`
already exits 3 on a spend ceiling; mu-cbmru adds **exit 4** for a lane
that is out of tokens — the daemon's typed `ProviderUsageLimit` reaches
the client as `session.provider_usage_limit`, and `mu ask` / `mu resume`
map it to that code. Nothing greps a stream: `mu ask` prints the model's
own reasoning to stderr verbatim, so a text match cannot tell a
provider's error from a model reasoning about one — and a reviewer seat
reads material full of both.

The dispatcher turns that code into the existing route-around contract:
**exit 75** already means *this seat never ran — safe to try the next
rank*, and `mu-spawn` and the review panel already walk on it (it is
produced today by a held or unreachable ollama box and by a
provider-auth failure). Because 75 asserts something a cap cannot prove
— a multi-turn worker can write a file and then exhaust its quota on the
next request — the conversion is the CALLER's declaration,
`AGENT_DISPATCH_CAP_ROUTE_AROUND=1` (default off; the review panel sets
it, since a seat produces a verdict and nothing else), with a
write-naming tool grant refused even then. A transient rate limit is
deliberately NOT in this class — that one is worth retrying on the same
seat, and the retry policy already does.

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

1. **Capability** (mu-core + mu-ai, shipped): the usage-limit class typed
   rather than string-matched (`ProviderError::UsageLimit { plan_type,
   resets_in_seconds }` from the codex adapter; `invoke.rs` maps it to a
   distinct outcome), `ProviderUsageLimit` on the log,
   `AgentConfig.fallback_routes` and the loop's switch-and-retry on that
   outcome (bounded: each route once), tested against the faux provider.
   Nothing arms it yet. (It also shipped a `[[fallback]]` config section;
   that was a second roster and is retired — tolerated-and-ignored so an
   operator config carrying it still loads.)
2. **Dispatch fall-through** (shipped): `mu ask`/`mu resume` exit 4 when
   the lane reported a usage cap or ran out of credit (typed in the
   codex, openai, openrouter and anthropic adapters; surfaced as
   `session.provider_usage_limit`), `agent-role --wrap` makes rank
   resolution circular, and `agent-dispatch` converts exit 4 to 75 — the
   walkers' existing "never ran, try the next rank" contract — when the
   caller has declared its task re-runnable.
3. **Integration** (mu-coding): the daemon resolves the session's routes
   from the role's ranked roster (`agent-role`) at creation and on
   `set_route`, passes `SessionEventLog::fallback_routes_used()` on a
   resume, and `mu ask`/mu-solo surface the fallback callout.
