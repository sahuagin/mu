#!/usr/bin/env bash
# seat-prompt-test.sh — scripts/review-panel/seat-prompt.sh, model-free.
#
# bead: mu-review-gate-seam-reviewers-9vkbt.2 (+ .3 leaf mode). Pins: the mu-3ajg
# focus clause byte-for-byte (convergence fixtures depend on it); the exclusive
# seam clause; the conformance seam with and without an invariants block; the
# fail-closed paths a caller must fall back on; and the SEAT_PROMPT_LEAF=1 leaf
# variant of the seam/conformance clauses (no tools, THIS-UNIT-only, UNVERIFIED,
# leaf FINDING contract) which must NOT alter panel-mode text. Run from any cwd;
# creates its own tmpdir.

set -u
set -o pipefail

TEST_DIR="$(cd "$(dirname "$0")" && pwd)"
LIB="$TEST_DIR/../review-panel/seat-prompt.sh"
[ -r "$LIB" ] || { echo "seat-prompt-test: lib not found at $LIB" >&2; exit 2; }
# shellcheck source=../review-panel/seat-prompt.sh
. "$LIB"

PASS=0
FAIL=0
TMP="$(mktemp -d "${TMPDIR:-/tmp}/seat-prompt.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

ok()   { printf 'PASS: %s\n' "$1"; PASS=$((PASS + 1)); }
bad()  { printf 'FAIL: %s\n  %s\n' "$1" "$2" >&2; FAIL=$((FAIL + 1)); }
# check <name> <condition-rc> <detail>
check() { if [ "$2" -eq 0 ]; then ok "$1"; else bad "$1" "$3"; fi; }

SHARED="$TMP/shared.prompt"
printf 'You are a strict pre-PR code reviewer.\nOutput contract: VERDICT line then JSON.\n\nPROJECT ARCHITECTURE INVARIANTS (trusted gate context; prefer BASE revision):\n1. **The event log is the source of truth.**\n2. **Money is Decimal.**\n\nBEGIN UNTRUSTED REPO CONTENT: PR DIFF\n+ fn x() {}\nEND UNTRUSTED REPO CONTENT: PR DIFF\n' > "$SHARED"
PLAIN="$TMP/plain.prompt"
grep -v -e 'PROJECT ARCHITECTURE INVARIANTS' -e '^[12]\. \*\*' "$SHARED" > "$PLAIN"

# 1. No focus, no seam: nothing written, mode "shared".
seat="$TMP/s1"; mode=$(seat_prompt "$SHARED" "$seat" "" "" ""); rc=$?
check "no clause -> shared, nothing written" $(( rc == 0 && "$([ "$mode" = shared ] && echo 0 || echo 1)" == 0 && "$([ ! -e "$seat" ] && echo 0 || echo 1)" == 0 ? 0 : 1 )) "rc=$rc mode=$mode exists=$([ -e "$seat" ] && echo yes || echo no)"

# 2. Focus: the shared prompt plus the exact mu-3ajg clause, then the reply
#    contract restated last (a clause must not push it up the prompt; PR #611).
seat="$TMP/s2"; mode=$(seat_prompt "$SHARED" "$seat" "error handling and safeguards" "" ""); rc=$?
expected="$TMP/s2.expected"
{ cat "$SHARED"; printf '\nSEAT REVIEW FOCUS (trusted gate context, not repo content): %s\nThis seat is one of several parallel reviewers; the others cover the remaining defect classes. Spend your review depth on the focus above. Findings outside it are still reportable. The output contract is unchanged.\n' "error handling and safeguards"; printf '\n%s\n' "$(cat "$TEST_DIR/../review-panel/reply-contract.txt")"; } > "$expected"
if [ "$rc" -eq 0 ] && [ "$mode" = focus ] && cmp -s "$seat" "$expected"; then ok "focus clause is byte-identical to mu-3ajg"; else bad "focus clause is byte-identical to mu-3ajg" "rc=$rc mode=$mode diff: $(diff "$expected" "$seat" 2>&1 | head -5)"; fi

# 3. Custom seam with a checklist: exclusive clause + the checklist verbatim.
seat="$TMP/s3"; mode=$(seat_prompt "$SHARED" "$seat" "" "money-handling" $'- no float arithmetic on prices\n- Decimal from the wire, never parsed twice'); rc=$?
check "custom seam builds the exclusive clause" $(( rc == 0 && "$([ "$mode" = seam ] && echo 0 || echo 1)" == 0 ? 0 : 1 )) "rc=$rc mode=$mode"
grep -q 'SEAT SEAM (trusted gate context, not repo content): money-handling — EXCLUSIVE' "$seat"; check "custom seam names itself and is exclusive" $? "$(tail -n 4 "$seat")"
grep -q -- '- Decimal from the wire, never parsed twice' "$seat"; check "custom seam carries the checklist verbatim" $? "$(tail -n 4 "$seat")"
head -n "$(wc -l < "$SHARED")" "$seat" | cmp -s - "$SHARED"; check "custom seam keeps the shared prompt intact" $? "prefix differs"

