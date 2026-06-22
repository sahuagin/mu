// Fixture builders specify every field then add `..Default::default()`; the
// trailing update is harmless test noise, not worth churning each literal.
#![allow(clippy::needless_update)]

use super::*;
use base64::Engine;
use bytes::Bytes;
use futures::StreamExt;
use mu_core::agent::{
    AgentMessage, AssistantMessage, ContentBlock, MessageInput, StopReason, ToolArgs, ToolCall,
    ToolSpec,
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

/// Reconstruct a `StreamState` from canned bytes for unit testing the
/// fold loop without an HTTP round-trip.
fn test_events_stream(
    bytes: impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
    cancel_rx: oneshot::Receiver<()>,
) -> BoxStream<'static, ProviderEvent> {
    let bytes: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> = Box::pin(bytes);
    let sse = SseStream::new(bytes);
    let state = new_stream_state(Box::pin(sse), cancel_rx);
    Box::pin(futures::stream::unfold(state, next_event))
}

fn run(raw: &str) -> Vec<ProviderEvent> {
    let bytes = futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::copy_from_slice(
        raw.as_bytes(),
    ))]);
    let (_tx, rx) = tokio::sync::oneshot::channel();
    let stream = test_events_stream(bytes, rx);
    futures::executor::block_on(stream.collect())
}

// ============================================================================
// Auth-mode construction
// ============================================================================

#[test]
fn codex_constructors_use_codex_endpoint_and_label() {
    let p = OpenaiProvider::from_parts("gpt-5.5".into(), sample_token(), None);
    assert!(p.is_codex());
    assert_eq!(p.endpoint, CODEX_ENDPOINT);
    assert_eq!(p.provider_label(), "openai_codex");
}

#[test]
fn public_constructor_uses_public_endpoint() {
    let p = OpenaiProvider::from_api_key("gpt-5".into(), "sk-test".into());
    assert!(!p.is_codex());
    assert_eq!(p.endpoint, PUBLIC_ENDPOINT);
    // Label is shared across modes (downstream expects "openai_codex").
    assert_eq!(p.provider_label(), "openai_codex");
}

#[test]
fn public_key_resolution_prefers_env() {
    // We can't safely mutate process env in parallel tests; instead test
    // the TOML fallback parser directly via from_api_key + a temp config.
    // Env-precedence is covered by from_env's order; the TOML path:
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[openai]\napi_key = \"sk-from-toml\"\n").unwrap();
    // Point T4C_AGENT_CONFIG at our temp file for this assertion only.
    // (Serialized via a guard so parallel tests don't clobber it.)
    let _guard = ENV_LOCK.lock().unwrap();
    let prev_t4c = std::env::var("T4C_AGENT_CONFIG").ok();
    let prev_key = std::env::var("OPENAI_API_KEY").ok();
    std::env::remove_var("OPENAI_API_KEY");
    std::env::set_var("T4C_AGENT_CONFIG", &path);
    let resolved = resolve_public_api_key().expect("resolve from toml");
    assert_eq!(resolved, "sk-from-toml");
    // restore
    match prev_t4c {
        Some(v) => std::env::set_var("T4C_AGENT_CONFIG", v),
        None => std::env::remove_var("T4C_AGENT_CONFIG"),
    }
    if let Some(v) = prev_key {
        std::env::set_var("OPENAI_API_KEY", v);
    }
}

use std::sync::Mutex as StdMutex;
static ENV_LOCK: StdMutex<()> = StdMutex::new(());

// ============================================================================
// JWT claim extraction
// ============================================================================

#[test]
fn jwt_extract_happy_path() {
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

#[test]
fn jwt_wrong_segment_count() {
    assert!(extract_chatgpt_account_id("not-a-jwt").is_err());
    assert!(extract_chatgpt_account_id("only.two").is_err());
    assert!(extract_chatgpt_account_id("a.b.c.d").is_err());
}

#[test]
fn jwt_bad_base64_payload() {
    let jwt = "header.!!!not-base64!!!.sig";
    assert!(extract_chatgpt_account_id(jwt).is_err());
}

#[test]
fn jwt_missing_openai_auth_claim() {
    let jwt = synthetic_jwt(json!({
        "iss": "https://auth.openai.com",
        "sub": "user-x",
    }));
    let err = extract_chatgpt_account_id(&jwt).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("openai") || msg.contains("auth"));
}

