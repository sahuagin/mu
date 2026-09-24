# AGENTS.md — mu

The canonical agent/cc context for this repository: what mu is, how it's laid
out, how to build and test it, the architecture invariants you must not break,
and how work flows here. Universal operating conduct (VCS discipline, work
claiming, tone) comes from the operator's environment-level config; this file is
mu-specific.

## What mu is

A coding agent and standalone agent runtime, built as a Rust workspace. The core
model: **`mu serve` is the JSON-RPC core daemon; everything else is a frontend**
to it — the TUIs, the one-shot `ask`, the web console. Sessions are
event-sourced (see the architecture invariants below).

## Current development map

If you are here to continue mu development, especially the long-running goal of
replacing Claude Code as the daily coding environment, read:

- `specs/plans/mu-claude-code-replacement-current-state.md`

That document is a repo-versioned operational snapshot: current terrain, stale
beads to reconcile, and recommended next work. It is a map, not an invariant;
verify against files, tests, `jj status`, and central beads before acting.

## Workspace layout

Twelve crates (`Cargo.toml` `[workspace]`):

| crate | role |
|---|---|
| `mu-core` | agent loop, JSON-RPC protocol, transport, session state |
| `mu-ai` | LLM provider abstraction |
| `providers/mu-anthropic` | Anthropic Messages API wire protocol as typed Rust (standalone; no `mu-core` dep) |
| `providers/mu-anthropic-py` | thin pyo3 binding over `mu-anthropic` |
| `providers/mu-openai` | OpenAI Responses API wire protocol as typed Rust (standalone; no `mu-core` dep) |
| `mu-events-py` | pyo3 helpers for mu event-log processing |
| `mu-coding` | the coding agent; **owns the `mu` binary** (`src/bin/mu.rs`) |
| `mu-tui` | terminal UI for `mu serve` |
| `mu-solo` | standalone single-pane chat TUI for `mu serve` |
| `mu-bridge` | Claude-code JSONL → mu event format (pyo3) |
| `t4c` | tools4claude — capability/tool discovery |
| `mu-dialogue` | inter-agent dialogue MCP service used by mu/cc workers |

The `mu` CLI subcommands: `serve` (daemon), `ask` (one-shot), `resume`, `tui`,
`orchestrate`, `console`, `login`/`logout`, `mark`, `list-sessions`,
`analytics`, `models`, `capabilities`, `audit`, `versions`.

## Build & test

- Toolchain: **stable** with `rustfmt` + `clippy` (`rust-toolchain.toml`).
  Host tools this repo's recipes call and does not vendor: `just`, `jj`,
  `invariant-audit` (agent_tools; used by `just invariants`). Pinned install,
  one line — bump the rev here when the tool changes:

  ```sh
  cargo install --git https://github.com/sahuagin/agent_tools --rev f3a3ab4e6e5c invariant-audit
  ```

  `scripts/tests/invariant-audit-test.sh` checks the installed tool parses this
  repo's shapes file.
- **`just ci` is the gate** — `fmt-check` → `clippy` → `test`, fail-fast in that
  order; it mirrors `.github/workflows/ci.yml` verbatim. A green `just ci` is the
  local proxy for green CI. Run it before pushing. The three steps are:
  - `cargo fmt --all -- --check` — **check-only, never rewrites files** (use
    `just fmt` to actually format)
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  - `cargo test --workspace --all-features --no-fail-fast` — plain `cargo test`,
    not nextest
- `just check` runs the superset `scripts/pre-pr-check.sh` (the `just ci` checks
  plus `verify-claims`). **Nothing runs it for you** — there is no PR-open hook
  or `gh` shim. Run it yourself.
