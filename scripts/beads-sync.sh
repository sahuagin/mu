#!/bin/sh
# beads-sync — trailing-PR sync of the canonical beads DB to .beads/issues.jsonl
# on main, so post-merge bead updates stop orphaning as dangling commits.
# (bead mu-4sf8)
#
# THE DISEASE: br auto-exports the DB to .beads/issues.jsonl on every mutation.
# Bead updates happen AFTER PR merges (close/comment on merge — correct), so the
# export lands in the backing workspace's working copy, which sits on a stale
# base. The next `jj new` strands it as an anonymous commit. 17 such commits
# accumulated 2026-06-04..06-06 while main's JSONL fell 151 issues behind the DB.
#
# THE CURE: run this at session end / after a merge wave. It re-exports the DB
# onto a fresh commit on main and ships it through the normal PR flow
# (push → bot approve → auto-merge). Idempotent: exits 0 with no PR when main's
# JSONL already matches a fresh export.
#
# SAFETY:
#   * Refuses to run if the working copy has NON-.beads changes (never stomps
#     real work). .beads-only changes are superseded by the fresh export.
#   * Refuses to force-export unless the DB is a strict superset of main's
#     JSONL (no issue id present in JSONL but missing from the DB) — the one
#     case where forcing would lose data.
#   * `br sync --flush-only` itself never touches git/jj state.
#
# REQUIREMENTS: run from the BACKING repo (not a sibling workspace — a bare
# `br` there resolves a divergent local DB; see memory c295ca3e). bot-gh on
# PATH for the approval step (skipped with a warning when absent).

set -eu

C_RED=$(printf '\033[31m'); C_GRN=$(printf '\033[32m'); C_YEL=$(printf '\033[33m'); C_OFF=$(printf '\033[0m')
say()  { printf '%s\n' "beads-sync: $*"; }
fail() { printf '%s\n' "${C_RED}beads-sync: $*${C_OFF}" >&2; exit 1; }

command -v jj >/dev/null 2>&1 || fail "jj not found"
command -v br >/dev/null 2>&1 || fail "br not found"
command -v gh >/dev/null 2>&1 || fail "gh not found"
command -v python3 >/dev/null 2>&1 || fail "python3 not found (needed for the superset check)"

ROOT=$(jj root 2>/dev/null) || fail "not inside a jj workspace"
cd "$ROOT"
[ -d .beads ] || fail "no .beads/ at jj root — wrong repo?"
# Backing repo only: a secondary workspace's .jj/repo is a FILE pointing at the
# backing store, and a bare `br` there resolves a divergent sibling DB
# (memory c295ca3e). The backing repo has the real directory.
[ -d .jj/repo ] || fail "run from the BACKING repo, not a sibling workspace ($ROOT)"
JSONL=.beads/issues.jsonl

# Repo slug for gh -R (jj leaves git in detached HEAD; gh can't infer).
REPO=$(git remote get-url origin 2>/dev/null \
  | sed -E 's#(git@github.com:|https://github.com/)##; s#\.git$##') \
  || fail "cannot resolve origin remote"
[ -n "$REPO" ] || fail "empty repo slug from origin remote"

# ── guard: never stomp real work in the working copy ────────────────────────
NON_BEADS=$(jj diff --summary 2>/dev/null | awk '$2 !~ /^\.beads\// {print $2}')
[ -z "$NON_BEADS" ] || fail "working copy has non-.beads changes; commit or stash them first:
$NON_BEADS"

