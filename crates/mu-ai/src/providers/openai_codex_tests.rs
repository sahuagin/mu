use super::*;
use base64::Engine;
use bytes::Bytes;
use futures::StreamExt;
use mu_core::agent::{
    AgentMessage, AssistantMessage, ContentBlock, MessageInput, StopReason, ToolCall, ToolSpec,
};
use serde_json::json;
use std::pin::Pin;
use tempfile::TempDir;

// ============================================================================
// Helpers
// ============================================================================

fn synthetic_jwt(payload: serde_json::Value) -> String {
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = engine.encode(b"{\"alg\":\"none\"}");
    let payload_b64 = engine.encode(serde_json::to_string(&payload).unwrap().as_bytes());
    let sig = engine.encode(b"sig");
    format!("{header}.{payload_b64}.{sig}")
}

fn sample_token() -> OAuthToken {
    OAuthToken {
        access_token: synthetic_jwt(json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-test-123",
                "chatgpt_plan_type": "prolite",
            },
            "exp": 1_900_000_000_u64,
        })),
        refresh_token: Some("refresh-test".into()),
        id_token: None,
        token_type: "bearer".into(),
        expires_at: Some(1_900_000_000),
    }
}

/// Reconstruct a `StreamState` from canned bytes for unit testing
/// the event-stream loop without an HTTP round-trip.
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
        final_status: None,
        incomplete_reason: None,
        usage: None,
        cancel_rx: Some(cancel_rx),
        finished: false,
        emitted_done: false,
        error_message: None,
    };
    Box::pin(futures::stream::unfold(state, next_event))
}

// ============================================================================
// B-1: JWT claim extraction — happy path
// ============================================================================

#[test]
fn b1_extract_chatgpt_account_id_happy_path() {
    let jwt = synthetic_jwt(json!({
        "https://api.openai.com/auth": {
            "chatgpt_account_id": "abc-123",
            "chatgpt_plan_type": "prolite",
        },
        "exp": 9_999_999_999_u64,
    }));
    let id = extract_chatgpt_account_id(&jwt).expect("extract");
    assert_eq!(id, "abc-123");
}

// ============================================================================
// B-2: JWT extraction rejects bad formats
// ============================================================================

#[test]
fn b2_jwt_wrong_segment_count() {
    assert!(extract_chatgpt_account_id("not-a-jwt").is_err());
    assert!(extract_chatgpt_account_id("only.two").is_err());
    assert!(extract_chatgpt_account_id("a.b.c.d").is_err());
}

#[test]
fn b2_jwt_bad_base64_payload() {
    // 3 segments but middle isn't valid base64url.
    let jwt = "header.!!!not-base64!!!.sig";
    assert!(extract_chatgpt_account_id(jwt).is_err());
}

#[test]
fn b2_jwt_missing_openai_auth_claim() {
    // Valid JWT shape but wrong claim structure.
    let jwt = synthetic_jwt(json!({
        "iss": "https://auth.openai.com",
        "sub": "user-x",
    }));
    let err = extract_chatgpt_account_id(&jwt).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("openai") || msg.contains("auth"));
}

#[test]
fn b2_jwt_missing_chatgpt_account_id() {
    // Has openai auth claim but no chatgpt_account_id inside it.
    let jwt = synthetic_jwt(json!({
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "free",
        },
    }));
    let err = extract_chatgpt_account_id(&jwt).unwrap_err();
    assert!(format!("{err}").contains("chatgpt_account_id"));
}

// ============================================================================
// B-3: from_store fails clean when no token file
// ============================================================================

#[test]
fn b3_load_token_fails_clean_when_not_logged_in() {
    let dir = TempDir::new().unwrap();
    let store = FileSystemTokenStore::with_base_dir(dir.path().to_path_buf());
    let err = load_token(&store).expect_err("should fail when no token");
    let msg = format!("{err}");
    assert!(
        msg.contains("not logged in") && msg.contains("mu login"),
        "error should guide user to log in; got: {msg}"
    );
}

// ============================================================================
// B-4: from_store loads happily when token exists
// ============================================================================

#[test]
fn b4_load_token_happy_path() {
    let dir = TempDir::new().unwrap();
    let store = FileSystemTokenStore::with_base_dir(dir.path().to_path_buf());
    let token = sample_token();
    store.save("openai-codex", &token).unwrap();
    let loaded = load_token(&store).expect("loads");
    assert_eq!(loaded.access_token, token.access_token);
    assert_eq!(loaded.refresh_token, token.refresh_token);
}

// ============================================================================
// B-5: request body shape
// ============================================================================

