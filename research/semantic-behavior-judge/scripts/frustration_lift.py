#!/usr/bin/env python3
"""Operator-frustration rate-lift per behavior (exposure-normalized).

For each target behavior: is the operator MORE frustrated in sessions that exhibit
it? Measured as marker-RATE (frustration markers / operator messages), which controls
for session length (presence of >=1 marker is an exposure artifact — it rises to ~100%
in long sessions regardless). Lift = rate|with / rate|without.

Reuses behavior_rates.py (sibling) for tool extraction + the syntactic scan. The NEG
set is an illustrative frustration-keyword lexicon (tune to taste). Read-only.

NOTE: this script is included to demonstrate that a frustration-keyword RATE is a
confounded, unreliable outcome signal (see METHODOLOGY.md) — NOT a recommended detector.

Emits JSON -> stdout, human summary -> stderr.
"""
import argparse
import glob
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import behavior_rates as B  # noqa: E402

# --- illustrative frustration-keyword (NEG) markers --------------------------
# Illustrative ONLY — a generic operator-frustration lexicon. Replace/extend with
# markers tuned to your own corpus; do NOT treat this list as authoritative.
NEG = re.compile("|".join([
    r"\bstop\b", r"\bwtf\b", r"\bno\.\s*$", r"that'?s not what", r"\bagain\?",
    r"\bi said\b", r"i didn'?t ask", r"you keep", r"hallucinat", r"\bbroken\b",
    r"don'?t do", r"please don'?t", r"wrong again", r"why are (we|you)",
    r"we'?ve already (done|covered) (it|this)", r"relitigat",
]), re.I)


def cc_user(f):
    """(concatenated operator-typed text, n_operator_messages). tool_result excluded."""
    parts, n = [], 0
    for line in open(f, errors="ignore"):
        try:
            o = json.loads(line)
        except Exception:
            continue
        if o.get("type") != "user":
            continue
        c = (o.get("message") or {}).get("content")
        if isinstance(c, str) and c.strip():
            parts.append(c)
            n += 1
        elif isinstance(c, list):
            txt = [b.get("text", "") for b in c if isinstance(b, dict) and b.get("type") == "text"]
            if any(t.strip() for t in txt):
                parts.append("\n".join(txt))
                n += 1
    return "\n".join(parts), n


def mu_user(f):
    """(operator text, n) from mu user_message events."""
    parts, n = [], 0
    for line in open(f, errors="ignore"):
        try:
            o = json.loads(line)
        except Exception:
            continue
        p = o.get("payload") if isinstance(o.get("payload"), dict) else None
        if p and p.get("kind") == "user_message":
            c = p.get("content")
            if isinstance(c, str) and c.strip():
                parts.append(c)
                n += 1
            elif isinstance(c, list):
                txt = [b.get("text", "") for b in c if isinstance(b, dict) and b.get("type") == "text"]
                if any(t.strip() for t in txt):
                    parts.append("\n".join(txt))
                    n += 1
    return "\n".join(parts), n


def rate(rows):
    msgs = sum(r["msgs"] for r in rows)
    return (sum(r["neg"] for r in rows) / msgs) if msgs else 0.0


def collect(files, parse_tools, parse_user, plane, root):
    rows = []
    for f in files:
        try:
            t = parse_tools(f)
        except Exception:
            continue
        if not t:
            continue
        text, nmsgs = parse_user(f)
        if nmsgs == 0:
            continue
        viol, _, _ = B.scan(t)
        rows.append({
            "neg": len(NEG.findall(text)), "msgs": nmsgs, "ntools": len(t),
            "viol": viol, "plane": plane, "host": B.categorize(f, root)[1],
            "frust": bool(NEG.search(text)),
        })
    return rows


def lift_table(rows):
    out = {}
    for p in B.PREDS:
        w = [r for r in rows if p in r["viol"]]
        wo = [r for r in rows if p not in r["viol"]]
        rw, rwo = rate(w), rate(wo)
        out[p] = {
            "n_with": len(w), "n_without": len(wo),
            "rate_with_per_1k": round(1000 * rw, 2),
            "rate_without_per_1k": round(1000 * rwo, 2),
            "lift": round(rw / rwo, 2) if rwo else None,
        }
    return out


