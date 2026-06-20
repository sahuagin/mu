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
#     MU_REVIEW_FALLBACK_PROVIDER  hosted provider primary-1 falls back to when
#                               the local ollama MODEL is NOT already resident
#                               (per /api/ps), so the gate never forces a model
#                               load/eviction. (default: openai-codex)
#     MU_REVIEW_FALLBACK_MODEL  hosted fallback model (default: gpt-5.5)
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
#   Chunked mode (review-gate v2 — beads mu-ja1x overflow detection, mu-u1it fan-out):
#     MU_REVIEW_SINGLE_SHOT_MAX_BYTES  cap on the assembled single-shot prompt, bytes
#                               (default 300000 ≈ 85k tokens at ~3.5 bytes/token —
#                               fits every panel model with headroom). At or under
#                               the cap the calibrated panel above runs untouched;
#                               over it the review is CHUNKED: one findings-only
#                               leaf per commit (primary 1's provider/model), then
#                               one synthesis verdict over all findings.
#     MU_REVIEW_HEAD            head rev of the review range (default: @ under jj,
#                               HEAD under git). Pins BASE..HEAD so a branch other
#                               than the checkout can be reviewed without moving
#                               the working copy. When set, full-file context is
#                               skipped (on-disk files belong to @, not HEAD).
#     MU_REVIEW_SYNTH_PROVIDER  synthesis provider (default: primary 2's)
#     MU_REVIEW_SYNTH_MODEL     synthesis model    (default: primary 2's)
#
# The log carries every reviewer's verdict: one {"event":"reviewer",...} line per
# reviewer that RAN plus one {"event":"panel",...} summary with the outcome and all
# three slots (r3_verdict is "" when the tiebreaker did not run). The panel line
# carries "mode":"single_shot"|"chunked" so dashboards can tell the paths apart.
# Chunked mode additionally writes one {"event":"leaf",...} line per leaf that
# returned usable findings and one {"event":"leaf_error",...} per leaf that did not.

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
MODEL="${MU_REVIEW_MODEL:-qwen3.6:27b}"
PROVIDER2="${MU_REVIEW_PROVIDER_2:-openrouter}"
MODEL2="${MU_REVIEW_MODEL_2:-deepseek/deepseek-v4-pro}"
# reviewer-3 (tiebreaker) = claude-sonnet-4-6 via the Max SUBSCRIPTION
# (claude-oauth → `claude -p`, handled in run_review). Was anthropic-api, which
# is per-token AND operator-deactivated, so the tiebreaker was failing every run.
PROVIDER3="${MU_REVIEW_PROVIDER_3:-claude-oauth}"
MODEL3="${MU_REVIEW_MODEL_3:-claude-sonnet-4-6}"
# Hosted reviewer that primary-1 falls back to when the local ollama model
# isn't safe to use (a DIFFERENT model is resident, or ollama is unreachable).
# See ensure_local_reviewer_loaded below. gpt-5.5 is the decided paired reviewer.
FALLBACK_PROVIDER="${MU_REVIEW_FALLBACK_PROVIDER:-openai-codex}"
FALLBACK_MODEL="${MU_REVIEW_FALLBACK_MODEL:-gpt-5.5}"
# Read-only tools ON by default: a reviewer that can `read`/`grep` checks a
# definition instead of hallucinating "undefined X", and can inspect the
# COMPLEMENT of the diff (what a change omitted) — the two blind spots that let
# whole-artifact defects (the Anthropic-core rewrite class) pass a diff-only
# review. This is a correctness lever; the extra wall-clock is irrelevant next
# to the cost of a missed rewrite. Set MU_REVIEW_TOOLS="" to force single-shot.
TOOLS="${MU_REVIEW_TOOLS:-read,grep}"
# Turn cap is an anti-FLAIL backstop (stop a model looping forever), NOT a
# throttle — set generous so it never truncates a legitimate investigation;
# TIMEOUT is the wall-clock backstop. Forwarded to `mu ask --max-turns`.
MAX_TURNS="${MU_REVIEW_MAX_TURNS:-15}"
BASE="${MU_REVIEW_BASE:-main}"
# Chunked-mode knobs (v2). Synthesis defaults to primary 2: the strong/cheap
# frontier lane is the right place for the one cross-commit judgement call.
SS_MAX="${MU_REVIEW_SINGLE_SHOT_MAX_BYTES:-300000}"
SYNTH_PROVIDER="${MU_REVIEW_SYNTH_PROVIDER:-$PROVIDER2}"
SYNTH_MODEL="${MU_REVIEW_SYNTH_MODEL:-$MODEL2}"
# Per-reviewer timeout: generous because tool-using reviews are multi-turn and
# thoroughness beats wall-clock for a correctness gate (a slow correct verdict
# >> a fast wrong one). The two reviewers run sequentially. Bumped from 600 to
# give read/grep investigation room without SIGTERM'ing mid-check (a truncated
# reviewer returns UNCLEAR — the failure mode we're eliminating).
TIMEOUT="${MU_REVIEW_TIMEOUT:-900}"
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
# HEADREV pins the far end of the range (MU_REVIEW_HEAD). The default — @ / HEAD
# — is byte-equivalent to the old unpinned diff; a pinned head lets the gate
# review another branch (e.g. a large historical branch, for chunked mode)
# without touching the working copy.
if command -v jj >/dev/null 2>&1 && jj root >/dev/null 2>&1; then
  IS_JJ=1
  HEADREV="${MU_REVIEW_HEAD:-@}"
  DIFF="$(jj diff --from "$BASE" --to "$HEADREV" --git 2>/dev/null)"
