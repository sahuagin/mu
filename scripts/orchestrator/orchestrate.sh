#!/bin/sh
# orchestrate.sh — v0 pipeline spine (Plan A skeleton).
#
# Flow:  PLAN (seat) -> IMPLEMENT (worker, isolated jj workspace) -> REVIEW (ci-aipr)
#        -> ADJUDICATE (seat: SHIP | ITERATE <focus> | ESCALATE).
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
SEAT_PROVIDER="${SEAT_PROVIDER:-openai-codex}";    SEAT_MODEL="${SEAT_MODEL:-gpt-5.5}"            # the conductor under test
WORKER_PROVIDER="${WORKER_PROVIDER:-ollama}"; WORKER_MODEL="${WORKER_MODEL:-qwen3.6:27b}"  # coding rank-1 (free, local; claude needs the proxy, absent on this box)
AIREVIEW="${AIREVIEW:-$REPO_DIR/scripts/ai-review.sh}"
# Fixed, neutral system prompts (role + minimal tool-orientation) kept CONSTANT
# across seat arms so identity/recall don't confound the A/B (--bare strips the rest).
CONDUCTOR_PROMPT="${CONDUCTOR_PROMPT:-$(dirname "$0")/conductor-prompt.txt}"
WORKER_PROMPT="${WORKER_PROMPT:-$(dirname "$0")/worker-prompt.txt}"

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

TASK:
$TASK
EOF
SYSPROMPT="$CONDUCTOR_PROMPT"
( cd "$REPO_DIR" && dispatch plan "$SEAT_PROVIDER" "$SEAT_MODEL" "read,grep,ls" "$RUN_DIR/plan.prompt" )
cp "$RUN_DIR/plan.out" "$RUN_DIR/plan.md"
log "plan -> $RUN_DIR/plan.md"

# ── 2. IMPLEMENT (worker, isolated jj workspace) ─────────────────────────────
WS_LINE="$( cd "$REPO_DIR" && sprint-start --no-bead "orch-$(date -u +%H%M%S)" 2>/dev/null | sed -n 's/^cd //p' )"
WS="${WS_LINE:-$REPO_DIR}"
log "worker workspace: $WS"
cat > "$RUN_DIR/impl.prompt" <<EOF
You are the IMPLEMENTER. Follow the approved plan EXACTLY. Make the edits, then stop.
If the plan is wrong or blocked, STOP and write a one-line BLOCKER — do not improvise
beyond the plan's scope.

APPROVED PLAN:
$(cat "$RUN_DIR/plan.md")
EOF
# Worker now routes through the shared dispatch too: agent_dispatch is write-capable
# (maps the write/bash tools + adds --bash-yolo / --permission-mode bypassPermissions
# as needed), so a Claude editing worker (WORKER_PROVIDER=claude-oauth) would route
# through `claude -p` correctly. MAX_TURNS bumped for an implementation loop.
MAX_TURNS=40; SYSPROMPT="$WORKER_PROMPT"
( cd "$WS" && dispatch implement "$WORKER_PROVIDER" "$WORKER_MODEL" \
    "read,write,edit,glob,grep,ls,bash" "$RUN_DIR/impl.prompt" )
( cd "$WS" && jj diff --git > "$RUN_DIR/worker.diff" 2>/dev/null )
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
  echo "seat:   $SEAT_PROVIDER/$SEAT_MODEL"
  echo "worker: $WORKER_PROVIDER/$WORKER_MODEL"
  echo "workspace: $WS"
  echo "review exit: $REVIEW_RC"
  echo "$DECISION"
} | tee "$RUN_DIR/summary.md"
