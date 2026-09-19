# Spec: spend ceiling — a session-level dollar limit, off unless asked for

| field      | value                        |
| ---------- | ---------------------------- |
| spec_id    | mu-048                       |
| status     | proposed                     |
| created    | 2026-09-18                   |
| updated    | 2026-09-18                   |
| authors    | tcovert + cc (claude-opus-5) |
| supersedes | none                         |
| bead       | mu-z94um                     |
| depends on | mu-047 (cost per request), mu-1x0ze (cards from the catalog) |

## Why

The one place a dollar limit is wanted is `mu ask`: a benchmark or any run
through a metered lane (openrouter, anthropic-api, openai-api) where the
operator agrees an upper bound on what the run may spend, and — less often
— a subscription lane where the bound protects the rest of the month. mu
has never enforced one: the trials' ceiling was the API key's prepaid
balance, and the `[budget]` section, the `--max-budget-usd` flag its doc
cited, and `TaskExitReason::BudgetCap` were placeholders (removed or
producer-less until this spec; mu-1x0ze).

Two rules from the operator, load-bearing:

1. **A limit you can hit and cannot turn off is a bug.** The ceiling is OFF
   unless configured or asked for on the command line. Nothing hidden, no
   compiled number.
2. **The caller that spends is the one that arms it.** `mu ask --max-usd X`
   is the runtime override; config is the standing setting. No env var.

## Terms

- **spent** — the session's priced cost so far, per request under the card
  in force at that request (mu-047), as metered in the agent loop from
  each assistant message's usage. On a subscription lane the figure is
  API-equivalent; `lanes` says whether it counts.
- **ceiling** — `SpendCeiling { max_usd, lanes }`; `lanes` is `billed`
  (metered lanes only) or `all` (subscription lanes' API-equivalent figure
  counts too).

## Config

```toml
[spend]
enabled = false        # default: no ceiling anywhere
max_usd = 2.00         # the standing ceiling when enabled
lanes   = "billed"     # or "all"
```

`enabled = true` with no `max_usd` is a config error (loud, the section is
ignored). The section applies to every session the daemon creates unless
the session request carries its own ceiling.

## Command line

`mu ask --max-usd X [--spend-lanes billed|all]` arms a ceiling for that
ask regardless of config (`X` must be finite and > 0). `--max-usd 0` is
refused, not "unlimited": to run unlimited, omit the flag.

## Enforcement

The daemon, at the model-call boundary, exactly where `max_turns` is
enforced today (`agent/loop_`), never the model:

- After each assistant message with usage, `spent += card.cost(usage)`
  where `card` is the current provider/model's catalog card under the live
  provider's registered usage semantics (`under_semantics`): each call is
  priced exactly the way mu-047's log projection prices it. An actionless
  turn the loop discards (mu-rb4u) is metered like any call; the log
  carries its usage in the ask's `Done` total, which the projection prices
  at base rate (`CostBasis::BaseRate`) — the one place the meter is more
  exact than the fold. A provider with no card cannot be
  metered: the ceiling **refuses to arm** at session creation
  (`session.create` fails with the reason) rather than pretend, and a
  switch onto an unpriceable lane mid-session is refused before the call,
  so an unpriceable run is never silently unlimited.
- A call the provider does not account for is money the meter cannot see:
  a completed response with no usage, or a **dispatched** request
  (`Provider::stream` called) that errored, stalled, was cancelled or
  ended without its usage frame — at any point, output or not, since
  prefill is billable and a stall says nothing about it, and even a
  `stream()` error, since a transport failure while awaiting the response
  headers can follow the server accepting the request. Under a ceiling
  the loop retries nothing: a retry is a second billable request after
  the spend became unknown, and the overshoot bound is one request. The
  ask ends with an error, and the meter **fails closed** for the rest of
  the session: every later call is refused before it runs (`N call(s)
  ... not accounted for`). One such call is the overshoot the ceiling
  already allows; a second would make the ceiling a fiction. (A rejected
  request usually costs nothing; the loop cannot tell a rejection from a
  transport failure after acceptance, and the ceiling never guesses — a
  session under a ceiling that hits a 429 is locked, loudly, and
  restarted.) A completed-but-unaccounted response is still published to
  history and events (a text answer is not thrown away for missing
  accounting metadata); its tool calls are refused, each closed by a
  synthetic error result so the history stays a valid continuation.
- Before queueing the next model call, if `spent >= max_usd`, the ask ends
  with `Done { stop_reason: BudgetCap }` carrying the usage so far, the
  same way `IterationCap` does (an autonomous run is terminated first,
  reason `BudgetExhausted`). `task_telemetry_for` maps it to
  `TaskExitReason::BudgetCap` — its first producer.
