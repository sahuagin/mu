#!/usr/bin/env python3
"""Batch behavior-judge over a list of session_refs → results JSONL.

Reuses the verified `render_transcript.py` + `run_judge.py` via subprocess. Robust:
a bad session or call is recorded as an error row, never fatal. Appends one result
row per (session, class) and flushes, so a killed batch keeps partial progress.

Manifest: one line per session — "<session_ref>" or "<session_ref>\\t<documented_class>".
  session_ref = cc:<uuid>  |  mu:<daemon>:session-<n>
Result rows: {session_ref, fleet, documented, behavior, occurred, severity,
              confidence, n_evidence[, error]}

Usage: run_judge_batch.py --manifest M --out R.jsonl [--classes c,c,..] [--host H] [--tmp DIR]
Model handling is run_judge.py's: as-loaded, no num_ctx, no sampling override.
"""
import argparse
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
RENDER = os.path.join(HERE, "render_transcript.py")
JUDGE = os.path.join(HERE, "run_judge.py")
CLASSES = ["false_success", "map_as_terrain", "scope_overreach", "relitigation", "dismissiveness"]


def resolve_path(ref, host):
    """session_ref -> jsonl path under ~/agent-transcripts (or None).

    Adjust the layout globs below to match your own transcript store.
    """
    if ref.startswith("cc:"):
        uuid = ref[3:]
        rc = f"find ~/agent-transcripts/claude -name '{uuid}.jsonl' -not -path '*/subagents/*' 2>/dev/null | head -1"
    elif ref.startswith("mu:"):
        body = ref[3:]
        if ":" not in body:
            return None
        daemon, sess = body.split(":", 1)
        rc = f"ls ~/agent-transcripts/mu/*/events/{daemon}/{sess}.jsonl 2>/dev/null | head -1"
    else:
        return None
    r = subprocess.run(["ssh", "-o", "BatchMode=yes", host, rc],
                       capture_output=True, text=True, timeout=40)
    lines = r.stdout.strip().splitlines()
    return lines[0] if lines else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--manifest", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--classes", default=",".join(CLASSES))
    ap.add_argument("--host", default="localhost")
    ap.add_argument("--tmp", default="/tmp/cc-judge-batch")
    args = ap.parse_args()
    classes = [c.strip() for c in args.classes.split(",") if c.strip()]
    os.makedirs(args.tmp, exist_ok=True)

    items = []
    for line in open(args.manifest):
        line = line.rstrip("\n")
        if not line or line.startswith("#"):
            continue
        parts = line.split("\t")
        items.append((parts[0].strip(), parts[1].strip() if len(parts) > 1 else ""))

    out = open(args.out, "a")

    def emit(rec):
        out.write(json.dumps(rec) + "\n")
        out.flush()

    for i, (ref, documented) in enumerate(items):
        fleet = ref.split(":", 1)[0]
        sys.stderr.write(f"[{i+1}/{len(items)}] {ref} ({documented})\n")
        sys.stderr.flush()
        path = resolve_path(ref, args.host)
        if not path:
            emit({"session_ref": ref, "documented": documented, "error": "resolve_failed"})
            continue
        safe = ref.replace(":", "_").replace("/", "_")
        jl = os.path.join(args.tmp, safe + ".jsonl")
        txt = os.path.join(args.tmp, safe + ".txt")
        # pull (bytes, to survive any encoding quirks)
        cat = subprocess.run(["ssh", "-o", "BatchMode=yes", args.host, f"cat {path}"],
                             capture_output=True, timeout=180)
        if cat.returncode != 0 or not cat.stdout:
            emit({"session_ref": ref, "documented": documented, "error": "cat_failed"})
            continue
        open(jl, "wb").write(cat.stdout)
        # render
        rr = subprocess.run(["python3", RENDER, jl], capture_output=True, text=True, timeout=180)
        open(txt, "w").write(rr.stdout)
        if not rr.stdout.strip():
            emit({"session_ref": ref, "documented": documented, "error": "render_empty"})
            continue
        for cls in classes:
            try:
                jr = subprocess.run(["python3", JUDGE, "--transcript", txt, "--cls", cls],
                                    capture_output=True, text=True, timeout=900)
                v = json.loads(jr.stdout)
                rec = {"session_ref": ref, "fleet": fleet, "documented": documented,
                       "behavior": cls, "occurred": v.get("occurred"),
                       "severity": v.get("severity"), "confidence": v.get("confidence"),
                       "n_evidence": len(v.get("evidence", []))}
                emit(rec)
                sys.stderr.write(f"    {cls}: occurred={v.get('occurred')}\n")
                sys.stderr.flush()
            except Exception as e:
                emit({"session_ref": ref, "fleet": fleet, "documented": documented,
                      "behavior": cls, "occurred": None, "error": str(e)[:200]})
    out.close()
    sys.stderr.write("BATCH DONE\n")


if __name__ == "__main__":
    main()