else
  IS_JJ=""
  HEADREV="${MU_REVIEW_HEAD:-HEAD}"
  DIFF="$(git diff "$BASE...$HEADREV" 2>/dev/null)"
fi
# All-whitespace check via grep, NOT bash pattern substitution:
# `${DIFF//[[:space:]]/}` is quadratic in the string length and burned
# 10+ MINUTES of pure CPU on a ~1MB diff before the first reviewer ever
# ran (mu-ai-review-quadratic-diff-emptycheck-4v89). Herestring, NOT a
# `printf | grep -q` pipeline: under `set -o pipefail`, grep -q's
# early exit SIGPIPEs the printf (status 141) and a NON-empty diff
# reads as empty.
if ! grep -q '[^[:space:]]' <<<"$DIFF"; then
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
# Skipped when MU_REVIEW_HEAD is set: the on-disk files belong to the working
# copy, not the pinned head — appending them would hand reviewers the WRONG
# definitions (worse than none).
CONTEXT=""
if [ "${MU_REVIEW_FULL_FILES:-1}" = "1" ] && [ -z "${MU_REVIEW_HEAD:-}" ]; then
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
# Prefer the DEBUG binary: it is the one the build line above just
# refreshed. Preferring release here once handed the panel a weeks-old
# release build that lacked a flag the script passed — every reviewer
# died at clap with UNCLEAR (2026-06-11). Release is only a fallback.
cargo build --bin mu -q 2>/dev/null || true
if   [ -x ./target/debug/mu ];   then MU=./target/debug/mu
elif [ -x ./target/release/mu ]; then MU=./target/release/mu
else MU="$(command -v mu || true)"; fi
[ -n "${MU:-}" ] || { echo "${C_RED}ai-review: no mu binary found${C_OFF}" >&2; exit 2; }

# ── Local-reviewer pre-flight: never trigger an ollama model reload ─────────
# The local ollama reviewer (primary-1 in single-shot, the per-commit leaf in
# chunked) reads $PROVIDER/$MODEL — so resolving them here covers both modes.
# A cold load of the 262k reviewer is minutes, and that reload has SIGTERM'd
# reviewers mid-stream before (see the MU_REVIEW_TIMEOUT note above). So decide
# from ollama's /api/ps what's actually resident:
#   - same model already loaded  -> run local (no delay)
#   - box reachable but empty     -> run local (a load evicts nobody)
#   - a DIFFERENT model loaded    -> fall back (don't evict it / eat the reload)
#   - ollama unreachable          -> fall back (can't run local against a dead box)
# Match is tag-tolerant (ollama reports ':latest'). Synthesis (SYNTH_*) is
# hosted by default and is not checked here.
ensure_local_reviewer_loaded() {
  [ "$PROVIDER" = ollama ] || return 0
  local base want body loaded shown
  base="${OLLAMA_API_BASE:-http://10.1.1.143:11434}"
  want="$MODEL"; case "$want" in *:*) : ;; *) want="$want:latest" ;; esac
  if ! body="$(curl -s --max-time 5 "$base/api/ps" 2>/dev/null)"; then
    echo "${C_DIM}ai-review: ollama unreachable at $base; primary-1 -> $FALLBACK_PROVIDER/$FALLBACK_MODEL.${C_OFF}" >&2
    PROVIDER="$FALLBACK_PROVIDER"; MODEL="$FALLBACK_MODEL"; return 0
  fi
  loaded="$(printf '%s' "$body" | grep -o '"name":"[^"]*"' | sed 's/^"name":"//; s/"$//')"
  [ -z "$loaded" ] && return 0                      # reachable + empty -> safe load
  printf '%s\n' "$loaded" | grep -qxF "$want" && return 0   # same model resident
  shown="${loaded//$'\n'/, }"
  echo "${C_DIM}ai-review: ollama has a different model resident at $base (loaded: $shown); primary-1 -> $FALLBACK_PROVIDER/$FALLBACK_MODEL to avoid an eviction/reload.${C_OFF}" >&2
  PROVIDER="$FALLBACK_PROVIDER"; MODEL="$FALLBACK_MODEL"
}
ensure_local_reviewer_loaded