#[test]
fn jwt_missing_chatgpt_account_id() {
    let jwt = synthetic_jwt(json!({
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "free",
        },
    }));
    let err = extract_chatgpt_account_id(&jwt).unwrap_err();
    assert!(format!("{err}").contains("chatgpt_account_id"));
}

// ============================================================================
// Token store
// ============================================================================

#[test]
fn load_token_fails_clean_when_not_logged_in() {
    let dir = TempDir::new().unwrap();
    let store = FileSystemTokenStore::with_base_dir(dir.path().to_path_buf());
    let err = load_token(&store).expect_err("should fail when no token");
    let msg = format!("{err}");
    assert!(
        msg.contains("not logged in") && msg.contains("mu login"),
        "error should guide user to log in; got: {msg}"
    );
}

#[test]
fn load_token_happy_path() {
    let dir = TempDir::new().unwrap();
    let store = FileSystemTokenStore::with_base_dir(dir.path().to_path_buf());
    let token = sample_token();
    store.save("openai-codex", &token).unwrap();
    let loaded = load_token(&store).expect("loads");
    assert_eq!(loaded.access_token, token.access_token);
    assert_eq!(loaded.refresh_token, token.refresh_token);
}

// ============================================================================
// Request body shape (Legacy)
// ============================================================================

#[test]
fn build_request_body_basic() {
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let body = build_request_value("gpt-5-codex", "high", "you are a test", &messages, &[]);
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
fn instructions_under_cap_unchanged() {
    let short = "you are mu";
    assert!(short.len() < INSTRUCTIONS_SOFT_CAP);
    let body = build_request_value(
        "gpt-5-codex",
        "medium",
        short,
        &[AgentMessage::User {
            content: "hi".into(),
        }],
        &[],
    );
    assert_eq!(body["instructions"], short);
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[0]["content"][0]["text"], "hi");
}

#[test]
fn instructions_over_cap_moved_to_input() {
    // codex's instructions field silently fails (200 OK, empty SSE
    // stream) when oversized; we move overflow to a synthetic user
    // message at input[0] and the field holds DEFAULT_INSTRUCTIONS.
    let huge = "X".repeat(INSTRUCTIONS_SOFT_CAP + 1);
    let body = build_request_value(
        "gpt-5-codex",
        "medium",
        &huge,
        &[AgentMessage::User {
            content: "hi".into(),
        }],
        &[],
    );

    let field = body["instructions"].as_str().unwrap();
    assert!(
        field.len() <= INSTRUCTIONS_SOFT_CAP,
        "instructions field still oversized after split: {} bytes",
        field.len()
    );

    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 2, "expected overflow + original user msg");
    assert_eq!(input[0]["role"], "user");
    let overflow_text = input[0]["content"][0]["text"].as_str().unwrap();
    assert!(
        overflow_text.contains(&huge),
        "overflow message should carry the original instructions"
    );
    assert!(
        overflow_text.starts_with("[System context"),
        "overflow should be prefixed with framing"
    );
    assert_eq!(input[1]["role"], "user");
    assert_eq!(input[1]["content"][0]["text"], "hi");
}

#[test]
fn build_request_body_with_tools() {
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

        ..Default::default()
    }];
    let body = build_request_value("gpt-5-codex", "medium", "sys", &messages, &tools);
    let api_tools = body["tools"].as_array().expect("tools array");
    assert_eq!(api_tools.len(), 1);
    // Responses API: flat function shape, NOT nested {function: {...}}.
    assert_eq!(api_tools[0]["type"], "function");
    assert_eq!(api_tools[0]["name"], "read");
    assert_eq!(api_tools[0]["description"], "Read a file.");
    assert_eq!(api_tools[0]["parameters"]["type"], "object");
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["parallel_tool_calls"], false);
}

#[test]
fn translate_assistant_with_tool_call_produces_two_items() {
    let m = AgentMessage::Assistant(AssistantMessage {
        content: vec![
            ContentBlock::Text {
                text: "I will read it.".into(),
            },
            ContentBlock::ToolCall(ToolCall {
                id: "call_x".into(),
                name: "read".into(),
                arguments: ToolArgs::new(json!({"path": "/x"})).unwrap(),
            }),
        ],
        stop_reason: StopReason::ToolUse,
        usage: None,
    });
    let body = build_request_value("m", "high", "sys", std::slice::from_ref(&m), &[]);
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 2, "expected message + function_call");
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[0]["role"], "assistant");
    assert_eq!(input[0]["content"][0]["type"], "output_text");
    assert_eq!(input[0]["content"][0]["text"], "I will read it.");
    assert_eq!(input[1]["type"], "function_call");
    assert_eq!(input[1]["call_id"], "call_x");
    assert_eq!(input[1]["name"], "read");
    let args_str = input[1]["arguments"].as_str().expect("args string");
    let parsed: Value = serde_json::from_str(args_str).unwrap();
    assert_eq!(parsed["path"], "/x");
}

