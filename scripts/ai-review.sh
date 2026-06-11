#!/usr/bin/env bash
# ai-review.sh — pre-PR review PANEL gate (beads mu-6qst, mu-ai-review-panel-lrwq, mu-f0ls).
#
# A reviewer panel checks the working diff before a PR — a check on top of CI and
# the human/agent. Run it via `just ci-aipr`, which runs `just ci` first and only
# reviews green code.
#
# PANEL SHAPE (mu-f0ls): TWO primaries + a conditional TIEBREAKER.
#   Primary 1 (local):  qwen3-coder-next-agent262k over ollama — 3-GPU local
#                       agentic/code reviewer, 262k context, temp 0.6.
#   Primary 2:          deepseek-v4-pro over openrouter — frontier-ish and cheap.
#   Tiebreaker:         Claude over anthropic-api — invoked ONLY when the two
#                       primaries disagree, so the Anthropic key/cost is reserved
#                       for actual ties.
#
# WHY THIS SHAPE: the previous panel ran two LOCAL models co-resident (qwen +
# gpt-oss:20b) and dead-ended a split at the operator. gpt-oss@49152 truncated its
# review on larger diffs before the VERDICT line — a model that can't return a
# complete verdict adds no signal — and it over-flags (high recall, low precision),
# the wrong failure mode for a primary, where every noisy REJECT would drag a good
# PR to a tiebreak. So we drop the co-residency compromise: one PRECISE local
# primary that always answers, paired with gpt-5.5, and an INDEPENDENT third model
# (deepseek) to BREAK a genuine tie rather than bouncing every disagreement to the
# operator. gpt-oss keeps its Modelfile for the future review-gate v2 per-file
# worker role; it is just not a flat-panel primary. (co-residency bench: memory
# a721c14d; reviewers-as-team: d88e133e.)
#
# PANEL SEMANTICS (each verdict is read from reviewer STDOUT, NOT its process exit
# code — `mu ask` historically exits non-zero on a shutdown wart (mu-qc08) even on
# success, so the exit code is not load-bearing here):
#
#   both primaries APPROVE   → PASS     (exit 0)
#   both primaries REJECT    → BLOCK    (exit 1)  — a real design/correctness call
#   primaries disagree       → TIEBREAK: run deepseek; its verdict decides —
#         tiebreaker APPROVE → PASS     (exit 0)  — tiebroken
#         tiebreaker REJECT  → BLOCK    (exit 1)  — tiebroken
#         tiebreaker UNCLEAR → ESCALATE (exit 3)  — tie unbroken; operator decides
#   both primaries UNCLEAR   → ESCALATE (exit 3)  — no verdict to tiebreak (infra?)
#
# "disagree" INCLUDES the case where exactly one primary is UNCLEAR (no VERDICT
# line parsed): one real opinion + one missing is still a tie for deepseek to
# resolve. "both UNCLEAR" is held out — a tiebreaker breaks a tie between OPINIONS;
# with zero usable opinions there is nothing to break (likely a provider/infra
# fault), and a lone third model must not stand in for a dead panel. The non-pass
# paths have DISTINCT exit codes (BLOCK 1 vs ESCALATE 3) and verdict-naming
# messages. MU_REVIEW_OVERRIDE=1 is the operator's override on BLOCK *or* ESCALATE:
# it proceeds (exit 0) and is logged as a calibration signal.
#
# Design: ~/.claude-personal/notes/design-prepr-review-and-degradation-gate.md
# Process-layer auditors / correlation: bead mu-pr6r.
#
# Env:
#   Primary 1 (default ollama / qwen-rev):
#     MU_REVIEW_PROVIDER        provider (default: ollama)
#     MU_REVIEW_MODEL           model    (default: qwen3-coder-next-agent262k)
#   Primary 2 (default openrouter / deepseek-v4-pro):
#     MU_REVIEW_PROVIDER_2      provider (default: openrouter)
#     MU_REVIEW_MODEL_2         model    (default: deepseek/deepseek-v4-pro)
#   Tiebreaker (default anthropic-api / claude-sonnet-4-6; runs ONLY on a split):
#     MU_REVIEW_PROVIDER_3      provider (default: anthropic-api)
#     MU_REVIEW_MODEL_3         model    (default: claude-sonnet-4-6)
#   Shared:
#     MU_REVIEW_TOOLS           reviewer tools, e.g. "read,grep" (default: none, single-shot)
#     MU_REVIEW_BASE            base ref to diff against (default: main)
#     MU_REVIEW_FULL_FILES      1 = append full content of each changed file to the
#                               prompt so reviewers see definitions outside the diff
#                               window (default: 1; set 0 for diff-only)
#     MU_REVIEW_CONTEXT_MAX_BYTES  cap on appended full-file context (default: 200000)
#     MU_REVIEW_TIMEOUT         per-reviewer wall-clock cap, seconds (default: 600). Bounds a
#                               hung/slow model; reviewers run SEQUENTIALLY, so panel wall-clock
#                               is up to ~2x this (and up to ~3x when a split triggers the
#                               tiebreaker). 300 was too tight — a typical Claude/reasoning
#                               response (>5min) plus a possible ollama model reload (~2min)
#                               overran it, SIGTERMing the reviewer mid-stream before its final
#                               VERDICT line (spurious UNCLEAR).
#     MU_REVIEW_OVERRIDE=1      operator override: proceed despite BLOCK/ESCALATE (logged)
#     MU_REVIEW_SYSTEM_PROMPT   reviewer system-prompt file (default: ai-review-system-prompt.txt)
#     MU_REVIEW_LOG             event log (default: ~/.local/share/mu/review-events.jsonl)
#     MU_REVIEW_NO_COLOR        disable color
#
# The log carries every reviewer's verdict: one {"event":"reviewer",...} line per
# reviewer that RAN plus one {"event":"panel",...} summary with the outcome and all
# three slots (r3_verdict is "" when the tiebreaker did not run).