if [ -n "$TOOLS" ]; then
  TOOL_CLAUSE="Use the read and grep tools to inspect surrounding code when a judgement needs it."
else
  TOOL_CLAUSE="Review the diff exactly as given below — do NOT call any tools and do NOT emit any function-call or tool-call syntax; respond with prose only."
fi

# Architecture-invariant conformance: the operator's rules live in the repo's
# AGENTS.md "## Architecture invariants" section. Feed them to the reviewer so
# EVERY review checks the change against the declared invariants — the
# whole-artifact defect class (event-log-first, capability representation, ...)
# that a hunk-level review structurally cannot see. Empty when AGENTS.md absent.
INVARIANTS=""
if [ -r "$ROOT/AGENTS.md" ]; then
  INVARIANTS="$(awk '/^## Architecture invariants/{f=1} /^## /{if(f && !/^## Architecture invariants/) exit} f' "$ROOT/AGENTS.md")"
fi
INVARIANTS_CLAUSE=""; INVARIANTS_BLOCK=""
if [ -n "$INVARIANTS" ]; then
  INVARIANTS_CLAUSE=" ALSO check the change against the project ARCHITECTURE INVARIANTS shown below: a diff that violates one — or moves the code toward violating it — is a finding even when every line is locally correct; use read/grep to confirm a suspected violation before reporting it."
  INVARIANTS_BLOCK="
PROJECT ARCHITECTURE INVARIANTS (the change MUST uphold these):
$INVARIANTS
"
fi
PROMPT="You are a strict pre-PR code reviewer. The DIFF below shows exactly what changed; review ONLY that change for: correctness bugs; concurrency / lifecycle hazards (e.g. a held reference that blocks shutdown, a clone that outlives its owner); missing error handling; and safeguards that nearby code already applies but this diff omits. The FULL CONTENT of each changed file is included after the diff so you can see definitions, helpers, and guards that live OUTSIDE the changed hunks — a variable or function used in the diff is often defined there, so CHECK the full content before reporting anything as undefined/unset, and do NOT raise findings about unchanged code.$INVARIANTS_CLAUSE $TOOL_CLAUSE

Output contract:
- Do not narrate your review process or repeat the prompt.
- Report at most 5 findings; omit low-confidence concerns.
- If there is no blocking correctness/security/lifecycle issue in this diff, say so briefly.
- Keep the review under 1200 words.
- Your reply's LAST line MUST be exactly 'VERDICT: APPROVE' or 'VERDICT: REJECT' (those literal words). Do not continue after the verdict line.
$INVARIANTS_BLOCK
DIFF:
$DIFF
$CONTEXT"

# The prompt goes to `mu ask` via --prompt-file, NEVER argv: a
# megabyte-scale prompt as an exec argument overflows ARG_MAX and the
# reviewer dies before it starts ("/bin/timeout: Argument list too
# long" — mu-b6tl, observed live on a ~1MB review prompt 2026-06-11).
PROMPT_FILE="$(mktemp "${TMPDIR:-/tmp}/ai-review-prompt.XXXXXX")"
trap 'rm -f "$PROMPT_FILE"' EXIT
printf '%s' "$PROMPT" > "$PROMPT_FILE"

