use super::*;
use mu_core::agent::ToolCall;
use mu_core::context::{
    assemble_rope, CacheMarker, CacheStrategy, ProjectionTarget, ProviderRenderer, ProviderRole,
    SpanKind,
};

#[test]
fn b1_translate_user_message() {
    let m = AgentMessage::User {
        content: "hi".into(),
    };
    let v = translate_message_single(&m).expect("translates");
    assert_eq!(v["role"], "user");
    assert_eq!(v["content"], "hi");
}

#[test]
fn b2_translate_assistant_message() {
    let m = AgentMessage::Assistant(AssistantMessage {
        content: vec![ContentBlock::Text { text: "hi".into() }],
        stop_reason: StopReason::EndTurn,
        usage: None,
    });
    let v = translate_message_single(&m).expect("translates");
    assert_eq!(v["role"], "assistant");
    assert_eq!(v["content"][0]["type"], "text");
    assert_eq!(v["content"][0]["text"], "hi");
}

#[test]
fn translate_message_single_skips_tool_result() {
    let m = AgentMessage::ToolResult {
        call_id: "x".into(),
        content: "out".into(),
        is_error: false,
    };
    assert!(translate_message_single(&m).is_none());
}

#[test]
fn build_request_body_basics() {
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let body = build_request_body("claude-test", None, &messages, &[]);
    assert_eq!(body["model"], "claude-test");
    assert_eq!(body["stream"], true);
    // Unknown model name falls back to the conservative 4096.
    assert_eq!(body["max_tokens"], 4096);
    assert_eq!(body["messages"][0]["role"], "user");
}

#[test]
fn build_request_body_max_tokens_is_model_aware() {
    // mu-ql2: real-model identifiers get their per-family ceiling so
    // longer responses don't get prematurely truncated.
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let opus = build_request_body("claude-opus-4-7", None, &messages, &[]);
    assert_eq!(opus["max_tokens"], 16384);
    let haiku = build_request_body("claude-haiku-4-5", None, &messages, &[]);
    assert_eq!(haiku["max_tokens"], 8192);
}

#[test]
fn b1_translate_tool_spec_shape() {
    let spec = ToolSpec {
        name: "read".into(),
        description: "Read a file".into(),
        input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        policy: Default::default(),
    };
    assert_eq!(
        translate_tool_spec(&spec),
        json!({
            "name":"read",
            "description":"Read a file",
            "input_schema":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}
        })
    );
}

#[test]
fn b2_translate_messages_preserves_order() {
    let messages = vec![
        AgentMessage::User {
            content: "first".into(),
        },
        assistant_text("second"),
        AgentMessage::User {
            content: "third".into(),
        },
        assistant_text("fourth"),
    ];
    let translated = translate_messages(&messages);
    assert_eq!(translated.len(), 4);
    assert_eq!(translated[0]["role"], "user");
    assert_eq!(translated[0]["content"], "first");
    assert_eq!(translated[1]["role"], "assistant");
    assert_eq!(translated[1]["content"][0]["text"], "second");
    assert_eq!(translated[2]["role"], "user");
    assert_eq!(translated[2]["content"], "third");
    assert_eq!(translated[3]["role"], "assistant");
    assert_eq!(translated[3]["content"][0]["text"], "fourth");
}

