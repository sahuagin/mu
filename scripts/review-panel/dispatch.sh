#!/bin/sh
# panel_review.sh — dispatch the code_review panel at one prompt, in parallel, via `mu ask`.
#
# Single source of truth = ~/.config/mu/agent_roles.toml (`[[code_review.ranked]]`):
#   provider+model  -> resolved via `agent-role code_review <rank>`
#   tools           -> per-rank `tools` field (default "read,grep"; "" => zero tools)
# Nothing model-specific is hardcoded here — change models/tools in the TOML, not this script.
#
# Behavior:
#   - ollama ranks are WARMED first (a tiny probe forces the VRAM load — ~232s first call —
#     so the real review isn't charged the load tax and mis-read as a timeout / "bad model").
#   - each rank's tool grant comes from config; "" is passed as `--tools ""` (zero tools),
#     which is NOT the same as omitting --tools (that falls back to the daemon default set).
#
# usage: panel_review.sh <prompt-file> <out-prefix> [review-cwd] [timeout-sec]
set -u
PF="$1"; OUT="$2"; CWD="${3:-$PWD}"; TMO="${4:-600}"
MU="${MU_BIN:-$HOME/src/public_github/mu/target/release/mu}"
ROLES="${AGENT_ROLES:-$HOME/.config/mu/agent_roles.toml}"
TQ="${TQ:-$HOME/.cargo/bin/tq}"; command -v "$TQ" >/dev/null 2>&1 || TQ=tq
# OpenRouter key for the metered rank — exported silently, never printed.
OPENROUTER_API_KEY=$(tq -f "$HOME/.config/agent/config.toml" -r openrouter.api_key)
export OPENROUTER_API_KEY
cd "$CWD" || exit 1

ranks_json=$("$TQ" -o json -f "$ROLES" code_review.ranked)
N=$(printf '%s' "$ranks_json" | jq -r 'length')

# warm a local model into VRAM so the first real call isn't paying the load tax.
warmup() {  # $1=provider $2=model
  [ "$1" = "ollama" ] || return 0
  wf=$(mktemp); printf 'Reply with only: ok\n' > "$wf"
  timeout 600 "$MU" ask --bare --provider "$1" --model "$2" --tools "" --prompt-file "$wf" >/dev/null 2>&1
  rm -f "$wf"
}

r=0
while [ "$r" -lt "$N" ]; do
  set -- $(agent-role code_review "$r"); prov="$1"; model="$2"
  tools=$(printf '%s' "$ranks_json" | jq -r ".[$r].tools // \"read,grep\"")
  tag="rank${r}.$(printf '%s' "$model" | tr '/:' '__')"
  (
    warmup "$prov" "$model"
    timeout "$TMO" "$MU" ask --bare --provider "$prov" --model "$model" \
      --tools "$tools" --prompt-file "$PF" \
      > "${OUT}.${tag}.out" 2> "${OUT}.${tag}.err"
    echo "exit=$? prov=$prov model=$model tools=[$tools]" > "${OUT}.${tag}.done"
  ) &
  r=$((r + 1))
done
wait
echo "PANEL_COMPLETE" > "${OUT}.complete"
