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
            print(f"   [{x.get('severity','?')}] {x.get('file','?')}:{x.get('line','?')} — {x.get('issue','')[:170]}")
        if not d.get('findings'):
            print("   (no findings)")
    except Exception as e:
        verdicts[name] = 'PARSE-FAIL'
        print(f"\n### {name}: PARSE-FAIL {e}\n   raw: {open(f).read()[:160]!r}")
uniq = set(verdicts.values())
print(f"\n>>> verdicts: {verdicts}")
print(f">>> {'UNANIMOUS '+list(uniq)[0] if len(uniq)==1 else 'SPLIT — needs another round / arbitration'}")
