#!/usr/bin/env python3
"""PR-scoped beads orphan detector — no external tools.

Surfaces beads referenced in THIS PR's commits that are still **open**, so they
can be closed in-PR (atomic with the merge) instead of needing a second PR.

Everything it needs is already in the checkout, so there is NO dependency on
`br`, the network, or a Rust build:
  - bead statuses come from the tracked `.beads/issues.jsonl`
  - bead references come from `git log BASE..HEAD` commit messages

Prints a Markdown nudge block to stdout when any PR-scoped open beads are found,
and nothing (empty stdout) when clean, so the caller can test emptiness.

ALWAYS exits 0: this is informational and must never block a merge.

Usage:
    beads_orphans_prscope.py <base_sha> <head_sha>
"""
import json
import os
import re
import subprocess
import sys

# Bead ids look like `mu-ab12`, `mu-kex4.6`, `mu-pr6r.1`: a prefix, a dash, then
# alphanumerics with optional dotted suffixes. We match liberally and then keep
# only tokens that are actually open beads in the JSONL, so over-matching
# (e.g. "co-authored") is harmless.
BEAD_RE = re.compile(r"\b[a-z][a-z0-9]*-[a-z0-9]+(?:\.[a-z0-9]+)*\b", re.IGNORECASE)

JSONL = os.path.join(".beads", "issues.jsonl")
OPEN_STATUSES = {"open", "in_progress"}

# Field separators for `git log` output: NUL between sha and body, RS between
# commits — neither occurs in commit text.
_NUL, _RS = "\x00", "\x1e"


def load_beads() -> dict:
    """Map bead id -> (status_lower, title) from the tracked JSONL (last wins)."""
    beads: dict = {}
    try:
        with open(JSONL) as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    d = json.loads(line)
                except json.JSONDecodeError:
                    continue
                bid = d.get("id")
                if bid:
                    beads[bid] = ((d.get("status") or "").lower(), d.get("title") or "")
    except FileNotFoundError:
        pass
    return beads


def referenced_in_range(base: str, head: str) -> dict:
    """Map bead id -> first referencing short-sha across base..head commits."""
    refs: dict = {}
    try:
        out = subprocess.run(
            # Use git's own %x00/%x1e escapes — a literal NUL byte in the argv
            # would truncate the format string at exec().
            ["git", "log", "--no-merges", "--format=%h%x00%B%x1e", f"{base}..{head}"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout
    except Exception:  # bad range / git unavailable — degrade quietly
        return refs
    for entry in out.split(_RS):
        sha, _, body = entry.strip().partition(_NUL)
        if not sha:
            continue
        for m in BEAD_RE.finditer(body):
            refs.setdefault(m.group(0), sha.strip())
    return refs


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: beads_orphans_prscope.py <base_sha> <head_sha>", file=sys.stderr)
        return 0

    base, head = sys.argv[1], sys.argv[2]
    beads = load_beads()
    refs = referenced_in_range(base, head)

    scoped = [
        (bid, sha)
        for bid, sha in sorted(refs.items())
        if beads.get(bid, ("", ""))[0] in OPEN_STATUSES
    ]
    if not scoped:
        return 0

    lines = [
        "### 🫧 Beads referenced by this PR that are still **open**",
        "",
        "These beads are referenced in this PR's commits but not closed. "
        "Close them *in this PR* so the merge closes them atomically — "
        "`br close <id>` then commit `.beads/issues.jsonl` — or the post-merge "
        "sweep will close them for you.",
        "",
        "| bead | title | referencing commit |",
        "| --- | --- | --- |",
    ]
    for bid, sha in scoped:
        title = beads[bid][1].replace("|", "\\|")
        lines.append(f"| `{bid}` | {title} | `{sha}` |")
    lines += ["", "_Informational only — this check never blocks a merge._"]
    print("\n".join(lines))
    return 0


if __name__ == "__main__":
    sys.exit(main())
