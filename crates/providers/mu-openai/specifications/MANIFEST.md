# OpenAI API specification — manifest

A **time-pinned snapshot** of OpenAI's REST API spec, captured so this crate has
a stable, referenceable contract that does NOT silently drift under us. When the
live spec changes, the drift canary catches it (see `INTEGRATION.md` "Test tiers"
and `examples/drift_check.rs`); this snapshot is the "what it said when we built
it" record.

## Captured

- **Date:** 2026-09-09 (refreshed from 2026-09-02, itself from 2026-06-22).
  The spec's own `info.version` reads 2.3.0 in both captures, so the capture
  date is the pin. The delta since 2026-09-02 is recorded in its own section
  below. The 2026-09-02 delta (request/response gained `moderation` +
  `prompt_cache_options`, `reasoning.effort` gained `max`, eight stream-event
  schemas — Beta/WebSocket wrappers + shell-call events) was modeled or
  tolerated as of that refresh.
- **Source:** https://github.com/openai/openai-openapi (the official OpenAPI 3.1
  spec of record for the OpenAI REST API)

## Files

| file | source URL | stored | what it is |
|---|---|---|---|
| `openapi.yaml.xz` | https://raw.githubusercontent.com/openai/openai-openapi/master/openapi.yaml | ~208K (xz; ~3.0M raw, 89.6k lines) | The ENTIRE OpenAI OpenAPI 3.1 spec in one file — the machine-readable contract of record. Stored `xz -9` compressed to keep the repo light. |

This crate models the **Responses API** (`/v1/responses`) surface for agent/text
+ tool-calling + reasoning. The relevant schemas in the spec:
`CreateResponse`, `Response`, `ResponseStreamEvent`, `ReasoningItem`,
`FunctionTool`, and the `response.*` streaming events.

## Delta since the 2026-09-02 pin (mu-openai-protocol-2026q3-yyg3j.2)

Measured on the two captures: 1421 -> 1473 component schemas (52 added, none
removed, none renamed), `ResponseStreamEvent` unchanged, 87,410 -> 89,583
lines. Of the 52, 24 are `Beta*` mirrors of the same shapes (the spec keeps a
Beta namespace alongside the GA one) and two are Beta-only
(`BetaResponseInjectCreatedEvent` / `BetaResponseInjectFailedEvent`). The GA
additions, grouped by what they are for, and where the crate stands on each:

**Modeled** (no change needed): nothing in the delta touches a shape the crate
already types except as noted under tolerated; the crate's 39 unit tests and
the offline drift canary pass unchanged against this capture.

