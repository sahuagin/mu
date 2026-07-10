#!/usr/bin/env python3
"""Parse a panel's per-rank .out files into verdicts/findings.
usage: parse_panel.py <prefix>   # e.g. .../pr282.r1  -> reads <prefix>.rank*.out
Robust to ```json fences and [thinking] lines."""
import json, re, glob, sys, os

def extract(s):
    s = s.strip()
    s = re.sub(r'^\s*\[thinking\].*?$', '', s, flags=re.M)
    m = re.search(r'```(?:json)?\s*(\{.*\})\s*```', s, re.S)
    if m:
        s = m.group(1)
    else:
        a, b = s.find('{'), s.rfind('}')
        if a >= 0 and b > a:
            s = s[a:b+1]
    return json.loads(s)


def has_verdict(s):
    """True if `s` yields a usable verdict. Used by --check so the dispatch
    harness can decide whether a reviewer needs a constrained re-ask (mu-0htd):
    local models sometimes answer in prose without any verdict, which
    consensus can't score.

    Acceptance MUST match converge.py's `parse()` exactly, or --check triggers
    a re-ask for output converge.py would have scored fine (a reviewer flagged
    this drift). converge.py accepts EITHER a leading `VERDICT: <x>` line (even
    with the JSON body absent/truncated — verdict_prefix) OR a parseable JSON
    object; mirror both here."""
    # Leading-VERDICT-line acceptance (converge.py::verdict_prefix).
    first = s.lstrip().splitlines()[0] if s.strip() else ""
    if re.match(r'(?i)^VERDICT\s*:\s*(APPROVE|NEEDS[-_ ]CHANGES|REJECT)\b', first.strip()):
        return True
    # JSON-object acceptance.
    try:
        d = extract(s)
        return isinstance(d, dict) and str(d.get('verdict', '')).lower() in (
            'approve', 'needs-changes', 'reject')
    except Exception:
        return False


# --check <file>: exit 0 if the file holds a parseable verdict, 1 otherwise.
# No stdout — a pure predicate for shell `if`.
if len(sys.argv) >= 3 and sys.argv[1] == '--check':
    try:
        ok = has_verdict(open(sys.argv[2]).read())
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
