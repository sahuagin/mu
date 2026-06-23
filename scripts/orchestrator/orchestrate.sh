#!/bin/sh
# orchestrate.sh — v0 pipeline spine (Plan A skeleton).
#
# Flow:  SPEC-CRITIC (request coherence: CLEAR | NEEDS-CLARIFICATION) -> ARCHITECT (invariant
#        guardian: GO | VETO) -> PLAN (seat) -> IMPLEMENT (worker(s), isolated jj workspace) ->
#        [CONVERGE: N>=2 competing workers -> select best] -> REVIEW (ci-aipr) -> ADJUDICATE
#        (seat: SHIP | ITERATE <focus> | ESCALATE). The SPEC-CRITIC halts on a contradictory/
#        ambiguous REQUEST before any work — the leverage point: catch the bad spec, not the bad
#        output (the #354 case: a request bundling "build cleanroom crate" + "cut mu-ai over"
#        under autonomy produced a fusion the operator rejected). A VETO short-circuits to the
#        operator; CONVERGE (CONVERGE_WORKERS>=2) fans out competing candidates.
#
# The SPEC-CRITIC reads the REQUEST and surfaces forks that would change the deliverable; the
# ARCHITECT reads the repo's invariants (AGENTS.md + specs/) and VETOes or briefs the planner.
# Gate seats default to gpt-5.5 (mu-rb4u fixed its empty-turns; one provider, no anthropic dep);
# the 2026-06-22 seat A/B found opus the deeper skeptic — override *_PROVIDER/*_MODEL for it.
#
# The SEAT is the orchestrator model under test. Default gpt-5.5; set
# SEAT_PROVIDER/SEAT_MODEL to claude-oauth/claude-opus-4-8 to run the other arm.
# We pass --provider/--model explicitly on every dispatch (no proxy, no reroute).
# Models mirror ~/.config/mu/agent_roles.toml; all artifacts land under RUN_DIR.
#
# usage: orchestrate.sh <task-file> <repo-dir>
set -u

TASK_FILE="${1:?usage: orchestrate.sh <task-file> <repo-dir>}"
REPO_DIR="${2:?usage: orchestrate.sh <task-file> <repo-dir>}"

# --- roles ---
# Gate seats default to gpt-5.5: mu-rb4u fixed its reasoning-only empty-turns, so the whole
# pipeline runs one provider with no anthropic dependency. The 2026-06-22 seat A/B found opus
# the DEEPER skeptic for gate roles — set the matching *_PROVIDER/*_MODEL to
# claude-oauth/claude-opus-4-8 to trade reliability for that depth.
SPEC_CRITIC_PROVIDER="${SPEC_CRITIC_PROVIDER:-openai-codex}"; SPEC_CRITIC_MODEL="${SPEC_CRITIC_MODEL:-gpt-5.5}"  # spec-critic gate (forks before code)
ARCHITECT_PROVIDER="${ARCHITECT_PROVIDER:-openai-codex}"; ARCHITECT_MODEL="${ARCHITECT_MODEL:-gpt-5.5}"  # invariant-guardian veto gate
SEAT_PROVIDER="${SEAT_PROVIDER:-openai-codex}";    SEAT_MODEL="${SEAT_MODEL:-gpt-5.5}"            # the conductor under test
WORKER_PROVIDER="${WORKER_PROVIDER:-ollama}"; WORKER_MODEL="${WORKER_MODEL:-qwen3.6:27b}"  # coding rank-1 (free, local; claude needs the proxy, absent on this box)
AIREVIEW="${AIREVIEW:-$REPO_DIR/scripts/ai-review.sh}"
SPEC_GATE="${SPEC_GATE:-1}"             # set 0 to skip the spec-critic gate (request-coherence check)
ARCHITECT_GATE="${ARCHITECT_GATE:-1}"   # set 0 to skip the architect gate (e.g. seat A/B isolation)
# CONVERGE: >=2 fans out N competing workers (each its own isolated workspace) on the SAME
# plan, then a CONVERGER selects the best candidate (antagonistic, citation-gated — the
# consensus discipline). Default 1 = single worker (behavior unchanged). Roster is
# provider:model per slot (diverse $0 coding ranks); slots beyond it fall back to WORKER_*.
CONVERGE_WORKERS="${CONVERGE_WORKERS:-1}"
CONVERGE_ROSTER="${CONVERGE_ROSTER:-ollama:qwen3.6:27b,claude-oauth:claude-sonnet-4-6,openai-codex:gpt-5.5}"
# The converger is a skeptical SELECTION gate (antagonistic, citation-gated). Defaults to
# gpt-5.5 like the other gates now that mu-rb4u fixed the codex empty-turn bug that used to
# return empty and silently default the pick; set claude-oauth/claude-opus-4-8 for opus depth.
CONVERGER_PROVIDER="${CONVERGER_PROVIDER:-openai-codex}"; CONVERGER_MODEL="${CONVERGER_MODEL:-gpt-5.5}"
# Fixed, neutral system prompts (role + minimal tool-orientation) kept CONSTANT
# across seat arms so identity/recall don't confound the A/B (--bare strips the rest).
CONDUCTOR_PROMPT="${CONDUCTOR_PROMPT:-$(dirname "$0")/conductor-prompt.txt}"
WORKER_PROMPT="${WORKER_PROMPT:-$(dirname "$0")/worker-prompt.txt}"
CONVERGE_PROMPT="${CONVERGE_PROMPT:-$(dirname "$0")/converge-prompt.txt}"
ARCHITECT_PROMPT="${ARCHITECT_PROMPT:-$(dirname "$0")/architect-prompt.txt}"
SPEC_CRITIC_PROMPT="${SPEC_CRITIC_PROMPT:-$(dirname "$0")/spec-critic-prompt.txt}"

