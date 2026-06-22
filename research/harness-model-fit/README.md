# Harness–model fit

**Question that started this:** GLM-5.2 is ~¼–1/10 the cost of Opus 4.8 and can
approach its agentic-coding performance — *but only with an appropriate harness*.
Why does a harness suit one model more than another, and what would `mu` need to
change to be well-suited to all models rather than implicitly Claude-shaped? And —
the founding framing — can the Stanford IRIS **meta-harness** approach let `mu`
*self-improve* toward that fit, with receipts?

**Status:** 2026-06-21 research → **2026-06-22: the on-the-wire knobs landed.** The
three "model-fit on the wire" changes are merged to `main`; the forward direction
is now the **meta-harness Path A** — config-space self-improvement over those knobs.

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

## The forward direction: meta-harness Path A

With the knobs now real, they **are** the search space a Stanford-IRIS-style outer
loop would optimize. The next lever isn't another hand-tuned knob — it's letting
`mu` *search* for the best per-model harness profile, scored from its own event
log. See [meta-harness-assessment.md](meta-harness-assessment.md) §"Path A": a
config-space loop hosted in **mu-analytics** (Python over the event-log JSONL,
which already reads both fleets), proposing profile changes, running `mu` headless,
and scoring per-model — *score-per-dollar*, since `mu` logs cost. That closes the
loop back to the founding question of using the IRIS frame to self-improve `mu`.

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
| [meta-harness-assessment.md](meta-harness-assessment.md) | Whether the Stanford IRIS [meta-harness](https://github.com/stanford-iris-lab/meta-harness) can help `mu` self-improve. Verdict: strong conceptual fit (mu's event log *is* the rich-trace substrate); build a mu-native config-space loop (Path A). **The forward direction.** |
| [implementation-plan.md](implementation-plan.md) | The sequenced plan. Reconciled: the effort / rescue / sampling phases are **done**; remaining = prompt addendum, `discover()`→`t4c` parity, and the meta-harness eval (Path A). |

See also the operator reference [`docs/mu-solo-runbook.md`](../../docs/mu-solo-runbook.md).

## Open questions / next

- **Stand up meta-harness Path A** (the headline) — the eval harness in mu-analytics
  + a proposer loop over the landed knobs. Needs scope sign-off.
- Remaining hand-knobs: per-model **system-prompt addendum** (modest now that
  dialect rescue landed) and **`discover()` → `t4c` parity** (the tool-surface lever).
- Live GLM-5.2 numbers *through mu* now that the knobs exist (the Path A eval
  generates them with receipts).
- Whether to consume probed **pricing** (written by `mu models sync` but inert) —
  needed for *score-per-dollar* in the Path A eval.
