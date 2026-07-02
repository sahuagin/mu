// Fixture builders specify every field then add `..Default::default()`; the
// trailing update is harmless test noise, not worth churning each literal.
#![allow(clippy::needless_update)]

use super::*;
use mu_core::agent::{MessageInput, ToolArgs, ToolCall};
use mu_core::context::CacheTtl;
use mu_core::context::{
    assemble_rope, CacheMarker, CacheStrategy, ProjectionTarget, ProviderRenderer, ProviderRole,
    SpanKind,
};

// Shims preserving the old `Value`-returning test interface, now backed by the
// typed mu_anthropic mapping. The shape assertions below therefore double as a
// check that the typed path serializes to the same wire JSON.
fn translate_message_single(m: &AgentMessage) -> Option<Value> {
    map_agent_message_single(m).map(|am| serde_json::to_value(&am).unwrap())
}
fn translate_messages(messages: &[AgentMessage]) -> Vec<Value> {
    map_agent_messages(messages)
        .iter()
        .map(|am| serde_json::to_value(am).unwrap())
        .collect()
}
fn translate_tool_spec(spec: &ToolSpec) -> Value {
    serde_json::to_value(map_tool_spec(spec, None)).unwrap()
}

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
    // mu-ql2: real-model identifiers get their catalog max_output so longer
    // responses aren't prematurely truncated. opus-4-7 = 128000 (Anthropic's
    // queried extended-output ceiling, set in models.default.toml).
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let opus = build_request_body_with_catalog(
        &mu_core::model_catalog::built_in(),
        "claude-opus-4-7",
        None,
        &messages,
        &[],
    );
    assert_eq!(opus["max_tokens"], 128000);
    let haiku = build_request_body_with_catalog(
        &mu_core::model_catalog::built_in(),
        "claude-haiku-4-5",
        None,
        &messages,
        &[],
    );
    assert_eq!(haiku["max_tokens"], 8192);
}

