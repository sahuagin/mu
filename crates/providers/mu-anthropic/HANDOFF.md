# mu-anthropic — handoff / task note

_Last updated: 2026-06-14 — reconciled against the actual tree after the founding
session continued past the first draft (pyo3 layer + cc-log probe landed after
the 23:59 handoff commit). Claims below were re-verified against the working copy._

A note to the next session (likely claude-code). Read PLAN.md, AGENTS.md,
INTEGRATION.md first — those are the design. This file is **state + next
actions**, not design.

## Status: structurally complete library, first ground-truth validation passing

The Rust protocol library is built end-to-end. 58 unit tests + 3 integration
tests green (2 tier-3 `tier3_opus48_ground_truth` + 1 `cc_log_parse_probe`);
fmt + clippy clean throughout. Nothing depends on mu (the DAG holds). The chain
is a sequence of coherent commits (one per slice), intended to be read in order.
The pyo3 binding crate (`mu-anthropic-py`) is also scaffolded — see Done.

### Done

- **specs** — pinned Anthropic docs snapshot (`specifications/`, xz'd full dump
  + `llms.txt` index + MANIFEST). The wire is already *ahead* of this snapshot
  (see "wire ahead of spec" below).
- **slice 1** ContentBlock + CacheControl
- **slice 2** Message / Content / Role
- **slice 3** MessagesRequest envelope + Tool
- **slice 4** response Message + Usage + StopReason
- **slice 5** StreamEvent (SSE events)
- **slice 6** async stream accumulator
- **JsonValue** — quarantines `serde_json::Value` so its trait limits (no `Eq`,
  NaN) can't dictate the trait surface of containing types. Validates finite on
  construct + deserialize. Pattern mirrors mu-core's `ToolArgs` (referenced, NOT
  imported — DAG).
- **FiniteF64** — sampling knobs (`temperature`/`top_p`) coerce non-finite to
  absent, both directions, never error. Dissolved the `MessagesRequest: !Eq`.
- **tier-3** — first test against REAL captured `claude-opus-4-8` traffic
  (anthropic-wiretap). All 7 SSE events deserialize; Usage ignores
  service_tier/inference_geo (newer than spec) while parsing cache-tier split.
- **cc-log probe** (`tests/cc_log_parse_probe.rs`) — parses every message in a
  REAL captured Claude Code session log (`fixtures/cc_log_messages.json`, which
  DOES contain `tool_use`/`tool_result` blocks) as `ResponseMessage`, asserting
  100% parse. This is the capture that next-action #1 was blocked on; it is now
  obtained and the assistant-output (`ToolUse`) path is exercised against real
  traffic. See #1 for what's still missing (explicit round-trip assertion +
  the user-input `ToolResult` path).
- **pyo3 binding** (`crates/providers/mu-anthropic-py`) — thin cdylib per the
  INTEGRATION.md plan: 4 `#[pyfunction]`s (`parse_response_message`,
  `parse_stream_event`, `parse_request`, `is_valid_response_message`) that
  delegate one-to-one to `mu-anthropic`, NO logic. `cc_log_smoke.py` imports the
  wheel and calls every export against real cc session logs (the binding's test
  lives in Python, by design). This was next-action #3; the scaffold is done —
  remaining polish is noted in #3 below.

### Methodology (keep doing this — it's why the build stayed clean)

Reference-don't-inherit. The old `mu-ai/src/providers/anthropic.rs` is an
**oracle**, not an ancestor — extract *facts and test-cases* from it, never copy
its (string-typed `json!{}`) code. Build greenfield in **vertical slices**: one
type + serde + tests (round-trip, spec-conformance, scar-regression), compiles +
passes before commit, small enough to hold in head. One slice ≈ one commit.

## Next actions (rough priority)

1. **Real tool-call modeling — PARTIALLY DONE.** The capture this was blocked on
   now exists: `fixtures/cc_log_messages.json` is real cc traffic containing
   `tool_use`/`tool_result`, and `cc_log_parse_probe.rs` asserts mu-anthropic
   parses all of it as `ResponseMessage`. So `ContentBlock::ToolUse` (assistant
   output) IS now exercised against real traffic. STILL MISSING: (a) an explicit
   *round-trip* assertion (parse → re-serialize → compare), not just parse-OK;
   (b) the user-input `ToolResult` path — the probe only deserializes each
   message as `ResponseMessage` (output side), so `tool_result` inside a
   *request* message is not yet round-tripped. To extend coverage with fresh
   captures: `ANTHROPIC_BASE_URL=http://127.0.0.1:8788 claude-code`, then
   `parse-wire.py` (scrubs payloads — fixtures keep *shape*, drop content).

