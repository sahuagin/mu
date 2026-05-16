#!/usr/bin/env bash
# Install local tooling for pre-PR checks. Idempotent.
#
# This script replaces an earlier version (PR #54) that installed a git
# pre-push hook. That hook gated every push, which conflicted with the
# operator pattern of pushing for off-machine persistence. The new approach
# moves the gate from "push" to "PR promotion" via a gh wrapper.
#
# Behavior:
#   - If a legacy pre-push hook from this script is present, BACK UP and remove it.
#   - Symlink scripts/gh-wrapper to ~/.local/bin/gh so it resolves earlier
#     in PATH than the system gh. The wrapper runs scripts/pre-pr-check.sh
#     before `gh pr create` and `gh pr ready`; everything else passes through.
#
# Pre-existing non-trivial pre-push hooks (not written by us) are LEFT IN PLACE.
#
# Bypass for a single PR: MU_SKIP_PR_CHECK=1 gh pr create ...

set -eu

# --- locate the repo root + .git dir --------------------------------------

if command -v jj >/dev/null 2>&1 && jj root >/dev/null 2>&1; then
  ROOT="$(jj root)"
elif ROOT="$(git rev-parse --show-toplevel 2>/dev/null)"; then
  :
else
  echo "install-hooks: not inside a jj or git repo" >&2
  exit 1
fi

if ! GIT_COMMON_DIR="$(git rev-parse --git-common-dir 2>/dev/null)"; then
  echo "install-hooks: git rev-parse failed — are you inside the mu repo?" >&2
  exit 1
fi
case "$GIT_COMMON_DIR" in
  /*) ;;
  *) GIT_COMMON_DIR="$(pwd)/$GIT_COMMON_DIR" ;;
esac

# --- Step 1: remove the legacy pre-push hook (PR #54 leftover) -------------

LEGACY_HOOK="$GIT_COMMON_DIR/hooks/pre-push"
if [ -f "$LEGACY_HOOK" ] && grep -q "pre-pr-check.sh" "$LEGACY_HOOK" 2>/dev/null; then
  backup="$LEGACY_HOOK.removed.$(date +%Y%m%d%H%M%S)"
  echo "install-hooks: removing legacy pre-push hook (PR #54 leftover; superseded by gh wrapper)"
  echo "install-hooks: backed up to $backup"
  mv "$LEGACY_HOOK" "$backup"
fi

# --- Step 2: install gh wrapper to ~/.local/bin/gh -------------------------

TARGET="${HOME}/.local/bin/gh"
WRAPPER="$ROOT/scripts/gh-wrapper"

if [ ! -x "$WRAPPER" ]; then
  echo "install-hooks: $WRAPPER missing or not executable — cannot install" >&2
  exit 1
fi

mkdir -p "$(dirname "$TARGET")"

# Refuse to overwrite an existing real file (not a symlink) at the target.
if [ -e "$TARGET" ] && [ ! -L "$TARGET" ]; then
  echo "install-hooks: $TARGET exists and is NOT a symlink — refusing to overwrite" >&2
  echo "install-hooks: move it aside or remove it manually, then re-run" >&2
  exit 1
fi

ln -sfn "$WRAPPER" "$TARGET"
echo "install-hooks: symlinked $TARGET -> $WRAPPER"

# --- Step 3: PATH sanity --------------------------------------------------

case ":$PATH:" in
  *":$HOME/.local/bin:"*)
    # In PATH. Confirm the wrapper resolves before the system gh.
    resolved="$(command -v gh 2>/dev/null || true)"
    if [ "$resolved" = "$TARGET" ]; then
      echo "install-hooks: PATH ordering OK — \`gh\` now resolves to the wrapper"
    else
      echo "install-hooks: WARNING: \`gh\` currently resolves to $resolved, not the wrapper at $TARGET" >&2
      echo "install-hooks: ~/.local/bin needs to come BEFORE the directory containing the system gh in PATH" >&2
    fi
    ;;
  *)
    echo "install-hooks: WARNING: \$HOME/.local/bin is NOT in PATH" >&2
    echo "install-hooks: add to your shell rc: export PATH=\"\$HOME/.local/bin:\$PATH\"" >&2
    ;;
esac

echo "install-hooks: done. Bypass any single PR with MU_SKIP_PR_CHECK=1 gh pr create ..."