**Tolerated** (a live response carrying it round-trips or is dropped
harmlessly today; the typed field is the epic's work):

- `Response.error.misalignment` (`MisalignmentErrorDetailsResource`:
  `error_type` — an extensible enum of `potentially_unintended_data_transfer`
  / `_data_access` / `_destructive_activity` / `other` —,
  `detailed_explanation`, `steer.message`) and the new
  `ResponseErrorCode` value `misalignment_policy_violation`. This is the
  Responses-API face of misalignment monitoring: a response stopped for
  review arrives as `status: failed` with that code and those details.
  `ResponseError.code` is a `String` in the crate, so the code parses; the
  `misalignment` object is an unknown field and is dropped.
- `Response.incomplete_details.reason` gains `steered` ("stopped at a safe
  output boundary after a WebSocket `response.steer` event", after which
  the server creates a successor response). `IncompleteDetails.reason` is a
  `String`, so it parses.
- `Response.prompt_cache_diagnostics` (`PromptCacheDiagnostics`: a
  discriminated union of hit / miss / comparison-response-not-found /
  unavailable bodies, the miss carrying a `CacheMissReasonTypeEnum` —
  `model_changed`, `prompt_cache_key_changed`, `tools_changed`,
  `text_format_changed`, `reasoning_effort_changed`, `verbosity_changed`,
  `context_compacted`, `input_changed`, `service_tier_changed`). Unknown
  field, dropped.
- `configuration_update` as an output item (`ResponseConfigurationUpdate`:
  `id` `cnfu_…`, `reasoning.effort`): falls to `OutputItem::Unknown`.

**Unmodeled** (the epic's items 1 and 2):

- `prompt_cache_options` grew: the request param is now
  `ResponsePromptCacheOptionsParam` with `ttl` (`30m`, the only value),
  `mode` (`implicit` / `explicit`) and `comparison_response_id` (requests
  the diagnostics above). The crate's `PromptCacheOptions` has `mode` and
  `ttl` only; the codex lane strips the whole object regardless
  (wire-verified 2026-09-02).
- `configuration_update` as an INPUT item (`ResponseConfigurationUpdateItemParam`
  in the `Item` union: `{type: "configuration_update", reasoning: {effort}}`,
  "remains in effect for subsequent responses until it is replaced") — the
  mid-conversation effort change from the Sep 3 changelog entry. The crate's
  outbound item enum is closed, so mu cannot send it until it is typed.
- Steering, which the spec defines as WebSocket-only: the client events
  (`ResponsesClientEvent` = `response.create` with a `stream_id` lane, or
  `response.steer` with `ResponseSteerInput`, user-role messages only) and
  the server events (`ResponsesServerEvent` adds `response.steer.accepted`,
  `.pending` with `ResponseSteerPendingReason` and `ResponseSteerRequiredInput`
  stubs, `.failed` with `ResponseSteerErrorCode`, plus `ResponseWsError`).
  "`stream_id` is WebSocket-only and is not part of `POST /v1/responses`";
  `stream` is implicit and `background` unsupported over WebSocket. The
  crate has no WebSocket transport; these are typed shapes without a lane
  until one exists.
- Async tool calling: a new boolean `async` on the tool definitions
  (`FunctionToolParam` / `CustomToolParam`: "Whether the tool response can
  be returned asynchronously versus immediately returned on next response
  creation") and on the call items (`FunctionToolCall` / `CustomToolCall`:
  "Whether the function tool call runs asynchronously") — ten new sites,
  none in the 2026-09-02 capture. The async-tool-calling guide adds the
  protocol the spec leaves implicit: the model keeps working while the
  application runs the tool, and the result is returned on a later request
  as a `function_call_output` matched by the original `call_id`, with
  `previous_response_id` continuing from the latest response. The crate's
  `Tool` and call-item types carry neither flag today (an inbound
  `async: true` on a call item is dropped as an unknown field).
- `UserMessageItemParam` and `WebSearchCallStatus`: supporting shapes for
  the above (the steer input's message form; a status enum the crate reads
  as a string).
- `InferenceRateLimited` / `InferenceServiceUnavailable`: the 429
  `slow_down` and 503 `server_is_overloaded` response components with
  `Retry-After`. Handled lane-wide by the shared HTTP-error renderer since
  #602 (epic item 3); nothing for the crate.
- `SafetyAlertResource` / `SafetyAlertErrorType`: the `safety.alert` object
  of the Safety Alerts API (a project resource: `request_id`, `response_id`,
  `model`, `request_paused`, `error_type`, `reason`) delivered by the
  `safety_alert_created` WEBHOOK (`WebhookSafetyAlertCreated`; scope
  `api.safety.alerts.read`). This corrects the epic's item-1 premise: there
  is no `safety_alert_created` stream event on `/v1/responses`; what a
  Responses client sees is the `misalignment_policy_violation` error above.

**Not in the spec at all** (so not this crate's contract to type):

- `gpt-6-astra` appears 211 times, every one an example model id or an
  "ID like `gpt-6-astra`" description; the OpenAPI states none of its
  constraints (no `none` effort, no custom `temperature` / `top_p`, no
  `logprobs`). Item 1's catalog rules come from the changelog entry and the
  model page, and belong in the model catalog, not here.
- The guide-level protocols behind the Sep 3 "long-running work" entry
  (which `function_call_output` to send when, the wait-tool pattern, the
  steering successor handshake) are prose in the guides, not schema; the
  spec carries only the fields and events listed above.

## Reading the compressed spec (works offline)

The spec stays fully navigable while compressed — the host has the `xz*`
wrappers, so it's just CPU, no decompress-to-disk:

```sh
xzcat  openapi.yaml.xz                      # stream the whole thing
xzgrep '/responses'        openapi.yaml.xz  # find the endpoint
xzgrep -A30 'CreateResponse:' openapi.yaml.xz
xzgrep 'ResponseStreamEvent' openapi.yaml.xz
xzless openapi.yaml.xz                      # page through it
```

## Refresh

```sh
curl -sSL https://raw.githubusercontent.com/openai/openai-openapi/master/openapi.yaml \
  | xz -9 > openapi.yaml.xz
```

After refreshing, run the crate tests and the drift canary
(`scripts/openai-protocol-canary.sh`) — any new/renamed/dropped field that the
typed model no longer round-trips will surface there.
