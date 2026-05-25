#!/usr/bin/env python3
"""Convert claude-code JSONL session logs into mu event log format.

Usage:
    import-claude-history.py <input-dir-or-file> [--output-dir DIR] [--dry-run]

Input:  Claude-code JSONL files (one session per file).
Output: Mu-format JSONL files at <output-dir>/<daemon_id>/<session_id>.jsonl

The converter is lossy by design for v1 — it maps the conversational
core (user/assistant/tool_call/tool_result/done) faithfully and drops
or tags metadata events (permission-mode, bridge-session, etc.) as
audit_event. Image/attachment content is dropped with a placeholder.

See bead mu-ij7g and specs/architecture/exo-protocol-catalog.md for
the full source schema documentation.
"""

import argparse
import json
import os
import sys
from datetime import datetime
from pathlib import Path
from typing import Any


def iso_to_unix_ms(ts: str) -> int:
    """Parse ISO 8601 timestamp to unix milliseconds."""
    try:
        dt = datetime.fromisoformat(ts.replace("Z", "+00:00"))
        return int(dt.timestamp() * 1000)
    except (ValueError, AttributeError):
        return 0


def make_event(
    event_id: int,
    timestamp_unix_ms: int,
    actor: dict,
    payload: dict,
) -> dict:
    """Construct a mu-format event envelope."""
    return {
        "id": event_id,
        "timestamp_unix_ms": timestamp_unix_ms,
        "actor": actor,
        "payload": payload,
    }


def extract_tool_calls(content_blocks: list) -> list[dict]:
    """Extract tool_use blocks from an assistant message's content array."""
    calls = []
    for block in content_blocks:
        if block.get("type") == "tool_use":
            calls.append({
                "call_id": block.get("id", ""),
                "name": block.get("name", ""),
                "arguments": block.get("input", {}),
            })
    return calls


def extract_text(content_blocks: list) -> str:
    """Extract text content from content blocks."""
    parts = []
    for block in content_blocks:
        if block.get("type") == "text":
            parts.append(block.get("text", ""))
    return "\n".join(parts)


def extract_thinking(content_blocks: list) -> list[dict]:
    """Extract thinking blocks from content."""
    thinking = []
    for block in content_blocks:
        if block.get("type") == "thinking":
            thinking.append({
                "text": block.get("thinking", ""),
                "signature": block.get("signature", ""),
            })
        elif block.get("type") == "redacted_thinking":
            thinking.append({
                "text": "[redacted]",
                "data": block.get("data", ""),
            })
    return thinking


def convert_user_event(
    cc_event: dict, event_id: int
) -> list[dict]:
    """Convert a claude-code user event to mu events.

    A user event can be either a user message or a tool result,
    distinguished by the presence of sourceToolAssistantUUID.
    """
    ts = iso_to_unix_ms(cc_event.get("timestamp", ""))
    msg = cc_event.get("message", {})
    content = msg.get("content", "")
    events = []

    if "sourceToolAssistantUUID" in cc_event:
        # Tool result
        tool_result = cc_event.get("toolUseResult", "")
        if isinstance(content, list):
            # Extract tool_result blocks
            for block in content:
                if block.get("type") == "tool_result":
                    result_content = block.get("content", "")
                    if isinstance(result_content, list):
                        result_content = "\n".join(
                            b.get("text", "")
                            for b in result_content
                            if b.get("type") == "text"
                        )
                    events.append(make_event(
                        event_id,
                        ts,
                        {"Tool": {"name": "unknown"}},
                        {
                            "kind": "tool_result",
                            "call_id": block.get("tool_use_id", ""),
                            "content": str(result_content)[:65536],
                            "is_error": block.get("is_error", False),
                        },
                    ))
                    event_id += 1
        elif isinstance(tool_result, str):
            events.append(make_event(
                event_id,
                ts,
                {"Tool": {"name": "unknown"}},
                {
                    "kind": "tool_result",
                    "call_id": "",
                    "content": tool_result[:65536],
                    "is_error": False,
                },
            ))
            event_id += 1
    else:
        # User message
        if isinstance(content, list):
            text = extract_text(content)
        elif isinstance(content, str):
            text = content
        else:
            text = str(content)

        if text.strip():
            events.append(make_event(
                event_id,
                ts,
                {"User": None},
                {
                    "kind": "user_message",
                    "content": text,
                },
            ))
            event_id += 1

    return events


