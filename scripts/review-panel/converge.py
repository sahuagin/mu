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


def skipped_ollama_lease(done_text):
    if not re.search(r'\bexit=75\b', done_text):
        return False
    # Round 1 writes:       exit=75 prov=ollama model=...
    # Convergence writes:   exit=75 ollama/<model>
    # Only those ollama-provider forms mean with-ollama-lease --skip-if-held.
    return bool(
        re.search(r'\bprov=ollama(?:\b|-)', done_text)
        or re.search(r'\bexit=75\s+ollama(?:\b|/|-)', done_text)
    )


def load(prefix):
    base = os.path.basename(prefix)
    out = {}
    for f in sorted(glob.glob(prefix + ".rank*.out")):
        tag = os.path.basename(f)[len(base) + 1:-4]   # strip "<base>." and ".out"
        done = f[:-4] + ".done"
        try:
            if os.path.exists(done):
                with open(done) as fh:
                    done_text = fh.read()
                if skipped_ollama_lease(done_text):
                    # with-ollama-lease --skip-if-held: this ollama reviewer
                    # intentionally routed around an operator-held local box.
                    # Omit it from quorum rather than counting it as unparsed.
                    continue
        except Exception:
            pass
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
            # Findings may come back as plain strings (some providers ignore the
            # JSON-object shape and emit free text), so guard each one: only
            # dicts get the structured format; everything else is stringified.
            fs = "; ".join(
                (f"[{x.get('severity', '?')}] {x.get('file', '?')}:{x.get('line', '?')} "
                 f"{str(x.get('issue', ''))[:120]}") if isinstance(x, dict)
                else f"- {str(x)[:120]}"
                for x in d.get('findings', [])
            ) or "(no findings)"
            others.append(f"- reviewer {tag}: verdict={d.get('verdict', '?')} :: {fs}")
        me = data.get(self_tag)
        mine = f"verdict={me.get('verdict', '?')}" if me else "(your previous reply was unparseable)"
        # Subject one-liner from ai-review.sh's required template (mu-599y),
        # handed down via env; literal default keeps any other caller unchanged.
        proj = os.environ.get("_AI_REVIEW_PROJECT_DESC") or "mu (a Rust agent runtime)"
        hdr = (
            f"This is convergence ROUND {rnd} of an antagonistic code-review panel for {proj}. "
            f"You ({self_tag}) previously gave: {mine}.\n"
            "The other reviewers' current positions:\n" + "\n".join(others) +
            "\n\nPress your STRONGEST objections to positions you disagree with, but CONCEDE "
            "points you now accept after seeing their reasoning, and move toward a SINGLE agreed "
            "verdict. Re-read the code (Read/Grep) to settle disputes with evidence — do not just "
            "restate your prior view.\n\n"
            "Respond with ONLY one JSON object (no prose, no fence, nothing before or after it):\n"
            '{"verdict":"approve"|"needs-changes","summary":"<1-2 sentences>",'
            '"concede":["<point you now drop>"],"maintain":["<point you hold, + why>"],'
            '"findings":[{"file":"<path>","line":<int>,"severity":"high"|"medium"|"low",'
            '"issue":"<desc>"}]}\n'
            'Every element of "findings" MUST be a JSON object with exactly those four '
            "keys (file, line, severity, issue), never a bare string and never null. "
            "Use [] if there are no findings.\n\n"
            "Original PR diff under review:")
        with open(outf, 'w') as fh:
            fh.write(hdr + "\n```diff\n" + open(difff).read() + "\n```\n")
        print(f"wrote {outf}")
        return 0

    print(f"unknown subcommand: {cmd}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
