# Detecting agent failures by reading transcripts: a semantic behavior-judge

A method and toolkit for detecting when an AI coding agent has *misbehaved* — not by
matching strings or watching metrics, but by having a separate LLM read the session
transcript for meaning. This write-up is the generic methodology; the detectors are in
[`scripts/`](scripts/) and the judge in [`judge/`](judge/). Prior art is surveyed in
[`RELATED-WORK.md`](RELATED-WORK.md).

## The question

People try to make coding agents behave with two tools: **hooks** (programmatic gates —
"edit attempted before the file was read: blocked") and **directives** (CLAUDE.md / AGENTS.md
prose). A natural first question is whether either reliably *changes* behavior. Answering it
requires first being able to **detect** the bad behavior in the first place. That detection
problem turned out to be the hard and interesting part.

## Finding 1 — the cheap signals are dead ends

Two obvious detectors don't work:

- **Syntactic anti-patterns** ([`scripts/behavior_rates.py`](scripts/behavior_rates.py)).
  Tool-stream regexes for things like heredoc-instead-of-write, shell file-writes, dangerous
  bash, force-push, edit-loop, edit-before-read are easy to compute and genuinely vary across
  setups — but they capture only the *mechanical* mistakes, not the substance of what went
  wrong.
- **Frustration-keyword rates** ([`scripts/frustration_lift.py`](scripts/frustration_lift.py)).
  Counting operator-frustration markers ("stop", "again?", "that's not what…") per message is
  the obvious outcome proxy, and inferring agent/user state from such signals is established
  practice (see RELATED-WORK §e). But the **rate is confounded**: marker density is
  non-monotonic in session length and is dominated by short, hot exchanges, so naive
  correlations with any behavior flip sign once you control for size. It measures the wrong
  thing. (We include the script precisely to demonstrate the confound, not to recommend it.)

## Finding 2 — the failures that matter are *semantic*

When you look at what actually goes wrong in real sessions — the episodes a human reviewer
flags — the large majority are **semantic**, not syntactic: the *same* tool calls are fine or
not depending on what they assert and why. They have **no syntactic tell** and, separately,
**no telemetry fingerprint** (a supervised model predicting operator sentiment from session
telemetry features lands at R² ≈ 0 — there is simply no metric signature). The recurring root
cause is the same one practitioners report for prose rules: **advisory-in-context does not
enforce** (see RELATED-WORK §c/§e — "rules are requests, hooks are laws").

We distilled the recurring semantic failures into five classes (full rubric in
[`judge/rubric.md`](judge/rubric.md)):

| class | one-line |
|---|---|
| `false_success` | claims work done/verified that the transcript doesn't support |
| `map_as_terrain` | trusts a label / memory / prior claim as ground truth without checking |
| `scope_overreach` | exceeds the instruction's blast radius |
| `relitigation` | re-derives a question already settled |
| `dismissiveness` | refutes-don't-engage / strawman / preference-substitution / over-explains |

## The behavior-judge

If the failures are semantic, the detector has to read for meaning. So:

- An LLM **judge** is given a turn-numbered rendering of one session
  ([`scripts/render_transcript.py`](scripts/render_transcript.py)) and **one** behavior class,
  and returns `{occurred, severity, confidence, evidence[]}` — where every `evidence` item must
  **quote a verbatim span and cite its turn** (fabricated evidence disqualifies the verdict).
  Prompt template: [`judge/behavior-judge-system-prompt.txt`](judge/behavior-judge-system-prompt.txt);
  per-class fills: [`judge/rubric.md`](judge/rubric.md); runner:
  [`scripts/run_judge.py`](scripts/run_judge.py) (one call per (transcript, class)) and
  [`scripts/run_judge_batch.py`](scripts/run_judge_batch.py) (batch, robust, append-per-result).
- Default to **`occurred=false`** under uncertainty, but always report what was inspected —
  this and the per-class **EXCLUSIONS** (look-alikes that are *not* the behavior, e.g. correct
  pushback is not dismissiveness) are what hold down false positives.

## Validation methodology + result

We validated against a held-out set: a **positive set** of sessions independently confirmed to
contain a flagged behavior (recall = fraction the judge flags `occurred=true`), and a
**stratified "presumed-clean" negative sample** (false-positive rate = fraction it flags). On
the harness the judge's rubric was tuned for, it reached **100% recall and 0% false positives**
on this set.

Three methodological lessons, all of which generalize:

1. **"Presumed-clean" is leaky.** A negative set built from "no reviewer mark" is *necessary,
   not sufficient* — absence of a mark ≠ absence of a behavior. On inspection, most of our
   apparent false positives were **real behaviors the reviewer simply hadn't recorded**. The
   judge's precision was better than the raw number implied — and this is a strong argument for
   **low-friction in-the-moment marking** so real episodes don't leak into "clean" sets.
2. **The judge is non-deterministic.** Run at a normal sampling temperature (temperature 0
   degenerates qwen3-family models), verdicts near the decision boundary can flip between runs.
   Single-sample recall/FP are point estimates with noise — **majority-vote (N≥3)** before
   reporting a rate, or fix a seed at the recommended temperature for reproducibility.
3. **Cross-harness calibration doesn't transfer for free.** A rubric tuned on one transcript
   format/agent-style can mis-fire on another (different rendering, different conversational
   dynamics). Calibrate per harness; don't assume.

## Using the toolkit

1. Point `behavior_rates.py` / `frustration_lift.py` at a corpus to see the (limited) syntactic
   + frustration baselines.
2. Build a positive set (sessions you *know* exhibit a class) and a negative sample.
3. Run `run_judge_batch.py` over both; compute recall + FP; majority-vote for stability.
4. Calibrate the rubric `EXCLUSIONS` against your false positives; recheck.

The judge labels (`{session_ref, behavior, occurred, evidence}`) are also a natural **supervised
target** for downstream analysis — e.g. "does any session-telemetry feature predict a given
semantic failure?" (in our data: no — which is itself the finding that you must read the
transcript, not the metrics).

## Scope / what this is not

This repository is the **generic methodology + detectors + a survey of public prior art**. It
deliberately contains **no session logs, no transcripts, no operator-specific data, and no
defensive-tooling configuration** — see [`AGENTS.md`](AGENTS.md) for the publishing rules this
project follows.
