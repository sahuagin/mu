#!/usr/bin/env python3
"""Convergence helpers for the consensus review loop (consensus.sh).

Subcommands:
  agree  <prefix>
      Read <prefix>.rank*.out, parse each reviewer's verdict. Print
      "AGREE <verdict>" and exit 0 iff every reviewer parsed AND all verdicts
      are equal; otherwise print "SPLIT <json of per-reviewer verdicts>" exit 1.

  prompt <prev-prefix> <round> <diff-file> <self-tag> <out-file>
      Write <self-tag>'s convergence prompt for <round>: the diff, every
      reviewer's previous-round position, and antagonistic-converge instructions.
"""
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


def load(prefix):
    base = os.path.basename(prefix)
    out = {}
    for f in sorted(glob.glob(prefix + ".rank*.out")):
        tag = os.path.basename(f)[len(base) + 1:-4]   # strip "<base>." and ".out"
        try:
            out[tag] = extract(open(f).read())
        except Exception:
            out[tag] = None
    return out


def main():
    cmd = sys.argv[1]
    if cmd == "agree":
        data = load(sys.argv[2])
        verdicts = {t: (d.get('verdict', '?').lower() if d else 'unparsed')
                    for t, d in data.items()}
        real = [v for v in verdicts.values() if v not in ('unparsed', '?')]
        if data and len(set(real)) == 1 and len(real) == len(verdicts):
            print("AGREE " + real[0])
            return 0
        print("SPLIT " + json.dumps(verdicts))
        return 1

    if cmd == "prompt":
        prev_prefix, rnd, difff, self_tag, outf = sys.argv[2:7]
        data = load(prev_prefix)
        others = []
        for tag, d in data.items():
            if tag == self_tag or not d:
                continue
            fs = "; ".join(
                f"[{x.get('severity', '?')}] {x.get('file', '?')}:{x.get('line', '?')} "
                f"{x.get('issue', '')[:120]}" for x in d.get('findings', [])
            ) or "(no findings)"
            others.append(f"- reviewer {tag}: verdict={d.get('verdict', '?')} :: {fs}")
        me = data.get(self_tag)
        mine = f"verdict={me.get('verdict', '?')}" if me else "(your previous reply was unparseable)"
        hdr = (
            f"This is convergence ROUND {rnd} of an antagonistic code-review panel for `mu` "
            f"(a Rust agent runtime). You ({self_tag}) previously gave: {mine}.\n"
            "The other reviewers' current positions:\n" + "\n".join(others) +
            "\n\nPress your STRONGEST objections to positions you disagree with, but CONCEDE "
            "points you now accept after seeing their reasoning, and move toward a SINGLE agreed "
            "verdict. Re-read the code (Read/Grep) to settle disputes with evidence — do not just "
            "restate your prior view.\n\n"
            "Respond with ONLY one JSON object (no prose, no fence):\n"
            '{"verdict":"approve"|"needs-changes","summary":"<1-2 sentences>",'
            '"concede":["<point you now drop>"],"maintain":["<point you hold, + why>"],'
            '"findings":[{"file":"<path>","line":<int>,"severity":"high"|"medium"|"low",'
            '"issue":"<desc>"}]}\n\n'
            "Original PR diff under review:")
        with open(outf, 'w') as fh:
            fh.write(hdr + "\n```diff\n" + open(difff).read() + "\n```\n")
        print(f"wrote {outf}")
        return 0

    print(f"unknown subcommand: {cmd}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