# ── fetch + fresh commit on main ─────────────────────────────────────────────
# Remember the current @: if it held .beads-only auto-export changes (the
# usual disease), it would dangle after `jj new main` — exactly the orphan
# this script exists to prevent. Abandon it once we've moved off, but only
# when it's an anonymous snapshot (no description, no bookmarks).
OLD_AT=$(jj log -r @ --no-graph -T 'change_id' 2>/dev/null)
OLD_AT_DIRTY=$(jj diff --summary 2>/dev/null | head -1)
OLD_AT_ANON=$(jj log -r @ --no-graph -T 'if(description || bookmarks, "no", "yes")' 2>/dev/null)
say "fetching origin"
jj git fetch >/dev/null 2>&1
jj new main >/dev/null 2>&1
if [ -n "$OLD_AT_DIRTY" ] && [ "$OLD_AT_ANON" = "yes" ]; then
  say "abandoning superseded .beads-only snapshot $OLD_AT"
  jj abandon "$OLD_AT" >/dev/null 2>&1 || true
fi

# ── guard: DB must be a strict superset of main's JSONL before forcing ──────
# (the only case where --force could lose data is an issue id present in the
# committed JSONL but absent from the DB)
MISSING=$(python3 - "$JSONL" <<'EOF'
import json, subprocess, sys
jsonl_ids = set()
for line in open(sys.argv[1]):
    line = line.strip()
    if line:
        try: jsonl_ids.add(json.loads(line)["id"])
        except Exception: pass
out = subprocess.run(["br","list","--all","--json","--limit","100000"],
                     capture_output=True, text=True)
if out.returncode != 0:
    print("BR_LIST_FAILED"); sys.exit(0)
db_ids = {i["id"] for i in json.loads(out.stdout)["issues"]}
missing = sorted(jsonl_ids - db_ids)
print(" ".join(missing) if missing else "")
EOF
)
[ "$MISSING" != "BR_LIST_FAILED" ] || fail "br list --all --json failed; cannot verify superset"
[ -z "$MISSING" ] || fail "DB is MISSING issues present in main's JSONL — refusing to force-export (would lose: $MISSING)"

# ── export + no-op detection ─────────────────────────────────────────────────
br sync --flush-only --force >/dev/null
if [ -z "$(jj diff --summary 2>/dev/null)" ]; then
  jj abandon @ >/dev/null 2>&1 || true
  say "${C_GRN}main's JSONL already matches the DB — nothing to sync${C_OFF}"
  exit 0
fi

STAMP=$(date +%Y%m%d-%H%M%S)
BRANCH="agent/beads-sync-$STAMP"
N_ISSUES=$(grep -c . "$JSONL" || true)

jj describe -m "beads: sync DB→JSONL ($N_ISSUES issues, beads-sync trailing PR)

Automated by scripts/beads-sync.sh (mu-4sf8): re-export of the canonical
beads DB onto main so post-merge bead updates stop orphaning as dangling
commits. DB verified a strict superset of main's JSONL before export." >/dev/null
jj new >/dev/null 2>&1
jj bookmark create "$BRANCH" -r @- >/dev/null
say "pushing $BRANCH"
jj git push --bookmark "$BRANCH" --allow-new >/dev/null 2>&1

PR_URL=$(gh pr create -R "$REPO" --head "$BRANCH" \
  --title "beads: sync DB→JSONL ($STAMP)" \
  --body "Automated trailing-PR beads sync (scripts/beads-sync.sh, mu-4sf8). DB verified a strict superset of main's JSONL before force-export — nothing lost.")
PR_NUM=${PR_URL##*/}
say "opened PR #$PR_NUM ($PR_URL)"

if command -v bot-gh >/dev/null 2>&1; then
  bot-gh pr review "$PR_NUM" -R "$REPO" --approve \
    --body "Automated beads-only sync; superset-checked by scripts/beads-sync.sh." \
    >/dev/null 2>&1 || say "${C_YEL}bot-gh approve failed — approve manually${C_OFF}"
else
  say "${C_YEL}bot-gh not found — approve PR #$PR_NUM manually${C_OFF}"
fi
gh pr merge "$PR_NUM" -R "$REPO" --auto --merge --delete-branch >/dev/null 2>&1 \
  || say "${C_YEL}auto-merge not enabled — merge PR #$PR_NUM manually${C_OFF}"
say "${C_GRN}done — PR #$PR_NUM will merge when CI is green${C_OFF}"
