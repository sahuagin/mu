# Stanford IRIS Meta-Harness: can `mu` use it to self-improve?

*Assessment, 2026-06-21. Facts gathered by a web-research agent from the repo,
ONBOARDING.md, the paper, and the Terminal-Bench reference source; mu-fit synthesis
is mine. Sources inline.*

- Repo: <https://github.com/stanford-iris-lab/meta-harness> (MIT, ~1.1k stars, 11
  commits, research prototype — "not tested beyond verifying that it runs").
- Paper: <https://arxiv.org/abs/2603.28052> · Site: <https://yoonholee.com/meta-harness/>

## CONCLUSION (2026-06-22) — Path A NOT pursued; mu already has the better system

Acted on, then closed. After building toward Path A (the profile-search loop, PR #378)
**and reading the actual paper**, the "build Path A" recommendation in this doc is
**superseded — do not rebuild it.** Recorded here so a future pass short-circuits:

1. **The paper is a METHOD, not a transferable catalog.** Read 2026-06-22
   (arxiv 2603.28052): Meta-Harness is an outer loop that *searches over harness code*
   with an agentic proposer (text-classification / math / agentic-coding). Fixes are
   *discovered by the search, per-domain* — there is **no enumerated "deficiency →
   prescribed fix" catalog** to harvest. The reusable idea is "iterate the harness
   using metrics," which mu already does.
2. **mu already has the more substantive system.** The full cycle (run → event log →
   mu-analytics) + the analysis layer — `scans.py` (message-signal markers) →
   `degradation.py` (ML probe: telemetry → signed good/bad sentiment, with
   model/provider features + unnoticed-degraded detection) → `anomaly_worklist.py`
   (IsolationForest outliers) — is richer than what the paper assumes a target harness
   provides. mu *is* the rich-trace substrate; it doesn't need their loop.
3. **The automation doesn't pay off at our scale.** A propose→evaluate→store loop earns
   its cost only with too many model×config combos to hand-tune. At ~10 curated models
   in rotation, hand-tuning the landed knobs (effort / rescue / sampling / addendum)
   suffices — and those knobs are merged.
4. **There was never a runner to build.** "Run a config then read its log" is just
   generic `mu ask` launching + the event log; no benchmark/runner needed. The #378
   loop re-implemented arch-bench and re-searched *models* (a non-problem — the ~10 in
   rotation are given) and is closed.

**The one real, optional lever** is the *underutilized* `degradation.py`: get it to
surface *unexpected* per-model signals (the point of ML over ripgrep) and feed it
richer message-derived failure-mode features (dialect-leak / stall / tool-error → the
knob that fixes each). That's mu-analytics work, not a Stanford thing. The valuable
harness-model-fit output — the on-the-wire knobs — is already landed.

*(The 2026-06-21 assessment below — "strong fit, build Path A" — is kept as the dated
research record; the CONCLUSION above supersedes its recommendation.)*

## Verdict up front

**Strong conceptual fit, partial mechanical fit.** Meta-harness's central bet is
that an agentic *proposer* improves a harness fastest when it can read **full
execution traces** of prior candidates — source, scores, and *why each run
failed* — rather than a compressed scalar score (~10M tokens of diagnostic context
per step vs. ~26K for prior text-optimizers, per the site). **`mu` is
event-sourced; it natively emits exactly that trace substrate.** Most harnesses
would have to *add* rich trace logging to be optimized this way — `mu` already has
receipts. That is an unusually good alignment.

The friction is mechanical: meta-harness searches over **Python source** (the
candidate harness is a Python class behind an import path), and `mu` is Rust. So the
right move is **not** to adopt the repo wholesale and let it rewrite `mu`'s loop.
It's to **borrow the pattern** (propose → evaluate → store, with a trace-reading
proposer and a greedy frontier) and point it at `mu`'s **configuration space** —
the very knobs the [implementation plan](implementation-plan.md) makes exist.

## What meta-harness actually is

An **outer-loop search over harness code** for LLM applications, base model frozen.
The loop (from the Terminal-Bench reference `meta_harness.py`):

1. **Propose** — an agentic proposer (Claude Code, `model=opus`, `effort=max`) with
   filesystem access to *all prior candidates' source + scores + traces* writes a new
   candidate harness.
2. **Validate** — import check + 1-task smoke test.
3. **Benchmark** — run the candidate on the task set (Harbor + `terminal-bench@2.0`
   in the reference), parse per-task rewards into a pass rate.
4. **Update frontier** — greedy: keep the best per task and overall; append every
   candidate to `evolution_summary.jsonl`.