#[test]
fn translate_tool_result_ok() {
    let m = AgentMessage::ToolResult {
        call_id: "call_x".into(),
        content: "the file says hi".into(),
        is_error: false,
    };
    let body = build_request_value("m", "high", "sys", std::slice::from_ref(&m), &[]);
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["type"], "function_call_output");
    assert_eq!(input[0]["call_id"], "call_x");
    assert_eq!(input[0]["output"], "the file says hi");
}

#[test]
fn translate_tool_result_error_embeds_marker() {
    let m = AgentMessage::ToolResult {
        call_id: "call_x".into(),
        content: "permission denied".into(),
        is_error: true,
    };
    let body = build_request_value("m", "high", "sys", std::slice::from_ref(&m), &[]);
    let input = body["input"].as_array().unwrap();
    let output = input[0]["output"].as_str().unwrap();
    assert!(output.contains("[error]"));
    assert!(output.contains("permission denied"));
}

#[test]
fn thinking_block_dropped_outbound() {
    // PR-A behavior preservation: Thinking is NOT echoed back to the model.
    let m = AgentMessage::Assistant(AssistantMessage {
        content: vec![
            ContentBlock::Thinking {
                text: "INTERNAL_REASONING_DO_NOT_LEAK".into(),
            },
            ContentBlock::Text {
                text: "public answer".into(),
            },
        ],
        stop_reason: StopReason::EndTurn,
        usage: None,
    });
    let body = build_request_value("m", "high", "sys", std::slice::from_ref(&m), &[]);
    let wire = serde_json::to_string(&body).unwrap();
    assert!(
        !wire.contains("INTERNAL_REASONING_DO_NOT_LEAK"),
        "Thinking leaked to wire: {wire}"
    );
    assert!(wire.contains("public answer"));
}

// ============================================================================
// SSE fold — text only (official text-delta spelling)
// ============================================================================

