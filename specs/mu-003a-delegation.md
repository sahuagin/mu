# Delegation: implement mu-003 part A (types + traits)

This file is the prompt sent to a sub-agent (currently `agent-router
--auth codex-oauth`) to implement the foundation layer of spec
`mu-003`. It is committed to the repo so the prompt itself is
reviewable.

**This is part A of a two-part split.** Part A builds the data types
and trait definitions. Part B (claude, in-conversation) will add the
queue-driven loop on top of the foundation you build here.

---

## Workspace hygiene (READ FIRST)

You are working in `/home/tcovert/src/public_github/mu`. A parallel
claude-code session is editing this same workspace concurrently. If
you see files in the working copy that are NOT in your deliverable
list (in particular: anything under `specs/`, anything you didn't
create yourself), **leave them alone**.

Specifically: do NOT `jj restore`, `jj abandon`, `git checkout --`, or
otherwise revert files that aren't yours. Do NOT delete untracked
files. Your "don't touch any other file" rule extends to *restore*
operations, not just edits.

This is a hard rule. Violating it deletes in-flight work from the
parallel session. Previous attempts have caused real data loss; this
prompt exists because of that. (See
`specs/delegations/mu-002-attempt-1-postmortem.md` for the full
incident report — read it if you want context.)

---

## Read first (REQUIRED)

1. `specs/mu-003-agent-loop.md` — the full specification. Your scope
   is the §Interfaces blocks for `types.rs`, `provider.rs`, and
   `tool.rs` PLUS the `agent/mod.rs` setup. The §Interfaces blocks
   for `loop_.rs` and the §Behaviors B-1..B-7 are **out of scope for
   you** — those are part B.
2. `crates/mu-core/src/lib.rs` — you will add ONE line: `pub mod
   agent;`. Don't reorder; don't reformat anything.
3. `crates/mu-core/src/protocol.rs` — already exists from mu-001;
   read briefly to see the project's serde idioms.
4. `crates/mu-core/src/transport.rs` — already exists from mu-002;
   read the test patterns in its test module so your tests match the
   project style.
5. `AGENTS.md` (root) — project-wide rules.
6. `Cargo.toml` (root) — confirms which workspace deps are available.
   You may NOT add new ones.

## Deliverable

Five files, four new + one modified:

- **`crates/mu-core/src/agent/mod.rs`** (new) — module root. Contains
  ONLY the submodule declarations that exist in part A (`types`,
  `provider`, `tool`) and the corresponding `pub use` re-exports.
  **Do NOT declare `pub mod loop_;`** — that file doesn't exist yet
  and will be added in part B. Adding the declaration would break
  the build.
- **`crates/mu-core/src/agent/types.rs`** (new) — implementing the
  §Interfaces `agent/types.rs` block from the spec verbatim, plus
  serde round-trip tests for each public enum/struct.
- **`crates/mu-core/src/agent/provider.rs`** (new) — implementing the
  §Interfaces `agent/provider.rs` block. Tests: serde round-trip for
  `ProviderEvent` enum (every variant), and a compile-time
  `assert_send_sync` for the trait via the hand-rolled
  `fn assert_send<T: Send + Sync>() {}` pattern (no `static_assertions`
  dep — see §INV-8).
- **`crates/mu-core/src/agent/tool.rs`** (new) — implementing the
  §Interfaces `agent/tool.rs` block. Tests: serde round-trip for
  `ToolSpec` and `ToolResult`, and the same `assert_send_sync` pattern
  for the `Tool` trait.
- **`crates/mu-core/src/lib.rs`** (modified) — add `pub mod agent;`.
  One line. Don't touch the existing `pub mod protocol;` or
  `pub mod transport;` lines.

## What NOT to do

- **Don't add new dependencies.** §INV-8 lists what you may use:
  `tokio` (full), `serde`, `serde_json`, `thiserror`, `tracing`,
  `async-trait`, `futures`, plus stdlib. All already in the workspace
  deps.
- **Don't write `loop_.rs`.** That's part B's job. Your `mod.rs` must
  not declare or re-export anything from it.