set -u
set -o pipefail

# Primary 1 is local ollama: free, reliable, a non-Claude opinion, warm on the box
# (24h keep-alive). It is a BAKED model tag so mu can avoid per-request sampling
# and context overrides on the Ollama/Anthropic wire. With the 3-GPU review host,
# qwen3-coder-next-agent262k fits at 262144 context, temp 0.6 and leaves headroom;
# it has been the stronger local AGENTIC/code-exploration lane than gpt-oss.
#
# Primary 2 defaults to deepseek-v4-pro over openrouter — frontier-ish, cheap,
# and independent of the local runner. Tiebreaker is Claude over anthropic-api,
# invoked only on a primary split so the Anthropic key/cost is used as the final
# adjudicator, not the routine second opinion.
# Bench provenance: ~/src/public_github/code-review-bench/reports/NOTES.md.
PROVIDER="${MU_REVIEW_PROVIDER:-ollama}"
MODEL="${MU_REVIEW_MODEL:-qwen3-coder-next-agent262k}"
PROVIDER2="${MU_REVIEW_PROVIDER_2:-openrouter}"
MODEL2="${MU_REVIEW_MODEL_2:-deepseek/deepseek-v4-pro}"
PROVIDER3="${MU_REVIEW_PROVIDER_3:-anthropic-api}"
MODEL3="${MU_REVIEW_MODEL_3:-claude-sonnet-4-6}"
TOOLS="${MU_REVIEW_TOOLS:-}"   # empty = single-shot (default); e.g. "read,grep" lets the reviewer inspect surrounding code (slower, multi-turn)
BASE="${MU_REVIEW_BASE:-main}"
# Per-reviewer timeout: 2x a typical Claude response, with room for one ollama
# reload. The two reviewers run sequentially, so panel wall-clock is up to ~2x.
TIMEOUT="${MU_REVIEW_TIMEOUT:-600}"
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
  # Default sized to the reviewer models' context window, NOT to "as much
  # as possible": the panel models run at num_ctx=32768 (~100-130KB of
  # text), and ollama SILENTLY truncates an oversized prompt down to the
  # window — when the truncated prompt fills it, generation gets ~1 token
  # of budget and the reviewer emits a single word ("Based"/"Looking"),
  # finish_reason=length, exit 0. The old 200000 default did exactly that
  # to every FULL_FILES review on 2026-06-06: both reviewers UNCLEAR →
  # every PR escalated. 100000 bytes ≈ 25-30k tokens of context leaves
  # room for the diff + prompt + a real generated review. (mu-1mvq)
  _max="${MU_REVIEW_CONTEXT_MAX_BYTES:-100000}"
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
PROMPT="You are a strict pre-PR code reviewer. The DIFF below shows exactly what changed; review ONLY that change for: correctness bugs; concurrency / lifecycle hazards (e.g. a held reference that blocks shutdown, a clone that outlives its owner); missing error handling; and safeguards that nearby code already applies but this diff omits. The FULL CONTENT of each changed file is included after the diff so you can see definitions, helpers, and guards that live OUTSIDE the changed hunks — a variable or function used in the diff is often defined there, so CHECK the full content before reporting anything as undefined/unset, and do NOT raise findings about unchanged code. $TOOL_CLAUSE

