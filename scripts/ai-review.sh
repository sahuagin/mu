#!/usr/bin/env bash
# ai-review.sh — pre-PR two-reviewer PANEL gate (beads mu-6qst, mu-ai-review-panel-lrwq).
#
# Two LOCAL reviewer models form a panel that reviews the working diff before a
# PR — a third check on top of CI and the human/agent. Run it via `just ci-aipr`,
# which runs `just ci` first and only reviews green code.
#
# WHY A PANEL (not a single champion): the two reviewers have complementary,
# inverted strengths — gpt-oss:20b leads single-shot review but trails
# agentically; qwen3-coder:30b is the reverse (code-review-bench + agentic-bench,
# 2026-06-04..06). A single-reviewer gate inherits one model's blind spots. On
# 2026-06-06 gpt-oss:20b APPROVEd a correct PR (#200) and REJECTed an equally
# correct one (#201) on a false premise (a hypothetical non-Copy compile error on
# the very line that fixes it), contradicted by the green `just ci` in the same
# recipe run. The panel exists to absorb exactly that single-reviewer miss: keep
# BOTH on the gate, treat them like a team, never optimize to one winner.
# (Operator-ratified: memory d88e133e / reviewer-models-as-team, 2026-06-06.)
#
# PANEL SEMANTICS (the verdict of each reviewer is read from its STDOUT, NOT its
# process exit code — `mu ask` historically exits non-zero on a shutdown wart
# (mu-qc08) even on success, so the exit code is not load-bearing here):
#
#   both APPROVE            → PASS     (exit 0)
#   both REJECT             → BLOCK    (exit 1)  — a real design/correctness call
#   split / any UNCLEAR     → ESCALATE (exit 3)  — operator decides; NOT self-overridden
#
# The two non-pass outcomes have DISTINCT exit codes (1 vs 3) and distinct,
# verdict-naming messages so a split is never confused with a unanimous block.
# MU_REVIEW_OVERRIDE=1 is the operator's override on BLOCK *or* ESCALATE: it
# proceeds (exit 0) and is logged as a calibration signal.
#
# This panel REPLACES the old single-reviewer + opt-in escalation-ladder shape
# (MU_REVIEW_ESCALATE_PROVIDER/_MODEL): the second reviewer IS the differential,
# always-on rather than only-on-reject.
#
# Design: ~/.claude-personal/notes/design-prepr-review-and-degradation-gate.md
# Process-layer auditors / correlation: bead mu-pr6r.
#
# Env:
#   Reviewer 1 (default ollama / qwen3-coder:30b):
#     MU_REVIEW_PROVIDER        provider (default: ollama; codex = openai-codex)
#     MU_REVIEW_MODEL           model    (default: qwen3-coder:30b)
#   Reviewer 2 (default ollama / gpt-oss:20b):
#     MU_REVIEW_PROVIDER_2      provider (default: ollama)
#     MU_REVIEW_MODEL_2         model    (default: gpt-oss:20b)
#   Shared:
#     MU_REVIEW_TOOLS           reviewer tools, e.g. "read,grep" (default: none, single-shot)
#     MU_REVIEW_BASE            base ref to diff against (default: main)
#     MU_REVIEW_FULL_FILES      1 = append full content of each changed file to the
#                               prompt so reviewers see definitions outside the diff
#                               window (default: 1; set 0 for diff-only)
#     MU_REVIEW_CONTEXT_MAX_BYTES  cap on appended full-file context (default: 200000)
#     MU_REVIEW_OVERRIDE=1      operator override: proceed despite BLOCK/ESCALATE (logged)
#     MU_REVIEW_SYSTEM_PROMPT   reviewer system-prompt file (default: ai-review-system-prompt.txt)
#     MU_REVIEW_LOG             event log (default: ~/.local/share/mu/review-events.jsonl)
#     MU_REVIEW_NO_COLOR        disable color
#
# The log carries BOTH reviewers' verdicts: one {"event":"reviewer",...} line per
# reviewer plus one {"event":"panel",...} summary line with the panel outcome.

set -u
set -o pipefail

