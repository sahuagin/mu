# Spec: usage-limit fallback — when a subscription lane caps out, the session continues on the next configured route

| field      | value                        |
| ---------- | ---------------------------- |
| spec_id    | mu-049                       |
| status     | proposed                     |
| created    | 2026-09-19                   |
| updated    | 2026-09-25                   |
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

- The fallback is **the role's ranks, as a circular queue** (operator,
  2026-09-25: *"A role should have multiple models in it. When one fails
  it just moves to the next … If there are no available models,
  something is wrong and it should stop."*). The role is the one the
  model was chosen from: whoever dispatched it resolved it with
  `agent-role`, and passes it down (`DISPATCH_ROLE` → `mu ask --role` →
  `CreateSessionRequest.role`). The daemon reads the role's ranks from
  `agent_roles.toml` itself — the file `agent-role` reads, found the same
  way and only that way (`$AGENT_ROLES`, else
  `~/.config/mu/agent_roles.toml`), so the dispatcher's pick and the
  session's ranks never come from different files — and pre-builds every rank with the
  provider factory `set_route` uses. It does not run the `agent-role`
  script: what the script adds over the file never reaches a fallback (an
  `AGENT_ROLE_PIN` names one exact model, so the dispatcher passes no role
  under it; lease-aware ordering only moves ollama ranks, which are not
  switch targets), and a child process in the daemon's create path was
  lifecycle risk for nothing (five board rounds said so). A rank that
  cannot run in a mu session is recorded as unrunnable with the reason,
  never dropped silently: a `claude-oauth` rank (it runs as `claude -p`,
  a fork-exec of the `claude` CLI that works fine — from the dispatcher,
  as a separate agent a mu session cannot hand its conversation to), a
  provider that fails to build, and a LOCAL rank — ollama, vLLM, a
  config-defined endpoint, or any rank with its own `endpoint`/`lease` —
  which runs under arrangements the dispatcher makes per call that a
  session switching in place cannot take. An unreadable roster, an
  unknown role, or a role with no ranks refuses the session — when the
  caller named the role. A role a resume INHERITED from its predecessor's
  log that no longer resolves does not block recovery: the session resumes
  without the fallback and says why. The armed
  ranks are durable (`FallbackArmed { role, ranks, unrunnable }`), so a
  resume arms the same role. A role armed short is told to the caller
  (`CreateSessionResponse.fallback_unrunnable`; `mu ask` prints
  `mu: fallback cannot use: …`), not only logged. Each rank is built once
  at creation (so an unbuildable one is named then) and built again at the
  switch, from the credentials current then; one that fails to build at the
  switch is skipped with its reason and the walk moves on.
- It fires **only** on the usage-limit class (`usage_limit_reached`),
  never on a transport error, a 5xx, a refusal, a context overflow or a
  cap the operator set (mu-048's ceiling). Everything else keeps today's
  behaviour: retry where the retry policy says so, else end the turn.
- When it fires, the loop switches the session to the next rank after the
  route in force (wrapping; from rank 0 if the route in force is not a
  rank) that has not capped
  (exactly what `SwitchProvider` does: provider, kind, model, limits,
  output budget, usage semantics), logs `ProviderSwitched`, emits a
  `Callout` (`kind = "fallback"`) naming the capped lane, the reason (plan,
  resets-in) and the route now in force, and **re-issues the same model
  call**. The turn continues; the caller sees a switch, not an error.
- A route that capped is not walked onto again; when every runnable rank
  has capped, the ask stops with the cap error naming the role, the
  models that ran out, and the ranks that cannot run in a mu session
  (`mu ask` exits 4). The session stays on the fallback route afterwards
  (a cap resets in hours, not turns); `set_route` can move it, and the
  walk continues from wherever it is. The capped set is a projection of
  the log (invariant 1): the durable `ProviderUsageLimit` records
  (`SessionEventLog::capped_routes`), which a continuation hands to the
  loop (`AgentConfig.capped_routes`) and carries into the new head's
  durable bootstrap (`CapsCarried`), so a resume — or a resume of a
  resume — never walks back onto an empty account.
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
3. **Integration** (shipped): the role rides down from the dispatcher
   (`DISPATCH_ROLE`, set by mu-spawn, the leaf reviewer and the
   orchestrator for a model their role resolved; not for one an override
   named, and NOT by the review panel — a panel seat is counted as one
   independent reviewer, and a seat continuing on another rank's model
   would be counted twice under two names; a capped seat is skipped and
   named on the census instead. The dispatcher only passes the flag to a
   `mu` that takes it, and says so when the installed one does not) to `mu ask --role` / `mu resume --role`; the daemon
   resolves and arms the ranks at creation (a resume inherits the
   predecessor's role and capped set from its log); the walk is circular
   from the route in force, so `set_route` needs nothing; `mu ask` prints
   the fallback callout on stderr (`mu: usage limit on A — continuing on
   B`) so the caller can say which model answered and that the capped
   account may need credit. A dispatcher sends a seat's stderr to its
   errlog, so `mu ask --notices <file>` also writes mu's own notices (the
   switch, the ranks it cannot use — never model output) to a file the
   dispatcher forwards to its caller, success or not. mu-solo sessions have no role (the operator
   picked the model) and are not armed.
