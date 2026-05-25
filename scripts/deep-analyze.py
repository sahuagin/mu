#!/usr/bin/env python3
"""Deep analysis of individual mu event log sessions.

Usage:
    deep-analyze.py <session-jsonl> [--report-dir DIR]

Drills into one session's event stream and produces a detailed report:
tool error patterns, retry loops, context growth, turn productivity.

Designed to be called by the forensic orchestrator or manually for
investigation. Reads mu-format JSONL (native or imported).
"""

import argparse
import json
import sys
from collections import Counter, defaultdict
from pathlib import Path


def load_events(path: Path) -> list[dict]:
    events = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if line:
                try:
                    events.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
    return events


# ── Tool error analysis ────────────────────────────────────────────

def analyze_tool_errors(events: list[dict]) -> dict:
    """Extract tool error patterns: which tools fail, error messages,
    retry success rate."""
    errors = []
    tool_calls = []

    for ev in events:
        p = ev.get("payload", {})
        kind = p.get("kind", "")
        if kind == "tool_call":
            tool_calls.append({
                "id": ev["id"],
                "ts": ev.get("timestamp_unix_ms", 0),
                "call_id": p.get("call_id", ""),
                "name": p.get("name", ""),
            })
        elif kind == "tool_result" and p.get("is_error", False):
            content = p.get("content", "")[:500]
            errors.append({
                "id": ev["id"],
                "ts": ev.get("timestamp_unix_ms", 0),
                "call_id": p.get("call_id", ""),
                "content": content,
            })

    # Match errors to tool names via call_id
    call_id_to_name = {tc["call_id"]: tc["name"] for tc in tool_calls}
    error_by_tool = Counter()
    error_messages = defaultdict(list)

    for err in errors:
        name = call_id_to_name.get(err["call_id"], "unknown")
        error_by_tool[name] += 1
        # Normalize error message to first line for clustering
        first_line = err["content"].split("\n")[0][:120]
        error_messages[name].append(first_line)

    # Cluster error messages per tool
    error_clusters = {}
    for tool, msgs in error_messages.items():
        clusters = Counter(msgs)
        error_clusters[tool] = [
            {"message": msg, "count": count}
            for msg, count in clusters.most_common(5)
        ]

    return {
        "total_tool_calls": len(tool_calls),
        "total_errors": len(errors),
        "error_rate": round(len(errors) / max(len(tool_calls), 1), 3),
        "errors_by_tool": dict(error_by_tool.most_common()),
        "error_clusters": error_clusters,
    }


# ── Retry loop detection ──────────────────────────────────────────

def detect_retry_loops(events: list[dict]) -> list[dict]:
    """Find sequences where the same tool is called with similar
    arguments, suggesting the model is retrying after failure.

    Distinguishes retries (same tool + similar args) from normal
    sequential work (same tool + different args, e.g., many Bash
    calls doing different things)."""
    tool_sequence = []
    for ev in events:
        p = ev.get("payload", {})
        kind = p.get("kind", "")
        if kind == "tool_call":
            args = p.get("arguments", {})
            # Normalize args to a comparable key
            if isinstance(args, dict):
                # For Bash, the command is the discriminator
                arg_key = args.get("command", args.get("file_path",
                    args.get("query", json.dumps(args, sort_keys=True)[:200])))
            else:
                arg_key = str(args)[:200]
            tool_sequence.append({
                "id": ev["id"],
                "ts": ev.get("timestamp_unix_ms", 0),
                "name": p.get("name", ""),
                "call_id": p.get("call_id", ""),
                "arg_key": str(arg_key)[:200],
            })

    # Find runs of the same tool with similar arguments
    loops = []
    i = 0
    while i < len(tool_sequence):
        name = tool_sequence[i]["name"]
        arg_key = tool_sequence[i]["arg_key"]
        run = [tool_sequence[i]]
        j = i + 1
        while j < len(tool_sequence) and tool_sequence[j]["name"] == name:
            # Check if args are similar (same first 100 chars)
            if tool_sequence[j]["arg_key"][:100] == arg_key[:100]:
                run.append(tool_sequence[j])
            else:
                break
            j += 1
        if len(run) >= 3:
            loops.append({
                "tool": name,
                "length": len(run),
                "start_id": run[0]["id"],
                "end_id": run[-1]["id"],
                "arg_preview": arg_key[:80],
                "duration_ms": run[-1]["ts"] - run[0]["ts"] if run[-1]["ts"] and run[0]["ts"] else 0,
            })
        i = j if j > i + 1 else i + 1

    return loops


# ── Context growth analysis ───────────────────────────────────────