#[test]
fn b1_translate_tool_spec_shape() {
    let spec = ToolSpec {
        name: "read".into(),
        description: "Read a file".into(),
        input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        policy: Default::default(),

        ..Default::default()
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
        display: None,
        when: None,
        policy: Default::default(),

        ..Default::default()
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

#[test]
fn apply_thinking_none_is_noop() {
    let mut body = build_request_body("claude-test", None, &[], &[]);
    let before = body.clone();
    apply_thinking(&mut body, None);
    assert_eq!(body, before, "None must not touch the wire body");
    assert!(body.get("thinking").is_none());
    assert!(body.get("output_config").is_none());
}

#[test]
fn apply_thinking_sets_adaptive_summarized_and_effort() {
    // The modern Claude shape: adaptive + summarized display + output_config.effort.
    // (display:summarized is required for readable reasoning; enabled+budget 400s
    // on Opus 4.6+/Fable.)
    let mut body = build_request_body("claude-opus-4-8", None, &[], &[]);
    let base_max = body["max_tokens"].clone();
    apply_thinking(&mut body, Some("high"));
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert_eq!(body["thinking"]["display"], "summarized");
    assert_eq!(body["output_config"]["effort"], "high");
    // No budget any more → max_tokens is untouched.
    assert_eq!(body["max_tokens"], base_max);
}

#[test]
fn apply_thinking_preserves_existing_output_config_fields() {
    let mut body = build_request_body("claude-opus-4-8", None, &[], &[]);
    body["output_config"] = serde_json::json!({ "verbosity": "low" });
    apply_thinking(&mut body, Some("max"));
    assert_eq!(body["output_config"]["verbosity"], "low", "kept");
    assert_eq!(body["output_config"]["effort"], "max", "added");
}

#[test]
fn parse_thinking_flag_maps_to_effort_levels() {
    assert_eq!(parse_thinking_flag(""), None);
    assert_eq!(parse_thinking_flag("   "), None);
    // off/none/false/0/disabled → no thinking.
    for off in ["off", "none", "false", "0", "disabled"] {
        assert_eq!(parse_thinking_flag(off), None, "{off}");
    }
    assert_eq!(parse_thinking_flag("minimal").as_deref(), Some("low"));
    assert_eq!(parse_thinking_flag("low").as_deref(), Some("low"));
    assert_eq!(parse_thinking_flag("medium").as_deref(), Some("medium"));
    assert_eq!(parse_thinking_flag("high").as_deref(), Some("high"));
    assert_eq!(parse_thinking_flag("HIGH").as_deref(), Some("high"), "ci");
    assert_eq!(parse_thinking_flag("xhigh").as_deref(), Some("xhigh"));
    assert_eq!(parse_thinking_flag("max").as_deref(), Some("max"));
    // Unrecognized non-empty → high, not silently nothing.
    assert_eq!(parse_thinking_flag("banana").as_deref(), Some("high"));
    assert_eq!(parse_thinking_flag("8000").as_deref(), Some("high"));
}

// mu-yqeq.8 retired the unconditional cache_control emission from
// the Legacy build_request_body. AnthropicCacheStrategy is now the
// sole source; the projected wire emitter propagates per-message
// cache_marker flags. The Legacy path no longer caches at all —
// preserved only for rollback and out-of-loop callers.

#[test]
fn yqeq8_legacy_build_request_body_emits_no_cache_control() {
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let tools = vec![
        ToolSpec {
            name: "read".into(),
            description: "Read a file".into(),
            input_schema: json!({ "type": "object" }),
            display: None,
            when: None,
            policy: Default::default(),

            ..Default::default()
        },
        ToolSpec {
            name: "grep".into(),
            description: "Search".into(),
            input_schema: json!({ "type": "object" }),
            display: None,
            when: None,
            policy: Default::default(),

            ..Default::default()
        },
    ];
    let body = build_request_body("claude-test", Some("be concise"), &messages, &tools);

    let arr = body["system"].as_array().expect("system is array");
    assert_eq!(arr[0]["text"], "be concise");
    assert!(
        arr[0].get("cache_control").is_none(),
        "Legacy path must not emit cache_control on body.system",
    );

    let tool_arr = body["tools"].as_array().expect("tools array");
    for (i, tool) in tool_arr.iter().enumerate() {
        assert!(
            tool.get("cache_control").is_none(),
            "Legacy path must not emit cache_control on body.tools[{i}]",
        );
    }
}

// mu-n48: system prompt rendered as content-block array. The
// content-block shape is preserved post-mu-yqeq.8 (cache_control
// emission is now strategy-driven, not unconditional).

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
fn mu_n48_system_prompt_set_emits_content_block() {
    // mu-yqeq.8: cache_control is no longer unconditional here —
    // the content-block shape is preserved (text + type) but
    // caching is driven by the strategy on the Projected path.
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let body = build_request_body("claude-test", Some("you are concise"), &messages, &[]);
    let arr = body["system"].as_array().expect("system is array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["type"], "text");
    assert_eq!(arr[0]["text"], "you are concise");
    assert!(arr[0].get("cache_control").is_none());
}

// mu-yqeq.8: cache_control emission verified via the Projected path
// with AnthropicCacheStrategy applied. This is the canonical
// post-Phase-D path; the Legacy path tests above pin the
// no-cache-control behavior.

fn build_projection_with_cache_strategy(
    system_prompt: Option<&str>,
    messages: &[AgentMessage],
    tools: &[ToolSpec],
) -> ProviderMessages {
    let rope = assemble_rope(system_prompt, messages, tools);
    let mut projection =
        crate::context::AnthropicProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
    let strategy = crate::context::AnthropicCacheStrategy::new();
    let boundaries = strategy.boundaries(&rope);
    strategy.annotate(&mut projection, &boundaries);
    projection
}

#[test]
fn yqeq8_projected_emits_cache_control_on_system_and_last_tool() {
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let tools = vec![
        ToolSpec {
            name: "read".into(),
            description: "Read".into(),
            input_schema: json!({ "type": "object" }),
            display: None,
            when: None,
            policy: Default::default(),

            ..Default::default()
        },
        ToolSpec {
            name: "glob".into(),
            description: "Glob".into(),
            input_schema: json!({ "type": "object" }),
            display: None,
            when: None,
            policy: Default::default(),

            ..Default::default()
        },
        ToolSpec {
            name: "grep".into(),
            description: "Grep".into(),
            input_schema: json!({ "type": "object" }),
            display: None,
            when: None,
            policy: Default::default(),

            ..Default::default()
        },
    ];
    let projection = build_projection_with_cache_strategy(Some("be concise"), &messages, &tools);
    let body =
        build_request_body_from_projection("claude-test", &projection, &tools, CacheTtl::default());

    // System carries cache_control.
    let sys = &body["system"].as_array().expect("system array")[0];
    assert_eq!(
        sys["cache_control"],
        json!({ "type": "ephemeral" }),
        "system block must carry cache_control"
    );

    // Only the LAST tool carries cache_control; earlier tools must not.
    let tool_arr = body["tools"].as_array().expect("tools array");
    assert!(tool_arr[0].get("cache_control").is_none());
    assert!(tool_arr[1].get("cache_control").is_none());
    assert_eq!(
        tool_arr[2]["cache_control"],
        json!({ "type": "ephemeral" }),
        "last tool must carry cache_control"
    );
}

#[test]
fn yqeq8_projected_no_system_no_tools_emits_no_cache_control() {
    // mu-0q44: empty tools now injects a no-tools clause as a System
    // span, so there IS a system block — but the test still verifies
    // that no tools array is emitted.
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let projection = build_projection_with_cache_strategy(None, &messages, &[]);
    let body =
        build_request_body_from_projection("claude-test", &projection, &[], CacheTtl::default());
    let sys = body["system"].as_array().expect("mu-0q44 system block");
    assert!(sys[0]["text"]
        .as_str()
        .unwrap()
        .contains("no tools available"));
    assert!(body.get("tools").is_none());
}

#[test]
fn yqeq8_projected_system_only_caches_system() {
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let projection = build_projection_with_cache_strategy(Some("be concise"), &messages, &[]);
    let body =
        build_request_body_from_projection("claude-test", &projection, &[], CacheTtl::default());
    let sys = &body["system"].as_array().expect("system array")[0];
    assert_eq!(sys["cache_control"], json!({ "type": "ephemeral" }));
    assert!(body.get("tools").is_none());
}

#[test]
fn yqeq8_projected_tools_only_caches_last_tool() {
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let tools = vec![
        ToolSpec {
            name: "read".into(),
            description: "Read".into(),
            input_schema: json!({ "type": "object" }),
            display: None,
            when: None,
            policy: Default::default(),

            ..Default::default()
        },
        ToolSpec {
            name: "grep".into(),
            description: "Grep".into(),
            input_schema: json!({ "type": "object" }),
            display: None,
            when: None,
            policy: Default::default(),

            ..Default::default()
        },
    ];
    let projection = build_projection_with_cache_strategy(None, &messages, &tools);
    let body =
        build_request_body_from_projection("claude-test", &projection, &tools, CacheTtl::default());
    assert!(body.get("system").is_none());
    let tool_arr = body["tools"].as_array().expect("tools array");
    assert!(tool_arr[0].get("cache_control").is_none());
    assert_eq!(tool_arr[1]["cache_control"], json!({ "type": "ephemeral" }));
}

#[test]
fn yqeq8_projected_without_cache_strategy_emits_no_cache_control() {
    // No strategy applied ⇒ no markers ⇒ no cache_control. This is
    // what the yqeq4_parity_* tests rely on for byte-equality with
    // the Legacy path (which also emits no cache_control post-yqeq.8).
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let tools = vec![ToolSpec {
        name: "read".into(),
        description: "Read".into(),
        input_schema: json!({ "type": "object" }),
        display: None,
        when: None,
        policy: Default::default(),

        ..Default::default()
    }];
    let rope = assemble_rope(Some("be concise"), &messages, &tools);
    let projection =
        crate::context::AnthropicProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
    let body =
        build_request_body_from_projection("claude-test", &projection, &tools, CacheTtl::default());
    let sys = &body["system"].as_array().expect("system array")[0];
    assert!(sys.get("cache_control").is_none());
    let tool_arr = body["tools"].as_array().expect("tools array");
    assert!(tool_arr[0].get("cache_control").is_none());
}

#[test]
fn mu_s855_projected_concatenates_memory_injection_and_file_load_into_system_block() {
    // mu-s855 regression test. mu-phl v0 introduced MemoryInjection +
    // FileLoad spans into the rope via assemble_rope_with_context. The
    // Anthropic Projected arm must include their content in
    // body.system[0].text (Anthropic's system field can be a string or
    // an array of content blocks; mu currently emits a single text
    // block with all System-role content concatenated).
    //
    // Pre-fix this test failed: translate_provider_messages only
    // captured the span with id literally "system-prompt" and silently
    // dropped memory-recall:* + project-file:* spans.
    //
    // Codex sibling: mu-2puu. OpenRouter sibling: mu-745h.
    use mu_core::context::{
        assemble_rope_with_context, ProjectContext, ProjectionTarget, ProviderRenderer,
        RecallSource, RecalledItem,
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
    let projection =
        crate::context::AnthropicProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
    let body =
        build_request_body_from_projection("claude-test", &projection, &[], CacheTtl::default());

    let sys_arr = body.get("system").and_then(|v| v.as_array()).unwrap();
    assert_eq!(sys_arr.len(), 1, "expect one consolidated system block");
    let sys_text = sys_arr[0]["text"].as_str().unwrap_or("");

    assert!(
        sys_text.contains("you are mu"),
        "system prompt missing: {sys_text:?}",
    );
    assert!(
        sys_text.contains("favorite color is 'cat'"),
        "memory-recall content missing: {sys_text:?}",
    );
    assert!(
        sys_text.contains("use SpanText for content fields."),
        "project-file content missing: {sys_text:?}",
    );
}

#[test]
fn mu_s855_projected_excludes_tool_schema_from_system_block() {
    // Tool-schema spans map to ProviderRole::System but MUST NOT
    // appear in body.system — tools go separately via body.tools.
    use mu_core::context::{assemble_rope, ProjectionTarget, ProviderRenderer};

    let tools = vec![ToolSpec {
        name: "read".into(),
        description: "read a file content here".into(),
        input_schema: json!({ "type": "object" }),
        display: None,
        when: None,
        policy: Default::default(),

        ..Default::default()
    }];
    let messages = vec![AgentMessage::User {
        content: "go".into(),
    }];
    let rope = assemble_rope(Some("system-only-text"), &messages, &tools);
    let projection =
        crate::context::AnthropicProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
    let body =
        build_request_body_from_projection("claude-test", &projection, &tools, CacheTtl::default());

    let sys_arr = body.get("system").and_then(|v| v.as_array()).unwrap();
    let sys_text = sys_arr[0]["text"].as_str().unwrap_or("");
    assert!(
        sys_text.contains("system-only-text"),
        "system prompt missing: {sys_text:?}",
    );
    // Tool-schema content (description, JSON schema) MUST NOT appear
    // in the system block. The description string we check for is
    // distinctive enough that an accidental leak would catch it.
    assert!(
        !sys_text.contains("read a file content here"),
        "tool-schema content leaked into system block: {sys_text:?}",
    );
}

#[test]
fn mu_s855_cache_marker_on_recall_span_triggers_system_cache_control() {
    // mu-s855 issue 2: when AnthropicCacheStrategy places a cache
    // marker on a memory-recall:* or project-file:* span (which it
    // can post-mu-phl since those are stable+cacheable spans extending
    // the cacheable prefix), detect_cache_targets must recognize the
    // marker as a system_should_cache trigger — those spans now
    // contribute to body.system.
    //
    // Pre-fix the detect_cache_targets helper only triggered on the
    // literal "system-prompt" span id, so markers on recall spans
    // were effectively dead.
    use mu_core::context::{
        assemble_rope_with_context, CacheStrategy, ProjectContext, ProjectionTarget,
        ProviderRenderer, RecallSource, RecalledItem,
    };
    use std::path::PathBuf;

    let project_context = ProjectContext {
        items: vec![RecalledItem {
            source: RecallSource::ProjectFile {
                path: PathBuf::from("/home/u/CLAUDE.md"),
            },
            content: "project content".into(),
            stable_id: "x".into(),
        }],
    };
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    let rope =
        assemble_rope_with_context(Some("you are mu"), Some(&project_context), &messages, &[]);
    let mut projection =
        crate::context::AnthropicProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
    let strategy = crate::context::AnthropicCacheStrategy::new();
    let boundaries = strategy.boundaries(&rope);
    strategy.annotate(&mut projection, &boundaries);
    let body =
        build_request_body_from_projection("claude-test", &projection, &[], CacheTtl::default());

    let sys = &body["system"].as_array().expect("system array")[0];
    assert_eq!(
        sys["cache_control"],
        json!({ "type": "ephemeral" }),
        "system block must carry cache_control when the cache strategy \
         marks any non-tool-schema System-role span (mu-s855)",
    );
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
        arguments: ToolArgs::new(json!({ "path": path })).unwrap(),
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
                ContentBlock::Text { text } => assert_eq!(text.as_ref(), "hello world"),
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
async fn mu_yz48_message_delta_top_level_usage_is_captured() {
    // Regression for mu-yz48 — Anthropic's streaming API puts the
    // cumulative usage on the message_delta event at the TOP level,
    // sibling to `delta` (not nested inside it). Previously we
    // deserialized only the nested `delta.usage` (which is always
    // absent), so output_tokens stayed pinned to message_start's
    // baseline (1) across the entire stream. Verified against real
    // 10-turn opus-4-7 session 3262c036eaca7daa where 25 messages
    // totaling 1.4M chars of text all reported output_tokens=1-6.
    let raw = concat!(
        r#"event: message_start"#,
        "\n",
        r#"data: {"type":"message_start","message":{"id":"m_1","role":"assistant","usage":{"input_tokens":2000,"output_tokens":1,"cache_read_input_tokens":500,"cache_creation_input_tokens":100}}}"#,
        "\n\n",
        r#"event: content_block_start"#,
        "\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "\n\n",
        r#"event: content_block_delta"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"long reply"}}"#,
        "\n\n",
        r#"event: content_block_stop"#,
        "\n",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "\n\n",
        r#"event: message_delta"#,
        "\n",
        // The bug: pre-fix we'd parse `delta.usage` only — actual API puts usage HERE, top-level.
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5547}}"#,
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
        .rev()
        .find(|e| matches!(e, ProviderEvent::Done(_)))
        .expect("stream emits a Done event");
    let ProviderEvent::Done(msg) = done else {
        unreachable!()
    };
    let usage = msg.usage.expect("Done carries usage");
    assert_eq!(
        usage.output_tokens, 5547,
        "output_tokens must come from top-level usage on message_delta, not the message_start baseline of 1"
    );
    assert_eq!(
        usage.input_tokens, 2000,
        "input_tokens from message_start preserved"
    );
    assert_eq!(usage.cache_read_input_tokens, Some(500));
    assert_eq!(usage.cache_creation_input_tokens, Some(100));
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
                ContentBlock::Text { text } => assert_eq!(text.as_ref(), "partial"),
                other => panic!("expected Text block, got {other:?}"),
            }
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn map_stop_reason_known_and_unknown() {
    use mu_anthropic::StopReason as A;
    assert_eq!(map_stop_reason(Some(&A::EndTurn)), StopReason::EndTurn);
    assert_eq!(map_stop_reason(Some(&A::ToolUse)), StopReason::ToolUse);
    assert_eq!(map_stop_reason(Some(&A::MaxTokens)), StopReason::MaxTokens);
    // unmapped wire reasons (stop_sequence / refusal / pause_turn / unknown)
    // all fold to EndTurn.
    assert_eq!(map_stop_reason(Some(&A::StopSequence)), StopReason::EndTurn);
    assert_eq!(map_stop_reason(Some(&A::Other)), StopReason::EndTurn);
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

    // TextDelta, then the tool_use block streams ToolCallDelta (name on start,
    // args on each input_json_delta), then Done:
    //   TextDelta, ToolCallDelta(name), ToolCallDelta(args), ToolCallDelta(args), Done.
    assert_eq!(events.len(), 5, "got {events:?}");
    match &events[0] {
        ProviderEvent::TextDelta(t) => assert_eq!(t, "I will read it. "),
        other => panic!("expected TextDelta, got {other:?}"),
    }
    match &events[1] {
        ProviderEvent::ToolCallDelta { id, name_delta, .. } => {
            assert_eq!(id, "toolu_X");
            assert_eq!(name_delta.as_deref(), Some("read"));
        }
        other => panic!("expected ToolCallDelta(start), got {other:?}"),
    }
    let done = match events.into_iter().nth(4).unwrap() {
        ProviderEvent::Done(msg) => msg,
        other => panic!("expected Done, got {other:?}"),
    };
    assert_eq!(done.stop_reason, StopReason::ToolUse);
    assert_eq!(done.content.len(), 2);
    match &done.content[0] {
        ContentBlock::Text { text } => assert_eq!(text.as_ref(), "I will read it. "),
        other => panic!("expected Text, got {other:?}"),
    }
    match &done.content[1] {
        ContentBlock::ToolCall(tc) => {
            assert_eq!(tc.id, "toolu_X");
            assert_eq!(tc.name, "read");
            assert_eq!(tc.arguments.as_value()["path"], "/etc/hostname");
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

    // No text block, so no TextDelta — but the tool_use block streams:
    //   ToolCallDelta(name), ToolCallDelta(args), Done.
    assert_eq!(events.len(), 3, "got {events:?}");
    match &events[0] {
        ProviderEvent::ToolCallDelta { id, name_delta, .. } => {
            assert_eq!(id, "toolu_Y");
            assert_eq!(name_delta.as_deref(), Some("echo"));
        }
        other => panic!("expected ToolCallDelta(start), got {other:?}"),
    }
    let done = match events.into_iter().nth(2).unwrap() {
        ProviderEvent::Done(msg) => msg,
        other => panic!("expected Done, got {other:?}"),
    };
    assert_eq!(done.content.len(), 1);
    match &done.content[0] {
        ContentBlock::ToolCall(tc) => {
            assert_eq!(tc.id, "toolu_Y");
            assert_eq!(tc.name, "echo");
            assert_eq!(tc.arguments.as_value()["text"], "hi");
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
}

/// Thinking: a thinking block streams ThinkingDelta events (signature_delta
/// produces none) and assembles into a ContentBlock::Thinking, in document
/// order ahead of the answer text. Covers the Anthropic extended-thinking and
/// ollama-reasoning-model paths (same Anthropic SSE wire).
#[tokio::test]
async fn thinking_sse_streams_deltas_and_assembles_block() {
    let raw = concat!(
        r#"event: message_start"#,
        "\n",
        r#"data: {"type":"message_start","message":{"id":"m_1","role":"assistant"}}"#,
        "\n\n",
        // Block 0: thinking (start carries empty thinking; content arrives as deltas)
        r#"event: content_block_start"#,
        "\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        "\n\n",
        r#"event: content_block_delta"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me "}}"#,
        "\n\n",
        r#"event: content_block_delta"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"think."}}"#,
        "\n\n",
        // signature_delta is consumed but surfaces no event
        r#"event: content_block_delta"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-abc"}}"#,
        "\n\n",
        r#"event: content_block_stop"#,
        "\n",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "\n\n",
        // Block 1: the visible answer text
        r#"event: content_block_start"#,
        "\n",
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        "\n\n",
        r#"event: content_block_delta"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Answer"}}"#,
        "\n\n",
        r#"event: content_block_stop"#,
        "\n",
        r#"data: {"type":"content_block_stop","index":1}"#,
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
    let (_tx, rx) = tokio::sync::oneshot::channel();
    let mut stream = test_events_stream(bytes, rx);

    let mut events = Vec::new();
    while let Some(e) = stream.next().await {
        events.push(e);
    }

    // ThinkingDelta("Let me "), ThinkingDelta("think."), TextDelta("Answer"), Done.
    // signature_delta surfaces nothing.
    assert_eq!(events.len(), 4, "got {events:?}");
    match &events[0] {
        ProviderEvent::ThinkingDelta(t) => assert_eq!(t, "Let me "),
        other => panic!("expected ThinkingDelta, got {other:?}"),
    }
    match &events[1] {
        ProviderEvent::ThinkingDelta(t) => assert_eq!(t, "think."),
        other => panic!("expected ThinkingDelta, got {other:?}"),
    }
    match &events[2] {
        ProviderEvent::TextDelta(t) => assert_eq!(t, "Answer"),
        other => panic!("expected TextDelta, got {other:?}"),
    }
    let done = match events.into_iter().nth(3).unwrap() {
        ProviderEvent::Done(msg) => msg,
        other => panic!("expected Done, got {other:?}"),
    };
    assert_eq!(done.stop_reason, StopReason::EndTurn);
    assert_eq!(done.content.len(), 2, "thinking block then text block");
    match &done.content[0] {
        ContentBlock::Thinking { text, .. } => assert_eq!(text.as_ref(), "Let me think."),
        other => panic!("expected Thinking, got {other:?}"),
    }
    match &done.content[1] {
        ContentBlock::Text { text } => assert_eq!(text.as_ref(), "Answer"),
        other => panic!("expected Text, got {other:?}"),
    }
}

/// Tool-use args stream live as ToolCallDelta events (name on the block start,
/// argument fragments on each input_json_delta) while the Done payload still
/// carries the fully-assembled tool call.
#[tokio::test]
async fn tool_use_args_stream_as_deltas_and_finalize() {
    let raw = concat!(
        r#"event: content_block_start"#,
        "\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_Y","name":"echo","input":{}}}"#,
        "\n\n",
        r#"event: content_block_delta"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"text\":"}}"#,
        "\n\n",
        r#"event: content_block_delta"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"hi\"}"}}"#,
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

    let mut events = Vec::new();
    while let Some(e) = stream.next().await {
        events.push(e);
    }

    // ToolCallDelta(name=echo), ToolCallDelta(args="{\"text\":"),
    // ToolCallDelta(args="\"hi\"}"), Done.
    assert_eq!(events.len(), 4, "got {events:?}");
    match &events[0] {
        ProviderEvent::ToolCallDelta {
            id,
            name_delta,
            arguments_delta,
        } => {
            assert_eq!(id, "toolu_Y");
            assert_eq!(name_delta.as_deref(), Some("echo"));
            assert_eq!(arguments_delta.as_deref(), None);
        }
        other => panic!("expected ToolCallDelta(start), got {other:?}"),
    }
    match &events[1] {
        ProviderEvent::ToolCallDelta {
            id,
            name_delta,
            arguments_delta,
        } => {
            assert_eq!(id, "toolu_Y");
            assert_eq!(name_delta.as_deref(), None);
            assert_eq!(arguments_delta.as_deref(), Some("{\"text\":"));
        }
        other => panic!("expected ToolCallDelta(args), got {other:?}"),
    }
    match &events[2] {
        ProviderEvent::ToolCallDelta {
            arguments_delta, ..
        } => assert_eq!(arguments_delta.as_deref(), Some("\"hi\"}")),
        other => panic!("expected ToolCallDelta(args), got {other:?}"),
    }
    let done = match events.into_iter().nth(3).unwrap() {
        ProviderEvent::Done(msg) => msg,
        other => panic!("expected Done, got {other:?}"),
    };
    assert_eq!(done.content.len(), 1);
    match &done.content[0] {
        ContentBlock::ToolCall(tc) => {
            assert_eq!(tc.id, "toolu_Y");
            assert_eq!(tc.name, "echo");
            assert_eq!(tc.arguments.as_value()["text"], "hi");
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
            assert!(tc.arguments.as_value().is_object());
            assert_eq!(tc.arguments.as_value().as_object().unwrap().len(), 0);
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
            assert!(tc.arguments.as_value().is_object());
            assert_eq!(tc.arguments.as_value().as_object().unwrap().len(), 0);
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
        assert_eq!(text.as_str(), final_text.as_ref());
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

            ..Default::default()
        };

        let messages = vec![AgentMessage::User {
            content: "Use the echo tool with text='hi there'. Just call the tool; no preamble."
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
            tool_call.arguments.as_value().is_object(),
            "arguments must be an object, got: {:?}",
            tool_call.arguments
        );
        let text_arg = tool_call.arguments.as_value()["text"]
            .as_str()
            .unwrap_or("");
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

        ..Default::default()
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
                    arguments: ToolArgs::new(serde_json::json!({"path": "/etc/hostname"})).unwrap(),
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
    let roles: Vec<ProviderRole> = projection.messages.iter().map(|m| m.role()).collect();
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
        .find(|m| m.role() == ProviderRole::User)
        .expect("user message");
    assert_eq!(user_msg.content(), "what's in /etc/hostname?");

    // Assistant text + tool call flatten into one span; verify both
    // surfaces are present in the projection content. The wire body
    // emits them as separate content blocks (text + tool_use);
    // equivalence here is at the "model saw this byte sequence" level.
    let assistant_msg = projection
        .messages
        .iter()
        .find(|m| m.role() == ProviderRole::Assistant)
        .expect("assistant message");
    assert!(assistant_msg.content().contains("I'll read it."));
    assert!(assistant_msg.content().contains("[tool_call:read("));

    // ToolResult content surfaces verbatim (non-error path — no
    // "error:" prefix).
    let tool_result = projection
        .messages
        .iter()
        .find(|m| m.role() == ProviderRole::ToolResult)
        .expect("tool result");
    assert_eq!(tool_result.content(), "myhost");
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
fn fb0_cache_boundaries_land_on_system_and_last_tool_schema() {
    // mu-yqeq.8: AnthropicCacheStrategy now emits TWO boundaries —
    // the system span (index 0) and the last span in the
    // stable+cacheable prefix (index 1, the tool schema). For our
    // fixture: system at 0, tool schema at 1, then volatile
    // user/assistant/tool_result. The Projected wire body picks up
    // the markers via cache_marker on each ProviderMessage; the
    // Legacy wire body no longer emits cache_control at all.
    let (system_prompt, messages, tools) = equivalence_fixture();
    let rope = assemble_rope(system_prompt.as_deref(), &messages, &tools);
    let renderer = crate::context::AnthropicProviderRenderer::new();
    let strategy = crate::context::AnthropicCacheStrategy::new();
    let mut projection = renderer.render(&rope, ProjectionTarget::AgentView);
    let boundaries = strategy.boundaries(&rope);
    strategy.annotate(&mut projection, &boundaries);

    // mu-chiw: THREE boundaries — system (0), last-in-prefix (1, the
    // tool schema), and the conversation run-end anchor (last span).
    // The pre_turn anchor dedups into (1) for this fixture.
    assert_eq!(boundaries.len(), 3);
    assert_eq!(boundaries[0].message_index, 0);
    assert_eq!(boundaries[1].message_index, 1);
    assert_eq!(boundaries[2].message_index, rope.len() - 1);
    assert_eq!(rope.spans()[0].kind(), &SpanKind::System);
    assert_eq!(rope.spans()[1].kind(), &SpanKind::ToolSchema);

    // Annotations land on all three messages.
    for b in &boundaries {
        assert_eq!(
            projection.messages[b.message_index].cache_marker(),
            Some(CacheMarker::Ephemeral),
            "marker missing at boundary index {}",
            b.message_index
        );
    }

    // Projected wire body picks up the markers: system block, last
    // tool spec, and (mu-chiw) a cache_control on the content block
    // of the marked conversation message somewhere in messages.
    let wire =
        build_request_body_from_projection("claude-test", &projection, &tools, CacheTtl::default());
    let sys = &wire["system"].as_array().unwrap()[0];
    assert_eq!(sys["cache_control"], json!({ "type": "ephemeral" }));
    let last_tool = wire["tools"].as_array().unwrap().last().unwrap();
    assert_eq!(last_tool["cache_control"], json!({ "type": "ephemeral" }));
    let message_marks = wire["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter(|block| block.get("cache_control").is_some())
        .count();
    assert_eq!(
        message_marks, 1,
        "exactly one conversation content block carries cache_control"
    );

    // Legacy wire body no longer emits cache_control (mu-yqeq.8).
    let legacy = build_request_body("claude-test", system_prompt.as_deref(), &messages, &tools);
    let legacy_sys = &legacy["system"].as_array().unwrap()[0];
    assert!(legacy_sys.get("cache_control").is_none());
    let legacy_last_tool = legacy["tools"].as_array().unwrap().last().unwrap();
    assert!(legacy_last_tool.get("cache_control").is_none());
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
            m.role() == ProviderRole::System
                && m.source_span_ids()
                    .iter()
                    .any(|id| id.as_ref() == "system-prompt")
        })
        .count();
    assert_eq!(system_count, 0);

    let wire = build_request_body("claude-test", None, &messages, &tools);
    assert!(
        wire.get("system").is_none(),
        "no system_prompt → no `system` field in wire body",
    );
}

#[test]
fn yqeq4_thinking_blocks_are_skipped_in_projected_wire_output() {
    // Spec mu-044 §"Thinking-block skip": Projected wire emission MUST
    // NOT echo the model's reasoning trace back as input. Mirrors the
    // Legacy `translate_message_single` behavior (anthropic.rs:208
    // filters Thinking blocks).
    let messages = vec![AgentMessage::Assistant(AssistantMessage {
        content: vec![
            ContentBlock::Thinking {
                text: "INTERNAL_REASONING_DO_NOT_LEAK".into(),
                opaque: None,
            },
            ContentBlock::Text {
                text: "public answer".into(),
            },
        ],
        stop_reason: StopReason::EndTurn,
        usage: None,
    })];
    let rope = assemble_rope(None, &messages, &[]);
    let projection =
        crate::context::AnthropicProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
    let projected =
        build_request_body_from_projection("claude-test", &projection, &[], CacheTtl::default());

    let wire = serde_json::to_string(&projected).expect("serialize");
    assert!(
        !wire.contains("INTERNAL_REASONING_DO_NOT_LEAK"),
        "Thinking block content leaked to wire: {wire}",
    );
    assert!(
        wire.contains("public answer"),
        "non-thinking text was lost: {wire}",
    );
}

#[test]
fn f1a0_one_hour_ttl_reaches_every_cache_control_site() {
    // mu-f1a0: with CacheTtl::OneHour, every cache_control emission
    // (system block, last tool spec, conversation content block)
    // carries `"ttl": "1h"`; with the default FiveMinutes the wire is
    // byte-identical to pre-f1a0 (bare ephemeral, no ttl key).
    let (system_prompt, messages, tools) = equivalence_fixture();
    let rope = assemble_rope(system_prompt.as_deref(), &messages, &tools);
    let renderer = crate::context::AnthropicProviderRenderer::new();
    let strategy = crate::context::AnthropicCacheStrategy::new();
    let mut projection = renderer.render(&rope, ProjectionTarget::AgentView);
    let boundaries = strategy.boundaries(&rope);
    strategy.annotate(&mut projection, &boundaries);

    let wire =
        build_request_body_from_projection("claude-test", &projection, &tools, CacheTtl::OneHour);
    let expected = serde_json::json!({ "type": "ephemeral", "ttl": "1h" });
    assert_eq!(
        wire["system"].as_array().unwrap()[0]["cache_control"],
        expected
    );
    assert_eq!(
        wire["tools"].as_array().unwrap().last().unwrap()["cache_control"],
        expected
    );
    let conversation_marks: Vec<_> = wire["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter_map(|b| b.get("cache_control").cloned())
        .collect();
    assert_eq!(conversation_marks, vec![expected.clone()]);

    // Default tier: no ttl key anywhere (wire parity with pre-f1a0).
    let wire_5m = build_request_body_from_projection(
        "claude-test",
        &projection,
        &tools,
        CacheTtl::FiveMinutes,
    );
    assert_eq!(
        wire_5m["system"].as_array().unwrap()[0]["cache_control"],
        serde_json::json!({ "type": "ephemeral" })
    );
}

#[test]
fn f1a0_cache_ttl_serde_wire_values() {
    use mu_core::context::CacheTtl;
    assert_eq!(
        serde_json::to_string(&CacheTtl::FiveMinutes).unwrap(),
        "\"5m\""
    );
    assert_eq!(serde_json::to_string(&CacheTtl::OneHour).unwrap(), "\"1h\"");
    assert_eq!(
        serde_json::from_str::<CacheTtl>("\"1h\"").unwrap(),
        CacheTtl::OneHour
    );
    assert_eq!(CacheTtl::default(), CacheTtl::FiveMinutes);
}

// ─── mu-cache-write-tier-split-umq6: per-tier cache-write tests ──────────────

/// Wire fixture: message_start with both tier fields nonzero. The parsed
/// Usage must carry the breakdown and the tier sum must equal the flat total.
#[tokio::test]
async fn umq6_streaming_wire_both_tiers_nonzero_parsed_and_sum_to_total() {
    // Anthropic API response with cache_creation object carrying both tiers.
    // Total cache_creation_input_tokens = 5m(300) + 1h(700) = 1000.
    let raw = concat!(
        r#"event: message_start"#,
        "\n",
        r#"data: {"type":"message_start","message":{"id":"m_1","role":"assistant","usage":{"input_tokens":2000,"output_tokens":1,"cache_read_input_tokens":500,"cache_creation_input_tokens":1000,"cache_creation":{"ephemeral_5m_input_tokens":300,"ephemeral_1h_input_tokens":700}}}}"#,
        "\n\n",
        r#"event: content_block_start"#,
        "\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "\n\n",
        r#"event: content_block_delta"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"reply"}}"#,
        "\n\n",
        r#"event: content_block_stop"#,
        "\n",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "\n\n",
        r#"event: message_delta"#,
        "\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":12}}"#,
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
        .rev()
        .find(|e| matches!(e, ProviderEvent::Done(_)))
        .expect("stream emits a Done event");
    let ProviderEvent::Done(msg) = done else {
        unreachable!()
    };
    let usage = msg.usage.expect("Done carries usage");

    // Flat total preserved.
    assert_eq!(usage.cache_creation_input_tokens, Some(1000));
    // Tier breakdown present.
    assert_eq!(
        usage.cache_creation_5m_input_tokens,
        Some(300),
        "5m tier should be 300"
    );
    assert_eq!(
        usage.cache_creation_1h_input_tokens,
        Some(700),
        "1h tier should be 700"
    );
    // Tiers sum to total.
    let tier_sum = usage.cache_creation_5m_input_tokens.unwrap()
        + usage.cache_creation_1h_input_tokens.unwrap();
    assert_eq!(
        tier_sum,
        usage.cache_creation_input_tokens.unwrap(),
        "tier sum must equal flat total"
    );
}

/// Streaming merge: tier breakdown from message_start survives the
/// message_delta merge (which only carries output_tokens).
#[tokio::test]
async fn umq6_streaming_merge_preserves_both_tiers() {
    // message_start carries the tier breakdown; message_delta carries only
    // output_tokens. After merge, tier fields must be intact.
    let raw = concat!(
        r#"event: message_start"#,
        "\n",
        r#"data: {"type":"message_start","message":{"id":"m_2","role":"assistant","usage":{"input_tokens":1000,"output_tokens":1,"cache_creation_input_tokens":500,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":500}}}}"#,
        "\n\n",
        r#"event: content_block_start"#,
        "\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "\n\n",
        r#"event: message_delta"#,
        "\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":99}}"#,
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
        .rev()
        .find(|e| matches!(e, ProviderEvent::Done(_)))
        .expect("stream emits a Done event");
    let ProviderEvent::Done(msg) = done else {
        unreachable!()
    };
    let usage = msg.usage.expect("Done carries usage");

    // output_tokens came from message_delta.
    assert_eq!(usage.output_tokens, 99);
    // Tier fields from message_start survived the merge.
    assert_eq!(usage.cache_creation_5m_input_tokens, Some(0));
    assert_eq!(usage.cache_creation_1h_input_tokens, Some(500));
}

/// When no cache_creation object is present (old-style response with flat
/// field only), tier fields remain None — no regression on existing wire.
#[tokio::test]
async fn umq6_streaming_no_tier_object_fields_remain_none() {
    // Same wire format as pre-umq6 (no cache_creation object).
    let raw = concat!(
        r#"event: message_start"#,
        "\n",
        r#"data: {"type":"message_start","message":{"id":"m_3","role":"assistant","usage":{"input_tokens":800,"output_tokens":1,"cache_creation_input_tokens":200}}}"#,
        "\n\n",
        r#"event: message_delta"#,
        "\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":20}}"#,
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
        .rev()
        .find(|e| matches!(e, ProviderEvent::Done(_)))
        .expect("stream emits a Done event");
    let ProviderEvent::Done(msg) = done else {
        unreachable!()
    };
    let usage = msg.usage.expect("Done carries usage");

    // Flat field present.
    assert_eq!(usage.cache_creation_input_tokens, Some(200));
    // Tier fields absent — no regression.
    assert!(
        usage.cache_creation_5m_input_tokens.is_none(),
        "no tier breakdown when cache_creation object absent"
    );
    assert!(usage.cache_creation_1h_input_tokens.is_none());
}

#[test]
fn ollama_thinking_flag_is_switch_not_effort() {
    assert_eq!(parse_ollama_thinking_flag(""), None);
    assert_eq!(parse_ollama_thinking_flag("   "), None);
    for off in ["off", "none", "false", "0", "disabled"] {
        assert_eq!(
            parse_ollama_thinking_flag(off).as_deref(),
            Some("off"),
            "{off}"
        );
    }
    for on in ["on", "true", "enabled", "low", "high", "xhigh", "banana"] {
        assert_eq!(
            parse_ollama_thinking_flag(on).as_deref(),
            Some("on"),
            "{on}"
        );
    }
}

#[test]
fn apply_ollama_thinking_on_sets_only_thinking_object() {
    let mut body = build_request_body("gpt-oss:20b", None, &[], &[]);
    apply_ollama_thinking(&mut body, Some("on"));
    assert_eq!(
        body["thinking"],
        json!({"type":"adaptive", "display":"summarized"})
    );
    assert!(
        body.get("output_config").is_none(),
        "ollama must not get Anthropic effort"
    );
}

#[test]
fn apply_ollama_thinking_off_disables_without_output_config() {
    let mut body = build_request_body("gpt-oss:20b", None, &[], &[]);
    apply_ollama_thinking(&mut body, Some("off"));
    assert_eq!(body["thinking"], json!({"type":"disabled"}));
    assert!(
        body.get("output_config").is_none(),
        "ollama must not get Anthropic effort"
    );
}
