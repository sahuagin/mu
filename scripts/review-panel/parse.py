#!/usr/bin/env python3
"""Parse a panel's per-rank .out files into verdicts/findings.
usage: parse_panel.py <prefix>   # e.g. .../pr282.r1  -> reads <prefix>.rank*.out
Robust to ```json fences, [thinking] lines, and inline <think> blocks."""
import json, re, glob, sys, os
# One parser, not two: --check must accept exactly what converge.py scores, or
# a re-ask fires on a reply converge would have read fine (a reviewer flagged
# that drift once). This file reads through converge.extract.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from converge import extract, seat_verdict, findings_of, KNOWN_VERDICTS


def has_verdict(s):
    """True if `s` yields a verdict the panel can act on (approve or
    needs-changes after normalisation). An off-contract verdict is not one:
    it must reach the dissent-only re-ask, not bypass it (PR #611)."""
    try:
        return seat_verdict(extract(s)) in KNOWN_VERDICTS
    except Exception:
        return False


def rescuable(s):
    """True if `s` is DISSENT worth promoting from a verdict re-ask: a
    needs-changes carrying at least one finding on a real path. A re-ask may
    rescue a review that was written but not enveloped; it may not manufacture
    an approve from notes that never reached a conclusion. Measured on PR #611:
    a seat that ran out of turns mid-investigation was re-asked every round and
    answered "No review concerns were recorded" — an approve, with no review
    behind it, and in one round against its own logged conclusion."""
    try:
        d = extract(s)
    except Exception:
        return False
    return seat_verdict(d) == "needs-changes" and any(True for _ in findings_of(d))


# --check <file>:     exit 0 if the file holds a usable verdict, 1 otherwise.
# --rescuable <file>: exit 0 if it holds a needs-changes with findings.
# No stdout — pure predicates for shell `if`.
if len(sys.argv) >= 3 and sys.argv[1] in ('--check', '--rescuable'):
    pred = has_verdict if sys.argv[1] == '--check' else rescuable
    try:
        ok = pred(open(sys.argv[2], encoding="utf-8", errors="replace").read())
    except Exception:
        ok = False
    sys.exit(0 if ok else 1)

prefix = sys.argv[1]
verdicts = {}
for f in sorted(glob.glob(prefix + ".rank*.out")):
    name = os.path.basename(f)[len(os.path.basename(prefix))+1:-4]
    try:
        d = extract(open(f).read())
        v = d.get('verdict', '?').upper()
        verdicts[name] = v
        print(f"\n### {name}: {v} — {d.get('summary','')[:170]}")
        for x in d.get('findings', []):
            if isinstance(x, dict):
                print(f"   [{x.get('severity','?')}] {x.get('file','?')}:{x.get('line','?')} — {str(x.get('issue',''))[:170]}")
            else:
                print(f"   - {str(x)[:170]}")
        if not d.get('findings'):
            print("   (no findings)")
    except Exception as e:
        verdicts[name] = 'PARSE-FAIL'
        print(f"\n### {name}: PARSE-FAIL {e}\n   raw: {open(f).read()[:160]!r}")
uniq = set(verdicts.values())
print(f"\n>>> verdicts: {verdicts}")
print(f">>> {'UNANIMOUS '+list(uniq)[0] if len(uniq)==1 else 'SPLIT — needs another round / arbitration'}")