It optimizes **scaffolding code**: prompt templates, output parsers, the agent loop,
tool-use strategy, retrieval/memory — whole Python modules, not config knobs.
Results: Opus 4.6 → **76.4%** Terminal-Bench-2 (#2 overall, vs. 74.7% baseline);
Haiku 4.5 → 37.6% (#1 for its tier). Compute- and token-heavy; needs `ANTHROPIC_API_KEY`,
a sandbox provider (Runloop) with quota, and many multi-trial evaluations.

## The integration contract (what a target harness must provide)

From ONBOARDING.md + the reference, a harness to be driven by meta-harness needs:

1. **A Python entry point to the harness it can edit** (candidate = `module:ClassName`).
   A Rust runtime needs a Python shim that *is* the editable harness even if it
   ultimately invokes the Rust binary.
2. **A test-checkable harness interface** (a base class/API shape) + import-validity
   + smoke-test path — *you* design this per domain.
3. **A headless eval runner: candidate → numeric score**, with a search-set /
   held-out-test split and trace capture.
4. **Rich per-candidate filesystem traces** — the load-bearing input; a bare scalar
   defeats the method's main advantage.
5. **A frozen base model + swappable proposer** (defaults to the Claude Code CLI via
   `claude_wrapper.py`) + the API-key and sandbox budget to run it.

## How this maps onto `mu` — two paths

### Path A — config-space outer loop (recommended; cheap, high-fit)

Don't let it rewrite Rust. Expose `mu`'s **model-fit configuration surface** as the
search space and search over *that*:

- **Search space = a `mu` harness profile** (declarative): per-model sampling
  (`temperature`/`top_p`/`min_p`), tool dialect / rescue on/off, `quirks`, the
  system-prompt addendum, tool-menu (`FavoriteConfig.tools`), `max_turns`,
  compaction policy/threshold, reasoning-request policy. **These are exactly the
  knobs the [implementation plan](implementation-plan.md) creates.** The plan makes
  them config-driven; this loop tunes them.
- **Proposer** = `mu ask` / `claude` / `mu serve` — reuse mu's own model access; no
  new dependency on the repo's `claude_wrapper.py`.
- **Eval runner** = a thin harness that runs `mu ask`/`mu serve` headlessly over a
  task corpus and grades it. `mu` already has grader concepts in the goal-protocol /
  autonomy machinery to reuse, and OS-enforced sandboxing (per `specs/architecture/`)
  for safe parallel candidate runs.
- **Traces** = **the event log, for free.** `ContextAssembly`, `CompactionAssembly`,
  tool calls, usage, provider-switch — the proposer reads JSONL receipts and can
  "trace a failure back to the specific harness decision," which is precisely the
  mechanism the paper credits. This is mu's structural advantage.
- **Frontier / store** = greedy-best-per-model in a small index — and because mu
  records cost per run, the objective can be *score-per-dollar*, not just score,
  which directly serves the GLM-vs-Opus economics question.

This is a "meta-harness over mu's config space." It's how you'd *automatically*
answer "which harness profile fits GLM-5.2" instead of hand-tuning it — and the
answer arrives with receipts. It does **not** require the meta-harness repo at
runtime; it borrows its loop shape.

### Path B — code-space outer loop (defer)

Let it edit Rust strategy modules behind a stable trait (a `HarnessProfile` /
pluggable-strategy seam) and recompile per candidate. Possible, but Rust compile
times + the Python candidate interface make per-candidate iteration slow and the
integration heavy. Not an early fit; revisit only if config-space search plateaus and
the wins are clearly in code the config can't reach.

## Recommendation

1. **Land the [implementation plan](implementation-plan.md) first.** It is the
   prerequisite either way: there is nothing to search over until the knobs exist and
   are config-driven. Its Phase 4 (a measured harness-fit eval reading the event log)
   is *already the skeleton* of Path A's eval runner — build it with that second life
   in mind.
2. **Then build Path A as a `mu`-native loop**, borrowing meta-harness's
   propose→evaluate→store + trace-reading-proposer + greedy-frontier pattern. Treat
   the repo as a **design reference and a credibility anchor** (MIT-licensed, cite it),
   not a runtime dependency — it's a research prototype and the Python/Rust seam isn't
   worth carrying.
3. **Keep Path B on the shelf.** Only worth it if config-space tuning demonstrably
   leaves wins in the Rust loop itself.

The honest framing: meta-harness validated, with real numbers, the idea `mu`'s
architecture was already betting on — that *legible, replayable traces* are what let
you improve a harness. `mu` doesn't need their code so much as it can *be the
substrate their method assumes*. The cheapest, highest-fit version is a config-space
search whose trace channel is the event log we already write.

## Provenance / caveats

Facts from the web agent's reads of the repo README, ONBOARDING.md (summarized, not
quoted verbatim), the arXiv abstract, and the Terminal-Bench reference source
(`meta_harness.py`, `run_eval.sh`, `agents/baseline_kira.py`, `claude_wrapper.py`,
`SETUP.md`). Unverified: exact last-commit date; the precise per-method signatures a
non-Terminal-Bench harness must implement (domain-defined — the repo gives the
onboarding questions and one worked example, not a universal interface). Before
committing real effort to Path A, skim ONBOARDING.md and `domain_spec.md` directly to
confirm the eval-runner shape.
