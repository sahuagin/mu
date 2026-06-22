# INTEGRATION.md — how mu-openai plugs into mu (and how we build it)

This doc is the seam map. It exists so a new session does NOT have to re-derive
(a) where this crate connects to mu, or (b) which side of the boundary a given
job lives on. Read alongside `AGENTS.md` (the rules) and `specifications/`
(the contract).

The one-line invariant: **this crate is the typed wire contract; everything that
moves bytes or knows what "mu" means stays in `mu-ai`.** Dependency direction is
a strict DAG — `mu-ai → mu-openai`, never back.

---

## 1. What we are replacing

mu's current OpenAI support is spread across **three overlapping files** in
`crates/mu-ai/src/providers/`:

- `openai_codex.rs` — the ChatGPT/Codex subscription path (OAuth + the
  `chatgpt.com/backend-api/codex` endpoint).
- `openai_api.rs` — the public API-key path (`api.openai.com`).
- `openai_responses.rs` — the shared Responses-protocol request/response/stream
  handling those two drive.

They were built by hand without a spec and grew redundant request-building,
inbound-parsing, and accumulation logic. `mu-openai` re-implements the **wire
protocol** parts of all three as one typed contract. The TRANSPORT parts
(HTTP, auth, endpoint selection, SSE framing, cancellation, the agent loop)
stay in mu, consolidated into a single new `mu-ai/src/providers/openai.rs`
(to be built in a later PR). Those three current files then **die together**
when mu's call site is rewired — their deletion is the migration's done-signal.

### The four jobs (which side owns each)

