# Harness–model fit

**Question that started this:** GLM-5.2 is ~¼–1/10 the cost of Opus 4.8 and can
approach its agentic-coding performance — *but only with an appropriate harness*.
Why does a harness suit one model more than another, and what would `mu` need to
change to be well-suited to all models rather than implicitly Claude-shaped? And —
the founding framing — can the Stanford IRIS **meta-harness** approach let `mu`
*self-improve* toward that fit, with receipts?

**Status:** 2026-06-21 research → **2026-06-22: the on-the-wire knobs landed (the epic's
value is delivered); meta-harness Path A then investigated and NOT pursued.** The
"model-fit on the wire" changes are merged to `main`. Path A (a config-space search
loop) was built toward, reviewed, and closed — see
[meta-harness-assessment.md](meta-harness-assessment.md) §CONCLUSION. The only remaining
(optional) lever is the underutilized `degradation.py` in mu-analytics, not a new loop.

## What landed (2026-06-22)

The seams this research said `mu` "left empty" are now filled — partly by
`mu-vcbm` (Thaddeus's parallel work) and partly by three slices from this epic:

| Knob | How it landed |
|---|---|
| **Reasoning effort** reaches OpenRouter/vLLM | `mu-vcbm` threaded per-turn `effort` through `Provider::stream`; **mu-13ve** (#356) maps it to OpenRouter's `reasoning` field |
| **Tool-call dialect rescue** on the OpenAI wire | **mu-xblz** (#357) applies `rescue_assistant_message` to the OpenRouter/vLLM `Done` path — GLM/Qwen leaks recovered |
| **Per-model sampling** (temperature/top_p) | **mu-y8gp** (#357) adds catalog `temperature`/`top_p`, clamped + finite-guarded, injected at the OpenRouter request |

So the plumbing question this research agonized over is **settled** (per-turn
`effort` threading + catalog-resolved sampling, no trait churn), and GLM/open
models on OpenRouter/vLLM now get effort, dialect recovery, and per-model
sampling. The deeper docs below are the **dated research record**; each carries a
short *Reconciled 2026-06-22* note where its pre-merge claims have since changed.

## Forward direction (resolved 2026-06-22): NOT Path A

Path A (a config-space propose→evaluate→store loop) was investigated and **closed** —
see [meta-harness-assessment.md](meta-harness-assessment.md) §CONCLUSION for the full
reasoning. In short: the Stanford paper is a *method*, not a transferable
deficiency→fix catalog (nothing to harvest); mu already has the full cycle plus a more
substantive analysis layer (`scans` → `degradation.py` → `anomaly_worklist`); and an
automated search doesn't pay off at ~10 curated models, where hand-tuning the landed
knobs suffices.

The remaining, optional lever is the **underutilized `degradation.py`** (mu-analytics):
get its ML probe to surface *unexpected* per-model signals, and feed it richer
message-derived failure-mode features (dialect-leak / stall / tool-error → the knob that
fixes each). That's analytics work on the existing substrate — not a new loop, not the
Stanford repo.

## The one-line answer (unchanged)

A harness's loop-and-tools skeleton barely varies between harnesses. What makes the
*same model weights* swing 8–26 points across harnesses is the set of **adaptation
layers** around the loop: tool-call wire encoding, system-prompt shape,
reasoning-token handling, sampling parameters, context management, tool-menu size,
and error feedback. A model is post-trained against a particular set of those
assumptions; a harness "fits" when its adaptation layers match them.

## Documents

| Doc | What it is |
|---|---|
| [findings.md](findings.md) | The research report: mechanisms of harness–model fit, the GLM specifics, the evidence, and a scorecard of where `mu` stood (now reconciled — the on-the-wire gaps are closed). |
| [model-catalog-tooling.md](model-catalog-tooling.md) | Teardown of `mu models sync` / `catalog_probe` / `model_catalog` (closed loop, manual refresh). Reconciled: the catalog now carries `temperature`/`top_p`. |
| [meta-harness-assessment.md](meta-harness-assessment.md) | Whether the Stanford IRIS [meta-harness](https://github.com/stanford-iris-lab/meta-harness) can help `mu` self-improve. **Verdict (2026-06-22 §CONCLUSION): NO — the paper is a *method*, not a transferable deficiency→fix catalog; mu's analytics are already more substantive; Path A investigated and not pursued.** |
| [implementation-plan.md](implementation-plan.md) | The sequenced plan. Reconciled: the effort / rescue / sampling phases are **done**; remaining = prompt addendum, `discover()`→`t4c` parity, and the meta-harness eval (Path A). |

See also the operator reference [`docs/mu-solo-runbook.md`](../../docs/mu-solo-runbook.md).

## Open questions / next

- ~~Stand up meta-harness Path A~~ — **investigated and dropped 2026-06-22** (see
  "Forward direction" above + the assessment §CONCLUSION). The optional lever instead:
  get the underutilized `degradation.py` to surface *unexpected* per-model signals.
- Remaining hand-knobs: per-model **system-prompt addendum** (modest now that
  dialect rescue landed) and **`discover()` → `t4c` parity** (the tool-surface lever).
- Live GLM-5.2 numbers *through mu* now that the knobs exist — read them from the event
  log / mu-analytics on real runs (no dedicated eval loop needed).
- Whether to consume probed **pricing** (written by `mu models sync` but inert) —
  needed for *score-per-dollar* in the Path A eval.
