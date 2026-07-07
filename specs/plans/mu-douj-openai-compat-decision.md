# mu-douj: OpenRouter / OpenAI-compat extraction — decision memo

| field | value |
| --- | --- |
| status | decision memo — awaiting operator decision |
| last terrain check | 2026-07-06 |
| starting revision | `xpovozxv` / git `6c017f53` (`land-jul2-line`, merge: land the 2026-07-02 line) |
| checks run | full reads of `openrouter.rs` / `ollama.rs` / `vllm.rs`, structural scan + targeted reads of `openai.rs` / `anthropic.rs`, `crates/providers/mu-openai` layout + `lib.rs`, `factory.rs` provider routing, `jj file log` churn on `openrouter.rs`, bead `mu-douj` full comment trail from central beadsd, specs mu-017 / mu-019 headers, `loop_/mod.rs` mu-w8ap guard, `catalog_probe.rs` num_ctx probe |
| bead | `mu-douj` (P1, created 2026-06-07) |
| roadmap item | current-state doc "high-leverage next sequence" item 3 (`specs/plans/mu-claude-code-replacement-current-state.md:175-177`) |
| decision owner | operator (see OPERATOR DECISION items at the end) |

The roadmap item: *"Decide OpenRouter/OpenAI-compat extraction — either extract
a shared chat-completions layer or explicitly document why OpenRouter stays
separate from the typed Responses crate."* This memo is the decision input. It
is a spec/terrain document, not a refactor; no provider code changes ride with
it.

Claims are labeled **Observed** (verified against files at the starting
revision, with file:line) or **Inferred** (reasoned from observed terrain).

## 1. The question, restated against live terrain

Bead mu-douj Part 1 (written 2026-06-07) asked to extract an OpenAI-compatible
request-building/translation layer (`providers/openai_compat.rs`) so
`openai_codex.rs` ("1345 lines, partially parallel logic") could share what
`ollama.rs` shared "via composing OpenRouterProvider (~800 lines avoided)."

**Every load-bearing premise of Part 1 is stale.** The real remaining question
is the roadmap-item form: does the hand-rolled chat-completions wire in
`openrouter.rs` get (a) extracted into a shared module, (b) typed as a wire
crate parallel to `mu-openai`/`mu-anthropic`, or (c) documented as
intentionally separate?

## 2. Terrain findings

### 2.1 The bead premise is stale on all three counts

1. **`openai_codex.rs` no longer exists.** *Observed:* the providers directory
   contains `anthropic.rs`, `ollama.rs`, `openai.rs`, `openrouter.rs`,
   `vllm.rs`, plus shared modules (`sse.rs`, `tool_dialect.rs`,
   `output_limits.rs`) — no `openai_codex.rs`
   (`crates/mu-ai/src/providers/`). Codex is one of two auth modes inside
   `OpenaiProvider` (`openai.rs:116-130` `enum AuthMode { Codex {..}, Public
   {..} }`), constructed via `from_store`/`from_store_ephemeral`
   (`openai.rs:151-163`), routed from
   `crates/mu-coding/src/serve/factory.rs:129-141`.

2. **The "partially parallel logic" the bead wanted to deduplicate was
   eliminated by typing, not extraction.** *Observed:* `openai.rs` (1728
   lines) speaks the **Responses API** through the typed `mu-openai` wire
   crate (`openai.rs:1-21` module docs; `openai.rs:58-62` imports
   `mu_openai::{CreateResponseRequest, ResponseStreamEvent, ...}`).
   `crates/providers/mu-openai` is a standalone crate (1591 lines across
   `request.rs`/`response.rs`/`stream.rs`/`accumulate.rs`/`finite.rs`/`json.rs`)
   with an explicit no-mu-dependency rule (`mu-openai/src/lib.rs:1-13`:
   "Standalone. Knows nothing about mu... dependency direction is a strict DAG
   (`mu-ai → mu-openai`, never back)").

3. **`ollama.rs` no longer composes `OpenRouterProvider`.** *Observed:*
   `ollama.rs:1-22` — ollama (>= v0.14) serves the Anthropic Messages API at
   `/v1/messages`, and `OllamaProvider` composes `AnthropicProvider`
   (`ollama.rs:75-79`, `ollama.rs:96-98`), explicitly noting "(bead mu-fmas;
   was mu-818c, which composed the OpenAI-compat `OpenRouterProvider`)". The
   switch bought native `tool_use`, thinking blocks, top-level `system`, and
   fixed the mu-mdds dropped-reasoning class. The bead's own comment trail
   (comments 211/214/216, 2026-06-07) had already predicted and argued for
   exactly this move ("door 3").

