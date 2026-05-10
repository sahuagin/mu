# Delegation: mu-008 part A — Anthropic request-side translation

This is part A of mu-008. Part B (claude, in-conversation) does the
response-side StreamState refactor on top of what you build here.

**Universal rules: read `specs/delegations/CONVENTIONS.md`.** It
covers workspace handling, output envelope, spec-vs-prompt
disagreement, universal don'ts, verification ritual, and read order.
This prompt only adds spec-specific content.

## Spec to read first

`specs/mu-008-anthropic-tool-support.md` — focus on:
- §Why and §Scope to understand the larger context
- §Interfaces blocks for "Request side: tool spec translation",
  "Request side: message translation with tool-result grouping", and
  "Request side: build_request_body"
- §Behaviors B-1 through B-5 — those are yours
- §Invariants INV-3 (tool-result grouping) and INV-6 (omit empty tools)
- §OOC-2 and §OOC-4

§Interfaces blocks for the response-side stream state and §Behaviors
B-6..B-9 are out of scope for you — that's part B.

## Deliverable

One file modified: `crates/mu-ai/src/providers/anthropic.rs`.

Add three functions and update one:

1. `translate_tool_spec(spec: &ToolSpec) -> Value` — pub(crate). Per
   the §Interfaces sketch.

2. `translate_messages(messages: &[AgentMessage]) -> Vec<Value>` —
   pub(crate). Replaces internal callers of the existing
   `translate_message`. Implements the consecutive-tool-result
   grouping per INV-3.

3. `translate_message_single(m: &AgentMessage) -> Option<Value>` —
   private helper used by `translate_messages` for non-ToolResult
   variants. Per the §Interfaces sketch. (Rename or repurpose the
   existing `translate_message` function as appropriate; there are
   no callers outside this file. Existing tests can update to call
   either the renamed function or `translate_messages`.)

4. **Update** `build_request_body(model, messages, tools)` to take
   `tools: &[ToolSpec]` as a third argument, call
   `translate_messages` instead of the old per-message map, and
   include `"tools"` in the body only when non-empty (INV-6).

Update the existing call site in `Provider::stream` (only one) to
pass the tools argument through.

Tests to add:

- `b1_translate_tool_spec_shape`
- `b2_translate_messages_preserves_order`
- `b3_consecutive_tool_results_group_into_one_user_message`
- `b4_build_request_body_includes_tools_when_present`
- `b5_build_request_body_omits_tools_when_empty`

You may also update the existing `translate_skips_tool_result_in_v1`
test if its assertion no longer matches the new shape (with
`translate_messages`, ToolResult IS handled — just not by the
single-message helper). Either rename the test or replace its
assertion to test `translate_message_single` instead.

## Spec-specific don'ts

- **Don't touch the response-side code.** That's `StreamState`,
  `BlockBuilder`, `next_event`, `events_stream`. Part B's territory.
- **Don't add a new `tool_use` content-block variant to the
  `ContentBlock` enum.** mu-003 already has `ContentBlock::ToolCall(ToolCall)`.
  Use it.
- **Don't rename the public `AnthropicProvider` struct or its
  methods.** External callers depend on them.
- **Don't touch `Provider::stream`'s function signature.** Tools
  comes in as `_tools: &[ToolSpec]` already. Drop the underscore on
  the parameter name and pass it through to `build_request_body`.
- **Don't introduce per-test serde_json fixtures from external files.**
  Inline JSON in tests via `serde_json::json!`.

## Verification

Per CONVENTIONS Rule 7. Specifically:

```sh
cargo build -p mu-ai
cargo nextest run -p mu-ai
wc -l crates/mu-ai/src/providers/anthropic.rs    # under 800
grep -nE '\bunsafe\b|\.unwrap\(\)|\.expect\(|\bpanic\!|\btodo\!|\bunimplemented\!' \
  crates/mu-ai/src/providers/anthropic.rs \
  | grep -v '^[[:space:]]*//' \
  | grep -v 'cfg(test)'
git diff Cargo.toml    # workspace deps unchanged
```

The pre-mu-008 mu-ai test count was 16. After your B-1..B-5 plus the
existing tests (some may be renamed or repurposed), expect 20+ tests
passing in mu-ai. Workspace total should remain 70+ green.

## Output envelope

Per CONVENTIONS Rule 5. Add to `design_notes` how you handled the
`translate_message` rename / repurpose decision (kept it, renamed it,
or replaced it entirely).
