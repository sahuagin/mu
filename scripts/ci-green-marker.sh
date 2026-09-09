#!/bin/sh
# ci-green-marker.sh — "fmt + clippy + tests were green AT THIS COMMIT" receipt
# (bead mu-ash9p).
#
# WHY: `just ci-aipr` re-ran the whole cargo check phase that `just ci` (or an
# earlier `just check`) had proved green on the same tree minutes before —
# 5-15 min of a 45-80 min gate, and the part that collides with other sessions'
# builds. The marker lets ci-aipr skip the REPEAT without skipping the gate: the
# cheap audits (converge audit, canary bead, verify-claims) still run.
#
# The marker is <CARGO_TARGET_DIR or <root>/target>/ci-green-<commit>. Naming it
# after the commit is half the safety argument: it cannot be honoured for a tree
# it does not describe.
#
# The other half is WHEN the id is resolved (panel finding, PR #611). Resolving
# it at write time — after the cargo steps and five more — folds an edit made
# DURING the run into the id, so the receipt would certify a tree the checks
# never saw. So the caller captures the id BEFORE the first check (`id`) and
# hands it back (`write <id>`); write re-resolves and refuses unless the two
# still agree. A refusal is never fatal: the only cost is that the next ci-aipr
# re-checks.
#
# WHICH commit: under jj the WORKING-COPY commit (@), not @-. jj re-snapshots @
# on every command, so an edit gives a new id and both the capture check above
# and a later `gate` stop matching. @- would keep matching across edits, and
# before a `jj describe` it names the PARENT (often main) — a receipt that would
# then be honoured on an unrelated branch built on the same parent. Under plain
# git the id is HEAD, which does NOT move when files change; there the capture
# comparison cannot see an edit, so the git path additionally refuses to
# capture, write or honour a receipt while the worktree is dirty. Capture too,
# not just write: a tree dirty at capture may have been checked WITH an
# uncommitted fix that is discarded before write, when HEAD reads clean and the
# receipt would certify a tree the checks never saw (panel finding, PR #611).
#
# usage: ci-green-marker.sh id            print the commit id to check against
#        ci-green-marker.sh write <id>    record <id> as green, if it still holds
#                                         (never fails a build: a refusal or an
#                                         unwritable target dir just means no
#                                         receipt)
#        ci-green-marker.sh gate          exit 0 = the check may be skipped for
#                                         this commit (prints one line saying
#                                         so), exit 1 = run it
#        ci-green-marker.sh path          print the marker path for this commit
# env:   CARGO_TARGET_DIR       marker directory (default <repo root>/target)
#        MU_REVIEW_FORCE_CHECK  non-empty: `gate` always says run
set -u

# Repo root, not $PWD: the marker belongs to the workspace, and jj workspaces
# have no top-level .git dir (same probe order as pre-pr-check.sh).
USING_JJ=""
if command -v jj >/dev/null 2>&1 && jj root >/dev/null 2>&1; then
  ROOT="$(jj root)"; USING_JJ=1
elif ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" && [ -n "$ROOT" ]; then
  :
else
  ROOT=""
fi

commit_id() {
  [ -n "$ROOT" ] || return 1
  if [ -n "$USING_JJ" ]; then
    # jj first, always: in a colocated repo git HEAD does not follow @, so
    # `git rev-parse HEAD` there names main while the work sits in @.
    jj log --no-graph -r @ -T commit_id 2>/dev/null
  else
    git rev-parse HEAD 2>/dev/null
  fi
}

# Under jj the working copy IS @, so an uncommitted edit already changes the id
# and the capture comparison catches it. Under git it does not, so ask.
tree_dirty() {
  [ -z "$USING_JJ" ] || return 1
  # A FAILING `git status` (corrupt index, unreadable tree) reads as dirty:
  # empty output from a command that failed is not a clean tree, and a receipt
  # must never rest on a cleanliness nobody established. --untracked-files is
  # pinned because a repo with status.showUntrackedFiles=no would hide a new
  # untracked test from the receipt (panel findings, PR #611).
  _st="$(git status --porcelain --untracked-files=normal 2>/dev/null)" || return 0
  [ -n "$_st" ]
}

marker_dir() { printf '%s\n' "${CARGO_TARGET_DIR:-$ROOT/target}"; }
marker_file() { printf '%s/ci-green-%s\n' "$(marker_dir)" "$1"; }

case "${1:-}" in
  id|path)
    _id="$(commit_id)" || exit 1
    [ -n "$_id" ] || exit 1
    if [ "$1" = id ] && tree_dirty; then
      echo "ci-green-marker: worktree has uncommitted changes at capture — this run writes no receipt" >&2
      exit 1
    fi
    if [ "$1" = id ]; then printf '%s\n' "$_id"; else marker_file "$_id"; fi ;;
  write)
    cap="${2:-}"
    now="$(commit_id)" || now=""
    if [ -z "$now" ]; then
      echo "ci-green-marker: no commit id (not a jj or git repo) — nothing recorded" >&2
      exit 0
    fi
    if [ -z "$cap" ]; then
      echo "ci-green-marker: no captured commit id — a receipt must name the tree the checks ran on; nothing recorded" >&2
      exit 0
    fi
    if [ "$cap" != "$now" ]; then
      echo "ci-green-marker: tree moved during the run ($cap -> $now) — nothing recorded; the next ci-aipr re-checks" >&2
      exit 0
    fi
    if tree_dirty; then
      echo "ci-green-marker: worktree has uncommitted changes — nothing recorded; the next ci-aipr re-checks" >&2
      exit 0
    fi
    p="$(marker_file "$now")"
    mkdir -p "$(marker_dir)" 2>/dev/null || true
    # One marker at a time: every earlier commit's receipt is dead weight, and a
    # tree of them in target/ invites reading the wrong one. If a second
    # workspace shares this CARGO_TARGET_DIR, clearing its receipt costs it a
    # re-check — the direction that is always safe.
    rm -f "$(marker_dir)"/ci-green-* 2>/dev/null || true
    if printf 'fmt+clippy+test green at %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$p" 2>/dev/null; then
      echo "ci-green-marker: recorded $p"
    else
      # An optimization must not be able to fail a green run.
      echo "ci-green-marker: could not write $p — the next ci-aipr will re-check" >&2
    fi
    exit 0 ;;
  gate)
    if [ -n "${MU_REVIEW_FORCE_CHECK:-}" ]; then
      echo "ci-aipr: MU_REVIEW_FORCE_CHECK set — running the full check."
      exit 1
    fi
    now="$(commit_id)" || exit 1
    [ -n "$now" ] || exit 1
    # A receipt describes a clean tree at $now; on git a dirty worktree is not
    # that tree, and nothing in the id would say so.
    tree_dirty && exit 1
    p="$(marker_file "$now")"
    [ -f "$p" ] || exit 1
    echo "ci-aipr: fmt/clippy/tests already green for this commit ($(basename "$p")) — skipping the repeat; set MU_REVIEW_FORCE_CHECK=1 to force."
    exit 0 ;;
  *)
    echo "usage: ci-green-marker.sh id|write <id>|gate|path" >&2
    exit 2 ;;
esac