### 2.2 Current provider topology (Observed)

| provider | wire | wire ownership | lines | composes |
| --- | --- | --- | --- | --- |
| `anthropic.rs` | Anthropic Messages `/v1/messages` | typed crate `mu-anthropic` | 1214 | — |
| `openai.rs` | OpenAI Responses (`/v1/responses`, codex backend) | typed crate `mu-openai` | 1728 | — |
| `openrouter.rs` | OpenAI chat-completions (SSE deltas-by-index) | hand-rolled `serde_json::Value` + local serde structs | 1038 | — |
| `vllm.rs` | OpenAI chat-completions | — | 142 | `OpenRouterProvider` (`vllm.rs:40-53`) |
| `ollama.rs` | Anthropic Messages | — | 551 | `AnthropicProvider` (`ollama.rs:75-104`) |

Shared modules already extracted: `sse.rs` (SSE byte framing, used by all
HTTP providers), `tool_dialect.rs` (text-dialect tool-call rescue, applied on
both the OpenRouter/vLLM chat-completions path `openrouter.rs:265-278` and the
ollama Anthropic path `ollama.rs:355-368`), `output_limits.rs` (catalog-driven
`max_tokens`, used by Anthropic- and OpenAI-shaped request bodies,
`output_limits.rs:1-14`).

**Key structural fact:** `OpenRouterProvider` is already the shared
chat-completions engine. It has overridable `api_base`/`api_path`
(`openrouter.rs:94-103`) designed for OpenAI-compatible local backends
(`openrouter.rs:29-32`, `openrouter.rs:53-66`), and `VllmProvider` composes it
in 142 lines total. There is exactly **one** implementation of the
chat-completions wire in the workspace; nothing else re-implements it.
Consumers: `factory.rs:142-149` (openrouter + vllm selectors),
`examples/compaction-bench.rs:248`.

### 2.3 What duplication actually exists today

**Chat-completions-specific duplication: none.** *Observed:* no other file
builds chat-completions request bodies or parses `choices[].delta` chunks.
The ~800-line-class duplication the bead targeted was resolved by composition
(vllm) and by the ollama wire switch (mu-fmas). Extracting
`providers/openai_compat.rs` today would move ~635 lines out of
`openrouter.rs` (request side `openrouter.rs:216-477`, projected path
`openrouter.rs:501-672`, response side `openrouter.rs:678-1034`) and hand them
to **the same two consumers that already share them** — a relocation, not a
deduplication. Net lines: positive (new module boilerplate), dedup: zero.

**What IS duplicated is wire-agnostic mu-side translation helpers, three ways
across the three wires** (*Observed*, near-identical bodies):

1. `parse_tool_input` — accumulated tool-argument JSON → `ToolArgs` with
   non-object/parse-failure/non-finite fallbacks:
   `anthropic.rs:1180-1210`, `openai.rs:994-1016`, `openrouter.rs:1012-1034`.
   ~25 lines × 3.
2. System-span hoisting loop — hoist all `ProviderRole::System` spans except
   `tool-schema:*`, blank-line-joined, in the projected path:
   `anthropic.rs:594-613` (inside `map_provider_messages`),
   `openai.rs:632-654` (inside `translate_provider_messages`),
   `openrouter.rs:513-541` (inside `translate_provider_messages_openrouter`).
   ~25 lines × 3.
