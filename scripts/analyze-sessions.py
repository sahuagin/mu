#!/usr/bin/env python3
"""Analyze converted mu event logs for session behavior patterns.

Usage:
    analyze-sessions.py <dir-or-file> [--format json|text] [--top N]

Reads mu-format JSONL event logs (native or imported from claude-code)
and produces a session-level profile: turn count, tool usage, context
growth, thinking ratio, error patterns, and duration.

Can process both:
- Native mu sessions (from ~/.local/share/mu/events/<daemon>/)
- Imported claude-code sessions (from import-claude-history.py output)
"""

import argparse
import json
import sys
from collections import Counter
from pathlib import Path


def analyze_session(events: list[dict]) -> dict:
    """Profile one session from its event list."""
    profile = {
        "event_count": len(events),
        "user_messages": 0,
        "assistant_messages": 0,
        "tool_calls": 0,
        "tool_results": 0,
        "tool_errors": 0,
        "audit_events": 0,
        "thinking_blocks": 0,
        "compactions": 0,
        "errors": 0,
        "tools_used": Counter(),
        "audit_subtypes": Counter(),
        "first_ts": None,
        "last_ts": None,
        "duration_minutes": 0,
        "model": "unknown",
        "provider": "unknown",
        "source": "unknown",
        "cwd": "",
        "total_input_tokens": 0,
        "total_output_tokens": 0,
        "total_cache_read_tokens": 0,
        "total_cache_creation_tokens": 0,
    }

    for ev in events:
        ts = ev.get("timestamp_unix_ms", 0)
        if ts > 0:
            if profile["first_ts"] is None or ts < profile["first_ts"]:
                profile["first_ts"] = ts
            if profile["last_ts"] is None or ts > profile["last_ts"]:
                profile["last_ts"] = ts

        payload = ev.get("payload", {})
        kind = payload.get("kind", "")

        if kind == "session_created":
            profile["model"] = payload.get("model", "unknown")
            profile["provider"] = payload.get("provider_kind", "unknown")

        elif kind == "user_message":
            profile["user_messages"] += 1

        elif kind == "assistant_message_event":
            profile["assistant_messages"] += 1
            msg = payload.get("message", {})
            usage = msg.get("usage")
            if usage:
                profile["total_input_tokens"] += usage.get("input_tokens", 0)
                profile["total_output_tokens"] += usage.get("output_tokens", 0)
                profile["total_cache_read_tokens"] += usage.get(
                    "cache_read_input_tokens", 0
                )
                profile["total_cache_creation_tokens"] += usage.get(
                    "cache_creation_input_tokens", 0
                )

        elif kind == "tool_call":
            profile["tool_calls"] += 1
            profile["tools_used"][payload.get("name", "?")] += 1

        elif kind == "tool_result":
            profile["tool_results"] += 1
            if payload.get("is_error", False):
                profile["tool_errors"] += 1

        elif kind == "audit_event":
            profile["audit_events"] += 1
            subtype = payload.get("subtype", "")
            profile["audit_subtypes"][subtype] += 1
            if subtype == "thinking":
                profile["thinking_blocks"] += 1

        elif kind == "error":
            profile["errors"] += 1

        elif kind == "callout":
            if payload.get("category") == "import":
                body = payload.get("body", {})
                profile["source"] = body.get("source", "mu")
                profile["cwd"] = body.get("cwd", "")

    if profile["first_ts"] and profile["last_ts"]:
        profile["duration_minutes"] = round(
            (profile["last_ts"] - profile["first_ts"]) / 60000, 1
        )

    # Convert Counters to dicts for JSON serialization
    profile["tools_used"] = dict(profile["tools_used"].most_common())
    profile["audit_subtypes"] = dict(profile["audit_subtypes"].most_common())

    return profile