def analyze_context_growth(events: list[dict]) -> dict:
    """Track input token growth per assistant turn.
    Only works for sessions that have usage data."""
    turns = []
    for ev in events:
        p = ev.get("payload", {})
        if p.get("kind") == "assistant_message_event":
            msg = p.get("message", {})
            usage = msg.get("usage")
            if usage:
                # Total prompt size = input_tokens + cache_read + cache_creation.
                # claude-code's input_tokens is only the non-cached portion;
                # the real context size includes cached spans.
                raw_input = usage.get("input_tokens", 0)
                cache_read = usage.get("cache_read_input_tokens", 0)
                cache_creation = usage.get("cache_creation_input_tokens", 0)
                total_prompt = raw_input + cache_read + cache_creation
                if total_prompt > 0:
                    turns.append({
                        "id": ev["id"],
                        "ts": ev.get("timestamp_unix_ms", 0),
                        "total_prompt_tokens": total_prompt,
                        "raw_input_tokens": raw_input,
                        "output_tokens": usage.get("output_tokens", 0),
                        "cache_read": cache_read,
                        "cache_creation": cache_creation,
                    })

    if not turns:
        return {"has_usage_data": False}

    prompt_tokens = [t["total_prompt_tokens"] for t in turns]
    output_tokens = [t["output_tokens"] for t in turns]

    n = len(prompt_tokens)
    if n >= 2:
        avg_growth_per_turn = (prompt_tokens[-1] - prompt_tokens[0]) / max(n - 1, 1)
    else:
        avg_growth_per_turn = 0

    # Detect token drops (likely compaction)
    drops = []
    for i in range(1, len(prompt_tokens)):
        prev = prompt_tokens[i - 1]
        curr = prompt_tokens[i]
        if prev > 1000 and curr < prev * 0.7:
            drops.append({
                "turn": i,
                "before": prev,
                "after": curr,
                "reduction_pct": round((1 - curr / prev) * 100, 1),
            })

    # Cache hit ratio: cache_read / total_prompt across all turns
    total_cache_read = sum(t["cache_read"] for t in turns)
    total_prompt_all = sum(t["total_prompt_tokens"] for t in turns)
    cache_hit_ratio = round(total_cache_read / max(total_prompt_all, 1), 3)

    return {
        "has_usage_data": True,
        "turn_count": n,
        "first_prompt_tokens": prompt_tokens[0] if prompt_tokens else 0,
        "last_prompt_tokens": prompt_tokens[-1] if prompt_tokens else 0,
        "peak_prompt_tokens": max(prompt_tokens) if prompt_tokens else 0,
        "avg_growth_per_turn": round(avg_growth_per_turn),
        "total_output_tokens": sum(output_tokens),
        "likely_compactions": drops,
        "cache_hit_ratio": cache_hit_ratio,
    }


# ── Turn productivity ─────────────────────────────────────────────

def analyze_turn_productivity(events: list[dict]) -> dict:
    """Classify turns as productive (file writes/edits, tool use with
    results) vs unproductive (errors, empty responses, retries)."""
    productive_tools = {"Edit", "Write", "NotebookEdit"}
    read_tools = {"Read", "Grep", "Glob", "Bash"}

    tool_call_names = {}
    tool_results = {}
    for ev in events:
        p = ev.get("payload", {})
        kind = p.get("kind", "")
        if kind == "tool_call":
            tool_call_names[p.get("call_id", "")] = p.get("name", "")
        elif kind == "tool_result":
            cid = p.get("call_id", "")
            tool_results[cid] = {
                "is_error": p.get("is_error", False),
                "content_len": len(p.get("content", "")),
            }

    # Count by category
    write_calls = 0
    read_calls = 0
    error_calls = 0
    other_calls = 0
    empty_results = 0

    for cid, name in tool_call_names.items():
        result = tool_results.get(cid)
        if result and result["is_error"]:
            error_calls += 1
        elif name in productive_tools:
            write_calls += 1
        elif name in read_tools:
            read_calls += 1
        else:
            other_calls += 1

        if result and result["content_len"] == 0:
            empty_results += 1

    total = len(tool_call_names)
    return {
        "total_tool_calls": total,
        "write_calls": write_calls,
        "read_calls": read_calls,
        "error_calls": error_calls,
        "other_calls": other_calls,
        "empty_results": empty_results,
        "write_ratio": round(write_calls / max(total, 1), 3),
        "error_ratio": round(error_calls / max(total, 1), 3),
    }


# ── Assistant message patterns ────────────────────────────────────

# ── Startup tax detection ─────────────────────────────────────────

