#!/usr/bin/env bash
# verify-claims.sh — verify a commit's `## Files` block matches its actual diff.
#
# bead: mu-b5kl. Failure pattern: a worker writes a commit message claiming
# file changes that don't match the actual diff. PR #52 was the canonical case
# (claimed a 6-file refactor; actual diff was unrelated bleed-through). This
# gate compares the two and fails on path mismatch.
#
# VCS-agnostic: the gate runs in two contexts that use different VCSs —
#   * locally it is invoked from a jj workspace (tcovert works in jj; a
#     secondary jj workspace has no `.git` in cwd, so raw-git calls fail there);
#   * in CI it runs on a plain `actions/checkout` (git, no jj).
# So it detects the backend (prefer jj when in a jj workspace, else git — the
# same rule pre-pr-check.sh uses) and pulls the commit metadata + diff from
# whichever is present. Every backend normalizes to the same two tuple streams
#   name-status:  <path>\t<status>
#   per-file LOC: <path>\t<added>\t<deleted>
# and the `## Files`/trailer parsing + the claim↔actual comparison (pure awk)
# are shared, backend-independent. Override detection with MU_VERIFY_VCS=jj|git.
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

# --- bypass ----------------------------------------------------------------

if [ "${MU_SKIP_CLAIM_CHECK:-}" = "1" ]; then
  echo "verify-claims: MU_SKIP_CLAIM_CHECK=1 — skipping ${1:-}" >&2
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

# --- backend detection -----------------------------------------------------
#
# Prefer jj when invoked from inside a jj workspace (incl. colocated repos,
# where jj's view is the authoritative one); otherwise use git. This mirrors
# pre-pr-check.sh's `command -v jj && jj root` rule. MU_VERIFY_VCS forces one.

VCS="${MU_VERIFY_VCS:-}"
if [ -z "$VCS" ]; then
  if command -v jj >/dev/null 2>&1 && jj root >/dev/null 2>&1; then
    VCS="jj"
  elif git rev-parse --git-dir >/dev/null 2>&1; then
    VCS="git"
  else
    echo "${C_RED}verify-claims: no jj workspace or git repo found in $(pwd)${C_OFF}" >&2
    exit 2
  fi
fi

case "$VCS" in
  jj)  COMMIT="${1:-@}" ;;
  git) COMMIT="${1:-HEAD}" ;;
  *)   echo "${C_RED}verify-claims: MU_VERIFY_VCS must be jj or git (got '$VCS')${C_OFF}" >&2; exit 2 ;;
esac

# --- backend wrappers ------------------------------------------------------
#
# Each emits a backend-neutral result; downstream logic is identical for both.

# Short, stable id for diagnostics. Exit non-zero if the rev doesn't resolve to
# exactly one commit (the claim target is a single commit).
vcs_short() {
  if [ "$VCS" = "jj" ]; then
    local s
    s="$(jj log --no-graph -r "$COMMIT" -T 'commit_id.short(12) ++ "\n"' 2>/dev/null)" || return 1
    [ -n "$s" ] || return 1
    [ "$(printf "%s\n" "$s" | grep -c .)" -eq 1 ] || return 2   # multiple commits
    printf "%s\n" "$s"
  else
    git rev-parse --verify "$COMMIT^{commit}" >/dev/null 2>&1 || return 1
    git rev-parse --short=12 "$COMMIT"
  fi
}

vcs_parent_count() {
  if [ "$VCS" = "jj" ]; then
    jj log --no-graph -r "$COMMIT" -T 'parents.len() ++ "\n"' 2>/dev/null
  else
    git rev-list --parents -n 1 "$COMMIT" | awk '{print NF-1}'
  fi
}

vcs_message() {
  if [ "$VCS" = "jj" ]; then
    jj log --no-graph -r "$COMMIT" -T 'description' 2>/dev/null
  else
    git log -1 --format=%B "$COMMIT" 2>/dev/null
  fi
}

