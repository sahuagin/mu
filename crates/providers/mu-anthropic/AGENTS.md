# AGENTS.md — mu-anthropic

You are about to work on `mu-anthropic`. Read this and `PLAN.md` before
writing a line. This file is what a previous session wished it had known
going in. It is the HOW and the WHY-the-rules-exist; `PLAN.md` is the WHAT.

This crate is GREENFIELD ON PURPOSE. It exists because mu's in-place provider
path was built by hand, without specs, and accreted into a mess (and spawned
8-10 redundant versions of related machinery). The whole point of a clean
sibling crate is to build to a *contract* instead of editing a *mess*. If you
find yourself "making it fit the existing code," stop — that is the failure
mode this crate was created to escape.

## The rules (these are load-bearing; violating one regenerates the mess)

1. **This crate imports NOTHING from mu.** No `mu-core`, no mu types, no mu
   conventions leaking in. It models Anthropic's wire protocol and nothing
   else. The `From<AnthropicMessage> for <mu type>` conversion lives in the
   CONSUMER, not here. Dependency direction is a one-way DAG: `mu-core →
   mu-anthropic`, never back. WHY: the reverse creates a self-eating
   watermelon — a semantic cycle where the normalized type and the provider
   type each define themselves in terms of the other — AND it destroys the
   reusability that justifies the crate (external users can't depend on it if
   it drags mu along). No-recursion and reusability are the same constraint.
   The test when tempted: "am I about to make this crate know about mu?" If
   yes, the code belongs on the other side of the boundary.

2. **Specs DOWN and READ before code.** The Anthropic Messages API specs go
   in `specifications/`. They have evidently never been read by whoever wrote
   the current path — that is the root cause of every downstream bug. You do
   not get to pattern-match one example response. Read the contract.

3. **Types-as-contract, both directions.** The job is to make the serialized
   form of a type byte-MATCH Anthropic's wire, so that wrong shapes become
   unrepresentable. `content` is `Vec<ContentBlock>` where `ContentBlock` is
   an enum — NOT a single struct, NOT "the first element." The current code's
   "only one array element, only one dict key" assumption is the exact bug we
   are killing; do not reintroduce it.

4. **Parse-don't-validate. No mid-pipeline mutation.** If a value of a public
   type exists, it is already valid. No `.modify()` step. Provider config
   (cache_ttl, max_tokens, headers) enters at a NAMED door at construction
   time, on the envelope — never smuggled in by mutation or hardcoded inline
   (the current code hardcodes `cache_ttl=1h` next to the content; that's the
   anti-pattern). Builders are allowed ONLY for genuine accumulation, with
   `build()`/`collect()` as the single valid exit; the builder is never sent
   on the wire.

5. **Envelope separate from payload** (financial/protocol-spec model).
   Message content is payload; model/max_tokens/system/tools/headers/
   cache-control are envelope. Don't flatten them — flattening is how the
   current code lost the per-block-vs-per-request distinction in cache
   control.

6. **Transport independence.** `generate() -> String` and `parse(&str) ->
   Result<_>`. Neither touches a socket. Something else moves the bytes. This
   is not optional polish — it's what makes tier-3 testing against recorded
   traffic possible without a live API.

7. **Don't trust a derive (or an upstream crate) to pick your wire bytes
   when a foreign party owns the contract.** serde's `#[derive]` MIGHT match
   Anthropic; it might not. Find out by `to_json` + eyeball against the spec
   (this is empirical — stop theorizing, run it). When derive can't express
   the rule, hand-write `serialize_with`/`deserialize_with` against stable
   public types and document the committed format in the module header. WHY:
   see the `serde_compat.rs` war story below — an upstream crate silently
   changed the operator's wire format via a derive, out to customers.

8. **No polars/pandas/scipy/numpy anywhere in this crate or its eventual
   Python binding.** Standing operator rule, currently HARD because the jails
   that held fat venvs keep getting destroyed/sabotaged, so a heavy venv is
   not available to lean on. The lib is pure Rust serde. The eventual Python
   layer is "curl with extra steps": `requests`, maybe `datetime`, nothing
   else. polars belongs ONLY in a future SEPARATE metrics wheel, never here.

9. **The pyo3 binding (when it exists) is THIN and contains NO logic.** Only
   `#[pyfunction]`/`#[pymethods]` delegating 1:1 to the lib, plus error
   conversion. WHY this is a hard rule and not a preference: the testing
   strategy is "test the Rust side, test the Python side, DON'T test the
   binding seam." That only stays safe if the seam holds no logic to be wrong.
   In the operator's other project (spline/joshua) the binding crate grew to
   ~80KB because a human was making judicious push-down calls; an agent
   without that judgment will pile testable logic into the untested seam. So:
   no branching beyond error conversion, no parsing, no logic. Keep it
   mechanical and the "no seam tests" decision stays true by construction.

