# OpenAI API specification — manifest

A **time-pinned snapshot** of OpenAI's REST API spec, captured so this crate has
a stable, referenceable contract that does NOT silently drift under us. When the
live spec changes, the drift canary catches it (see `INTEGRATION.md` "Test tiers"
and `examples/drift_check.rs`); this snapshot is the "what it said when we built
it" record.

## Captured

- **Date:** 2026-06-22
- **Source:** https://github.com/openai/openai-openapi (the official OpenAPI 3.1
  spec of record for the OpenAI REST API)

## Files

| file | source URL | stored | what it is |
|---|---|---|---|
| `openapi.yaml.xz` | https://raw.githubusercontent.com/openai/openai-openapi/master/openapi.yaml | ~205K (xz; ~2.8M raw) | The ENTIRE OpenAI OpenAPI 3.1 spec in one file — the machine-readable contract of record. Stored `xz -9` compressed to keep the repo light. |

This crate models the **Responses API** (`/v1/responses`) surface for agent/text
+ tool-calling + reasoning. The relevant schemas in the spec:
`CreateResponse`, `Response`, `ResponseStreamEvent`, `ReasoningItem`,
`FunctionTool`, and the `response.*` streaming events.

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