# Both reviewers default to local ollama: free, reliable, non-Claude second/third
# opinions, both warm on the box (24h keep-alive, memory 3d973420) and co-resident
# in 48GB. qwen3-coder:30b and gpt-oss:20b have inverted strengths (see header) —
# the panel keeps both. codex/gpt-5.5 is a stronger reviewer when its OAuth is
# healthy; point either slot at it with MU_REVIEW_PROVIDER[_2]=openai-codex
# MU_REVIEW_MODEL[_2]=gpt-5.5. It is NOT a default because its OAuth refresh was
# failing at build time (bead mu-cea). Bench provenance:
# ~/src/public_github/code-review-bench/reports/NOTES.md.
PROVIDER="${MU_REVIEW_PROVIDER:-ollama}"
MODEL="${MU_REVIEW_MODEL:-qwen3-coder:30b}"
PROVIDER2="${MU_REVIEW_PROVIDER_2:-ollama}"
MODEL2="${MU_REVIEW_MODEL_2:-gpt-oss:20b}"
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

# Full content of each changed file, appended to the prompt as CONTEXT. A thin
# (-U3) diff hides definitions/guards that live outside the changed hunks, so
# single-shot reviewers false-positive on "undefined variable X" when X is
# defined ~100 lines away in unchanged code. Observed live 2026-06-06: both
# panel reviewers wrongly REJECTed this very script claiming ERRLOG was
# undefined — it is defined (line 81), just not inside the diff window. Giving
# them the full files lets them check before reporting. Disable with
# MU_REVIEW_FULL_FILES=0; cap appended bytes with MU_REVIEW_CONTEXT_MAX_BYTES.
CONTEXT=""
if [ "${MU_REVIEW_FULL_FILES:-1}" = "1" ]; then
  while IFS= read -r _f; do
    case "$_f" in ""|/dev/null) continue ;; esac   # skip blanks + pure deletions
    [ -f "$_f" ] || continue
    CONTEXT="$CONTEXT

===== FULL CONTENT: $_f =====
$(cat "$_f")"
  done <<EOF_CTX
$(printf '%s\n' "$DIFF" | sed -n 's#^+++ b/##p')
EOF_CTX
  _max="${MU_REVIEW_CONTEXT_MAX_BYTES:-200000}"
  if [ "${#CONTEXT}" -gt "$_max" ]; then
    CONTEXT="$(printf '%s' "$CONTEXT" | head -c "$_max")
... [changed-file context truncated at ${_max} bytes — review the diff above]"
  fi
fi

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
PROMPT="You are a strict pre-PR code reviewer. The DIFF below shows exactly what changed; review ONLY that change for: correctness bugs; concurrency / lifecycle hazards (e.g. a held reference that blocks shutdown, a clone that outlives its owner); missing error handling; and safeguards that nearby code already applies but this diff omits. The FULL CONTENT of each changed file is included after the diff so you can see definitions, helpers, and guards that live OUTSIDE the changed hunks — a variable or function used in the diff is often defined there, so CHECK the full content before reporting anything as undefined/unset, and do NOT raise findings about unchanged code. $TOOL_CLAUSE Be concise and specific; cite file:line. Your reply's LAST line MUST be exactly 'VERDICT: APPROVE' or 'VERDICT: REJECT' (those literal words). List the most important findings first.

