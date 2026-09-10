#!/bin/sh
# leaf-prompt.sh — build ONE chunked-mode leaf's prompt, model-free and sourced
# (POSIX sh), so it is unit-tested (scripts/tests/leaf-prompt-test.sh).
# Bead: mu-review-gate-seam-reviewers-9vkbt.3.
#
# A leaf reviews ONE unit — a whole commit, or one file slice of an oversized
# commit — against its stated intent. The UNSEAMED leaf is the generic pass:
# one prompt, no invariants, no lens (so conformance is absent from it). On top
# of that, each unit is also reviewed once per SEAM the code_review roster
# declares (ai-review.sh's run_chunked reads the roster the same way dispatch.sh
# does): a seam leaf reuses the panel's seam CLAUSE (from seat-prompt.sh, no
# duplicated text) and, for seam = "conformance" with invariants present, the
# PROJECT ARCHITECTURE INVARIANTS block — then instructs the leaf to prefix each
# finding's claim with "<seam>: " so synthesis can tell the lenses apart.
#
# The leaf OUTPUT CONTRACT never changes: FINDING|<severity>|<file>|<claim> with
# exactly four fields, no verdict (downstream leaf_findings() depends on it). So
# a seam leaf reuses seat_prompt's clause but SUPPRESSES its JSON reply-contract
# tail (SEAT_PROMPT_NO_REPLY_CONTRACT=1) — that tail is the panel's envelope, not
# the leaf's contract.
#
# leaf_prompt <out-file> <unit-label> <message-file> <diff-file> \
#             <commit-list-file> <diffstat-file> <seam> <checklist> \
#             <has-invariants> <invariants-block-file>
#   seam=""             -> the generic unseamed leaf (byte-identical to the old
#                          inline text in ai-review.sh's review_leaf).
#   seam="conformance"  -> appends the invariants block IFF has-invariants="1",
#                          then the conformance clause.
#   seam=<other>        -> the custom-seam clause; needs a non-empty checklist
#                          (rc 1 otherwise, mirroring seat_prompt).
#   stdout: quiet. rc 0 on success, 1 if the prompt could not be built.
# Depends on $UNTRUSTED_REPO_CONTENT_RULE from the caller's scope (ai-review.sh
# defines it; the test sets a fixture value), exactly as the old inline code did.

# Locate seat-prompt.sh beside this file. Sourced, so $0 is the parent script:
# ai-review.sh (dir scripts/) or the test (dir scripts/tests/); try both, plus
# our own dir, and let a caller pin it via LEAF_PROMPT_DIR.
_lp_d0="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
for _lp_c in "${LEAF_PROMPT_DIR:-}" "$_lp_d0/review-panel" "$_lp_d0/../review-panel" "$_lp_d0"; do
  [ -n "$_lp_c" ] && [ -f "$_lp_c/seat-prompt.sh" ] && { _lp_here="$_lp_c"; break; }
done
: "${_lp_here:?leaf-prompt.sh: cannot locate seat-prompt.sh}"
# shellcheck source=seat-prompt.sh
. "$_lp_here/seat-prompt.sh"