#[test]
fn b3_consecutive_tool_results_group_into_one_user_message() {
    let messages = vec![
        AgentMessage::User {
            content: "read both".into(),
        },
        AgentMessage::Assistant(AssistantMessage {
            content: vec![tool_call("toolu_a", "a.txt"), tool_call("toolu_b", "b.txt")],
            stop_reason: StopReason::ToolUse,
            usage: None,
        }),
        AgentMessage::ToolResult {
            call_id: "toolu_a".into(),
            content: "a contents".into(),
            is_error: false,
        },
        AgentMessage::ToolResult {
            call_id: "toolu_b".into(),
            content: "b failed".into(),
            is_error: true,
        },
        assistant_text("done"),
    ];

    let translated = translate_messages(&messages);
    assert_eq!(translated.len(), 4);
    assert_eq!(translated[0]["role"], "user");
    assert_eq!(translated[1]["role"], "assistant");
    assert_eq!(translated[1]["content"].as_array().map(Vec::len), Some(2));
    assert_eq!(translated[1]["content"][0]["type"], "tool_use");
    assert_eq!(translated[1]["content"][0]["id"], "toolu_a");
    assert_eq!(
        translated[1]["content"][0]["input"],
        json!({ "path": "a.txt" })
    );
    assert_eq!(translated[1]["content"][1]["type"], "tool_use");
    assert_eq!(translated[1]["content"][1]["id"], "toolu_b");
    assert_eq!(translated[2]["role"], "user");
    let tool_results = translated[2]["content"].as_array();
    assert_eq!(tool_results.map(Vec::len), Some(2));
    assert_eq!(translated[2]["content"][0]["type"], "tool_result");
    assert_eq!(translated[2]["content"][0]["tool_use_id"], "toolu_a");
    assert_eq!(translated[2]["content"][0]["content"], "a contents");
    assert_eq!(translated[2]["content"][0]["is_error"], false);
    assert_eq!(translated[2]["content"][1]["type"], "tool_result");
    assert_eq!(translated[2]["content"][1]["tool_use_id"], "toolu_b");
    assert_eq!(translated[2]["content"][1]["content"], "b failed");
    assert_eq!(translated[2]["content"][1]["is_error"], true);
    assert_eq!(translated[3]["role"], "assistant");
}

#[test]
fn b4_build_request_body_includes_tools_when_present() {
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let tools = vec![ToolSpec {
        name: "read".into(),
        description: "Read a file".into(),
        input_schema: json!({ "type": "object" }),
        policy: Default::default(),
    }];
    let body = build_request_body("claude-test", None, &messages, &tools);
    assert_eq!(body["messages"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["tools"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["tools"][0]["name"], "read");
}

#[test]
fn b5_build_request_body_omits_tools_when_empty() {
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let body = build_request_body("claude-test", None, &messages, &[]);
    assert!(body.get("tools").is_none());
    assert_eq!(body["messages"].as_array().map(Vec::len), Some(1));
}

// mu-i6j: tool definitions are stable across asks within a
// session, so they're a high-value cache target. We mark the last
// tool with cache_control: ephemeral, which tells Anthropic to
// cache everything up to and including that marker (i.e. the
// entire tools array).

#[test]
fn mu_i6j_single_tool_gets_cache_control_marker() {
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let tools = vec![ToolSpec {
        name: "read".into(),
        description: "Read a file".into(),
        input_schema: json!({ "type": "object" }),
        policy: Default::default(),
    }];
    let body = build_request_body("claude-test", None, &messages, &tools);
    let tool = &body["tools"][0];
    assert_eq!(tool["name"], "read");
    assert_eq!(
        tool["cache_control"],
        json!({ "type": "ephemeral" }),
        "single tool should carry the cache_control marker"
    );
}

// mu-n48: system prompt rendered as content-block array with
// cache_control: ephemeral. Mirrors the tool-cache pattern so
// the system block also becomes part of the cacheable prefix.

#[test]
fn mu_n48_system_prompt_none_omits_system_field() {
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let body = build_request_body("claude-test", None, &messages, &[]);
    assert!(
        body.get("system").is_none(),
        "no system field when system_prompt is None"
    );
}

#[test]
fn mu_n48_system_prompt_empty_omits_system_field() {
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let body = build_request_body("claude-test", Some(""), &messages, &[]);
    assert!(
        body.get("system").is_none(),
        "no system field when system_prompt is empty"
    );
}

#[test]
fn mu_n48_system_prompt_set_emits_content_block_with_cache_control() {
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let body = build_request_body("claude-test", Some("you are concise"), &messages, &[]);
    let arr = body["system"].as_array().expect("system is array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["type"], "text");
    assert_eq!(arr[0]["text"], "you are concise");
    assert_eq!(arr[0]["cache_control"], json!({ "type": "ephemeral" }));
}

#[test]
fn mu_i6j_only_last_tool_gets_cache_control_marker() {
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let tools = vec![
        ToolSpec {
            name: "read".into(),
            description: "Read a file".into(),
            input_schema: json!({ "type": "object" }),
            policy: Default::default(),
        },
        ToolSpec {
            name: "glob".into(),
            description: "Find files".into(),
            input_schema: json!({ "type": "object" }),
            policy: Default::default(),
        },
        ToolSpec {
            name: "grep".into(),
            description: "Search contents".into(),
            input_schema: json!({ "type": "object" }),
            policy: Default::default(),
        },
    ];
    let body = build_request_body("claude-test", None, &messages, &tools);
    let tool_arr = body["tools"].as_array().expect("tools array");
    assert_eq!(tool_arr.len(), 3);
    // Anthropic caches everything UP TO AND INCLUDING the marker,
    // so the marker on the final tool is sufficient — earlier
    // tools must NOT carry their own markers (Anthropic allows up
    // to 4 markers per request, but more is wasteful here).
    assert!(
        tool_arr[0].get("cache_control").is_none(),
        "first tool should not carry cache_control"
    );
    assert!(
        tool_arr[1].get("cache_control").is_none(),
        "middle tool should not carry cache_control"
    );
    assert_eq!(
        tool_arr[2]["cache_control"],
        json!({ "type": "ephemeral" }),
        "last tool should carry the cache_control marker"
    );
    // Sanity: tool order is preserved.
    assert_eq!(tool_arr[0]["name"], "read");
    assert_eq!(tool_arr[1]["name"], "glob");
    assert_eq!(tool_arr[2]["name"], "grep");
}

fn assistant_text(text: &str) -> AgentMessage {
    AgentMessage::Assistant(AssistantMessage {
        content: vec![ContentBlock::Text { text: text.into() }],
        stop_reason: StopReason::EndTurn,
        usage: None,
    })
}

fn tool_call(id: &str, path: &str) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: "read".into(),
        arguments: json!({ "path": path }),
    })
}

