#!/usr/bin/env zsh
#
# delegate.sh — run a sub-agent delegation in an isolated jj workspace.
#
# Usage:
#   scripts/delegate.sh <spec-id> <attempt-number> <auth-mode> [extra agent-router args...]
#
# Example:
#   scripts/delegate.sh mu-008 1 codex-oauth
#
# What this does:
#   1. Creates a fresh jj workspace at .delegations/<spec-id>-attempt-<N>/
#      (path is gitignored; co-located with the repo for easy review).
#   2. In that workspace, makes a new commit on top of main on a
#      bookmark named delegate/<spec-id>-attempt-<N>.
#   3. Invokes ~/src/claude-personal/scripts/agent-router with --cwd
#      pointing at the workspace.
#   4. After the delegate finishes, prints a summary: the bookmark, the
#      diff stat, the workspace path. Does NOT auto-merge; the
#      orchestrator (claude or human) reviews and merges explicitly.
#
# Cleanup:
#   The workspace is left in place for review. To remove a finished
#   workspace:
#     jj workspace forget <name>
#     rm -rf .delegations/<spec-id>-attempt-<N>
#   Or use the helper:
#     scripts/delegate-cleanup.sh <spec-id> <attempt-number>
#
# Why this exists:
#   Earlier delegations ran with --cwd pointing at the user's working
#   copy, which meant a parallel claude-code session and the delegate
#   shared one filesystem checkout. We patched at the prompt level
#   ("don't jj restore unrelated files") for four delegations; the fix
#   held but it's etiquette, not isolation. This wrapper gives each
#   delegate its own filesystem checkout via jj workspaces. Branch
#   isolation via jj bookmarks lets the orchestrator review and merge
#   when the work is good.

set -euo pipefail

if [[ $# -lt 3 ]]; then
  print -u 2 "usage: $0 <spec-id> <attempt-number> <auth-mode> [extra agent-router args]"
  print -u 2 "example: $0 mu-008 1 codex-oauth"
  exit 64
fi

spec_id="$1"
attempt="$2"
auth="$3"
shift 3

# Locate the repo root by asking jj. This is the orchestrator's
# workspace, the one we're spinning a new workspace off of.
repo_root="$(jj workspace root)"
prompt_file="${repo_root}/specs/${spec_id}-delegation.md"
delegations_dir="${repo_root}/.delegations"
workspace="${delegations_dir}/${spec_id}-attempt-${attempt}"
bookmark="delegate/${spec_id}-attempt-${attempt}"

# Sanity checks before doing anything.
if [[ ! -f "$prompt_file" ]]; then
  print -u 2 "no delegation prompt at: $prompt_file"
  print -u 2 "(create it before invoking this script)"
  exit 65
fi

if [[ -e "$workspace" ]]; then
  print -u 2 "workspace path already exists: $workspace"
  print -u 2 "either pick a different attempt number, or clean up first."
  exit 70
fi

# Make sure .delegations/ is gitignored so review tooling doesn't
# treat the workspaces as project files.
if [[ ! -f "${repo_root}/.gitignore" ]] || ! grep -q "^/.delegations" "${repo_root}/.gitignore"; then
  print -u 2 "warning: ${repo_root}/.gitignore doesn't have /.delegations/ entry."
  print -u 2 "         workspaces created here will appear in git status. add to .gitignore."
fi

mkdir -p "$delegations_dir"

print "=== Creating jj workspace at: $workspace"
jj workspace add "$workspace"

print "=== Setting up branch in workspace"
(
  cd "$workspace"
  # Move the workspace's working-copy commit on top of main.
  jj new -r main
  # Create the bookmark on the new commit. (Use --allow-existing in
  # case re-running on the same attempt name — but our sanity check
  # above should prevent that.)
  jj bookmark create "$bookmark" -r @
)

print "=== Running agent-router"
print "=== prompt: $prompt_file"
print "=== cwd:    $workspace"
print "=== auth:   $auth"
print

agent_router="${HOME}/src/claude-personal/scripts/agent-router"
if [[ ! -x "$agent_router" ]]; then
  print -u 2 "agent-router not found at: $agent_router"
  print -u 2 "set AGENT_ROUTER env var to override, or fix the path."
  exit 66
fi
[[ -n "${AGENT_ROUTER-}" ]] && agent_router="$AGENT_ROUTER"

set +e
"$agent_router" \
  --auth "$auth" \
  --cwd "$workspace" \
  --task-file "$prompt_file" \
  --output summary \
  "$@"
delegate_exit=$?
set -e

print
print "=== Delegate finished (exit=$delegate_exit)"
print "=== Workspace:  $workspace"
print "=== Bookmark:   $bookmark"
print
print "=== Diff vs main:"
(cd "$workspace" && jj diff --stat -r "main..@" 2>/dev/null) || \
  print "(no diff or jj couldn't compute one — check workspace state)"
print

# verify-claims gate (mu-b5kl): compare each delegate commit's `## Files`
# claim block against the actual diff. Surfaces hallucinated-claim failures
# right next to the workspace path, so the operator sees them at review time.
print "=== Verify claims (each commit in main..@):"
gate_exit=0
verify_script="${repo_root}/scripts/verify-claims.sh"
if [[ -x "$verify_script" ]]; then
  (
    cd "$workspace"
    base=$(git merge-base main HEAD 2>/dev/null || true)
    if [[ -n "$base" ]]; then
      commits=$(git rev-list --reverse --no-merges "$base..HEAD")
    else
      commits=$(git rev-parse HEAD)
    fi
    if [[ -z "$commits" ]]; then
      print "(no commits in main..@ — nothing to verify)"
      exit 0
    fi
    rc=0
    for c in $commits; do
      "$verify_script" "$c" || rc=$?
    done
    exit "$rc"
  )
  gate_exit=$?
else
  print "(scripts/verify-claims.sh missing — skipping)"
fi
print

# If the delegate exited cleanly but the gate caught a hallucinated claim,
# inherit the gate's exit code so callers (CI, operator scripts) see the failure.
if [[ "$delegate_exit" -eq 0 && "$gate_exit" -ne 0 ]]; then
  print "=== gate failure detected — overriding clean delegate exit (was 0, now $gate_exit)"
  delegate_exit="$gate_exit"
fi
print "Review steps:"
print "  cd $workspace"
print "  jj log -r main..@"
print "  jj diff -r main..@"
print
print "If the work looks good, merge from the orchestrator workspace:"
print "  cd $repo_root"
print "  jj rebase -r $bookmark -d main"
print "  jj bookmark move main --to $bookmark"
print "  jj git push --remote origin --bookmark main"
print
print "If the work needs another attempt:"
print "  scripts/delegate.sh $spec_id $((attempt + 1)) $auth"
print
print "When done with this workspace:"
print "  scripts/delegate-cleanup.sh $spec_id $attempt"

exit $delegate_exit