- `just check-quick` — fmt + clippy only (fast inner loop).
- **`just ci-aipr` is required before opening a PR** — it runs the pre-PR checks
  then the cross-provider review panel (`scripts/ai-review.sh`). Local-only, not
  a CI step, so nothing else will run it for you. Reviewer seats come from
  `agent-role code_review`; probe `/api/ps` before using an ollama seat so a
  panel run can't evict a model someone is holding. Budget **10-20 min** on the
  all-API roster (it was 45-80 before mu-ash9p; a timing run of the new path took
  639s) — a local seat can hold a round open longer, on purpose. The panel runs
  at most `MU_REVIEW_MAX_ROUNDS` (3) rounds and caps each seat by provider class,
  with no retry (a non-empty reply that parses to nothing gets one verdict
  re-ask, itself capped by `MU_REVIEW_REASK_TIMEOUT_SECS`, 180s):
  `MU_REVIEW_SEAT_TIMEOUT_SECS` (900s) for an API seat,
  `MU_REVIEW_LOCAL_SEAT_TIMEOUT_SECS` (1800s) for an ollama/vllm/LAN-endpoint one,
  since measured local seats take 26-36 min while every API seat answers inside
  13. A seat that times out or returns unparseable output is ABSENT for that
  round rather than a dissenter — so a dead seat no longer forces every round; at
  least `MU_REVIEW_MIN_LIVE_SEATS` (3, a majority of the five seats) must answer
  for a round to conclude, an approve additionally needs every exclusive seam
  seat live (nobody else reviews its checklist), and the absent ones are named
  on the PANEL line. The
  cargo steps are not repeated when `just ci` or `just check` was already green
  **on this exact commit** (receipt: `target/ci-green-<commit>`; the cheap audits
  and `verify-claims` still run) — `MU_REVIEW_FORCE_CHECK=1` re-runs them.
- **Seats are seams.** A `[[code_review.ranked]]` rank in
  `~/.config/mu/agent_roles.toml` carries either `focus` (soft emphasis) or
  `seam` + `checklist` (exclusive: that seat reviews for its checklist only).
  `seam = "conformance"` checks the diff against this file's *Architecture
  invariants* verbatim, so keep that section current: it is the reviewer's
  checklist, not prose. Put the conformance seam on a different model family
  from the correctness seat; bias diversity pays across families, not within
  one. Chunked mode (oversized branches) reviews each unit through these same
  seam lenses too: an unseamed leaf plus one leaf per seam.
  `MU_REVIEW_CHUNK_MAX_DISPATCHES` (default 40) is a TRUE cap on the actual leaf
  model calls, timeout retries included: up front on the plan units × (1 + seams)
  — if units alone exceed it the branch can't be chunked and the gate ESCALATEs
  (split the branch); if units fit but the product doesn't, the seam lenses are
  dropped and only the unseamed leaves run — and again at runtime, where a leaf
  that times out is not retried once the running count of leaf calls has reached
  the cap (that leaf is recorded as unreviewed). The conformance seam is skipped
  when this file declares no *Architecture invariants*.
