#!/usr/bin/env bash
# verify-claims.sh — verify a commit's `## Files` block matches its actual diff.
#
# bead: mu-b5kl. Failure pattern: a worker writes a commit message claiming
# file changes that don't match `git diff-tree --name-status`. PR #52 was the
# canonical case (claimed a 6-file refactor; actual diff was unrelated bleed-
# through). This gate compares the two and fails on path mismatch.
#
# Block format (in the commit message body):
#
#   ## Files
#   A path/to/file.rs +562
#   M path/to/other.rs +5 -3
#   D path/to/removed.rs
#
# Strictness model: OPT-IN. A commit with NO `## Files` block exits 0 with a
# one-line note. Workers (and humans) opt in by emitting the block. The
# goal-protocol skill teaches workers to always emit it.
#
# Exit codes: 0 = pass / opt-out / skipped; 1 = claim/reality mismatch; 2 = usage error.
#
# Bypass: MU_SKIP_CLAIM_CHECK=1 verify-claims.sh ...

set -u
set -o pipefail

# --- args + bypass ---------------------------------------------------------

COMMIT="${1:-HEAD}"

if [ "${MU_SKIP_CLAIM_CHECK:-}" = "1" ]; then
  echo "verify-claims: MU_SKIP_CLAIM_CHECK=1 — skipping $COMMIT" >&2
  exit 0
fi

# --- color setup (mirrors pre-pr-check.sh) --------------------------------

if [ -t 2 ] && [ -z "${PRE_PR_NO_COLOR:-}" ]; then
  C_RED=$'\033[31m'
  C_GREEN=$'\033[32m'
  C_YELLOW=$'\033[33m'
  C_DIM=$'\033[2m'
  C_OFF=$'\033[0m'
else
  C_RED=""; C_GREEN=""; C_YELLOW=""; C_DIM=""; C_OFF=""
fi

# --- sanity: resolve commit + skip merges ---------------------------------

if ! git rev-parse --verify "$COMMIT^{commit}" >/dev/null 2>&1; then
  echo "${C_RED}verify-claims: $COMMIT is not a commit${C_OFF}" >&2
  exit 2
fi

# Resolve to a stable sha for diagnostics
SHA="$(git rev-parse --short=12 "$COMMIT")"

# A merge commit has 2+ parents; the combined diff isn't a meaningful claim target.
PARENT_COUNT="$(git rev-list --parents -n 1 "$COMMIT" | awk '{print NF-1}')"
if [ "$PARENT_COUNT" -gt 1 ]; then
  echo "${C_DIM}verify-claims: $SHA is a merge commit ($PARENT_COUNT parents) — skipping${C_OFF}" >&2
  exit 0
fi

# --- extract ## Files block from the commit message -----------------------

MSG="$(git log -1 --format=%B "$COMMIT" 2>/dev/null)"
if [ -z "$MSG" ]; then
  echo "${C_RED}verify-claims: empty commit message for $SHA${C_OFF}" >&2
  exit 2
fi

# Block starts at a line `## Files` and ends at the next `## ` heading or EOF.
# Blank lines inside the block are allowed and ignored. Lines starting with `#`
# (other than the heading delimiter) are treated as comments and ignored.
CLAIM_BLOCK="$(printf "%s\n" "$MSG" | awk '
  /^## Files[[:space:]]*$/ { in_block = 1; next }
  in_block && /^## / { in_block = 0 }
  in_block && /^[[:space:]]*$/ { next }
  in_block && /^[[:space:]]*#/ { next }
  in_block { print }
')"

if [ -z "$CLAIM_BLOCK" ]; then
  echo "${C_DIM}verify-claims: $SHA has no \`## Files\` block — skipping (opt-in strictness)${C_OFF}" >&2
  exit 0
fi

# --- parse claim block into (path, status, added, deleted) tuples ---------
#
# Format per line:  <STATUS> <PATH> [+<added>] [-<deleted>]
#   STATUS: single letter A/M/D/R/C/T (we keep only the first char of $1).
#   PATH:   single token (no spaces). Renames recorded as the new path.
#   ±<n>:   optional LOC numbers; missing fields treated as 0.

CLAIM_TUPLES="$(printf "%s\n" "$CLAIM_BLOCK" | awk '
  {
    status = substr($1, 1, 1)
    path = $2
    added = 0
    deleted = 0
    for (i = 3; i <= NF; i++) {
      ch = substr($i, 1, 1)
      n  = substr($i, 2) + 0
      if (ch == "+") added = n
      else if (ch == "-") deleted = n
    }
    if (path == "") {
      printf "PARSE_ERROR line: %s\n", $0 > "/dev/stderr"
      bad = 1
      next
    }
    printf "%s\t%s\t%d\t%d\n", path, status, added, deleted
  }
  END { if (bad) exit 2 }
')" || {
  echo "${C_RED}verify-claims: $SHA \`## Files\` block has unparseable lines (see above)${C_OFF}" >&2
  exit 2
}

# --- gather actual diff: name-status + numstat ----------------------------