#[tokio::test]
async fn b4_sse_to_provider_events() {
    // Build a fake SSE byte stream that mimics Anthropic's shape.
    let raw = concat!(
        r#"event: message_start"#,
        "\n",
        r#"data: {"type":"message_start","message":{"id":"m_1","role":"assistant"}}"#,
        "\n\n",
        r#"event: content_block_start"#,
        "\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "\n\n",
        r#"event: content_block_delta"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#,
        "\n\n",
        r#"event: content_block_delta"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}"#,
        "\n\n",
        r#"event: content_block_stop"#,
        "\n",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "\n\n",
        r#"event: message_delta"#,
        "\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        "\n\n",
        r#"event: message_stop"#,
        "\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n",
    );
    let bytes = futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::copy_from_slice(
        raw.as_bytes(),
    ))]);
    // events_stream takes Stream<Item = reqwest::Result<Bytes>>;
    // we adapt by mapping our io::Error to reqwest's. Since we
    // don't have access to a reqwest::Error constructor, build a
    // separate adapter for tests.
    let bytes = bytes.map(|r| r.map_err(|_| panic!("test stream errored")));
    // Wrap so the stream type matches what events_stream expects
    // (reqwest::Result<Bytes>). The simplest path: change
    // events_stream to be generic over any Stream<Item =
    // Result<Bytes, _>>, so tests can use io::Error. Refactor
    // below in test_events_stream.
    let (_tx, rx) = tokio::sync::oneshot::channel();
    let mut stream = test_events_stream(bytes, rx);

    let mut events = Vec::new();
    while let Some(e) = stream.next().await {
        events.push(e);
    }

    // Expected: TextDelta("hello"), TextDelta(" world"),
    // Done(AssistantMessage { content: [Text("hello world")], EndTurn })
    assert_eq!(events.len(), 3);
    match &events[0] {
        ProviderEvent::TextDelta(t) => assert_eq!(t, "hello"),
        other => panic!("expected TextDelta, got {other:?}"),
    }
    match &events[1] {
        ProviderEvent::TextDelta(t) => assert_eq!(t, " world"),
        other => panic!("expected TextDelta, got {other:?}"),
    }
    match &events[2] {
        ProviderEvent::Done(msg) => {
            assert_eq!(msg.stop_reason, StopReason::EndTurn);
            match &msg.content[0] {
                ContentBlock::Text { text } => assert_eq!(text, "hello world"),
                other => panic!("expected Text block, got {other:?}"),
            }
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

/// Test-only variant of events_stream that accepts a stream with
/// any Result error type, not specifically reqwest::Result.
fn test_events_stream(
    bytes: impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
    cancel_rx: oneshot::Receiver<()>,
) -> BoxStream<'static, ProviderEvent> {
    let bytes: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> = Box::pin(bytes);
    let sse = SseStream::new(bytes);
    let state = StreamState {
        sse: Box::pin(sse),
        blocks: HashMap::new(),
        block_order: Vec::new(),
        stop_reason: None,
        usage: AnthropicUsage::default(),
        cancel_rx: Some(cancel_rx),
        finished: false,
        emitted_done: false,
    };
    Box::pin(futures::stream::unfold(state, next_event))
}

#[tokio::test]
async fn anthropic_error_event_terminates_with_provider_error() {
    let raw = concat!(
        r#"event: error"#,
        "\n",
        r#"data: {"type":"error","error":{"type":"rate_limit_error","message":"too many"}}"#,
        "\n\n",
    );
    let bytes = futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::copy_from_slice(
        raw.as_bytes(),
    ))]);
    let (_tx, rx) = tokio::sync::oneshot::channel();
    let mut stream = test_events_stream(bytes, rx);
    let event = stream.next().await.expect("expected error event");
    match event {
        ProviderEvent::Error(msg) => {
            assert!(msg.contains("rate_limit_error"));
            assert!(msg.contains("too many"));
        }
        other => panic!("expected Error, got {other:?}"),
    }
    // No more events.
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn eof_without_message_stop_emits_degraded_eof() {
    // Simulate a stream that ends mid-response without a terminal message_stop
    // event (connection drop, upstream truncation, etc.). The provider should emit
    // Done with stop_reason: DegradedEof to signal the degraded condition.
    let raw = concat!(
        r#"event: message_start"#,
        "\n",
        r#"data: {"type":"message_start","message":{"id":"m_1","role":"assistant"}}"#,
        "\n\n",
        r#"event: content_block_start"#,
        "\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "\n\n",
        r#"event: content_block_delta"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}"#,
        "\n\n",
        // NOTE: NO message_stop event. Stream ends here.
    );
    let bytes = futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::copy_from_slice(
        raw.as_bytes(),
    ))]);
    let (_tx, rx) = tokio::sync::oneshot::channel();
    let mut stream = test_events_stream(bytes, rx);

    let mut events = Vec::new();
    while let Some(e) = stream.next().await {
        events.push(e);
    }

    // Expected: TextDelta("partial"), Done with DegradedEof
    assert_eq!(events.len(), 2);
    match &events[0] {
        ProviderEvent::TextDelta(t) => assert_eq!(t, "partial"),
        other => panic!("expected TextDelta, got {other:?}"),
    }
    match &events[1] {
        ProviderEvent::Done(msg) => {
            // The key assertion: stop_reason should be DegradedEof, not EndTurn or whatever
            // the provider might have seen mid-stream.
            assert_eq!(msg.stop_reason, StopReason::DegradedEof);
            match &msg.content[0] {
                ContentBlock::Text { text } => assert_eq!(text, "partial"),
                other => panic!("expected Text block, got {other:?}"),
            }
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn map_stop_reason_known_and_unknown() {
    assert_eq!(map_stop_reason(Some("end_turn")), StopReason::EndTurn);
    assert_eq!(map_stop_reason(Some("tool_use")), StopReason::ToolUse);
    assert_eq!(map_stop_reason(Some("max_tokens")), StopReason::MaxTokens);
    assert_eq!(map_stop_reason(Some("weird")), StopReason::EndTurn);
    assert_eq!(map_stop_reason(None), StopReason::EndTurn);
}

/// B-6 (mixed content): text block then tool_use block in same response.
/// Final AssistantMessage.content has both blocks in document order.
#[tokio::test]
async fn b6_sse_mixed_text_and_tool_use() {
    let raw = concat!(
        r#"event: message_start"#,
        "\n",
        r#"data: {"type":"message_start","message":{"id":"m_1","role":"assistant"}}"#,
        "\n\n",
        // Block 0: text
        r#"event: content_block_start"#,
        "\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "\n\n",
        r#"event: content_block_delta"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"I will read it. "}}"#,
        "\n\n",
        r#"event: content_block_stop"#,
        "\n",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "\n\n",
        // Block 1: tool_use
        r#"event: content_block_start"#,
        "\n",
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_X","name":"read","input":{}}}"#,
        "\n\n",
        r#"event: content_block_delta"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
        "\n\n",
        r#"event: content_block_delta"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"/etc/hostname\"}"}}"#,
        "\n\n",
        r#"event: content_block_stop"#,
        "\n",
        r#"data: {"type":"content_block_stop","index":1}"#,
        "\n\n",
        r#"event: message_delta"#,
        "\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
        "\n\n",
        r#"event: message_stop"#,
        "\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n",
    );
    let bytes = futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::copy_from_slice(
        raw.as_bytes(),
    ))]);
    let (_tx, rx) = tokio::sync::oneshot::channel();
    let mut stream = test_events_stream(bytes, rx);

    let mut events = Vec::new();
    while let Some(e) = stream.next().await {
        events.push(e);
    }

    // Should be 2 events: TextDelta("I will read it. "), Done.
    assert_eq!(events.len(), 2, "got {events:?}");
    match &events[0] {
        ProviderEvent::TextDelta(t) => assert_eq!(t, "I will read it. "),
        other => panic!("expected TextDelta, got {other:?}"),
    }
    let done = match events.into_iter().nth(1).unwrap() {
        ProviderEvent::Done(msg) => msg,
        other => panic!("expected Done, got {other:?}"),
    };
    assert_eq!(done.stop_reason, StopReason::ToolUse);
    assert_eq!(done.content.len(), 2);
    match &done.content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "I will read it. "),
        other => panic!("expected Text, got {other:?}"),
    }
    match &done.content[1] {
        ContentBlock::ToolCall(tc) => {
            assert_eq!(tc.id, "toolu_X");
            assert_eq!(tc.name, "read");
            assert_eq!(tc.arguments["path"], "/etc/hostname");
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
}

/// B-7: tool_use only (no text block).
#[tokio::test]
async fn b7_sse_tool_use_only() {
    let raw = concat!(
        r#"event: content_block_start"#,
        "\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_Y","name":"echo","input":{}}}"#,
        "\n\n",
        r#"event: content_block_delta"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"text\":\"hi\"}"}}"#,
        "\n\n",
        r#"event: content_block_stop"#,
        "\n",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "\n\n",
        r#"event: message_delta"#,
        "\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
        "\n\n",
        r#"event: message_stop"#,
        "\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n",
    );
    let bytes = futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::copy_from_slice(
        raw.as_bytes(),
    ))]);
    let (_tx, rx) = tokio::sync::oneshot::channel();
    let mut stream = test_events_stream(bytes, rx);

    let events: Vec<_> = {
        let mut v = Vec::new();
        while let Some(e) = stream.next().await {
            v.push(e);
        }
        v
    };

    // Just one Done event (no text deltas because no text block).
    assert_eq!(events.len(), 1);
    let done = match events.into_iter().next().unwrap() {
        ProviderEvent::Done(msg) => msg,
        other => panic!("expected Done, got {other:?}"),
    };
    assert_eq!(done.content.len(), 1);
    match &done.content[0] {
        ContentBlock::ToolCall(tc) => {
            assert_eq!(tc.id, "toolu_Y");
            assert_eq!(tc.name, "echo");
            assert_eq!(tc.arguments["text"], "hi");
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
}

/// B-8: malformed input_json falls back to empty object, no panic.
#[tokio::test]
async fn b8_malformed_input_json_yields_empty_object() {
    let raw = concat!(
        r#"event: content_block_start"#,
        "\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_Z","name":"oops","input":{}}}"#,
        "\n\n",
        r#"event: content_block_delta"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{not valid"}}"#,
        "\n\n",
        r#"event: content_block_stop"#,
        "\n",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "\n\n",
        r#"event: message_stop"#,
        "\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n",
    );
    let bytes = futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::copy_from_slice(
        raw.as_bytes(),
    ))]);
    let (_tx, rx) = tokio::sync::oneshot::channel();
    let mut stream = test_events_stream(bytes, rx);

    let mut events = Vec::new();
    while let Some(e) = stream.next().await {
        events.push(e);
    }
    let done = events
        .into_iter()
        .find_map(|e| match e {
            ProviderEvent::Done(msg) => Some(msg),
            _ => None,
        })
        .expect("expected Done event");
    assert_eq!(done.content.len(), 1);
    match &done.content[0] {
        ContentBlock::ToolCall(tc) => {
            assert_eq!(tc.id, "toolu_Z");
            // Per INV-5: fall back to empty object on parse failure.
            assert!(tc.arguments.is_object());
            assert_eq!(tc.arguments.as_object().unwrap().len(), 0);
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
}

/// Non-object valid JSON also falls back to empty object per INV-5.
#[tokio::test]
async fn non_object_input_json_yields_empty_object() {
    // input_json is the JSON array `[1,2,3]` — valid JSON, not an object.
    let raw = concat!(
        r#"event: content_block_start"#,
        "\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"x","input":{}}}"#,
        "\n\n",
        r#"event: content_block_delta"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"[1,2,3]"}}"#,
        "\n\n",
        r#"event: message_stop"#,
        "\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n",
    );
    let bytes = futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::copy_from_slice(
        raw.as_bytes(),
    ))]);
    let (_tx, rx) = tokio::sync::oneshot::channel();
    let mut stream = test_events_stream(bytes, rx);

    let done = {
        let mut found = None;
        while let Some(e) = stream.next().await {
            if let ProviderEvent::Done(msg) = e {
                found = Some(msg);
                break;
            }
        }
        found.expect("expected Done")
    };
    match &done.content[0] {
        ContentBlock::ToolCall(tc) => {
            assert!(tc.arguments.is_object());
            assert_eq!(tc.arguments.as_object().unwrap().len(), 0);
        }
        _ => panic!("expected ToolCall"),
    }
}