3. Tool-result call-id recovery + error re-encoding —
   `extract_call_id_from_span_id(source_span_ids[0])` +
   `strip_prefix("error: ")` → `"[error] {content}"`:
   `anthropic.rs:655-665` (approx., inside `map_provider_tool_result`),
   `openai.rs:678-693`, `openrouter.rs:630-645`. ~15 lines × 3.

Total literal duplication ≈ **150–200 lines across three files**, all of it
*downstream-of-wire* mu logic (identical regardless of wire format). The
streaming skeletons (`ToolCallBuilder`/`StreamState`/`unfold`-loop at
`openrouter.rs:795-980` vs `openai.rs:794-891` vs `anthropic.rs:880-1143`) are
*structurally parallel but materially different* — different event grammars,
different state fields (`snapshot_content`/`streamed_reasoning` vs
`finish_reason`/`accumulated_thinking` vs Anthropic block builders). *Inferred:*
forcing those into one abstraction would be a generic-over-wire framework, the
kind of coupling the typed-crate DAG rule exists to avoid.

### 2.4 Churn and test coverage on openrouter.rs (Observed)

`jj file log` shows 22 commits touching `openrouter.rs` since creation
(2026-05-10), of which 5 landed in one feature batch on 2026-06-22 (per-turn
reasoning effort mu-13ve, catalog sampling mu-y8gp, system-prompt addendum
mu-g1f2, dialect rescue mu-xblz) and **zero since 2026-06-22** — two weeks
quiet at survey time. One genuine wire-parsing bug in its history: mu-mdds
(reasoning deltas silently dropped for lack of a serde field,
`openrouter.rs:750-761` now carries `reasoning`/`reasoning_content` aliases).
`openrouter_tests.rs` is 1214 lines including the `yqeq6_parity_*` suite that
asserts **byte-identical** wire JSON between the Legacy and Projected request
paths (`openrouter.rs:119-127`, `openrouter.rs:490-494`). *Inferred:* that
parity contract makes any extraction/refactor of the request side a
test-migration project as much as a code move — and is also exactly the safety
net that would make one mechanically checkable.

### 2.5 Why the Responses crate cannot absorb OpenRouter (Observed)

The roadmap item's "why OpenRouter stays separate from the typed Responses
crate" has a concrete wire answer:

- **Request shape:** chat-completions takes a `messages` array with an inline
  `{role: "system"}` message (`openrouter.rs:445-477`); Responses takes
  `input` items plus a top-level `instructions` field with codex's ~8 KB soft
  cap and overflow-to-input workaround (`openai.rs:87-109`,
  `openai.rs:538-561`).
- **Streaming shape:** chat-completions streams untyped-ish
  `choices[].delta` chunks accumulated by index with a `data: [DONE]`
  sentinel (`openrouter.rs:739-793`, `openrouter.rs:887-899`); Responses
  streams a typed event lifecycle (`response.output_item.added/done`,
  terminal response snapshots, encrypted reasoning items —
  `openai.rs:803-836`, `mu-openai/src/stream.rs`).
- **Semantics:** Responses threads encrypted reasoning across turns
  (`openai.rs:23-43`); chat-completions has no equivalent, and OpenRouter's
  `reasoning` knob is a request-side effort hint (`openrouter.rs:227-248`).

These are two different protocols that happen to share a vendor. `mu-openai`
typing the Responses wire says nothing about the chat-completions wire.

## 3. Options

### Option (a): extract `providers/openai_compat.rs` (the bead's Part 1 as written)

Move the ~635-line request/translation/stream layer out of `openrouter.rs`
into a shared module; `OpenRouterProvider` and `VllmProvider` consume it.

- *Observed:* the only consumers would be the two that already share the code
  via composition. No third chat-completions implementation exists to
  deduplicate.
- *Inferred:* net negative — new seam, renamed symbols, migrated tests
  (including 1214 lines of `openrouter_tests.rs` with byte-parity contracts),
  zero removed duplication. This option made sense in the 2026-06-07 world
  where ollama-composed-OpenRouter and a parallel `openai_codex.rs` both
  existed; that world is gone.

