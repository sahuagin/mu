#!/bin/sh
# panel_review.sh — dispatch the code_review panel at one prompt, in parallel, via `mu ask`.
#
# Single source of truth = ~/.config/mu/agent_roles.toml (`[[code_review.ranked]]`):
#   provider+model  -> resolved via `agent-role code_review <rank>`
#   tools           -> per-rank `tools` field (default "read,grep"; "" => zero tools)
#   max_turns       -> per-rank or role-level turn budget; omitted = provider default
#   focus           -> per-rank review focus (mu-3ajg): appended to the shared
#                      round-1 prompt as that seat's primary depth assignment, so
#                      parallel seats (e.g. two ornith cards) review DIFFERENT
#                      topics instead of duplicating one review. Omitted = the
#                      seat reviews the full surface, exactly as before.
#   seam, checklist -> per-rank EXCLUSIVE lens (mu-review-gate-seam-reviewers-9vkbt.2):
#                      the seat's only criteria are its checklist; other defect
#                      classes belong to other seats. seam = "conformance" is
#                      built in: its checklist is the PROJECT ARCHITECTURE
#                      INVARIANTS block already in the shared prompt (AGENTS.md
#                      "## Architecture invariants"). A rank carries focus OR
#                      seam. Prompt assembly lives in seat-prompt.sh (testable).
# Nothing model-specific is hardcoded here — change models/tools in the TOML, not this script.
#
# Behavior:
#   - ollama ranks are WARMED first (a tiny probe forces the VRAM load — ~232s first call —
#     so the real review isn't charged the load tax and mis-read as a timeout / "bad model").
#   - each rank's tool grant comes from config; "" is passed as `--tools ""` (zero tools),
#     which is NOT the same as omitting --tools (that falls back to the daemon default set).
#
# Per-seat wall-clock caps come from seat-timeout.sh: local seats get a longer
# one than API seats, and a ranked entry's `timeout_secs` overrides both. The
# optional <timeout-sec> argument forces ONE cap on every seat (callers pass it
# only to override the whole panel; consensus.sh does not).
#
# usage: panel_review.sh <prompt-file> <out-prefix> [review-cwd] [timeout-sec]
set -u
PF="$1"; OUT="$2"; CWD="${3:-$PWD}"; TMO_ALL="${4:-}"
# Fail fast on a missing/unreadable prompt — otherwise it only surfaces later
# as N parallel per-seat failures with no clear cause (panel finding, mu-3ajg).
[ -r "$PF" ] || { echo "dispatch.sh: prompt file missing or unreadable: $PF" >&2; exit 2; }
MU="${MU_BIN:-$HOME/src/public_github/mu/target/release/mu}"
ROLES="${AGENT_ROLES:-$HOME/.config/mu/agent_roles.toml}"
TQ="${TQ:-$HOME/.cargo/bin/tq}"; command -v "$TQ" >/dev/null 2>&1 || TQ=tq
# Dispatch via the shared lib (mu-q0xl): provider routing — incl. claude-oauth ->
# `claude -p` — lives in ONE place. Calling `mu ask --provider` directly here meant
# a claude-oauth rank hit "unknown provider" and emitted nothing every round. $HERE
# is captured before any cd so the relative lib path resolves regardless of $CWD.
HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
AGENT_DISPATCH_LIB="${AGENT_DISPATCH_LIB:-$HERE/../lib/agent-dispatch.sh}"
[ -r "$AGENT_DISPATCH_LIB" ] || { echo "dispatch.sh: missing dispatch lib: $AGENT_DISPATCH_LIB" >&2; exit 2; }
. "$AGENT_DISPATCH_LIB"
# mu-0htd: constrained verdict re-ask when a reviewer answers in prose.
. "$HERE/verdict-retry.sh"
# Seat prompt assembly (focus / seam / conformance), kept model-free so it is
# unit-tested (scripts/tests/seat-prompt-test.sh).
. "$HERE/seat-prompt.sh"
# mu-ash9p: per-provider-class seat caps (seat_timeout).
. "$HERE/seat-timeout.sh"
# OpenRouter key for the metered rank — exported silently, never printed.
OPENROUTER_API_KEY=$(tq -f "$HOME/.config/agent/config.toml" -r openrouter.api_key)
export OPENROUTER_API_KEY
cd "$CWD" || exit 1

# ci-aipr should not wait behind an operator's interactive ollama lease. When the
# shared box is already held, the ollama rank exits 75 and converge.py ignores it,
# allowing hosted reviewers to form the gate verdict. Set AI_REVIEW_OLLAMA_SKIP_IF_HELD=0
# to restore the old fair-queue wait behavior for an explicit local-review run.
AGENT_DISPATCH_OLLAMA_SKIP_IF_HELD="${AI_REVIEW_OLLAMA_SKIP_IF_HELD:-1}"
export AGENT_DISPATCH_OLLAMA_SKIP_IF_HELD

ranks_json=$("$TQ" -o json -f "$ROLES" code_review.ranked)
N=$(printf '%s' "$ranks_json" | jq -r 'length')

# warm a local model into VRAM so the first real call isn't paying the load tax.
warmup() {  # $1=provider $2=model
  [ "$1" = "ollama" ] || return 0
  wf=$(mktemp); printf 'Reply with only: ok\n' > "$wf"
  if [ -z "${AGENT_DISPATCH_NO_LEASE:-}" ] && [ "${AGENT_DISPATCH_OLLAMA_SKIP_IF_HELD:-}" = "1" ] && command -v with-ollama-lease >/dev/null 2>&1; then
    with-ollama-lease --skip-if-held timeout 600 "$MU" ask --bare --provider "$1" --model "$2" --tools "" --prompt-file "$wf" >/dev/null 2>&1
  else
    timeout 600 "$MU" ask --bare --provider "$1" --model "$2" --tools "" --prompt-file "$wf" >/dev/null 2>&1
  fi
  rc=$?
  rm -f "$wf"
  return "$rc"
}