- **Don't add a `tokio-util` dep** for `CancellationToken`. The spec
  explicitly uses `oneshot::Receiver<()>` for cancel propagation.
- **Don't add a `tokio-stream` dep**. Use `futures::StreamExt`.
- **Don't add derives beyond what the spec specifies.** Look at each
  type's derives in the §Interfaces block and copy them exactly. No
  speculative `Default`, `Eq`, `Hash`, `Copy`, `PartialOrd`. (`StopReason`
  in the spec is `Copy + Eq` because it's a unit-only enum; `AgentMessage`
  and others are deliberately not.)
- **Don't paraphrase or "improve" field names.** Use the §Interfaces
  identifiers verbatim (`call_id`, not `callId` or `tool_call_id`;
  `arguments_delta`, not `args_delta`; etc.).
- **Don't extend the trait surfaces.** Provider has exactly one method
  (`stream`); Tool has exactly two (`spec`, `execute`). Don't add
  helper methods.
- **Don't introduce a separate `agent/error.rs`**. `ProviderError` lives
  in `provider.rs` (already specified). No other error types in part A.

## Verification (run before declaring done)

```sh
cd /home/tcovert/src/public_github/mu
cargo build -p mu-core
cargo nextest run -p mu-core
wc -l crates/mu-core/src/agent/*.rs    # each file under 400
grep -E '\bunsafe\b|\.unwrap\(\)|\.expect\(|\bpanic\!|\btodo\!|\bunimplemented\!' \
  crates/mu-core/src/agent/*.rs \
  | grep -v '^[[:space:]]*//' \
  | grep -v 'cfg(test)'
# ^ should print nothing outside test modules
diff <(grep -E '^(tokio|serde|reqwest|futures|async-trait|thiserror|tracing|sha2|base64|rand|url|urlencoding|ratatui|crossterm|rmcp|toml|clap|anyhow|rusqlite|serde_json) =' Cargo.toml | sort) <(git show HEAD:Cargo.toml | grep -E '^(tokio|serde|reqwest|futures|async-trait|thiserror|tracing|sha2|base64|rand|url|urlencoding|ratatui|crossterm|rmcp|toml|clap|anyhow|rusqlite|serde_json) =' | sort)
# ^ should be empty (no workspace-deps changes)
```

All checks must pass:
1. Build clean, no NEW warnings.
2. All tests green: existing 19 (`protocol::tests::*` + `transport::tests::*`
   + `version_is_nonempty`) plus new tests in your three module files.
3. Each `agent/*.rs` file under 400 lines including tests.
4. No new dependencies.
5. No `unsafe`, no `unwrap`/`expect`/`panic!`/`todo!`/`unimplemented!`
   outside test modules.

## Output protocol

Final message is the JSON envelope:

```json
{
  "status": "complete",
  "files_changed": ["crates/mu-core/src/agent/mod.rs",
                    "crates/mu-core/src/agent/types.rs",
                    "crates/mu-core/src/agent/provider.rs",
                    "crates/mu-core/src/agent/tool.rs",
                    "crates/mu-core/src/lib.rs"],
  "tests_added": [
    "agent::types::tests::<list>",
    "agent::provider::tests::<list>",
    "agent::tool::tests::<list>"
  ],
  "verification_results": {
    "build": "clean",
    "tests_passed": <N>,
    "module_lines": {
      "mod.rs": <N>,
      "types.rs": <N>,
      "provider.rs": <N>,
      "tool.rs": <N>
    },
    "no_new_deps": true,
    "grep_unsafe_unwrap_outside_tests": "empty"
  },
  "design_notes": "<brief note on any judgment calls — e.g., how you structured the assert_send_sync test pattern>",
  "notes": "<anything surprising; especially flag if you saw any files in the working copy that you decided to leave alone per the workspace hygiene rule>"
}
```

If you hit a blocker (compiler error you can't resolve in 3 attempts,
spec ambiguity, behavior you can't make pass), use
`status: "blocked"` and explain in `notes`.
