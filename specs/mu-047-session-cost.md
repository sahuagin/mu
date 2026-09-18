# Spec: session cost — pricing a session's event log per request, per era, with provenance

| field      | value                        |
| ---------- | ---------------------------- |
| spec_id    | mu-047                       |
| status     | proposed                     |
| created    | 2026-09-17                   |
| updated    | 2026-09-17                   |
| authors    | tcovert + cc (claude-opus-5) |
| supersedes | none                         |
| bead       | mu-hx0ta                     |

## Why

mu prices a session from its rate card (`mu_core::pricing`, mu-fqvc) by
pricing the session's summed usage. Two things made that wrong once the
OpenAI lanes were rated:

- OpenAI reports cache reads AND cache writes inside `input_tokens`;
  Anthropic reports them as disjoint buckets. Pricing an OpenAI sample by
  the Anthropic rule bills every cached token at 1.10x and every written
  token at 2.25x (mu #626 board finding).
- gpt-6-astra bills a REQUEST whose prompt exceeds 272k tokens at 2x
  input/cache and 1.5x output for the whole request. A tier keyed on the
  request cannot be applied to a session's sum: one 300k request is $6.00,
  two 150k requests are $3.00, and the sum of either is 300k.

So cost is a property of the event log, not of a total. This spec fixes what
the log's cost means, where it comes from, and what a consumer may say about
it. Implementation: `crates/mu-core/src/pricing.rs` (the model),
`crates/mu-core/src/session_cost.rs` (the projection).

## Terms

- **request** — one model call: an `AssistantMessageEvent` in the log.
- **ask** — one `ask_session` round trip: from its `UserMessage` to its
  `Done`. An interjection lands as a `UserMessage` inside an ask and does
  not start one. An ask that ends in an `Error` may or may not get a
  `Done` (ticketed asks get a synthesised one).
- **era** — the (provider, model) in force: `SessionCreated`, then each
  `ProviderSwitched`, folded forward, together with the usage convention
  (`UsageSemantics`) registered by that event.
- **card** — `ModelPricing` for an era's (provider, model), interpreted
  under the era's registered convention (`under_semantics`): the log's
  declaration of how the provider counted its tokens outranks the card's
  default flags, read and write independently.
- **lane** — whether an era is metered (`billed`) or a flat-rate
  subscription (`api_equivalent`: `openai_codex`, `anthropic_oauth`). A
  subscription figure is what the tokens WOULD have cost on the api-key
  lane, not money paid.

## The projection

One pass over the events (`session_cost::project`) yields:

- **session** — `SessionCost { usd, basis, lane }`.
- **last ask** — `Option<f64>`: the most recent ask's exact per-call figure,
  or none.

Rules, in the order the events are folded:

1. **Per request, under its era.** Each request with usage is priced by the
   card in force AT THAT REQUEST (`ModelPricing::cost`, the tier applied to
   that request's prompt). A later `ProviderSwitched` never reprices it. A
   buffered switch is logged before the ask's own `Done`, so the era at
   `Done` is not the ask's identity.
2. **Reconciliation.** At an ask's `Done`, whatever its usage reports beyond
   the ask's recorded requests (component-wise, clipped at zero) is a
   remainder: a legacy ask with usage only on its `Done`, or a reasoning-only
   retry the loop folded into the ask total without a message. The remainder
   is priced at the BASE rate (`base_rate_cost`, the tier switched off) under
   the era the ask STARTED on, and makes the session figure a floor
   (`basis = base_rate`). A reported zero tier stays reported; a tier split
   that does not cover the flat remainder is dropped so the flat rule prices
   it. An ask with no events of its own has only the current era to go on.
3. **Unknown, never partial.** The session is `unknown` (no figure) when any
   request ran under an era with no card, when a remainder falls in an ask
   whose era changed mid-way (no safe attribution), or when a request
   arrived without usage (the loop's `Done` total sums only the requests
   that reported, so nothing can account for it) — from that event on, not
   from a later `Done` an errored ask may never get.
4. **Ask boundaries.** A `UserMessage` opens an ask and never resets one.
   `Done` closes it. An ask that hit an `Error` is closed by a following
   `Done` (synthesised or not) or, failing that, by the next ask's
   `UserMessage`, so it never bleeds into the next ask.
5. **Lane.** Folded per priced usage under the lane in force at the time:
   `billed`, `api_equivalent`, or `mixed` when the session spans both. A
   consumer labels from this, never from the current provider.
6. **The last ask.** `Some` only for an exact per-call figure: none if any
   of its requests had no card or no usage, none if its `Done` carried a
   remainder.

## What a consumer may say

- `per_call` — exact; show as the cost (`$`).
- `base_rate` — a floor; show as `≥$`.
- `unknown` — no number; show `$?`, never `$0.00`, which reads as free.
- `api_equivalent` — `~$`; `mixed` — `$X~`.
- A figure a consumer computes itself from a SUM (no per-request sizes, no
  per-era cards) is an ESTIMATE, not exact and not a bound across a switch
  or a tier: `≈$`. It is what an offline display, or a display talking to a
  daemon that predates the provenance fields, has.

## Increments

1. **Capability** (this spec's first increment): the pricing model
   (inclusion flags, `LongContextTier`, `SessionCost`/`CostBasis`/
   `CostLane`) and the projection, tested against the log; the tiered card
   and the subscription lanes are NOT in the rate table, so no existing
   consumer changes what it shows except the corrected cached-input rule.
2. **Integration**: `SessionStatus` and `SessionInfo` carry `cost_usd`
   with `cost_basis` and `cost_lane`; each task's telemetry carries the
   ask's exact figure (only on the `Done` envelope) and the analytics sink
   stores it (`tasks.cost_usd`); mu-solo and mu-tui label from the
   daemon's provenance; the tiered card and the subscription lanes enter
   the table.

## Non-goals

- Metered spend reconciliation against provider invoices.
- Re-pricing sessions compacted before `tasks.cost_usd` existed: the
  reader prices those from totals and labels them as the base rate.
