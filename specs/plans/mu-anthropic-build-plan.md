# mu-anthropic — build plan

Status: planned, not started. 2026-06-13. This is the WHAT. The sibling
`AGENTS.md` is the HOW + the context every new session needs before touching
this. Read both before writing code.

## What this crate is

A standalone Rust library that models **Anthropic's Messages API wire
protocol** as typed enums/structs with explicit, tested (de)serialization.
It turns "the bytes we send to / receive from Anthropic" from hand-crafted
JSON into a type whose serialized form IS the wire contract.

It is a sibling under `crates/providers/` because there will be more than one
provider (anthropic first; cc and mu themselves later, see "Providers as
oracles"). The `providers/` parent makes that plural structural from day one.

## Why it exists (the finding that started this)

mu's current provider path was written to *just barely work* against one
provider's happy path, by hand, without the specs open:

- The on-the-wire JSON is **hand-crafted**, not derived from provider-shaped
  types. serde only does mu's *internal* ser/de; the wire format is assembled
  by hand elsewhere.
- The response parse is a shallow field `match` that assumes
  `content: [{ "text": "..." }]` is **always one array element, always one
  dict key**. Anthropic returns multi-block content (text + tool_use in one
  message, multiple blocks) — so this is a latent bug, not a working design.
- Outbound user messages are under-populated; `cache_ttl` is hardcoded to 1h
  inline (provider-specific config smuggled in next to content).
- Builder patterns are sprinkled inconsistently.

The unifying diagnosis: **nobody read the wire contract.** The fix is to make
the contract a type, in both directions, so the "only one element / only one
key" class of bug becomes *unrepresentable* (it's `Vec<ContentBlock>` where
`ContentBlock` is an enum).

## Architecture

### Crate boundary (the no-self-eating-watermelon rule)
**This crate knows NOTHING about mu.** It does not depend on `mu-core`. It
defines Anthropic's wire types and (de)serialization, full stop. The
`From<AnthropicMessage> for <mu type>` conversion lives in the CONSUMER
(`mu-core` or a thin adapter), because the consumer owns the mapping, not the
producer. Dependency direction is a strict DAG: `mu-core → mu-anthropic`,
never back. This is also what makes the crate reusable by external users —
they can't if it drags mu's internal model along. No-recursion and
reusability are the SAME constraint.

### Header / payload separation (the financial-spec model)
Model Anthropic's wire the way protocol/financial specs do: **envelope
separate from payload.**
- **Payload** = the message content. role, `content: Vec<ContentBlock>`.
  Knows nothing about cache, sampling, transport.
- **Envelope / request config** = the knobs. `model`, `max_tokens`, `system`,
  `tools`, headers (`anthropic-version`, betas), and cache-control. These are
  provider-specific, configurable, and NOT part of the message.
- **Request** = envelope + payload, assembled at construction, immutable
  after.

Open seam to settle from the spec (NOT now): Anthropic's cache_control is
**per-content-block** (breakpoints on specific blocks) while max_tokens/
temperature are **per-request**. So "config" may split: some binds to blocks,
some to the whole request. The hand-crafted code got this wrong by flattening
(one hardcoded ttl for everything). Modeling the granularity correctly is an
early spec-reading task.

### Construction & mutation
- **Parse-don't-validate / RAII**: if you hold an `AnthropicRequest`, it is
  already well-formed — invalid states are unrepresentable. No mid-pipeline
  `.modify()`.
- **Builder pattern** is allowed ONLY for genuine accumulation (you don't have
  all the pieces at once). The builder may hold a half-formed mess; `build()`
  / `collect()` is the only door out and returns a valid type (or
  `Result<Valid, _>`). No public type is constructible invalid except via the
  builder, and the builder is not sendable on the wire. Reach for it when
  accumulation is real, not by default.
- Provider config (cache_ttl etc.) enters at a NAMED door at construction
  time — as adjacent data on the envelope — not smuggled in by mutation. The
  hardcoded 1h becomes a documented default on the config type that callers
  override.

### Transport independence
`generate()` returns a `String` (or bytes); it is NOT bound to transport. The
string goes to a socket / fd / spsc / whatever — the provider produces bytes,
something else moves them. Symmetrically, `parse(&str) -> Result<_>` does not
care where the string came from. This independence is what makes the thing
testable against recorded reality without a live API (see Test tiers 3).

## Serialization — the actual hard part

The CALL is trivial: `serde_json::to_string(&obj)`. Python never touches
serde; nothing crosses FFI but a string. The difficulty is **making the
type's serialized output byte-match Anthropic's wire**, which is correctness,
not mechanism:

- **Tagging/shape**: content blocks are an internally-tagged union
  (`{"type":"text","text":...}`, `{"type":"tool_use","id","name","input"}`).
  serde can express this with `#[serde(tag="type", rename_all=...)]` — but the
  attributes must match Anthropic's tag names/casing exactly, verified against
  the spec. Wrong tag = valid-but-wrong JSON.
