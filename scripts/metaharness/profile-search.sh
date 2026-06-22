#!/bin/sh
# profile-search.sh — meta-harness Path A: the profile-search sibling loop.
#
# The "remaining glue" for harness-model-fit (see ./AGENTS.md). Fans out N harness
# PROFILES, runs the agentic-bench corpus per profile, grades each answer, computes a
# numeric OBJECTIVE per profile, and SELECTS by deterministic argmax(score).
#
#   fan out profiles --> run agentic cases (agent-dispatch) --> grade (arch_score)
#                    --> aggregate objective --> argmax(score) --> summary.md
#
# This is the NUMERIC sibling of scripts/orchestrator/orchestrate.sh — NOT the LLM
# converger / ci-aipr (those grade unquantifiable code diffs; this has a number, so
# argmax is strictly better and free; see ./AGENTS.md "The loop"). It REUSES, never
# rebuilds:
#   - worker:  scripts/lib/agent-dispatch.sh  (one hermetic model run; reads
#              TOOLS/SYSPROMPT/THINKING/MAX_TURNS/TIMEOUT/ERRLOG from this scope)
#   - corpus:  agentic-bench/arch_cases/agentic_*.json
#   - grader:  agentic-bench/arch_score.grade_agentic (via grade_one.py)
# and copies orchestrate.sh's RUN_DIR + provenance.jsonl + summary.md discipline.
#
# A PROFILE is one *.env file in <profiles-dir>, sourced in an isolated subshell. It
# sets the search-space unit (see ./AGENTS.md "What a profile is"):
#   PROFILE_ID   required  short label, also the per-profile result subdir
#   PROVIDER     required  agent-dispatch provider (ollama / openrouter / vllm / claude-oauth)
#   MODEL        required  model id for that provider
#   THINKING     optional  effort: low|medium|high (default low) — the effort knob
#   SYSPROMPT    optional  path to a system-prompt-addendum file — the addendum knob
#                          (provider-agnostic; works on ollama, unlike the catalog field)
#   TOOLS        optional  mu tool CSV (default $TOOLS_DEFAULT)
#   plus any MU_MODELS_* env to overlay the catalog (sampling/addendum) for
#   openrouter/vllm — isolated per profile by the subshell.
#
# usage: profile-search.sh <profiles-dir> [run-tag]
set -u

PROFILES_DIR="${1:?usage: profile-search.sh <profiles-dir> [run-tag]}"
RUN_TAG="${2:-$(date -u +%Y%m%dT%H%M%SZ)}"

HERE=$(CDPATH= cd "$(dirname "$0")" && pwd)
export MH_ROOT="$HERE"   # so profile *.env files can reference $MH_ROOT/experiments/...
MU_REPO="${MU_REPO:-$HOME/src/public_github/mu}"
ARCH_BENCH="${ARCH_BENCH:-$HOME/src/public_github/agentic-bench}"
CASES_GLOB="${CASES_GLOB:-agentic_rust.json agentic_python.json}"  # case files to use
CASE_LIMIT="${CASE_LIMIT:-0}"                  # 0 = all cases in each file
PER_CASE_TIMEOUT="${PER_CASE_TIMEOUT:-300}"    # wall-clock backstop per case (s)
MAX_TURNS="${MAX_TURNS:-20}"
TOOLS_DEFAULT="${TOOLS_DEFAULT:-read,grep,ls,glob}"  # agentic tool grant (matches arch_bench)
OBJECTIVE="${OBJECTIVE:-pass_rate}"            # pass_rate | pass_per_dollar
WARMUP="${WARMUP:-0}"                           # 1 = untimed warm of each ollama model.
                                                # DEFAULT OFF: the ollama box is SHARED —
                                                # warming/loading a model evicts whatever
                                                # others are using. Only set WARMUP=1 (and
                                                # only run ollama profiles at all) once you
                                                # hold EXCLUSIVE use of the box.
RUN_DIR="${RUN_DIR:-$HOME/metaharness-runs/run-$RUN_TAG}"
OLLAMA_ENDPOINT="${OLLAMA_ENDPOINT:-http://10.1.1.143:11434}"

fixture_for(){  # $1=lang -> repo cwd the agentic case is graded against (mirrors arch_bench)
  case "$1" in
    rust)   echo "$MU_REPO" ;;
    python) echo "$HOME/src/public_github/mu-analytics" ;;
    *)      echo "$PWD" ;;
  esac
}

mkdir -p "$RUN_DIR"
log(){ printf '[profile-search] %s\n' "$*" >&2; }

