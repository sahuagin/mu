#!/usr/bin/env bash
# verify-claims-test.sh — exercise scripts/verify-claims.sh against synthetic
# commits in a throwaway git repo.
#
# bead: mu-b5kl. Runs zero-dependency: pure git + bash + the gate script.
#
# Run from any cwd; the test creates its own tmpdir.

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
  local out rc grep_rc=0
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

# --- temp repo ------------------------------------------------------------

TMP="$(mktemp -d -t verify-claims-test.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

cd "$TMP"
git init -q -b main
git config user.email "test@example.com"
git config user.name  "test"

# Seed commit so HEAD~1 exists for diff-tree consistency.
echo "seed" > seed.txt
git add seed.txt
git commit -q -m "seed"

# Helper: make a commit with given filesystem changes and a given message.
#   make_commit <message-file> <action>...
# where each action is one of:
#   add  <path> <bytes>
#   mod  <path> <bytes>          (append bytes)
#   del  <path>
make_commit() {
  local msgfile="$1"; shift
  while [ "$#" -gt 0 ]; do
    case "$1" in
      add)
        local p="$2" b="$3"
        mkdir -p "$(dirname "$p")"
        : > "$p"
        local i=0
        while [ "$i" -lt "$b" ]; do echo "line $i" >> "$p"; i=$((i+1)); done
        git add "$p"
        shift 3
        ;;
      mod)
        local p="$2" b="$3"
        local i=0
        while [ "$i" -lt "$b" ]; do echo "added $i" >> "$p"; i=$((i+1)); done
        git add "$p"
        shift 3
        ;;
      del)
        git rm -q "$2"
        shift 2
        ;;
      *)
        echo "make_commit: unknown action $1" >&2
        return 2
        ;;
    esac
  done
  git commit -q -F "$msgfile"
}

# --- case 1: matching block → pass ---------------------------------------

cat > /tmp/msg.$$ <<'EOF'
feat: matching case

This commit's claim matches the diff.

## Files
A foo.txt +5
EOF
make_commit /tmp/msg.$$ add foo.txt 5
rm /tmp/msg.$$
assert_gate "matching block passes" 0 "matches diff" HEAD

# --- case 2: claimed file not in diff → fail -----------------------------

cat > /tmp/msg.$$ <<'EOF'
feat: fictional claim

Claims a file that's not in the diff (the PR #52 failure pattern).

## Files
A bar.txt +10
A imaginary.rs +500
EOF
make_commit /tmp/msg.$$ add bar.txt 10
rm /tmp/msg.$$
assert_gate "claimed-but-missing fails" 1 "claimed file not in diff: imaginary.rs" HEAD

# --- case 3: file in diff but not claimed → fail -------------------------

cat > /tmp/msg.$$ <<'EOF'
feat: incomplete claim

Touches two files but only claims one.

## Files
M foo.txt +3
EOF
make_commit /tmp/msg.$$ mod foo.txt 3 mod bar.txt 2
rm /tmp/msg.$$
assert_gate "unclaimed diff fails" 1 "diff touched bar.txt but it was not claimed" HEAD

# --- case 4: status mismatch (A vs M) → fail -----------------------------

cat > /tmp/msg.$$ <<'EOF'
feat: status lies

Claims A but the file already existed (M).

## Files
A foo.txt +1
EOF
make_commit /tmp/msg.$$ mod foo.txt 1
rm /tmp/msg.$$
assert_gate "status mismatch fails" 1 "status mismatch for foo.txt: claim=A actual=M" HEAD

# --- case 5: LOC drift >20% → warn (exit 0) ------------------------------

cat > /tmp/msg.$$ <<'EOF'
feat: loc drift

Claim is off by >20% on LOC counts; should warn but exit 0.

## Files
M foo.txt +100
EOF
make_commit /tmp/msg.$$ mod foo.txt 5
rm /tmp/msg.$$
assert_gate "LOC drift warns (exit 0)" 0 "WARN.*added LOC drift" HEAD

# --- case 6: no ## Files block → exit 0 with note ------------------------

cat > /tmp/msg.$$ <<'EOF'
feat: legacy commit, no claim block

This commit has no ## Files section — opt-in strictness means we skip.
EOF
make_commit /tmp/msg.$$ mod foo.txt 1
rm /tmp/msg.$$
assert_gate "no block exits 0 with note" 0 "no .## Files. block — skipping" HEAD

# --- case 7: MU_SKIP_CLAIM_CHECK=1 bypasses -----------------------------

# Make a commit that WOULD fail without the bypass.
cat > /tmp/msg.$$ <<'EOF'
feat: would fail without bypass

## Files
A nonexistent-file.rs +999
EOF
make_commit /tmp/msg.$$ mod foo.txt 1
rm /tmp/msg.$$
assert_gate "bypass env var skips" 0 "MU_SKIP_CLAIM_CHECK=1" HEAD MU_SKIP_CLAIM_CHECK=1

# --- case 8: bad commit ref → exit 2 ------------------------------------

assert_gate "bad commit ref returns 2" 2 "is not a commit" "definitely-not-a-commit"

# --- case 9: git trailers after ## Files block are ignored (mu-d33g) ----
# Regression: before the fix, lines like "Co-Authored-By: Claude ..." after
# the ## Files block were parsed as Files entries with status=C path=Claude,
# causing "claimed file not in diff" failures on any commit with git trailers.

cat > /tmp/msg.$$ <<'EOF'
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
make_commit /tmp/msg.$$ add trailer-test.txt 5
rm /tmp/msg.$$
assert_gate "trailers after files block ignored" 0 "matches diff" HEAD

# --- summary -------------------------------------------------------------

printf "\n%d passed, %d failed\n" "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
