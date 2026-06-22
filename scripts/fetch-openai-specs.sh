#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
out="$root/crates/providers/mu-openai/specifications"
mkdir -p "$out"
curl -fsSL https://raw.githubusercontent.com/openai/openai-openapi/master/openapi.yaml -o "$out/openapi.yaml"
cat > "$out/MANIFEST.md" <<'MANIFEST'
# OpenAI protocol source material

- `openapi.yaml`: fetched from `https://raw.githubusercontent.com/openai/openai-openapi/master/openapi.yaml`.
- Public docs pages are available under `https://platform.openai.com/docs/api-reference`; the canonical machine-readable schema is the OpenAPI file above.

Refresh with `scripts/fetch-openai-specs.sh`.
MANIFEST
