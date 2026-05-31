#!/usr/bin/env bash
# verify-claims-test.sh — exercise scripts/verify-claims.sh against synthetic
# commits in a throwaway repo.
#
# bead: mu-b5kl. The gate is VCS-agnostic (jj when invoked from a jj workspace,
# git otherwise), so the SAME case suite runs against BOTH a throwaway git repo
# and a throwaway jj repo, forcing the backend via MU_VERIFY_VCS. The git suite
# is zero-dependency (git + bash); the jj suite is skipped with a note if `jj`
# is not installed.
#
# Run from any cwd; each suite creates its own tmpdir.

set -u
set -o pipefail

# Locate the gate script relative to this test file.
TEST_DIR="$(cd "$(dirname "$0")" && pwd)"
GATE="$TEST_DIR/../verify-claims.sh"

if [ ! -x "$GATE" ]; then
  echo "verify-claims-test: gate not found at $GATE" >&2
  exit 2
fi

# --- harness --------------------------------------------------------------

PASS=0
FAIL=0

# Each test runs the gate and asserts exit code + (optionally) stderr content.
# Usage:
#   assert_gate <name> <expected-rc> <expected-stderr-grep> <commit-ish> [env...]
assert_gate() {
  local name="$1" expected_rc="$2" expected_grep="$3" commit="$4"
  shift 4
  local out rc
  out=$(env "$@" "$GATE" "$commit" 2>&1)
  rc=$?
  if [ "$rc" -ne "$expected_rc" ]; then
    printf "FAIL: %s — exit=%d expected=%d\n  stderr: %s\n" "$name" "$rc" "$expected_rc" "$out" >&2
    FAIL=$((FAIL + 1))
    return
  fi
  if [ -n "$expected_grep" ]; then
    if ! printf "%s\n" "$out" | grep -qE "$expected_grep"; then
      printf "FAIL: %s — exit=%d ok, but stderr did not match /%s/\n  stderr: %s\n" \
        "$name" "$rc" "$expected_grep" "$out" >&2
      FAIL=$((FAIL + 1))
      return
    fi
  fi
  printf "PASS: %s\n" "$name"
  PASS=$((PASS + 1))
}

# --- backend-parametrized commit construction ----------------------------
#
# Each suite sets VCS (jj|git) and REV (commit-ish for "the commit just made").
# apply_actions mutates the working tree; mk_commit wraps it in a commit.
#
#   make_commit <message-file> <action>...
# where each action is one of:
#   add  <path> <bytes>          (create with <bytes> lines)
#   mod  <path> <bytes>          (append <bytes> lines)
#   del  <path>

apply_actions() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      add)
        local p="$2" b="$3" i=0
        mkdir -p "$(dirname "$p")"
        : > "$p"
        while [ "$i" -lt "$b" ]; do echo "line $i" >> "$p"; i=$((i + 1)); done
        [ "$VCS" = git ] && git add "$p"
        shift 3
        ;;
      mod)
        local p="$2" b="$3" i=0
        while [ "$i" -lt "$b" ]; do echo "added $i" >> "$p"; i=$((i + 1)); done
        [ "$VCS" = git ] && git add "$p"
        shift 3
        ;;
      del)
        if [ "$VCS" = git ]; then git rm -q "$2"; else rm -f "$2"; fi
        shift 2
        ;;
      *)
        echo "make_commit: unknown action $1" >&2
        return 2
        ;;
    esac
  done
}

make_commit() {
  local msgfile="$1"; shift
  if [ "$VCS" = git ]; then
    apply_actions "$@"
    git commit -q -F "$msgfile"
  else
    # advance to a fresh empty change off the previous commit, then fill it
    jj new >/dev/null 2>&1
    apply_actions "$@"
    jj describe -m "$(cat "$msgfile")" >/dev/null 2>&1
  fi
}

setup_repo() {
  local tmp
  tmp="$(mktemp -d -t verify-claims-test.XXXXXX)"
  SUITE_TMP="$tmp"
  cd "$tmp"
  if [ "$VCS" = git ]; then
    git init -q -b main
    git config user.email "test@example.com"
    git config user.name  "test"
    echo "seed" > seed.txt
    git add seed.txt
    git commit -q -m "seed"
  else
    jj git init >/dev/null 2>&1
    jj config set --repo user.name  "test"               >/dev/null 2>&1
    jj config set --repo user.email "test@example.com"   >/dev/null 2>&1
    echo "seed" > seed.txt
    jj describe -m "seed" >/dev/null 2>&1   # seed into the initial working-copy change
  fi
}

# --- the case suite (identical for both backends) ------------------------

