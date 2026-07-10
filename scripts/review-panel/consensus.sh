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
# mu-0htd: constrained verdict re-ask when a reviewer answers in prose.
. "$HERE/verdict-retry.sh"
# ci-aipr/review-panel should route around an operator-held ollama box instead
# of waiting behind the fair lock. dispatch.sh uses the same default for round 1;
# keep it exported for convergence rounds that call agent_dispatch directly.
AGENT_DISPATCH_OLLAMA_SKIP_IF_HELD="${AI_REVIEW_OLLAMA_SKIP_IF_HELD:-1}"
export AGENT_DISPATCH_OLLAMA_SKIP_IF_HELD
mkdir -p "$OUT"

# Scrub the round-1 prompt to valid UTF-8 IN PLACE before any dispatch (mu-4xfs).
# ai-review.sh assembles this file with several `head -c` truncations (full-file
# context, single-shot diff, spec, synth context) that cut on a BYTE boundary —
# splitting a multibyte character (—, ∈, →, …) leaves an invalid trailing
# sequence. `mu ask --prompt-file` then rejects the whole file with "stream did
# not contain valid UTF-8", killing round 1 for every reviewer (rounds 2+ quote
# only the already-fenced diff and survived, masking the cause). iconv is
# base-system; python3 (this dir's converge.py/parse.py) is the fallback.
scrub_utf8() { # $1=path, scrubbed in place; no-op if no scrubber available
  _f="$1"; [ -f "$_f" ] || return 0
  _tmp="$(mktemp "${TMPDIR:-/tmp}/ai-review-utf8.XXXXXX")" || return 0
  if command -v iconv >/dev/null 2>&1 && iconv -f UTF-8 -t UTF-8 -c "$_f" >"$_tmp" 2>/dev/null; then
    mv "$_tmp" "$_f"
  elif command -v python3 >/dev/null 2>&1 && python3 -c 'import sys; open(sys.argv[2],"wb").write(open(sys.argv[1],"rb").read().decode("utf-8","ignore").encode("utf-8"))' "$_f" "$_tmp" 2>/dev/null; then
    mv "$_tmp" "$_f"
  else
    rm -f "$_tmp"
  fi
}
scrub_utf8 "$P1"

# the review material that convergence prompts quote = content of the first
# ```diff fence. In normal mode this is the PR diff; in chunked mode it is the
# aggregated leaf findings plus targeted file context for cited paths.
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
    max_turns=$(agent-role --max-turns code_review "$r" 2>/dev/null || true)
    # Per-rank endpoint/lease (mu-vneb) — same as dispatch.sh round 1, so a
    # per-card rank keeps dialing its own server across all convergence rounds.
    # Requires agent-role --env (mu#478); warn once, don't silently no-op.
    rank_env=$(agent-role --env code_review "$r" 2>/dev/null) || {
      [ -n "${_env_warned:-}" ] || { echo "consensus.sh: 'agent-role --env' failed — per-rank endpoints DISABLED (reviewers use the default box). Update agent-role (mu#478)." >&2; _env_warned=1; }
      rank_env=""
    }
    tag="rank${r}.$(printf '%s' "$model" | tr '/:' '__')"
    python3 "$HERE/converge.py" prompt "$OUT/r$prev" "$round" "$OUT/diff.txt" "$tag" \
      "$OUT/r${round}.${tag}.prompt" >/dev/null
    (
      cd "$CWD" || exit 1
      [ -n "$rank_env" ] && eval "export $rank_env"
      # agent_dispatch reads TOOLS/TIMEOUT/MU/ERRLOG from scope; stdout -> .out,
      # stderr -> $ERRLOG. claude-oauth now routes to `claude -p` instead of erroring.
      TOOLS="$tools"; TIMEOUT=900; MAX_TURNS="$max_turns"; ERRLOG="$OUT/r${round}.${tag}.err"
      _out="$OUT/r${round}.${tag}.out"
      _retry=0
      _max_retries="${MU_REVIEW_TIMEOUT_RETRIES:-${AI_REVIEW_TIMEOUT_RETRIES:-1}}"
      agent_dispatch "$prov" "$model" "$OUT/r${round}.${tag}.prompt" > "$_out"
      _rc=$?
      while [ "$_rc" -eq 124 ] && [ "$_retry" -lt "$_max_retries" ]; do
        _retry=$((_retry + 1))
        printf '%s\n' "reviewer timeout after ${TIMEOUT}s; retry ${_retry}/${_max_retries}" >> "$ERRLOG"
        agent_dispatch "$prov" "$model" "$OUT/r${round}.${tag}.prompt" > "$_out"
        _rc=$?
      done
      reask_if_unparsed "$prov" "$model" "$_out"
      echo "exit=$_rc retry=$_retry $prov/$model" > "$OUT/r${round}.${tag}.done"
    ) &
    r=$((r + 1))
  done
  wait
  res=$(python3 "$HERE/converge.py" agree "$OUT/r${round}"); echo "round $round: $res"
  case "$res" in AGREE\ *) echo "CONSENSUS ${res#AGREE }"; exit 0;; esac
done
echo "NO CONSENSUS after $MAXR rounds — ESCALATE. Final positions: $res"
exit 3
