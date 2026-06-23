#!/usr/bin/env python3
"""Run the behavior-judge on one (transcript, class) via ollama /api/chat.

Uses the model AS-LOADED: NO `num_ctx` in options — passing num_ctx forces ollama
to evict and reload the model. Stdlib only.

System prompt = judge/behavior-judge-system-prompt.txt with {CLASS_RUBRIC} filled
from judge/rubric.md for the given class. Forces JSON output (format=json) and does
NOT override sampling (temperature 0 degenerates qwen3-family models — use the model's
recommended sampling; make it reproducible with a fixed seed if needed).

Usage: run_judge.py --transcript <rendered.txt> --cls <class-id> [--model M --host H]
Prints the verdict JSON to stdout; timing/token counts to stderr.
"""
import argparse
import json
import os
import sys
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
JUDGE = os.path.join(HERE, "..", "judge")


def class_rubric(cls):
    text = open(os.path.join(JUDGE, "rubric.md")).read()
    for blk in text.split("\n## "):
        if blk.strip().startswith(cls):
            return "## " + blk.strip()
    raise SystemExit(f"class '{cls}' not found in rubric.md")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--transcript", required=True)
    ap.add_argument("--cls", required=True)
    ap.add_argument("--model", default="qwen3.6:35b-a3b-q8_0")
    ap.add_argument("--host", default="localhost:11434")
    args = ap.parse_args()

    sys_t = open(os.path.join(JUDGE, "behavior-judge-system-prompt.txt")).read()
    system = sys_t.replace("{CLASS_RUBRIC}", class_rubric(args.cls))
    transcript = open(args.transcript, errors="ignore").read()

    payload = {
        "model": args.model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": "Rendered transcript follows.\n\n" + transcript},
        ],
        "stream": False,
        "format": "json",
        # Use the model AS-LOADED: do NOT override sampling here. The loaded model's
        # baked params (for qwen3-family, e.g. temp ~1 / top_k 20 / top_p 0.95 /
        # min_p 0) are used as-is; temperature 0 is a known footgun on qwen3, and
        # changing num_ctx would force a reload. No options overrides.
    }
    req = urllib.request.Request(
        "http://" + args.host + "/api/chat",
        data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json"},
    )
    t0 = time.time()
    out = json.loads(urllib.request.urlopen(req, timeout=900).read())
    dt = time.time() - t0
    content = out.get("message", {}).get("content", "")
    sys.stderr.write(
        f"[{args.cls}] {dt:.0f}s  prompt_tokens={out.get('prompt_eval_count')} "
        f"gen_tokens={out.get('eval_count')}\n"
    )
    print(content)


if __name__ == "__main__":
    main()