def convert_assistant_event(
    cc_event: dict, event_id: int
) -> list[dict]:
    """Convert a claude-code assistant event to mu events.

    Produces: one assistant_message_event for text, one tool_call per
    tool_use block, and optionally thinking events.
    """
    ts = iso_to_unix_ms(cc_event.get("timestamp", ""))
    msg = cc_event.get("message", {})
    content = msg.get("content", [])
    events = []

    if not isinstance(content, list):
        content = [{"type": "text", "text": str(content)}]

    # Extract components
    text = extract_text(content)
    tool_calls = extract_tool_calls(content)
    thinking = extract_thinking(content)

    # Model info from the message envelope
    model = msg.get("model", cc_event.get("model", "unknown"))
    stop_reason = msg.get("stop_reason", "end_turn")

    # Usage from the message
    usage = msg.get("usage", {})
    mu_usage = None
    if usage:
        mu_usage = {
            "input_tokens": usage.get("input_tokens", 0),
            "output_tokens": usage.get("output_tokens", 0),
            "cache_creation_input_tokens": usage.get(
                "cache_creation_input_tokens", 0
            ),
            "cache_read_input_tokens": usage.get(
                "cache_read_input_tokens", 0
            ),
        }

    # Emit thinking as audit events
    for t in thinking:
        events.append(make_event(
            event_id,
            ts,
            {"Agent": None},
            {
                "kind": "audit_event",
                "subtype": "thinking",
                "body": t,
            },
        ))
        event_id += 1

    # Emit tool calls
    for tc in tool_calls:
        events.append(make_event(
            event_id,
            ts,
            {"Agent": None},
            {
                "kind": "tool_call",
                "call_id": tc["call_id"],
                "name": tc["name"],
                "arguments": tc["arguments"],
            },
        ))
        event_id += 1

    # Emit assistant message (text portion)
    if text.strip() or (not tool_calls and not thinking):
        events.append(make_event(
            event_id,
            ts,
            {"Agent": None},
            {
                "kind": "assistant_message_event",
                "message": {
                    "content": text,
                    "stop_reason": stop_reason,
                    "model": model,
                    "usage": mu_usage,
                },
            },
        ))
        event_id += 1

    return events


def convert_metadata_event(
    cc_event: dict, event_id: int
) -> list[dict]:
    """Convert metadata events (system, attachment, etc.) to audit_event."""
    ts = iso_to_unix_ms(cc_event.get("timestamp", ""))
    cc_type = cc_event.get("type", "unknown")

    # Extract meaningful content based on type
    body: dict[str, Any] = {"source_type": cc_type}

    if cc_type == "system":
        body["subtype"] = cc_event.get("subtype", "")
        body["content"] = cc_event.get("content", "")[:4096]
    elif cc_type == "attachment":
        att = cc_event.get("attachment", {})
        body["attachment_type"] = att.get("type", "")
        body["content"] = str(att.get("content", ""))[:4096]
        if "hookName" in att:
            body["hook_name"] = att["hookName"]
            body["hook_event"] = att.get("hookEvent", "")
    elif cc_type == "permission-mode":
        body["permission_mode"] = cc_event.get("permissionMode", "")
    elif cc_type == "ai-title":
        body["title"] = cc_event.get("aiTitle", "")

    return [make_event(
        event_id,
        ts,
        {"System": None},
        {
            "kind": "audit_event",
            "subtype": cc_type,
            "body": body,
        },
    )]