Output contract:
- Do not narrate your review process or repeat the prompt.
- Report at most 5 findings; omit low-confidence concerns.
- If there is no blocking correctness/security/lifecycle issue in this diff, say so briefly.
- Keep the review under 1200 words.
- Your reply's LAST line MUST be exactly 'VERDICT: APPROVE' or 'VERDICT: REJECT' (those literal words). Do not continue after the verdict line.

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
    timeout "$TIMEOUT" "$MU" ask --bare --provider "$1" --model "$2" --thinking low $SYS_FLAGS --tools "$TOOLS" "$PROMPT" 2>>"$ERRLOG"
  else
    timeout "$TIMEOUT" "$MU" ask --bare --provider "$1" --model "$2" --thinking low $SYS_FLAGS "$PROMPT" 2>>"$ERRLOG"
  fi
}
verdict_of() { # stdin -> APPROVE | REJECT | UNCLEAR
  local out last; out="$(cat)"
  # The verdict is the reviewer's LAST line ("VERDICT: APPROVE"/"REJECT" per the
  # prompt). Parse only the LAST VERDICT-bearing line, not the whole output:
  # reviewers sometimes QUOTE the opposite token earlier while explaining the
  # format, and grepping the whole output mis-classifies those. Observed live
  # 2026-06-06: a reviewer ending in 'VERDICT: APPROVE' but quoting
  # '"VERDICT: REJECT"' mid-prose was read as REJECT, producing a false panel
  # split. Fall back to the whole output if no line mentions VERDICT.
  # (bead mu-pnqr)
  last="$(printf '%s\n' "$out" | grep -iE 'VERDICT' | tail -n 1)"
  [ -n "$last" ] || last="$out"
  # Tolerate markdown-dressed verdicts ("**Verdict:** APPROVE") — models flake
  # on the literal format; up to a few non-letter chars may sit between VERDICT
  # and the word (observed live 2026-06-05, was UNCLEAR).
  if   printf '%s' "$last" | grep -qiE 'VERDICT[^A-Za-z]{1,8}REJECT';  then echo REJECT
  elif printf '%s' "$last" | grep -qiE 'VERDICT[^A-Za-z]{1,8}APPROVE'; then echo APPROVE
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
  # Carries all three slots. r3_verdict is "" when the tiebreaker did not run
  # (the primaries agreed) — dashboards detect a tiebreak by r3_verdict != "".
  mkdir -p "$(dirname "$LOG")" 2>/dev/null || true
  printf '{"ts":"%s","event":"panel","outcome":"%s","r1_provider":"%s","r1_model":"%s","r1_verdict":"%s","r2_provider":"%s","r2_model":"%s","r2_verdict":"%s","r3_provider":"%s","r3_model":"%s","r3_verdict":"%s","base":"%s","files_changed":%s,"override":%s}\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$1" \
    "$(json_escape "$PROVIDER")"  "$(json_escape "$MODEL")"  "$(json_escape "$V1")" \
    "$(json_escape "$PROVIDER2")" "$(json_escape "$MODEL2")" "$(json_escape "$V2")" \
    "$(json_escape "$PROVIDER3")" "$(json_escape "$MODEL3")" "$(json_escape "$V3")" \
    "$(json_escape "$BASE")" "$FILES" "$2" >> "$LOG"
}

# --- run the panel: two primaries, same diff, sequentially -----------------
echo "${C_DIM}ai-review: PANEL reviewing $FILES file(s) vs $BASE — primaries: $PROVIDER/$MODEL + $PROVIDER2/$MODEL2 (tiebreaker $PROVIDER3/$MODEL3 on split)${C_OFF}"

echo "${C_DIM}── primary 1: $PROVIDER/$MODEL ─────────────────────────────${C_OFF}"
REVIEW1="$(run_review "$PROVIDER" "$MODEL")"
printf '%s\n' "$REVIEW1"
V1="$(printf '%s' "$REVIEW1" | verdict_of)"
log_reviewer r1 "$PROVIDER" "$MODEL" "$V1"
echo "${C_DIM}  → primary 1 ($MODEL): $V1${C_OFF}"

echo "${C_DIM}── primary 2: $PROVIDER2/$MODEL2 ─────────────────────────────${C_OFF}"
REVIEW2="$(run_review "$PROVIDER2" "$MODEL2")"
printf '%s\n' "$REVIEW2"
V2="$(printf '%s' "$REVIEW2" | verdict_of)"
log_reviewer r2 "$PROVIDER2" "$MODEL2" "$V2"
echo "${C_DIM}  → primary 2 ($MODEL2): $V2${C_OFF}"

V3=""   # set only if the tiebreaker runs; kept in the panel log either way
OVERRIDE_BOOL=false; [ "${MU_REVIEW_OVERRIDE:-}" = "1" ] && OVERRIDE_BOOL=true

# --- primaries agree: short-circuit (no tiebreaker / openrouter call) ------
if [ "$V1" = APPROVE ] && [ "$V2" = APPROVE ]; then
  log_panel PASS false
  echo "${C_GREEN}ai-review: PANEL PASS — both primaries APPROVE ($MODEL + $MODEL2).${C_OFF}"
  exit 0
