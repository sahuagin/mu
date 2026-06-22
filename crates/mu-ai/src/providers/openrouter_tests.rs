// Fixture builders specify every field then add `..Default::default()`; the
// trailing update is harmless test noise, not worth churning each literal.
#![allow(clippy::needless_update)]

use super::*;
use bytes::Bytes;
use futures::StreamExt;
use mu_core::agent::{
    AgentMessage, AssistantMessage, ContentBlock, MessageInput, StopReason, ToolArgs, ToolCall,
};
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
                arguments: ToolArgs::new(json!({"path": "/tmp/foo"})).unwrap(),
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

        ..Default::default()
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
        display: None,
        when: None,
        policy: Default::default(),

        ..Default::default()
    }];
    let body = build_request_body("test/model", None, &messages, &tools);
    assert_eq!(body["model"], "test/model");
    assert_eq!(body["stream"], true);
    // Unknown model name falls back to the conservative 4096.
    assert_eq!(body["max_tokens"], 4096);
    assert_eq!(body["tools"][0]["function"]["name"], "read");
    assert_eq!(body["messages"][0]["role"], "user");
}

#[test]
fn b6c_build_request_body_max_tokens_is_model_aware() {
    // mu-ql2: real-model identifiers get their per-family ceiling.
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let gpt5 = build_request_body("gpt-5", None, &messages, &[]);
    assert_eq!(gpt5["max_tokens"], 16384);
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

// ============================================================================
// mu-yqeq.6 parity tests
//
// Each test runs the SAME scenario through both wire-body builders:
//   - Legacy:    build_request_body(model, system_prompt, &[AgentMessage], tools)
//   - Projected: build_request_body_from_projection(model, &ProviderMessages, tools)
//                where ProviderMessages comes from FauxProviderRenderer::render
//                over assemble_rope(system_prompt, messages, tools).
//
// Byte-equality on the two `serde_json::Value` outputs is the contract.
// Phase D (mu-yqeq.8) flips the call site at mod.rs:818 from Legacy to
// Projected; if these parity tests pass, the cutover is observably
// equivalent at the wire layer.
// ============================================================================

fn parity_compare(
    system_prompt: Option<&str>,
    messages: &[AgentMessage],
    tools: &[mu_core::agent::ToolSpec],
) {
    use mu_core::context::{
        assemble_rope, FauxProviderRenderer, ProjectionTarget, ProviderRenderer,
    };

    let legacy = build_request_body("openrouter-test-model", system_prompt, messages, tools);

    let rope = assemble_rope(system_prompt, messages, tools);
    let projection = FauxProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
    let projected = build_request_body_from_projection("openrouter-test-model", &projection, tools);

    assert_eq!(
        legacy, projected,
        "Legacy vs Projected wire body diverged.\nLegacy:    {legacy:#}\nProjected: {projected:#}",
    );
}

#[test]
fn yqeq6_parity_pure_text_turn() {
    // User → Assistant text, no tool calls. Dummy tool supplied so
    // mu-0q44's no-tools clause doesn't fire (intentional Legacy vs
    // Projected divergence when tools is empty).
    let dummy = mu_core::agent::ToolSpec {
        name: "noop".into(),
        description: "no-op".into(),
        input_schema: json!({"type": "object"}),
        display: None,
        when: None,
        policy: Default::default(),

        ..Default::default()
    };
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
    parity_compare(None, &messages, &[dummy]);
}

#[test]
fn yqeq6_parity_single_tool_call() {
    // User → Assistant(text + ToolCall) → ToolResult → Assistant text.
    // Exercises the OpenRouter assistant-message shape: a single
    // message with both `content` text AND a `tool_calls` array; tool
    // results become separate `{role: "tool", ...}` messages.
    let tool = mu_core::agent::ToolSpec {
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
fn yqeq6_parity_consecutive_tool_results() {
    // Three back-to-back ToolResults. Unlike Anthropic (which groups
    // these into a single user message), OpenRouter (OpenAI
    // chat-completions) emits each as a separate `{role: "tool", ...}`
    // message. The Projected path must preserve that lack of grouping.
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
    // Dummy tool: mu-0q44's no-tools clause diverges Legacy vs Projected.
    let dummy = mu_core::agent::ToolSpec {
        name: "noop".into(),
        description: "no-op".into(),
        input_schema: json!({"type": "object"}),
        display: None,
        when: None,
        policy: Default::default(),

        ..Default::default()
    };
    parity_compare(None, &messages, &[dummy]);
}

#[test]
fn mu_745h_projected_concatenates_memory_injection_and_file_load_into_leading_system_msg() {
    // mu-745h regression test. mu-phl v0 introduced MemoryInjection +
    // FileLoad spans into the rope via assemble_rope_with_context.
    // OpenRouter's Projected arm must include their content in the
    // leading role=system message (Chat Completions API has one
    // canonical system slot; multiple system spans concat there).
    //
    // Pre-fix this test failed: translate_provider_messages_openrouter
    // only emitted the span with id literally "system-prompt" and
    // silently dropped memory-recall:* + project-file:* spans.
    //
    // Codex sibling: mu_2puu_projected_hoists_memory_injection_and_file_load_into_instructions.
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
    let body = build_request_body_from_projection("test-or-model", &projection, &[]);

    let messages_arr = body.get("messages").and_then(|v| v.as_array()).unwrap();
    // Leading message must be role=system.
    let leading = &messages_arr[0];
    assert_eq!(leading["role"], "system");
    let system_content = leading["content"].as_str().unwrap_or("");

    // System prompt content
    assert!(
        system_content.contains("you are mu"),
        "system prompt missing from leading system message: {system_content:?}",
    );
    // Memory recall content
    assert!(
        system_content.contains("favorite color is 'cat'"),
        "memory-recall content missing: {system_content:?}",
    );
    // Project-file content
    assert!(
        system_content.contains("use SpanText for content fields."),
        "project-file content missing: {system_content:?}",
    );

    // Only one leading system message (concatenated, not duplicated).
    let system_count = messages_arr
        .iter()
        .filter(|m| m.get("role").and_then(|v| v.as_str()) == Some("system"))
        .count();
    assert_eq!(system_count, 1, "expected exactly one leading system msg");
}

#[test]
fn mu_745h_projected_excludes_tool_schema_from_leading_system_msg() {
    // Companion to the regression above: tool-schema spans also map
    // to ProviderRole::System (per renderer.rs From<&SpanKind>), but
    // they MUST NOT land in the leading system message — tools go
    // separately via body.tools.
    use mu_core::context::{
        assemble_rope, FauxProviderRenderer, ProjectionTarget, ProviderRenderer,
    };

    let tools = vec![mu_core::agent::ToolSpec {
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
    let body = build_request_body_from_projection("test-or-model", &projection, &tools);

    let messages_arr = body.get("messages").and_then(|v| v.as_array()).unwrap();
    let leading = &messages_arr[0];
    assert_eq!(leading["role"], "system");
    let system_content = leading["content"].as_str().unwrap_or("");
    assert!(
        system_content.contains("system-only-text"),
        "system prompt missing: {system_content:?}",
    );
    // Tool-schema content (description, JSON schema) MUST NOT appear
    // in the system message.
    assert!(
        !system_content.contains("read a file"),
        "tool-schema content leaked into system msg: {system_content:?}",
    );
}

#[test]
fn yqeq6_parity_system_prompt_plus_tools() {
    // System prompt + multiple tools. Exercises:
    //   - the mu-n48 prepend path: `{role: "system", content: ...}`
    //     emitted as the FIRST message in the array.
    //   - the tools-present body shape with nested
    //     `{type: function, function: {...}}` tool specs.
    let tools = vec![
        mu_core::agent::ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            input_schema: json!({"type": "object"}),
            display: None,
            when: None,
            policy: Default::default(),

            ..Default::default()
        },
        mu_core::agent::ToolSpec {
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
fn yqeq6_thinking_blocks_are_skipped_in_projected_wire_output() {
    // Spec mu-044 §"Thinking-block skip": Projected wire emission
    // MUST NOT echo the model's reasoning trace back as input.
    // Mirrors the Legacy `translate_message` behavior
    // (openrouter.rs:150-153 filters Thinking blocks).
    use mu_core::context::{
        assemble_rope, FauxProviderRenderer, ProjectionTarget, ProviderRenderer,
    };

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
    let rope = assemble_rope(None, &messages, &[]);
    let projection = FauxProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
    let projected = build_request_body_from_projection("openrouter-test-model", &projection, &[]);

    let wire = serde_json::to_string(&projected).expect("serialize");
    assert!(
        !wire.contains("INTERNAL_REASONING_DO_NOT_LEAK"),
        "Thinking block content leaked to wire: {wire}",
    );
    assert!(
        wire.contains("public answer"),
        "non-thinking text was lost: {wire}",
    );

    // Also: parity vs Legacy (which also strips thinking). Dummy tool
    // avoids mu-0q44 no-tools clause divergence.
    let dummy = mu_core::agent::ToolSpec {
        name: "noop".into(),
        description: "no-op".into(),
        input_schema: json!({"type": "object"}),
        display: None,
        when: None,
        policy: Default::default(),

        ..Default::default()
    };
    parity_compare(None, &messages, &[dummy]);
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
        accumulated_thinking: String::new(),
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
async fn sse_reasoning_then_text_captures_thinking_mu_mdds() {
    // mu-mdds: a thinking model (ollama OpenAI-compat) streams its
    // reasoning on the `reasoning` delta field, then the answer on
    // `content`. Before the fix, serde dropped `reasoning` → empty
    // visible response. Now reasoning surfaces as ThinkingDelta and
    // rides into the final message as a Thinking block (before Text).
    let raw = concat!(
        r#"data: {"choices":[{"delta":{"reasoning":"let me think"}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{"reasoning":" about it"}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{"content":"the answer"}}]}"#,
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

    // 2 ThinkingDelta + 1 TextDelta + 1 Done.
    assert_eq!(events.len(), 4, "got {events:?}");
    match &events[0] {
        ProviderEvent::ThinkingDelta(t) => assert_eq!(t, "let me think"),
        other => panic!("expected ThinkingDelta, got {other:?}"),
    }
    match &events[1] {
        ProviderEvent::ThinkingDelta(t) => assert_eq!(t, " about it"),
        other => panic!("expected ThinkingDelta, got {other:?}"),
    }
    match &events[2] {
        ProviderEvent::TextDelta(t) => assert_eq!(t, "the answer"),
        other => panic!("expected TextDelta, got {other:?}"),
    }
    match &events[3] {
        ProviderEvent::Done(msg) => {
            assert_eq!(msg.stop_reason, StopReason::EndTurn);
            // Thinking block first, then Text.
            assert_eq!(msg.content.len(), 2, "got {:?}", msg.content);
            match &msg.content[0] {
                ContentBlock::Thinking { text } => {
                    assert_eq!(text.as_ref(), "let me think about it")
                }
                other => panic!("expected Thinking first, got {other:?}"),
            }
            match &msg.content[1] {
                ContentBlock::Text { text } => assert_eq!(text.as_ref(), "the answer"),
                other => panic!("expected Text second, got {other:?}"),
            }
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

// mu-13ve: effort -> OpenRouter `reasoning` field mapping.
#[test]
fn reasoning_param_none_when_effort_absent_or_off() {
    // None, "off", and "" → no reasoning key (byte-identical request).
    assert_eq!(reasoning_param(None), None);
    assert_eq!(reasoning_param(Some("off")), None);
    assert_eq!(reasoning_param(Some("")), None);
    // Unrecognized values are dropped, not forwarded raw.
    assert_eq!(reasoning_param(Some("turbo")), None);
}

#[test]
fn reasoning_param_maps_levels() {
    assert_eq!(reasoning_param(Some("low")), Some(json!({"effort": "low"})));
    assert_eq!(
        reasoning_param(Some("medium")),
        Some(json!({"effort": "medium"}))
    );
    assert_eq!(
        reasoning_param(Some("high")),
        Some(json!({"effort": "high"}))
    );
}

#[test]
fn reasoning_param_clamps_above_high_to_high() {
    // mu's xhigh/max have no OpenRouter level above `high`.
    assert_eq!(
        reasoning_param(Some("xhigh")),
        Some(json!({"effort": "high"}))
    );
    assert_eq!(
        reasoning_param(Some("max")),
        Some(json!({"effort": "high"}))
    );
}

#[test]
fn reasoning_param_is_case_and_whitespace_insensitive() {
    assert_eq!(
        reasoning_param(Some(" HIGH ")),
        Some(json!({"effort": "high"}))
    );
    assert_eq!(
        reasoning_param(Some("Medium")),
        Some(json!({"effort": "medium"}))
    );
}

// mu-xblz: dialect rescue is wired into the OpenRouter Done event.
#[tokio::test]
async fn dialect_rescue_rewrites_leaked_tool_call_on_done() {
    let tools = vec![ToolSpec {
        name: "read".into(),
        description: "Read".into(),
        input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        ..Default::default()
    }];
    // A turn that "ended" as prose but is really a training-native XML tool call.
    let leaked = AssistantMessage {
        content: vec![ContentBlock::Text {
            text: "<function=read>\n<parameter=path>Cargo.toml</parameter>\n</function>".into(),
        }],
        stop_reason: StopReason::EndTurn,
        usage: None,
    };
    let input = futures::stream::iter(vec![ProviderEvent::Done(leaked)]).boxed();
    let mut out = apply_dialect_rescue(input, &tools);
    match out.next().await.expect("one event") {
        ProviderEvent::Done(msg) => {
            assert_eq!(msg.stop_reason, StopReason::ToolUse);
            let calls = msg
                .content
                .iter()
                .filter(|b| matches!(b, ContentBlock::ToolCall(_)))
                .count();
            assert_eq!(calls, 1, "leaked dialect should become one tool call");
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test]
async fn dialect_rescue_passes_non_done_events_through() {
    let tools: Vec<ToolSpec> = vec![];
    let input = futures::stream::iter(vec![
        ProviderEvent::TextDelta("hello".into()),
        ProviderEvent::ThinkingDelta("hmm".into()),
    ])
    .boxed();
    let mut out = apply_dialect_rescue(input, &tools);
    assert!(matches!(out.next().await, Some(ProviderEvent::TextDelta(t)) if t == "hello"));
    assert!(matches!(out.next().await, Some(ProviderEvent::ThinkingDelta(t)) if t == "hmm"));
    assert!(out.next().await.is_none());
}

// mu-y8gp: per-model sampling from the catalog.
#[test]
fn sampling_for_model_resolves_catalog_temperature_top_p() {
    use mu_core::model_catalog::{ModelCatalogConfig, ModelCatalogEntry};
    let mut catalog = ModelCatalogConfig::default();
    catalog.models.insert(
        "glm".to_string(),
        ModelCatalogEntry {
            model: Some("z-ai/glm-5.2".to_string()),
            temperature: Some(0.6),
            top_p: Some(0.95),
            ..Default::default()
        },
    );
    assert_eq!(
        sampling_for_model_with_catalog(&catalog, "z-ai/glm-5.2"),
        (Some(0.6), Some(0.95))
    );
    // A model with no catalog entry → no sampling (provider default).
    assert_eq!(
        sampling_for_model_with_catalog(&catalog, "openai/gpt-5.5"),
        (None, None)
    );
}

#[test]
fn inject_sampling_adds_fields_only_when_set() {
    // All-None leaves the body untouched (byte-for-byte parity).
    let mut body = json!({"model": "m"});
    inject_sampling(&mut body, None, None);
    assert_eq!(body, json!({"model": "m"}));
    // Both set → JSON numbers.
    let mut body = json!({"model": "m"});
    inject_sampling(&mut body, Some(0.6), Some(0.95));
    assert_eq!(body["temperature"], json!(0.6));
    assert_eq!(body["top_p"], json!(0.95));
    // Only temperature → no top_p key.
    let mut body = json!({});
    inject_sampling(&mut body, Some(0.2), None);
    assert_eq!(body["temperature"], json!(0.2));
    assert!(body.get("top_p").is_none());
}

#[test]
fn clamp_sampling_bounds_and_drops_non_finite() {
    assert_eq!(clamp_sampling(Some(0.6), 0.0, 2.0), Some(0.6)); // in range
    assert_eq!(clamp_sampling(Some(5.0), 0.0, 2.0), Some(2.0)); // clamp high
    assert_eq!(clamp_sampling(Some(-1.0), 0.0, 1.0), Some(0.0)); // clamp low
    assert_eq!(clamp_sampling(Some(f64::NAN), 0.0, 2.0), None); // drop NaN
    assert_eq!(clamp_sampling(Some(f64::INFINITY), 0.0, 2.0), None); // drop Inf
    assert_eq!(clamp_sampling(None, 0.0, 2.0), None);
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
                ContentBlock::Text { text } => assert_eq!(text.as_ref(), "hello world"),
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
            assert_eq!(tc.arguments.as_value()["path"], "/tmp/foo");
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
        ContentBlock::Text { text } => assert_eq!(text.as_ref(), "I will read it. "),
        other => panic!("expected Text, got {other:?}"),
    }
    match &done.content[1] {
        ContentBlock::ToolCall(tc) => {
            assert_eq!(tc.id, "call_b");
            assert_eq!(tc.name, "read");
            assert_eq!(tc.arguments.as_value()["path"], "/x");
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
            assert!(tc.arguments.as_value().is_object());
            assert_eq!(tc.arguments.as_value().as_object().unwrap().len(), 0);
        }
        _ => panic!("expected ToolCall"),
    }
}

// ============================================================================
// Base/path override (mu-spawn: OpenAI-compatible local backends, e.g. ollama)
// ============================================================================

#[test]
fn local_backend_overrides_base_and_path() {
    // Default points at OpenRouter with the nested /api/v1 path.
    let def = OpenRouterProvider::new("k".into(), "m".into());
    assert_eq!(def.api_base, "https://openrouter.ai");
    assert_eq!(def.api_path, "/api/v1/chat/completions");

    // Pointed at a local ollama box: bare /v1 path, no /api prefix.
    let local = OpenRouterProvider::new("local-nokey".into(), "qwen3-coder:30b".into())
        .with_api_base("http://10.1.1.143:11434".into())
        .with_api_path("/v1/chat/completions".into());
    assert_eq!(local.api_base, "http://10.1.1.143:11434");
    assert_eq!(local.api_path, "/v1/chat/completions");
    // The composed URL is the ollama OpenAI-compatible endpoint.
    assert_eq!(
        format!("{}{}", local.api_base, local.api_path),
        "http://10.1.1.143:11434/v1/chat/completions"
    );
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
            .stream(None, None, MessageInput::Legacy(&messages), &[], rx)
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
        assert_eq!(text.as_str(), content_text.as_ref());
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

            ..Default::default()
        };

        let messages = vec![AgentMessage::User {
            content: "Use the echo tool with text='hi there'. Just call the tool, no preamble."
                .into(),
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
        assert!(tool_call.arguments.as_value().is_object());
        let text_arg = tool_call.arguments.as_value()["text"]
            .as_str()
            .unwrap_or("");
        assert!(
            text_arg.to_lowercase().contains("hi"),
            "expected text arg to contain 'hi', got: {text_arg:?}"
        );
    }
}
