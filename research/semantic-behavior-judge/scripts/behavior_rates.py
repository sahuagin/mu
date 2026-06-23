#!/usr/bin/env python3
"""Syntactic behavior-rate scanner over a corpus of agent session transcripts.

Self-contained (stdlib only). Detection is purely syntactic over the tool stream
(no model call, read-only): regex/tool-stream predicates for common anti-patterns
(heredoc, shell-file-write, dangerous-bash, force-push, edit-loop, edit-before-read,
etc.). Walks a corpus laid out as `<root>/{claude,mu}/<host>/...` and breaks results
down by fleet (cc/mu) x host, with at-risk denominators (bash-behaviors over sessions
with >=1 bash turn; edit-behaviors over >=1 edit turn).

Emits: human summary -> stderr, full JSON -> stdout.
"""
import argparse
import glob
import json
import os
import re
import sys

# --- syntactic violation predicates ------------------------------------------
READ = {"read"}
EDIT = {"edit", "write", "multiedit", "notebookedit", "applypatch", "apply_patch"}
BASH = {"bash", "shell", "run"}

RX_DANGER = re.compile(r"\brm\s+-[a-z]*r[a-z]*f|\brm\s+-[a-z]*f[a-z]*r|\bdd\s+.*\bof=|\bmkfs\b|:\(\)\s*\{", re.I)
RX_FORCE = re.compile(r"\b(git|jj)\b.*(push\s+.*(--force|-f)\b|reset\s+--hard|push\s+--force|branch\s+-D)", re.I)
RX_HEREDOC = re.compile(r"<<-?\s*['\"\\]?[A-Za-z_]")
RX_CODE_HEREDOC = re.compile(r"\b(python3?|node|deno|ruby|perl|php|jq|psql|sqlite3|Rscript)\b[^\n]{0,60}<<")
RX_SHELL_WRITE = re.compile(
    r"(?<![0-9&])>{1,2}\s*(?!/dev/)(?!/tmp)(?!/var/tmp)(?!\$\{?TMP)(?!&)([~./][^\s&|;>]*|[A-Za-z][\w.-]*\.[A-Za-z][\w.]*)|\btee\s+(?!-)[^\s&|;>]",
    re.I,
)

PREDS = [
    "heredoc", "code_in_heredoc", "shell_file_write", "large_bash",
    "dangerous_bash", "force_push", "edit_loop", "edit_before_read",
]
BASH_PREDS = {"heredoc", "code_in_heredoc", "shell_file_write", "large_bash", "dangerous_bash", "force_push"}
EDIT_PREDS = {"edit_loop", "edit_before_read"}


def _argval(args, *keys):
    if isinstance(args, str):
        try:
            args = json.loads(args)
        except Exception:
            return args
    if isinstance(args, dict):
        for k in keys:
            if args.get(k):
                return args[k]
    return ""


def cc_tools(f):
    out = []
    for line in open(f, errors="ignore"):
        try:
            o = json.loads(line)
        except Exception:
            continue
        msg = o.get("message") if isinstance(o.get("message"), dict) else None
        content = msg.get("content") if msg else None
        if isinstance(content, list):
            for c in content:
                if isinstance(c, dict) and c.get("type") == "tool_use":
                    out.append((str(c.get("name", "")).lower(), c.get("input", {})))
    return out


def mu_tools(f):
    out = []
    for line in open(f, errors="ignore"):
        try:
            o = json.loads(line)
        except Exception:
            continue
        p = o.get("payload") if isinstance(o.get("payload"), dict) else None
        if p and p.get("kind") == "tool_call":
            out.append((str(p.get("name", "")).lower(), p.get("arguments", {})))
    return out


def mu_is_faux(f):
    """True if the session_created event stamps model='faux' (a synthetic/test
    session — e.g. fixtures with a provider stamped but no real context assembly).
    Excluded so the baseline reflects real work, not test fixtures."""
    for line in open(f, errors="ignore"):
        try:
            o = json.loads(line)
        except Exception:
            continue
        p = o.get("payload") if isinstance(o.get("payload"), dict) else None
        if p and p.get("kind") == "session_created":
            return p.get("model") == "faux"
    return False


