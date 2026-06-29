#!/usr/bin/env bash
# Pre-PR verification — mirrors the CI checks in .github/workflows/ci.yml.
#
# Runs fmt, clippy, and test in sequence. Exits non-zero on first failure so
# the failing step is the last output. Print elapsed time per step.
#
# Env:
#   PRE_PR_QUICK=1   skip cargo test (fmt + clippy only)
#   PRE_PR_NO_COLOR  disable color output

set -u
set -o pipefail

# --- color setup -----------------------------------------------------------

if [ -t 1 ] && [ -z "${PRE_PR_NO_COLOR:-}" ]; then
  C_RED=$'\033[31m'
  C_GREEN=$'\033[32m'
  C_YELLOW=$'\033[33m'
  C_DIM=$'\033[2m'
  C_OFF=$'\033[0m'
else
  C_RED=""; C_GREEN=""; C_YELLOW=""; C_DIM=""; C_OFF=""
fi

# --- locate repo root ------------------------------------------------------

# jj workspaces don't have a top-level .git dir; try jj first, fall back to git.
if command -v jj >/dev/null 2>&1 && jj root >/dev/null 2>&1; then
  REPO_ROOT="$(jj root)"
elif git rev-parse --show-toplevel >/dev/null 2>&1; then
  REPO_ROOT="$(git rev-parse --show-toplevel)"
else
  echo "${C_RED}pre-pr-check: not inside a jj or git repo${C_OFF}" >&2
  exit 2
fi

cd "$REPO_ROOT"

# --- step runner -----------------------------------------------------------

run_step() {
  local name="$1"; shift
  local start_s end_s elapsed
  start_s=$(date +%s)

  printf "%s==>%s %s\n" "$C_YELLOW" "$C_OFF" "$name"
  printf "%s    %s%s\n" "$C_DIM" "$*" "$C_OFF"

  if "$@"; then
    end_s=$(date +%s)
    elapsed=$((end_s - start_s))
    printf "%s    ok (%ds)%s\n\n" "$C_GREEN" "$elapsed" "$C_OFF"
    return 0
  else
    local rc=$?
    end_s=$(date +%s)
    elapsed=$((end_s - start_s))
    printf "%s    FAIL exit=%d (%ds)%s\n" "$C_RED" "$rc" "$elapsed" "$C_OFF"
    printf "%spre-pr-check: %s failed. Fix locally and re-run.%s\n" "$C_RED" "$name" "$C_OFF" >&2
    exit "$rc"
  fi
}

# --- checks ----------------------------------------------------------------

run_step "cargo fmt --check"  cargo fmt --all -- --check
run_step "cargo clippy"       cargo clippy --workspace --all-targets --all-features -- -D warnings

if [ "${PRE_PR_QUICK:-}" = "1" ]; then
  printf "%s==> skipping cargo test (PRE_PR_QUICK=1)%s\n\n" "$C_DIM" "$C_OFF"
else
  run_step "cargo test --workspace" cargo test --workspace --all-features --no-fail-fast
fi

# verify-claims gate (mu-b5kl): iterate every non-merge commit in main..@ (jj)
# or main..HEAD (git) and run scripts/verify-claims.sh on each. Opt-in
# strictness: commits without a `## Files` block exit 0 with a skip note.
# Bypass: MU_SKIP_CLAIM_CHECK=1.
#
# jj colocated mode keeps refs/heads/<bookmark> in sync but doesn't move git
# HEAD with @, so `git diff-tree HEAD` sees main and main..HEAD is empty.
# Prefer jj's view when jj is available.
verify_claims_step() {
  local check="$REPO_ROOT/scripts/verify-claims.sh"
  if [ ! -x "$check" ]; then
    printf "%s    verify-claims.sh missing — skipping%s\n\n" "$C_DIM" "$C_OFF"
    return 0
  fi
  local commits=""
  if command -v jj >/dev/null 2>&1 && jj root >/dev/null 2>&1; then
    commits=$(jj log -r 'main..@ ~ empty() ~ merges()' --no-graph \
                --reversed -T 'commit_id ++ "\n"' 2>/dev/null || true)
  fi
  if [ -z "$commits" ]; then
    local base
    if base=$(git merge-base main HEAD 2>/dev/null); then
      commits=$(git rev-list --reverse --no-merges "$base..HEAD")
    fi
  fi
  if [ -z "$commits" ]; then
    printf "%s    no commits in main..@%s\n\n" "$C_DIM" "$C_OFF"
    return 0
  fi
  local rc=0 c
  for c in $commits; do
    "$check" "$c" || rc=$?
  done
  return "$rc"
}
run_step "verify-claims (main..@)" verify_claims_step

# beads orphans nudge (mu-da27): surface beads referenced by main..@ that are
# still OPEN in the CENTRAL store, so they get closed before merge instead of
# left open. The old GitHub-CI check read the retired in-repo
# .beads/issues.jsonl; this queries central beadsd via the `beads` client, so it
# runs HERE (locally, where the service is reachable). Informational — never
# blocks, and silent if beadsd is unreachable or `beads` isn't installed.
orphans_nudge() {
  local script="$REPO_ROOT/scripts/beads_orphans_prscope.py"
  [ -f "$script" ] || return 0
  local text=""
  if command -v jj >/dev/null 2>&1 && jj root >/dev/null 2>&1; then
    text=$(jj log -r 'main..@ ~ merges()' --no-graph -T 'description ++ "\n\n"' 2>/dev/null || true)
  else
    local base
    if base=$(git merge-base main HEAD 2>/dev/null); then
      text=$(git log --no-merges --format='%B%n%n' "$base..HEAD" 2>/dev/null || true)
    fi
  fi
  [ -n "$text" ] || return 0
  local out
  out=$(printf '%s' "$text" | python3 "$script" 2>/dev/null || true)
  if [ -n "$out" ]; then
    printf "%s==> beads referenced by this branch are still open:%s\n" "$C_YELLOW" "$C_OFF"
    printf "%s\n\n" "$out"
  fi
  return 0
}
orphans_nudge

printf "%spre-pr-check: all checks green%s\n" "$C_GREEN" "$C_OFF"