| Job | mu-openai equivalent | Side |
|---|---|---|
| **Outbound request build** — assembling the `/v1/responses` body (model, instructions, input items, tools, tool_choice, reasoning, sampling, stream, store, include) | typed `CreateResponseRequest` + `InputItem` / `InputContent` / `Tool` / `FunctionTool` / `ToolChoice` / `Reasoning` + their `Serialize` | **crate provides the types** (mu's `From` builds them) |
| **Inbound wire types** — the non-streaming response and every `response.*` SSE event | typed `Response` / `OutputItem` / `OutputContent` / `Usage` (+ details) / `ResponseError` / `IncompleteDetails` and the `ResponseStreamEvent` enum + their `Deserialize` | **crate** |
| **Async accumulator** — fold the SSE event stream into the final `Response` | `accumulate(Stream<ResponseStreamEvent>) -> Result<Response, AccumulateError>` (async fold; terminal `response.completed`/`.failed`/`.incomplete` is authoritative, deltas backfill a degraded-empty `output`) | **crate provides it; mu wraps it** |
| **Stream driver + transport + auth + endpoint** — reqwest HTTP, SSE `data:` byte-line framing, public-key Bearer vs ChatGPT/Codex OAuth (+ `chatgpt-account-id` header + 401 refresh), `api.openai.com` vs `chatgpt.com/backend-api/codex` selection, cancellation, `ProviderEvent` emission, mu↔wire translation | — | **stays in mu** |

The split is the same shape as `mu-anthropic`'s: three wire jobs come into the
crate, the agent-loop-coupled transport/driver job stays in mu.

---

## 2. The seam (both directions)

```
OUTBOUND
  mu ProviderMessages / AgentMessage (mu-core normalized form)        [KEEP: mu]
    ── From<…>  (lives MU-SIDE in providers/openai.rs, NOT here) ──▶
         mu_openai::CreateResponseRequest { instructions, input: Vec<InputItem>,
                                            tools, tool_choice, reasoning, … }
                                                              │ serialize
                                                              ▼  wire JSON  → mu's HTTP client

INBOUND
  wire bytes  → mu frames SSE `data:` lines (MU-SIDE)
              ── deserialize ──▶ mu_openai::ResponseStreamEvent
                                     │  mu's stream driver folds these into
                                     │  ProviderEvents AS THEY ARRIVE; and/or
                                     │  mu_openai::accumulate(...) assembles the
                                     │  final Response at the terminal event.
                                     ▼  ProviderEvent / AgentMessage      [mu narrows]
  wire (non-stream)  ── deserialize ──▶ mu_openai::Response ─ [mu narrows] ─▶ AgentMessage
```

**mu-openai owns:** typed `CreateResponseRequest` (+serialize), typed `Response`
/ `OutputItem` / `OutputContent` / `Usage` and `ResponseStreamEvent`
(+deserialize), the async `accumulate()`, the local invariant types
(`JsonValue`, `FiniteF64`), and the protocol constants/spellings (the `type`
tags, the snake_case enum strings, the `response.*` event names incl. the
ChatGPT/Codex `function_call.arguments.delta` compat spelling, function-call
`arguments` being a JSON *string*, `store=false` statelessness, the
`reasoning.encrypted_content` include token).

**mu-openai does NOT own and MUST NOT import:** mu's `ProviderMessages` /
`AgentMessage` / normalized `ContentBlock`, the HTTP client, the SSE byte
framing, auth/OAuth/endpoint logic, `ProviderEvent`, the agent loop. (See
AGENTS.md rules 1 and 6: no-import-mu DAG, no sockets in the lib.) The
conversions live mu-side; the consumer owns the mapping.

### Mapping notes (so the mu-side `From` stays mechanical)

- mu tool call → `InputItem::FunctionCall { call_id, name, arguments, id }`.
  `arguments` is the model's emitted JSON **as a string**, not an object.
- mu tool result → `InputItem::FunctionCallOutput { call_id, output }`
  (correlated by `call_id`, not positionally).
- mu user/assistant text → `InputItem::Message { role, content:
  Vec<InputContent> }`, where user text is `InputContent::InputText` and prior
  assistant text echoed back is `InputContent::OutputText`.
- mu system prompt → the top-level `instructions` envelope field, **not** an
  `input` item (Responses moved the system prompt out of the message array).
- **Reasoning threading (the load-bearing one):** an inbound
  `OutputItem::Reasoning { id, encrypted_content, summary, content }` must be
  threaded back verbatim as `InputItem::Reasoning` on the next turn, with the
  request carrying `include: ["reasoning.encrypted_content"]` while stateless
  (`store=false`). The crate models both ends fully; mu does the round-trip so
  the model keeps chain-of-thought across tool calls.

---

## 3. Streaming is the production path; accumulation is async-only

- mu-openai models the streaming event vocabulary as first-class typed events
  (`ResponseStreamEvent`: the `response.created/.in_progress/.queued/.completed/
  .failed/.incomplete` lifecycle, `output_item.*`, `content_part.*`,
  `output_text.*`, `function_call_arguments.*`, the reasoning summary/text
  events, refusal, and error frames).
- It provides an **async accumulator**, `accumulate()`: it consumes the typed
  event stream and yields the assembled `Response`. Unlike Anthropic (where the
  final message is built purely from deltas), the Responses API sends the
  **authoritative full `Response`** on the terminal lifecycle event — so that is
  the source of truth. Text and function-call-argument deltas are accumulated
  only to BACKFILL a terminal response whose `output` came back empty (a degraded
  backend), so a partial turn stays usable. A truncated stream (no terminal
  event, no snapshot) is `AccumulateError::UnexpectedEof`; a mid-stream error
  frame is `AccumulateError::StreamError`.
- No synchronous interfaces. The accumulator is async + streaming and never
  touches a socket — mu frames the SSE bytes and feeds it decoded events.

---

## 4. The drift sentinel (where the contract is pinned)

The authority for "what the wire actually looks like" is **captured real
traffic** in `tests/fixtures/`, NOT the spec text alone. Spec informs; traces
adjudicate. The vendored `specifications/openapi.yaml.xz` is the documented
contract (tier-2 spec-exact tests); the fixtures are the observed contract
(tier-3 goldens). The canary (`examples/openai_drift_check.rs` +
`scripts/openai-protocol-canary.sh`, run from cron) replays captured responses
through the types: because the inbound types carry NO catch-all `extra` field
(AGENTS.md rule 8), a field OpenAI adds that we don't model is DROPPED on
round-trip and the canary flags it — turning a silent upstream change into a
loud red test before it reaches production.

---

## 5. Migration (later, mu-side, operator's call)

When the slices are green and trace-validated, mu's OpenAI call site is rewired:
the request-build sites construct a `mu_openai::CreateResponseRequest` (via the
mu-side `From`) and serialize that; the SSE parse uses
`mu_openai::ResponseStreamEvent`; the final-turn assembly uses
`mu_openai::accumulate`. The three current files — `openai_codex.rs`,
`openai_api.rs`, `openai_responses.rs` — are then **deleted, not refactored**,
their transport-only residue consolidated into one new
`mu-ai/src/providers/openai.rs`. mu stays on the old path until that single swap,
so the rewrite never percolates through the rest of mu.
