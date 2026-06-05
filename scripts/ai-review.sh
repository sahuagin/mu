#!/usr/bin/env bash
# ai-review.sh — pre-PR cross-provider review gate (bead mu-6qst).
#
# A SEPARATE-provider model reviews the working diff before a PR — a third
# check on top of CI and the two humans/agent. Run it via `just ci-aipr`,
# which runs `just ci` first and only reviews green code.
#
# The verdict is read from the reviewer's STDOUT, NOT its process exit code:
# a reviewer's process exit != its verdict, and `mu ask` historically exits
# non-zero on a shutdown wart (mu-qc08) even on success — so the exit code is
# not load-bearing here.
#
# Design: ~/.claude-personal/notes/design-prepr-review-and-degradation-gate.md
# Process-layer auditors / correlation: bead mu-pr6r.
#
# Env:
#   MU_REVIEW_PROVIDER          reviewer provider (default: ollama; codex = openai-codex)
#   MU_REVIEW_MODEL             reviewer model    (default: gpt-oss:20b)
#   MU_REVIEW_TOOLS             reviewer tools, e.g. "read,grep" (default: none, single-shot)
#   MU_REVIEW_BASE              base ref to diff against (default: main)
#   MU_REVIEW_OVERRIDE=1        operator override: proceed despite REJECT (logged)
#   MU_REVIEW_ESCALATE_PROVIDER on REJECT, re-review with this provider; the
#                               spread is a differential diagnosis (all-fail =
#                               design problem; any-pass = capacity datapoint).
#   MU_REVIEW_ESCALATE_MODEL    model for the escalation (default: MU_REVIEW_MODEL)
#   MU_REVIEW_LOG               event log (default: ~/.local/share/mu/review-events.jsonl)
#   MU_REVIEW_NO_COLOR          disable color

set -u
set -o pipefail

# Default reviewer = local ollama: free, reliable, and a non-Claude second
# opinion. codex/gpt-5.5 is a stronger reviewer when its OAuth is healthy —
# select it with MU_REVIEW_PROVIDER=openai-codex MU_REVIEW_MODEL=gpt-5.5, and/or
# use it as the escalation target (MU_REVIEW_ESCALATE_PROVIDER). It is NOT the
# default because its OAuth refresh was failing at build time (cf. bead mu-cea).
# Default model gpt-oss:20b: scored 0.954 (12/12 recall, 119 tok/s) on
# code-review-bench 2026-06-04, vs 0.700 for the previous default
# qwen3-coder:30b. Kept warm on the box with qwen3-embedding:8b (both fit
# 48GB). See ~/src/public_github/code-review-bench/reports/NOTES.md.
PROVIDER="${MU_REVIEW_PROVIDER:-ollama}"
MODEL="${MU_REVIEW_MODEL:-gpt-oss:20b}"
TOOLS="${MU_REVIEW_TOOLS:-}"   # empty = single-shot (default); e.g. "read,grep" lets the reviewer inspect surrounding code (slower, multi-turn)
BASE="${MU_REVIEW_BASE:-main}"
LOG="${MU_REVIEW_LOG:-$HOME/.local/share/mu/review-events.jsonl}"
# Minimal reviewer system prompt (mu-ai-review-minimal-sysprompt-9esh).
# Without this, `mu ask` sessions get the daemon-default system prompt —
# ~28KB of operator memory kernel, a pure distractor for a review gate
# and the prime suspect for persona-bleed verdicts. --append-system-prompt
# OVERRIDES the daemon default (mu-x83o semantics), which is what we want.
SYSPROMPT="${MU_REVIEW_SYSTEM_PROMPT:-$(dirname "$0")/ai-review-system-prompt.txt}"
ERRLOG="${TMPDIR:-/tmp}/ai-review-stderr.$$"   # reviewer stderr kept (not discarded) so silent failures (e.g. provider auth) are diagnosable

