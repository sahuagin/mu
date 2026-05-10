# Delegation: implement mu-001

This file is the prompt sent to a sub-agent (currently `agent-router
--auth codex-oauth`) to implement spec `mu-001`. It is committed to the
repo so the prompt itself is reviewable.

---

You are working in the `mu` Rust workspace at `/home/tcovert/src/public_github/mu`.
This is a brand-new project; `crates/mu-core/src/lib.rs` is currently a
stub with just a `version()` function and one smoke test.

## Read first (REQUIRED)

1. `specs/mu-001-protocol-types.md` — the full specification. This is your
   primary directive. Treat §Invariants as hard constraints, §Behaviors
   as test requirements, §Interfaces as the exact code shape to produce,
   §Out-of-circuit warnings as bug-prevention notes.
2. `crates/mu-core/src/lib.rs` — the existing file you'll modify (one
   line added: `pub mod protocol;`).
3. `AGENTS.md` (root) — project-wide rules. Especially: no `unsafe`, no
   third-party-OAuth-token holding, errors per crate (`thiserror`).

## Deliverable

Two files modified, no other changes:

- **`crates/mu-core/src/protocol.rs`** (new) — implementing the
  §Interfaces block from the spec verbatim, plus a `#[cfg(test)] mod
  tests` covering every behavior B-1 through B-7. Tests must use
  `serde_json::to_value` / `serde_json::from_value` round-trips, not
  just `to_string` round-trips, because the field-presence behaviors
  (B-3, B-6) need structured access.
- **`crates/mu-core/src/lib.rs`** (modified) — add `pub mod protocol;`
  and nothing else.

## Verification (run before declaring done)

```sh
cd /home/tcovert/src/public_github/mu
cargo build -p mu-core
cargo nextest run -p mu-core
wc -l crates/mu-core/src/protocol.rs    # under 800
grep -E '\bunsafe\b|\.unwrap\(\)|\.expect\(' crates/mu-core/src/protocol.rs \
  | grep -v '^[[:space:]]*//' \
  | grep -v 'cfg(test)'
# ^ the last command should print nothing outside test modules
```

All three must pass:
1. Build clean, no warnings beyond pre-existing ones
2. All tests green (the existing `version_is_nonempty` plus B-1..B-7)
3. Module under 800 lines

## What NOT to do

- Don't touch any other file. No formatting passes on existing code, no
  README updates, no Cargo.toml additions.
- Don't add derives beyond what the spec specifies (§Invariant 1 lists
  the exact set: `Serialize, Deserialize, Debug, Clone, PartialEq`).
  Specifically don't add `Default`, `Eq`, `Hash`, or `PartialOrd` even
  where they'd compile cleanly. (See §OOC-3.)
- Don't introduce new dependencies. Use `serde`, `serde_json`, and
  stdlib only. Both are already in `mu-core`'s `Cargo.toml`.
- Don't restructure the protocol into multiple files / sub-modules.
  §Invariant 6 says one file under 800 lines; this is deliberate.
- Don't paraphrase or "improve" field names. The §Interfaces block is
  the contract and the reviewer-facing identifier list.

## Output protocol

When done, your final message should be a JSON envelope:

```json
{
  "status": "complete",
  "files_changed": ["crates/mu-core/src/protocol.rs",
                    "crates/mu-core/src/lib.rs"],
  "tests_added": ["round_trip_request", "..."],
  "spec_coverage": {
    "B-1": "<test name(s) that prove it>",
    "B-2": "...",
    "B-3": "...",
    "B-4": "...",
    "B-5": "...",
    "B-6": "...",
    "B-7": "..."
  },
  "verification_results": {
    "build": "clean",
    "tests_passed": <N>,
    "module_lines": <N>
  },
  "notes": "<anything surprising or worth flagging for review>"
}
```

If you hit a blocker (spec ambiguity, build failure you can't resolve in
3 attempts), use `status: "blocked"` and explain in `notes` what's
blocked and what info would unblock you.
