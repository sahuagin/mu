#!/usr/bin/env python3
"""prior-art — the harness's dedup gate: before recommending NEW work, search
EXISTING tracked work and report candidate prior art.

The lesson this encodes (2026-06-22): a finding read off a *moving* `main` can be
real yet already-addressed — the analysis is correct, it just arrived late. Two of
three findings that session (unbounded tool grep; timeout not reaping children)
turned out to be instances of already-tracked themes; only one was novel. So before
any "build X" escapes a finding, fan out across the places work is recorded and
surface what already exists, so a human/loop decides: already-addressed | partial |
novel.

Channels (each isolated — one failing source never kills the report):
  - beads   open AND closed (`beads exec -- list --all`) — issues, the primary record
  - memory  `agent memory search` — decisions / feedback / prior reasoning
  - prs     `gh pr list --state all` — shipped + in-flight code (titles)
  - jj      `jj log` over recent main — what actually landed (first lines)

usage:
  prior_art.py "<finding description>" [keyword ...]
    keywords default to significant words extracted from the description.

env: BEADS_URL (default mu tracker), GH_REPO (default sahuagin/mu),
     MU_REPO (default ~/src/public_github/mu), JJ_DEPTH (default 120),
     PR_LIMIT (default 200), TOPK (default 6), MIN_HITS (default 2)

exit: 0 always (advisory). Prints per-channel candidates + a verdict line. A
caller that wants to gate can grep for "POSSIBLE PRIOR ART".
"""
import json
import os
import pathlib
import re
import subprocess
import sys

BEADS_URL = os.environ.get("BEADS_URL", "http://10.1.1.172:7771/mcp")
GH_REPO = os.environ.get("GH_REPO", "sahuagin/mu")
MU_REPO = os.environ.get("MU_REPO", str(pathlib.Path.home() / "src/public_github/mu"))
JJ_DEPTH = int(os.environ.get("JJ_DEPTH", "120"))
PR_LIMIT = int(os.environ.get("PR_LIMIT", "200"))
TOPK = int(os.environ.get("TOPK", "6"))
MIN_HITS = int(os.environ.get("MIN_HITS", "2"))

STOP = set("""the a an and or of to for in on at by with from into over under is are be
been being this that these those it its as no not your you we our their than then so
add adds added fix fixes fixed via use uses make makes when where which what how new
only also per via -> code repo main bead beads pr prs""".split())


def keywords(desc, extra):
    if extra:
        return [k.lower() for k in extra]
    words = re.findall(r"[A-Za-z][A-Za-z0-9_./-]{3,}", desc.lower())
    seen, out = set(), []
    for w in words:
        if w in STOP or w in seen:
            continue
        seen.add(w)
        out.append(w)
    return out[:12]


def score(text, kws):
    t = text.lower()
    return sum(1 for k in kws if k in t)


def run(cmd, timeout=30):
    return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)


def chan_beads(kws):
    p = run(["beads", "--url", BEADS_URL, "exec", "--", "list", "--all",
             "--limit", "0", "--json"])
    data = json.loads(p.stdout)
    # br --json is {"issues":[...], "total", "has_more", ...}; older/other paths
    # may hand back a bare list. Handle both — and treat an empty result as an
    # ERROR (there are hundreds of beads), so a parse/shape regression surfaces as
    # a skipped channel instead of a silent "0 candidates" → false-novel verdict.
    if isinstance(data, dict):
        issues = data.get("issues", [])
    elif isinstance(data, list):
        issues = data
    else:
        issues = []
    issues = [x for x in issues if isinstance(x, dict)]
    if not issues:
        raise RuntimeError(f"beads returned no issues (shape={type(data).__name__}); "
                           "parse error — NOT a clean empty tracker")
    scored = []
    for it in issues:
        s = score(f"{it.get('id','')} {it.get('title','')} {it.get('description','')}", kws)
        if s >= MIN_HITS:
            scored.append((s, f"[{it.get('status','?'):11}] {it.get('id','?')}: {it.get('title','')[:74]}"))
    scored.sort(reverse=True)
    return [f"(hits={s}) {line}" for s, line in scored[:TOPK]], len(issues)


def chan_memory(kws, desc):
    p = run(["agent", "memory", "search", desc])
    lines = [ln.strip() for ln in p.stdout.splitlines()
             if re.match(r"^\[[0-9a-f]+\]", ln.strip())]
    return lines[:TOPK], len(lines)


def chan_prs(kws):
    p = run(["gh", "pr", "list", "-R", GH_REPO, "--state", "all",
             "--limit", str(PR_LIMIT), "--json", "number,title,state"])
    prs = json.loads(p.stdout)
    scored = []
    for pr in prs:
        s = score(pr.get("title", ""), kws)
        if s >= MIN_HITS:
            scored.append((s, f"#{pr['number']} {pr.get('state','?'):8} {pr.get('title','')[:74]}"))
    scored.sort(reverse=True)
    return [f"(hits={s}) {line}" for s, line in scored[:TOPK]], len(prs)


def chan_jj(kws):
    p = run(["jj", "log", "-r", f"ancestors(main, {JJ_DEPTH})", "--no-graph",
             "-T", 'change_id.shortest(8) ++ "\t" ++ description.first_line() ++ "\n"'],
            timeout=30)
    scored = []
    for ln in p.stdout.splitlines():
        if "\t" not in ln:
            continue
        cid, msg = ln.split("\t", 1)
        s = score(msg, kws)
        if s >= MIN_HITS:
            scored.append((s, f"{cid} {msg[:74]}"))
    scored.sort(reverse=True)
    return [f"(hits={s}) {line}" for s, line in scored[:TOPK]], len(p.stdout.splitlines())


def main():
    if len(sys.argv) < 2:
        print(__doc__.strip().splitlines()[0])
        print("usage: prior_art.py \"<finding>\" [keyword ...]")
        return 2
    desc = sys.argv[1]
    kws = keywords(desc, sys.argv[2:])
    print(f"# prior-art check\nfinding: {desc}\nkeywords: {', '.join(kws)}\n")
    channels = [("beads (open+closed)", chan_beads, (kws,)),
                ("memory", chan_memory, (kws, desc)),
                ("PRs (all states)", chan_prs, (kws,)),
                ("jj history (main)", chan_jj, (kws,))]
    total, errored = 0, []
    for name, fn, fnargs in channels:
        try:
            hits, scanned = fn(*fnargs)
        except Exception as e:  # noqa: BLE001 — one dead source must not blind the rest
            print(f"## {name}: ERROR ({str(e)[:90]}) — channel skipped\n")
            errored.append(name)
            continue
        print(f"## {name} (scanned {scanned}) — {len(hits)} candidate(s)")
        for h in hits:
            print(f"  - {h}")
        if not hits:
            print("  (none)")
        total += len(hits)
        print()
    if total:
        print(f"VERDICT: POSSIBLE PRIOR ART — {total} candidate(s). Review "
              "already-addressed | partial | novel BEFORE recommending new work.")
    elif errored:
        print(f"VERDICT: INCONCLUSIVE — {len(errored)} channel(s) failed "
              f"({', '.join(errored)}); cannot claim novelty. Fix the channel and re-run.")
    else:
        print("VERDICT: no strong prior art found — likely novel "
              "(still sanity-check the framing before building).")
    return 0


if __name__ == "__main__":
    sys.exit(main())