#[test]
fn sse_text_only() {
    let raw = concat!(
        r#"data: {"type":"response.output_text.delta","delta":"hello","sequence_number":1}"#,
        "\n\n",
        r#"data: {"type":"response.output_text.delta","delta":" world","sequence_number":2}"#,
        "\n\n",
        r#"data: {"type":"response.completed","sequence_number":3,"response":{"id":"r","status":"completed"}}"#,
        "\n\n",
    );
    let events = run(raw);
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

/// Terminal snapshot with an authoritative `output` adopts it over the
/// streamed accumulation (here they agree); usage is surfaced.
#[test]
fn sse_text_with_snapshot_and_usage() {
    let raw = concat!(
        r#"data: {"type":"response.output_text.delta","delta":"hi","sequence_number":1}"#,
        "\n\n",
        r#"data: {"type":"response.completed","sequence_number":2,"response":{"id":"r","status":"completed","output":[{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"hi","annotations":[]}]}],"usage":{"input_tokens":10,"output_tokens":3,"output_tokens_details":{"reasoning_tokens":2}}}}"#,
        "\n\n",
    );
    let events = run(raw);
    let done = events
        .iter()
        .find_map(|e| match e {
            ProviderEvent::Done(m) => Some(m),
            _ => None,
        })
        .expect("Done");
    assert_eq!(done.stop_reason, StopReason::EndTurn);
    match &done.content[0] {
        ContentBlock::Text { text } => assert_eq!(text.as_ref(), "hi"),
        other => panic!("expected Text, got {other:?}"),
    }
    let u = done.usage.expect("usage");
    assert_eq!(u.input_tokens, 10);
    assert_eq!(u.output_tokens, 3);
    assert_eq!(u.reasoning_tokens, Some(2));
}

// ============================================================================
// mu-s545: two message output items must not fuse
// ============================================================================

#[test]
fn two_message_items_get_paragraph_break() {
    let raw = concat!(
        r#"data: {"type":"response.output_item.added","output_index":0,"sequence_number":0,"item":{"type":"message","id":"msg_1"}}"#,
        "\n\n",
        r#"data: {"type":"response.output_text.delta","delta":"take one, or hold.","sequence_number":1}"#,
        "\n\n",
        r#"data: {"type":"response.output_item.done","output_index":0,"sequence_number":2,"item":{"type":"message","id":"msg_1"}}"#,
        "\n\n",
        r#"data: {"type":"response.output_item.added","output_index":1,"sequence_number":3,"item":{"type":"message","id":"msg_2"}}"#,
        "\n\n",
        r#"data: {"type":"response.output_text.delta","delta":"No worries, take two.","sequence_number":4}"#,
        "\n\n",
        r#"data: {"type":"response.completed","sequence_number":5,"response":{"id":"r","status":"completed"}}"#,
        "\n\n",
    );
    let events = run(raw);
    let deltas: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::TextDelta(d) => Some(d.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        deltas,
        vec!["take one, or hold.", "\n\n", "No worries, take two."],
        "separator must also stream as a TextDelta"
    );
    match events.last().expect("no events") {
        ProviderEvent::Done(msg) => match &msg.content[0] {
            ContentBlock::Text { text } => assert_eq!(
                text.as_ref(),
                "take one, or hold.\n\nNo worries, take two.",
                "message items must not fuse"
            ),
            other => panic!("expected Text, got {other:?}"),
        },
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn message_after_toolcall_no_leading_separator() {
    let raw = concat!(
        r#"data: {"type":"response.output_item.added","output_index":0,"sequence_number":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_a","name":"read","arguments":"{}"}}"#,
        "\n\n",
        r#"data: {"type":"response.output_item.done","output_index":0,"sequence_number":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_a","name":"read","arguments":"{}"}}"#,
        "\n\n",
        r#"data: {"type":"response.output_item.added","output_index":1,"sequence_number":2,"item":{"type":"message","id":"msg_1"}}"#,
        "\n\n",
        r#"data: {"type":"response.output_text.delta","delta":"after tool","sequence_number":3}"#,
        "\n\n",
        r#"data: {"type":"response.completed","sequence_number":4,"response":{"id":"r","status":"completed"}}"#,
        "\n\n",
    );
    let events = run(raw);
    match events.last().expect("no events") {
        ProviderEvent::Done(msg) => {
            let text = msg
                .content
                .iter()
                .find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_ref()),
                    _ => None,
                })
                .expect("no text block");
            assert_eq!(text, "after tool", "no separator when accumulator empty");
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

// ============================================================================
// SSE fold — tool call accumulation (official + codex-compat spellings)
// ============================================================================

#[test]
fn sse_tool_call_accumulation_official_spelling() {
    let raw = concat!(
        r#"data: {"type":"response.output_item.added","output_index":0,"sequence_number":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_a","name":"read","arguments":""}}"#,
        "\n\n",
        r#"data: {"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_1","delta":"{\"path\":","sequence_number":1}"#,
        "\n\n",
        r#"data: {"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_1","delta":"\"/tmp/foo\"}","sequence_number":2}"#,
        "\n\n",
        r#"data: {"type":"response.output_item.done","output_index":0,"sequence_number":3,"item":{"type":"function_call","id":"fc_1","call_id":"call_a","name":"read","arguments":"{\"path\":\"/tmp/foo\"}"}}"#,
        "\n\n",
        r#"data: {"type":"response.completed","sequence_number":4,"response":{"id":"r","status":"completed"}}"#,
        "\n\n",
    );
    let events = run(raw);
    let deltas: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, ProviderEvent::ToolCallDelta { .. }))
        .collect();
    assert_eq!(deltas.len(), 2, "got: {events:?}");
    for e in deltas {
        if let ProviderEvent::ToolCallDelta { id, .. } = e {
            assert_eq!(id, "call_a");
        }
    }
    let done = events
        .iter()
        .find_map(|e| match e {
            ProviderEvent::Done(m) => Some(m),
            _ => None,
        })
        .expect("Done");
    assert_eq!(done.stop_reason, StopReason::ToolUse);
    assert_eq!(done.content.len(), 1);
    match &done.content[0] {
        ContentBlock::ToolCall(tc) => {
            assert_eq!(tc.id, "call_a"); // call_id, not item id
            assert_eq!(tc.name, "read");
            assert_eq!(tc.arguments.as_value()["path"], "/tmp/foo");
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
}

/// The Codex backend's `.arguments.delta` (dot) spelling must fold the
/// same way as the official underscore spelling.
#[test]
fn sse_tool_call_accumulation_codex_compat_spelling() {
    let raw = concat!(
        r#"data: {"type":"response.output_item.added","output_index":0,"sequence_number":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_a","name":"read","arguments":""}}"#,
        "\n\n",
        r#"data: {"type":"response.function_call.arguments.delta","output_index":0,"item_id":"fc_1","delta":"{\"path\":\"/x\"}"}"#,
        "\n\n",
        r#"data: {"type":"response.output_item.done","output_index":0,"sequence_number":3,"item":{"type":"function_call","id":"fc_1","call_id":"call_a","name":"read","arguments":"{\"path\":\"/x\"}"}}"#,
        "\n\n",
        r#"data: {"type":"response.completed","sequence_number":4,"response":{"id":"r","status":"completed"}}"#,
        "\n\n",
    );
    let events = run(raw);
    let done = events
        .iter()
        .find_map(|e| match e {
            ProviderEvent::Done(m) => Some(m),
            _ => None,
        })
        .expect("Done");
    assert_eq!(done.stop_reason, StopReason::ToolUse);
    match &done.content[0] {
        ContentBlock::ToolCall(tc) => {
            assert_eq!(tc.name, "read");
            assert_eq!(tc.arguments.as_value()["path"], "/x");
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
}

#[test]
fn sse_mixed_text_and_tool() {
    let raw = concat!(
        r#"data: {"type":"response.output_text.delta","delta":"reading ","sequence_number":0}"#,
        "\n\n",
        r#"data: {"type":"response.output_item.added","output_index":1,"sequence_number":1,"item":{"type":"function_call","id":"fc_2","call_id":"call_b","name":"read","arguments":""}}"#,
        "\n\n",
        r#"data: {"type":"response.function_call_arguments.delta","output_index":1,"item_id":"fc_2","delta":"{\"path\":\"/x\"}","sequence_number":2}"#,
        "\n\n",
        r#"data: {"type":"response.output_item.done","output_index":1,"sequence_number":3,"item":{"type":"function_call","id":"fc_2","call_id":"call_b","name":"read","arguments":"{\"path\":\"/x\"}"}}"#,
        "\n\n",
        r#"data: {"type":"response.completed","sequence_number":4,"response":{"id":"r","status":"completed"}}"#,
        "\n\n",
    );
    let events = run(raw);
    let done = events
        .iter()
        .find_map(|e| match e {
            ProviderEvent::Done(m) => Some(m),
            _ => None,
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
            assert_eq!(tc.arguments.as_value()["path"], "/x");
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
}

// ============================================================================
// Reasoning summary/text → ThinkingDelta
// ============================================================================

#[test]
fn reasoning_summary_text_emits_thinking_delta() {
    let raw = concat!(
        r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","output_index":0,"summary_index":0,"delta":"planning...","sequence_number":0}"#,
        "\n\n",
        r#"data: {"type":"response.output_text.delta","delta":"ok","sequence_number":1}"#,
        "\n\n",
        r#"data: {"type":"response.completed","sequence_number":2,"response":{"id":"r","status":"completed"}}"#,
        "\n\n",
    );
    let events = run(raw);
    assert!(events
        .iter()
        .any(|e| matches!(e, ProviderEvent::ThinkingDelta(t) if t == "planning...")));
    assert!(events
        .iter()
        .any(|e| matches!(e, ProviderEvent::TextDelta(t) if t == "ok")));
    assert!(events.iter().any(|e| matches!(e, ProviderEvent::Done(_))));
}

#[test]
fn reasoning_text_delta_emits_thinking_delta() {
    let raw = concat!(
        r#"data: {"type":"response.reasoning_text.delta","item_id":"rs_1","output_index":0,"delta":"chain","sequence_number":0}"#,
        "\n\n",
        r#"data: {"type":"response.completed","sequence_number":1,"response":{"id":"r","status":"completed"}}"#,
        "\n\n",
    );
    let events = run(raw);
    assert!(events
        .iter()
        .any(|e| matches!(e, ProviderEvent::ThinkingDelta(t) if t == "chain")));
}

// ============================================================================
// Failure / error events → ProviderEvent::Error
// ============================================================================

#[test]
fn failed_event_emits_error() {
    let raw = concat!(
        r#"data: {"type":"response.failed","sequence_number":0,"response":{"id":"r","status":"failed","error":{"message":"boom"}}}"#,
        "\n\n",
    );
    let events = run(raw);
    assert_eq!(events.len(), 1);
    match &events[0] {
        ProviderEvent::Error(m) => assert!(m.contains("boom")),
        other => panic!("expected Error, got {other:?}"),
    }
}

#[test]
fn response_error_event_emits_error() {
    let raw = concat!(
        r#"data: {"type":"response.error","code":"rate_limit","message":"slow","sequence_number":0}"#,
        "\n\n",
    );
    let events = run(raw);
    assert_eq!(events.len(), 1);
    match &events[0] {
        ProviderEvent::Error(m) => assert!(m.contains("slow")),
        other => panic!("expected Error, got {other:?}"),
    }
}

// ============================================================================
// Cancel mid-stream → Done(Aborted)
// ============================================================================

#[tokio::test]
async fn cancel_mid_stream_yields_aborted() {
    let raw = concat!(
        r#"data: {"type":"response.output_text.delta","delta":"partial","sequence_number":0}"#,
        "\n\n",
    );
    let bytes = futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::copy_from_slice(
        raw.as_bytes(),
    ))]);
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut stream = test_events_stream(bytes, rx);

    let e0 = stream.next().await.expect("first event");
    match e0 {
        ProviderEvent::TextDelta(t) => assert_eq!(t, "partial"),
        other => panic!("expected TextDelta, got {other:?}"),
    }
    let _ = tx.send(());
    let remaining: Vec<_> = stream.collect().await;
    assert_eq!(remaining.len(), 1);
    let done = match &remaining[0] {
        ProviderEvent::Done(m) => m,
        other => panic!("expected Done, got {other:?}"),
    };
    assert!(matches!(
        done.stop_reason,
        StopReason::Aborted | StopReason::EndTurn
    ));
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
    let (_tx, rx) = tokio::sync::oneshot::channel();
    let mut state = new_stream_state(Box::pin(futures::stream::empty()), rx);
    state.accumulated_text = "partial".into();
    state.final_status = Some(ResponseStatus::Incomplete);
    state.incomplete_reason = Some("max_output_tokens".into());
    assert_eq!(map_stop(&state), StopReason::MaxTokens);
    // Even without final_status set, incomplete_reason wins.
    state.final_status = None;
    assert_eq!(map_stop(&state), StopReason::MaxTokens);
}

// ============================================================================
// Legacy vs Projected parity
//
// Each test runs the SAME scenario through both wire-body builders and
// asserts byte-equal `serde_json::Value`. The Projected projection comes
// from FauxProviderRenderer::render(&assemble_rope(...), AgentView).
// ============================================================================

const PARITY_DEFAULT_INSTRUCTIONS: &str = "you are a test";

fn parity_compare(system_prompt: Option<&str>, messages: &[AgentMessage], tools: &[ToolSpec]) {
    use mu_core::context::{
        assemble_rope, FauxProviderRenderer, ProjectionTarget, ProviderRenderer,
    };

    let resolved_instructions = system_prompt
        .filter(|s| !s.is_empty())
        .unwrap_or(PARITY_DEFAULT_INSTRUCTIONS);
    let legacy = build_request_value(
        "gpt-5-codex",
        "high",
        resolved_instructions,
        messages,
        tools,
    );

    let rope = assemble_rope(system_prompt, messages, tools);
    let projection = FauxProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
    let projected = build_request_value_from_projection(
        "gpt-5-codex",
        "high",
        PARITY_DEFAULT_INSTRUCTIONS,
        &projection,
        tools,
    );

    assert_eq!(
        legacy, projected,
        "Legacy vs Projected wire body diverged.\nLegacy:    {legacy:#}\nProjected: {projected:#}",
    );
}

fn dummy_tool() -> ToolSpec {
    ToolSpec {
        name: "noop".into(),
        description: "no-op".into(),
        input_schema: json!({"type": "object"}),
        display: None,
        when: None,
        policy: Default::default(),

        ..Default::default()
    }
}

#[test]
fn parity_pure_text_turn() {
    let messages = vec![
        AgentMessage::User {
            content: "hi".into(),
        },
        AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: None,
        }),
    ];
    parity_compare(None, &messages, &[dummy_tool()]);
}

#[test]
fn parity_single_tool_call() {
    let tool = ToolSpec {
        name: "read".into(),
        description: "read a file".into(),
        input_schema: json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
        }),
        policy: Default::default(),

        ..Default::default()
    };
    let messages = vec![
        AgentMessage::User {
            content: "what's in /tmp/x?".into(),
        },
        AgentMessage::Assistant(AssistantMessage {
            content: vec![
                ContentBlock::Text {
                    text: "I'll read it.".into(),
                },
                ContentBlock::ToolCall(ToolCall {
                    id: "call_42".into(),
                    name: "read".into(),
                    arguments: ToolArgs::new(json!({"path": "/tmp/x"})).unwrap(),
                }),
            ],
            stop_reason: StopReason::ToolUse,
            usage: None,
        }),
        AgentMessage::ToolResult {
            call_id: "call_42".into(),
            content: "contents".into(),
            is_error: false,
        },
        AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::Text {
                text: "it says contents".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: None,
        }),
    ];
    parity_compare(None, &messages, std::slice::from_ref(&tool));
}

