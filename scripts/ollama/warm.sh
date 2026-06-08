#!/usr/bin/env bash
# warm.sh — load the review-gate v2 models resident at their BAKED contexts,
# with a long keep-alive, then show residency. Run after create-models.sh (or
# any time the box restarts) to get both models co-resident before a review.
#
# The load requests deliberately send NO num_ctx: each model picks up the
# context baked into its Modelfile (qwen-orch 131072, gpt-oss-rev 49152) — the
# whole point (see create-models.sh / memory a721c14d). Sending num_ctx here
# would re-introduce the eviction this design avoids.
set -euo pipefail

export OLLAMA_HOST="${OLLAMA_HOST:-10.1.1.143:11434}"
base="http://${OLLAMA_HOST}"
keep="${KEEP_ALIVE:-24h}"

load() { # $1 = model
  curl -sf "${base}/api/generate" \
    -d "{\"model\":\"$1\",\"keep_alive\":\"${keep}\"}" >/dev/null \
    && echo "  loaded $1" || { echo "  FAILED to load $1" >&2; return 1; }
}

echo "warming review-gate v2 models on ${OLLAMA_HOST} (keep_alive=${keep}) ..."
load qwen-orch
load gpt-oss-rev

# Residency over the API (no dependency on the ollama CLI being local).
echo "residency:"
curl -sf "${base}/api/ps" \
  | jq -r '.models[] | "  \(.name)  ctx=\(.context_length)  vram=\(.size_vram)"' \
  2>/dev/null || curl -sf "${base}/api/ps"