// Live integration test (gated on MU_LIVE_ANTHROPIC env var)
mod live_tests {
    use super::*;
    use mu_core::agent::AgentMessage;

    fn live_enabled() -> bool {
        std::env::var("MU_LIVE_ANTHROPIC")
            .ok()
            .as_deref()
            .map(|v| v == "1")
            .unwrap_or(false)
    }

    /// Live API smoke from mu-006: verifies basic text streaming.
    /// Only runs when MU_LIVE_ANTHROPIC=1.
    #[tokio::test]
    async fn live_text_smoke() {
        if !live_enabled() {
            eprintln!("skipping live_text_smoke (set MU_LIVE_ANTHROPIC=1 to run)");
            return;
        }

        let provider = AnthropicProvider::from_env("claude-haiku-4-5-20251001".into())
            .expect("ANTHROPIC_API_KEY must be set when MU_LIVE_ANTHROPIC=1");

        let messages = vec![AgentMessage::User {
            content: "Reply with the single word 'hello' and nothing else.".into(),
        }];
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let mut stream = provider
            .stream(None, &messages, &[], rx)
            .await
            .expect("provider.stream");

        let mut text = String::new();
        let mut done_payload: Option<AssistantMessage> = None;
        while let Some(event) = stream.next().await {
            match event {
                ProviderEvent::TextDelta(d) => text.push_str(&d),
                ProviderEvent::Done(msg) => {
                    done_payload = Some(msg);
                    break;
                }
                ProviderEvent::Error(e) => panic!("anthropic error: {e}"),
                _ => {}
            }
        }

        let done = done_payload.expect("expected Done");
        let final_text = match &done.content[..] {
            [ContentBlock::Text { text }] => text.clone(),
            other => panic!("unexpected content blocks: {other:?}"),
        };
        eprintln!("live text smoke: {final_text:?}");
        assert!(
            final_text.to_lowercase().contains("hello"),
            "expected response to contain 'hello', got: {final_text:?}"
        );
        assert_eq!(text, final_text);
    }

