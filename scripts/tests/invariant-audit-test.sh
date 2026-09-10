#!/usr/bin/env bash
# invariant-audit-test.sh — scripts/review-panel/invariant_audit.py, against a
# throwaway git repo with its own rules file.
#
# bead: mu-review-gate-seam-reviewers-9vkbt.6. Hermetic and free: no network, no
# model, no mu binary — the audit is stdlib Python. The fixture repo uses its own
# invariants.toml and its own baseline under a tmpdir, and inherits none of the
# host's git config (GIT_CONFIG_NOSYSTEM / GIT_CONFIG_GLOBAL), so nothing on the
# box can change a case. Run from any cwd; creates its own tmpdir.

set -u
set -o pipefail

TEST_DIR="$(cd "$(dirname "$0")" && pwd)"
AUDIT="$TEST_DIR/../review-panel/invariant_audit.py"
[ -f "$AUDIT" ] || { echo "invariant-audit-test: audit not found at $AUDIT" >&2; exit 2; }
command -v python3 >/dev/null 2>&1 || { echo "invariant-audit-test: python3 missing — skipping"; exit 0; }
command -v git >/dev/null 2>&1 || { echo "invariant-audit-test: git missing" >&2; exit 2; }

export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null

PASS=0
FAIL=0
TMP="$(mktemp -d "${TMPDIR:-/tmp}/invariant-audit.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
REPO="$TMP/repo"
RULES="$TMP/rules.toml"
BASE="$TMP/baseline"

ok()  { printf 'PASS: %s\n' "$1"; PASS=$((PASS + 1)); }
bad() { printf 'FAIL: %s\n  %s\n' "$1" "$2" >&2; FAIL=$((FAIL + 1)); }

# run <expected-rc> <name> -- <audit args...>: run the audit, check exit code,
# leave stdout in $OUT for the caller to grep.
OUT=""
run() {
  local exp="$1" name="$2"; shift 3   # drop the literal --
  OUT="$(python3 "$AUDIT" --root "$REPO" --rules "$RULES" --baseline "$BASE" "$@" 2>"$TMP/err")"
  local rc=$?
  if [ "$rc" -ne "$exp" ]; then
    bad "$name" "exit=$rc expected=$exp; stderr=$(cat "$TMP/err"); stdout=$OUT"
    return 1
  fi
  return 0
}

# --- fixture rules: one regex shape, one forbidden_path shape ---------------
cat > "$RULES" <<'EOF'
[[rule]]
id = "R1"
title = "forbidden token"
kind = "regex"
pattern = ['FORBIDDEN_TOKEN']
include = ["**/*.rs"]
exclude = ["**/tests/**"]
severity = "high"
why = "test regex shape"

[[rule]]
id = "R2"
title = "doc in bad/"
kind = "forbidden_path"
pattern = ["bad/*.md"]
severity = "medium"
why = "test forbidden_path shape"
EOF

# --- fixture repo ----------------------------------------------------------
mkdir -p "$REPO"
cd "$REPO" || exit 2
git init -q -b main . 2>/dev/null || { git init -q . && git checkout -qb main; }
git config user.email audit@test
git config user.name audit
git config commit.gpgSign false
git config core.hooksPath /dev/null

mkdir -p src tests bad good
printf 'fn f() { let x = FORBIDDEN_TOKEN; }\n' > src/hit.rs
printf 'fn g() { ok(); }\n'                    > src/clean.rs
printf 'fn t() { FORBIDDEN_TOKEN; }\n'         > tests/ignored.rs   # excluded by glob
printf 'FORBIDDEN_TOKEN in prose\n'            > notes.txt          # not *.rs, not included
printf '# design\n'                            > bad/design.md      # forbidden_path hit
printf '# fine\n'                              > good/design.md     # not under bad/
printf '# readme\n'                            > bad/README.md      # matches bad/*.md too
git add -A && git commit -qm "base"
BASE_REV="$(git rev-parse HEAD)"