#[test]
fn parity_consecutive_tool_results() {
    // The OpenAI Responses API emits each ToolResult as a separate
    // function_call_output item (no Anthropic-style grouping).
    let messages = vec![
        AgentMessage::Assistant(AssistantMessage {
            content: vec![
                ContentBlock::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    arguments: ToolArgs::new(json!({"path": "/a"})).unwrap(),
                }),
                ContentBlock::ToolCall(ToolCall {
                    id: "call_2".into(),
                    name: "read".into(),
                    arguments: ToolArgs::new(json!({"path": "/b"})).unwrap(),
                }),
                ContentBlock::ToolCall(ToolCall {
                    id: "call_3".into(),
                    name: "read".into(),
                    arguments: ToolArgs::new(json!({"path": "/c"})).unwrap(),
                }),
            ],
            stop_reason: StopReason::ToolUse,
            usage: None,
        }),
        AgentMessage::ToolResult {
            call_id: "call_1".into(),
            content: "a-contents".into(),
            is_error: false,
        },
        AgentMessage::ToolResult {
            call_id: "call_2".into(),
            content: "b-contents".into(),
            is_error: true,
        },
        AgentMessage::ToolResult {
            call_id: "call_3".into(),
            content: "c-contents".into(),
            is_error: false,
        },
    ];
    parity_compare(None, &messages, &[dummy_tool()]);
}