if [ -t 1 ] && [ -z "${MU_REVIEW_NO_COLOR:-}" ]; then
  C_RED=$'\033[31m'; C_GREEN=$'\033[32m'; C_YEL=$'\033[33m'; C_DIM=$'\033[2m'; C_OFF=$'\033[0m'
else
  C_RED=""; C_GREEN=""; C_YEL=""; C_DIM=""; C_OFF=""
fi

# --- repo root (jj workspaces have no top-level .git) ----------------------
if command -v jj >/dev/null 2>&1 && jj root >/dev/null 2>&1; then
  ROOT="$(jj root)"
else
  ROOT="$(git rev-parse --show-toplevel 2>/dev/null)"
fi
[ -n "${ROOT:-}" ] || { echo "${C_RED}ai-review: not in a repo${C_OFF}" >&2; exit 2; }
cd "$ROOT" || exit 2

# --- the diff to review (jj-aware) -----------------------------------------
if command -v jj >/dev/null 2>&1 && jj root >/dev/null 2>&1; then
  DIFF="$(jj diff --from "$BASE" --git 2>/dev/null)"
else
  DIFF="$(git diff "$BASE"...HEAD 2>/dev/null)"
fi
if [ -z "${DIFF//[[:space:]]/}" ]; then
  echo "${C_DIM}ai-review: no diff vs $BASE — nothing to review.${C_OFF}"
  exit 0
fi
FILES=$(printf '%s\n' "$DIFF" | grep -c '^diff --git ')
ESC_BOOL=false; [ -n "${MU_REVIEW_ESCALATE_PROVIDER:-}" ] && ESC_BOOL=true

# --- reviewer client = freshly-built mu, never the (possibly stale) installed one
cargo build --bin mu -q 2>/dev/null || true
if   [ -x ./target/release/mu ]; then MU=./target/release/mu
elif [ -x ./target/debug/mu ];   then MU=./target/debug/mu
else MU="$(command -v mu || true)"; fi
[ -n "${MU:-}" ] || { echo "${C_RED}ai-review: no mu binary found${C_OFF}" >&2; exit 2; }

if [ -n "$TOOLS" ]; then
  TOOL_CLAUSE="Use the read and grep tools to inspect surrounding code when a judgement needs it."
else
  TOOL_CLAUSE="Review the diff exactly as given below — do NOT call any tools and do NOT emit any function-call or tool-call syntax; respond with prose only."
fi
PROMPT="You are a strict pre-PR code reviewer. Review the diff below for: correctness bugs; concurrency / lifecycle hazards (e.g. a held reference that blocks shutdown, a clone that outlives its owner); missing error handling; and safeguards that nearby code already applies but this diff omits. $TOOL_CLAUSE Be concise and specific; cite file:line. Your reply's LAST line MUST be exactly 'VERDICT: APPROVE' or 'VERDICT: REJECT' (those literal words). List the most important findings first.

DIFF:
$DIFF"