run_review() { # $1=provider $2=model [$3=prompt-file, default $PROMPT_FILE] — prints reviewer stdout; stderr -> $ERRLOG
  # The reviewer session must be hermetic: --bare (PR #187) guarantees
  # mu injects nothing — no session-start memory/project-file recall,
  # no discovery bootstrap — so the session's system prompt is exactly
  # the minimal reviewer prompt below (and nothing at all if the file
  # is missing). Replaces the MU_NO_RECALL=1 env spelling from #185.
  # shellcheck disable=SC2086 — $SYS_FLAGS intentionally word-splits
  local PF="${3:-$PROMPT_FILE}"   # chunked mode passes leaf/synthesis prompt files
  # claude subscription reviewer: `mu ask` has no path to the Max sub —
  # anthropic-api is per-token (and the operator deactivated it), and the
  # OAuth provider is unimplemented in mu. So reach the $0 sub by shelling to
  # `claude -p`. Prompt on STDIN, not argv: the review prompt can be ~1MB and
  # would overflow ARG_MAX (mu-b6tl). Read-only tools (Read,Grep) mirror the
  # mu-path TOOLS default; --exclude-dynamic-system-prompt-sections strips
  # claude's agent scaffolding so the reviewer sees only $SYSPROMPT + the diff.
  if [ "$1" = "claude-oauth" ]; then
    local CL_SYS=""
    [ -r "$SYSPROMPT" ] && CL_SYS="--append-system-prompt-file $SYSPROMPT"
    # shellcheck disable=SC2086 — $CL_SYS intentionally word-splits
    timeout "$TIMEOUT" claude -p --model "$2" $CL_SYS \
      --exclude-dynamic-system-prompt-sections \
      --allowedTools Read Grep --output-format text <"$PF" 2>>"$ERRLOG"
    return
  fi
  SYS_FLAGS=""
  [ -r "$SYSPROMPT" ] && SYS_FLAGS="--append-system-prompt $SYSPROMPT"
  if [ -n "$TOOLS" ]; then
    timeout "$TIMEOUT" "$MU" ask --bare --provider "$1" --model "$2" --thinking low $SYS_FLAGS --max-turns "$MAX_TURNS" --tools "$TOOLS" --prompt-file "$PF" 2>>"$ERRLOG"
  else
    timeout "$TIMEOUT" "$MU" ask --bare --provider "$1" --model "$2" --thinking low $SYS_FLAGS --prompt-file "$PF" 2>>"$ERRLOG"
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
  printf '{"ts":"%s","event":"panel","mode":"single_shot","outcome":"%s","r1_provider":"%s","r1_model":"%s","r1_verdict":"%s","r2_provider":"%s","r2_model":"%s","r2_verdict":"%s","r3_provider":"%s","r3_model":"%s","r3_verdict":"%s","base":"%s","files_changed":%s,"override":%s}\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$1" \
    "$(json_escape "$PROVIDER")"  "$(json_escape "$MODEL")"  "$(json_escape "$V1")" \
    "$(json_escape "$PROVIDER2")" "$(json_escape "$MODEL2")" "$(json_escape "$V2")" \
    "$(json_escape "$PROVIDER3")" "$(json_escape "$MODEL3")" "$(json_escape "$V3")" \
    "$(json_escape "$BASE")" "$FILES" "$2" >> "$LOG"
}

# ── CHUNKED MODE (review-gate v2: beads mu-ja1x, mu-u1it) ───────────────────
#
# WHY: the single-shot panel dies on large branches — a ~1MB/12-commit branch
# overflowed every reviewer's context (one emitted 2 characters), and
# overflow-UNCLEAR is indistinguishable from substantive disagreement. So when
# the assembled single-shot prompt exceeds $SS_MAX the review splits BY COMMIT,
# never by file: a commit carries the author's stated intent, and review checks
# change-against-claim — a bare file slice has no claim attached. Each LEAF
# (primary 1: cheap, local) reports FINDINGS ONLY, no verdict; one SYNTHESIS
# pass (primary 2's lane: strong, cross-cutting) judges which findings are real
# and whether they interact across commits, and its verdict IS the gate verdict
# — the leaves are its eyes; no second panel vote in v1. A commit whose lone
# diff exceeds the cap is split per-file (same message, one file's diff per
# leaf). Failure honesty: a leaf that errors/times out/breaks the contract is
# logged as leaf_error and shown to synthesis as UNREVIEWED; if >1/3 of leaves
# fail, synthesis is SKIPPED and the gate ESCALATEs — it must not approve a
# mostly-unreviewed branch.

leaf_findings() { # stdin = raw leaf output -> <=5 FINDING| lines, or NO_FINDINGS, or "" (unusable)
  # Tolerate leading whitespace/markdown bullets around contract lines, but
  # nothing looser: output with neither token is unusable and the caller
  # records a leaf_error rather than guessing.
  local out f
  out="$(cat)"
  f="$(printf '%s\n' "$out" | sed -n 's/^[^A-Za-z]*\(FINDING|.*\)$/\1/p' | head -n 5)"
  if [ -n "$f" ]; then printf '%s\n' "$f"; return 0; fi
  if printf '%s' "$out" | grep -q 'NO_FINDINGS'; then echo NO_FINDINGS; fi
  return 0
}