# Pin a STABLE mu binary into the run dir: `mu` in PATH is `emu`, an auto-build
# launcher, and a concurrent session's emu run could rebuild target/release/mu out
# from under the experiment. Copy once; every dispatch uses the frozen copy so all
# arms see byte-identical mu (controlled experiment).
PINNED_MU="$RUN_DIR/mu"
if [ ! -x "$PINNED_MU" ]; then
  src="${MU:-$MU_REPO/target/release/mu}"
  [ -x "$src" ] || { log "FATAL: no mu binary at $src (build it, or set MU=)"; exit 1; }
  cp "$src" "$PINNED_MU" || { log "FATAL: cannot pin mu from $src"; exit 1; }
fi
export MU="$PINNED_MU"
log "run dir: $RUN_DIR"
log "pinned mu: $("$PINNED_MU" --version 2>/dev/null | head -1 || echo '?')"

. "$MU_REPO/scripts/lib/agent-dispatch.sh"

# ---- build the case list (lang \t id), prompts + case-json stashed in RUN_DIR/cases ----
mkdir -p "$RUN_DIR/cases"
CASE_LIST="$RUN_DIR/cases.tsv"; : > "$CASE_LIST"
for cf in $CASES_GLOB; do
  lang=$(printf '%s' "$cf" | sed -E 's/agentic_([a-z]+)\.json/\1/')
  src_cf="$ARCH_BENCH/arch_cases/$cf"
  [ -f "$src_cf" ] || { log "WARN: missing case file $src_cf"; continue; }
  python3 - "$src_cf" "$lang" "$RUN_DIR/cases" "$CASE_LIMIT" >> "$CASE_LIST" <<'PY'
import json, pathlib, sys
cf, lang, outdir, limit = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
cases = json.load(open(cf))
if limit:
    cases = cases[:limit]
for c in cases:
    cid = c["id"]
    pathlib.Path(outdir, f"{lang}-{cid}.prompt").write_text(c["prompt"])
    pathlib.Path(outdir, f"{lang}-{cid}.case.json").write_text(json.dumps(c))
    print(f"{lang}\t{cid}")
PY
done
NCASES=$(wc -l < "$CASE_LIST" | tr -d ' ')
[ "$NCASES" -gt 0 ] || { log "FATAL: no cases loaded from [$CASES_GLOB]"; exit 1; }
log "cases: $NCASES from [$CASES_GLOB] (limit=${CASE_LIMIT})"

TAB=$(printf '\t')

warm_ollama(){  # $1=model — untimed load so cold-start doesn't burn a scored case
  [ "$WARMUP" = 1 ] || return 0
  log "  warming $1 ..."
  curl -sS --max-time 1800 "$OLLAMA_ENDPOINT/api/chat" \
    -H 'Content-Type: application/json' \
    -d "$(printf '{"model":"%s","messages":[{"role":"user","content":"ok"}],"stream":false,"options":{"num_predict":1}}' "$1")" \
    >/dev/null 2>&1 || log "  warmup $1 failed (continuing)"
}

run_profile(){  # $1 = profile env file (runs in a per-profile subshell from the caller)
  pf="$1"
  PROFILE_ID=""; PROVIDER=""; MODEL=""; THINKING="low"; SYSPROMPT=""; TOOLS="$TOOLS_DEFAULT"
  . "$pf"
  if [ -z "$PROFILE_ID" ] || [ -z "$PROVIDER" ] || [ -z "$MODEL" ]; then
    log "SKIP $pf: a profile needs PROFILE_ID + PROVIDER + MODEL"; return 0
  fi
  pdir="$RUN_DIR/$PROFILE_ID"; mkdir -p "$pdir"
  rows="$pdir/rows.jsonl"; : > "$rows"
  printf '{"profile":"%s","provider":"%s","model":"%s","thinking":"%s","sysprompt":"%s","tools":"%s"}\n' \
    "$PROFILE_ID" "$PROVIDER" "$MODEL" "$THINKING" "${SYSPROMPT:-}" "$TOOLS" > "$pdir/profile.json"
  log "=== profile $PROFILE_ID: $PROVIDER/$MODEL think=$THINKING addendum=${SYSPROMPT:+yes} ==="
  [ "$PROVIDER" = ollama ] && warm_ollama "$MODEL"

  while IFS="$TAB" read -r lang cid; do
    [ -n "$cid" ] || continue
    cprompt="$RUN_DIR/cases/$lang-$cid.prompt"
    cjson="$RUN_DIR/cases/$lang-$cid.case.json"
    out="$pdir/$lang-$cid.out"; err="$pdir/$lang-$cid.err"
    fx=$(fixture_for "$lang")
    TOOLS="$TOOLS" THINKING="$THINKING" SYSPROMPT="$SYSPROMPT"
    TIMEOUT="$PER_CASE_TIMEOUT"; ERRLOG="$err"
    t0=$(date +%s)
    ( cd "$fx" && agent_dispatch "$PROVIDER" "$MODEL" "$cprompt" ) > "$out" 2>>"$err"
    rc=$?
    wall=$(( $(date +%s) - t0 ))
    if [ "$rc" -eq 124 ]; then reason=timeout
    elif [ "$rc" -ne 0 ]; then reason=error
    else reason=done; fi
    # Match arch_score.grade_row: only grade completed runs; timeout/error -> ungraded.
    if [ "$reason" = done ]; then
      grade=$(ARCH_BENCH="$ARCH_BENCH" python3 "$HERE/grade_one.py" "$cjson" "$out" 2>>"$err" || printf '{"score":null,"ungraded":"grade-error"}')
    else
      grade=$(printf '{"score":null,"ungraded":"%s"}' "$reason")
    fi
    python3 - "$rows" "$PROFILE_ID" "$PROVIDER" "$MODEL" "$lang" "$cid" "$reason" "$wall" "$grade" <<'PY'
import json, sys
rows, pid, prov, model, lang, cid, reason, wall, grade = sys.argv[1:10]
try:
    g = json.loads(grade)
except Exception:
    g = {"score": None, "ungraded": "grade-parse"}
row = {"profile": pid, "provider": prov, "model": model, "lang": lang, "case": cid,
       "exit_reason": reason, "wall_s": int(wall), "cost_usd": 0.0, "grade": g}
open(rows, "a").write(json.dumps(row) + "\n")
PY
    printf '{"profile":"%s","case":"%s/%s","exit":"%s","wall_s":%s}\n' \
      "$PROFILE_ID" "$lang" "$cid" "$reason" "$wall" >> "$RUN_DIR/provenance.jsonl"
    log "  [$PROFILE_ID] $lang/$cid: $reason ${wall}s grade=$grade"
  done < "$CASE_LIST"
}