    /// B-9: live API tool round-trip. Sends a tool spec; verifies the
    /// response includes a ToolCall with parsed arguments.
    /// Only runs when MU_LIVE_ANTHROPIC=1.
    #[tokio::test]
    async fn b9_live_anthropic_tool_call() {
        if !live_enabled() {
            eprintln!("skipping b9_live_anthropic_tool_call (set MU_LIVE_ANTHROPIC=1 to run)");
            return;
        }

        let provider = AnthropicProvider::from_env("claude-haiku-4-5-20251001".into())
            .expect("ANTHROPIC_API_KEY must be set when MU_LIVE_ANTHROPIC=1");

        let echo_tool = ToolSpec {
            name: "echo".to_string(),
            description: "Echo a string back to the user.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "The text to echo."
                    }
                },
                "required": ["text"]
            }),
            policy: Default::default(),
        };

        let messages = vec![AgentMessage::User {
            content: "Use the echo tool with text='hi there'. Just call the tool; no preamble."
                .into(),
        }];
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let mut stream = provider
            .stream(None, &messages, std::slice::from_ref(&echo_tool), rx)
            .await
            .expect("provider.stream");

        let mut done_payload: Option<AssistantMessage> = None;
        while let Some(event) = stream.next().await {
            match event {
                ProviderEvent::Done(msg) => {
                    done_payload = Some(msg);
                    break;
                }
                ProviderEvent::Error(e) => panic!("anthropic error: {e}"),
                _ => {}
            }
        }

        let done = done_payload.expect("expected Done");
        eprintln!("live tool smoke content: {:#?}", done.content);

        let tool_call = done
            .content
            .iter()
            .find_map(|b| match b {
                ContentBlock::ToolCall(tc) => Some(tc),
                _ => None,
            })
            .expect("expected at least one ToolCall in the response");

        assert_eq!(tool_call.name, "echo");
        assert!(
            tool_call.arguments.is_object(),
            "arguments must be an object, got: {:?}",
            tool_call.arguments
        );
        let text_arg = tool_call.arguments["text"].as_str().unwrap_or("");
        assert!(
            text_arg.to_lowercase().contains("hi"),
            "expected text arg to contain 'hi', got: {text_arg:?}"
        );

        // Stop reason should be tool_use when the model calls a tool.
        assert_eq!(done.stop_reason, StopReason::ToolUse);
    }
}

