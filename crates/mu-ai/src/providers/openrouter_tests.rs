use super::*;
use bytes::Bytes;
use futures::StreamExt;
use mu_core::agent::{AgentMessage, AssistantMessage, ContentBlock, StopReason, ToolCall};
use serde_json::json;

#[test]
fn b1_translate_user_message() {
    let m = AgentMessage::User {
        content: "hi".into(),
    };
    let v = translate_message(&m).expect("translates");
    assert_eq!(v["role"], "user");
    assert_eq!(v["content"], "hi");
}

#[test]
fn b2_translate_assistant_text_only() {
    let m = AgentMessage::Assistant(AssistantMessage {
        content: vec![ContentBlock::Text { text: "hi".into() }],
        stop_reason: StopReason::EndTurn,
        usage: None,
    });
    let v = translate_message(&m).expect("translates");
    assert_eq!(v["role"], "assistant");
    assert_eq!(v["content"], "hi");
    // tool_calls absent when no tools.
    assert!(v.get("tool_calls").is_none());
}

#[test]
fn b3_translate_assistant_with_tool_call() {
    let m = AgentMessage::Assistant(AssistantMessage {
        content: vec![
            ContentBlock::Text {
                text: "I will read it.".into(),
            },
            ContentBlock::ToolCall(ToolCall {
                id: "call_x".into(),
                name: "read".into(),
                arguments: json!({"path": "/tmp/foo"}),
            }),
        ],
        stop_reason: StopReason::ToolUse,
        usage: None,
    });
    let v = translate_message(&m).expect("translates");
    assert_eq!(v["role"], "assistant");
    assert_eq!(v["content"], "I will read it.");
    assert_eq!(v["tool_calls"][0]["id"], "call_x");
    assert_eq!(v["tool_calls"][0]["type"], "function");
    assert_eq!(v["tool_calls"][0]["function"]["name"], "read");
    // arguments is a JSON-stringified object per OpenAI's format.
    let args_str = v["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .expect("string");
    let parsed: Value = serde_json::from_str(args_str).expect("valid json");
    assert_eq!(parsed["path"], "/tmp/foo");
}

#[test]
fn b4_translate_tool_result_ok() {
    let m = AgentMessage::ToolResult {
        call_id: "call_x".into(),
        content: "the file says hello".into(),
        is_error: false,
    };
    let v = translate_message(&m).expect("translates");
    assert_eq!(v["role"], "tool");
    assert_eq!(v["tool_call_id"], "call_x");
    assert_eq!(v["content"], "the file says hello");
}

#[test]
fn b4_translate_tool_result_error_embeds_marker() {
    let m = AgentMessage::ToolResult {
        call_id: "call_x".into(),
        content: "permission denied".into(),
        is_error: true,
    };
    let v = translate_message(&m).expect("translates");
    let content = v["content"].as_str().expect("string");
    assert!(
        content.contains("[error]"),
        "is_error: true should put a marker in content; got: {content:?}"
    );
    assert!(content.contains("permission denied"));
}

#[test]
fn b5_translate_tool_spec() {
    let spec = ToolSpec {
        name: "read".into(),
        description: "Read a file.".into(),
        input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        policy: Default::default(),
    };
    let v = translate_tool_spec(&spec);
    assert_eq!(v["type"], "function");
    assert_eq!(v["function"]["name"], "read");
    assert_eq!(v["function"]["description"], "Read a file.");
    assert_eq!(v["function"]["parameters"]["type"], "object");
}

#[test]
fn b6_build_request_body_includes_tools() {
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let tools = vec![ToolSpec {
        name: "read".into(),
        description: "Read".into(),
        input_schema: json!({"type": "object"}),
        policy: Default::default(),
    }];
    let body = build_request_body("test/model", None, &messages, &tools);
    assert_eq!(body["model"], "test/model");
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_tokens"], 4096);
    assert_eq!(body["tools"][0]["function"]["name"], "read");
    assert_eq!(body["messages"][0]["role"], "user");
}

#[test]
fn b6b_build_request_body_omits_tools_when_empty() {
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let body = build_request_body("test/model", None, &messages, &[]);
    assert!(body.get("tools").is_none());
}

// mu-n48: OpenAI-style providers express the system prompt as a
// {role: "system"} message prepended to the messages array.

#[test]
fn mu_n48_system_prompt_none_does_not_prepend_system_message() {
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let body = build_request_body("test/model", None, &messages, &[]);
    let arr = body["messages"].as_array().expect("messages array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["role"], "user");
}

#[test]
fn mu_n48_system_prompt_set_prepends_system_message() {
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let body = build_request_body("test/model", Some("you are concise"), &messages, &[]);
    let arr = body["messages"].as_array().expect("messages array");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["role"], "system");
    assert_eq!(arr[0]["content"], "you are concise");
    assert_eq!(arr[1]["role"], "user");
}

#[test]
fn mu_n48_empty_system_prompt_does_not_prepend() {
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let body = build_request_body("test/model", Some(""), &messages, &[]);
    let arr = body["messages"].as_array().expect("messages array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["role"], "user");
}

#[test]
fn map_finish_reason_known_and_unknown() {
    assert_eq!(map_finish_reason(Some("stop")), StopReason::EndTurn);
    assert_eq!(map_finish_reason(Some("tool_calls")), StopReason::ToolUse);
    assert_eq!(map_finish_reason(Some("length")), StopReason::MaxTokens);
    assert_eq!(map_finish_reason(Some("weird")), StopReason::EndTurn);
    assert_eq!(map_finish_reason(None), StopReason::EndTurn);
}

/// Helper: build a stream from raw SSE bytes for tests.
fn test_events_stream(
    bytes: impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
    cancel_rx: oneshot::Receiver<()>,
) -> BoxStream<'static, ProviderEvent> {
    let bytes: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> = Box::pin(bytes);
    let sse = SseStream::new(bytes);
    let state = StreamState {
        sse: Box::pin(sse),
        accumulated_text: String::new(),
        tool_calls: HashMap::new(),
        tool_call_order: Vec::new(),
        finish_reason: None,
        usage: None,
        cancel_rx: Some(cancel_rx),
        finished: false,
        emitted_done: false,
    };
    Box::pin(futures::stream::unfold(state, next_event))
}

#[tokio::test]
async fn b7_sse_text_only() {
    let raw = concat!(
        r#"data: {"choices":[{"delta":{"content":"hello"}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{"content":" world"}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        "\n\n",
        r#"data: [DONE]"#,
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

    assert_eq!(events.len(), 3, "got {events:?}");
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
            assert_eq!(msg.content.len(), 1);
            match &msg.content[0] {
                ContentBlock::Text { text } => assert_eq!(text, "hello world"),
                other => panic!("expected Text, got {other:?}"),
            }
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test]
async fn b8_sse_tool_call_accumulation() {
    let raw = concat!(
        // First chunk: tool_call starts with id, name, partial args
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"read","arguments":""}}]}}]}"#,
        "\n\n",
        // Second chunk: more arguments
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}"#,
        "\n\n",
        // Third chunk: rest of arguments
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"/tmp/foo\"}"}}]}}]}"#,
        "\n\n",
        // Final chunk: finish_reason
        r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        "\n\n",
        r#"data: [DONE]"#,
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

    // Just one Done event (we don't emit ToolCallDelta during streaming in v1).
    assert_eq!(events.len(), 1);
    let done = match events.into_iter().next().unwrap() {
        ProviderEvent::Done(msg) => msg,
        other => panic!("expected Done, got {other:?}"),
    };
    assert_eq!(done.stop_reason, StopReason::ToolUse);
    assert_eq!(done.content.len(), 1);
    match &done.content[0] {
        ContentBlock::ToolCall(tc) => {
            assert_eq!(tc.id, "call_a");
            assert_eq!(tc.name, "read");
            assert_eq!(tc.arguments["path"], "/tmp/foo");
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
}

#[tokio::test]
async fn b9_sse_mixed_text_and_tool_call() {
    let raw = concat!(
        r#"data: {"choices":[{"delta":{"content":"I will read it. "}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_b","type":"function","function":{"name":"read","arguments":"{\"path\":\"/x\"}"}}]}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        "\n\n",
        r#"data: [DONE]"#,
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

    // 1 TextDelta + 1 Done.
    assert_eq!(events.len(), 2);
    let done = match events.into_iter().nth(1).unwrap() {
        ProviderEvent::Done(msg) => msg,
        other => panic!("expected Done, got {other:?}"),
    };
    assert_eq!(done.content.len(), 2);
    match &done.content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "I will read it. "),
        other => panic!("expected Text, got {other:?}"),
    }
    match &done.content[1] {
        ContentBlock::ToolCall(tc) => {
            assert_eq!(tc.id, "call_b");
            assert_eq!(tc.name, "read");
            assert_eq!(tc.arguments["path"], "/x");
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
}

#[tokio::test]
async fn b10_malformed_tool_args_yield_empty_object() {
    let raw = concat!(
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_z","type":"function","function":{"name":"oops","arguments":"{not valid"}}]}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        "\n\n",
        r#"data: [DONE]"#,
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
        found.expect("Done")
    };
    match &done.content[0] {
        ContentBlock::ToolCall(tc) => {
            assert!(tc.arguments.is_object());
            assert_eq!(tc.arguments.as_object().unwrap().len(), 0);
        }
        _ => panic!("expected ToolCall"),
    }
}

// ============================================================================
// Live integration tests (gated on MU_LIVE_OPENROUTER=1)
// ============================================================================

mod live_tests {
    use super::*;
    use mu_core::agent::AgentMessage;

    fn live_enabled() -> bool {
        std::env::var("MU_LIVE_OPENROUTER").ok().as_deref() == Some("1")
    }

    /// B-12: live text smoke. Cheap model.
    #[tokio::test]
    async fn b12_live_openrouter_text_smoke() {
        if !live_enabled() {
            eprintln!("skipping b12_live_openrouter_text_smoke (set MU_LIVE_OPENROUTER=1)");
            return;
        }

        let provider = OpenRouterProvider::from_env("anthropic/claude-haiku-4.5".into())
            .expect("OPENROUTER_API_KEY must be set when MU_LIVE_OPENROUTER=1");

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
                ProviderEvent::Error(e) => panic!("openrouter error: {e}"),
                _ => {}
            }
        }

        let done = done_payload.expect("expected Done");
        eprintln!("live openrouter text smoke: {text:?}");
        assert!(
            text.to_lowercase().contains("hello"),
            "expected response to contain 'hello', got: {text:?}"
        );
        // Final content's text matches accumulated.
        let content_text = match &done.content[..] {
            [ContentBlock::Text { text }] => text.clone(),
            other => panic!("unexpected content blocks: {other:?}"),
        };
        assert_eq!(text, content_text);
    }

    /// B-13: live tool round-trip.
    #[tokio::test]
    async fn b13_live_openrouter_tool_call() {
        if !live_enabled() {
            eprintln!("skipping b13_live_openrouter_tool_call (set MU_LIVE_OPENROUTER=1)");
            return;
        }

        let provider = OpenRouterProvider::from_env("anthropic/claude-haiku-4.5".into())
            .expect("OPENROUTER_API_KEY must be set when MU_LIVE_OPENROUTER=1");

        let echo_tool = ToolSpec {
            name: "echo".into(),
            description: "Echo a string back to the user.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Text to echo" }
                },
                "required": ["text"]
            }),
            policy: Default::default(),
        };

        let messages = vec![AgentMessage::User {
            content: "Use the echo tool with text='hi there'. Just call the tool, no preamble."
                .into(),
        }];
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let mut stream = provider
            .stream(None, &messages, std::slice::from_ref(&echo_tool), rx)
            .await
            .expect("provider.stream");

        let mut done_payload: Option<AssistantMessage> = None;
        while let Some(event) = stream.next().await {
            if let ProviderEvent::Done(msg) = event {
                done_payload = Some(msg);
                break;
            }
        }

        let done = done_payload.expect("expected Done");
        eprintln!("live openrouter tool smoke content: {:#?}", done.content);

        let tool_call = done
            .content
            .iter()
            .find_map(|b| match b {
                ContentBlock::ToolCall(tc) => Some(tc),
                _ => None,
            })
            .expect("expected at least one ToolCall");

        assert_eq!(tool_call.name, "echo");
        assert!(tool_call.arguments.is_object());
        let text_arg = tool_call.arguments["text"].as_str().unwrap_or("");
        assert!(
            text_arg.to_lowercase().contains("hi"),
            "expected text arg to contain 'hi', got: {text_arg:?}"
        );
    }
}
