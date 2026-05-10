# Delegation conventions

Universal rules for sub-agent delegations on `mu`. Each per-spec
delegation prompt (`specs/mu-NNN-delegation.md`) references this
file rather than restating these rules. The per-spec prompt only
covers what's *spec-specific*: deliverable list, what NOT to do
that's tied to the spec's content, and the verification commands
particular to the work.

This separation exists because we noticed delegation prompts had
~50% boilerplate overhead, eating the delegate's attention budget
for the actual problem. Writing once here, referencing each time,
gets that budget back.

---

## Rules

### 1. Working directory

You are working in a fresh `jj` workspace at the directory passed as
your `--cwd` (or however your runtime was invoked). Treat it as the
*entire* world for this task. Do not reach outside it (no
`/home/whoever/`, no `/tmp/`-elsewhere paths).

The workspace is **already isolated** from other concurrent
delegations. There are no parallel-session files to step on. You
own this checkout for the duration of your task. Branch
collaboration happens at orchestrator level after you finish — your
job is to land your work cleanly within your scope.

### 2. The spec is the contract; the prompt is the dispatch

Every delegation references a spec file (`specs/mu-NNN-*.md`). The
spec defines:
- §Why — motivation
- §Scope — what's in / out / non-goals
- §Invariants — hard constraints (normative)
- §Interfaces — code shapes (illustrative; if §Invariants forbid
  what an §Interfaces sketch shows, follow the invariants)
- §Behaviors — testable observable behaviors (each must have a
  passing test)
- §Acceptance — concrete done-ness criteria
- §Out-of-circuit warnings — bug-prevention notes

The delegation prompt adds: deliverable file list, spec-specific
"don't do this" items, verification commands, expected output
envelope. **It does not contradict the spec. If it appears to,
that's a bug — see Rule 3.**

### 3. If the spec and the prompt disagree

Make the call your judgment supports and SURFACE the deviation in
your output envelope's `notes` field. Do NOT silently fix and do
NOT refuse. Past delegations have flagged genuine inconsistencies
this way (mu-003a's `futures` dep, mu-004a's `expect("mutex
poisoned")` in non-test code, mu-007's avoidance of `async-trait`
as a new dep). That's the behavior we want.

### 4. Treat invariants as normative; sketches as illustrative

§Invariants are hard constraints. §Interfaces blocks are
illustrative — they show *one* shape that satisfies the invariants.
If a sketch contains code that VIOLATES an invariant (e.g., shows
`expect("...")` while INV-X says "no expect outside tests"),
follow the invariant.

### 5. Output envelope

Every delegation's final message is a JSON envelope in this shape
(extend per-spec where the prompt asks):

```json
{
  "status": "complete" | "blocked",
  "files_changed": ["path/relative/to/workspace/root", ...],
  "tests_added": ["module::path::test_name", ...],
  "verification_results": {
    "build": "clean",
    "tests_passed": <N>,
    "module_lines": <N | object of paths>,
    "no_new_workspace_deps": true | false,
    "grep_unsafe_unwrap_outside_tests": "empty" | "<paste of matches>"
  },
  "design_notes": "<judgment calls and any sub-trait/desugaring choices>",
  "notes": "<deviations from spec/prompt; anything surprising; blockers>"
}
```

If `status: "blocked"`, fill `notes` with: what's blocked, what info
or decision would unblock you, what you've tried.

### 6. Universal don'ts

These apply regardless of the spec:

- **Don't add new workspace dependencies.** "Workspace" here means
  the root `Cargo.toml`'s `[workspace.dependencies]`. Per-crate
  `Cargo.toml` files MAY add a workspace-already-listed dep to
  their own `[dependencies]` section if a new use case requires it
  — that's not a "new dependency" in this rule's sense (resolved
  per mu-003 INV-8 amendment).
- **No `unsafe`. No `unwrap` / `expect` / `panic!` / `todo!` /
  `unimplemented!` outside `#[cfg(test)]`.** Tests may use these.
- **Don't reformat existing code.** No "while I'm here" cleanup.
  Touching files outside your deliverable list is the most common
  way delegates introduce diff noise; resist.
- **Don't paraphrase or "improve" identifier names.** §Interfaces
  identifiers are the contract surface; reviewers and consumers
  expect them verbatim.

### 7. Verification ritual

Every delegation runs the same shape of pre-completion check:

```sh
# Build is clean (no new warnings beyond pre-existing).
cargo build -p <crate>

# Tests pass.
cargo nextest run -p <crate>

# No safety violations outside tests.
grep -nE '\bunsafe\b|\.unwrap\(\)|\.expect\(|\bpanic\!|\btodo\!|\bunimplemented\!' \
  <relevant files> \
  | grep -v '^[[:space:]]*//' \
  | grep -v 'cfg(test)'
# ^ should print nothing

# File sizes within spec invariants.
wc -l <files>

# Workspace deps unchanged at root.
git diff Cargo.toml
# ^ should be empty unless the spec specifically calls for a workspace-dep change
```

Per-spec prompts may add extra checks (e.g., "no new dependencies in
the per-crate Cargo.toml beyond X"). Run them all. Report results
in the envelope's `verification_results` block.

### 8. Read order

Before writing any code, read in this order:
1. The spec (the file the delegation prompt names as "primary directive").
2. The crate's existing files that your work will compose with —
   the spec's §Prior work / §Read first list points to these.
3. The workspace's `AGENTS.md` for project-wide rules.
4. The crate's `Cargo.toml` to see what deps are available.
5. Any existing test files in the same module to match style.

This is a hard ~5-minute investment; it pays back in not-rewriting
code that contradicts existing patterns.

---

## What this file deliberately doesn't have

Things that belong in **per-spec** delegation prompts, NOT here:

- Specific deliverable file paths
- Spec-specific "don't do this" items (e.g., "don't use
  `tokio::fs::read` instead of `spawn_blocking`")
- Specific identifier names to use verbatim
- Verification commands tailored to the spec (e.g., live API smoke
  tests gated on env vars)

If a rule applies to MORE than one delegation, lift it into this
file and reference from the per-spec prompt. If a rule is genuinely
spec-specific, leave it in the per-spec prompt.

## Changelog

- 2026-05-10 — initial draft, extracting universal rules from
  mu-001a, mu-002, mu-003a, mu-004a, mu-007 delegation prompts. The
  pre-existing prompts stay as historical record; future prompts
  reference this file.