RUN_DIR="${RUN_DIR:-$HOME/orchestrator-runs/run-$(date -u +%Y%m%dT%H%M%SZ)}"
mkdir -p "$RUN_DIR"
cp "$TASK_FILE" "$RUN_DIR/intent.md"
TASK="$(cat "$TASK_FILE")"

log(){ printf '[orchestrate] %s\n' "$*" >&2; }

# Model dispatch via the shared in-repo agent-dispatch lib (scripts/lib): routes
# claude-oauth -> `claude -p` (the $0 Max sub via the approved client), else ->
# `mu ask --bare`. It is write-capable (grants the mapped write/bash tools +
# --permission-mode bypassPermissions / --bash-yolo as needed), so all three
# stages — read-only seat (plan/adjudicate) and the write worker (implement) —
# share it. agent_dispatch reads TOOLS/SYSPROMPT/ERRLOG/MAX_TURNS from this scope.
. "${AGENT_DISPATCH_LIB:-$(dirname "$0")/../lib/agent-dispatch.sh}"
dispatch(){  # $1=label $2=provider $3=model $4=tools $5=prompt-file
  label="$1"; prov="$2"; model="$3"; TOOLS="$4"; pf="$5"
  ERRLOG="$RUN_DIR/$label.err"
  log "$label: $prov/$model (tools: ${TOOLS:-none})"
  agent_dispatch "$prov" "$model" "$pf" > "$RUN_DIR/$label.out" 2>>"$ERRLOG"
  rc=$?
  printf '{"label":"%s","provider":"%s","model":"%s","exit":%d}\n' \
     "$label" "$prov" "$model" "$rc" >> "$RUN_DIR/provenance.jsonl"
  return $rc
}

# ── 0a. SPEC-CRITIC (request-coherence gate) ─────────────────────────────────
# Runs FIRST, on the REQUEST itself (before invariants or planning): surfaces contradictions
# / material ambiguities that would make an autonomous worker pick a fork silently.
# NEEDS-CLARIFICATION halts to the operator — the #354 leverage point (catch the bad spec,
# not the bad output). Skippable with SPEC_GATE=0.
if [ "$SPEC_GATE" = 1 ]; then
  cat > "$RUN_DIR/spec-critic.prompt" <<EOF
Critique the REQUEST below for forks that would change the deliverable, BEFORE any plan is
written. You may read/grep the repo only to judge whether an ambiguity is real. Output your
forks and verdict in the required shape.