#[test]
fn b5_build_request_body_basic() {
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let body = build_request_body("gpt-5-codex", "high", "you are a test", &messages, &[]);
    assert_eq!(body["model"], "gpt-5-codex");
    assert_eq!(body["instructions"], "you are a test");
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(body["reasoning"]["effort"], "high");
    assert_eq!(body["reasoning"]["summary"], "auto");

    let input = body["input"].as_array().expect("input array");
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[0]["content"][0]["type"], "input_text");
    assert_eq!(input[0]["content"][0]["text"], "hi");

    // No tools means no tools field.
    assert!(body.get("tools").is_none());
}

#[test]
fn b5_build_request_body_with_tools() {
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let tools = vec![ToolSpec {
        name: "read".into(),
        description: "Read a file.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
        }),
        policy: Default::default(),
    }];
    let body = build_request_body("gpt-5-codex", "medium", "sys", &messages, &tools);
    let api_tools = body["tools"].as_array().expect("tools array");
    assert_eq!(api_tools.len(), 1);
    // Responses API: flat function shape, NOT nested {function: {...}}.
    assert_eq!(api_tools[0]["type"], "function");
    assert_eq!(api_tools[0]["name"], "read");
    assert_eq!(api_tools[0]["description"], "Read a file.");
    assert_eq!(api_tools[0]["parameters"]["type"], "object");
    // tool_choice + parallel_tool_calls present when tools are.
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["parallel_tool_calls"], false);
}

#[test]
fn b5_translate_assistant_with_tool_call_produces_two_items() {
    let m = AgentMessage::Assistant(AssistantMessage {
        content: vec![
            ContentBlock::Text {
                text: "I will read it.".into(),
            },
            ContentBlock::ToolCall(ToolCall {
                id: "call_x".into(),
                name: "read".into(),
                arguments: json!({"path": "/x"}),
            }),
        ],
        stop_reason: StopReason::ToolUse,
        usage: None,
    });
    let items = translate_message(&m);
    assert_eq!(items.len(), 2, "expected message + function_call");
    assert_eq!(items[0]["type"], "message");
    assert_eq!(items[0]["role"], "assistant");
    assert_eq!(items[0]["content"][0]["type"], "output_text");
    assert_eq!(items[0]["content"][0]["text"], "I will read it.");
    assert_eq!(items[1]["type"], "function_call");
    assert_eq!(items[1]["call_id"], "call_x");
    assert_eq!(items[1]["name"], "read");
    // arguments is stringified JSON.
    let args_str = items[1]["arguments"].as_str().expect("args string");
    let parsed: Value = serde_json::from_str(args_str).unwrap();
    assert_eq!(parsed["path"], "/x");
}

#[test]
fn b5_translate_tool_result_ok() {
    let m = AgentMessage::ToolResult {
        call_id: "call_x".into(),
        content: "the file says hi".into(),
        is_error: false,
    };
    let items = translate_message(&m);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["type"], "function_call_output");
    assert_eq!(items[0]["call_id"], "call_x");
    assert_eq!(items[0]["output"], "the file says hi");
}

#[test]
fn b5_translate_tool_result_error_embeds_marker() {
    let m = AgentMessage::ToolResult {
        call_id: "call_x".into(),
        content: "permission denied".into(),
        is_error: true,
    };
    let items = translate_message(&m);
    let output = items[0]["output"].as_str().unwrap();
    assert!(output.contains("[error]"));
    assert!(output.contains("permission denied"));
}

// ============================================================================
// B-6: SSE → ProviderEvent — text only
// ============================================================================

