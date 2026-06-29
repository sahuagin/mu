#!/usr/bin/env python3
"""Beads orphan detector — surfaces referenced beads still OPEN in central beadsd.

Reads commit text (a branch's commit messages) on STDIN, extracts bead ids, and
lists those still open/in_progress in the central store, so they get closed
before the merge instead of left open.

mu-da27: the old version was a GitHub CI check that read the in-repo
`.beads/issues.jsonl` mirror. That mirror is retired and CI can't reach the
internal beadsd, so this now queries the central service via the `beads` client
and is meant to run LOCALLY (e.g. from scripts/pre-pr-check.sh).

  - bead references: regex over the commit text on stdin
  - bead statuses:   `beads --url <u> list --limit 0 --json`; URL from
                     $BEADS_REMOTE, else the `mu` entry in
                     ~/.config/beads/remotes.env

Prints a Markdown nudge to stdout when open referenced beads are found, nothing
when clean. ALWAYS exits 0 and degrades quietly (no `beads` client, service
unreachable, bad output -> empty), so callers treat it as informational and it
never blocks.

Usage:  <commit text> | beads_orphans_prscope.py
"""
import json
import os
import re
import subprocess
import sys

# Bead ids look like `mu-ab12`, `mu-kex4.6`. Match liberally, then keep only
# tokens that are actually open beads in the store, so over-matching is harmless.
BEAD_RE = re.compile(r"\b[a-z][a-z0-9]*-[a-z0-9]+(?:\.[a-z0-9]+)*\b", re.IGNORECASE)
OPEN_STATUSES = {"open", "in_progress"}


def beadsd_url():
    """Resolve the mu beadsd URL: $BEADS_REMOTE, else remotes.env `mu=`."""
    u = os.environ.get("BEADS_REMOTE")
    if u:
        return u
    try:
        with open(os.path.expanduser("~/.config/beads/remotes.env")) as f:
            for line in f:
                line = line.strip()
                if line.startswith("mu="):
                    return line.split("=", 1)[1].strip()
    except OSError:
        pass
    return None


def load_beads():
    """Map bead id -> (status_lower, title) from the central store via `beads`."""
    url = beadsd_url()
    if not url:
        return {}
    try:
        out = subprocess.run(
            ["beads", "--url", url, "list", "--limit", "0", "--json"],
            capture_output=True, text=True, check=True,
        ).stdout
        data = json.loads(out)
    except Exception:  # no client / service down / unexpected output -> quiet
        return {}
    issues = data.get("issues") if isinstance(data, dict) else data
    beads = {}
    for d in issues or []:
        bid = d.get("id")
        if bid:
            beads[bid] = ((d.get("status") or "").lower(), d.get("title") or "")
    return beads


def main() -> int:
    text = sys.stdin.read()
    beads = load_beads()
    if not beads:
        return 0

    seen = []
    for m in BEAD_RE.finditer(text):
        bid = m.group(0)
        if bid in beads and beads[bid][0] in OPEN_STATUSES and bid not in seen:
            seen.append(bid)
    if not seen:
        return 0

    lines = [
        "### 🫧 Beads referenced here that are still **open**",
        "",
        "Referenced in these commits but not closed in the central store. Close "
        "them before/with the merge so they aren't left open: `beads close <id>` "
        "(or `beads --url <u> close <id>`).",
        "",
        "| bead | title |",
        "| --- | --- |",
    ]
    for bid in seen:
        title = beads[bid][1].replace("|", "\\|")
        lines.append(f"| `{bid}` | {title} |")
    lines += ["", "_Informational only — never blocks._"]
    print("\n".join(lines))
    return 0


if __name__ == "__main__":
    sys.exit(main())