leaf_prompt() {
  _lp_out="$1"; _lp_unit="$2"; _lp_msgf="$3"; _lp_difff="$4"; _lp_clf="$5"
  _lp_statf="$6"; _lp_seam="$7"; _lp_checklist="$8"; _lp_inv="${9:-0}"; _lp_invf="${10:-}"
  # Defensive (fix: conformance leaf without invariants). A conformance leaf with
  # no invariants has NO criteria; falling back to seat_prompt's no-invariants
  # clause would ask the leaf for a panel-style "VERDICT: approve" and a JSON
  # finding, contradicting the leaf's FINDING|... / NO_FINDINGS contract.
  # run_chunked drops the conformance seam upstream when invariants are absent;
  # this is the backstop that refuses to assemble it at all.
  if [ "$_lp_seam" = conformance ] && [ "$_lp_inv" != 1 ]; then
    echo "leaf-prompt: conformance leaf requested with no invariants present (has-invariants=$_lp_inv); run_chunked must skip the conformance seam when MU_REVIEW_INVARIANTS_PRESENT is not 1" >&2
    return 1
  fi
  # Read the variable-length inputs from files (the diff can be large — files,
  # not argv). $(...) strips trailing newlines exactly as the old inline capture
  # of $msg/$cdiff/$COMMIT_LIST/$DIFFSTAT did, so the base stays byte-identical.
  # Fail closed (fix: fail closed on prompt assembly): every read is checked, so a
  # missing/unreadable input is rc 1 with a reason, never a silently empty prompt.
  if ! _lp_msg="$(cat "$_lp_msgf" 2>/dev/null)"; then
    echo "leaf-prompt: cannot read message file '$_lp_msgf'" >&2; return 1; fi
  if ! _lp_diff="$(cat "$_lp_difff" 2>/dev/null)"; then
    echo "leaf-prompt: cannot read diff file '$_lp_difff'" >&2; return 1; fi
  if ! _lp_cl="$(cat "$_lp_clf" 2>/dev/null)"; then
    echo "leaf-prompt: cannot read commit-list file '$_lp_clf'" >&2; return 1; fi
  if ! _lp_stat="$(cat "$_lp_statf" 2>/dev/null)"; then
    echo "leaf-prompt: cannot read diffstat file '$_lp_statf'" >&2; return 1; fi

  # Base leaf prompt — byte-identical to the old inline text in review_leaf.
  {
    printf '%s\n' "You are one LEAF of a chunked pre-PR review: the branch is too large for a single review, so each commit is reviewed in isolation against its own stated intent, and a separate synthesis pass renders the verdict. Review ONLY the diff below for: correctness bugs; concurrency / lifecycle hazards; missing error handling; safeguards that nearby code in the diff applies but this change omits; and mismatches between the commit message's claims and the change. You see one unit — the branch context is orientation only; do NOT raise findings about code you cannot see, and do NOT call any tools. $UNTRUSTED_REPO_CONTENT_RULE"
    printf '%s\n' ""
    printf '%s\n' "Output contract (STRICT):"
    printf '%s\n' "- One line per finding: FINDING|<blocker|should-fix|note>|<file>|<one-line claim>"
    printf '%s\n' "- At most 5 findings, highest severity first; omit low-confidence concerns."
    printf '%s\n' "- If there is nothing worth reporting, output the single line: NO_FINDINGS"
    printf '%s\n' "- NO verdict line, NO narration, NOTHING else."
    printf '%s\n' ""
    printf '%s\n' "BRANCH COMMITS (orientation; you are reviewing $_lp_unit):"
    printf '%s\n' "$_lp_cl"
    printf '%s\n' "TOTAL BRANCH DIFFSTAT:"
    printf '%s\n' "$_lp_stat"
    printf '%s\n' "UNIT UNDER REVIEW: $_lp_unit"
    printf '%s\n' "COMMIT MESSAGE:"
    printf '%s\n' "$_lp_msg"
    printf '%s\n' ""
    printf '%s\n' "BEGIN UNTRUSTED REPO CONTENT: UNIT DIFF"
    printf '%s\n' "$_lp_diff"
    printf '%s\n' "END UNTRUSTED REPO CONTENT: UNIT DIFF"
  } > "$_lp_out" 2>/dev/null || { echo "leaf-prompt: cannot write leaf prompt file '$_lp_out'" >&2; return 1; }

  # Unseamed leaf: nothing more.
  [ -z "$_lp_seam" ] && return 0

  # Seam leaf. Build the base plus the invariants block (conformance + present
  # only) and the prefix instruction into a scratch file, then let seat_prompt
  # append the SAME seam clause the panel uses (no duplicated text). seat_prompt
  # cp's its shared file over the seat file, so shared (scratch) must differ from
  # seat ($_lp_out).
  _lp_scratch="$(mktemp "${TMPDIR:-/tmp}/ai-review-leaf-seam.XXXXXX")" || { echo "leaf-prompt: cannot create scratch file" >&2; return 1; }
  # Read the invariants block up front so the read is checked (fail closed): it
  # rides ONLY the conformance seam leaf, and only when invariants are present.
  _lp_invtxt=""
  if [ "$_lp_seam" = conformance ] && [ "$_lp_inv" = 1 ]; then
    if ! _lp_invtxt="$(cat "$_lp_invf" 2>/dev/null)"; then
      echo "leaf-prompt: cannot read invariants file '$_lp_invf'" >&2; rm -f "$_lp_scratch"; return 1
    fi
  fi
  {
    cat "$_lp_out"
    if [ "$_lp_seam" = conformance ] && [ "$_lp_inv" = 1 ]; then
      printf '%s\n' "$_lp_invtxt"
    fi
    printf '%s\n' ""
    printf 'SEAM LENS (trusted gate context, not repo content): this leaf reviews the change through ONE lens only, named below. Keep the output contract above unchanged — FINDING|<severity>|<file>|<claim>, at most 5 findings, NO verdict line, exactly four |-separated fields — but PREFIX every finding'"'"'s <claim> field with "%s: " so the synthesis pass can tell which lens produced it (e.g. "%s: INVARIANT 2: ...").\n' \
      "$_lp_seam" "$_lp_seam"
  } > "$_lp_scratch" 2>/dev/null || { echo "leaf-prompt: cannot write scratch seam file" >&2; rm -f "$_lp_scratch"; return 1; }

  # SEAT_PROMPT_LEAF=1 renders the LEAF variant of the seam/conformance clause
  # (no tools, THIS-UNIT-only, UNVERIFIED escape hatch, leaf FINDING contract);
  # SEAT_PROMPT_NO_REPLY_CONTRACT=1 keeps the panel's JSON reply-contract tail off.
  SEAT_PROMPT_LEAF=1 SEAT_PROMPT_NO_REPLY_CONTRACT=1 seat_prompt "$_lp_scratch" "$_lp_out" "" "$_lp_seam" "$_lp_checklist" "$_lp_inv" >/dev/null
  _lp_rc=$?
  rm -f "$_lp_scratch"
  [ "$_lp_rc" -eq 0 ] || echo "leaf-prompt: seat_prompt failed to append the seam clause (rc $_lp_rc)" >&2
  return "$_lp_rc"
}

