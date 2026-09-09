#!/usr/bin/env bash
# review-size-gate-test.sh — the SIZE gate in scripts/ai-review.sh, against a
# throwaway git repo.
#
# bead: mu-review-gate-seam-reviewers-9vkbt.1. Hermetic and free:
# MU_REVIEW_SIZE_CHECK_ONLY=1 stops the gate right after the size check passes,
# so no reviewer, model, mu binary, cargo build, ollama probe, or network is
# touched. A block happens before that point anyway. To PROVE that rather than
# assume it, the suite runs with every $HOME-rooted PATH entry removed (that is
# where mu, cargo, tq and agent-role live on the dev boxes) and the gate is
# invoked under `env -i` with an explicit allowlist, so no MU_REVIEW_* export
# in the caller's shell can change a case. The seam exits 4 (NO REVIEW), never
# 0, so a stray export cannot make ci-aipr look green either. Run from any
# cwd; creates its own tmpdir.

set -u
set -o pipefail
PATH="$(printf '%s' "$PATH" | tr ':' '\n' | grep -vE "^${HOME}/" | paste -sd: -)"
export PATH
# The fixture repo must not inherit the host's git configuration either:
# a global commit.gpgSign or core.hooksPath would sign, hook, prompt, or
# fail the throwaway commits. No system config, no global config; the
# repo-local config below is the only one that applies.
export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null

TEST_DIR="$(cd "$(dirname "$0")" && pwd)"
GATE="$TEST_DIR/../ai-review.sh"
[ -x "$GATE" ] || { echo "review-size-gate-test: gate not found at $GATE" >&2; exit 2; }
command -v git >/dev/null 2>&1 || { echo "review-size-gate-test: git missing" >&2; exit 2; }

PASS=0
FAIL=0
TMP="$(mktemp -d "${TMPDIR:-/tmp}/ai-review-size.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
REPO="$TMP/repo"
LOGF="$TMP/review-events.jsonl"

mkdir -p "$REPO"
cd "$REPO" || exit 2
git init -q -b main . 2>/dev/null || { git init -q . && git checkout -qb main; }
git config user.email size-gate@test
git config user.name size-gate
git config commit.gpgSign false
git config core.hooksPath /dev/null
printf '{ "project_desc": "size-gate fixture", "spec": { "id_pattern": "zz-[0-9]{3}", "dir": "specs/" } }\n' > .ai-review.json
echo base > base.txt
git add -A && git commit -qm "base"

# fresh_branch: reset the feature branch to main so each case starts clean.
fresh_branch() {
  git checkout -q main
  git branch -qD feature 2>/dev/null || true
  git checkout -qb feature
}
# gen <lines> <path>: a text file of N distinct lines.
gen() { mkdir -p "$(dirname "$2")"; seq 1 "$1" | sed 's/^/line /' > "$2"; }
commit_all() { git add -A && git commit -qm "$1"; }

# run_gate <name> <expected-rc> <expected-grep-ERE> [env...]
run_gate() {
  local name="$1" expected_rc="$2" expected_grep="$3"
  shift 3
  local out rc
  out=$(cd "$REPO" && env -i PATH="$PATH" HOME="$HOME" ${TMPDIR:+TMPDIR="$TMPDIR"} GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null MU_REVIEW_BASE=main MU_REVIEW_LOG="$LOGF" MU_REVIEW_SIZE_CHECK_ONLY=1 MU_REVIEW_NO_COLOR=1 "$@" "$GATE" 2>&1)
  rc=$?
  if [ "$rc" -ne "$expected_rc" ]; then
    printf 'FAIL: %s — exit=%d expected=%d\n  output: %s\n' "$name" "$rc" "$expected_rc" "$out" >&2
    FAIL=$((FAIL + 1)); return
  fi
  if [ -n "$expected_grep" ] && ! printf '%s\n' "$out" | grep -qE "$expected_grep"; then
    printf 'FAIL: %s — exit ok, output did not match /%s/\n  output: %s\n' "$name" "$expected_grep" "$out" >&2
    FAIL=$((FAIL + 1)); return
  fi
  printf 'PASS: %s\n' "$name"
  PASS=$((PASS + 1))
}

# 1. Under the default cap: the gate passes the size check.
fresh_branch; gen 100 src/small.rs; commit_all "small change"
run_gate "under cap passes (seam exits 4: no review)" 4 'size gate passed \(100 reviewable lines, cap 2000;.*NO REVIEW PERFORMED'