def analyze_startup_tax(events: list[dict]) -> dict:
    """Detect early-session error patterns: tool calls in the first N
    events that fail and are retried. The classic pattern is agent
    memory commands with unquoted args failing 2-3 times before the
    model learns the quoting convention."""
    # Collect the first 30 tool call/result pairs
    tool_events = []
    for ev in events:
        p = ev.get("payload", {})
        kind = p.get("kind", "")
        if kind in ("tool_call", "tool_result"):
            tool_events.append(ev)
        if len(tool_events) >= 60:
            break

    if not tool_events:
        return {"has_startup_data": False}

    # Pair calls with results
    early_calls = []
    early_errors = 0
    call_map = {}
    for ev in tool_events:
        p = ev.get("payload", {})
        kind = p.get("kind", "")
        if kind == "tool_call":
            call_map[p.get("call_id", "")] = {
                "name": p.get("name", ""),
                "args_preview": str(p.get("arguments", ""))[:100],
            }
        elif kind == "tool_result":
            cid = p.get("call_id", "")
            call_info = call_map.get(cid, {"name": "?", "args_preview": ""})
            is_err = p.get("is_error", False)
            early_calls.append({
                "name": call_info["name"],
                "args": call_info["args_preview"],
                "is_error": is_err,
                "error_preview": p.get("content", "")[:100] if is_err else "",
            })
            if is_err:
                early_errors += 1

    if not early_calls:
        return {"has_startup_data": False}

    # Find the first N calls and compute startup error rate
    first_10 = early_calls[:10]
    first_10_errors = sum(1 for c in first_10 if c["is_error"])

    # Detect the "agent memory" quoting pattern specifically
    agent_memory_retries = 0
    for i, call in enumerate(early_calls[:15]):
        if "agent" in call["args"].lower() or "memory" in call["args"].lower():
            if call["is_error"]:
                agent_memory_retries += 1

    # Compare first-10 error rate to session-wide
    all_tool_results = [
        ev for ev in events
        if ev.get("payload", {}).get("kind") == "tool_result"
    ]
    all_errors = sum(
        1 for ev in all_tool_results
        if ev.get("payload", {}).get("is_error", False)
    )
    overall_rate = all_errors / max(len(all_tool_results), 1)
    startup_rate = first_10_errors / max(len(first_10), 1)

    return {
        "has_startup_data": True,
        "first_10_calls": len(first_10),
        "first_10_errors": first_10_errors,
        "startup_error_rate": round(startup_rate, 3),
        "overall_error_rate": round(overall_rate, 3),
        "startup_tax_detected": startup_rate > overall_rate * 2 and first_10_errors >= 2,
        "agent_memory_retries": agent_memory_retries,
        "first_errors": [
            {"tool": c["name"], "args": c["args"][:60], "error": c["error_preview"][:80]}
            for c in first_10 if c["is_error"]
        ],
    }