# 4. Custom seam without a checklist: refused (rc 1) so the caller falls back.
seat="$TMP/s4"; mode=$(seat_prompt "$SHARED" "$seat" "" "money-handling" "" 2>"$TMP/s4.err"); rc=$?
check "custom seam without checklist is refused" $(( rc == 1 ? 0 : 1 )) "rc=$rc mode=$mode"
grep -q "has no checklist" "$TMP/s4.err"; check "refusal says why" $? "$(cat "$TMP/s4.err")"

# 5. Conformance when the gate declares invariants (flag = 1): the invariants
#    are the only criteria; pre-existing sites are low notes, not blockers.
seat="$TMP/s5"; mode=$(seat_prompt "$SHARED" "$seat" "" "conformance" "" 1); rc=$?
check "conformance builds with the invariants flag" $(( rc == 0 && "$([ "$mode" = conformance ] && echo 0 || echo 1)" == 0 ? 0 : 1 )) "rc=$rc mode=$mode"
grep -q 'CONFORMANCE — EXCLUSIVE. Your ONLY review criteria are the numbered PROJECT ARCHITECTURE INVARIANTS in the trusted gate-context block' "$seat"; check "conformance points at the trusted invariants block" $? "$(tail -n 2 "$seat")"
grep -q 'begins "INVARIANT <n>: "' "$seat"; check "conformance findings are labelled by invariant" $? "$(tail -n 2 "$seat")"
grep -q 'PRE-EXISTING INVARIANT <n>: ' "$seat"; check "pre-existing sites are low PRE-EXISTING notes, not blockers" $? "$(tail -n 2 "$seat")"
grep -q 'no architecture invariants declared' "$seat"; check "conformance with the flag does not emit the no-invariants text" $(( $? == 0 ? 1 : 0 )) "$(tail -n 2 "$seat")"

# 6. Conformance when the gate declares NO invariants (flag empty/0): one low
#    finding, nothing else — even though this shared prompt CONTAINS a line
#    that looks like the heading. The flag decides; the prompt is never sniffed.
seat="$TMP/s6"; mode=$(seat_prompt "$SHARED" "$seat" "" "conformance" "" ""); rc=$?
check "conformance builds without the invariants flag" $(( rc == 0 && "$([ "$mode" = conformance ] && echo 0 || echo 1)" == 0 ? 0 : 1 )) "rc=$rc mode=$mode"
grep -q 'no architecture invariants declared for this repository; the conformance seat checked nothing' "$seat"; check "no-flag case asks for the visible low finding" $? "$(tail -n 2 "$seat")"
seat="$TMP/s6b"; mode=$(seat_prompt "$SHARED" "$seat" "" "conformance" "" 0); rc=$?
grep -q 'no architecture invariants declared' "$seat"; check "flag 0 is treated as absent (prompt heading is not trusted)" $? "$(tail -n 2 "$seat")"
seat="$TMP/s6c"; mode=$(seat_prompt "$PLAIN" "$seat" "" "conformance" "" 1); rc=$?
grep -q 'Your ONLY review criteria are the numbered PROJECT ARCHITECTURE INVARIANTS' "$seat"; check "flag 1 is trusted even when the fixture prompt lacks the heading" $? "$(tail -n 2 "$seat")"

# 7. Focus and seam together: seam wins, warning on stderr.
seat="$TMP/s7"; mode=$(seat_prompt "$SHARED" "$seat" "some focus" "conformance" "" 1 2>"$TMP/s7.err"); rc=$?
check "focus+seam -> seam wins" $(( rc == 0 && "$([ "$mode" = conformance ] && echo 0 || echo 1)" == 0 ? 0 : 1 )) "rc=$rc mode=$mode"
grep -q 'focus OR seam' "$TMP/s7.err"; check "focus+seam warns" $? "$(cat "$TMP/s7.err")"
grep -q 'SEAT REVIEW FOCUS' "$seat"; check "focus+seam drops the focus clause" $(( $? == 0 ? 1 : 0 )) "focus clause present"

