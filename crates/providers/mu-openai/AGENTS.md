# AGENTS.md — mu-openai

You are about to work on `mu-openai`. Read this and `INTEGRATION.md` before
writing a line. This file is the HOW and the WHY-the-rules-exist;
`INTEGRATION.md` is the seam map (where this crate meets mu).

This crate is a **standalone, strongly-typed model of the OpenAI Responses API
wire protocol** (`POST /v1/responses`). It exists for the same reason its
sibling `mu-anthropic` does: mu's in-place OpenAI path was built by hand,
without specs, and accreted into a mess (three overlapping files —
`openai_codex.rs`, `openai_api.rs`, `openai_responses.rs` — see
`INTEGRATION.md` §1). The point of a clean sibling crate is to build to a
*contract* instead of editing a *mess*. If you find yourself "making it fit the
existing mu code," stop — that is the failure mode this crate was created to
escape.

This is the SECOND attempt. The first (now superseded) version leaked transport
into the library — it carried a `reqwest` client, did its own SSE byte framing,
and read `MU_*` env vars. Those were cleanroom violations and have been removed.
Do not reintroduce them (see rule 6).

## The rules (these are load-bearing; violating one regenerates the mess)

1. **This crate imports NOTHING from mu.** No `mu-core`, no `mu-ai`, no mu
   types, no mu conventions leaking in. Its only dependencies are `serde`,
   `serde_json`, `thiserror`, and `futures` (plus `tokio` as a dev-dependency
   for the async tests). It models OpenAI's wire protocol and nothing else. The
   `From<mu type> for mu_openai::CreateResponseRequest` and the inverse
   narrowing live in the CONSUMER (`mu-ai`), not here. Dependency direction is a
   strict one-way DAG: `mu-ai → mu-openai`, never back. WHY: the reverse creates
   a semantic cycle (the normalized mu type and the provider type each defined
   in terms of the other) AND it destroys the reusability that justifies the
   crate — external projects can depend on it precisely because it drags no mu
   along. No-recursion and reusability are the same constraint. The test when
   tempted: "am I about to make this crate know about mu?" If yes, the code
   belongs on the other side of the boundary, in `mu-ai/src/providers/openai.rs`.

   This is also why `JsonValue` and `FiniteF64` are **reimplemented locally, not
   imported** from `mu-core`. They mirror mu-core's `ToolArgs` pattern, but
   importing them would breach the DAG. Re-deriving a 60-line invariant type is
   the correct price of independence.

2. **Specs DOWN and READ before code.** The OpenAI OpenAPI spec is vendored,
   time-pinned, in `specifications/` (see `specifications/MANIFEST.md`). The
   schemas of record are `CreateResponse`, `Response`, `ResponseStreamEvent`,
   `ReasoningItem`, `FunctionTool`, and the `response.*` streaming events. You do
   not get to pattern-match one example response. Read the contract — it stays
   navigable while xz-compressed (`xzgrep '/responses' openapi.yaml.xz`).

3. **Types-as-contract, both directions.** The job is to make the serialized
   form of a type byte-MATCH OpenAI's wire, so wrong shapes become
   unrepresentable. `input` is `Vec<InputItem>` where `InputItem` is an enum
   (message / function_call / function_call_output / reasoning) — NOT a single
   struct, NOT "the first element." `output` is `Vec<OutputItem>` the same way.
   The old code's "one item, one shape" assumptions are the exact bugs we are
   killing; do not reintroduce them.

4. **Parse-don't-validate. No mid-pipeline mutation.** If a value of a public
   type exists, it is already valid. `FiniteF64` cannot hold NaN/±Inf; `JsonValue`
   cannot hold a non-finite number anywhere in its tree — both reject the bad
   state at construction and at `Deserialize`, then honestly assert `Eq`. There
   is no `.modify()` step and no inbound revalidation. The builder-style helpers
   on `CreateResponseRequest` (`with_tools`, `with_reasoning`, `streaming`, …)
   are convenience constructors, not a mutate-then-send pipeline; the request you
   hold is always a valid request.

5. **Envelope separate from payload.** `CreateResponseRequest` is the ENVELOPE
   (model, instructions, tools, tool_choice, reasoning, sampling knobs, stream,
   store, include, metadata). `input: Vec<InputItem>` is the PAYLOAD (the
   conversation so far). Don't flatten them. Note specifically:
   `instructions` is a TOP-LEVEL envelope field, **not** a message item — the
   Responses API moved the system prompt out of the message array. Putting it
   back in `input` is wrong.

6. **Transport independence — NO sockets in the lib.** This crate serializes
   request types to JSON and deserializes wire JSON to response/event types.
   That is the whole job. It contains NO `reqwest`, NO HTTP client, NO SSE
   byte-line framing, NO `MU_*` (or any) env var, NO auth, NO endpoint
   selection, NO cancellation. The `accumulate()` function consumes an already-
   decoded `Stream<ResponseStreamEvent>` — it never touches a wire. Something
   else (mu) moves the bytes and frames the SSE `data:` lines. This is not
   optional polish: it is what makes tier-3 testing against recorded traffic
   possible without a live API, and it is the exact line the prior version
   crossed. If you are reaching for an HTTP crate, you are writing code that
   belongs in `mu-ai`, not here.