def scan(tools):
    """Return (violation_set, has_bash_turn, has_edit_turn)."""
    v = set()
    read_files = set()
    edit_counts = {}
    has_bash = False
    has_edit = False
    for name, args in tools:
        f = _argval(args, "file_path", "path", "notebook_path")
        cmd = _argval(args, "command", "cmd")
        if not isinstance(cmd, str):
            cmd = ""
        if name in READ and f:
            read_files.add(f)
        if name in EDIT:
            has_edit = True
            if f and f not in read_files:
                v.add("edit_before_read")
            if f:
                edit_counts[f] = edit_counts.get(f, 0) + 1
                if edit_counts[f] >= 5:
                    v.add("edit_loop")
        if name in BASH and cmd:
            has_bash = True
            if RX_DANGER.search(cmd):
                v.add("dangerous_bash")
            if RX_FORCE.search(cmd):
                v.add("force_push")
            if RX_HEREDOC.search(cmd):
                v.add("heredoc")
            if RX_CODE_HEREDOC.search(cmd):
                v.add("code_in_heredoc")
            if RX_SHELL_WRITE.search(cmd):
                v.add("shell_file_write")
            if len(cmd) > 1200:
                v.add("large_bash")
    return v, has_bash, has_edit


def new_bucket():
    return {
        "sessions_total": 0, "sessions_with_tools": 0,
        "sessions_with_bash": 0, "sessions_with_edit": 0,
        "counts": {p: 0 for p in PREDS},
    }


def categorize(path, root):
    rel = os.path.relpath(path, root).split(os.sep)
    # layout: {claude,mu}/<host>/...
    plane = {"claude": "cc", "mu": "mu"}.get(rel[0], rel[0])
    host = rel[1] if len(rel) > 1 else "?"
    return plane, host


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default=os.path.expanduser("~/agent-transcripts"))
    ap.add_argument("--exclude-faux", action="store_true",
                    help="drop mu sessions whose session_created model is 'faux' (test fixtures)")
    args = ap.parse_args()
    root = args.root

    cc_files = [f for f in glob.glob(os.path.join(root, "claude/*/projects/**/*.jsonl"), recursive=True) if "bench" not in f]
    mu_files = glob.glob(os.path.join(root, "mu/*/events/**/session-*.jsonl"), recursive=True)
    n_faux = 0
    if args.exclude_faux:
        kept = []
        for f in mu_files:
            if mu_is_faux(f):
                n_faux += 1
            else:
                kept.append(f)
        mu_files = kept

    buckets = {}  # key -> bucket;  keys: "overall", "cc", "mu", "cc/host", "mu/host"
    buckets["overall"] = new_bucket()

    def record(plane, host, parse, f):
        try:
            tools = parse(f)
        except Exception:
            return
        keys = ["overall", plane, f"{plane}/{host}"]
        for k in keys:
            buckets.setdefault(k, new_bucket())
            buckets[k]["sessions_total"] += 1
        if not tools:
            return
        v, has_bash, has_edit = scan(tools)
        for k in keys:
            b = buckets[k]
            b["sessions_with_tools"] += 1
            if has_bash:
                b["sessions_with_bash"] += 1
            if has_edit:
                b["sessions_with_edit"] += 1
            for p in v:
                b["counts"][p] += 1

    for f in cc_files:
        plane, host = categorize(f, root)
        record("cc", host, cc_tools, f)
    for f in mu_files:
        plane, host = categorize(f, root)
        record("mu", host, mu_tools, f)

    # compute rates (at-risk normalized)
    def rates(b):
        out = {}
        for p in PREDS:
            denom = b["sessions_with_bash"] if p in BASH_PREDS else b["sessions_with_edit"]
            n = b["counts"][p]
            out[p] = {
                "n": n,
                "at_risk_denom": denom,
                "pct_at_risk": round(100 * n / denom, 2) if denom else None,
                "pct_of_tool_sessions": round(100 * n / b["sessions_with_tools"], 2) if b["sessions_with_tools"] else None,
            }
        return out

    result = {"root": root, "cc_files": len(cc_files), "mu_files": len(mu_files),
              "mu_faux_excluded": n_faux, "buckets": {}}
    for k, b in sorted(buckets.items()):
        result["buckets"][k] = {**{x: b[x] for x in ("sessions_total", "sessions_with_tools", "sessions_with_bash", "sessions_with_edit")}, "rates": rates(b)}

    json.dump(result, sys.stdout, indent=1)
    sys.stdout.write("\n")

    # human summary -> stderr
    def line(*a):
        print(*a, file=sys.stderr)
    line(f"\ncorpus: {root}   cc_files={len(cc_files)} mu_files={len(mu_files)} (mu_faux_excluded={n_faux})")
    for k in ["overall", "cc", "cc/host1", "cc/host2", "mu", "mu/host1", "mu/host2"]:
        if k not in result["buckets"]:
            continue
        b = result["buckets"][k]
        line(f"\n[{k}]  tool-sessions={b['sessions_with_tools']}  bash={b['sessions_with_bash']} edit={b['sessions_with_edit']}")
        for p in PREDS:
            r = b["rates"][p]
            ar = f"{r['pct_at_risk']}%" if r["pct_at_risk"] is not None else "  -"
            line(f"  {p:18} n={r['n']:>5}  {ar:>7} of at-risk")


if __name__ == "__main__":
    main()