- **Omission/conditionals**: absent vs null matters; cache_control appears
  only on the breakpoint block; `system` may be string-or-array. Needs
  `skip_serializing_if`, `serialize_with`, sometimes a hand-written
  `impl Serialize`.

Whether serde's derive matches the wire, or we hand-write composable custom
serializers (write the message "shape", let serde serialize the leaves), is
**discovered on contact, not decided here.** The precedent is spline's
`restapi/src/serde_compat.rs`: when an external party owns the contract, do
NOT trust a derive (or an upstream crate's derive) to pick your wire bytes —
hand-write `serialize_with`/`deserialize_with` against the stable public API
and document the committed format in the module header.

## Test tiers (round-trip is necessary, NOT sufficient)

1. **Round-trip** `de(ser(x)) == x` — self-consistency only. We can be
   perfectly, symmetrically WRONG. Necessary, insufficient.
2. **Spec conformance** `our_output == documented_example` — we match what
   Anthropic SAYS. Pure Rust unit test, no network, no Python:
   `assert_eq!(to_json(&built), spec_example_json)`.
3. **Ground-truth golden** `our_output == captured_real_wire` — we match what
   Anthropic DOES. Captured via socat-in-front-of-cc / proxy full logging
   (see oracles). **This tier is the drift SENTINEL**: when it fails and our
   code is unchanged, it's announcing that Anthropic moved OR a dependency
   bumped and silently altered our bytes. War story: a polars 0.43→0.46 bump
   silently changed DataFrame serialization to Arrow IPC; the operator's
   string golden test caught it IN THE SUITE before it shipped to customers.
   The fix took a year — but the contract held the whole year because the test
   converted a silent break into a loud red. That is the entire value of
   tier 3: it buys time to fix correctly instead of shipping the break.

## Providers as oracles (and the corpus for later)

There is no to/from provider for cc or mu themselves. Put **socat in front of
claude-code** (or use a proxy with full logging) to capture what real
Anthropic traffic looks like. Two uses:
1. The ground-truth source for tier-3 golden tests (above).
2. Later, a free test corpus: feed captured response strings into `parse()`
   and assert round-trip — no live API, no spend.

## Python layer — "curl with extra steps" (LATER, not part of proving the lib)

Build and test the library FIRST, fully, in Rust. Python does not exist until
the question becomes "does Anthropic ACCEPT this byte string, and can we parse
the reply?" — a live-network question the lib can't answer alone. Python's
entire initial job: take the lib's generated string, put it on the wire, hand
the response back. curl-with-types. It can evolve; we do not design for that
now.

Layering (validated against spline: libspline → joshua(pyo3) → python wheel):
- `mu-anthropic` (this lib): pure Rust, serde, no transport, **no polars**.
  Full Rust unit tests; maybe Rust integration tests.
- `mu-anthropic-py` (separate pyo3 cdylib, path-dep on the lib): THIN.
  Only `#[pyfunction]`/`#[pymethods]` delegating 1:1 to the lib + error
  conversion. **No logic. No tests** (kept thin enough that the seam needs no
  test — enforced by the no-logic rule, see AGENTS.md).
- Python wheel: imports the lib, calls every exported function — THIS is what
  tests the binding. Use the modern maturin src-layout (no `python/<name>/
  <name>` double-nest from the pyo3-of-2024 era).
- Python deps: `requests`, maybe `datetime`. **No polars/pandas/scipy** —
  standing operator rule, hardened right now because the jails that held fat
  venvs keep getting destroyed. Keep it skinny.

polars appears ONLY later in a SEPARATE metrics wheel that consumes parsed
events and tabulates them (polars-in-Rust, LazyFrames across FFI — never a
pip dependency). Not this crate.

## Build order

1. Specs DOWN first — download Anthropic Messages API specs into the
   `specifications/` dir in this crate. They have evidently never been read;
   reading them is step zero.
2. The smallest thing that proves types-as-contract: ONE content-block enum,
   its serde impl, and `to_json(&block) == documented_example`. Decide
   derive-vs-hand-written ON CONTACT here.
3. Outward from the minimal message set: connect / simplest request / parse
   one response. Then end-session. Then breadth.
4. Wire the crate into the workspace `members` only once it builds and the
   tier-2 tests pass. (Until then it's a staked, unwired directory.)
5. Stand up socat/proxy capture; add tier-3 golden tests against real traffic.
6. Only then: `mu-anthropic-py` + the curl-Python live test.

## Open questions (find out on contact — do NOT pre-decide)

- Does serde's internally-tagged enum byte-match Anthropic's content blocks,
  or do we hand-write `serialize_with`? (Step 2 answers this.)
- Is config one struct or split block-vs-request? (Spec reading answers this.)
- Arc vs copy for shared pieces? (Defer entirely until something is actually
  shared — premature now.)

## Migration posture

Build in PARALLEL to the existing hand-crafted path; mu migrates ONTO this
crate only once it's proven. Do NOT refactor the live provider path in place —
that is the exact condition that produced the mess (and the 8-10 littered
spawn versions). Greenfield + specs + types-as-contract is what breaks the
pattern. Keep mu usable the whole time.