DIFF:
$DIFF
$CONTEXT"

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
# Escape a value for embedding inside a JSON string (no surrounding quotes
# added). Pure bash, no jq dependency — this gate runs on boxes where jq may be
# absent (pots, fresh hosts), and the script already degrades gracefully on its
# other tools. Without this, a provider/model/base/verdict value containing a
# double-quote or backslash would corrupt review-events.jsonl, which the
# mu-mucm dashboards parse line-by-line. Backslash MUST be escaped first so the
# escapes added by the later substitutions are not themselves re-escaped.
# (bead mu-ai-review-log-escaping-augj)
json_escape() { # $1=raw -> JSON-string-safe text on stdout
  local s=$1
  s=${s//\\/\\\\}      # backslash  -> \\   (first, see note above)
  s=${s//\"/\\\"}      # double quote -> \"
  s=${s//$'\n'/\\n}    # newline    -> \n
  s=${s//$'\r'/\\r}    # carriage return -> \r
  s=${s//$'\t'/\\t}    # tab        -> \t
  printf '%s' "$s"
}
log_reviewer() { # $1=role $2=provider $3=model $4=verdict
  mkdir -p "$(dirname "$LOG")" 2>/dev/null || true
  printf '{"ts":"%s","event":"reviewer","role":"%s","provider":"%s","model":"%s","verdict":"%s","base":"%s","files_changed":%s}\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$1" "$(json_escape "$2")" "$(json_escape "$3")" "$(json_escape "$4")" "$(json_escape "$BASE")" "$FILES" >> "$LOG"
}
log_panel() { # $1=outcome(PASS|BLOCK|ESCALATE) $2=override(true|false)
  mkdir -p "$(dirname "$LOG")" 2>/dev/null || true
  printf '{"ts":"%s","event":"panel","outcome":"%s","r1_provider":"%s","r1_model":"%s","r1_verdict":"%s","r2_provider":"%s","r2_model":"%s","r2_verdict":"%s","base":"%s","files_changed":%s,"override":%s}\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$1" "$(json_escape "$PROVIDER")" "$(json_escape "$MODEL")" "$(json_escape "$V1")" "$(json_escape "$PROVIDER2")" "$(json_escape "$MODEL2")" "$(json_escape "$V2")" "$(json_escape "$BASE")" "$FILES" "$2" >> "$LOG"
}

# --- run the panel: both reviewers, same diff, sequentially ----------------
echo "${C_DIM}ai-review: PANEL reviewing $FILES file(s) vs $BASE — reviewers: $PROVIDER/$MODEL + $PROVIDER2/$MODEL2${C_OFF}"

echo "${C_DIM}── reviewer 1: $PROVIDER/$MODEL ─────────────────────────────${C_OFF}"
REVIEW1="$(run_review "$PROVIDER" "$MODEL")"
printf '%s\n' "$REVIEW1"
V1="$(printf '%s' "$REVIEW1" | verdict_of)"
log_reviewer r1 "$PROVIDER" "$MODEL" "$V1"
echo "${C_DIM}  → reviewer 1 ($MODEL): $V1${C_OFF}"

echo "${C_DIM}── reviewer 2: $PROVIDER2/$MODEL2 ─────────────────────────────${C_OFF}"
REVIEW2="$(run_review "$PROVIDER2" "$MODEL2")"
printf '%s\n' "$REVIEW2"
V2="$(printf '%s' "$REVIEW2" | verdict_of)"
log_reviewer r2 "$PROVIDER2" "$MODEL2" "$V2"
echo "${C_DIM}  → reviewer 2 ($MODEL2): $V2${C_OFF}"

if [ "$V1" = UNCLEAR ] || [ "$V2" = UNCLEAR ]; then
  echo "${C_DIM}  (an UNCLEAR verdict means no VERDICT line parsed — reviewer may have erred; stderr: $ERRLOG)${C_OFF}"
fi

# --- panel verdict ---------------------------------------------------------
OVERRIDE_BOOL=false; [ "${MU_REVIEW_OVERRIDE:-}" = "1" ] && OVERRIDE_BOOL=true

if [ "$V1" = APPROVE ] && [ "$V2" = APPROVE ]; then
  log_panel PASS false
  echo "${C_GREEN}ai-review: PANEL PASS — both reviewers APPROVE ($MODEL + $MODEL2).${C_OFF}"
  exit 0
fi

if [ "$V1" = REJECT ] && [ "$V2" = REJECT ]; then
  # Unanimous block: a real design/correctness call.
  if [ "$OVERRIDE_BOOL" = true ]; then
    log_panel BLOCK true
    echo "${C_YEL}ai-review: PANEL BLOCK ($MODEL=REJECT, $MODEL2=REJECT) overridden by operator (MU_REVIEW_OVERRIDE=1). Logged.${C_OFF}"
    exit 0
  fi
  log_panel BLOCK false
  echo "${C_RED}ai-review: PANEL BLOCK — both reviewers REJECT ($MODEL=REJECT, $MODEL2=REJECT). Set MU_REVIEW_OVERRIDE=1 to proceed if you disagree.${C_OFF}" >&2
  exit 1
fi

# Split (one APPROVE one REJECT) or any UNCLEAR: escalate to the operator.
# The panel does NOT self-override a split — that is the operator's call.
if [ "$OVERRIDE_BOOL" = true ]; then
  log_panel ESCALATE true
  echo "${C_YEL}ai-review: PANEL SPLIT → ESCALATE ($MODEL=$V1, $MODEL2=$V2) overridden by operator (MU_REVIEW_OVERRIDE=1). Logged.${C_OFF}"
  exit 0
fi
log_panel ESCALATE false
echo "${C_YEL}ai-review: PANEL SPLIT → ESCALATE — reviewers disagree: $MODEL=$V1, $MODEL2=$V2.${C_OFF}" >&2
echo "${C_YEL}  → operator decision required. This is NOT a unanimous block; do not self-override.${C_OFF}" >&2
echo "${C_DIM}  Set MU_REVIEW_OVERRIDE=1 to proceed once you've adjudicated.${C_OFF}" >&2
exit 3