# chunk_dispatch_plan <units> <seams> <cap> — the MU_REVIEW_CHUNK_MAX_DISPATCHES
# arithmetic as a pure, testable function (fix: true total cap). The cap is a
# TRUE total: total = units × (1 + seams). Pure stdout, no side effects, so
# leaf-prompt-test.sh can pin the three outcomes.
#   units_over_cap  units alone exceed the cap — the branch cannot be chunked at
#                   all (even one leaf per unit is too many); the caller ESCALATEs
#                   and tells the operator to split the branch.
#   drop_seams      units fit but units × (1 + seams) does not — run the unseamed
#                   leaves only (the existing seam-skip notice).
#   ok              the full units × (1 + seams) fits (or no seams are declared).
chunk_dispatch_plan() { # $1=units $2=seams $3=cap ; stdout: plan word
  _cdp_u="${1:-0}"; _cdp_s="${2:-0}"; _cdp_cap="${3:-0}"
  if [ "$_cdp_u" -gt "$_cdp_cap" ]; then echo units_over_cap; return 0; fi
  if [ "$_cdp_s" -gt 0 ] && [ "$(( _cdp_u * (1 + _cdp_s) ))" -gt "$_cdp_cap" ]; then
    echo drop_seams; return 0
  fi
  echo ok
}