run_cases() {
  local M="$SUITE_TMP/msg"

  # case 1: matching block → pass
  cat > "$M" <<'EOF'
feat: matching case

This commit's claim matches the diff.

## Files
A foo.txt +5
EOF
  make_commit "$M" add foo.txt 5
  assert_gate "[$VCS] matching block passes" 0 "matches diff" "$REV" "MU_VERIFY_VCS=$VCS"

  # case 2: claimed file not in diff → fail
  cat > "$M" <<'EOF'
feat: fictional claim

Claims a file that's not in the diff (the PR #52 failure pattern).

## Files
A bar.txt +10
A imaginary.rs +500
EOF
  make_commit "$M" add bar.txt 10
  assert_gate "[$VCS] claimed-but-missing fails" 1 "claimed file not in diff: imaginary.rs" "$REV" "MU_VERIFY_VCS=$VCS"

  # case 3: file in diff but not claimed → fail
  cat > "$M" <<'EOF'
feat: incomplete claim

Touches two files but only claims one.

## Files
M foo.txt +3
EOF
  make_commit "$M" mod foo.txt 3 mod bar.txt 2
  assert_gate "[$VCS] unclaimed diff fails" 1 "diff touched bar.txt but it was not claimed" "$REV" "MU_VERIFY_VCS=$VCS"

  # case 4: status mismatch (A vs M) → fail
  cat > "$M" <<'EOF'
feat: status lies

Claims A but the file already existed (M).

## Files
A foo.txt +1
EOF
  make_commit "$M" mod foo.txt 1
  assert_gate "[$VCS] status mismatch fails" 1 "status mismatch for foo.txt: claim=A actual=M" "$REV" "MU_VERIFY_VCS=$VCS"

  # case 5: LOC drift >20% → warn (exit 0)
  cat > "$M" <<'EOF'
feat: loc drift

Claim is off by >20% on LOC counts; should warn but exit 0.

## Files
M foo.txt +100
EOF
  make_commit "$M" mod foo.txt 5
  assert_gate "[$VCS] LOC drift warns (exit 0)" 0 "WARN.*added LOC drift" "$REV" "MU_VERIFY_VCS=$VCS"

  # case 6: no ## Files block → exit 0 with note
  cat > "$M" <<'EOF'
feat: legacy commit, no claim block

This commit has no ## Files section — opt-in strictness means we skip.
EOF
  make_commit "$M" mod foo.txt 1
  assert_gate "[$VCS] no block exits 0 with note" 0 "no .## Files. block — skipping" "$REV" "MU_VERIFY_VCS=$VCS"

  # case 7: MU_SKIP_CLAIM_CHECK=1 bypasses
  cat > "$M" <<'EOF'
feat: would fail without bypass

## Files
A nonexistent-file.rs +999
EOF
  make_commit "$M" mod foo.txt 1
  assert_gate "[$VCS] bypass env var skips" 0 "MU_SKIP_CLAIM_CHECK=1" "$REV" "MU_VERIFY_VCS=$VCS" "MU_SKIP_CLAIM_CHECK=1"

  # case 8: bad commit ref → exit 2
  assert_gate "[$VCS] bad commit ref returns 2" 2 "is not a commit" "definitely-not-a-commit" "MU_VERIFY_VCS=$VCS"

  # case 9: git trailers after ## Files block are ignored (mu-d33g)
  # Regression: before the fix, lines like "Co-Authored-By: ..." after the
  # ## Files block were parsed as Files entries (status=C path=Claude),
  # causing "claimed file not in diff" failures on any trailer-bearing commit.
  cat > "$M" <<'EOF'
feat: trailers after files block must be ignored

Regression test for mu-d33g. Git trailer lines (Co-Authored-By,
Signed-off-by, Reviewed-by, etc.) at the end of the commit message
follow the convention of being placed AFTER the body. A ## Files
block that's the last body section will see those trailers in its
scan, and they must be skipped — not parsed as Files entries.

## Files
A trailer-test.txt +5

Co-Authored-By: Someone Else <someone@example.com>
Signed-off-by: Maintainer Mantis <maint@example.com>
Reviewed-by: A Reviewer <review@example.com>
EOF
  make_commit "$M" add trailer-test.txt 5
  assert_gate "[$VCS] trailers after files block ignored" 0 "matches diff" "$REV" "MU_VERIFY_VCS=$VCS"
}

# --- run a suite for one backend -----------------------------------------

# Run all cases against one backend. Cases run inline (not in a subshell) so the
# PASS/FAIL counters accumulate across both suites.
run_suite() {
  VCS="$1"
  case "$VCS" in
    git) REV="HEAD" ;;
    jj)  REV="@" ;;
  esac
  printf "\n==== suite: %s ====\n" "$VCS"
  local start_dir="$PWD"
  setup_repo
  run_cases
  cd "$start_dir"
  [ -n "${SUITE_TMP:-}" ] && rm -rf "$SUITE_TMP"
}

# --- main -----------------------------------------------------------------

run_suite git

if command -v jj >/dev/null 2>&1; then
  run_suite jj
else
  printf "\n==== suite: jj — SKIPPED (jj not installed) ====\n"
fi

# --- summary -------------------------------------------------------------

printf "\n%d passed, %d failed\n" "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