# name-status → `<path>\t<status>` (single-letter status; renames as new path).
vcs_name_status() {
  if [ "$VCS" = "jj" ]; then
    # jj `--summary` lines are `<L> <pathspec>` (SPACE-delimited; git used TAB).
    # Renames/copies use git path-compression in the pathspec — `R {old => new}`,
    # `dir/{old => new}/file`, etc. — which we expand to the NEW path (the claim
    # convention records renames as the new path).
    jj diff -r "$COMMIT" --summary 2>/dev/null | awk '
      NF == 0 { next }
      {
        status = substr($0, 1, 1)
        sp = index($0, " ")                 # path = everything after 1st space
        pathspec = (sp > 0) ? substr($0, sp + 1) : ""
        if (pathspec == "") next
        if (match(pathspec, /\{[^}]*\}/)) { # pre{old => new}post  ->  pre+new+post
          pre   = substr(pathspec, 1, RSTART - 1)
          brace = substr(pathspec, RSTART, RLENGTH)
          post  = substr(pathspec, RSTART + RLENGTH)
          inner = substr(brace, 2, length(brace) - 2)   # old => new
          ai = index(inner, " => ")
          newpart = (ai > 0) ? substr(inner, ai + 4) : inner
          path = pre newpart post
        } else if (index(pathspec, " => ") > 0) {       # brace-less rename form
          ai = index(pathspec, " => ")
          path = substr(pathspec, ai + 4)
        } else {
          path = pathspec
        }
        printf "%s\t%s\n", path, status
      }'
  else
    # git name-status: `<status>\t<path>` (rename: `<status>\t<old>\t<new>`).
    git diff-tree -r --name-status --no-commit-id "$COMMIT" | awk '
      BEGIN { FS = "\t" }
      NF > 0 { printf "%s\t%s\n", $NF, substr($1, 1, 1) }   # $NF = new path
    '
  fi
}