7. **Don't trust a derive (or an upstream crate) to pick your wire bytes when a
   foreign party owns the contract.** serde's `#[derive]` MIGHT match OpenAI; it
   might not. Find out by `to_value` + eyeball against the spec and against
   captured traffic (empirical — stop theorizing, run it). The serde attributes
   here are load-bearing and deliberate, e.g.: `InputItem` / `OutputItem` are
   `#[serde(tag = "type", rename_all = "snake_case")]`; `FunctionTool` is the
   "flat" Responses shape (name/description/parameters at the tool top level, NOT
   nested under `function`); `ToolChoice` is `#[serde(untagged)]` so the mode
   string and the named-function object both round-trip; function-call
   `arguments` is a JSON **string** on the wire, not an object; optional fields
   `skip_serializing_if` so they are OMITTED, never sent as `null`. When a derive
   can't express the rule, hand-write it (`deserialize_option_finite` is the
   precedent) and document the committed format in the module header.

8. **Forward-compat asymmetry: OUTBOUND closed, INBOUND open — but with NO
   `extra` catch-all on response types.** We only ever *construct* requests, so
   the request enums are CLOSED (no `Unknown` fallback) — a request we can't name
   is a bug, not wire data. INBOUND types we *receive*, so they tolerate the
   unknown: `ResponseStatus` has `#[serde(other)] Other`; `OutputItem`,
   `OutputContent`, and `ResponseStreamEvent` each have an untagged
   `Unknown(JsonValue)` so a new item/event kind degrades instead of failing the
   parse. CRUCIALLY: the modeled inbound *fields* have NO catch-all `extra` map.
   That is deliberate — it is what makes the drift canary work. A field OpenAI
   adds to a response that we don't model is DROPPED on round-trip, and the
   canary's re-serialize-and-diff catches the drop as a loud red test. An `extra`
   map would silently absorb the addition and blind the canary. Do not add one.

9. **Reasoning-item threading is a modeled shape, not a behavior here.** The
   o-series / gpt-5 Responses contract: a `reasoning` item returned in a prior
   response's `output` (id + encrypted_content + summary) must be fed back
   verbatim as an `InputItem::Reasoning` on the next turn (with
   `include: ["reasoning.encrypted_content"]` when stateless, `store=false`), or
   the model loses its chain-of-thought across tool calls and can stall. This
   crate models BOTH shapes — `OutputItem::Reasoning` (inbound) and
   `InputItem::Reasoning` (outbound) — fully, not just the `id`. The mu provider
   does the round-trip; the crate just makes the round-trip *expressible*.

## Test tiers — round-trip is NECESSARY but NOT SUFFICIENT

1. **round-trip `de(ser(x)) == x`:** self-consistency. You can be symmetrically
   WRONG and pass this. Never rely on it alone. (Present today in every module's
   `#[cfg(test)]` block.)
2. **spec-exact shape `to_value(&built) == documented_example`:** matches what
   OpenAI SAYS, against the vendored spec. Pure Rust, no net. (E.g.
   `minimal_text_request_shape`, `function_tool_is_flat_with_optional_strict`.)
3. **ground-truth golden `our_output == captured_real_wire`:** matches what
   OpenAI DOES — fixtures in `tests/fixtures/` captured from real wire traffic
   (both the public `api.openai.com` path and the ChatGPT/Codex backend, which
   forks some spellings — see the `function_call.arguments.delta` compat variant
   in `stream.rs`). This is the DRIFT SENTINEL. If a golden goes red with NO code
   change on our side, the message is "upstream moved (OpenAI, or a `cargo
   update`)" — and learning that from a red test instead of from production IS
   the point.

The **drift canary** (`examples/openai_drift_check.rs`, driven by
`scripts/openai-protocol-canary.sh` from cron) replays captured responses
through the types and flags any deviation. Because inbound types have no `extra`
catch-all (rule 8), a newly-added upstream field surfaces as DROPPED on
round-trip and trips the alarm.

## Build methodology — greenfield vertical slices, leaf-first

Build leaf-first, one complete slice per commit. A slice = **one type + its
serde + its tests** (round-trip, spec-exact, golden where it applies). It
compiles, tests pass, it's committed — that is the done-gate. No half-types, no
1000-line file to edit; the crate is always coherent and small enough to hold in
head. Parse-don't-validate throughout. Tag-dispatch via serde attributes with an
untagged `Unknown` fallback on inbound enums; closed enums on outbound request
types. The dependency layering of the modules is itself the slice order:
`finite` / `json` (leaves) → `request` / `response` → `stream` (which reuses
`response` types in its events) → `accumulate` (which folds `stream` events back
into a `response`).

## Where things are

- `INTEGRATION.md` — the seam map: what this crate owns vs. what stays mu-side,
  the outbound/inbound diagram, and the three mu-ai files it replaces.
- `specifications/` — the vendored, time-pinned OpenAPI spec + its `MANIFEST.md`.
  Read it before changing a type.
- `src/` — `lib.rs` (public re-exports), `request.rs`, `response.rs`,
  `stream.rs`, `accumulate.rs`, `json.rs`, `finite.rs`. Each module owns its own
  `#[cfg(test)]` tier-1/tier-2 tests.
- `tests/fixtures/` — captured real wire traffic for the tier-3 goldens (the
  drift sentinel).