## Test tiers — round-trip is NECESSARY but NOT SUFFICIENT

1. round-trip `de(ser(x)) == x`: self-consistency. You can be symmetrically
   WRONG and pass this. Never rely on it alone.
2. spec conformance `to_json(&built) == documented_example`: matches what
   Anthropic SAYS. Pure Rust, no net.
3. ground-truth golden `our_output == captured_real_wire`: matches what
   Anthropic DOES (captured via socat/proxy — see PLAN). This is the DRIFT
   SENTINEL. If it goes red with NO code change on our side, the message is
   "upstream moved (Anthropic, or a dependency bump)" — and finding that out
   from a red test instead of from production IS the point.

## Note to future you — context I wish I'd had going in

- **mu and claude-code logs/protocols are TWO DIFFERENT TOOLS' constructs.**
  They look similar enough to fool you. cc says `tool_use`; mu says
  `tool_call`. cc tools are `Bash/Read/Grep` (capitalized, plus `mcp__*__*`);
  mu's are lowercase. cc timestamps are ISO; mu uses `timestamp_unix_ms`. If
  you build the cc oracle, expect the wire to resemble Anthropic's real API
  (cc is "a reasonable facsimile") but VERIFY rather than assume.

- **The operator has already built this architecture once** — `spline`
  (Rust→pyo3→Python→polars), in another workspace. The layout we're copying
  (lib crate → thin cdylib binding → Python wheel; polars only where DataFrames
  are actually needed) is proven there, not invented here. The ONE thing to
  change vs spline: use the modern maturin src-layout to avoid the
  `python/<name>/<name>` double-nest (that stutter is a pyo3-of-2024 artifact;
  newer pyo3/maturin fixed it). Do not reproduce the stutter.

- **The `serde_compat.rs` war story (why rule 7 exists):** in spline, polars
  0.43 serialized a DataFrame as column-oriented JSON; polars 0.46 silently
  changed `DataFrame::Serialize` to emit raw Arrow IPC bytes. That would have
  broken the operator's customer API contract — but a string golden test
  caught it IN THE SUITE before it shipped. The proper fix (hand-written
  `serialize_with`/`deserialize_with` pinning the committed format against the
  stable public API) took a YEAR to land, but the contract held the whole year
  because the test had converted a silent breaking change into a loud red one.
  THAT is why we hand-write wire serialization and golden-test it: not for
  elegance, but because a foreign party (or your own `cargo update`) WILL
  change the bytes underneath you, and the only question is whether you find
  out from a test or from production.

- **Why "specs first, smallest-thing-end-to-end, then breadth" is non-
  negotiable:** the mess this crate replaces came from building the critical
  path repeatedly without a spec. The discipline that prevents version 11 is:
  one content-block enum + its serde + a `to_json == documented_example` test,
  PROVEN, before any breadth. Resist the urge to scaffold the whole API
  surface up front.

- **Posture with the operator:** plan/decide before acting. This crate was
  designed in a planning conversation where the agent was explicitly NOT to
  write/edit/commit/spawn without a green light, and the operator watched for
  the announce-then-act drift signature. Honor that. Reads to ground yourself
  are fine; writes/commits/spawns wait for an explicit go.

- **Spawning:** sub-agents (Explore/general-purpose via the Agent tool) work
  fine and are useful for read-only codebase mapping (the integration map was
  built that way). The old warning about broken *mu* spawn machinery was about a
  different system; for protocol/integration work, in-session Agent sub-tasks
  are fair game.

## Where things are

- `PLAN.md` — the build plan (what + order + open questions).
- `specifications/` — Anthropic API specs go HERE, downloaded, before code.
- The reference implementation of the Rust→Python→polars pattern is the
  operator's `spline` workspace (`libspline` lib, `joshua` pyo3 crate,
  `restapi/src/serde_compat.rs` for the hand-written-wire-serde precedent).
  Ask before assuming its paths; it lives outside the mu repo.

## Status

**Built and full-protocol-complete, in the workspace, merged to `main`** (PRs
#291–#298, #304). The library models the entire Messages wire protocol (request
envelope + all params + tool taxonomy, every content block incl. server-tool
results + multimodal, response, streaming, usage, citations); 86 unit + 3
integration tests; a drift canary (`examples/drift_check.rs`) guards it. The
rules above are still load-bearing — they're WHY the build stayed clean; keep
honoring them for any extension.

The remaining work is the **mu-side integration** — see `HANDOFF.md` (current
state) and `INTEGRATION-PLAN.md` (the step-by-step). PLAN.md's build phases are
done (historical now).
