#!/usr/bin/env bash
# create-models.sh — (re)create the review-gate v2 ollama models from their
# Modelfiles, idempotently. Derived models share their base weights (this only
# writes a manifest + the baked PARAMETERs — no re-download).
#
# Why these models exist: mu talks to ollama over the Anthropic Messages wire
# (bead mu-fmas), which carries no num_ctx. Per-model context therefore has to
# be BAKED into the artifact, or ollama reloads at the server default and evicts
# the co-resident model. Full finding + proof:
#   memory ollama-anthropic-wire-numctx-modelfile-fix-2026-06-08 (a721c14d).
#
# Targets the ollama box by default; override with OLLAMA_HOST.
set -euo pipefail

export OLLAMA_HOST="${OLLAMA_HOST:-10.1.1.143:11434}"
here="$(cd "$(dirname "$0")" && pwd)"

echo "creating review-gate models on ${OLLAMA_HOST} ..."
# qwen-rev: the SOLE local primary of the flat ai-review panel (mu-f0ls) — full
# qwen3.6:35b-a3b-q8_0 @ num_ctx 262144 (43GB/100% GPU on 48GB), temp 0.6.
ollama create qwen-rev    -f "$here/qwen-rev.Modelfile"
# qwen-orch + gpt-oss-rev: the tiered review-gate v2 pair (orchestrator + per-file
# worker, bead mu-u1it) — kept for that future shape, not used by the flat panel.
ollama create qwen-orch   -f "$here/qwen-orch.Modelfile"
ollama create gpt-oss-rev -f "$here/gpt-oss-rev.Modelfile"

echo "done: qwen-rev (num_ctx 262144, temp 0.6) + qwen-orch (131072) + gpt-oss-rev (49152)"
echo "load them resident with: $here/warm.sh"
