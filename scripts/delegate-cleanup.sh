#!/usr/bin/env zsh
#
# delegate-cleanup.sh — remove a finished delegation workspace.
#
# Usage:
#   scripts/delegate-cleanup.sh <spec-id> <attempt-number>
#
# Calls `jj workspace forget` then removes the workspace directory.
# The bookmark is preserved (run `jj bookmark delete <name>` separately
# if you also want to drop it).

set -euo pipefail

if [[ $# -ne 2 ]]; then
  print -u 2 "usage: $0 <spec-id> <attempt-number>"
  exit 64
fi

spec_id="$1"
attempt="$2"

repo_root="$(jj workspace root)"
workspace_name="${spec_id}-attempt-${attempt}"
workspace_path="${repo_root}/.delegations/${workspace_name}"

if [[ ! -d "$workspace_path" ]]; then
  print -u 2 "no workspace at: $workspace_path"
  exit 0  # nothing to do, not an error
fi

print "=== Forgetting jj workspace: $workspace_name"
jj workspace forget "$workspace_name" || \
  print -u 2 "warning: jj workspace forget failed; continuing"

print "=== Removing directory: $workspace_path"
rm -rf "$workspace_path"

print "=== Done."
print "(Bookmark preserved. To delete: jj bookmark delete delegate/$workspace_name)"
