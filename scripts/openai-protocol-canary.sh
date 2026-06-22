#!/usr/bin/env bash
# openai-protocol-canary.sh — protocol-drift canary for the typed `mu-openai`
# crate, the OpenAI sibling of the Anthropic canary.
#
# What it does (the Anthropic canary's shape): replay a corpus of captured
# OpenAI Responses-API messages through the typed model and NOTIFY if any
# deviates from what we model — i.e. a field OpenAI added/renamed that our types
# silently drop on round-trip. Plus the crate's own tests and a spec sanity
# check. Designed for cron; logs to a file and (optionally) files a bead.
#
# Usage:
#   openai-protocol-canary.sh [--live] [--alert=bead]
#
# Corpus: the crate's checked-in fixtures always, plus every *.json under
# $MU_OPENAI_CANARY_CORPUS (a dir of captured real responses/streams) if set.
#
# Exit: 0 = clean, non-zero = drift / failure (also logged + alerted).

set -euo pipefail

# Cron-robust PATH.
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin:$PATH"

alert="none"
live=0
for arg in "$@"; do
  case "$arg" in
    --live) live=1 ;;
    --alert=*) alert="${arg#--alert=}" ;;
  esac
done

repo="${MU_REPO:-$(cd "$(dirname "$0")/.." && pwd)}"
crate="$repo/crates/providers/mu-openai"
spec="$crate/specifications/openapi.yaml.xz"
log="${MU_OPENAI_CANARY_LOG:-$HOME/.local/share/openai-protocol-canary.log}"
log_prefix="[openai-protocol-canary]"
mkdir -p "$(dirname "$log")"

say() { echo "$log_prefix $*" | tee -a "$log"; }

failures=()
run_check() {
  local name="$1"; shift
  say "running $name"
  if ! "$@" >>"$log" 2>&1; then failures+=("$name"); say "FAIL $name"; fi
}

say "=== run $(date) on $(hostname); repo=$repo ==="

# (1) THE drift signal: replay every captured message through drift_check.
#     Exit 3 from the example means a modeled type dropped/changed a field.
corpus=()
for f in "$crate"/tests/fixtures/*.json; do [ -e "$f" ] && corpus+=("$f"); done
if [ -n "${MU_OPENAI_CANARY_CORPUS:-}" ] && [ -d "$MU_OPENAI_CANARY_CORPUS" ]; then
  while IFS= read -r f; do corpus+=("$f"); done \
    < <(find "$MU_OPENAI_CANARY_CORPUS" -name '*.json' -type f)
fi
say "replaying ${#corpus[@]} captured message file(s) through drift_check"
run_check drift_replay cargo run --quiet --manifest-path "$crate/Cargo.toml" \
  --example openai_drift_check -- "${corpus[@]}"

# (2) The typed model's own tests (round-trip + spec-exact + golden fixtures).
run_check mu_openai_unit cargo test --quiet --manifest-path "$crate/Cargo.toml"

# (3) Spec sanity: the vendored snapshot still has the surface we model.
run_check spec_responses_path xzgrep -q '^  /responses:' "$spec"
run_check spec_event_name \
  xzgrep -q 'response.function_call_arguments.delta' "$spec"

# (4) Optional live checks against the real backends (gated).
if [ "$live" = 1 ]; then
  if [ -z "${OPENAI_API_KEY:-}" ] && command -v tq >/dev/null 2>&1; then
    OPENAI_API_KEY="$(tq -f "$HOME/.config/agent/config.toml" -r openai.api_key 2>/dev/null || true)"
    export OPENAI_API_KEY
  fi
  if [ -n "${OPENAI_API_KEY:-}" ] && [ "${OPENAI_API_KEY}" != "null" ]; then
    run_check live_public_openai env MU_LIVE_OPENAI_API=1 \
      cargo test --quiet -p mu-ai live_public_api
  else
    say "OPENAI_API_KEY unavailable; skipping public live checks"
  fi
  if [ -f "$HOME/.config/mu/auth/openai-codex.json" ]; then
    run_check live_codex env MU_LIVE_OPENAI_CODEX=1 \
      cargo test --quiet -p mu-ai live_codex
  else
    say "openai-codex token unavailable; skipping Codex live checks"
  fi
fi

if [ "${#failures[@]}" -eq 0 ]; then
  say "ok"
  exit 0
fi

msg="OpenAI protocol canary failed: ${failures[*]}"
say "$msg"

if [ "$alert" = "bead" ] && command -v beads >/dev/null 2>&1; then
  url="${BEADS_REMOTE:-$(awk -F= '/^mu=/{print $2}' "$HOME/.config/beads/remotes.env" 2>/dev/null || true)}"
  if [ -n "$url" ]; then
    beads --url "$url" exec -- create \
      --title "$msg" \
      --slug openai-protocol-canary-drift \
      --type bug --priority P1 \
      --description "OpenAI protocol canary on $(hostname) detected: ${failures[*]}. See $log." \
      --actor openai-protocol-canary >/dev/null 2>&1 || true
  fi
fi

exit 1
