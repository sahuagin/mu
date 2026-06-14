#!/usr/bin/env python3
"""Smoke-test the mu_anthropic_py binding against REAL Claude Code session logs.

This is the layer that tests the binding (the Rust seam itself is untested by
design — it's thin one-to-one delegation). We import the wheel and call every
export, feeding it the wire messages cc stores verbatim under `.message`.

Run AFTER `maturin develop` (or with the built .so on sys.path):
    python3 cc_log_smoke.py
"""

import glob
import json
import os
import sys

import mu_anthropic_py as ma  # ty: ignore[unresolved-import]


def load_cc_messages(limit: int = 1000):
    msgs = []
    for f in sorted(glob.glob(os.path.expanduser("~/.claude-personal/projects/*/*.jsonl"))):
        try:
            lines = open(f).readlines()
        except PermissionError:
            continue
        for line in lines:
            try:
                o = json.loads(line)
            except Exception:
                continue
            if o.get("type") == "assistant" and isinstance(o.get("message"), dict):
                msgs.append(o["message"])
                if len(msgs) >= limit:
                    return msgs
    return msgs


def main():
    msgs = load_cc_messages()
    print(f"loaded {len(msgs)} real assistant messages from cc logs")

    ok = bad = 0
    tool_use = thinking = text = 0
    for m in msgs:
        s = json.dumps(m)
        if ma.is_valid_response_message(s):
            normalized = ma.parse_response_message(s)  # round-trips through typed Rust
            nv = json.loads(normalized)
            for b in nv.get("content", []):
                t = b.get("type")
                tool_use += t == "tool_use"
                thinking += t == "thinking"
                text += t == "text"
            ok += 1
        else:
            bad += 1
            if bad <= 5:
                print(f"  REJECTED: {json.dumps(m)[:120]}")

    print(f"parsed {ok}/{len(msgs)}  (rejected {bad})")
    print(f"  blocks via Rust types: tool_use={tool_use} thinking={thinking} text={text}")
    assert bad == 0, f"{bad} real messages failed the typed binding"
    print("OK — every real cc-log message round-trips through mu-anthropic via pyo3.")


if __name__ == "__main__":
    sys.exit(main())