REQUEST:
$TASK
EOF
  SYSPROMPT="$SPEC_CRITIC_PROMPT"
  ( cd "$REPO_DIR" && dispatch spec-critic "$SPEC_CRITIC_PROVIDER" "$SPEC_CRITIC_MODEL" "read,grep,ls" "$RUN_DIR/spec-critic.prompt" )
  SPEC_VERDICT="$(grep -m1 '^VERDICT:' "$RUN_DIR/spec-critic.out" 2>/dev/null || echo 'VERDICT: (none parsed)')"
  log "spec-critic -> $SPEC_VERDICT"
  case "$SPEC_VERDICT" in
    "VERDICT: NEEDS-CLARIFICATION"*)
      { echo "# Run $RUN_DIR"
        echo "spec-critic: $SPEC_CRITIC_PROVIDER/$SPEC_CRITIC_MODEL"
        echo "$SPEC_VERDICT"
        echo "PIPELINE HALTED at the spec-critic gate — the request needs clarification before any work. See spec-critic.out for the forks."
      } | tee "$RUN_DIR/summary.md"
      exit 3 ;;
    "VERDICT: CLEAR"*) : ;;   # request is coherent — proceed
    *)
      log "spec-critic verdict unparsed — proceeding (review spec-critic.out)" ;;
  esac
fi

# ── 0b. ARCHITECT (invariant-guardian veto gate) ─────────────────────────────
# Runs BEFORE planning: reads the repo's invariant sources and either VETOes the task
# (short-circuit to the operator) or hands the planner the invariant brief. Skippable
# with ARCHITECT_GATE=0 (e.g. the seat A/B, which isolates the conductor stages).
INVARIANT_BRIEF=""
if [ "$ARCHITECT_GATE" = 1 ]; then
  cat > "$RUN_DIR/architect.prompt" <<EOF
Judge the task below against THIS repository's architecture invariants BEFORE any plan is
written. Read the invariant sources (the repo's AGENTS.md "Architecture invariants" section
and the specs/ that the task touches) with read/grep/ls — do not edit. Then output your
invariant brief and verdict in the required shape.

TASK:
$TASK
EOF
  SYSPROMPT="$ARCHITECT_PROMPT"
  ( cd "$REPO_DIR" && dispatch architect "$ARCHITECT_PROVIDER" "$ARCHITECT_MODEL" "read,grep,ls" "$RUN_DIR/architect.prompt" )
  ARCH_VERDICT="$(grep -m1 '^VERDICT:' "$RUN_DIR/architect.out" 2>/dev/null || echo 'VERDICT: (none parsed)')"
  log "architect -> $ARCH_VERDICT"
  case "$ARCH_VERDICT" in
    "VERDICT: VETO"*)
      { echo "# Run $RUN_DIR"
        echo "architect: $ARCHITECT_PROVIDER/$ARCHITECT_MODEL"
        echo "$ARCH_VERDICT"
        echo "PIPELINE HALTED at the architect gate — operator review required."
      } | tee "$RUN_DIR/summary.md"
      exit 3 ;;
    "VERDICT: GO"*)
      # Hand the planner everything the architect wrote ABOVE its verdict line (the brief).
      INVARIANT_BRIEF="$(sed '/^VERDICT:/,$d' "$RUN_DIR/architect.out")" ;;
    *)
      log "architect verdict unparsed — proceeding WITHOUT a brief (review architect.out)" ;;
  esac
fi

# ── 1. PLAN (seat) ───────────────────────────────────────────────────────────
cat > "$RUN_DIR/plan.prompt" <<EOF
You are the PLANNER. Read the repo (read/grep/ls only — do not edit) and produce a
STRUCTURED plan for the task below. Output exactly these sections, nothing else:

## Goal
## Affected files
## Invariants touched   (cite specs/ if relevant; "none" if none)
## Steps                (numbered, each a single concrete edit)
## Tests / verification  (how we'll know it worked)
## Risks
${INVARIANT_BRIEF:+
The ARCHITECT gate cleared this task with the invariant brief below. Your plan MUST honor it
— treat these as hard constraints, and reflect the relevant ones in "Invariants touched":
$INVARIANT_BRIEF
}
TASK:
$TASK
EOF
SYSPROMPT="$CONDUCTOR_PROMPT"
# The planner must investigate across the codebase before it can conclude; the dispatch default
# (15) is a GATE-sized budget and starves a thorough seat — mu-uvuo run #5: deepseek-v4-pro spent
# all 15 turns reading and was cancelled ONE turn before emitting the plan, so plan.md captured
# only its trailing "let me verify a few more details" line. Give the planner the worker's budget.
MAX_TURNS="${PLAN_MAX_TURNS:-40}"
( cd "$REPO_DIR" && dispatch plan "$SEAT_PROVIDER" "$SEAT_MODEL" "read,grep,ls" "$RUN_DIR/plan.prompt" )
cp "$RUN_DIR/plan.out" "$RUN_DIR/plan.md"
log "plan -> $RUN_DIR/plan.md"