# 2. Over the default cap: BLOCK with the SIZE finding and split hints, exit 1,
#    and one {"mode":"size"} panel line in the log.
: > "$LOGF"
fresh_branch; gen 1500 src/a.rs; commit_all "part a"; gen 1000 src/b.rs; commit_all "part b"
run_gate "over cap blocks" 1 'PANEL BLOCK — SIZE: 2500 lines > cap 2000'
run_gate "block carries the finding line" 1 '^FINDING\|blocker\|\(branch\)\|change too large'
run_gate "block names per-file split points" 1 '^ +1500 +src/a\.rs'
run_gate "block names per-commit split points" 1 'part a  \[1 file changed, 1500 insertions'
if grep -q '"event":"panel","mode":"size","outcome":"BLOCK","why":"lines","measure":2500,"cap":2000,"lines":2500,"line_cap":2000,"base":"main","files_changed":2,"override":false,"override_kind":""' "$LOGF"; then
  printf 'PASS: size block is logged\n'; PASS=$((PASS + 1))
else
  printf 'FAIL: size block log line missing or wrong\n  log: %s\n' "$(cat "$LOGF")" >&2; FAIL=$((FAIL + 1))
fi

# 2b. Content lines that look like file headers still count (panel finding).
fresh_branch; mkdir -p src; { for i in $(seq 1 30); do printf '++ plus %s\n-- minus %s\n' "$i" "$i"; done; } > src/weird.rs; commit_all "header-shaped content"
run_gate "content lines starting with ++ or -- are counted" 4 'size gate passed \(60 reviewable lines'

# 3. Lockfiles and binary/media paths do not count.
fresh_branch; gen 5000 Cargo.lock; gen 50 src/c.rs; commit_all "lockfile churn"
run_gate "lockfile lines are excluded" 4 'size gate passed \(50 reviewable lines, cap 2000; excluded from the reviewer diff: 1 file'

# 4. The cap is a knob; 0 disables it.
fresh_branch; gen 100 src/small.rs; commit_all "small change"
run_gate "custom cap blocks" 1 'SIZE: 100 lines > cap 50' MU_REVIEW_MAX_DIFF_LINES=50
fresh_branch; gen 2500 src/big.rs; commit_all "big change"
run_gate "cap 0 disables the gate" 4 'size gate passed \(2500 reviewable lines, cap 0;' MU_REVIEW_MAX_DIFF_LINES=0
run_gate "garbage cap falls back to the default" 1 'cap 2000' MU_REVIEW_MAX_DIFF_LINES=lots

# 5. Opting into a degraded review continues past the block and says so.
: > "$LOGF"
run_gate "MU_REVIEW_CHUNK=1 continues into a chunked review" 4 'continuing with a DEGRADED CHUNKED review.*verdict is binding' MU_REVIEW_CHUNK=1
run_gate "MU_REVIEW_SIZE_OVERRIDE=1 continues on the normal path" 4 'continuing with the normal panel despite the size.*verdict is binding' MU_REVIEW_SIZE_OVERRIDE=1
run_gate "MU_REVIEW_OVERRIDE=1 passes the gate but is named as the verdict override" 4 'VERDICT override too' MU_REVIEW_OVERRIDE=1
run_gate "an overridden block is not reported as passed" 4 'size gate BLOCKED and overridden \(chunk' MU_REVIEW_CHUNK=1
if grep -q '"override":true,"override_kind":"chunk"' "$LOGF" && grep -q '"override":true,"override_kind":"size-override"' "$LOGF" && grep -q '"override":true,"override_kind":"override"' "$LOGF"; then
  printf 'PASS: all three override kinds are logged as such\n'; PASS=$((PASS + 1))
else
  printf 'FAIL: override log lines missing or wrong\n  log: %s\n' "$(cat "$LOGF")" >&2; FAIL=$((FAIL + 1))
fi
if grep -q '"mode":"size-check","outcome":"NO_REVIEW"' "$LOGF"; then
  printf 'PASS: the test seam logs NO_REVIEW\n'; PASS=$((PASS + 1))
else
  printf 'FAIL: size-check log line missing\n  log: %s\n' "$(cat "$LOGF")" >&2; FAIL=$((FAIL + 1))
fi

# 6. Paths git quotes (non-ASCII) or that contain " b/" or spaces still resolve,
#    so the exclusion list applies to them and counts land on the right file.
fresh_branch; gen 3000 "地方/Cargo.lock"; gen 40 "src/a b/c.rs"; gen 20 "src/plain.rs"; commit_all "quoted and spaced paths"
run_gate "git-quoted lockfile path is still excluded" 4 'size gate passed \(60 reviewable lines'
run_gate "a path containing space and b/ is counted once" 1 '^ +40 +src/a b/c\.rs' MU_REVIEW_MAX_DIFF_LINES=50
git checkout -q main; gen 30 src/gone.rs; commit_all "on main: a file the branch will delete"
fresh_branch; git rm -q src/gone.rs; commit_all "delete"
run_gate "a deleted file's removed lines count under its own path" 1 '^ +30 +src/gone\.rs' MU_REVIEW_MAX_DIFF_LINES=10

printf '\nreview-size-gate-test: %d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
