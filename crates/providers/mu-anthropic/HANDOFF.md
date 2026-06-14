# mu-anthropic — handoff / task note

_Last updated: 2026-06-13, end of the founding session._

A note to the next session (likely claude-code). Read PLAN.md, AGENTS.md,
INTEGRATION.md first — those are the design. This file is **state + next
actions**, not design.

## Status: structurally complete library, first ground-truth validation passing

The Rust protocol library is built end-to-end. 58 unit tests + 2 tier-3
integration tests green; fmt + clippy clean throughout. Nothing depends on mu
(the DAG holds). The chain is a sequence of coherent commits (one per slice),
intended to be read in order.

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

### Methodology (keep doing this — it's why the build stayed clean)

Reference-don't-inherit. The old `mu-ai/src/providers/anthropic.rs` is an
**oracle**, not an ancestor — extract *facts and test-cases* from it, never copy
its (string-typed `json!{}`) code. Build greenfield in **vertical slices**: one
type + serde + tests (round-trip, spec-conformance, scar-regression), compiles +
passes before commit, small enough to hold in head. One slice ≈ one commit.

## Next actions (rough priority)

1. **Real tool-call modeling (tier-3, blocked on a capture).** mu-anthropic has
   `ContentBlock::ToolUse` but it's NOT yet validated against real `tool_use` /
   `tool_result` wire traffic. The only capture so far is a trivial `"ok"` probe
   — no tool calls in it. NEED: a capture of claude-code doing real tool work
   (`ANTHROPIC_BASE_URL=http://127.0.0.1:8788 claude-code`, then
   `parse-wire.py`). The parser now scrubs payloads, so fixtures keep the
   *shape*, drop the content. Then add a tier-3 test asserting ToolUse/ToolResult
   round-trip against real traffic.

2. **Wire-ahead-of-spec: model the new request fields.** Real requests carry
   `metadata`, `thinking` (`{"type":"adaptive"}`), `context_management`
   (`{"edits":[{"type":"clear_thinking_...","keep":"all"}]}`), `output_config`
   (`{"effort":"high"}`), and the path is `/v1/messages?beta=true`. These are
   OUTBOUND completeness — we can't *send* them yet. Decide which mu will use,
   model them as typed fields (envelope-side), add to `MessagesRequest`.

3. **pyo3 layer + Python "curl with extra steps" REPL.** Not started. Plan
   (INTEGRATION.md): `mu-anthropic` (lib) -> `mu-anthropic-py` (thin pyo3 cdylib,
   one-to-one delegation, NO logic, NO tests) -> Python wheel that imports +
   calls every export (THIS tests the binding). Python deps: `requests`, maybe
   `datetime`. NO polars/pandas/scipy (host venv rule). Reference: tcovert's
   spline project (`/jails/spline/.../jj_spline`) is the known-good Rust→py→polars
   skeleton — copy its topology, kill the double-nest, harden no-logic-in-binding.

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

- **The PLAN/AGENTS docs commit (`docs-mu-anthropic-plan`) is in the `default`
  workspace, NOT in this chain's ancestry.** It branched separately off main.
  If this bookmark is pushed alone, those docs don't go with it. Decide whether
  to rebase/fold them in.
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