log_leaf() { # $1=commit $2=unit-label $3=findings-count
  mkdir -p "$(dirname "$LOG")" 2>/dev/null || true
  printf '{"ts":"%s","event":"leaf","commit":"%s","unit":"%s","provider":"%s","model":"%s","findings":%s}\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$(json_escape "$1")" "$(json_escape "$2")" \
    "$(json_escape "$PROVIDER")" "$(json_escape "$MODEL")" "$3" >> "$LOG"
}

log_leaf_error() { # $1=commit $2=unit-label $3=reason
  mkdir -p "$(dirname "$LOG")" 2>/dev/null || true
  printf '{"ts":"%s","event":"leaf_error","commit":"%s","unit":"%s","provider":"%s","model":"%s","reason":"%s"}\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$(json_escape "$1")" "$(json_escape "$2")" \
    "$(json_escape "$PROVIDER")" "$(json_escape "$MODEL")" "$(json_escape "$3")" >> "$LOG"
}

log_panel_chunked() { # $1=outcome $2=override $3=synth-verdict $4=leaves $5=leaf-errors
  mkdir -p "$(dirname "$LOG")" 2>/dev/null || true
  printf '{"ts":"%s","event":"panel","mode":"chunked","outcome":"%s","leaf_provider":"%s","leaf_model":"%s","leaves":%s,"leaf_errors":%s,"synth_provider":"%s","synth_model":"%s","synth_verdict":"%s","base":"%s","head":"%s","files_changed":%s,"override":%s}\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$1" \
    "$(json_escape "$PROVIDER")" "$(json_escape "$MODEL")" "$4" "$5" \
    "$(json_escape "$SYNTH_PROVIDER")" "$(json_escape "$SYNTH_MODEL")" "$(json_escape "$3")" \
    "$(json_escape "$BASE")" "$(json_escape "$HEADREV")" "$FILES" "$2" >> "$LOG"
}

review_leaf() { # $1=commit $2=short-id $3=unit-label ("" = whole commit) $4=message $5=diff
  # Runs ONE leaf and folds its result into the caller's (run_chunked's)
  # accumulators — leaves/failed/findings_total/SYNTH_FINDINGS via bash
  # dynamic scoping, same pattern as verify_claims_step in pre-pr-check.sh.
  local c="$1" cshort="$2" unit="${3:-commit $2}" msg="$4" d="$5"
  leaves=$((leaves + 1))
  echo "${C_DIM}── leaf $leaves: $unit ($PROVIDER/$MODEL) ─────────────────${C_OFF}"
  {
    printf '%s\n' "You are one LEAF of a chunked pre-PR review: the branch is too large for a single review, so each commit is reviewed in isolation against its own stated intent, and a separate synthesis pass renders the verdict. Review ONLY the diff below for: correctness bugs; concurrency / lifecycle hazards; missing error handling; safeguards that nearby code in the diff applies but this change omits; and mismatches between the commit message's claims and the change. You see one unit — the branch context is orientation only; do NOT raise findings about code you cannot see, and do NOT call any tools."
    printf '%s\n' ""
    printf '%s\n' "Output contract (STRICT):"
    printf '%s\n' "- One line per finding: FINDING|<blocker|should-fix|note>|<file>|<one-line claim>"
    printf '%s\n' "- At most 5 findings, highest severity first; omit low-confidence concerns."
    printf '%s\n' "- If there is nothing worth reporting, output the single line: NO_FINDINGS"
    printf '%s\n' "- NO verdict line, NO narration, NOTHING else."
    printf '%s\n' ""
    printf '%s\n' "BRANCH COMMITS (orientation; you are reviewing $unit):"
    printf '%s\n' "$COMMIT_LIST"
    printf '%s\n' "TOTAL BRANCH DIFFSTAT:"
    printf '%s\n' "$DIFFSTAT"
    printf '%s\n' "UNIT UNDER REVIEW: $unit"
    printf '%s\n' "COMMIT MESSAGE:"
    printf '%s\n' "$msg"
    printf '%s\n' ""
    printf '%s\n' "DIFF:"
    printf '%s\n' "$d"
  } > "$LEAF_FILE"
  local out rc f
  out="$(run_review "$PROVIDER" "$MODEL" "$LEAF_FILE")"; rc=$?
  f="$(printf '%s' "$out" | leaf_findings)"
  if [ "$rc" -eq 124 ] || [ -z "$f" ]; then
    # mu ask's exit code is not load-bearing (mu-qc08) — only timeout's 124 is
    # trusted; otherwise "failed" means the output carried no contract lines.
    local reason="no contract output"
    [ "$rc" -eq 124 ] && reason="timeout after ${TIMEOUT}s"
    failed=$((failed + 1))
    log_leaf_error "$c" "$unit" "$reason"
    echo "${C_YEL}  → leaf FAILED ($reason) — recorded as unreviewed. stderr: $ERRLOG${C_OFF}"
    SYNTH_FINDINGS="$SYNTH_FINDINGS
$unit: REVIEW FAILED — treat as unreviewed"
    return 0
  fi
  local n=0
  [ "$f" != "NO_FINDINGS" ] && n="$(printf '%s\n' "$f" | grep -c .)"
  findings_total=$((findings_total + n))
  log_leaf "$c" "$unit" "$n"
  printf '%s\n' "$f"
  echo "${C_DIM}  → leaf $leaves: $n finding(s)${C_OFF}"
  SYNTH_FINDINGS="$SYNTH_FINDINGS
$unit — $(printf '%s' "$msg" | head -n 1):
$f"
}

