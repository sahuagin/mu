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

printf "%spre-pr-check: all checks green%s\n" "$C_GREEN" "$C_OFF"
