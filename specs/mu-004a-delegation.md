# Delegation: implement mu-004 part A (FauxProvider)

This file is the prompt sent to a sub-agent (currently `agent-router
--auth codex-oauth`) to implement the FauxProvider portion of spec
`mu-004`. It is committed to the repo so the prompt itself is
reviewable.

**This is part A of a two-part split.** Part A builds `FauxProvider`
in `mu-ai`. Part B (claude, in-conversation) will build the `mu serve`
wiring in `mu-coding` on top of the foundation you build here.

---

## Workspace hygiene (READ FIRST)

You are working in `/home/tcovert/src/public_github/mu`. A parallel
claude-code session may be editing this same workspace concurrently.
If you see files in the working copy that are NOT in your deliverable
list (in particular: anything under `specs/`, anything you didn't
create yourself), **leave them alone**.

Specifically: do NOT `jj restore`, `jj abandon`, `git checkout --`, or
otherwise revert files that aren't yours. Do NOT delete untracked
files. Your "don't touch any other file" rule extends to *restore*
operations, not just edits.

This rule exists because a prior attempt deleted in-flight work from a
parallel session via `jj restore`. See
`specs/delegations/mu-002-attempt-1-postmortem.md` if you want context.

---

## If the spec and this prompt disagree

Make the call your judgment supports and SURFACE the deviation in
your output envelope's `notes` field. Don't silently fix and don't
refuse. Past delegations have flagged genuine inconsistencies this
way (mu-003a's `futures` dep) — that's the behavior we want.

---

## Read first (REQUIRED)

1. `specs/mu-004-serve-and-faux-provider.md` — full specification.
   Your scope is the `crates/mu-ai/src/faux.rs` §Interfaces block,
   the §Behaviors B-1..B-3, and the modifications to
   `crates/mu-ai/src/lib.rs` and `crates/mu-ai/Cargo.toml`. The
   §Interfaces blocks for `serve/*.rs` and the dispatcher
   §Behaviors B-4..B-7 are **out of scope for you** — those are part B.
2. `crates/mu-core/src/agent/provider.rs` — the `Provider` trait
   you're implementing.
3. `crates/mu-core/src/agent/types.rs` — the `AgentMessage`,
   `AssistantMessage`, `ContentBlock`, `StopReason` types your impl
   will construct.
4. `crates/mu-core/src/agent/tool.rs` — `ToolSpec` (your `stream`
   impl ignores tools but the signature uses the type).
5. `crates/mu-ai/Cargo.toml` — confirms which deps `mu-ai` already
   has. Empty `[dependencies]` to a first approximation; you'll add
   the ones the §Interfaces block uses.
6. `crates/mu-ai/src/lib.rs` — the existing stub. Your modification
   is two lines: `pub mod faux;` and `pub use faux::{FauxProvider,
   FauxResponse};`.
7. `AGENTS.md` — project-wide rules.
8. Existing FauxProvider-shaped reference: in
   `crates/mu-core/src/agent/loop_tests.rs` there's a `MockProvider`
   that's structurally similar. Your FauxProvider is the public,
   non-test version of that pattern. You may reference but do not
   import — that one is `#[cfg(test)]` only inside mu-core.

## Deliverable

Three files, two new + two modified:

- **`crates/mu-ai/src/faux.rs`** (new) — implementing the
  §Interfaces block from the spec verbatim, plus three tests for
  B-1, B-2, B-3.
- **`crates/mu-ai/src/lib.rs`** (modified) — add:
  ```rust
  pub mod faux;
  pub use faux::{FauxProvider, FauxResponse};
  ```
  Keep the existing `pub fn version()` and its test as-is. Don't
  reorder.
- **`crates/mu-ai/Cargo.toml`** (modified) — add the per-crate deps
  the §Interfaces block requires:
  - `mu-core = { path = "../mu-core" }` — for the Provider trait and
    types.
  - `tokio = { workspace = true }` — for `oneshot`.
  - `serde = { workspace = true }` — for `Serialize/Deserialize`
    (Provider's events derive these).
  - `serde_json` — only if your tests use it. Otherwise leave out.
  - `async-trait = { workspace = true }` — for the trait impl.
  - `futures = { workspace = true }` — for `BoxStream` and
    `stream::iter`.
  - `thiserror = { workspace = true }` — only if you actually use it
    for FauxProvider error variants. Probably not needed.
  - `tracing = { workspace = true }` — only if you use it. Probably
    not needed in faux.rs.
  Use your judgment on which subset is actually needed by the code
  you write. Document the chosen subset in `notes`.

## What NOT to do

- **Don't add new workspace deps.** Per-crate additions of
  workspace-listed deps are fine (per mu-003 INV-8 amendment).
- **Don't touch any file outside the deliverable list.** Especially:
  don't modify `crates/mu-core/`, don't modify `crates/mu-coding/`,
  don't `jj restore` anything you didn't author, don't update
  the workspace-root `Cargo.toml`.
- **Don't add a `Tool` impl** — only `Provider`. Tools are mu-coding's
  job (and not in this spec).
- **Don't write the serve module.** That's part B.
- **Don't paraphrase identifiers.** §Interfaces names are the
  contract: `FauxProvider`, `FauxResponse`, `Script`, `Echo`,
  `scripted`, `echo`, `responses`, `fallback`. Use these verbatim.
- **Don't add derives beyond what the spec specifies.** §Interfaces
  shows `#[derive(Debug, Clone)]` on `FauxResponse`. `FauxProvider`
  itself doesn't derive anything in the spec — leave it that way.
  (No speculative `Default`, `Serialize`, etc.)

## Verification (run before declaring done)

```sh
cd /home/tcovert/src/public_github/mu
cargo build -p mu-ai
cargo nextest run -p mu-ai
wc -l crates/mu-ai/src/faux.rs    # under 800
grep -nE '\bunsafe\b|\.unwrap\(\)|\.expect\(|\bpanic\!|\btodo\!|\bunimplemented\!' \
  crates/mu-ai/src/faux.rs \
  | grep -v '^[[:space:]]*//' \
  | grep -vE 'fn .* \{$' \
  | grep -v 'cfg(test)'
# ^ should print nothing outside test modules. expect("mutex poisoned")
# is acceptable inside tests; see how mu-core/src/agent/loop_tests.rs
# uses it.
cargo build  # also confirm the workspace as a whole still builds
cargo nextest run  # full suite still passes
```

All checks must pass:
1. `cargo build -p mu-ai` clean, no warnings.
2. `cargo build` (workspace) clean, no warnings.
3. `cargo nextest run -p mu-ai` passes — your B-1..B-3 tests.
4. `cargo nextest run` (workspace) passes — every existing test
   still green (currently 36; should be 36+3 = 39 after your work,
   modulo any version_is_nonempty test in mu-ai that already exists).
5. `crates/mu-ai/src/faux.rs` under 800 lines.
6. No `unsafe`, no `unwrap`/`expect`/`panic!`/`todo!`/`unimplemented!`
   outside test modules.

## Output protocol

```json
{
  "status": "complete",
  "files_changed": ["crates/mu-ai/src/faux.rs",
                    "crates/mu-ai/src/lib.rs",
                    "crates/mu-ai/Cargo.toml"],
  "tests_added": ["tests::echo_returns_last_user_message",
                  "tests::scripted_drains_in_fifo_order",
                  "tests::out_of_responses_returns_empty_stream"],
  "verification_results": {
    "build_p_mu_ai": "clean",
    "build_workspace": "clean",
    "tests_passed": <N>,
    "module_lines": <N>,
    "no_new_workspace_deps": true,
    "grep_unsafe_unwrap_outside_tests": "empty"
  },
  "deps_added_per_crate": ["mu-core", "tokio", "serde", "async-trait", "futures"],
  "design_notes": "<which deps you actually used and why; any judgment calls>",
  "notes": "<anything surprising; especially flag if you saw any files in the working copy that you decided to leave alone per the workspace hygiene rule, or if the spec/prompt disagreed>"
}
```

If you hit a blocker, use `status: "blocked"` and explain in `notes`.