# ── 2. IMPLEMENT (worker(s), isolated jj workspace) ──────────────────────────
# Shared implementer prompt — every worker gets the SAME approved plan.
cat > "$RUN_DIR/impl.prompt" <<EOF
You are the IMPLEMENTER. Follow the approved plan EXACTLY. Make the edits, then stop.
If the plan is wrong or blocked, STOP and write a one-line BLOCKER — do not improvise
beyond the plan's scope.

APPROVED PLAN:
$(cat "$RUN_DIR/plan.md")
EOF
# run_worker <slot> <provider> <model>: implement the plan in a FRESH isolated workspace;
# record the candidate diff (worker.<slot>.diff) + its workspace path (worker.<slot>.ws).
# agent_dispatch is write-capable (maps write/bash tools + --bash-yolo / bypassPermissions),
# so a Claude editing worker (provider=claude-oauth) routes through `claude -p` correctly.
run_worker(){  # $1=slot $2=provider $3=model
  rw_slot="$1"; rw_prov="$2"; rw_model="$3"
  rw_ws="$( cd "$REPO_DIR" && sprint-start --no-bead "orch-$(date -u +%H%M%S)-$rw_slot" 2>/dev/null | sed -n 's/^cd //p' )"
  rw_ws="${rw_ws:-$REPO_DIR}"
  printf '%s\n' "$rw_ws" > "$RUN_DIR/worker.$rw_slot.ws"
  MAX_TURNS="${WORKER_MAX_TURNS:-40}"; SYSPROMPT="$WORKER_PROMPT"  # WORKER_MAX_TURNS=0 ⇒ uncapped (safe for a free local worker; pair with a raised TIMEOUT)
  ( cd "$rw_ws" && dispatch "implement-$rw_slot" "$rw_prov" "$rw_model" \
      "read,write,edit,glob,grep,ls,bash" "$RUN_DIR/impl.prompt" )
  ( cd "$rw_ws" && jj diff --git > "$RUN_DIR/worker.$rw_slot.diff" 2>/dev/null )
  log "worker[$rw_slot] $rw_prov/$rw_model -> worker.$rw_slot.diff ($(wc -l < "$RUN_DIR/worker.$rw_slot.diff") lines), ws $rw_ws"
}

if [ "${CONVERGE_WORKERS:-1}" -le 1 ]; then
  run_worker 1 "$WORKER_PROVIDER" "$WORKER_MODEL"
  WS="$(cat "$RUN_DIR/worker.1.ws")"; cp "$RUN_DIR/worker.1.diff" "$RUN_DIR/worker.diff"
else
  # ── 2b. CONVERGE: fan out N competing workers, then SELECT the best ──────────
  i=1
  while [ "$i" -le "$CONVERGE_WORKERS" ]; do
    entry="$(printf '%s' "$CONVERGE_ROSTER" | cut -d, -f"$i")"
    if [ -z "$entry" ]; then rwp="$WORKER_PROVIDER"; rwm="$WORKER_MODEL"; else rwp="${entry%%:*}"; rwm="${entry#*:}"; fi
    run_worker "$i" "$rwp" "$rwm"
    i=$((i+1))
  done
  # Converger prompt: plan + (architect) invariant brief + each candidate's diff.
  {
    echo "APPROVED PLAN:"; cat "$RUN_DIR/plan.md"
    [ -n "${INVARIANT_BRIEF:-}" ] && { echo; echo "ARCHITECT INVARIANT BRIEF (hard constraints the winner must honor):"; echo "$INVARIANT_BRIEF"; }
    i=1
    while [ "$i" -le "$CONVERGE_WORKERS" ]; do
      echo; echo "===== CANDIDATE $i ====="
      echo '```diff'; tail -c 12000 "$RUN_DIR/worker.$i.diff"; echo '```'
      i=$((i+1))
    done
    echo; echo "Select the candidate that best satisfies the plan. End with 'WINNER: <n>' (or 'WINNER: none | <why>')."
  } > "$RUN_DIR/converge.prompt"
  SYSPROMPT="$CONVERGE_PROMPT"
  ( cd "$REPO_DIR" && dispatch converge "$CONVERGER_PROVIDER" "$CONVERGER_MODEL" "read,grep" "$RUN_DIR/converge.prompt" )
  WINNER="$(grep -m1 '^WINNER:' "$RUN_DIR/converge.out" 2>/dev/null | sed -E 's/^WINNER:[[:space:]]*([0-9]+).*/\1/')"
  case "$WINNER" in ''|*[!0-9]*) log "converge: WINNER unparsed or 'none' — defaulting to candidate 1 (review gate is the backstop); see converge.out"; WINNER=1 ;; esac
  WS="$(cat "$RUN_DIR/worker.$WINNER.ws")"; cp "$RUN_DIR/worker.$WINNER.diff" "$RUN_DIR/worker.diff"
  log "converge: WINNER=$WINNER -> $WS"
  # Abandon the loser sub-workspaces; keep the winner for REVIEW/ADJUDICATE.
  i=1
  while [ "$i" -le "$CONVERGE_WORKERS" ]; do
    if [ "$i" != "$WINNER" ]; then
      lws="$(cat "$RUN_DIR/worker.$i.ws" 2>/dev/null)"
      if [ -n "$lws" ] && [ "$lws" != "$REPO_DIR" ]; then
        ( cd "$lws" && sprint-end --force >/dev/null 2>&1 ); log "converge: abandoned loser candidate $i ($lws)"
      fi
    fi
    i=$((i+1))
  done