# 1. --all: regex hit fires, excluded/non-included files do not; forbidden_path
#    fires on bad/design.md (and bad/README.md — the fixture rule has no README
#    carve-out, unlike mu's real rule). New sites => exit 1.
rm -f "$BASE"
if run 1 "all: new sites fail" -- --all; then
  printf '%s\n' "$OUT" | grep -q '^INVARIANT R1|high|src/hit.rs:1|forbidden token|fn f() { let x = FORBIDDEN_TOKEN; }$' \
    && ok "regex hit reported with severity/loc/text" \
    || bad "regex hit reported with severity/loc/text" "$OUT"
  printf '%s\n' "$OUT" | grep -q 'tests/ignored.rs' && bad "excluded path must not fire" "$OUT" || ok "excluded path (tests/) does not fire"
  printf '%s\n' "$OUT" | grep -q 'notes.txt'        && bad "non-included path must not fire" "$OUT" || ok "non-included path (notes.txt) does not fire"
  printf '%s\n' "$OUT" | grep -q '^INVARIANT R2|medium|bad/design.md:0|doc in bad/|bad/design.md$' \
    && ok "forbidden_path hit reported" || bad "forbidden_path hit reported" "$OUT"
fi

# 2. --changed scoping: only the named files are scanned, so the forbidden_path
#    site is invisible when only a .rs file is passed.
rm -f "$BASE"
if run 1 "changed: scopes to named files" -- --changed src/hit.rs; then
  printf '%s\n' "$OUT" | grep -q '^INVARIANT R1|.*src/hit.rs' && ok "changed: regex hit in scope" || bad "changed: regex hit in scope" "$OUT"
  printf '%s\n' "$OUT" | grep -q 'bad/design.md' && bad "changed: out-of-scope site must not fire" "$OUT" || ok "changed: out-of-scope forbidden_path invisible"
fi

# 2b. A clean changed file => no sites, exit 0.
rm -f "$BASE"
run 0 "changed: clean file passes" -- --changed src/clean.rs && ok "changed: clean file exits 0" || true

# 3. --changed-from REV (git branch): add a new violating file on a branch.
git checkout -qb feature
printf 'let y = FORBIDDEN_TOKEN;\n' > src/added.rs
git add -A && git commit -qm "add violation"
rm -f "$BASE"
if run 1 "changed-from: diffs vs a revision" -- --changed-from "$BASE_REV"; then
  printf '%s\n' "$OUT" | grep -q 'src/added.rs' && ok "changed-from: new file in the diff fires" || bad "changed-from: new file in the diff fires" "$OUT"
  printf '%s\n' "$OUT" | grep -q 'src/hit.rs'   && bad "changed-from: unchanged file must not fire" "$OUT" || ok "changed-from: unchanged file excluded from the diff"
fi
git checkout -q main; git branch -qD feature

# 3b. --to pins BOTH the far end of the range and the revision file CONTENT is
#     read at. (i) A violation present only at a pinned commit — and gone from the
#     working tree — is still found when --to names that commit.
git checkout -qb pinned
printf 'let p = FORBIDDEN_TOKEN;\n' > src/pinned.rs
git add -A && git commit -qm "pinned violation"
PIN_REV="$(git rev-parse HEAD)"
git rm -q src/pinned.rs && git commit -qm "remove pinned from the tree"
rm -f "$BASE"
if run 1 "to: violation at a pinned rev is found though gone from the tree" -- --changed-from "$BASE_REV" --to "$PIN_REV"; then
  printf '%s\n' "$OUT" | grep -q 'src/pinned.rs' && ok "to: pinned-commit-only violation found via --to" || bad "to: pinned-commit-only violation found via --to" "$OUT"
  [ -f "$REPO/src/pinned.rs" ] && bad "to: fixture invariant — file should be gone from the tree" "present" || ok "to: the violation is genuinely absent from the working tree"
fi
git checkout -q main; git branch -qD pinned