#[tokio::test]
async fn b6_sse_text_only() {
    let raw = concat!(
        r#"event: response.output_text.delta"#,
        "\n",
        r#"data: {"type":"response.output_text.delta","delta":"hello"}"#,
        "\n\n",
        r#"event: response.output_text.delta"#,
        "\n",
        r#"data: {"type":"response.output_text.delta","delta":" world"}"#,
        "\n\n",
        r#"event: response.completed"#,
        "\n",
        r#"data: {"type":"response.completed","response":{"status":"completed"}}"#,
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

    assert_eq!(events.len(), 3, "got: {events:?}");
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
                ContentBlock::Text { text } => assert_eq!(text.as_ref(), "hello world"),
                other => panic!("expected Text, got {other:?}"),
            }
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

// ============================================================================
// B-7: SSE → ProviderEvent — tool call accumulation
// ============================================================================

#[tokio::test]
async fn b7_sse_tool_call_accumulation() {
    let raw = concat!(
        // Item added — function_call with empty args
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_a","name":"read","arguments":""}}"#,
        "\n\n",
        // Arguments stream
        r#"data: {"type":"response.function_call.arguments.delta","output_index":0,"item_id":"fc_1","delta":"{\"path\":"}"#,
        "\n\n",
        r#"data: {"type":"response.function_call.arguments.delta","output_index":0,"item_id":"fc_1","delta":"\"/tmp/foo\"}"}"#,
        "\n\n",
        // Item done — server replays the full arguments
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_a","name":"read","arguments":"{\"path\":\"/tmp/foo\"}"}}"#,
        "\n\n",
        // Stream completed
        r#"data: {"type":"response.completed","response":{"status":"completed"}}"#,
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

    // 2 ToolCallDelta + 1 Done
    assert_eq!(events.len(), 3, "got: {events:?}");
    for e in &events[..2] {
        match e {
            ProviderEvent::ToolCallDelta {
                id,
                arguments_delta,
                ..
            } => {
                assert_eq!(id, "call_a");
                assert!(
                    arguments_delta.as_deref().unwrap_or("").contains("path")
                        || arguments_delta.as_deref().unwrap_or("").contains("/tmp")
                );
            }
            other => panic!("expected ToolCallDelta, got {other:?}"),
        }
    }
    let done = match &events[2] {
        ProviderEvent::Done(m) => m,
        other => panic!("expected Done, got {other:?}"),
    };
    assert_eq!(done.stop_reason, StopReason::ToolUse);
    assert_eq!(done.content.len(), 1);
    match &done.content[0] {
        ContentBlock::ToolCall(tc) => {
            assert_eq!(tc.id, "call_a"); // call_id, not item id
            assert_eq!(tc.name, "read");
            assert_eq!(tc.arguments["path"], "/tmp/foo");
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
}

#[tokio::test]
async fn b7b_sse_mixed_text_and_tool() {
    let raw = concat!(
        r#"data: {"type":"response.output_text.delta","delta":"reading "}"#,
        "\n\n",
        r#"data: {"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_2","call_id":"call_b","name":"read","arguments":""}}"#,
        "\n\n",
        r#"data: {"type":"response.function_call.arguments.delta","output_index":1,"item_id":"fc_2","delta":"{\"path\":\"/x\"}"}"#,
        "\n\n",
        r#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"fc_2","call_id":"call_b","name":"read","arguments":"{\"path\":\"/x\"}"}}"#,
        "\n\n",
        r#"data: {"type":"response.completed","response":{"status":"completed"}}"#,
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
        .iter()
        .find_map(|e| {
            if let ProviderEvent::Done(m) = e {
                Some(m)
            } else {
                None
            }
        })
        .expect("Done");
    assert_eq!(done.stop_reason, StopReason::ToolUse);
    assert_eq!(done.content.len(), 2);
    match &done.content[0] {
        ContentBlock::Text { text } => assert_eq!(text.as_ref(), "reading "),
        other => panic!("expected Text, got {other:?}"),
    }
    match &done.content[1] {
        ContentBlock::ToolCall(tc) => {
            assert_eq!(tc.name, "read");
            assert_eq!(tc.arguments["path"], "/x");
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
}

// ============================================================================
// Reasoning summary → ThinkingDelta
// ============================================================================

#[tokio::test]
async fn reasoning_summary_emits_thinking_delta() {
    let raw = concat!(
        r#"data: {"type":"response.reasoning_summary.delta","delta":"planning..."}"#,
        "\n\n",
        r#"data: {"type":"response.output_text.delta","delta":"ok"}"#,
        "\n\n",
        r#"data: {"type":"response.completed","response":{"status":"completed"}}"#,
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
    assert!(events
        .iter()
        .any(|e| matches!(e, ProviderEvent::ThinkingDelta(t) if t == "planning...")));
    assert!(events
        .iter()
        .any(|e| matches!(e, ProviderEvent::TextDelta(t) if t == "ok")));
    assert!(events.iter().any(|e| matches!(e, ProviderEvent::Done(_))));
}

// ============================================================================
// Failure events → ProviderEvent::Error
// ============================================================================

#[tokio::test]
async fn failed_event_emits_error() {
    let raw = concat!(
        r#"data: {"type":"response.failed","response":{"status":"failed","error":{"message":"boom"}}}"#,
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
    assert_eq!(events.len(), 1);
    match &events[0] {
        ProviderEvent::Error(_) => {}
        other => panic!("expected Error, got {other:?}"),
    }
}

// ============================================================================
// Cancel mid-stream → Done(Aborted)
// ============================================================================

#[tokio::test]
async fn cancel_mid_stream_yields_aborted() {
    let raw = concat!(
        r#"data: {"type":"response.output_text.delta","delta":"partial"}"#,
        "\n\n",
    );
    let bytes = futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::copy_from_slice(
        raw.as_bytes(),
    ))]);
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut stream = test_events_stream(bytes, rx);

    // Take one delta.
    let e0 = stream.next().await.expect("first event");
    match e0 {
        ProviderEvent::TextDelta(t) => assert_eq!(t, "partial"),
        other => panic!("expected TextDelta, got {other:?}"),
    }
    // Fire cancel; the SSE stream has no further events,
    // so the loop will fall through and the cancel-check might
    // race. Drop tx instead to simulate cancel signal closing.
    let _ = tx.send(());
    // Drain.
    let remaining: Vec<_> = stream.collect().await;
    // Cancel may or may not arrive before EOF — accept either:
    // Done(Aborted) preferred, but Done(EndTurn) is acceptable if
    // EOF won the race. In both cases there's exactly one Done.
    assert_eq!(remaining.len(), 1);
    let done = match &remaining[0] {
        ProviderEvent::Done(m) => m,
        other => panic!("expected Done, got {other:?}"),
    };
    assert!(matches!(
        done.stop_reason,
        StopReason::Aborted | StopReason::EndTurn
    ));
    // Partial text preserved.
    match &done.content[..] {
        [ContentBlock::Text { text }] => assert_eq!(text.as_ref(), "partial"),
        other => panic!("expected Text content, got {other:?}"),
    }
}

// ============================================================================
// stop_reason mapping
// ============================================================================

#[test]
fn stop_reason_max_tokens_from_incomplete_details() {
    let mut state = StreamState {
        sse: Box::pin(futures::stream::empty()),
        accumulated_text: "partial".into(),
        tool_calls: HashMap::new(),
        tool_call_order: Vec::new(),
        final_status: Some("incomplete".into()),
        incomplete_reason: Some("max_output_tokens".into()),
        usage: None,
        cancel_rx: None,
        finished: false,
        emitted_done: false,
        error_message: None,
    };
    assert_eq!(map_stop(&state), StopReason::MaxTokens);
    // Even without final_status set, incomplete_reason wins.
    state.final_status = None;
    assert_eq!(map_stop(&state), StopReason::MaxTokens);
}

// ============================================================================
// Live integration tests (gated on MU_LIVE_OPENAI_CODEX=1)
// ============================================================================

mod live_tests {
    use super::*;
    use mu_core::agent::AgentMessage;

    fn live_enabled() -> bool {
        std::env::var("MU_LIVE_OPENAI_CODEX").ok().as_deref() == Some("1")
    }

    /// B-12: live text smoke against the user's actual Codex backend.
    #[tokio::test]
    async fn b12_live_codex_text_smoke() {
        if !live_enabled() {
            eprintln!("skipping b12_live_codex_text_smoke (set MU_LIVE_OPENAI_CODEX=1)");
            return;
        }

        let provider = OpenaiCodexProvider::from_store("gpt-5-codex".into())
            .expect("must be logged in via `mu login --provider openai-codex`");

        let messages = vec![AgentMessage::User {
            content: "Reply with the single word 'hello' and nothing else.".into(),
        }];
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let mut stream = provider
            .stream(None, MessageInput::Legacy(&messages), &[], rx)
            .await
            .expect("provider.stream");

        let mut text = String::new();
        let mut got_done = false;
        while let Some(event) = stream.next().await {
            match event {
                ProviderEvent::TextDelta(d) => text.push_str(&d),
                ProviderEvent::Done(_) => {
                    got_done = true;
                    break;
                }
                ProviderEvent::Error(e) => panic!("codex error: {e}"),
                _ => {}
            }
        }
        assert!(got_done);
        eprintln!("live codex smoke text: {text:?}");
        assert!(
            text.to_lowercase().contains("hello"),
            "expected 'hello', got: {text:?}"
        );
    }

    /// B-13: live tool round-trip.
    #[tokio::test]
    async fn b13_live_codex_tool_call() {
        if !live_enabled() {
            eprintln!("skipping b13_live_codex_tool_call (set MU_LIVE_OPENAI_CODEX=1)");
            return;
        }

        let provider =
            OpenaiCodexProvider::from_store("gpt-5-codex".into()).expect("must be logged in");

        let echo_tool = ToolSpec {
            name: "echo".into(),
            description: "Echo a string.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"],
            }),
            policy: Default::default(),
        };

        let messages = vec![AgentMessage::User {
            content: "Use the echo tool with text='hi there'. Just call the tool.".into(),
        }];
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let mut stream = provider
            .stream(
                None,
                MessageInput::Legacy(&messages),
                std::slice::from_ref(&echo_tool),
                rx,
            )
            .await
            .expect("provider.stream");

        let mut done_payload: Option<AssistantMessage> = None;
        while let Some(event) = stream.next().await {
            if let ProviderEvent::Done(msg) = event {
                done_payload = Some(msg);
                break;
            }
        }

        let done = done_payload.expect("Done");
        eprintln!("live codex tool content: {:#?}", done.content);

        let tc = done
            .content
            .iter()
            .find_map(|b| match b {
                ContentBlock::ToolCall(tc) => Some(tc),
                _ => None,
            })
            .expect("expected a ToolCall");
        assert_eq!(tc.name, "echo");
        assert!(tc.arguments.is_object());
    }
}
