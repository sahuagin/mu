# mu-anthropic — handoff / task note

_Last updated: 2026-06-14 — after the protocol-completion + drift-canary session._

A note to the next session. Read `AGENTS.md` (the rules — still load-bearing) and
`INTEGRATION.md` (the seam map) for design. **The next chunk of work is the
mu-side integration — its step-by-step plan is `INTEGRATION-PLAN.md`.** This file
is state + orientation, not design.

## Status: FULL protocol coverage, on `main`, with a live drift canary

The library models the entire Anthropic Messages wire protocol — request +
response + streaming. **86 unit + 3 integration tests green; clippy `-D warnings`
+ fmt clean.** It is in the workspace and merged to `main` (PRs #291–#298, #304).
Nothing depends on `mu` (the DAG holds). It is NOT yet wired into mu — that's the
integration plan.

### What's modeled (the whole surface)

- **Request envelope** (`request.rs`): `MessagesRequest` with model, max_tokens,
  messages, system (polymorphic string|blocks), tools, **tool_choice**, stream,
  temperature/top_p/top_k (via `FiniteF64`), stop_sequences, and the wire-ahead
  beta fields **metadata, thinking, context_management, output_config**, plus
  **service_tier, container, mcp_servers**. Omit-when-absent everywhere (never
  emits `null`).
- **Tool taxonomy** (`request.rs`): `ToolDef` = `Custom(Tool)` | `Server(ServerTool)`
  | `Unknown`, hand-written `Deserialize` keyed on `type`. `ServerTool` normalizes
  the versioned `type` (`web_search_20250305` → family `web_search` + version
  `20250305`) with a `config` tail; `mcp_toolset` falls out as a Server with no
  version. `ToolChoice` = auto|any|tool|none.
- **Content blocks** (`content.rs`): `text` (+ **citations**), `tool_use`,
  `tool_result`, `thinking`, `redacted_thinking`, `server_tool_use`, `fallback`,
  `image`, `document`, and the **server-tool result family** (`web_search_tool_result`,
  `bash_code_execution_tool_result`, … — generic `ServerToolResult` dispatched by
  the `_tool_result` suffix, `content` kept as raw JSON since shape varies per
  family). `Unknown(JsonValue)` is the forward-compat fallback for the rest.
- **Response** (`response.rs`): `Message` (+ top-level **container**), full
  `Usage` (service_tier, inference_geo, output_tokens_details, server_tool_use,
  iterations, cache tiers), **stop_details**, `StopReason`.
- **Streaming** (`stream.rs` + `accumulate.rs`): `StreamEvent` enum + async
  `accumulate()` folding events → `Accumulated`.
- **Invariants**: `JsonValue` (finite-validated `Value` wrapper, keeps `Eq`
  crate-wide), `FiniteF64`. **pyo3 binding** (`mu-anthropic-py`) scaffolded (thin,
  no logic).

### The drift canary (the standing guard)

`examples/drift_check.rs` parses real `/v1/messages` responses with these types
and reports DRIFT when the wire carries something unmodeled — a dropped/changed
field (round-trip diff) or an `Unknown` block. The library is intentionally
lenient (never errors on unknown fields → they land in `Unknown`/`extra`), so the
canary re-serializes and diffs to surface drift. Exit 0 clean / 3 drift.

- Regression corpus: `tests/fixtures/server_tool_{web_search,code_execution}_response.json.xz`
  (real opus-4-8 captures, scrubbed of `encrypted_content`, xz'd).
- Orchestration: `~/src/claude-personal/scripts/anthropic-protocol-canary.sh`
  (`--static` over fixtures = no spend; `--live` fires a server-tool curl matrix;
  `--alert=bead|issue`). Reads the spend-capped key at
  `anthropic.protocol-canary.api_key` (workspace "automation", $200/mo cap).
- Scheduled: local cron Mondays 09:17 `--live --alert=bead`. **Builds from
  `~/src/public_github/mu`, so that checkout must be on current `main`** (it is,
  via the operator's `mu-solo` workflow). The loop is: canary finds an unmodeled
  field → file bead → model it → fixture goes clean.

### Deferred (the canary will catch these if they appear in traffic)

- streaming `citations_delta` accumulation (non-streaming path preserves citations);
- `tool_result`-block citations (text-block citations ARE modeled);
- `document` `citations` config + the `content`-kind source;
- the pyo3 "curl with extra steps" REPL (actually *sending* requests over HTTP).

## NEXT: wire mu-anthropic into mu

This is the only remaining major work. **See `INTEGRATION-PLAN.md`** for the
step-by-step (dep → `From<&ProviderMessages> for MessagesRequest` mu-side → rewire
`providers/anthropic.rs` → delete the old `build_request_body*`/`translate_*` as
the done-signal). The old `crates/mu-ai/src/providers/anthropic.rs` stays running
and untouched until the single cutover.

## Traps / facts still live

- **jj snapshot size:** this repo's `snapshot.max-new-file-size` is 2 MiB (for the
  xz'd spec). jj silently EXCLUDES oversized files — a `describe` can "succeed"
  while dropping a file.
- **Capture redaction is a CONTROL:** `~/src/claude-personal/scripts/parse-wire.py`
  scrubs request bodies + auth headers before anything reaches a fixture; raw
  `~/private/cc-capture.bin` has live tokens — never commit. Response captures
  carry no auth, but scrub `encrypted_content` before committing (see the canary
  fixtures).
- **cc logs ≠ wire:** the cc session logs conflate Anthropic wire fields with cc
  client annotations (`diagnostics`/`speed`/`cost`/`provider` are cc-only). The
  anthropic-wiretap capture is the authority. (memory: cc-logs-conflate-wire-and-client.)
- **fixtures are `.json.xz`** — `xzcat` to read; the canary script decompresses them.
