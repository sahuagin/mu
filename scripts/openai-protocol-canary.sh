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
# One corpus dir for BOTH sides: live capture writes here and offline replay
# reads here — including the DEFAULT location, so default-mode captures are
# replayed on later runs (panel finding: the earlier form only read the dir
# when the env var was explicitly set, orphaning default captures).
corpus_dir="${MU_OPENAI_CANARY_CORPUS:-$HOME/.local/share/openai-canary-corpus}"
corpus=()
for f in "$crate"/tests/fixtures/*.json; do [ -e "$f" ] && corpus+=("$f"); done
if [ -d "$corpus_dir" ]; then
  while IFS= read -r f; do corpus+=("$f"); done \
    < <(find "$corpus_dir" -name '*.json' -type f)
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
    # Live public check WITH corpus capture. (The previous form ran
    # `cargo test -p mu-ai live_public_api`, which matches ZERO tests and
    # passed vacuously — mu-canary-hardening-1ljgx.) One minimal real
    # Responses request, tool included so the capture exercises the
    # function-call surface; the response must parse drift-clean, and is
    # kept in a rolling corpus so every later offline run replays CURRENT
    # wire shapes, not launch-week fixtures.
    mkdir -p "$corpus_dir"
    cap="$corpus_dir/capture-$(date +%Y%m%d).json"
    req='{"model":"gpt-5.5","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"Call the ping tool once."}]}],"tools":[{"type":"function","name":"ping","description":"Reply check","parameters":{"type":"object","properties":{}}}],"store":false,"max_output_tokens":64}'
    # Token-bearing config file and the in-flight capture live OUTSIDE the
    # corpus and are trap-cleaned, so an interrupt cannot leave a credential
    # file behind and a failed capture (an error body with id/object-shaped
    # fields, say) cannot poison the replay corpus — only a validated
    # response is copied in (panel findings).
    hdr="$(mktemp "${TMPDIR:-/tmp}/oai-canary-hdr.XXXXXX")"
    cap_tmp="$(mktemp "${TMPDIR:-/tmp}/oai-canary-cap.XXXXXX")"
    trap 'rm -f "$hdr" "$cap_tmp"' EXIT INT TERM
    printf 'header = "Authorization: Bearer %s"\n' "$OPENAI_API_KEY" > "$hdr"
    if curl -sS --max-time 60 --config "$hdr" \
         -H 'content-type: application/json' \
         -d "$req" https://api.openai.com/v1/responses -o "$cap_tmp" \
       && jq -e '.id? and (.object? == "response")' "$cap_tmp" >/dev/null 2>&1; then
      cp "$cap_tmp" "$cap"
      run_check live_public_capture cargo run --quiet \
        --manifest-path "$crate/Cargo.toml" --example openai_drift_check -- "$cap"
      # Rolling window: keep the newest 8 captures.
      ls -t "$corpus_dir"/capture-*.json 2>/dev/null | tail -n +9 | xargs rm -f 2>/dev/null || true
    else
      failures+=(live_public_capture)
      mv "$cap_tmp" "$cap_tmp.failed" 2>/dev/null || true
      say "FAIL live_public_capture (request failed or response not a Response object; body preserved at $cap_tmp.failed)"
    fi
    rm -f "$hdr"
  else
    say "OPENAI_API_KEY unavailable; skipping public live checks"
  fi
  if [ -f "$HOME/.config/mu/auth/openai-codex.json" ]; then
    run_check live_codex env MU_LIVE_OPENAI_CODEX=1 \
      cargo test --quiet --manifest-path "$repo/Cargo.toml" -p mu-ai live_codex
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
