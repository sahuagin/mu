# mu: goal session 2026-05-14d (mu-fb0)

Adopt `ProviderRenderer` + `CacheStrategy` in the live agent loop —
the mu-ktq / mu-nat / mu-bn4 substrate goes live in production.

## What landed

| Commit | Bead | One-line |
|---|---|---|
| (this session) | mu-fb0 | Live agent loop now projects session state into a `RetainedRope` and runs it through `Provider::renderer()` + `Provider::cache_strategy()` before each model call; `AgentEvent::ContextAssembly` gains renderer/strategy/span/cache-boundary provenance. |

Branch: `agent/fb0-live-loop-2026-05-14` (pushed to `sahuagin/mu`).
PR: opened against `main` — operator merges; **do not merge from
the worker session**.

## Test state

- `cargo check --workspace --all-targets`: green.
- `cargo test --workspace`: 447 passed, 0 failed across 14 binaries
  (107 + 115 + 4 + 9 + 204 + 4 + 1×4 = workspace total).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: clean.
- `cargo fmt --all -- --check`: clean.

Five new fb0-tagged tests in
`crates/mu-ai/src/providers/anthropic_tests.rs:825-1024` assert the
rope projection's role sequence, content surfaces, message count,
and cache-boundary placement match what `build_request_body`
produces for the Anthropic wire body. Existing Anthropic + agent-
loop fixtures pass byte-for-byte — the wire path
(`Provider::stream(system_prompt, &[AgentMessage], &[ToolSpec], …)`)
is unchanged.

## Goal status

- mu-fb0: **complete on branch** (PR open, awaiting operator merge).
- Sub-beads closed: 1.
- Sub-beads still open: none in this goal's worklist.

## Resolved design questions (per experiment spec § "Design questions")

1. **Rope storage** — per-turn projection from `messages`.
   `messages: Vec<AgentMessage>` stays canonical (external input
   lands there; `Provider::stream` still consumes it). The rope is
   built by `crate::context::assemble_rope(system_prompt, messages,
   tool_specs)` at each model call. Storing the rope as a field is
   correct in the long term when events become first-class; for the
   transition, building from the source of truth keeps the new path
   trivially equivalence-preserving.
2. **`CacheStrategy` dispatch** — trait method on `Provider`. Each
   impl declares its preferred renderer + strategy pair via
   `fn renderer() -> Arc<dyn ProviderRenderer>` and
   `fn cache_strategy() -> Arc<dyn CacheStrategy>`, both with
   default impls (`FauxProviderRenderer` / `NoCacheStrategy`).
   `AnthropicProvider` overrides; the other providers (Faux,
   OpenRouter, OpenaiCodex) inherit the defaults and compile
   unchanged.
3. **`ContextAssembly` event shape** — extended with five optional
   fields (`renderer`, `cache_strategy`, `span_count`,
   `cache_boundary_count`, `first_span_ids[<=5]`). All
   `serde(default, skip_serializing_if)` so existing fixtures
   serialize identically. `first_span_ids` capped at 5 to bound
   event-log row size; the full rope is reconstructable from the
   `SessionEventLog` walk (spec lines 167-228).
4. **Phased vs full cutover** — single commit. Trait additions are
   default-impl backward-compatible; the `Provider::stream`
   signature is unchanged (wire body byte-for-byte stable). The
   rope-projected `ProviderMessages` are observed (their content +
   cache markers describe what the model sees), but the wire
   request still travels the AgentMessage path — preserving stop-
   criterion #9.

## Stop criteria that fired

None. The work proceeded green at every checkpoint
(`cargo check` → `cargo test` → `cargo clippy` → `cargo fmt`,
in per-step verification order per mu-vw3).

## Capability invariant audit

| Invariant | Held? | Notes |
|---|---|---|
| INV-1 (`AutonomyCapability::Disallowed` is default) | Y | Not touched. |

## Spec drift check

- All trait / wire / `EventPayload` changes have matching spec
  updates? **Y.** `specs/architecture/event-sourced-context.md`
  gains a "Live-loop adoption (mu-fb0)" section documenting the
  three `Provider` trait method additions, the per-call pipeline,
  the design-question resolutions, and the equivalence guarantee.
  Changelog entry dated 2026-05-14.

## Things noticed but not addressed

- **Threading `ProviderMessages` into `Provider::stream`.** Today
  the renderer's output is computed and observed but the actual
  wire request still travels through `&[AgentMessage]`. A future
  bead can change the trait signature once a consumer wants the
  wire-shape change (e.g., when OpenAI's renderer needs the
  flattened content surface). The mu-fb0 boundary chosen here is
  the safest — preserves stop-criterion #9 (no wire-protocol
  surface change) while wiring all the rope plumbing.
- **`token_count_estimate` left None.** The spec hints at a
  future tokenizer hook (`ContextAssembly` payload doc-comment in
  `event_log.rs`); this bead does not wire one. Tokenization is
  per-provider — a follow-on can land Anthropic's claude-tokenizer
  output here.
- **`provider_label()` returns `&'static str`.** Could become a
  more structured `ProviderIdentity { renderer, strategy }` if
  A/B testing of strategy choice (independent of renderer)
  becomes a thing. The pair is reported as two fields in
  `ContextAssembly` already, so the upgrade is mechanical.

## Suggested next session

- **mu-3aa** (OpenAIProviderRenderer): now unblocked. The
  trait-method dispatch pattern lets OpenAI declare its own
  renderer the same way Anthropic does.
- **mu-ovl** (Agent-view vs operator-view): the rope is now in
  the live loop, so an OperatorView consumer (TUI) can render
  the same rope through a different projection. The
  `FauxProviderRenderer` already implements the `OperatorView`
  per-kind shapes (`crates/mu-core/src/context/renderer.rs:259`)
  — that's the reference projection a TUI can target.
- **mu-x9j** (Subagent context handoff): also now unblocked —
  the rope is the artifact a subagent inherits from a parent
  per spec lines 646-680.

## Cost / turns / wall-clock

- Budget cap: $20. Actual: see Claude Code session summary at
  termination.
- Wall-clock: started ~mid-afternoon 2026-05-14 by goal-protocol
  experiment 7d; total turn-time is recorded in the experiment
  doc's "actuals" section.
- Subagents used: none (the work was small enough to keep in the
  primary context — substrate already in place, only the loop
  wiring + spec amend + tests needed to land).

## DO NOT MERGE

Per experiment 7d kickoff: "operator authorized autonomous work,
do NOT merge". PR is opened; operator reviews and merges.