r=0
while [ "$r" -lt "$N" ]; do
  set -- $(agent-role code_review "$r"); prov="$1"; model="$2"
  tools=$(printf '%s' "$ranks_json" | jq -r ".[$r].tools // \"read,grep\"")
  focus=$(printf '%s' "$ranks_json" | jq -r ".[$r].focus // \"\"")
  seam=$(printf '%s' "$ranks_json" | jq -r ".[$r].seam // \"\"")
  checklist=$(printf '%s' "$ranks_json" | jq -r ".[$r].checklist // \"\"")
  max_turns=$(agent-role --max-turns code_review "$r" 2>/dev/null || true)
  # This seat's cap: roster `timeout_secs` > provider class (local vs API).
  tmo=$(seat_timeout "$prov" "$(printf '%s' "$ranks_json" | jq -r ".[$r].timeout_secs // \"\"")")
  [ -n "$TMO_ALL" ] && tmo="$TMO_ALL"
  # Per-rank endpoint/lease (mu-vneb): a config-defined per-card rank pins its
  # server + lock via agent_roles.toml `endpoint`/`lease` keys, emitted by
  # `agent-role --env` as OLLAMA_API_BASE / OLLAMA_LEASE_NAME (or VLLM_API_BASE).
  # Without this every ollama rank dialed the DEFAULT box (:11434) — the daily
  # driver's card — evicting it and serialising reviewers on one lock. Empty for
  # ranks without the keys, so unpinned rosters are unchanged. Requires
  # agent-role with `--env` (mu#478). A stale agent-role lacking it must NOT fail
  # silently (a reviewer flagged the swallowed error) — warn once, then degrade
  # to the default endpoint rather than break the run.
  rank_env=$(agent-role --env code_review "$r" 2>/dev/null) || {
    [ -n "${_env_warned:-}" ] || { echo "dispatch.sh: 'agent-role --env' failed — per-rank endpoints DISABLED (reviewers use the default box). Update agent-role (mu#478)." >&2; _env_warned=1; }
    rank_env=""
  }
  tag="rank${r}.$(printf '%s' "$model" | tr '/:' '__')"
  # mu-3ajg / 9vkbt.2: a rank with a `focus` or a `seam` reviews from its own
  # prompt file — the shared prompt plus a trusted seat clause — so parallel
  # seats dig into different topics in round 1. The clause changes the seat's
  # criteria only: the output contract is untouched, so converge.py and the
  # convergence rounds see no format difference. Built fail-closed (panel
  # reviewer finding, round 1 of mu-3ajg's own gate): if the seat file cannot
  # be built, the seat falls back to the SHARED prompt — a duplicate review
  # beats a review of half a prompt — and .done records the fallback.
  seat_pf="${OUT}.${tag}.prompt"
  # MU_REVIEW_INVARIANTS_PRESENT is the gate's own statement of whether an
  # invariants block exists — never inferred from the prompt text, which
  # embeds untrusted repo content.
  seat_mode=$(seat_prompt "$PF" "$seat_pf" "$focus" "$seam" "$checklist" "${MU_REVIEW_INVARIANTS_PRESENT:-}") || {
    echo "dispatch.sh: could not build the seat prompt for $tag (focus=[$focus] seam=[$seam]) — falling back to the shared prompt" >&2
    rm -f "$seat_pf"
    seat_mode=shared; focus=""; seam=""
  }
  case "$seat_mode" in
    shared) seat_pf="$PF" ;;
    seam|conformance) focus="" ;;  # the seam won; keep the .done record honest
  esac
  (
    [ -n "$rank_env" ] && eval "export $rank_env"
    warmup "$prov" "$model"
    # agent_dispatch reads TOOLS/TIMEOUT/MU/ERRLOG from scope; stdout = the model's
    # output (-> .out), stderr -> $ERRLOG (per-rank .err). claude-oauth now routes
    # to `claude -p` instead of erroring. (Subshell-local assignments: no leakage.)
    TOOLS="$tools"; TIMEOUT="$tmo"; MAX_TURNS="$max_turns"; ERRLOG="${OUT}.${tag}.err"
    _out="${OUT}.${tag}.out"
    _retry=0
    # Default 0: a retry doubles the wall-clock a dead seat costs, and since
    # mu-ash9p a timed-out seat is absent for the round rather than fatal to it.
    _max_retries="${MU_REVIEW_TIMEOUT_RETRIES:-${AI_REVIEW_TIMEOUT_RETRIES:-0}}"
    agent_dispatch "$prov" "$model" "$seat_pf" > "$_out"
    _rc=$?
    while [ "$_rc" -eq 124 ] && [ "$_retry" -lt "$_max_retries" ]; do
      _retry=$((_retry + 1))
      printf '%s\n' "reviewer timeout after ${tmo}s; retry ${_retry}/${_max_retries}" >> "$ERRLOG"
      agent_dispatch "$prov" "$model" "$seat_pf" > "$_out"
      _rc=$?
    done
    reask_if_unparsed "$prov" "$model" "$_out"
    echo "exit=$_rc retry=$_retry prov=$prov model=$model tmo=$tmo tools=[$tools] focus=[$focus] seam=[$seam]" > "${OUT}.${tag}.done"
  ) &
  r=$((r + 1))
done
wait
echo "PANEL_COMPLETE" > "${OUT}.complete"