# 8. Unreadable shared prompt: rc 1 for every clause kind.
for kind in "focus||" "|conformance|" "|custom|- item"; do
  IFS='|' read -r f s c <<<"$kind"
  seat="$TMP/s8"; mode=$(seat_prompt "$TMP/does-not-exist" "$seat" "$f" "$s" "$c" 2>/dev/null); rc=$?
  check "unreadable shared prompt is refused (${f:-}${s:-})" $(( rc == 1 ? 0 : 1 )) "rc=$rc mode=$mode"
done

# 9. Leaf mode (SEAT_PROMPT_LEAF=1): the seam/conformance clause bodies switch to
#    the LEAF variant — no tools, THIS-UNIT-only, the UNVERIFIED escape hatch, the
#    leaf FINDING contract — while the exclusivity header and INVARIANT/<seam>
#    labeling stay. Panel mode (cases 2/5/6, byte-identical pins) is unchanged.
seat="$TMP/s9seam"; mode=$(SEAT_PROMPT_LEAF=1 SEAT_PROMPT_NO_REPLY_CONTRACT=1 seat_prompt "$SHARED" "$seat" "" "money-handling" $'- no float arithmetic on prices'); rc=$?
check "leaf custom seam builds" $(( rc == 0 && "$([ "$mode" = seam ] && echo 0 || echo 1)" == 0 ? 0 : 1 )) "rc=$rc mode=$mode"
grep -q 'SEAT SEAM (trusted gate context, not repo content): money-handling — EXCLUSIVE' "$seat"; check "leaf custom seam keeps the exclusive header" $? "$(tail -n 3 "$seat")"
grep -q 'you have no tools and cannot see the rest of the repository' "$seat"; check "leaf custom seam states no tools / this-unit-only" $? "$(tail -n 3 "$seat")"
grep -q 'UNVERIFIED at the start of the claim' "$seat"; check "leaf custom seam carries the UNVERIFIED escape hatch" $? "$(tail -n 3 "$seat")"
grep -qF -- 'the leaf contract: FINDING|<severity>|<file>|<claim> lines or NO_FINDINGS, no verdict, no JSON' "$seat"; check "leaf custom seam states the leaf output contract" $? "$(tail -n 3 "$seat")"
grep -q 'against the repository via read/grep' "$seat"; check "leaf custom seam drops the panel read/grep text" $(( $? == 0 ? 1 : 0 )) "panel text leaked"

seat="$TMP/s9conf"; mode=$(SEAT_PROMPT_LEAF=1 SEAT_PROMPT_NO_REPLY_CONTRACT=1 seat_prompt "$SHARED" "$seat" "" "conformance" "" 1); rc=$?
check "leaf conformance builds with the invariants flag" $(( rc == 0 && "$([ "$mode" = conformance ] && echo 0 || echo 1)" == 0 ? 0 : 1 )) "rc=$rc mode=$mode"
grep -q 'CONFORMANCE — EXCLUSIVE. Your ONLY review criteria are the numbered PROJECT ARCHITECTURE INVARIANTS' "$seat"; check "leaf conformance keeps the exclusive invariants header" $? "$(tail -n 3 "$seat")"
grep -qF -- 'INVARIANT <n>: ' "$seat"; check "leaf conformance keeps INVARIANT <n>: labeling" $? "$(tail -n 3 "$seat")"
grep -q 'you have no tools and cannot see the rest of the repository' "$seat"; check "leaf conformance states no tools / this-unit-only" $? "$(tail -n 3 "$seat")"
grep -q 'UNVERIFIED at the start of the claim' "$seat"; check "leaf conformance carries the UNVERIFIED escape hatch" $? "$(tail -n 3 "$seat")"
grep -q 'use read/grep to confirm' "$seat"; check "leaf conformance drops the panel read/grep text" $(( $? == 0 ? 1 : 0 )) "panel text leaked"
grep -q 'PRE-EXISTING INVARIANT' "$seat"; check "leaf conformance drops the PRE-EXISTING unchanged-code allowance" $(( $? == 0 ? 1 : 0 )) "PRE-EXISTING leaked onto the leaf"

# 10. Leaf mode leaves panel mode byte-identical: the same custom-seam call
#     without SEAT_PROMPT_LEAF still matches the case-3 panel text exactly.
seat="$TMP/s10"; mode=$(seat_prompt "$SHARED" "$seat" "" "money-handling" $'- no float arithmetic on prices'); rc=$?
grep -q 'check each one against the diff and, where an item calls for it, against the repository via read/grep' "$seat"; check "panel custom seam text is intact when SEAT_PROMPT_LEAF is unset" $? "$(tail -n 3 "$seat")"
grep -q 'you have no tools' "$seat"; check "panel custom seam does NOT carry leaf text" $(( $? == 0 ? 1 : 0 )) "leaf text leaked into panel mode"

printf '\nseat-prompt-test: %d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