# 3c. (ii) A violation present ONLY in the working tree is NOT found: content is
#     read at REV, never from disk. clean.rs is benignly touched on a branch so it
#     IS in the BASE..REV diff, then a violation is added to it in the tree only.
git checkout -qb worktree
printf 'fn g() { ok(); } // touched\n' > src/clean.rs
git add -A && git commit -qm "touch clean.rs"
WT_REV="$(git rev-parse HEAD)"
printf 'fn g() { ok(); } // touched\nlet w = FORBIDDEN_TOKEN;\n' > src/clean.rs   # tree-only, uncommitted
rm -f "$BASE"
if run 0 "to: a tree-only violation in a diffed file is not found" -- --changed-from "$BASE_REV" --to "$WT_REV"; then
  printf '%s\n' "$OUT" | grep -q 'src/clean.rs' && bad "to: tree-only violation must not fire (content read at REV)" "$OUT" || ok "to: tree-only violation not reported (content read at REV)"
fi
git checkout -q -- src/clean.rs                 # discard the tree-only edit
git checkout -q main; git branch -qD worktree

# 3d. --to without --changed-from is a usage error (exit 2).
python3 "$AUDIT" --root "$REPO" --rules "$RULES" --baseline "$BASE" --all --to "$BASE_REV" >/dev/null 2>"$TMP/err"; rc=$?
[ "$rc" -eq 2 ] && ok "to: --to without --changed-from exits 2" || bad "to: --to without --changed-from exits 2" "rc=$rc err=$(cat "$TMP/err")"

# 4. Baseline ratchet: --update-baseline records every current site; the re-run
#    then passes (exit 0) with the same tree.
run 0 "update-baseline writes and exits 0" -- --all --update-baseline && {
  grep -q '^R1 src/hit.rs ' "$BASE" && ok "baseline records the regex site (no line number)" || bad "baseline records the regex site" "$(cat "$BASE")"
  grep -q '^R2 bad/design.md ' "$BASE" && ok "baseline records the forbidden_path site" || bad "baseline records the forbidden_path site" "$(cat "$BASE")"
}
run 0 "baselined sites pass" -- --all && ok "known sites exit 0 against the baseline" || bad "known sites exit 0" "$OUT"

# 4b. A brand-new site (not in the baseline) still fails even when the rest are baselined.
printf 'let z = FORBIDDEN_TOKEN;\n' > src/fresh.rs
git add -A && git commit -qm "fresh violation"
if run 1 "new site over a baseline fails" -- --all; then
  printf '%s\n' "$OUT" | grep -q 'src/fresh.rs' && ok "the new site is the one reported" || bad "the new site is the one reported" "$OUT"
  printf '%s\n' "$OUT" | grep -q 'src/hit.rs'   && bad "baselined sites must stay silent" "$OUT" || ok "baselined sites stay silent"
fi
git rm -q src/fresh.rs; git commit -qm "drop fresh"

# 4c. Fixed site: a baseline entry whose site is gone is reported, exit stays 0.
printf 'R9 gone/removed.rs deadbeefdeadbeef\n' >> "$BASE"
if run 0 "a fixed baseline entry is reported, run still passes" -- --all; then
  printf '%s\n' "$OUT" | grep -q '^fixed: remove from baseline: R9 gone/removed.rs deadbeefdeadbeef$' \
    && ok "fixed site reported for removal" || bad "fixed site reported for removal" "$OUT"
fi

# 4d. Comment and blank lines in the baseline are ignored.
{ printf '\n'; printf '# a comment\n'; printf '   \n'; } >> "$BASE"
run 0 "comment/blank baseline lines are ignored" -- --all && ok "baseline comments/blanks ignored (still exit 0)" || bad "baseline comments/blanks ignored" "$OUT"

# 5. --json shape: valid JSON with the documented keys.
rm -f "$BASE"
if run 1 "json emits one object" -- --all --json; then
  printf '%s' "$OUT" | python3 -c '