BANDS = [(0, 20, "<20"), (20, 60, "20-60"), (60, 200, "60-200"), (200, 10**9, ">200")]


def size_bands(rows):
    out = []
    for lo, hi, lab in BANDS:
        b = [r for r in rows if lo <= r["ntools"] < hi]
        if b:
            out.append({
                "band": lab, "n": len(b),
                "presence_pct": round(100 * sum(x["frust"] for x in b) / len(b), 1),
                "rate_per_1k": round(1000 * rate(b), 1),
            })
    return out


def banded_lift(rows):
    """Within-size-band rate-lift per predicate — controls for the session-size
    confound (these behaviors cluster in large sessions, which have a structurally
    different per-message marker rate). This is the size-stratified test, vs the raw
    lift which mixes sizes."""
    out = {}
    for p in B.PREDS:
        out[p] = []
        for lo, hi, lab in BANDS:
            band = [r for r in rows if lo <= r["ntools"] < hi]
            w = [r for r in band if p in r["viol"]]
            wo = [r for r in band if p not in r["viol"]]
            rw, rwo = rate(w), rate(wo)
            out[p].append({
                "band": lab, "n_with": len(w), "n_without": len(wo),
                "rate_with_per_1k": round(1000 * rw, 1),
                "rate_without_per_1k": round(1000 * rwo, 1),
                "lift": round(rw / rwo, 2) if (rwo and w) else None,
            })
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default=os.path.expanduser("~/agent-transcripts"))
    args = ap.parse_args()
    root = args.root

    cc_files = [f for f in glob.glob(os.path.join(root, "claude/*/projects/**/*.jsonl"), recursive=True) if "bench" not in f]
    mu_files = glob.glob(os.path.join(root, "mu/*/events/**/session-*.jsonl"), recursive=True)

    cc_rows = collect(cc_files, B.cc_tools, cc_user, "cc", root)
    mu_rows = collect(mu_files, B.mu_tools, mu_user, "mu", root)

    result = {"root": root, "planes": {}}
    for plane, rows in [("cc", cc_rows), ("mu", mu_rows)]:
        result["planes"][plane] = {
            "n_sessions": len(rows),
            "overall_marker_rate_per_1k": round(1000 * rate(rows), 2),
            "lift_raw": lift_table(rows),
            "size_bands": size_bands(rows),
            "lift_banded": banded_lift(rows),
        }
    json.dump(result, sys.stdout, indent=1)
    sys.stdout.write("\n")

    def line(*a):
        print(*a, file=sys.stderr)
    line(f"\ncorpus: {root}")
    for plane, rows in [("cc", cc_rows), ("mu", mu_rows)]:
        d = result["planes"][plane]
        line(f"\n[{plane}] sessions={d['n_sessions']}  overall={d['overall_marker_rate_per_1k']}/1k operator msgs")
        line("  size bands (presence% confounded vs rate normalized):")
        for sb in d["size_bands"]:
            line(f"    {sb['band']:7} n={sb['n']:>4}  presence={sb['presence_pct']:>5}%  rate={sb['rate_per_1k']:>5}/1k")
        line("  RAW predicate rate-lift (size-confounded):")
        for p in B.PREDS:
            L = d["lift_raw"][p]
            lv = f"{L['lift']}x" if L["lift"] is not None else "  -"
            line(f"    {p:18} with={L['rate_with_per_1k']:>6}/1k (n={L['n_with']:>4})  without={L['rate_without_per_1k']:>6}/1k  lift={lv:>6}")
        line("  SIZE-CONTROLLED within-band lift (the real test):")
        for p in ["heredoc", "shell_file_write", "dangerous_bash", "large_bash", "edit_loop"]:
            cells = []
            for bd in d["lift_banded"][p]:
                lv = f"{bd['lift']}x" if bd["lift"] is not None else "-"
                cells.append(f"{bd['band']}:{lv}(n{bd['n_with']})")
            line(f"    {p:18} " + "  ".join(cells))


if __name__ == "__main__":
    main()