run_chunked() { # never returns — exits with the gate verdict
  local LEAF_FILE SYNTH_FILE
  LEAF_FILE="$(mktemp "${TMPDIR:-/tmp}/ai-review-leaf.XXXXXX")"
  SYNTH_FILE="$(mktemp "${TMPDIR:-/tmp}/ai-review-synth.XXXXXX")"
  trap 'rm -f "$PROMPT_FILE" "$LEAF_FILE" "$SYNTH_FILE"' EXIT

  local ov=false
  [ "${MU_REVIEW_OVERRIDE:-}" = "1" ] && ov=true

  # Commits oldest-first — later leaves' "orientation" list reads naturally and
  # synthesis sees the branch as the author built it. Empty commits carry no
  # reviewable change (jj filters in the revset; git path re-checks per diff).
  local commits
  if [ -n "$IS_JJ" ]; then
    commits="$(jj log -r "$BASE..$HEADREV ~ empty()" --no-graph --reversed -T 'commit_id ++ "\n"' 2>/dev/null)"
  else
    commits="$(git rev-list --reverse "$BASE..$HEADREV" 2>/dev/null)"
  fi
  if ! grep -q '[^[:space:]]' <<<"$commits"; then
    echo "${C_RED}ai-review: chunked mode found no commits in $BASE..$HEADREV — cannot review${C_OFF}" >&2
    exit 2
  fi

  # Ambient context every leaf gets: the branch's whole shape, cheaply.
  if [ -n "$IS_JJ" ]; then
    COMMIT_LIST="$(jj log -r "$BASE..$HEADREV" --no-graph --reversed -T 'commit_id.short() ++ " " ++ description.first_line() ++ "\n"' 2>/dev/null)"
    DIFFSTAT="$(jj diff --from "$BASE" --to "$HEADREV" --stat 2>/dev/null | tail -c 6000)"
  else
    COMMIT_LIST="$(git log --reverse --format='%h %s' "$BASE..$HEADREV" 2>/dev/null)"
    DIFFSTAT="$(git diff --stat "$BASE...$HEADREV" 2>/dev/null | tail -c 6000)"
  fi

  local n_commits
  n_commits="$(printf '%s\n' "$commits" | grep -c .)"
  echo "${C_DIM}ai-review: CHUNKED mode — single-shot prompt ${PROMPT_BYTES}B > cap ${SS_MAX}B; $n_commits commit(s) in $BASE..$HEADREV. Leaves: $PROVIDER/$MODEL, synthesis: $SYNTH_PROVIDER/$SYNTH_MODEL.${C_OFF}"

  local leaves=0 failed=0 findings_total=0
  local SYNTH_FINDINGS="" ALL_MSGS=""
  local c cshort msg cdiff
  while IFS= read -r c; do
    [ -n "$c" ] || continue
    cshort="${c:0:12}"
    if [ -n "$IS_JJ" ]; then
      msg="$(jj log -r "$c" --no-graph -T description 2>/dev/null)"
      cdiff="$(jj diff -r "$c" --git 2>/dev/null)"
    else
      msg="$(git log -1 --format=%B "$c" 2>/dev/null)"
      cdiff="$(git show --format= "$c" 2>/dev/null)"
    fi
    ALL_MSGS="$ALL_MSGS
$msg"
    grep -q '[^[:space:]]' <<<"$cdiff" || continue
    if [ "$(printf '%s' "$cdiff" | wc -c)" -le "$SS_MAX" ]; then
      review_leaf "$c" "$cshort" "" "$msg" "$cdiff"
    else
      # One commit alone exceeds the cap: split per-file. The commit message
      # (the claim) rides along on every slice so each leaf still reviews
      # change-against-claim; the label tells it which slice it holds.
      local files nf i f fdiff
      files="$(printf '%s\n' "$cdiff" | sed -n 's#^diff --git a/.* b/##p')"
      nf="$(printf '%s\n' "$files" | grep -c .)"
      i=0
      while IFS= read -r f; do
        [ -n "$f" ] || continue
        i=$((i + 1))
        if [ -n "$IS_JJ" ]; then
          fdiff="$(jj diff -r "$c" --git -- "$f" 2>/dev/null)"
        else
          fdiff="$(git show --format= "$c" -- "$f" 2>/dev/null)"
        fi
        if [ "$(printf '%s' "$fdiff" | wc -c)" -gt "$SS_MAX" ]; then
          fdiff="$(printf '%s' "$fdiff" | head -c "$SS_MAX")
[diff truncated at ${SS_MAX} bytes]"
        fi
        review_leaf "$c" "$cshort" "file $i/$nf of commit $cshort: $f" "$msg" "$fdiff"
      done <<<"$files"
    fi
  done <<<"$commits"

  echo "${C_DIM}ai-review: $leaves leaf review(s) done — $findings_total finding(s), $failed failure(s)${C_OFF}"

  # Failure honesty: with >1/3 of leaves unreviewed, a synthesis verdict would
  # rest mostly on blind spots — name the infra failure and escalate instead.
  if [ "$failed" -gt 0 ] && [ $((failed * 3)) -gt "$leaves" ]; then
    if [ "$ov" = true ]; then
      log_panel_chunked ESCALATE true "" "$leaves" "$failed"
      echo "${C_YEL}ai-review: CHUNKED ESCALATE ($failed/$leaves leaf reviews failed) overridden by operator (MU_REVIEW_OVERRIDE=1). Logged.${C_OFF}"
      exit 0
    fi
    log_panel_chunked ESCALATE false "" "$leaves" "$failed"
    echo "${C_YEL}ai-review: CHUNKED ESCALATE — $failed of $leaves leaf reviews FAILED (leaf provider/infra fault, not a review opinion; check $ERRLOG). Synthesis skipped: it must not approve a mostly-unreviewed branch.${C_OFF}" >&2
    echo "${C_DIM}  Fix the leaf lane ($PROVIDER/$MODEL) and re-run, or set MU_REVIEW_OVERRIDE=1 once you've adjudicated.${C_OFF}" >&2
    exit 3
  fi

  # Spec inclusion: a commit that references a spec is judged against it. Read
  # from HEADREV, not the working copy — the spec usually lands IN the branch
  # under review, and @'s tree may predate (or postdate) it.
  local spec_ids spec_text="" id sf matches content
  spec_ids="$(printf '%s\n' "$ALL_MSGS" | grep -oE 'mu-[0-9]{3}' | sort -u || true)"
  for id in $spec_ids; do
    if [ -n "$IS_JJ" ]; then
      matches="$(jj file list -r "$HEADREV" -- specs/ 2>/dev/null | grep -E "^specs/${id}-[^/]*\.md$" || true)"
    else
      matches="$(git ls-tree -r --name-only "$HEADREV" -- specs/ 2>/dev/null | grep -E "^specs/${id}-[^/]*\.md$" || true)"
    fi
    for sf in $matches; do
      if [ -n "$IS_JJ" ]; then
        content="$(jj file show -r "$HEADREV" -- "$sf" 2>/dev/null)"
      else
        content="$(git show "$HEADREV:$sf" 2>/dev/null)"
      fi
      spec_text="$spec_text

===== SPEC: $sf =====
$content"
    done
  done
  if [ -n "$spec_text" ] && [ "$(printf '%s' "$spec_text" | wc -c)" -gt 60000 ]; then
    spec_text="$(printf '%s' "$spec_text" | head -c 60000)
[spec context truncated at 60000 bytes]"
  fi

  echo "${C_DIM}── synthesis: $SYNTH_PROVIDER/$SYNTH_MODEL ─────────────────────${C_OFF}"
  {
    printf '%s\n' "You are the SYNTHESIS reviewer of a chunked pre-PR review. The branch was too large for one review, so each commit was reviewed in isolation by a leaf reviewer; their findings are below, verbatim, in the form FINDING|<severity>|<file>|<claim>. You hold the only branch-wide view: judge which findings are REAL (leaves can be wrong — each saw one commit, and a later commit may already fix what an earlier leaf flagged) and whether any findings INTERACT across commits into a larger hazard no single commit shows. Units marked 'REVIEW FAILED — treat as unreviewed' carry unknown risk; weigh that. If a SPEC section is included, also judge whether the branch delivers what the spec claims. If PROJECT ARCHITECTURE INVARIANTS are included below, judge whether the branch upholds them — a violation (or a move toward one) is a REJECT-worthy finding even when each commit is locally correct."
    printf '%s\n' ""
    printf '%s\n' "Output contract:"
    printf '%s\n' "- Brief judgement on each finding you accept or reject (cite its unit); under 1200 words total."
    printf '%s\n' "- Your reply's LAST line MUST be exactly 'VERDICT: APPROVE' or 'VERDICT: REJECT' (those literal words). Do not continue after the verdict line."
    printf '%s\n' ""
    printf '%s\n' "BRANCH COMMITS (oldest first):"
    printf '%s\n' "$COMMIT_LIST"
    printf '%s\n' "TOTAL BRANCH DIFFSTAT:"
    printf '%s\n' "$DIFFSTAT"
    printf '%s\n' "$spec_text"
    printf '%s\n' "$INVARIANTS_BLOCK"
    printf '%s\n' ""
    printf '%s\n' "LEAF FINDINGS ($leaves units, $failed unreviewed):"
    printf '%s\n' "$SYNTH_FINDINGS"
  } > "$SYNTH_FILE"

  local SREV SV
  SREV="$(run_review "$SYNTH_PROVIDER" "$SYNTH_MODEL" "$SYNTH_FILE")"
  printf '%s\n' "$SREV"
  SV="$(printf '%s' "$SREV" | verdict_of)"
  log_reviewer synth "$SYNTH_PROVIDER" "$SYNTH_MODEL" "$SV"
  echo "${C_DIM}  → synthesis ($SYNTH_MODEL): $SV${C_OFF}"

  # Synthesis verdict IS the gate verdict (single reviewer; the leaves are its
  # eyes). Outcome/override/exit semantics mirror the single-shot panel.
  if [ "$SV" = APPROVE ]; then
    log_panel_chunked PASS false "$SV" "$leaves" "$failed"
    echo "${C_GREEN}ai-review: CHUNKED PASS — synthesis APPROVE over $leaves leaf review(s) ($findings_total finding(s), $failed unreviewed).${C_OFF}"
    exit 0
  fi
  if [ "$SV" = REJECT ]; then
    if [ "$ov" = true ]; then
      log_panel_chunked BLOCK true "$SV" "$leaves" "$failed"
      echo "${C_YEL}ai-review: CHUNKED BLOCK (synthesis $SYNTH_MODEL=REJECT) overridden by operator (MU_REVIEW_OVERRIDE=1). Logged.${C_OFF}"
      exit 0
    fi
    log_panel_chunked BLOCK false "$SV" "$leaves" "$failed"
    echo "${C_RED}ai-review: CHUNKED BLOCK — synthesis $SYNTH_MODEL=REJECT over $leaves leaf review(s). Set MU_REVIEW_OVERRIDE=1 to proceed if you disagree.${C_OFF}" >&2
    exit 1
  fi
  # Synthesis returned no verdict: nothing decided the gate — operator's call.
  if [ "$ov" = true ]; then
    log_panel_chunked ESCALATE true "$SV" "$leaves" "$failed"
    echo "${C_YEL}ai-review: CHUNKED ESCALATE (synthesis UNCLEAR) overridden by operator (MU_REVIEW_OVERRIDE=1). Logged.${C_OFF}"
    exit 0
  fi
  log_panel_chunked ESCALATE false "$SV" "$leaves" "$failed"
  echo "${C_YEL}ai-review: CHUNKED ESCALATE — synthesis $SYNTH_MODEL returned no verdict (check $ERRLOG). Set MU_REVIEW_OVERRIDE=1 to proceed once you've adjudicated.${C_OFF}" >&2
  exit 3
}

# ── MODE GATE (mu-ja1x): chunk only when the single-shot prompt cannot fit ──
# Bytes, not tokens: bytes/3.5 ≈ tokens, so the 300000B default ≈ 85k tokens —
# inside every panel model's window with headroom. At or under the cap the
# calibrated single-shot panel below runs EXACTLY as before (do not perturb
# it); over the cap, run_chunked() takes over and exits with the gate verdict.
PROMPT_BYTES=$(( $(printf '%s' "$PROMPT" | wc -c) ))
if [ "$PROMPT_BYTES" -gt "$SS_MAX" ]; then
  run_chunked
fi

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