def convert_session(cc_events: list[dict], session_id: str) -> list[dict]:
    """Convert a full claude-code session to mu event log format."""
    mu_events = []
    event_id = 1

    # Find session metadata from the first envelope event
    first_envelope = next(
        (e for e in cc_events if e.get("type") in ("user", "assistant")),
        {},
    )
    provider = "anthropic_api"
    model = "unknown"
    if first_envelope:
        msg = first_envelope.get("message", {})
        model = msg.get("model", "claude-opus-4-7")
    cwd = first_envelope.get("cwd", "")
    version = first_envelope.get("version", "")
    entrypoint = first_envelope.get("entrypoint", "cli")

    # Emit SessionCreated
    ts = iso_to_unix_ms(first_envelope.get("timestamp", "")) if first_envelope else 0
    mu_events.append(make_event(
        event_id,
        ts,
        {"System": None},
        {
            "kind": "session_created",
            "provider_kind": provider,
            "model": model,
        },
    ))
    event_id += 1

    # Emit a callout with import metadata
    mu_events.append(make_event(
        event_id,
        ts,
        {"System": None},
        {
            "kind": "callout",
            "category": "import",
            "title": "Imported from claude-code",
            "body": {
                "source": "claude-code",
                "version": version,
                "entrypoint": entrypoint,
                "cwd": cwd,
                "original_session_id": session_id,
                "event_count": len(cc_events),
            },
            "theme": None,
            "context_refs": [],
        },
    ))
    event_id += 1

    # Convert each event
    for cc_event in cc_events:
        cc_type = cc_event.get("type", "")

        if cc_type == "user":
            new_events = convert_user_event(cc_event, event_id)
        elif cc_type == "assistant":
            new_events = convert_assistant_event(cc_event, event_id)
        elif cc_type in (
            "system", "attachment", "permission-mode",
            "ai-title", "queue-operation",
        ):
            new_events = convert_metadata_event(cc_event, event_id)
        else:
            # Skip: last-prompt, file-history-snapshot, bridge-session,
            # agent-name, worktree-state, agent-color, pr-link
            continue

        mu_events.extend(new_events)
        event_id += len(new_events)

    # Emit SessionClosed at the end
    last_ts = mu_events[-1]["timestamp_unix_ms"] if mu_events else 0
    mu_events.append(make_event(
        event_id,
        last_ts,
        {"System": None},
        {"kind": "session_closed"},
    ))

    return mu_events


def process_file(
    input_path: Path,
    output_dir: Path,
    dry_run: bool = False,
) -> dict:
    """Process one claude-code JSONL file."""
    session_id = input_path.stem
    cc_events = []

    with open(input_path) as f:
        for line in f:
            line = line.strip()
            if line:
                try:
                    cc_events.append(json.loads(line))
                except json.JSONDecodeError:
                    continue

    if not cc_events:
        return {"session_id": session_id, "status": "empty", "events": 0}

    mu_events = convert_session(cc_events, session_id)

    # Output path: imported-claude/<session_id>.jsonl
    daemon_id = "imported-claude"
    out_dir = output_dir / daemon_id
    out_file = out_dir / f"{session_id}.jsonl"

    stats = {
        "session_id": session_id,
        "status": "converted",
        "input_events": len(cc_events),
        "output_events": len(mu_events),
        "output_file": str(out_file),
    }

    if not dry_run:
        out_dir.mkdir(parents=True, exist_ok=True)
        with open(out_file, "w") as f:
            for ev in mu_events:
                f.write(json.dumps(ev, separators=(",", ":")) + "\n")

    return stats


def main():
    parser = argparse.ArgumentParser(
        description="Convert claude-code JSONL sessions to mu event log format"
    )
    parser.add_argument(
        "input",
        help="Input directory containing JSONL files, or a single JSONL file",
    )
    parser.add_argument(
        "--output-dir",
        default=os.path.expanduser("~/.local/share/mu/events"),
        help="Output directory for mu event logs (default: ~/.local/share/mu/events)",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Parse and convert but don't write output files",
    )
    args = parser.parse_args()

    input_path = Path(args.input)
    output_dir = Path(args.output_dir)

    if input_path.is_file():
        files = [input_path]
    elif input_path.is_dir():
        files = sorted(input_path.glob("*.jsonl"))
    else:
        print(f"Error: {input_path} not found", file=sys.stderr)
        sys.exit(1)

    print(f"Converting {len(files)} session(s)...")
    if args.dry_run:
        print("(dry run — no files will be written)")

    total_in = 0
    total_out = 0
    errors = 0

    for f in files:
        try:
            stats = process_file(f, output_dir, args.dry_run)
            total_in += stats.get("input_events", 0)
            total_out += stats.get("output_events", 0)
            status = stats["status"]
            if status == "empty":
                print(f"  SKIP {f.name} (empty)")
            else:
                print(
                    f"  OK   {f.name}: "
                    f"{stats['input_events']} → {stats['output_events']} events"
                )
        except Exception as e:
            print(f"  ERR  {f.name}: {e}", file=sys.stderr)
            errors += 1

    print(f"\nDone: {len(files)} files, {total_in} → {total_out} events, {errors} errors")


if __name__ == "__main__":
    main()
