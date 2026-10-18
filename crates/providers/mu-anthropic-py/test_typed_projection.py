#!/usr/bin/env python3
"""Tests for `response_message_to_py` — the typed-value → native-Python-object
projection on the mu_anthropic_py binding.

This is the Python-side layer (the Rust seam is thin one-to-one delegation and
is untested by design — see AGENTS.md rule 9). Everything here is synthetic
wire JSON: NO API calls, NO real model ids, NO corpus. The three properties the
projection must guarantee:

  1. nested tool content + usage metadata survive as NATIVE Python values
     (dicts/lists/ints), not a re-serialized string the caller must json.loads;
  2. a KNOWN block with broken fields (or any contract violation) raises
     ValueError — loud, so a consumer can fall back per-message and surface drift;
  3. an UNKNOWN block type is preserved verbatim inside the projected content,
     never dropped and never an error.

Run after `maturin develop` (or with the built .so on sys.path):
    python3 test_typed_projection.py      # plain asserts, exit 0 = pass
    pytest test_typed_projection.py       # also pytest-discoverable
"""

import json

import mu_anthropic_py as ma  # ty: ignore[unresolved-import]

# A neutral placeholder model id — this is test data, not a dispatch target, so
# it deliberately is NOT a real model name.
MODEL = "test-model"


def _nested_tool_message():
    """A response carrying the shapes analytics cares about: a tool_use with a
    deeply nested `input`, a server-tool result whose `content` is a nested
    array, and a full `usage` block with the per-tier cache split."""
    return {
        "id": "msg_test_0001",
        "type": "message",
        "role": "assistant",
        "model": MODEL,
        "content": [
            {"type": "text", "text": "running the tool"},
            {
                "type": "tool_use",
                "id": "toolu_abc",
                "name": "calculator",
                "input": {
                    "operation": "add",
                    "operands": [1, 2, 3],
                    "opts": {"round": True, "precision": 4},
                },
            },
            {
                "type": "web_search_tool_result",
                "tool_use_id": "srvtoolu_1",
                "content": [
                    {"type": "web_search_result", "title": "T", "url": "https://x",
                     "encrypted_content": "ENC", "page_age": None}
                ],
                "caller": {"type": "direct"},
            },
        ],
        "stop_reason": "tool_use",
        "stop_details": {"category": None, "some_future_key": 7},
        "usage": {
            "input_tokens": 21,
            "output_tokens": 305,
            "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 248,
            "cache_creation": {
                "ephemeral_5m_input_tokens": 148,
                "ephemeral_1h_input_tokens": 100,
            },
            "server_tool_use": {"web_search_requests": 3, "web_fetch_requests": 1},
        },
    }


def test_nested_tool_content_and_metadata_fidelity():
    obj = ma.response_message_to_py(json.dumps(_nested_tool_message()))

    # Native object graph, not a string we have to re-parse.
    assert isinstance(obj, dict), type(obj)
    assert obj["id"] == "msg_test_0001"

    # usage metadata: real ints at every depth, incl. the per-tier cache split.
    usage = obj["usage"]
    assert isinstance(usage, dict)
    assert usage["output_tokens"] == 305 and isinstance(usage["output_tokens"], int)
    assert usage["input_tokens"] == 21
    assert usage["cache_creation"]["ephemeral_1h_input_tokens"] == 100
    # a known server-tool counter is typed; an unmodeled one rides through.
    assert usage["server_tool_use"]["web_search_requests"] == 3
    assert usage["server_tool_use"]["web_fetch_requests"] == 1

    # arbitrary stop_details keys we don't model survive as native values.
    assert obj["stop_details"]["some_future_key"] == 7

    # nested tool_use.input is a real dict/list, projected not stringified.
    blocks = obj["content"]
    assert isinstance(blocks, list) and len(blocks) == 3
    tool_use = blocks[1]
    assert tool_use["type"] == "tool_use" and tool_use["name"] == "calculator"
    inp = tool_use["input"]
    assert isinstance(inp, dict)
    assert inp["operation"] == "add"
    assert inp["operands"] == [1, 2, 3] and isinstance(inp["operands"], list)
    assert inp["opts"]["round"] is True and inp["opts"]["precision"] == 4

    # server-tool result: nested content array preserved verbatim, plus its
    # extra `caller` field.
    str_block = blocks[2]
    assert str_block["type"] == "web_search_tool_result"
    assert isinstance(str_block["content"], list)
    assert str_block["content"][0]["title"] == "T"
    assert str_block["content"][0]["page_age"] is None
    assert str_block["caller"] == {"type": "direct"}


def test_known_malformed_message_raises():
    # A KNOWN block type (tool_use) missing required `name`/`input` is wire
    # breakage and must be loud — not silently dropped to an Unknown block.
    broken_block = {
        "id": "msg_bad", "type": "message", "role": "assistant", "model": MODEL,
        "content": [{"type": "tool_use", "id": "toolu_x"}],
    }
    try:
        ma.response_message_to_py(json.dumps(broken_block))
        raise AssertionError("expected ValueError for a malformed known block")
    except ValueError:
        pass

    # A message missing a required top-level field (`id`) is also a hard error.
    missing_id = {
        "type": "message", "role": "assistant", "model": MODEL, "content": [],
    }
    try:
        ma.response_message_to_py(json.dumps(missing_id))
        raise AssertionError("expected ValueError for a missing required field")
    except ValueError:
        pass


def test_unknown_block_preserved_verbatim():
    # A content block whose `type` the crate does not model must NOT error and
    # must come back verbatim, so a consumer can detect a new block type from
    # the parsed data (the other half of the drift contract).
    unknown_block = {
        "type": "search_result",
        "source": "https://x",
        "title": "t",
        "content": [{"type": "text", "text": "snippet"}],
        "novel_field": {"k": [1, 2]},
    }
    msg = {
        "id": "msg_unknown", "type": "message", "role": "assistant", "model": MODEL,
        "content": [{"type": "text", "text": "hi"}, unknown_block],
    }
    obj = ma.response_message_to_py(json.dumps(msg))
    blocks = obj["content"]
    assert len(blocks) == 2
    # preserved byte-for-byte as its original object.
    assert blocks[1] == unknown_block


def main():
    tests = [
        test_nested_tool_content_and_metadata_fidelity,
        test_known_malformed_message_raises,
        test_unknown_block_preserved_verbatim,
    ]
    for t in tests:
        t()
        print(f"  ok: {t.__name__}")
    print(f"OK — {len(tests)} projection tests passed.")


if __name__ == "__main__":
    main()