// ============================================================================
// mu-fb0 equivalence: rope+renderer path vs. existing AgentMessage path.
// ============================================================================
//
// The bead's load-bearing safety property is that the new rope-backed
// projection must describe the same model-visible payload as the
// existing `build_request_body` path. Provider::stream() is still fed
// raw `&[AgentMessage]` (preserving the wire-protocol surface, per
// stop-criterion #9), so the two paths share the wire body trivially;
// these tests assert the rope/renderer projection is a faithful
// shadow — same conversational role ordering, same content surfaces,
// same cache-boundary intent.

fn equivalence_fixture() -> (
    Option<String>,
    Vec<AgentMessage>,
    Vec<mu_core::agent::ToolSpec>,
) {
    let system_prompt = Some("you are mu, a careful assistant".to_string());
    let tool = mu_core::agent::ToolSpec {
        name: "read".into(),
        description: "read a file from the workspace".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
        }),
        policy: Default::default(),
    };
    let messages = vec![
        AgentMessage::User {
            content: "what's in /etc/hostname?".into(),
        },
        AgentMessage::Assistant(AssistantMessage {
            content: vec![
                ContentBlock::Text {
                    text: "I'll read it.".into(),
                },
                ContentBlock::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "/etc/hostname"}),
                }),
            ],
            stop_reason: StopReason::ToolUse,
            usage: None,
        }),
        AgentMessage::ToolResult {
            call_id: "c1".into(),
            content: "myhost".into(),
            is_error: false,
        },
    ];
    (system_prompt, messages, vec![tool])
}

