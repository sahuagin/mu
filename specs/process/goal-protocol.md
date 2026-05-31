# mu — goal-protocol overrides

Per-repo addendum to `~/.claude-personal/skills/goal-protocol/SKILL.md`.

Applies to any `/goal` session run with `cwd=<this-repo>`. Read in addition to the standing rules; per-repo wins on stop criteria, skill wins on briefing format.

## Repo-specific rules of engagement

In addition to the standing rules in SKILL.md:

1. **`cargo test --workspace` is the test command.** Must be green at every commit. No `--release`-only paths. No `#[ignore]` shortcuts unless explicitly authorized — the `mu-yyi` lesson (inject configuration instead of mutating process env) is the preferred pattern.
2. **Bead IDs go in commit messages** — both the closing bead and any newly-filed beads. Example: `feat(capability): X (mu-3ao; files mu-NEW)`.
3. **Phased features have phase-tagged sub-beads.** When a feature naturally splits into phases (A foundation, B behavior, C extension, D delegation), each phase gets its own sub-bead with `addBlockedBy` edges. The mu-036 / mu-28u / mu-3ao / mu-7zn / mu-pv9 pattern is the model.
4. **Wire surface before behavior.** When possible, ship the dispatch/wire types as a phase that returns structured "Phase X not yet wired" errors, so clients can integrate against the protocol before behavior lands. (See mu-036 Phase A.2 for the pattern.)
5. **task_log entries include the bead ID in tags.** `--tags <bead-id>,<phase>,<feature>` so `task_log query --tags <bead-id>` is queryable.
6. **No FreeBSD-only commits.** mu must build clean on the target platforms in CI; FreeBSD-specific work goes behind cfg-guards.
7. **No jj state mutations as claude** when working in the spline jail — but this repo (`~/src/public_github/mu`) is NOT in a jail, so normal `jj describe` / `jj commit` are fine.

## Repo-specific stop criteria additions

Adds to the seven defaults in `references/stop-criteria.md`. These do not remove or weaken the defaults.

### 8. Capability invariant violation (mu-specific application of default #3)

**Trigger:** A proposed change would weaken or bypass any of mu's documented capability invariants:

- **INV-1:** `AutonomyCapability::Disallowed` is the default; sessions must explicitly opt in via attenuated delegation. A change that defaults a new session to `Allowed`, or that allows an unattenuated grant, fires this criterion.
- **INV-N:** (additional capability invariants documented in `specs/architecture/capability-delegation.md` — extend this list as they are formalized)

**On stop:** Apply the standard on-stop protocol. Additionally, the briefing must explicitly name the invariant in question and the specific code path that would have weakened it.

### 9. Provider trait or wire-protocol surface change without spec amendment

**Trigger:** A change to the `Provider` trait, JSON-RPC message shape, or any `EventPayload`/`AgentEvent` variant lands without a corresponding spec update in `specs/mu-NNN-*.md`.

**Why:** mu's protocol IS the product contract. Code-spec drift is the failure mode that erodes trust in the specs over time.

**On stop:** Either (a) update the spec in the same commit, OR (b) halt and document the proposed change for human review. Never let the code lead the spec silently.

### 10. Generated-file edits

**Trigger:** A change touches a file marked auto-generated or vendored (e.g., `target/`, `Cargo.lock` for unrelated reasons, anything under `vendored/`).

**Why:** Generated-file edits are almost always wrong — either the generator should be re-run, or the edit belongs upstream.

**On stop:** Revert the change. If the generator needs to be re-run, do that explicitly and commit the result as a separate bead-tagged commit.

## Repo-specific subagent policy

In this repo:

| Subtype | Allowed for | Disallowed for |
|---|---|---|
| **Explore** | Survey work — "find all uses of X across crates", "list event variants in EventPayload" | Anything that writes |
| **Plan** | Phase decomposition — "given this spec, propose the bead structure" | Anything that writes |
| **general-purpose** | **NOT authorized** without explicit per-experiment justification in the experiment doc | All |
| **claude** (catch-all) | **NOT authorized** | All |

**Codex via pi-rust** is allowed for:
- File summaries with file:line citations
- Test scaffolding (Rust test fn skeletons given a spec)
- Boilerplate codegen INTO TEMP FILES (`/tmp/codex-out-<bead>.rs`) — never directly into the source tree

