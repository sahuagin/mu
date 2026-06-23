#!/usr/bin/env python3
"""Render a session JSONL (cc-native or mu-native) into a turn-numbered transcript
for the behavior-judge. stdlib only, read-only.

Output: one block per event, prefixed `[NNN] ROLE:` where ROLE ∈ USER / ASSISTANT /
TOOL_CALL(name) / TOOL_RESULT(ok|err). Turn numbers are stable and citable by the
judge's evidence. Assistant prose is kept in full (that's where semantic failures
live); tool-result bodies are capped.

Auto-detects format: mu events carry `payload.kind`; cc lines carry `type` +
`message`. Usage: render_transcript.py <file.jsonl> [--max-tool-chars N]
"""
import argparse
import json
import sys

CAP = 1200


def _text_from_content(c):
    if isinstance(c, str):
        return c
    if isinstance(c, list):
        out = []
        for b in c:
            if not isinstance(b, dict):
                continue
            t = b.get("type")
            if t == "text" and b.get("text"):
                out.append(b["text"])
            elif t == "thinking" and b.get("thinking"):
                out.append("(thinking) " + b["thinking"])
        return "\n".join(out)
    return ""


def render_cc(lines, cap):
    out, n = [], 0
    for line in lines:
        try:
            o = json.loads(line)
        except Exception:
            continue
        typ = o.get("type")
        msg = o.get("message") if isinstance(o.get("message"), dict) else None
        content = msg.get("content") if msg else None
        if typ == "user":
            # may be operator text OR a tool_result block
            if isinstance(content, list) and any(isinstance(b, dict) and b.get("type") == "tool_result" for b in content):
                for b in content:
                    if isinstance(b, dict) and b.get("type") == "tool_result":
                        n += 1
                        body = b.get("content")
                        body = body if isinstance(body, str) else _text_from_content(body)
                        err = b.get("is_error")
                        out.append(f"[{n:03}] TOOL_RESULT({'err' if err else 'ok'}): {body[:cap]}")
            else:
                txt = _text_from_content(content)
                if txt.strip():
                    n += 1
                    out.append(f"[{n:03}] USER: {txt}")
        elif typ == "assistant" and isinstance(content, list):
            txt = _text_from_content(content)
            if txt.strip():
                n += 1
                out.append(f"[{n:03}] ASSISTANT: {txt}")
            for b in content:
                if isinstance(b, dict) and b.get("type") == "tool_use":
                    n += 1
                    args = json.dumps(b.get("input", {}))[:cap]
                    out.append(f"[{n:03}] TOOL_CALL({b.get('name','')}): {args}")
    return out


def render_mu(lines, cap):
    out, n = [], 0
    for line in lines:
        try:
            o = json.loads(line)
        except Exception:
            continue
        p = o.get("payload") if isinstance(o.get("payload"), dict) else None
        if not p:
            continue
        k = p.get("kind")
        if k == "user_message":
            txt = p.get("content")
            txt = txt if isinstance(txt, str) else _text_from_content(txt)
            if txt and txt.strip():
                n += 1
                out.append(f"[{n:03}] USER: {txt}")
        elif k == "assistant_message_event":
            m = p.get("message") or {}
            txt = _text_from_content(m.get("content")) or (m.get("text") or "")
            if txt and txt.strip():
                n += 1
                out.append(f"[{n:03}] ASSISTANT: {txt}")
        elif k == "tool_call":
            n += 1
            args = json.dumps(p.get("arguments", {}))[:cap]
            out.append(f"[{n:03}] TOOL_CALL({p.get('name','')}): {args}")
        elif k == "tool_result":
            n += 1
            body = p.get("content", "")
            body = body if isinstance(body, str) else json.dumps(body)
            out.append(f"[{n:03}] TOOL_RESULT({'err' if p.get('is_error') else 'ok'}): {body[:cap]}")
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("file")
    ap.add_argument("--max-tool-chars", type=int, default=CAP)
    args = ap.parse_args()
    lines = open(args.file, errors="ignore").read().splitlines()
    # detect format
    is_mu = False
    for line in lines[:50]:
        try:
            o = json.loads(line)
        except Exception:
            continue
        if isinstance(o.get("payload"), dict) and o["payload"].get("kind"):
            is_mu = True
            break
        if o.get("type") in ("user", "assistant"):
            break
    blocks = (render_mu if is_mu else render_cc)(lines, args.max_tool_chars)
    sys.stdout.write("\n".join(blocks) + "\n")
    sys.stderr.write(f"rendered {len(blocks)} turns ({'mu' if is_mu else 'cc'} format) from {args.file}\n")


if __name__ == "__main__":
    main()