- **Increments are capped.** `ci-aipr` BLOCKs a diff over
  `MU_REVIEW_MAX_DIFF_LINES` (default 2000 reviewable lines; lockfiles and
  binary/media files don't count) with a SIZE finding and suggested split points.
  Split the branch into stacked PRs, one increment each. `MU_REVIEW_CHUNK=1`
  (degraded per-commit review) and `MU_REVIEW_SIZE_OVERRIDE=1` (review as is)
  are the explicit fallbacks; both leave the panel verdict binding.
  `MU_REVIEW_OVERRIDE=1` is the verdict override and waives that too.

## Running it

- `just smoke` — faux-provider `mu ask` (no API key needed); the fastest
  end-to-end smoke test.
- `just ask "…"`, `just serve …`, `just tui …`, `just solo …` — pass-throughs.
- Direct: `cargo run -p mu-coding --bin mu -- <subcommand>`.

## Orchestration pipeline (`scripts/orchestrator/`)

A gated multi-model pipeline for autonomous coding tasks, layered on `mu ask` — local tooling
(like `ci-aipr`), not a CI step. Flow: **SPEC-CRITIC** (request coherence; halts a contradictory
/ ambiguous request) → **ARCHITECT** (invariant veto) → **PLAN** → **IMPLEMENT** (worker in an
isolated `sprint-start` workspace) → **\[CONVERGE** — `CONVERGE_WORKERS≥2` fans out competing
workers, a converger picks the best**]** → **REVIEW** (`ci-aipr`) → **ADJUDICATE**
(`SHIP`/`ITERATE`/`ESCALATE`).

- **Run:** `scripts/orchestrator/orchestrate.sh <task-file> <repo-dir>`. Artifacts land in
  `RUN_DIR` (default `~/orchestrator-runs/run-<ts>/`): `summary.md`, per-stage `<stage>.out`,
  `worker.diff`, `provenance.jsonl`.
- **Defaults:** gate seats resolve from the `orchestrate_gate` role
  (`agent-role orchestrate_gate`, with `openai-codex/gpt-5.5` as the
  script fallback); worker seats resolve from the `coding` role
  (`agent-role coding`, with `ollama/qwen3.6:27b` as the script fallback).
  `agent-role` is ollama-lease-aware: if the shared ollama box is held by
  another owner, ollama ranks sink below available non-ollama ranks. Knobs:
  `SEAT_`/`WORKER_`/`ARCHITECT_`/`SPEC_CRITIC_`/`CONVERGER_` `PROVIDER`+`MODEL`;
  `CONVERGE_WORKERS` (1 = single worker); `SPEC_GATE=0` / `ARCHITECT_GATE=0`
  to skip a gate; override a gate through the same role/env machinery when you
  need a deeper skeptic.
- **The worker writes autonomously and merges nothing** — review `worker.diff`. The neutral
  per-stage role prompts sit beside `orchestrate.sh`
  (`{spec-critic,architect,conductor,worker,converge}-prompt.txt`); role→model ranks live in
  `~/.config/mu/agent_roles.toml` via `scripts/agent-role`. The REVIEW stage pulls a metered
  `openrouter/deepseek` reviewer — the one non-free path.
- **Reusable dispatch — `scripts/lib/agent-dispatch.sh`.** `agent_dispatch <provider> <model>
  [<prompt-file>]` runs one model and prints its stdout, reading `TOOLS` / `SYSPROMPT` /
  `MAX_TURNS` / `THINKING` / `ERRLOG` from the caller's scope and routing ToS-cleanly
  (`claude-oauth` → `claude -p`; anything else → `mu ask --bare`). It backs both
  `orchestrate.sh` and `ci-aipr` — **source it** to build sibling loops (e.g. a
  benchmark/score-select loop) rather than re-implementing dispatch; copy `orchestrate.sh`'s
  `dispatch()` wrapper (one `provenance.jsonl` line per call) for reproducible runs.

## Architecture invariants — do not break these

1. **The on-disk event log is the source of truth.** Events persist to JSONL at
   `<state_dir>/events/<daemon_id>/<session_id>.jsonl` (`state_dir` defaults to
   `~/.local/share/mu`). In-memory session state is a *projection* rebuildable
   from the log: write to disk first, then map into memory.
2. **Durability is two-tier (spec mu-046).** The command journal is the
   fail-closed write-ahead path — an inbound command is journaled before
   processing, and a failed append *rejects* the command
   (`JOURNAL_UNAVAILABLE = -32003`). In disk-backed daemons,
   resume-bootstrap events are likewise fail-closed because the new head needs
   a durable copy of inherited context. Explicitly ephemeral daemons
   (`events_dir = None`, primarily tests) preserve bootstrap ordering in memory
   but make no persistence claim.
   Ordinary session-log gateway events (tool results, assistant messages) are
   best-effort disk-before-memory appends: IO errors are logged and ignored, not
   fatal.
3. **Rehydration is lazy and request-driven**
   (`mu-lazy-session-rehydration-bh4f`). `mu serve` parses nothing on cold start;
   a past session is loaded by id the first time it's addressed. Enumeration is
   the offline `mu list-sessions` (reads each log's first record + mtime only).
4. **Deep design lives in `specs/`** — the `architecture/` subdir, the numbered
   `mu-NNN` specs, and `specs/plans/`. Read it for the *why*; put new design docs
   there, **not** in crate roots.
5. **Increments are reviewable.** New capability is built at a seam (its own
   crate or module, to spec, tested in isolation) and integrated in a separate
   increment. An increment over the review gate's line cap is split, not
   chunked (see *Build & test*).
6. **Tunables are config, never compiled constants.** Provider and model
   choices, their rate cards, context limits and request rules live in the
   config layers (`crates/mu-core/config/models.default.toml` < generated
   layers < `~/.config/mu/models.toml`; `[settings]` in `config.toml`) and
   change without a build. A number in Rust that would need a rebuild to
   change — a price, a model id in a table, a dollar ceiling — is a bug the
   gate rejects: `.invariants.toml` `rate-cards-are-config` runs in
   `scripts/pre-pr-check.sh`. (mu-1x0ze: pricing was a compiled table and
   mu-tui carried a mocked `$10` budget for months.)
7. **Fail fast, fail loud — never degrade silently.** Two distinct duties, and
   a component owes both.

   **FAIL FAST** — stop, rather than continue in a state that cannot do the job:
   - a misconfiguration is a refusal to START, not a successful boot that
     answers nothing;
   - a quorum — a review panel, a consensus, a vote — does not issue a binding
     result as though it still had the participants it lost;
   - a capability that fails to resolve is not silently latched off for the
     process's life; it is retried when next needed, or its absence is an
     error the caller can see.

   **FAIL LOUD** — say what is wrong AND what to do about it. A diagnostic that
   only reports failure is half the duty; the reader still has to go and find
   the cause. Name the fault, the thing that faulted, and the remedy:
   - report a fault as ITSELF, not as the failure of the thing it disabled (an
     expired credential is not "the model is unavailable");
   - work killed by OUR OWN limit — a timeout, a cap, a budget — is recorded as
     our limit firing, never as the dependency failing;
   - name the setting that actually governs it: a diagnostic pointing at the
     wrong knob sends the reader to the wrong file;
   - prefer a typed, distinguishable error over prose. **The reader is
     increasingly a MODEL, not a person** — a spawned worker or an autonomous
     run has to decide from this whether to retry, route around, tell a peer or
     file a bead, and it cannot do that reliably by pattern-matching a
     sentence.

   A fallback is fine; a *silent* fallback is a bug. The test to apply: would
   whoever is operating this — human or model — have to diagnose by hand
   something the system already knew?

   (Scars, all 2026-09-23: an expired OpenRouter key quietly dropped the review
   panel to 3/5 seats while it went on issuing binding verdicts
   (`mu-review-panel-openrouter-key-expired-e8yvl`); every mesh-enabled `mu`
   session boots without `code_recall`/`code_status` behind two WARNs that
   scroll past (`mu-mesh-code-index-not-discoverable-30e5k`); review seats
   SIGTERMed mid-work at our own 900s wall were logged as the model timing out
   (`mu-review-panel-seat-timeout-self-inflicted-wdn45`); and the verdict line
   naming a seat's failure printed an unrelated boot WARN instead of the error
   that actually killed it (`mu-ai-review-seat-failure-misreported-sylcp`).)

   Deliberately NOT gated in `.invariants.toml`: the greppable proxies
   (`let _ =` on a fallible call, a bare `.ok();`, `unwrap_or_default()` on a
   `Result`) are mostly legitimate uses, so a shape would carry a large
   baseline and little signal, and the ratchet is hard to retract. Enforced by
   review.

## How work flows here

- **VCS is `jj`** over a colocated git+jj repo. **`main` is protected and is
  production** → branch + PR for everything. Local commits are ungated; push / PR
  is the reviewed, ask-first step.
- **Force-push and direct push to `main` are disabled for agents —
  intentionally and permanently.** The branch ruleset requires every change to go
  through a PR and its bypass list is empty. Do **not** try to force-push or
  re-grant a bypass to "work around" a rejected push; that guardrail is
  deliberate. The forward path is: a normal forward commit → PR → a human admin
  merges.
- **PR authorship & merging.** Push branches via `bot-jj git push` and open PRs
  via `bot-gh pr create` — the GitHub-App identity — so the operator can APPROVE
  the PR instead of admin-overriding his own authorship. Agents never merge; a
  human does. This applies to `sahuagin/*` repos (where the App is installed; a
  PreToolUse guard denies raw `gh pr create|merge` there). In repos where the
  App cannot be installed (e.g. work orgs without admin), raw `gh` IS the
  sanctioned path — always with `-R owner/repo`, since jj sibling workspaces
  have no `.git`. Same routing lives in `t4c find "create a PR"`.
- **Work is tracked in beads.** The canonical store is the central **beadsd**
  service (rmcp over HTTP), `mu → http://10.1.1.172:7771/mcp` (resolved from
  `~/.config/beads/remotes.env`). Query and mutate it with the `beads --url <u>`
  client, which relays the full `br` surface — `beads list / ready / show /
  count / blocked / dep / …`, plus `claim` / `unclaim`. There is **no in-repo
  `.beads/` mirror** any more (it was untracked + gitignored as stale legacy, and
  `just beads-sync` retired); a bare `br` or a repo `.beads/` read is gone/stale —
  do not rely on it. Claim a bead before editing its code. The `mu-NNN` /
  `mu-<slug>` ids that pepper code comments and spec filenames are the durable
  link from a line back to its rationale.
- **Code search:** prefer semantic code-index recall (`code_recall`) for
  orientation and concept-location when it's configured; fall back to `rg` for
  literal / regex matches.
