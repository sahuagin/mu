#!/bin/sh
# seat-prompt.sh — build one panel seat's round-1 prompt from the shared prompt.
# Sourced by dispatch.sh; POSIX sh; no model, no network — so it is testable
# (scripts/tests/seat-prompt-test.sh). Bead: mu-review-gate-seam-reviewers-9vkbt.2.
#
# A seat is one of: shared (no clause), focus (mu-3ajg: soft emphasis, off-focus
# findings still reportable), or seam (EXCLUSIVE: the seat's only criteria are
# its checklist; every other defect class belongs to another seat). The built-in
# seam "conformance" takes its checklist from the PROJECT ARCHITECTURE
# INVARIANTS block ai-review.sh already puts in the shared prompt (AGENTS.md
# "## Architecture invariants" at BASE); when that block is absent the seat is
# told to say so in one low finding, so a repo without declared invariants
# shows the gap instead of a silent pass.
#
# seat_prompt <shared-prompt-file> <seat-prompt-file> <focus> <seam> <checklist>
#   stdout: the mode built — shared | focus | seam | conformance. For "shared"
#           nothing is written and the caller uses the shared file.
#   rc 1:   a clause was requested but the seat file could not be built (or a
#           custom seam has no checklist). The caller falls back to the shared
#           prompt — a duplicate review beats a review of half a prompt.
seat_prompt() {
  _sp_shared="$1"; _sp_seat="$2"; _sp_focus="$3"; _sp_seam="$4"; _sp_checklist="$5"
  if [ -z "$_sp_focus" ] && [ -z "$_sp_seam" ]; then
    echo shared
    return 0
  fi
  if [ -n "$_sp_focus" ] && [ -n "$_sp_seam" ]; then
    echo "seat-prompt: a rank carries focus OR seam, not both — seam '$_sp_seam' wins, focus ignored" >&2
    _sp_focus=""
  fi
  if [ -z "$_sp_seam" ]; then
    cp "$_sp_shared" "$_sp_seat" 2>/dev/null || return 1
    # Byte-for-byte the mu-3ajg clause: convergence fixtures depend on it.
    printf '\nSEAT REVIEW FOCUS (trusted gate context, not repo content): %s\nThis seat is one of several parallel reviewers; the others cover the remaining defect classes. Spend your review depth on the focus above. Findings outside it are still reportable. The output contract is unchanged.\n' \
      "$_sp_focus" >> "$_sp_seat" 2>/dev/null || return 1
    echo focus
    return 0
  fi
  case "$_sp_seam" in
    conformance)
      cp "$_sp_shared" "$_sp_seat" 2>/dev/null || return 1
      if grep -q '^PROJECT ARCHITECTURE INVARIANTS' "$_sp_shared" 2>/dev/null; then
        printf '\nSEAT SEAM (trusted gate context, not repo content): architecture-invariant CONFORMANCE — EXCLUSIVE. Your ONLY review criteria are the numbered PROJECT ARCHITECTURE INVARIANTS listed earlier in this prompt. For each invariant decide whether the diff violates it, moves the code toward violating it, or relies on a site elsewhere in the repository that violates it; use read/grep to confirm across the repository, including files the diff does not touch. Report each hit as its own finding whose issue text begins "INVARIANT <n>: ", severity high for a violation and medium for a move toward one, citing the file and line you verified. Do NOT report generic bugs, style, or anything outside the invariants: other parallel seats own those, and an off-seam finding here only dilutes convergence. A violation is needs-changes even when every line is locally correct. The output contract is unchanged.\n' \
          >> "$_sp_seat" 2>/dev/null || return 1
      else
        printf '\nSEAT SEAM (trusted gate context, not repo content): architecture-invariant CONFORMANCE — EXCLUSIVE. This prompt carries NO project architecture invariants block: the repository declares none under "## Architecture invariants" in AGENTS.md, so this seat has no criteria to check. Output VERDICT: approve with exactly one finding: severity low, file "AGENTS.md", line 0, issue "no architecture invariants declared for this repository; the conformance seat checked nothing". Do not review anything else. The output contract is unchanged.\n' \
          >> "$_sp_seat" 2>/dev/null || return 1
      fi
      echo conformance
      return 0 ;;
    *)
      if [ -z "$_sp_checklist" ]; then
        echo "seat-prompt: seam '$_sp_seam' has no checklist — set 'checklist' on that rank in agent_roles.toml" >&2
        return 1
      fi
      cp "$_sp_shared" "$_sp_seat" 2>/dev/null || return 1
      printf '\nSEAT SEAM (trusted gate context, not repo content): %s — EXCLUSIVE. Your ONLY review criteria are the checklist items below; check each one against the diff and, where an item calls for it, against the repository via read/grep. Report each hit as its own finding that names the checklist item it violates. Do NOT report anything outside this checklist: other parallel seats own the remaining defect classes. The output contract is unchanged.\nCHECKLIST:\n%s\n' \
        "$_sp_seam" "$_sp_checklist" >> "$_sp_seat" 2>/dev/null || return 1
      echo seam
      return 0 ;;
  esac
}