#[test]
fn parity_system_prompt_plus_tools() {
    let tools = vec![
        ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            input_schema: json!({"type": "object"}),
            display: None,
            when: None,
            policy: Default::default(),

            ..Default::default()
        },
        ToolSpec {
            name: "bash".into(),
            description: "run shell".into(),
            input_schema: json!({"type": "object"}),
            display: None,
            when: None,
            policy: Default::default(),

            ..Default::default()
        },
    ];
    let messages = vec![AgentMessage::User {
        content: "list files".into(),
    }];
    parity_compare(Some("you are a helpful assistant"), &messages, &tools);
}

#[test]
fn parity_thinking_blocks_skipped() {
    let messages = vec![AgentMessage::Assistant(AssistantMessage {
        content: vec![
            ContentBlock::Thinking {
                text: "INTERNAL_REASONING_DO_NOT_LEAK".into(),
            },
            ContentBlock::Text {
                text: "public answer".into(),
            },
        ],
        stop_reason: StopReason::EndTurn,
        usage: None,
    })];
    parity_compare(None, &messages, &[dummy_tool()]);
}

#[test]
fn projected_hoists_memory_and_file_into_instructions() {
    // mu-2puu regression: memory-recall:* and project-file:* spans must
    // be hoisted into body.instructions (the Responses API has one
    // instructions slot), not silently dropped.
    use mu_core::context::{
        assemble_rope_with_context, FauxProviderRenderer, ProjectContext, ProjectionTarget,
        ProviderRenderer, RecallSource, RecalledItem,
    };
    use std::path::PathBuf;

    let memory_blob = "## Active Memory Context\n\nfavorite color is 'cat'";
    let claude_md_text = "# CLAUDE.md\n\nuse SpanText for content fields.";

    let project_context = ProjectContext {
        items: vec![
            RecalledItem {
                source: RecallSource::Memory,
                content: memory_blob.into(),
                stable_id: "abc123".into(),
            },
            RecalledItem {
                source: RecallSource::ProjectFile {
                    path: PathBuf::from("/home/u/CLAUDE.md"),
                },
                content: claude_md_text.into(),
                stable_id: "def456".into(),
            },
        ],
    };

    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let rope =
        assemble_rope_with_context(Some("you are mu"), Some(&project_context), &messages, &[]);
    let projection = FauxProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
    let body = build_request_value_from_projection(
        "gpt-5-codex",
        "high",
        "fallback default instructions",
        &projection,
        &[],
    );

    let instructions = body
        .get("instructions")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(instructions.contains("you are mu"), "{instructions:?}");
    assert!(
        instructions.contains("favorite color is 'cat'"),
        "{instructions:?}"
    );
    assert!(
        instructions.contains("use SpanText for content fields."),
        "{instructions:?}"
    );
    assert!(
        !instructions.contains("fallback default instructions"),
        "{instructions:?}"
    );
}

