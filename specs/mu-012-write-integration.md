# Spec: wire `write` into the factory + end-to-end test

| field      | value                                          |
| ---------- | ---------------------------------------------- |
| spec_id    | mu-012                                         |
| status     | ready                                          |
| created    | 2026-05-10                                     |
| updated    | 2026-05-10                                     |
| authors    | tcovert + claude-personal (claude-opus-4.7)    |
| supersedes | none                                           |

## Why

mu-011 added `WriteTool`. mu-012 wires it into the factory so
`mu serve --tools write` (and `mu serve --tools read,write`) work,
plus an end-to-end live test mirroring mu-010 for the read slice.

Tiny spec. CONVENTIONS apply.

## Scope

- **In:**
  - `crates/mu-coding/src/serve/factory.rs` — add a match arm for
    `"write"` → `Arc::new(WriteTool::new())`. Update the error
    message's expected-list. Update the unit test for unknown tool to
    cover the new known one.
  - `crates/mu-coding/tests/anthropic_write_smoke.rs` — new
    integration test. Same shape as `anthropic_read_smoke.rs`. Asks
    Claude to write a known string to a temp file via the write
    tool; asserts (a) the response indicates success, (b) the file
    on disk contains the expected content. Gated on
    `MU_LIVE_ANTHROPIC=1`.

- **Out:**
  - Any code changes to `WriteTool` itself. mu-011's done.
  - Multi-tool tests (running with `--tools read,write` and a prompt
    that invokes both). Future spec if useful.

## Behaviors

1. **B-1 (factory build):** `build_tools(&["write".to_string()])`
   returns a one-element vec; `tools[0].spec().name == "write"`.
2. **B-2 (factory unknown still errors):** `build_tools(&["bogus"])`
   still returns an error mentioning known names.
3. **B-3 (end-to-end live):** Gated test. Spawns `mu ask --provider
   anthropic-api --tools write "<prompt>"`. Prompt asks Claude to
   write a fixed string to a temp path. Assertions:
   - Process exits 0.
   - The temp file exists and contains the expected string.
   - Cleanup removes the temp file.

## Acceptance

- Modified files: `crates/mu-coding/src/serve/factory.rs` (small).
- New file: `crates/mu-coding/tests/anthropic_write_smoke.rs`.
- `cargo nextest run` passes (B-3 skipped without env var).
- With `MU_LIVE_ANTHROPIC=1`: B-3 also passes.

## Prior work

- mu-011 — `WriteTool`.
- mu-010 — read end-to-end test (the structural template).

## Changelog

- 2026-05-10 — initial draft (claude-personal).