fi
log "worker diff -> $RUN_DIR/worker.diff ($(wc -l < "$RUN_DIR/worker.diff") lines)"

# ── 3. REVIEW (existing ci-aipr gate) ────────────────────────────────────────
if [ -s "$RUN_DIR/worker.diff" ] && [ -x "$AIREVIEW" ]; then
  ( cd "$WS" && MU_REVIEW_OVERRIDE=0 "$AIREVIEW" ) > "$RUN_DIR/review.out" 2>&1
  REVIEW_RC=$?
else
  echo "no diff or ai-review.sh not found — skipping review" > "$RUN_DIR/review.out"; REVIEW_RC=2
fi
log "review exit=$REVIEW_RC -> $RUN_DIR/review.out"

# ── 4. ADJUDICATE (seat) ─────────────────────────────────────────────────────
cat > "$RUN_DIR/adjudicate.prompt" <<EOF
You are the ORCHESTRATOR adjudicating one pipeline iteration. Do NOT re-implement anything
and do NOT do a full line-by-line re-review. You MAY read/grep the repo to VERIFY a specific
concern before raising it — NEVER flag a concern you have not checked against the actual code
(e.g. confirm a shell flag like 'set -e' is actually set before claiming it breaks something).
Read the plan, the diff stat, and the review gate's output, then output exactly ONE line:

  DECISION: SHIP
  DECISION: ITERATE | <one sentence of focus for the worker>
  DECISION: ESCALATE | <one sentence: why this needs the operator>

PLAN:
$(cat "$RUN_DIR/plan.md")

DIFF STAT:
$(cd "$WS" && jj diff --stat 2>/dev/null | tail -c 3000)

REVIEW GATE OUTPUT (exit=$REVIEW_RC):
$(tail -c 6000 "$RUN_DIR/review.out")
EOF
SYSPROMPT="$CONDUCTOR_PROMPT"
( cd "$WS" && dispatch adjudicate "$SEAT_PROVIDER" "$SEAT_MODEL" "read,grep" "$RUN_DIR/adjudicate.prompt" )
DECISION="$(grep -m1 '^DECISION:' "$RUN_DIR/adjudicate.out" 2>/dev/null || echo 'DECISION: (none parsed)')"

# ── summary ──────────────────────────────────────────────────────────────────
{
  echo "# Run $RUN_DIR"
  [ "$SPEC_GATE" = 1 ] && echo "spec-critic: $SPEC_CRITIC_PROVIDER/$SPEC_CRITIC_MODEL — ${SPEC_VERDICT:-(skipped)}"
  [ "$ARCHITECT_GATE" = 1 ] && echo "architect: $ARCHITECT_PROVIDER/$ARCHITECT_MODEL — ${ARCH_VERDICT:-(skipped)}"
  echo "seat:   $SEAT_PROVIDER/$SEAT_MODEL"
  if [ "${CONVERGE_WORKERS:-1}" -gt 1 ]; then
    echo "converge: $CONVERGE_WORKERS workers [$CONVERGE_ROSTER] -> ${CONVERGER_PROVIDER}/${CONVERGER_MODEL} picked candidate ${WINNER:-?}"
  else
    echo "worker: $WORKER_PROVIDER/$WORKER_MODEL"
  fi
  echo "workspace: $WS"
  echo "review exit: $REVIEW_RC"
  echo "$DECISION"
} | tee "$RUN_DIR/summary.md"