#[test]
fn fb0_rope_role_sequence_matches_anthropic_wire_role_sequence() {
    // The rope's AgentView projection must yield the same role
    // sequence the Anthropic wire body would produce, augmented with
    // the System-role spans for the system prompt + tool schemas
    // (which the wire body emits as top-level `system` + `tools`
    // fields — same intent, different surface).
    let (system_prompt, messages, tools) = equivalence_fixture();
    let rope = assemble_rope(system_prompt.as_deref(), &messages, &tools);
    let projection =
        crate::context::AnthropicProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);

    // Expected role sequence: System (prompt), System (tool schema),
    // User, Assistant (with tool call), ToolResult.
    let roles: Vec<ProviderRole> = projection.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![
            ProviderRole::System,
            ProviderRole::System,
            ProviderRole::User,
            ProviderRole::Assistant,
            ProviderRole::ToolResult,
        ]
    );
}

#[test]
fn fb0_rope_user_assistant_toolresult_contents_round_trip() {
    let (system_prompt, messages, tools) = equivalence_fixture();
    let rope = assemble_rope(system_prompt.as_deref(), &messages, &tools);
    let projection =
        crate::context::AnthropicProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);

    // User message content is verbatim in the rope projection.
    let user_msg = projection
        .messages
        .iter()
        .find(|m| m.role == ProviderRole::User)
        .expect("user message");
    assert_eq!(user_msg.content, "what's in /etc/hostname?");

    // Assistant text + tool call flatten into one span; verify both
    // surfaces are present in the projection content. The wire body
    // emits them as separate content blocks (text + tool_use);
    // equivalence here is at the "model saw this byte sequence" level.
    let assistant_msg = projection
        .messages
        .iter()
        .find(|m| m.role == ProviderRole::Assistant)
        .expect("assistant message");
    assert!(assistant_msg.content.contains("I'll read it."));
    assert!(assistant_msg.content.contains("[tool_call:read("));

    // ToolResult content surfaces verbatim (non-error path — no
    // "error:" prefix).
    let tool_result = projection
        .messages
        .iter()
        .find(|m| m.role == ProviderRole::ToolResult)
        .expect("tool result");
    assert_eq!(tool_result.content, "myhost");
}