- The call that crosses the line is allowed to finish (a request cannot be
  priced before it runs); the overshoot is bounded by one request. A
  `max_usd` below one request's cost therefore stops after the first call.
- The ceiling is per session (a `mu ask` is one session) and lives as long
  as the session: `/clear` empties the context but refunds nothing, so a
  session that reached its ceiling stays stopped until a new session arms a
  new one. A delegate is a new session and arms from its own request or the
  config; the spent figure does not carry over to it.
- The meter is a projection (invariant 1), not a second source of truth:
  `spent` is what `session_cost::project` computes over the session's log
  (same card, same semantics), and `unaccounted` is the log's `unknown`
  (a usage-less assistant event) plus the accepted-and-broken streams.
  A fresh session arms `SpendMeter::new` (zero) and every metered call is
  also an assistant event in the log. A **continuation** — a resume; the
  loop is told so explicitly (`AgentConfig.continuation`, from the
  daemon's resume bootstrap), never inferred from history, since a resume
  after `/clear` starts empty — arms
  `SpendMeter::from_projection(ceiling, log.cost_projection())`: what the
  log says was spent is what the ceiling has left to give. Only an exact
  (`PerCall`) figure restores: `Unknown` locks the meter, and so does
  `BaseRate` — a floor on a tiered card (a discarded empty turn's usage
  lands in the ask's `Done` total and is priced at base rate), and a
  ceiling restored from a floor could overshoot in the unsafe direction;
  under `lanes = "billed"` an all-subscription
  history counts as nothing spent and a mixed one counts in full (stops
  early, never late). The loop enforces the rule by construction: a
  continuation that arrives with a fresh meter is locked at start and
  every call refused (`without its spend history`), so no caller can
  grant a resumed session a fresh allowance by forgetting to restore.
  The lock itself is durable: each unaccounted call is logged as a
  `SpendUnaccounted { calls }` event before the error that ends the ask
  (a full cancel mid-dispatch included), the cost projection prices the
  session as unknown from that event on, and a meter restored from it
  locks. Increment 2 wires the resume handler to `from_projection`.

## What the caller sees

- `mu ask --max-usd X [--spend-lanes billed|all]` arms a ceiling for that
  ask's session (`CreateSessionRequest.spend_ceiling`; validated before a
  daemon is spawned). On `BudgetCap` it prints `spend ceiling reached:
  $spent of $max (lanes: billed)` to stderr and exits 3 (distinct from a
  model error, 1); the answer so far is on stdout. The figure rides on a
  `session.callout` (`category = "spend"`, `body.summary`) the loop emits
  just before the `session.done` — the Done carries usage, not dollars.
- The daemon arms the request's ceiling, else its `[spend]` default, at
  session creation, before anything is written: a lane with no rate card
  or a half-set `[spend]` refuses the session with the reason
  (`session.create` fails). The armed ceiling is on the log as
  `SpendArmed { ceiling }`. Every resume — armed or not — seeds the new
  head's log with `CostCarried { session }`, its predecessor's cost
  projection at the fork; `session_cost::project` folds it as the opening
  balance (weakest basis, folded lane), so a chain of resumes adds up
  across an unarmed hop and a later ceiling restores from the whole
  figure. A head born before the carry existed (`ContinuationSeeded`
  without `CostCarried`) prices only its own calls; its resume carries
  `Unknown`, and a ceiling restored from it locks — never a partial
  figure passed off as the chain's. `mu resume <ref> <prompt>` reports `budget_cap` the way `mu
  ask` does (stderr figure, exit 3): a head armed from `[spend]` may
  already have spent its ceiling. A delegate is a new session: the
  `[spend]` default, nothing carried.
- The status surfaces (`SessionStatus.spend_ceiling`, mu-solo `/status`
  `ceiling:` line) show the armed ceiling next to the cost the log prices.
- `session.error`/notifications: a `session.done` with `stop_reason =
  budget_cap`, no new notification type.

## Non-goals

- A daily or weekly ledger across sessions (mu-fvy0's axis).
- Reconciling against the provider's bill.
- Pre-flight estimation of a request's cost before it runs.

## Increments

1. **Capability** (mu-core): `SpendCeiling` + `[spend]` config, the
   `StopReason::BudgetCap` variant and its telemetry mapping, the meter in
   the loop with the stop, tested against the faux provider (a priced card
   in a test catalog). Nothing arms it yet.
2. **Integration**: `CreateSessionRequest.spend_ceiling`, the daemon's
   config default, `mu ask --max-usd`/`--spend-lanes` and exit code 3,
   the `spend` callout, `SpendArmed` and `CostCarried` on the log, the
   status surfaces, the resume handler restoring the meter from the
   predecessor's projection, `mu resume` exit 3, `[spend]` in the example
   config.