found=0
for pf in "$PROFILES_DIR"/*.env; do
  [ -e "$pf" ] || break
  found=1
  ( run_profile "$pf" )   # subshell isolates each profile's env overlay (MU_MODELS_*, etc.)
done
[ "$found" = 1 ] || { log "FATAL: no *.env profiles in $PROFILES_DIR"; exit 1; }

# ---- aggregate + deterministic argmax(score) -> summary.md ----
python3 - "$RUN_DIR" "$OBJECTIVE" > "$RUN_DIR/summary.md" <<'PY'
import glob, json, sys
run_dir, objective = sys.argv[1], sys.argv[2]
profs = []
for rows_path in sorted(glob.glob(f"{run_dir}/*/rows.jsonl")):
    rows = [json.loads(l) for l in open(rows_path) if l.strip()]
    if not rows:
        continue
    pid = rows[0]["profile"]
    graded = [r for r in rows if r["grade"].get("score") is not None]
    ng = len(graded)
    passes = sum(1 for r in graded if r["grade"].get("correct"))
    leaks = sum(1 for r in graded if r["grade"].get("leak"))
    fabs = sum(1 for r in graded if r["grade"].get("fabricated"))
    cost = sum(r.get("cost_usd") or 0 for r in rows)
    timeouts = sum(1 for r in rows if r["exit_reason"] == "timeout")
    errs = sum(1 for r in rows if r["exit_reason"] == "error")
    pass_rate = passes / ng if ng else 0.0
    if objective == "pass_per_dollar":
        score = pass_rate / cost if cost > 0 else pass_rate  # free -> degenerate; fall back
    else:
        score = pass_rate
    profs.append(dict(pid=pid, n=len(rows), ng=ng, passes=passes, leaks=leaks,
                      fabs=fabs, cost=cost, timeouts=timeouts, errs=errs,
                      pass_rate=pass_rate, score=score))
# argmax: score desc, then fewer leaks, then cheaper (deterministic tie-break)
profs.sort(key=lambda p: (-p["score"], p["leaks"], p["cost"], p["pid"]))
print(f"# meta-harness Path A — profile search\n")
print(f"`{run_dir}`\n")
print(f"objective: **{objective}** | profiles: {len(profs)}\n")
print("| rank | profile | pass_rate | pass/graded | leaks | fab | timeout | err | cost$ | score |")
print("|---|---|--:|--:|--:|--:|--:|--:|--:|--:|")
for i, p in enumerate(profs, 1):
    print(f"| {i} | {p['pid']} | {p['pass_rate']:.3f} | {p['passes']}/{p['ng']} | "
          f"{p['leaks']} | {p['fabs']} | {p['timeouts']} | {p['errs']} | "
          f"{p['cost']:.4f} | {p['score']:.4f} |")
if profs:
    w = profs[0]
    print(f"\n**WINNER: {w['pid']}** "
          f"(score={w['score']:.4f}, pass_rate={w['pass_rate']:.3f}, leaks={w['leaks']})")
else:
    print("\n**WINNER: none — no graded rows**")
PY
cat "$RUN_DIR/summary.md" >&2
log "done -> $RUN_DIR/summary.md"