fi
if [ "$V1" = REJECT ] && [ "$V2" = REJECT ]; then
  if [ "$OVERRIDE_BOOL" = true ]; then
    log_panel BLOCK true
    echo "${C_YEL}ai-review: PANEL BLOCK ($MODEL=REJECT, $MODEL2=REJECT) overridden by operator (MU_REVIEW_OVERRIDE=1). Logged.${C_OFF}"
    exit 0
  fi
  log_panel BLOCK false
  echo "${C_RED}ai-review: PANEL BLOCK — both primaries REJECT ($MODEL=REJECT, $MODEL2=REJECT). Set MU_REVIEW_OVERRIDE=1 to proceed if you disagree.${C_OFF}" >&2
  exit 1
fi

# --- both primaries UNCLEAR: no verdict to tiebreak — escalate -------------
# A tiebreaker breaks a tie between OPINIONS. If NEITHER primary produced a
# verdict (likely infra: provider auth, model-reload overrun, truncation), there
# is nothing to break; surfacing ESCALATE is more honest than letting a lone
# third model stand in for a dead panel.
if [ "$V1" = UNCLEAR ] && [ "$V2" = UNCLEAR ]; then
  echo "${C_DIM}  (both primaries UNCLEAR — no VERDICT line parsed; likely a provider/infra fault. stderr: $ERRLOG)${C_OFF}"
  if [ "$OVERRIDE_BOOL" = true ]; then
    log_panel ESCALATE true
    echo "${C_YEL}ai-review: PANEL ESCALATE (both primaries UNCLEAR) overridden by operator (MU_REVIEW_OVERRIDE=1). Logged.${C_OFF}"
    exit 0
  fi
  log_panel ESCALATE false
  echo "${C_YEL}ai-review: PANEL ESCALATE — both primaries returned no verdict ($MODEL=$V1, $MODEL2=$V2); not a tie to break. Check $ERRLOG.${C_OFF}" >&2
  echo "${C_DIM}  Set MU_REVIEW_OVERRIDE=1 to proceed once you've adjudicated.${C_OFF}" >&2
  exit 3
fi

# --- primaries disagree (split, or exactly one UNCLEAR): run the tiebreaker -
echo "${C_YEL}ai-review: primaries SPLIT ($MODEL=$V1, $MODEL2=$V2) → tiebreaker $PROVIDER3/$MODEL3${C_OFF}"
echo "${C_DIM}── tiebreaker: $PROVIDER3/$MODEL3 ─────────────────────────────${C_OFF}"
REVIEW3="$(run_review "$PROVIDER3" "$MODEL3")"
printf '%s\n' "$REVIEW3"
V3="$(printf '%s' "$REVIEW3" | verdict_of)"
log_reviewer r3 "$PROVIDER3" "$MODEL3" "$V3"
echo "${C_DIM}  → tiebreaker ($MODEL3): $V3${C_OFF}"

if [ "$V3" = APPROVE ]; then
  log_panel PASS false
  echo "${C_GREEN}ai-review: PANEL PASS (tiebroken) — primaries split $MODEL=$V1/$MODEL2=$V2, tiebreaker $MODEL3=APPROVE.${C_OFF}"
  exit 0
fi
if [ "$V3" = REJECT ]; then
  if [ "$OVERRIDE_BOOL" = true ]; then
    log_panel BLOCK true
    echo "${C_YEL}ai-review: PANEL BLOCK (tiebroken: $MODEL3=REJECT) overridden by operator (MU_REVIEW_OVERRIDE=1). Logged.${C_OFF}"
    exit 0
  fi
  log_panel BLOCK false
  echo "${C_RED}ai-review: PANEL BLOCK (tiebroken) — primaries split $MODEL=$V1/$MODEL2=$V2, tiebreaker $MODEL3=REJECT. Set MU_REVIEW_OVERRIDE=1 to proceed if you disagree.${C_OFF}" >&2
  exit 1
fi

# Tiebreaker itself returned no verdict: the tie is UNBROKEN — operator decides.
if [ "$OVERRIDE_BOOL" = true ]; then
  log_panel ESCALATE true
  echo "${C_YEL}ai-review: PANEL ESCALATE (tiebreaker UNCLEAR) overridden by operator (MU_REVIEW_OVERRIDE=1). Logged.${C_OFF}"
  exit 0
fi
log_panel ESCALATE false
echo "${C_YEL}ai-review: PANEL ESCALATE — primaries split ($MODEL=$V1, $MODEL2=$V2) and tiebreaker $MODEL3 returned no verdict.${C_OFF}" >&2
echo "${C_YEL}  → operator decision required. Set MU_REVIEW_OVERRIDE=1 to proceed once you've adjudicated.${C_OFF}" >&2
exit 3