#[test]
fn projected_excludes_tool_schema_from_instructions() {
    use mu_core::context::{
        assemble_rope, FauxProviderRenderer, ProjectionTarget, ProviderRenderer,
    };

    let tools = vec![ToolSpec {
        name: "read".into(),
        description: "read a file".into(),
        input_schema: json!({"type": "object"}),
        display: None,
        when: None,
        policy: Default::default(),

        ..Default::default()
    }];
    let messages = vec![AgentMessage::User {
        content: "go".into(),
    }];
    let rope = assemble_rope(Some("system-only-text"), &messages, &tools);
    let projection = FauxProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
    let body =
        build_request_value_from_projection("gpt-5-codex", "high", "fallback", &projection, &tools);

    let instructions = body
        .get("instructions")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        instructions.contains("system-only-text"),
        "{instructions:?}"
    );
    assert!(!instructions.contains("read a file"), "{instructions:?}");
}

// ============================================================================
// mu-rb4u: 429 / usage-limit error rendering
// ============================================================================

#[test]
fn render_http_error_surfaces_usage_limit() {
    let body = r#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","plan_type":"prolite","resets_at":1782114418,"resets_in_seconds":3501}}"#;
    let msg = render_codex_http_error(reqwest::StatusCode::TOO_MANY_REQUESTS, body);
    assert!(msg.contains("usage limit reached"), "got: {msg}");
    assert!(msg.contains("prolite"), "got: {msg}");
    assert!(msg.contains("58m21s"), "got: {msg}");
    assert!(!msg.contains("resets_in_seconds"), "got: {msg}");
}