### Option (b): type the chat-completions wire — `crates/providers/mu-chatcompletions`

Build a third standalone wire crate on the `mu-anthropic`/`mu-openai` pattern
(request/response/stream/accumulate/finite/json, golden tests, drift-check
example), then rewire `openrouter.rs` onto it the way `openai.rs:16-21` welds
`mu-openai` to transport.

- *For (Inferred):* conforms to the repo invariant that wire protocols become
  standalone typed crates; the mu-mdds bug class (silently dropped serde
  fields) is exactly what golden/drift tests catch; chat-completions is the
  lingua franca wire (OpenRouter, vLLM, LM Studio, ollama's compat shim), so a
  typed crate has external-reuse value like its siblings.
- *Against (Observed + Inferred):* sibling crates run ~1600 lines each, so
  this is a ~1.5–2k-line build plus a 1038-line provider rewire plus parity
  test migration — for a provider that currently works, is quiet (no commits
  since 2026-06-22), and has heavy test cover. The chat-completions surface mu
  actually uses is small (messages, tools, stream deltas, usage chunk) and
  already fully exercised by `openrouter_tests.rs`. No second in-repo consumer
  of the *types* would exist (vllm consumes the *provider*, not the wire).
  Opportunity cost lands directly against the daily-driver roadmap items
  (dialogue receive, operator notifications, mu-solo UX).

### Option (c): keep OpenRouter separate; document the decision (this memo)

`openrouter.rs` stays the single hand-rolled chat-completions provider and the
designated composition target for any future OpenAI-compat backend
(base/path already overridable, `openrouter.rs:94-103`; vLLM as the worked
example, `vllm.rs:40-53`).

- *For (Observed):* the duplication Part 1 targeted no longer exists; sharing
  is already achieved by composition; the wire is genuinely distinct from the
  Responses wire (§2.5) so there is no unification with `mu-openai` to have;
  churn is currently near-zero.
- *Against (Inferred):* leaves one wire untyped and thus outside the
  golden-test drift discipline; the mu-mdds class remains possible on this
  wire (mitigated by the field aliases now present and by the parity suite).

### Carve-out (orthogonal to a/b/c): dedupe the wire-agnostic helpers

Extract the three literal three-way duplications of §2.3 into a small
`providers/translate_common.rs` (~60–80 lines shared, ~100–130 net lines
removed). *Inferred:* cheap, low-risk (pure functions, existing tests pin
behavior), and independent of whichever option is chosen. This is NOT the
bead's `openai_compat.rs` — it is cross-wire mu-side logic, not
chat-completions logic — and it should be its own small bead if wanted.

## 4. Recommendation

**Option (c), with the carve-out offered as an optional follow-up bead.**

Keyed to the repo's invariants:

- **Typed wire crates are standalone** — the invariant governs *how* a wire is
  typed when typing pays, not a mandate to type every wire. Both existing
  crates paid for themselves against high-value, high-subtlety wires
  (Anthropic: caching/thinking/tool blocks; Responses: encrypted reasoning,
  event lifecycle, codex quirks). The chat-completions surface mu uses is the
  smallest and most commodity of the three, has one implementation, one
  composition consumer, byte-parity tests, and near-zero churn. Type it when
  there is a second consumer of the *types* or sustained wire-drift pain —
  neither is observed today.
- **Provider abstraction lives in mu-ai; sharing is composition** — the
  established mechanism (vllm→openrouter, ollama→anthropic) already delivers
  what Part 1 wanted. `OpenRouterProvider` *is* the shared chat-completions
  layer; it just isn't named `openai_compat.rs`.
- **Frontends are hats over `mu serve`** — untouched by all options; noted
  only because no option may leak provider wire knowledge frontend-ward.
- **Terrain over maps** — the bead is a 2026-06-07 map of a repo that has
  since typed the Responses wire (mu-019 line), deleted `openai_codex.rs`, and
  moved ollama to the Anthropic wire (mu-fmas). Closing/reshaping the bead is
  the map correction.

Re-trigger conditions for revisiting option (b): a second first-class
chat-completions backend that cannot compose `OpenRouterProvider`, an external
consumer wanting the types, or a recurrence of the mu-mdds
silent-field-drop class that the parity suite fails to catch.

## 5. Part 2 seam analysis — ollama native `/api/chat` (still live)

Part 2 of mu-douj (per-request `num_ctx`, `keep_alive`, `format=json`) remains
motivated: it closes the silent-truncation class *constructively* rather than
by refusal.

- *Observed:* the mu-w8ap guard (`crates/mu-core/src/agent/loop_/mod.rs:104-130`)
  already fails closed for ollama when
  `predicted_input_tokens + output_reserve_tokens > context_hard_limit`, and
  `catalog_probe.rs:386-460` reads the *baked* `num_ctx` per model via
  `/api/show`. So today mu refuses over-window prompts instead of letting
  ollama clip them — correct but static.
- *Observed:* ollama now rides the Anthropic wire via composed
  `AnthropicProvider` (`ollama.rs:75-104`), and per the bead's own probe
  (comment 211) native `/api/chat` is "still the ONLY door with num_ctx" —
  the Anthropic and OpenAI compat shims accept no such field (upstream
  contribution possible per comment 216, ollama is open source).
- **What Part 2 needs from the chosen seam (Inferred):** nothing from
  `openrouter.rs` or from any chat-completions extraction — the OpenAI-compat
  door is closed terrain for ollama since mu-fmas. The needed seam is on
  `OllamaProvider`/`AnthropicProvider`: either (i) a native `/api/chat`
  request path selected when per-request options are wanted (new small wire;
  `tool_dialect.rs:19` already covers both endpoints' dialect leakage), or
  (ii) a request-body options hook on the composed Anthropic path plus an
  upstream ollama patch to accept `num_ctx` on `/v1/messages`, preserving
  thinking/tool fidelity. In both cases the `num_ctx` value should come from
  the same estimate mu-w8ap already computes (size-to-need per bead comment
  210: next step above estimated prompt + headroom, capped at
  `context_hard_limit`, never default-to-max).
- *Conclusion:* choosing option (c) neither blocks nor shapes Part 2. Part 2
  should be re-scoped as its own bead against the ollama/Anthropic seam
  (related open bead: `mu-ollama-provider-richer-base-rjxu`).

## 6. OPERATOR DECISION

The final call is the operator's. Line items:

1. **[ACCEPT/REJECT] Option (c)** — keep `openrouter.rs` standalone as the
   single chat-completions engine + composition target; this memo becomes the
   documented "why OpenRouter stays separate" the roadmap item asked for.
2. **[YES/NO] Carve-out bead** — file a small bead to dedupe the three
   wire-agnostic helper triplets (§2.3) into `providers/translate_common.rs`
   (~100–130 net lines removed; optional hygiene, not architecture).
3. **[CHOOSE] Part 2 vehicle** — re-scope mu-douj Part 2 as a new bead on the
   ollama seam: (i) native `/api/chat` path for per-request options, or
   (ii) upstream `num_ctx`-on-`/v1/messages` contribution + options hook on
   the composed Anthropic path, or (iii) defer (mu-w8ap refusal stands as the
   safety line).
4. **[YES/NO] Close/rewrite mu-douj** — Part 1 closed as stale-premise
   (superseded by mu-019-line typing, mu-fmas wire switch, vllm composition),
   with a pointer to this memo; Part 2 spun out per item 3.
5. **[ACKNOWLEDGE] Re-trigger conditions** for option (b) (§4): second
   non-composable chat-completions backend, external type consumer, or
   recurring wire-drift bugs.

## 7. Provenance

Written 2026-07-06 by cc (Claude Code) agent in workspace
`mu-douj-decision` (sprint-start label `douj-decision`, base
`land-jul2-line`), from direct file reads at revision `6c017f53` and the
central-beadsd `mu-douj` record. No provider code was modified. All file:line
references are against the starting revision above.
