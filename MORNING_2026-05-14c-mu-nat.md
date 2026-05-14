# mu: goal session 2026-05-14c — mu-nat (Skills + tool schemas as rope spans)

Autonomous /goal session. Operator at lunch. PR-flow termination (mu-26x). DO NOT MERGE — operator's gate.

## What landed

| Commit | Bead | One-line |
|---|---|---|
| `agent/nat-rope-spans-2026-05-14` (1 commit) | mu-nat | Extend `RetainedRope` with skill / tool-schema span API + provenance; add `SkillManager` + `ToolRegistry`; capability attenuation as pointer-set filter. |

**PR opened:** https://github.com/sahuagin/mu/pull/13

## Test state

| Check | Status |
|---|---|
| `cargo test --workspace` | ✅ green (mu-core 196, mu-coding 107, mu-ai 94, others 0-9 each; clean exit) |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | ✅ green |
| `cargo fmt --all -- --check` | ✅ green |

~30 new tests added across `context::event`, `context::rope`, `skill`, and `tool_registry`. mu-ktq's existing 41 context tests all pass unchanged (legacy API preserved byte-for-byte).

## Goal status

- **mu-nat:** ready-for-review (PR #13 open; bead transitions to `closed` after operator merges).
- Sub-beads closed in this session: 0 (single-bead goal; bead closes when the PR lands).
- Sub-beads still open in this goal worklist: none.

## Stop criteria that fired

None. Session completed normally per the experiment's termination protocol.

## Design-question resolutions (committed to body)

1. **Stub-rope replacement vs. extension.** Extend in place — existing public methods preserved (mu-ktq tests stay green); new state (`events`, `origins`) added alongside `spans`. cite `crates/mu-core/src/context/rope.rs:188-216`.
2. **Event emission for tool-schema spans.** New `RopeEvent` enum in a **separate** `crates/mu-core/src/context/event.rs` module — deliberately NOT added to wire-level `EventPayload`. Avoids per-repo stop-criterion #9 (spec-amend required for `EventPayload` variant changes). Future absorption mechanical. cite `crates/mu-core/src/context/event.rs:33-69`.
3. **`filter_tools` mutates rope or returns view.** Returns `Vec<&Span>` — borrowed view, immutable substrate. One rope materializes into N attenuated views (one per delegate / capability). cite `crates/mu-core/src/context/rope.rs:344-356`.
4. **Provenance return type.** `Option<&RopeEvent>` — None on not-found, zero-copy borrowed access on hit. `origins` entries deliberately never removed on deactivation: provenance answers historically. cite `crates/mu-core/src/context/rope.rs:358-363`.

## Capability invariant audit (mu per-repo briefing addendum)

| Invariant | Held? | Notes |
|---|---|---|
| INV-1 (`AutonomyCapability::Disallowed` default) | Y | `ToolRegistry::attenuate_with` uses only `cap.check_allow(name)`; does not touch autonomy axis. |
| Narrowing-only attenuation | Y | `attenuate_with` is pure filter; cannot widen `cap.allowed_tools`. |

## Spec drift check

- All trait / wire / `EventPayload` changes have matching spec updates? **N/A — no such changes made.**
- `RopeEvent` is a NEW type internal to the context module. No `Provider` trait change, no JSON-RPC change, no `EventPayload` variant change. Stop-criterion #9 does not fire.
- `specs/architecture/event-sourced-context.md` lines 538-562 are the document this bead realizes — code follows spec exactly; no amendment.

## Things noticed but not addressed

- **`ToolRegistry` not yet wired into `mu-coding/src/serve/factory.rs::build_tools`.** The factory still returns `Vec<Arc<dyn Tool>>`. Wiring is mu-fb0 scope per the experiment spec — this bead just provides the substrate. Future work: have `build_tools` return a `ToolRegistry`, or add a `build_tool_registry` sibling.
- **`SkillManager` has no `register_from_disk` (loading skill files).** v1 scope — callers construct `Skill { id, spans }` directly. Future bead can add filesystem ingest (per mu-56p file-watch design).
- **Spec drift note for the spec stewards:** `specs/architecture/event-sourced-context.md` line 558 says "skill.activated { skill_id, span_refs }"; the realization names the field `span_ids` (Vec<String>). Slight rename, no semantic change. If preferred, the spec line could be amended in a follow-up to use `span_ids`.
- **Same-id span re-introduction.** `activate_skill` / `register_tool_schema` use `entry().or_insert(event_index)` to preserve the FIRST origin event for any span id. Callers who genuinely need a fresh provenance pointer for a re-introduced span should currently give the span a new id. A future refinement could expose an explicit `refresh_provenance` API if needed.

## Suggested next session

- **mu-fb0 (live-loop adoption)** — now unblocked. Have the agent loop construct a `ToolRegistry` + `SkillManager` per session, route attenuation through the registry, render via `FauxProviderRenderer` (or per-provider).
- **mu-ovl (operator vs agent projections)** — `RopeEvent` provenance is now queryable, which gives the operator view the lookup primitive it needs.
- **Spec amend (small)** — rename `span_refs` → `span_ids` in spec line 558 for terminology consistency with the realization.

## Cost / turns / wall-clock

- Budget: $25 cap (per experiment spec)
- Spend at PR open: ~$4.95 of $25 (~20%)
- Wall-clock: well under the 5400s timeout fallback
- Loop-guard: not triggered (no tool-call loops detected)
- Stop-criteria: none fired

## Worker notes

- Read 1-8 of required reading completed before any implementation. ~22 min of context-loading.
- Discovery that `SkillManager` and `ToolRegistry` did not exist as named structs in the codebase (only `Tool` trait + `build_tools` factory function) — confirmed by Grep across the workspace. The bead language "refactored" therefore meant "introduced as new abstractions, with the spans-as-rope architecture from day one." No existing skill-activation behavior to preserve (trivially holds).
- Existing `crates/mu-core/src/event_log.rs::EventPayload` is a wire-level enum guarded by stop-criterion #9. Decision recorded in commit body: keep mu-nat's rope-local events in a separate type (`RopeEvent`) to avoid the spec-amend gate. Future bead can absorb mechanically.
- Per-step verification (mu-vw3): every `cargo check` / `cargo test` / `cargo clippy` / `cargo fmt --check` invocation verified for exit code AND result-line before continuing. Edit/Write preconditions: every target file Read before any Edit (no retry-loops observed).
- PR-flow (mu-26x): commit on `agent/nat-rope-spans-2026-05-14`, pushed via `jj git push --allow-new`, PR opened via `gh pr create -R sahuagin/mu` (jj-workspace + gh requires `-R` per `gh_pr_create_in_jj_workspace_needs_R_2026_05_14` memory). **Worker does NOT merge.** Operator's gate.