def analyze_assistant_patterns(events: list[dict]) -> dict:
    """Analyze assistant message content for behavioral patterns."""
    messages = []
    for ev in events:
        p = ev.get("payload", {})
        if p.get("kind") == "assistant_message_event":
            msg = p.get("message", {})
            content = msg.get("content", "")
            messages.append({
                "id": ev["id"],
                "length": len(content),
                "stop_reason": msg.get("stop_reason", ""),
            })

    if not messages:
        return {"message_count": 0}

    lengths = [m["length"] for m in messages]
    stop_reasons = Counter(m["stop_reason"] for m in messages)

    # Detect very short responses (possible confusion/refusal)
    short_responses = sum(1 for l in lengths if l < 50)
    # Detect very long responses (possible verbosity)
    long_responses = sum(1 for l in lengths if l > 5000)

    return {
        "message_count": len(messages),
        "avg_length": round(sum(lengths) / len(lengths)),
        "median_length": sorted(lengths)[len(lengths) // 2],
        "short_responses": short_responses,
        "long_responses": long_responses,
        "stop_reasons": dict(stop_reasons.most_common()),
    }


# ── Report generation ─────────────────────────────────────────────

def generate_report(session_id: str, events: list[dict]) -> dict:
    """Run all analyses and combine into a report."""
    return {
        "session_id": session_id,
        "event_count": len(events),
        "tool_errors": analyze_tool_errors(events),
        "retry_loops": detect_retry_loops(events),
        "context_growth": analyze_context_growth(events),
        "turn_productivity": analyze_turn_productivity(events),
        "startup_tax": analyze_startup_tax(events),
        "assistant_patterns": analyze_assistant_patterns(events),
    }


def format_report_text(report: dict) -> str:
    """Format report as human-readable text."""
    lines = []
    sid = report["session_id"]
    lines.append(f"═══ Deep Analysis: {sid} ═══")
    lines.append(f"Events: {report['event_count']}")

    # Tool errors
    te = report["tool_errors"]
    lines.append(f"\n── Tool Errors ──")
    lines.append(f"  {te['total_errors']}/{te['total_tool_calls']} calls failed ({te['error_rate']:.1%})")
    if te["errors_by_tool"]:
        lines.append("  by tool:")
        for tool, count in te["errors_by_tool"].items():
            lines.append(f"    {tool}: {count}")
            clusters = te["error_clusters"].get(tool, [])
            for c in clusters[:3]:
                lines.append(f"      └ {c['message'][:80]} (×{c['count']})")

    # Retry loops
    loops = report["retry_loops"]
    if loops:
        lines.append(f"\n── Retry Loops ({len(loops)}) ──")
        for loop in loops[:10]:
            dur = f"{loop['duration_ms']/1000:.0f}s" if loop["duration_ms"] else "?"
            lines.append(f"  {loop['tool']} ×{loop['length']} ({dur})")
    else:
        lines.append(f"\n── Retry Loops: none detected ──")

    # Context growth
    cg = report["context_growth"]
    lines.append("\n── Context Growth ──")
    if cg.get("has_usage_data"):
        lines.append(f"  {cg['first_prompt_tokens']:,} → {cg['last_prompt_tokens']:,} tokens ({cg['turn_count']} turns)")
        lines.append(f"  peak: {cg['peak_prompt_tokens']:,} | growth: ~{cg['avg_growth_per_turn']:,}/turn")
        lines.append(f"  cache hit ratio: {cg['cache_hit_ratio']:.1%}")
        if cg["likely_compactions"]:
            lines.append(f"  likely compactions: {len(cg['likely_compactions'])}")
            for d in cg["likely_compactions"][:5]:
                lines.append(
                    f"    turn {d['turn']}: {d['before']:,} → {d['after']:,} ({d['reduction_pct']}% drop)"
                )
    else:
        lines.append("  (no usage data available)")

    # Turn productivity
    tp = report["turn_productivity"]
    lines.append(f"\n── Turn Productivity ──")
    lines.append(
        f"  writes: {tp['write_calls']} | reads: {tp['read_calls']} | "
        f"errors: {tp['error_calls']} | other: {tp['other_calls']}"
    )
    lines.append(f"  write ratio: {tp['write_ratio']:.1%} | error ratio: {tp['error_ratio']:.1%}")
    if tp["empty_results"]:
        lines.append(f"  empty results: {tp['empty_results']}")

    # Startup tax
    st = report.get("startup_tax", {})
    if st.get("has_startup_data"):
        if st.get("startup_tax_detected"):
            lines.append("\n── Startup Tax (DETECTED) ──")
            lines.append(
                f"  first 10 calls: {st['first_10_errors']}/{st['first_10_calls']} errors "
                f"({st['startup_error_rate']:.0%} vs {st['overall_error_rate']:.0%} overall)"
            )
            if st.get("agent_memory_retries"):
                lines.append(f"  agent memory retries: {st['agent_memory_retries']}")
            for err in st.get("first_errors", [])[:5]:
                lines.append(f"    └ {err['tool']}: {err['error'][:70]}")
        elif st.get("first_10_errors", 0) > 0:
            lines.append(f"\n── Startup Tax: {st['first_10_errors']} early errors (below threshold) ──")

    # Assistant patterns
    ap = report["assistant_patterns"]
    lines.append(f"\n── Assistant Patterns ──")
    if ap["message_count"]:
        lines.append(
            f"  {ap['message_count']} messages | "
            f"avg {ap['avg_length']} chars | median {ap['median_length']} chars"
        )
        lines.append(f"  short (<50c): {ap['short_responses']} | long (>5000c): {ap['long_responses']}")
        if ap.get("stop_reasons"):
            lines.append(f"  stop reasons: {ap['stop_reasons']}")

    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(
        description="Deep analysis of a mu event log session"
    )
    parser.add_argument("input", help="Session JSONL file or directory")
    parser.add_argument(
        "--format",
        choices=["text", "json"],
        default="text",
    )
    parser.add_argument(
        "--report-dir",
        help="Write individual report files to this directory",
    )
    parser.add_argument(
        "--top",
        type=int,
        default=0,
        help="Analyze only top N files by size (0 = all)",
    )
    args = parser.parse_args()

    input_path = Path(args.input)
    if input_path.is_file():
        files = [input_path]
    elif input_path.is_dir():
        files = sorted(input_path.rglob("*.jsonl"))
    else:
        print(f"Error: {input_path} not found", file=sys.stderr)
        sys.exit(1)

    if args.top > 0:
        files = sorted(files, key=lambda f: f.stat().st_size, reverse=True)[
            : args.top
        ]

    report_dir = Path(args.report_dir) if args.report_dir else None
    if report_dir:
        report_dir.mkdir(parents=True, exist_ok=True)

    for f in files:
        events = load_events(f)
        if not events:
            continue

        session_id = f.stem
        report = generate_report(session_id, events)

        if args.format == "json":
            print(json.dumps(report, indent=2))
        else:
            print(format_report_text(report))
            print()

        if report_dir:
            out = report_dir / f"{session_id}.report.json"
            with open(out, "w") as fh:
                json.dump(report, fh, indent=2)


if __name__ == "__main__":
    main()