run_review() { # $1=provider $2=model — prints reviewer stdout; stderr -> $ERRLOG
  # The reviewer session must be hermetic: --bare (PR #187) guarantees
  # mu injects nothing — no session-start memory/project-file recall,
  # no discovery bootstrap — so the session's system prompt is exactly
  # the minimal reviewer prompt below (and nothing at all if the file
  # is missing). Replaces the MU_NO_RECALL=1 env spelling from #185.
  # shellcheck disable=SC2086 — $SYS_FLAGS intentionally word-splits
  SYS_FLAGS=""
  [ -r "$SYSPROMPT" ] && SYS_FLAGS="--append-system-prompt $SYSPROMPT"
  if [ -n "$TOOLS" ]; then
    timeout 300 "$MU" ask --bare --provider "$1" --model "$2" --thinking low $SYS_FLAGS --tools "$TOOLS" "$PROMPT" 2>>"$ERRLOG"
  else
    timeout 300 "$MU" ask --bare --provider "$1" --model "$2" --thinking low $SYS_FLAGS "$PROMPT" 2>>"$ERRLOG"
  fi
}
verdict_of() { # stdin -> APPROVE | REJECT | UNCLEAR
  local out; out="$(cat)"
  # Tolerate markdown-dressed verdicts ("**Verdict:** APPROVE") — models
  # flake on the literal format; up to a few non-letter chars may sit
  # between VERDICT and the word (observed live 2026-06-05, was UNCLEAR).
  if   printf '%s' "$out" | grep -qiE 'VERDICT[^A-Za-z]{1,8}REJECT';  then echo REJECT
  elif printf '%s' "$out" | grep -qiE 'VERDICT[^A-Za-z]{1,8}APPROVE'; then echo APPROVE
  else echo UNCLEAR; fi
}
log_event() { # $1=verdict $2=override(true/false) $3=escalated(true/false) [$4=provider $5=model]
  local p="${4:-$PROVIDER}" m="${5:-$MODEL}"
  mkdir -p "$(dirname "$LOG")" 2>/dev/null || true
  printf '{"ts":"%s","provider":"%s","model":"%s","verdict":"%s","base":"%s","files_changed":%s,"override":%s,"escalated":%s}\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$p" "$m" "$1" "$BASE" "$FILES" "$2" "$3" >> "$LOG"
}

echo "${C_DIM}ai-review: $PROVIDER/$MODEL reviewing $FILES file(s) vs $BASE …${C_OFF}"
REVIEW="$(run_review "$PROVIDER" "$MODEL")"
printf '%s\n' "$REVIEW"
VERDICT="$(printf '%s' "$REVIEW" | verdict_of)"

if [ "$VERDICT" = "APPROVE" ]; then
  log_event APPROVE false false
  echo "${C_GREEN}ai-review: APPROVE${C_OFF}"
  exit 0
fi

# REJECT or UNCLEAR from here.
echo "${C_YEL}ai-review: $PROVIDER returned $VERDICT${C_OFF}"
[ "$VERDICT" = "UNCLEAR" ] && echo "${C_DIM}  (no VERDICT line parsed — reviewer may have erred; stderr: $ERRLOG)${C_OFF}"

# --- escalation ladder = differential diagnosis (opt-in) -------------------
if [ -n "${MU_REVIEW_ESCALATE_PROVIDER:-}" ]; then
  EP="$MU_REVIEW_ESCALATE_PROVIDER"; EM="${MU_REVIEW_ESCALATE_MODEL:-$MODEL}"
  echo "${C_DIM}ai-review: escalating to $EP/$EM for differential …${C_OFF}"
  REVIEW2="$(run_review "$EP" "$EM")"
  printf '%s\n' "$REVIEW2"
  V2="$(printf '%s' "$REVIEW2" | verdict_of)"
  log_event "$V2" false true "$EP" "$EM"
  if [ "$V2" = "APPROVE" ]; then
    echo "${C_YEL}ai-review: DIFFERENTIAL — $PROVIDER said $VERDICT but $EP APPROVED.${C_OFF}"
    echo "  → likely a $PROVIDER dip, not a defect. Proceeding; logged as a capacity datapoint."
    exit 0
  fi
  echo "${C_RED}ai-review: UNANIMOUS non-approval across providers → treat as a real design/correctness problem.${C_OFF}"
fi

# --- gate (override is logged, and is itself a calibration signal) ---------
if [ "${MU_REVIEW_OVERRIDE:-}" = "1" ]; then
  log_event "$VERDICT" true "$ESC_BOOL"
  echo "${C_YEL}ai-review: $VERDICT overridden by operator (MU_REVIEW_OVERRIDE=1). Logged.${C_OFF}"
  exit 0
fi
log_event "$VERDICT" false "$ESC_BOOL"
echo "${C_RED}ai-review: gate BLOCKED ($VERDICT). Set MU_REVIEW_OVERRIDE=1 to proceed if you disagree.${C_OFF}" >&2
exit 1