def classify_session(profile: dict) -> list[str]:
    """Tag a session with behavioral categories."""
    tags = []

    if profile["tool_calls"] == 0 and profile["user_messages"] > 0:
        tags.append("conversation-only")
    if profile["tool_calls"] > 50:
        tags.append("heavy-tool-use")
    if profile["tool_errors"] > 3:
        tags.append("error-prone")
    if profile["thinking_blocks"] > 10:
        tags.append("heavy-thinking")
    if profile["compactions"] > 0:
        tags.append("compacted")
    if profile["duration_minutes"] > 120:
        tags.append("long-session")
    if profile["duration_minutes"] < 5 and profile["user_messages"] > 0:
        tags.append("short-session")
    if profile["errors"] > 0:
        tags.append("had-errors")
    # NOTE (mu-3b5c): total_input_tokens is a session-cumulative
    # BILLING aggregate (sum across every model call), not a context
    # size — a 12-turn codex session re-sends its prompt every call,
    # so this crosses 500k while actual context sits under 60k. The
    # tag means "expensive", not "big context"; per-call context
    # lives in deep-analyze.py's context-growth section.
    if profile["total_input_tokens"] > 500000:
        tags.append("high-cumulative-input")
    if profile["user_messages"] == 0:
        tags.append("no-user-input")

    total_tool_count = profile["tool_calls"]
    if total_tool_count > 0:
        error_rate = profile["tool_errors"] / total_tool_count
        if error_rate > 0.2:
            tags.append("high-error-rate")

    return tags


def format_text(session_id: str, profile: dict, tags: list[str]) -> str:
    """Format one session's profile as human-readable text."""
    lines = []
    lines.append(f"═══ {session_id} ═══")
    lines.append(f"  source: {profile['source']} | model: {profile['model']}")
    lines.append(
        f"  duration: {profile['duration_minutes']}min | "
        f"events: {profile['event_count']}"
    )
    lines.append(
        f"  turns: {profile['user_messages']}u / "
        f"{profile['assistant_messages']}a / "
        f"{profile['tool_calls']}tc / "
        f"{profile['tool_results']}tr"
    )
    if profile["tool_errors"]:
        lines.append(f"  tool errors: {profile['tool_errors']}")
    if profile["total_input_tokens"]:
        lines.append(
            f"  tokens: {profile['total_input_tokens']:,} in / "
            f"{profile['total_output_tokens']:,} out / "
            f"{profile['total_cache_read_tokens']:,} cache-read"
        )
    if profile["thinking_blocks"]:
        lines.append(f"  thinking blocks: {profile['thinking_blocks']}")
    if profile["tools_used"]:
        top_tools = list(profile["tools_used"].items())[:5]
        lines.append(
            f"  top tools: "
            + ", ".join(f"{n}({c})" for n, c in top_tools)
        )
    if tags:
        lines.append(f"  tags: {', '.join(tags)}")
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(
        description="Analyze mu event logs for session behavior patterns"
    )
    parser.add_argument("input", help="Directory or single JSONL file")
    parser.add_argument(
        "--format",
        choices=["text", "json"],
        default="text",
        help="Output format (default: text)",
    )
    parser.add_argument(
        "--top",
        type=int,
        default=0,
        help="Show only top N sessions by duration (0 = all)",
    )
    parser.add_argument(
        "--tag",
        help="Filter to sessions with this tag",
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

    results = []
    tag_counts = Counter()

    for f in files:
        events = []
        with open(f) as fh:
            for line in fh:
                line = line.strip()
                if line:
                    try:
                        events.append(json.loads(line))
                    except json.JSONDecodeError:
                        continue

        if not events:
            continue

        session_id = f.stem
        profile = analyze_session(events)
        tags = classify_session(profile)
        for t in tags:
            tag_counts[t] += 1

        if args.tag and args.tag not in tags:
            continue

        results.append((session_id, profile, tags))

    # Sort by duration descending
    results.sort(key=lambda x: x[1]["duration_minutes"], reverse=True)

    if args.top > 0:
        results = results[: args.top]

    if args.format == "json":
        output = []
        for sid, profile, tags in results:
            output.append(
                {"session_id": sid, "profile": profile, "tags": tags}
            )
        print(json.dumps(output, indent=2))
    else:
        for sid, profile, tags in results:
            print(format_text(sid, profile, tags))
            print()

        # Summary
        print(f"═══ Summary ═══")
        print(f"  sessions: {len(results)}")
        if tag_counts:
            print(f"  tag distribution:")
            for tag, count in tag_counts.most_common():
                filtered = " ←" if args.tag == tag else ""
                print(f"    {tag}: {count}{filtered}")


if __name__ == "__main__":
    main()