#[test]
fn render_http_error_generic_429_and_other_status() {
    let body = r#"{"error":{"type":"rate_limit_exceeded","message":"slow down"}}"#;
    let msg = render_codex_http_error(reqwest::StatusCode::TOO_MANY_REQUESTS, body);
    assert!(msg.contains("rate limited (429)"), "got: {msg}");
    assert!(msg.contains("slow down"), "got: {msg}");

    let msg = render_codex_http_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR, "boom");
    assert!(msg.contains("500"), "got: {msg}");
    assert!(msg.contains("boom"), "got: {msg}");
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

    #[tokio::test]
    async fn live_codex_text_smoke() {
        if !live_enabled() {
            eprintln!("skipping live_codex_text_smoke (set MU_LIVE_OPENAI_CODEX=1)");
            return;
        }
        let provider = OpenaiProvider::from_store("gpt-5-codex".into())
            .expect("must be logged in via `mu login --provider openai-codex`");
        let messages = vec![AgentMessage::User {
            content: "Reply with the single word 'hello' and nothing else.".into(),
        }];
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let mut stream = provider
            .stream(None, None, MessageInput::Legacy(&messages), &[], rx)
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
        assert!(
            text.to_lowercase().contains("hello"),
            "expected 'hello', got: {text:?}"
        );
    }

    #[tokio::test]
    async fn live_codex_tool_call() {
        if !live_enabled() {
            eprintln!("skipping live_codex_tool_call (set MU_LIVE_OPENAI_CODEX=1)");
            return;
        }
        let provider = OpenaiProvider::from_store("gpt-5-codex".into()).expect("must be logged in");
        let echo_tool = ToolSpec {
            name: "echo".into(),
            description: "Echo a string.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"],
            }),
            policy: Default::default(),

            ..Default::default()
        };
        let messages = vec![AgentMessage::User {
            content: "Use the echo tool with text='hi there'. Just call the tool.".into(),
        }];
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let mut stream = provider
            .stream(
                None,
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
        let tc = done
            .content
            .iter()
            .find_map(|b| match b {
                ContentBlock::ToolCall(tc) => Some(tc),
                _ => None,
            })
            .expect("expected a ToolCall");
        assert_eq!(tc.name, "echo");
        assert!(tc.arguments.as_value().is_object());
    }
}
