#!/bin/sh
# consensus.sh — goal-protocol consensus code review.
#
# The code_review panel (agent_roles.toml) reviews a diff, then converges over
# ANTAGONISTIC rounds toward one agreed verdict: each round, every reviewer sees
# the others' positions and is told to press objections, concede, and move toward
# agreement. Stops when the panel agrees OR after <max-rounds> (default 4); the
# orchestrator escalates an unresolved split. Models can't talk directly, so this
# script mediates the rounds (mu-dialogue peer-convergence is a future swap-in).
#
# usage: consensus.sh <round1-prompt-file> <out-dir> [review-cwd] [max-rounds]
# exit:  0 consensus reached (verdict on stdout: "CONSENSUS <verdict>")
#        3 no consensus after max-rounds (ESCALATE)
set -u
P1="$1"; OUT="$2"; CWD="${3:-$PWD}"; MAXR="${4:-4}"
HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROLES="${AGENT_ROLES:-$HOME/.config/mu/agent_roles.toml}"
MU="${MU_BIN:-mu}"; TQ="${TQ:-tq}"
# Convergence rounds dispatch via the shared lib (mu-q0xl) so provider routing —
# incl. claude-oauth -> `claude -p` — matches round 1 (dispatch.sh) and ai-review.
# Calling `mu ask --provider` directly here made a claude-oauth rank emit nothing
# every round -> permanent SPLIT/false NO CONSENSUS.
AGENT_DISPATCH_LIB="${AGENT_DISPATCH_LIB:-$HERE/../lib/agent-dispatch.sh}"
[ -r "$AGENT_DISPATCH_LIB" ] || { echo "consensus.sh: missing dispatch lib: $AGENT_DISPATCH_LIB" >&2; exit 2; }
. "$AGENT_DISPATCH_LIB"
mkdir -p "$OUT"
# the diff that convergence prompts quote = the content of the first ```diff fence.
awk '/^```diff/{f=1;next} /^```/{if(f)exit} f' "$P1" > "$OUT/diff.txt"

# round 1: whole panel, same prompt (config-driven dispatch + ollama warm-up).
sh "$HERE/dispatch.sh" "$P1" "$OUT/r1" "$CWD" 900
res=$(python3 "$HERE/converge.py" agree "$OUT/r1"); echo "round 1: $res"
case "$res" in AGREE\ *) echo "CONSENSUS ${res#AGREE }"; exit 0;; esac

ranks_json=$("$TQ" -o json -f "$ROLES" code_review.ranked)
N=$(printf '%s' "$ranks_json" | jq -r 'length')
OPENROUTER_API_KEY=$(tq -f "$HOME/.config/agent/config.toml" -r openrouter.api_key 2>/dev/null)
export OPENROUTER_API_KEY

round=1
while [ "$round" -lt "$MAXR" ]; do
  prev=$round; round=$((round + 1))
  echo "--- convergence round $round ---"
  r=0
  while [ "$r" -lt "$N" ]; do
    set -- $(agent-role code_review "$r"); prov="$1"; model="$2"
    tools=$(printf '%s' "$ranks_json" | jq -r ".[$r].tools // \"read,grep\"")
    tag="rank${r}.$(printf '%s' "$model" | tr '/:' '__')"
    python3 "$HERE/converge.py" prompt "$OUT/r$prev" "$round" "$OUT/diff.txt" "$tag" \
      "$OUT/r${round}.${tag}.prompt" >/dev/null
    (
      cd "$CWD" || exit 1
      # agent_dispatch reads TOOLS/TIMEOUT/MU/ERRLOG from scope; stdout -> .out,
      # stderr -> $ERRLOG. claude-oauth now routes to `claude -p` instead of erroring.
      TOOLS="$tools"; TIMEOUT=900; ERRLOG="$OUT/r${round}.${tag}.err"
      agent_dispatch "$prov" "$model" "$OUT/r${round}.${tag}.prompt" \
        > "$OUT/r${round}.${tag}.out"
      echo "exit=$? $prov/$model" > "$OUT/r${round}.${tag}.done"
    ) &
    r=$((r + 1))
  done
  wait
  res=$(python3 "$HERE/converge.py" agree "$OUT/r${round}"); echo "round $round: $res"
  case "$res" in AGREE\ *) echo "CONSENSUS ${res#AGREE }"; exit 0;; esac
done
echo "NO CONSENSUS after $MAXR rounds — ESCALATE. Final positions: $res"
exit 3