Codex strikes (per default criterion #5) reset between subtasks within this repo.

## mu-native worker orchestration

When a `/goal` session running *in mu* dispatches workers, it has a native
in-loop path distinct from the skill's host-side `agent-spawn-v2` + `claude -p`
model: the `spawn_worker` tool spawns an interactive (subscription-billed)
Claude under a pty, the worker posts its result back through the mailbox to the
calling session (waking its loop directly), and the host reaps on result.

**Canonical reference:** [`specs/architecture/worker-orchestration.md`](../architecture/worker-orchestration.md)
(as-built) — read it instead of re-deriving from the code + `mu-slat-design.md`
+ scattered memories. Note its **dead-letter gap**: a result posted to a dead
session is silently lost, so a worker's `reply_to` (calling) session must stay
live until the result lands.

## Capability-invariant quick reference

For the stop-criterion #8 check during the session:

| Invariant | Location | Tripwire shape |
|---|---|---|
| INV-1 | `crates/mu-core/src/capability.rs` (Default impl for `AutonomyCapability`) | Default must be `Disallowed`; `intersect()` must produce the most-restrictive of two inputs |
| (add more as they're formalized) | | |

## Workflow shortcuts (mu-specific)

These are conventions that recur often enough to be worth naming:

- **"Phase X.Y commit pattern"**: `<type>(<scope>): mu-036 Phase X.Y — <one-line> (<bead>)`. Example: `feat(protocol): mu-036 Phase A.2 — wire types + EventPayload variants + dispatch stubs (mu-28u)`.
- **"Bead-close commit"**: include `closes mu-XXX` in the commit body when the bead is fully done; the `task_log add` happens after the commit lands.
- **"Spec-amend commit"**: when a code change requires a spec update, both go in the same commit with a body line explaining the surface change.
- **Agent attribution**: use `<runtime> + <model>` form ONLY when the runtime distinction carries information. Today, `claude` alone is sufficient — `claude-code → Claude` is the only meaningful path and the runtime is implicit. Switch to structured form **when accurate**, not aspirationally:
  - **Today**: `claude` (implies claude-code driving Claude)
  - **First mu-driven Claude work**: `mu + Claude` (mu's own loop, not claude-code's)
  - **First Codex-via-mu work**: `mu + Codex` (or model-specific: `mu + gpt-5.5-codex`)
  - **gpt5.5's pi precedent**: their authorship signature is `pi + gpt-5.5` (or `pi` shorthand) — established by the mu-capability-substrate.md doc on PR #1.

  The convention earns its keep when (a) dogfooding makes the runtime distinction load-bearing, or (b) heterogeneous backends produce observable differences in commits. Pre-formalizing before that point makes attribution aspirational rather than informative. Pick the form that's accurate at commit time.

## Test command shortcuts

These are the canonical incantations — use them, don't reinvent:

```bash
cargo test --workspace                          # full test sweep; green-at-every-commit gate
cargo test -p mu-core capability                # focused: capability module tests
cargo test -p mu-coding --test integration      # integration tests for the agent loop
cargo build --workspace --release               # release build (verify before symlinking into ~/.local/bin/mu)
```

## On-stop briefing — mu addendum

Beyond the skill's default briefing format, mu briefings should include:

```
## Capability invariant audit
| Invariant | Held? | Notes |
|---|---|---|
| INV-1 | Y/N | one-line |

## Spec drift check
- All trait / wire / EventPayload changes have matching spec updates? Y/N
```

## Notes / loose threads

- The "wire-stub-before-implementation" discipline from mu-036 Phase A.2 is the recommended default pattern for new protocol features. Phase A ships the schema; subsequent phases swap in behavior. Clients can integrate before behavior lands.
- The `SessionEventLog::from_jsonl` primitive (added for FileBackend / mu-935) is load-bearing for mu-mh4 (session persistence across daemon restart). Coordinate any changes to it.
- BashTool's `b5_strict_env_scrub` / `b10_yolo_env_passes_through` still mutate process env (parallel to the mu-yyi GrepTool fix). Latent race; not currently triggering. Worth a follow-up bead if the parallel-test race ever bites.