ACTUAL_NS="$(git diff-tree -r --name-status --no-commit-id "$COMMIT")"
ACTUAL_NUM="$(git diff-tree -r --numstat       --no-commit-id "$COMMIT")"

# Join by path. name-status fields: <status>\t<path> (or <status>\t<old>\t<new> on rename).
# numstat fields: <added>\t<deleted>\t<path>. Build (path, status, added, deleted) tuples
# using awk associative arrays so we don't depend on row-order alignment.
#
# Feed both streams to awk via stdin (with a separator line) — awk -v rejects
# embedded newlines in many implementations (gawk, FreeBSD awk).
ACTUAL_TUPLES="$( {
  echo "@NS"
  printf "%s\n" "$ACTUAL_NS"
  echo "@NUM"
  printf "%s\n" "$ACTUAL_NUM"
} | awk '
  BEGIN { FS = "\t" }
  /^@NS$/  { mode = "ns";  next }
  /^@NUM$/ { mode = "num"; next }
  mode == "ns"  && NF > 0 { status[$NF] = substr($1, 1, 1) }
  mode == "num" && NF > 0 {
    a = $1; d = $2
    if (a == "-") a = 0
    if (d == "-") d = 0
    added[$NF]   = a
    deleted[$NF] = d
  }
  END {
    for (p in status) {
      a = (p in added)   ? added[p]   : 0
      d = (p in deleted) ? deleted[p] : 0
      printf "%s\t%s\t%s\t%s\n", p, status[p], a, d
    }
  }
')"

# --- compare claim ↔ actual ----------------------------------------------

# Use awk to do the diff. Inputs:
#   /dev/fd/3 → CLAIM_TUPLES
#   /dev/fd/4 → ACTUAL_TUPLES
# Outputs (on stdout): one diagnostic per line, prefixed with FAIL: or WARN:.

DIAGS="$(awk -v sha="$SHA" '
  function abs(x) { return x < 0 ? -x : x }
  function pct_drift(c, a,    m) {
    m = (c > a) ? c : a
    if (m == 0) return 0
    return abs(c - a) * 100 / m
  }
  NR == FNR {
    # First stream: claims
    c_status[$1] = $2
    c_added[$1]  = $3
    c_deleted[$1] = $4
    claim_paths[$1] = 1
    next
  }
  {
    # Second stream: actual
    a_status[$1]  = $2
    a_added[$1]   = $3
    a_deleted[$1] = $4
    actual_paths[$1] = 1
  }
  END {
    # Paths claimed but not in actual diff
    for (p in claim_paths) {
      if (!(p in actual_paths)) {
        printf "FAIL: claimed file not in diff: %s (status %s)\n", p, c_status[p]
        bad = 1
        continue
      }
      # Status mismatch
      if (c_status[p] != a_status[p]) {
        printf "FAIL: status mismatch for %s: claim=%s actual=%s\n", p, c_status[p], a_status[p]
        bad = 1
        continue
      }
      # LOC drift (warn only; >20%)
      ad = pct_drift(c_added[p],   a_added[p])
      dd = pct_drift(c_deleted[p], a_deleted[p])
      if (ad > 20) printf "WARN: added LOC drift for %s: claim=+%d actual=+%d (%.0f%%)\n",   p, c_added[p],   a_added[p],   ad
      if (dd > 20) printf "WARN: deleted LOC drift for %s: claim=-%d actual=-%d (%.0f%%)\n", p, c_deleted[p], a_deleted[p], dd
    }
    # Paths in actual but not claimed
    for (p in actual_paths) {
      if (!(p in claim_paths)) {
        printf "FAIL: diff touched %s but it was not claimed (status %s)\n", p, a_status[p]
        bad = 1
      }
    }
    if (bad) exit 1
    exit 0
  }
' <(printf "%s\n" "$CLAIM_TUPLES") <(printf "%s\n" "$ACTUAL_TUPLES"))"
rc=$?

# --- emit + exit ----------------------------------------------------------

if [ -z "$DIAGS" ]; then
  echo "${C_GREEN}verify-claims: $SHA \`## Files\` block matches diff${C_OFF}" >&2
  exit 0
fi

# Split FAIL / WARN
WARNS="$(printf "%s\n" "$DIAGS" | grep '^WARN:' || true)"
FAILS="$(printf "%s\n" "$DIAGS" | grep '^FAIL:' || true)"

if [ -n "$WARNS" ]; then
  printf "%s\n" "$WARNS" | sed "s/^/${C_YELLOW}verify-claims: $SHA: /;s/\$/${C_OFF}/" >&2
fi

if [ -n "$FAILS" ]; then
  printf "%s\n" "$FAILS" | sed "s/^/${C_RED}verify-claims: $SHA: /;s/\$/${C_OFF}/" >&2
  printf "%sverify-claims: $SHA failed — claim block must match git diff-tree output%s\n" "$C_RED" "$C_OFF" >&2
  exit 1
fi

# Warnings only — gate passes.
exit 0
