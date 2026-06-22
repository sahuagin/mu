# AGENTS.md — meta-harness (Path A)

Operating procedure + load-bearing nuances for the **meta-harness "Path A"**: a
config-space outer loop that self-improves mu's per-model harness fit by
**searching harness *profiles*** and selecting by a **numeric score from the
event log**. This file exists so the hard-won details below aren't rediscovered.

- **Design / rationale:** [`research/harness-model-fit/`](../../research/harness-model-fit/)
  (README headlines Path A; `meta-harness-assessment.md` = the Stanford-IRIS frame).
- **Status (2026-06-22):** the model-fit *knobs* are landed (effort→reasoning,
  tool-dialect rescue, per-model sampling, per-model prompt-addendum); the
  *propose→converge engine* exists ([`scripts/orchestrator/`](../orchestrator/));
  the eval engine exists (`agentic-bench`). **This loop is the remaining glue.**

## What a "profile" is (the search-space unit)

A profile is the model-fit knobs, set in **two** places — don't look for them in one:

| Knob | Set via |
|---|---|
| provider, model | dispatch args |
| effort | `THINKING` (agent-dispatch caller scope) |
| tool set | `TOOLS` (agent-dispatch caller scope) |
| system-prompt addendum | `SYSPROMPT` file (dispatch) **or** catalog `system_prompt_addendum` |
| temperature / top_p | **catalog only** (`~/.config/mu/models.toml`; mu reads it per model) |
| dialect-rescue | always-on for OpenRouter/vLLM (no knob) |

## Reuse map — DO NOT rebuild these

- **Worker** (run one model on a prompt, hermetically): [`../lib/agent-dispatch.sh`](../lib/agent-dispatch.sh).
  `agent_dispatch <provider> <model> [<prompt-file>]` → stdout; reads
  `TOOLS / SYSPROMPT / THINKING / MAX_TURNS / TIMEOUT / ERRLOG` from caller scope.
  Routes `claude-oauth`→`claude -p`, everything else→`mu ask --bare`. **Source it.**
- **Eval engine** (task corpus + grading): **`agentic-bench`**
  (`~/src/public_github/agentic-bench`, GitHub) — drives `mu ask` on **tool-gated**
  cases, grades `correct / leak / fabricated`. The `arch_*` variant adds a **cost
  report**, multi-task grading (agentic / coding-pytest-or-judge / review), a
  cross-family LLM judge, and **collect-separated-from-grade** (re-score without
  re-spending).
- **Cost → score-per-dollar:** the mu **event log** via **mu-analytics**
  (`~/src/public_github/mu-analytics` — DuckDB over both fleets' JSONL; already
  carries cost).
- **Reproducibility shape** (copy the *shape*, not the stages): `orchestrate.sh`
  uses `RUN_DIR` + per-call `<label>.out`/`.err` + a `provenance.jsonl` line
  `{label,provider,model,exit}` per call + `summary.md`. Copy that discipline.

## The loop — the ONLY new code

Fan out N profiles → run `agentic-bench` per profile → read **pass-rate-per-dollar**
from the event log → **select = deterministic `argmax(score)`**.

- **NOT the LLM converger, NOT ci-aipr.** Those grade *unquantifiable code diffs*.
  A numeric objective → `argmax` is strictly better and free (per `cc:a92625d4`,
  2026-06-22). The orchestrator's CONVERGE/REVIEW stages are **diff-shaped and not
  pluggable** on worker-product or scorer — so this is a **sibling loop** that
  reuses `agent-dispatch.sh`, not a fork of `orchestrate.sh`.
- **Future convergence:** generalize `orchestrate.sh` to parameterize
  `{worker-product-extractor, selection-strategy: llm-judge | numeric-argmax}` so
  profile-search and the code-diff pipeline share one engine — **only after** this
  score-select path proves out.

## Methodology (from the seat-ab A/B — `~/.claude/experiments/seat-ab-2026-06-22-*.md`)

- **Pre-register** the design + hypothesis to disk *before* the first model call.
- **Trust the dispatch authority, never model self-report.** Served model = whatever
  `--provider/--model` says. Models lie about identity (qwen self-ID'd "claude";
  gpt-5.5 said "gpt-5-mini"). Read which-model / which-profile from your own records.
- **Controlled:** vary ONE thing (the profile); hold model + task corpus + dispatch
  byte-identical across arms.
- **Collect ≠ grade:** grading is cheap, deterministic, re-runnable — don't re-spend
  tokens to re-score.

## Gotchas (the expensive-to-rediscover ones)

- **ollama reloads the model when sampling params hit the wire** — big models cold-start
  13+ minutes. So per-model `temperature`/`top_p` in the catalog is **OpenRouter/vLLM
  only**; for ollama, bake sampling into the Modelfile. agentic-bench **evicts** between
  model groups for clean timings — preserve that.
- **Hermetic dispatch is load-bearing.** `mu ask --bare` /
  `claude --exclude-dynamic-system-prompt-sections` strip the agent scaffolding so the
  model sees only the prompt (+ `SYSPROMPT`). Without it a `CLAUDE.md` kernel makes
  every model self-identify as "claude" and corrupts the experiment.
- **Cost:** `agentic-bench/run_benchmark.py` records wall-time only; the `arch_*` path
  emits a cost report. Use `arch`, or pull cost from the event log via mu-analytics.

## Pointers

- Sibling (code-diff) pipeline: [`scripts/orchestrator/`](../orchestrator/) — `orchestrate.sh`, prompts.
- Shared dispatch: [`scripts/lib/agent-dispatch.sh`](../lib/agent-dispatch.sh).
- Eval: `~/src/public_github/agentic-bench` · Scoring: `~/src/public_github/mu-analytics`.
- The frame: Stanford IRIS meta-harness — <https://github.com/stanford-iris-lab/meta-harness>.