2. **Wire-ahead-of-spec: model the new request fields.** Real requests carry
   `metadata`, `thinking` (`{"type":"adaptive"}`), `context_management`
   (`{"edits":[{"type":"clear_thinking_...","keep":"all"}]}`), `output_config`
   (`{"effort":"high"}`), and the path is `/v1/messages?beta=true`. These are
   OUTBOUND completeness — we can't *send* them yet. Decide which mu will use,
   model them as typed fields (envelope-side), add to `MessagesRequest`.

3. **pyo3 layer — SCAFFOLDED (was "not started").** `mu-anthropic-py` exists:
   thin pyo3 cdylib, 4 `#[pyfunction]`s delegating one-to-one to `mu-anthropic`,
   no logic (per AGENTS.md rule, enforced in the module header). `cc_log_smoke.py`
   imports the wheel and calls every export against real cc logs — the binding's
   test, by design. REMAINING: the "curl with extra steps" REPL itself (actually
   *sending* requests) is not built — `cc_log_smoke.py` only parses logs, it does
   no HTTP. Python deps still want to stay minimal (`requests`, maybe `datetime`;
   NO polars/pandas/scipy — host venv rule). Reference topology: tcovert's spline
   project (`/jails/spline/.../jj_spline`) Rust→py skeleton — the double-nest was
   meant to be killed; verify that was done.

4. **mu-side integration (mu-ai work, separate, operator's call).** Write
   `From<&ProviderMessages> for mu_anthropic::MessagesRequest` ON THE MU SIDE
   (consumer owns the mapping). The stream accumulation loop STAYS in mu
   (`next_event` is agent-loop+transport, not protocol). Final step: delete the
   old `build_request_body*` — that deletion is the migration's done-signal,
   all-at-once, not incremental in place.

5. **Re-run `just ci-aipr`.** The review panel last saw the pre-fix chain. It has
   NOT seen: F2/F8 fixes, JsonValue, FiniteF64, tier-3. Expect it to flag
   candidates (it's high-recall/low-precision — ~2 of 14 were real last time;
   adjudicate each against code that compiles, don't obey blindly).

## Traps / facts the next session needs

- **CORRECTED (2026-06-14): the PLAN/AGENTS docs ARE in this chain's ancestry.**
  The original draft claimed `docs-mu-anthropic-plan` branched separately off
  main and would be left behind on push. That is FALSE as of now — verify with
  `jj log -r 'main | docs-mu-anthropic-plan | mu-anthropic-founding | @'`: the
  graph is strictly linear
  `main → docs-mu-anthropic-plan → [slice chain] → mu-anthropic-founding → @`.
  Pushing the founding bookmark carries the docs with it; no rebase needed. (The
  chain was presumably rebased onto the docs commit after the first draft was
  written.) Lesson: run the graph query, don't trust a prose ancestry claim.
- **Capture redaction is now a CONTROL, not a reminder.** `parse-wire.py` scrubs
  request body (metadata.user_id, system[], tools[] descriptions, messages[]
  content) AND headers (auth + `x-claude-*` prefix + session-id + cookie) before
  anything reaches a fixture. Tool *names* are kept (innocuous, standard). Raw
  `~/private/cc-capture.bin` still has live auth tokens verbatim — never commit.
  (Those parse-wire.py edits are uncommitted in `claude-personal`, a SEPARATE
  repo — operator lands them.)
- **Session demux:** captures key on TCP `conn` id; multiple sessions in one
  capture file demux cleanly. To group by *claude-code session* you'd use the
  `X-Claude-Code-Session-Id` header — but that's now redacted; hash it instead of
  blanking if session-grouping in fixtures matters.
- **jj snapshot size:** this repo's `snapshot.max-new-file-size` was raised to
  2MiB (per-repo) to allow the xz'd spec dump. jj silently EXCLUDES oversized
  files from a snapshot — a `describe` can "succeed" while dropping a file.
- **beads:** the shared `.beads/issues.jsonl` was repaired this session (one
  record had unescaped literal newlines, blocking all imports). Operator regards
  beads + claude_proxy as net-negative right now; replacement planned. Don't sink
  time hardening either — route around, log friction, move on.