#[test]
fn fb0_rope_message_count_matches_wire_message_count() {
    // Every span in the rope's AgentView projection corresponds to
    // exactly one item the model is meant to see: system prompt,
    // each tool schema, then each conversational message in order.
    // The Anthropic wire body's `messages` field has fewer entries
    // (it groups tool results into a synthetic user message + omits
    // system from `messages`), but the LOGICAL count (system + tools
    // + conversational) is the same.
    let (system_prompt, messages, tools) = equivalence_fixture();
    let rope = assemble_rope(system_prompt.as_deref(), &messages, &tools);
    let projection =
        crate::context::AnthropicProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);

    let expected = usize::from(system_prompt.is_some()) + tools.len() + messages.len();
    assert_eq!(rope.len(), expected);
    assert_eq!(projection.len(), expected);
}

#[test]
fn fb0_cache_boundary_lands_on_last_stable_cacheable_span() {
    // mu-bn4's AnthropicCacheStrategy places its Ephemeral marker on
    // the last stable+cacheable span — for our fixture, that's the
    // tool schema (index 1: system at 0, tool schema at 1, then
    // volatile user/assistant/tool_result). Compare against the wire
    // body's `cache_control: ephemeral` on the last tool definition
    // (build_request_body, mu-i6j).
    let (system_prompt, messages, tools) = equivalence_fixture();
    let rope = assemble_rope(system_prompt.as_deref(), &messages, &tools);
    let renderer = crate::context::AnthropicProviderRenderer::new();
    let strategy = crate::context::AnthropicCacheStrategy::new();
    let mut projection = renderer.render(&rope, ProjectionTarget::AgentView);
    let boundaries = strategy.boundaries(&rope);
    strategy.annotate(&mut projection, &boundaries);

    // Boundary on the last stable+cacheable span (index 1, the tool
    // schema).
    assert_eq!(boundaries.len(), 1);
    assert_eq!(boundaries[0].message_index, 1);
    assert_eq!(rope.spans()[1].kind, SpanKind::ToolSchema);

    // Annotation lands on that message.
    assert_eq!(
        projection.messages[1].cache_marker,
        Some(CacheMarker::Ephemeral)
    );

    // And the wire body marks the last tool with cache_control
    // ephemeral — same intent.
    let wire = build_request_body("claude-test", system_prompt.as_deref(), &messages, &tools);
    assert_eq!(
        wire["tools"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .get("cache_control")
            .and_then(|v| v.get("type"))
            .and_then(|v| v.as_str()),
        Some("ephemeral"),
        "wire body must still place cache_control on the last tool",
    );
}

#[test]
fn fb0_no_system_prompt_yields_no_system_span() {
    // When system_prompt is None, neither the rope projection nor
    // the wire body should manifest a System span/field.
    let (_, messages, tools) = equivalence_fixture();
    let rope = assemble_rope(None, &messages, &tools);
    let projection =
        crate::context::AnthropicProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
    let system_count = projection
        .messages
        .iter()
        .filter(|m| {
            m.role == ProviderRole::System
                && m.source_span_ids.iter().any(|id| id == "system-prompt")
        })
        .count();
    assert_eq!(system_count, 0);

    let wire = build_request_body("claude-test", None, &messages, &tools);
    assert!(
        wire.get("system").is_none(),
        "no system_prompt → no `system` field in wire body",
    );
}