# per-file LOC → `<path>\t<added>\t<deleted>`.
vcs_loc() {
  if [ "$VCS" = "jj" ]; then
    # jj has no `--numstat`; sum +/- lines from the unified `--git` diff. The
    # `in_hunk` flag keeps the `+++ `/`--- ` file headers (before the first `@@`)
    # from being miscounted as content (a content line can start with `+++ `).
    jj diff -r "$COMMIT" --git 2>/dev/null | awk '
      /^diff --git / { in_hunk = 0; cur = ""; next }
      !in_hunk && /^\+\+\+ / {
        p = substr($0, 5)
        if (p != "/dev/null") { sub(/^b\//, "", p); cur = p }
        next
      }
      !in_hunk && /^--- / {
        p = substr($0, 5)
        if (cur == "" && p != "/dev/null") { sub(/^a\//, "", p); cur = p }
        next
      }
      /^@@ / { in_hunk = 1; next }
      in_hunk && /^\+/ { if (cur != "") added[cur]++;   next }
      in_hunk && /^-/  { if (cur != "") deleted[cur]++; next }
      END {
        for (p in added)   seen[p] = 1
        for (p in deleted) seen[p] = 1
        for (p in seen) {
          a = (p in added)   ? added[p]   : 0
          d = (p in deleted) ? deleted[p] : 0
          printf "%s\t%d\t%d\n", p, a, d
        }
      }'
  else
    # git numstat: `<added>\t<deleted>\t<path>` (binary files report `-`).
    git diff-tree -r --numstat --no-commit-id "$COMMIT" | awk '
      BEGIN { FS = "\t" }
      NF >= 3 {
        a = $1; d = $2
        if (a == "-") a = 0
        if (d == "-") d = 0
        printf "%s\t%s\t%s\n", $3, a, d
      }
    '
  fi
}

# --- sanity: resolve commit + skip merges ---------------------------------

SHA="$(vcs_short)" || {
  echo "${C_RED}verify-claims: $COMMIT is not a commit${C_OFF}" >&2
  exit 2
}
if [ -z "$SHA" ]; then
  echo "${C_RED}verify-claims: $COMMIT is not a commit${C_OFF}" >&2
  exit 2
fi

# A merge commit has 2+ parents; the combined diff isn't a meaningful claim target.
PARENT_COUNT="$(vcs_parent_count)"
if [ "${PARENT_COUNT:-0}" -gt 1 ]; then
  echo "${C_DIM}verify-claims: $SHA is a merge commit ($PARENT_COUNT parents) — skipping${C_OFF}" >&2
  exit 0
fi

# --- extract ## Files block from the commit message -----------------------

MSG="$(vcs_message)"
if [ -z "$MSG" ]; then
  echo "${C_RED}verify-claims: empty commit message for $SHA${C_OFF}" >&2
  exit 2
fi

# Block starts at a line `## Files` and ends at the next `## ` heading or EOF.
# Blank lines inside the block are allowed and ignored. Lines starting with `#`
# (other than the heading delimiter) are treated as comments and ignored.
# Git trailer lines (`Token: value` where Token is letters/digits/underscore/hyphen)
# are also skipped — e.g., `Co-Authored-By:`, `Signed-off-by:`, `Reviewed-by:`,
# `Reported-by:`. This matters because the trailer section conventionally appears
# at the END of the commit message, after the body, so a `## Files` block that's
# the last body section will see the trailer lines in its scan. Without this skip
# the trailer would be parsed as a Files entry (e.g. "Co-Authored-By:" parsed as
# status=C, path=Claude — see bead mu-d33g for the regression that motivated this).
CLAIM_BLOCK="$(printf "%s\n" "$MSG" | awk '
  /^## Files[[:space:]]*$/ { in_block = 1; next }
  in_block && /^## / { in_block = 0 }
  in_block && /^[[:space:]]*$/ { next }
  in_block && /^[[:space:]]*#/ { next }
  in_block && /^[A-Za-z][A-Za-z0-9_-]*:[[:space:]]/ { next }
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

# --- gather actual diff: name-status + per-file LOC -----------------------

SUMMARY_TUPLES="$(vcs_name_status)"   # <path>\t<status>
LOC_TUPLES="$(vcs_loc)"               # <path>\t<added>\t<deleted>

# Join the two streams by path. Feed both to one awk via stdin with a separator
# line — awk -v rejects embedded newlines in many implementations (gawk, FreeBSD
# awk). Status comes from name-status; a file with no content change (pure
# rename / mode change) simply has LOC 0.
ACTUAL_TUPLES="$( {
  echo "@SUMMARY"
  printf "%s\n" "$SUMMARY_TUPLES"
  echo "@LOC"
  printf "%s\n" "$LOC_TUPLES"
} | awk '
  BEGIN { FS = "\t" }
  /^@SUMMARY$/ { mode = "sum"; next }
  /^@LOC$/     { mode = "loc"; next }
  mode == "sum" && NF >= 2 { status[$1] = $2 }
  mode == "loc" && NF >= 3 { added[$1] = $2; deleted[$1] = $3 }
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
#   first stream  → CLAIM_TUPLES
#   second stream → ACTUAL_TUPLES
# Outputs (on stdout): one diagnostic per line, prefixed with FAIL: or WARN:.

DIAGS="$(awk -v sha="$SHA" '
  function abs(x) { return x < 0 ? -x : x }
  function pct_drift(c, a,    m) {
    m = (c > a) ? c : a
    if (m == 0) return 0
    return abs(c - a) * 100 / m
  }
  $1 == "" { next }   # ignore the trailing blank line from empty tuple streams
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
  printf "%sverify-claims: $SHA failed — claim block must match the actual diff%s\n" "$C_RED" "$C_OFF" >&2
  exit 1
fi

# Warnings only — gate passes.
exit 0
