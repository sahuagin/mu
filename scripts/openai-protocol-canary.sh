#!/usr/bin/env bash
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
log_prefix="[openai-protocol-canary]"
failures=()

run_check() {
  local name="$1"; shift
  echo "$log_prefix running $name"
  if ! "$@"; then failures+=("$name"); fi
}

run_check spec_event_name xzgrep -q 'response.function_call_arguments.delta' "$repo/crates/providers/mu-openai/specifications/openapi.yaml.xz"
run_check spec_responses_path xzgrep -q '^  /responses:' "$repo/crates/providers/mu-openai/specifications/openapi.yaml.xz"
run_check mu_openai_unit cargo test -p mu-openai --quiet

if [ "$live" = 1 ]; then
  if [ -z "${OPENAI_API_KEY:-}" ] && command -v tq >/dev/null 2>&1; then
    OPENAI_API_KEY="$(tq -f "$HOME/.config/agent/config.toml" -r openai.api_key 2>/dev/null || true)"
    export OPENAI_API_KEY
  fi
  if [ -n "${OPENAI_API_KEY:-}" ] && [ "${OPENAI_API_KEY}" != "null" ]; then
    run_check live_public_openai env MU_LIVE_OPENAI_API=1 cargo test -p mu-openai live_public_api --quiet
  else
    echo "$log_prefix OPENAI_API_KEY unavailable; skipping public API live checks"
  fi
  if [ -f "$HOME/.config/mu/auth/openai-codex.json" ]; then
    run_check live_codex_text env MU_LIVE_OPENAI_CODEX=1 cargo test -p mu-ai live_tests::b12_live_codex_text_smoke --quiet
    run_check live_codex_tool env MU_LIVE_OPENAI_CODEX=1 cargo test -p mu-ai live_tests::b13_live_codex_tool_call --quiet
  else
    echo "$log_prefix openai-codex token unavailable; skipping Codex live checks"
  fi
fi

if [ "${#failures[@]}" -eq 0 ]; then
  echo "$log_prefix ok"
  exit 0
fi

msg="OpenAI protocol canary failed: ${failures[*]}"
echo "$log_prefix $msg" >&2

if [ "$alert" = "bead" ] && command -v beads >/dev/null 2>&1; then
  url="$(awk -F= '/^mu=/{print $2}' "$HOME/.config/beads/remotes.env" 2>/dev/null || true)"
  if [ -n "$url" ]; then
    beads --url "$url" exec -- create \
      --title "$msg" \
      --slug openai-protocol-canary-drift \
      --type bug \
      --priority P1 \
      --description "Weekly OpenAI protocol canary detected drift/failure in: ${failures[*]}. Check $HOME/.local/share/openai-canary.log on $(hostname)." \
      --actor openai-protocol-canary >/dev/null || true
  fi
fi

if command -v task_log >/dev/null 2>&1; then
  task_log add "$msg" --status failed >/dev/null 2>&1 || true
fi

exit 1