import json,sys
o=json.load(sys.stdin)
assert isinstance(o["new"], list) and o["new"], o
assert set(o["new"][0]) == {"id","severity","file","line","title","match"}, o["new"][0]
assert o["exit"] == 1 and isinstance(o["fixed"], list) and "baselined" in o, o
' && ok "json has new[]/fixed[]/baselined/exit with the right finding keys" || bad "json shape" "$OUT"
fi

# 6. A malformed rules file is a usage error (exit 2), not a silent pass.
BADRULES="$TMP/bad-rules.toml"
printf '[[rule]\nid = "X"\n' > "$BADRULES"
python3 "$AUDIT" --root "$REPO" --rules "$BADRULES" --baseline "$BASE" --all >/dev/null 2>"$TMP/err"; rc=$?
[ "$rc" -eq 2 ] && ok "malformed rules file exits 2" || bad "malformed rules file exits 2" "rc=$rc err=$(cat "$TMP/err")"

# 6b. An unknown kind is also a rules error (exit 2).
printf '[[rule]]\nid="Y"\ntitle="t"\nkind="bogus"\npattern=["x"]\n' > "$BADRULES"
python3 "$AUDIT" --root "$REPO" --rules "$BADRULES" --baseline "$BASE" --all >/dev/null 2>"$TMP/err"; rc=$?
[ "$rc" -eq 2 ] && ok "unknown rule kind exits 2" || bad "unknown rule kind exits 2" "rc=$rc err=$(cat "$TMP/err")"

# 6c. Choosing zero or two selection modes is a usage error (exit 2).
python3 "$AUDIT" --root "$REPO" --rules "$RULES" --baseline "$BASE" >/dev/null 2>"$TMP/err"; rc=$?
[ "$rc" -eq 2 ] && ok "no selection mode exits 2" || bad "no selection mode exits 2" "rc=$rc err=$(cat "$TMP/err")"

# 7. --gate-severity re-renders the emitted severity in the gate's vocabulary
#    (blocker|should-fix|note) while the rules keep high|medium|low.
rm -f "$BASE"
if run 1 "gate-severity maps high|medium|low -> blocker|should-fix|note" -- --all --gate-severity; then
  printf '%s\n' "$OUT" | grep -q '^INVARIANT R1|blocker|src/hit.rs:1|' && ok "gate-severity: high -> blocker" || bad "gate-severity: high -> blocker" "$OUT"
  printf '%s\n' "$OUT" | grep -q '^INVARIANT R2|should-fix|bad/design.md:0|' && ok "gate-severity: medium -> should-fix" || bad "gate-severity: medium -> should-fix" "$OUT"
fi
# 7b. Default (no flag) keeps the raw high|medium|low vocabulary.
rm -f "$BASE"
if run 1 "no gate-severity keeps raw high|medium|low" -- --all; then
  printf '%s\n' "$OUT" | grep -q '^INVARIANT R1|high|' && ok "default: severity stays high" || bad "default: severity stays high" "$OUT"
fi
# 7c. low -> note, via an isolated one-rule file (the main fixture has no low rule).
LOWRULES="$TMP/low-rules.toml"
cat > "$LOWRULES" <<'EOF'
[[rule]]
id = "L1"
title = "low token"
kind = "regex"
pattern = ['FORBIDDEN_TOKEN']
include = ["**/*.rs"]
exclude = ["**/tests/**"]
severity = "low"
why = "test low"
EOF
rm -f "$BASE"
OUT="$(python3 "$AUDIT" --root "$REPO" --rules "$LOWRULES" --baseline "$BASE" --all --gate-severity 2>"$TMP/err")"
printf '%s\n' "$OUT" | grep -q '^INVARIANT L1|note|' && ok "gate-severity: low -> note" || bad "gate-severity: low -> note (err=$(cat "$TMP/err"))" "$OUT"

printf '\ninvariant-audit-test: %d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
