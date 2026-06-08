# Compaction-fidelity probe harness (mu-0fla, Layer 2)

Measures compaction fidelity **behaviorally**: a downstream model is given
a compacted rope as context and asked probe questions whose answers live
in specific spans; an LLM judge scores each answer against a hand-written
gold answer. The headline metric is **correctness conditioned on the
target span's fate** under each policy — which ties this (Layer 2) back to
the structural fates from `crates/mu-core/src/context/compaction/fidelity.rs`
(Layer 1).

## Layers

- **Layer 1 — structural retention** (Rust, free, deterministic):
  `compaction/fidelity.rs` + the `compaction-bench` example. *What* each
  policy kept (kept/summarized/dropped, per-kind, recency curve).
- **Layer 2 — behavioral probe eval** (this harness): *does it matter*
  that a span was dropped — can the model still answer?

## Run it

```sh
# 1. Emit fixtures (compacted rope CONTENT) for ONE session at a target.
cargo run --release --example compaction-fixtures -p mu-ai -- \
    ~/.local/share/mu/events/<daemon>/<session>.jsonl 3000 > /tmp/fixtures.json

# 2. Probe them. Two ways to set the A/B axis:

#   (a) settings PROFILES on one base model (recommended). Every sampling
#       parameter is a per-request ollama option, so a settings A/B needs
#       NO Modelfile variants — just profiles. --judge-model grades.
python3 scripts/compaction_probe.py \
    --fixtures /tmp/fixtures.json \
    --probes scripts/compaction_probes/<session>.json \
    --runs scripts/compaction_probes/runs-qwen3.6-sampling-ab.json \
    --judge-model qwen3.6-det:latest \
    --out /tmp/probe-results.json

#   (b) one run per named model, each using its OWN Modelfile defaults:
python3 scripts/compaction_probe.py --fixtures /tmp/fixtures.json \
    --probes scripts/compaction_probes/<session>.json \
    --models qwen3.6-det:latest --judge-model qwen3.6-det:latest
```

**Settings are per-request.** Anything a Modelfile bakes with `PARAMETER`
(temperature, seed, top_p, top_k, min_p, presence_penalty, repeat_penalty,
num_predict, num_ctx, …) can be sent in the ollama `options` field — see
`runs-qwen3.6-sampling-ab.json` for the coding-temp (0.6) vs general-temp
(1.0) A/B as two profiles on one base model, no variants — both using
Unsloth's authoritative Qwen3.6 thinking-mode values (memory 41771769). A
profile inherits the base model's *other* defaults, so set the full set
you care about. `num_predict` caps a runaway generation cheaply.

Determinism (per memory `6679bf86`): reproducibility comes from a **fixed
seed**, not temperature. `--models` runs do **not** send
temperature/seed/num_ctx unless you pass them (no silent temperature
override). A `num_ctx` different from the loaded instance forces a
**reload**, so hold it constant across profiles. Run the ollama server
with `OLLAMA_NUM_PARALLEL=1` for serial, batch-deterministic decoding.

> **Finding (2026-06-07):** `qwen3.6` at **temperature 0** can degenerate
> into a thinking-loop on some probes (one generation ran 12+ min to the
> timeout, non-reproducibly). The `code` profile (temp 0.6 + seed) avoids
> this; `num_predict` bounds it regardless. So temp-0 "determinism" trades
> against loop-robustness — a real input to picking the test profile.

## Probe sets

Each `<session>.json` is hand-authored for a specific `(session_id,
target_tokens)` — span survival depends on the target, so a probe set is
not portable across targets. Schema:

```json
{ "session_id": "...", "target_tokens": 3000, "probes": [
  { "id": "...", "question": "...", "gold_answer": "...",
    "target_span_id": "<span whose content answers it>", "note": "..." } ] }
```

`session-8c78230c-explore.json` — an agent exploring the mu workspace;
6 concrete single-fact probes (Cargo workspace members, what mu is, the
`mu serve` subcommand, mu-core/mu-ai crate roles, JSON-RPC version). Each
target span is dropped by `span-family-drop` and summarized by the mock
judge at target 3000, but kept under `no-compaction` (the control).

## Reading the output

Two tables: `correct/n` per `(model | policy)`, and per
`(model | policy | target-span fate)`. The control row
`<model> | no-compaction | kept` should be ~1.0 — if it isn't, the probes
or the judge are broken, not the policy.

## Known confounds

- **Prior knowledge.** If the session is about a *public* artifact (mu is
  on GitHub), the model may answer a dropped-span probe from training, not
  context, inflating a policy's score. For a rigorous eval, prefer
  sessions whose facts aren't guessable (private code, specific run
  values). The "answer ONLY from context" instruction mitigates but does
  not eliminate this.
- **Small n.** A 6-probe set validates the *mechanism*; policy verdicts
  need more probes across `deep-recent` / `needs-old-history` / `mixed`
  session types (see the test-design doc).
- **Judge ≈ answerer.** Using one local model as both is expedient but
  incestuous; a stronger/neutral judge is better when available.
