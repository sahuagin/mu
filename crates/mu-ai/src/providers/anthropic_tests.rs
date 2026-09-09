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

// Pins that the Anthropic wire carries NO sampling parameters, ever. The
// catalog's per-model sampling (mu-y8gp/mu-4sivd) is forwarded only on the
// OpenAI-compat wire; on gen-5 Claude models the same fields are wire
// errors — Sonnet 5 returns 400 for any non-default `temperature`/`top_p`/
// `top_k`, and Fable/Mythos 5 reject manual thinking budgets outright
// (2026-06..08 API release notes). If sampling ever gets wired into this
// body, it must be gated per-model on card capability flags first.
// (mu-provider-drift-2026q3-y43la)
#[test]
fn build_request_body_sends_no_sampling_params() {
    let messages = vec![AgentMessage::User {
        content: "hi".into(),
    }];
    for model in ["claude-sonnet-5", "claude-opus-5", "claude-fable-5"] {
        let body = build_request_body(model, None, &messages, &[]);
        let obj = body.as_object().expect("body is an object");
        for key in ["temperature", "top_p", "top_k", "presence_penalty"] {
            assert!(
                !obj.contains_key(key),
                "anthropic body for {model} must not carry `{key}`"
            );
        }
    }
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
    // (display:summarized is required for readable reasoning; enabled+budget is
    // deprecated on the 4.6 models and a 400 from Opus 4.7 on — the catalog's
    // rejects_manual_thinking rule, tested with the other model rules.)
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
    // mu-c9b2l: the shipped default cap, so the existing scenarios exercise
    // the same accumulator production runs.
    test_events_stream_budgeted(bytes, cancel_rx, Some(DEFAULT_MAX_TOOL_CALL_BYTES), None)
}

/// mu-c9b2l: [`test_events_stream`] with both ceilings spelled out — the
/// session's `max_tool_call_bytes` and what the request's `max_tokens` buys.
fn test_events_stream_budgeted(
    bytes: impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
    cancel_rx: oneshot::Receiver<()>,
    max_tool_call_bytes: Option<usize>,
    output_budget_bytes: Option<usize>,
) -> BoxStream<'static, ProviderEvent> {
    let bytes: Pin<Box<dyn Stream<Item = Result<Bytes, String>> + Send>> =
        Box::pin(bytes.map(|r| r.map_err(|e| e.to_string())));
    let sse = SseStream::new(bytes);
    let state = StreamState {
        sse,
        blocks: HashMap::new(),
        block_order: Vec::new(),
        stop_reason: None,
        usage: AnthropicUsage::default(),
        cancel_rx: Some(cancel_rx),
        finished: false,
        emitted_done: false,
        max_tool_call_bytes,
        output_budget_bytes,
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
    // Gen-5 terminal states map to their own core reasons
    // (mu-provider-drift-2026q3-y43la).
    assert_eq!(map_stop_reason(Some(&A::Refusal)), StopReason::Refusal);
    assert_eq!(map_stop_reason(Some(&A::PauseTurn)), StopReason::PauseTurn);
    // stop_sequence and unknown/absent reasons fold to EndTurn.
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

    // ----------------------------------------------------------------------
    // mu-anthropic-protocol-2026q3-6uqho.1: mid-conversation tool changes
    // ----------------------------------------------------------------------

    /// The shipped catalog carries the quirk for exactly the families the
    /// 2026-07-24 changelog names (and their successors by prefix), and
    /// for nothing else: not Sonnet 5, not Opus 4.7, not a local tag.
    #[test]
    fn shipped_catalog_grants_the_tool_changes_quirk_to_the_documented_families() {
        let catalog = mu_core::model_catalog::built_in();
        for model in [
            "claude-fable-5",
            "claude-fable-5-1",
            "claude-mythos-5-1",
            "claude-opus-4-8",
            "claude-opus-4-8-20260901",
            "claude-opus-5",
        ] {
            assert_eq!(
                beta_headers_for(&catalog, model, ANTHROPIC_API_BASE, None),
                vec![Beta::MidConversationToolChanges],
                "{model}"
            );
        }
        for model in [
            "claude-sonnet-5",
            "claude-opus-4-7",
            "claude-sonnet-4-6",
            "claude-haiku-4-5-20251001",
            "qwen3.6:27b",
            "",
        ] {
            assert!(
                beta_headers_for(&catalog, model, ANTHROPIC_API_BASE, None).is_empty(),
                "{model}"
            );
        }
    }

    /// The other two gates: only Anthropic's own endpoint gets the header
    /// (a gateway or an ollama box addressed with a Claude id may 400 on an
    /// unknown beta; a trailing slash or case on the configured base must
    /// not hide the real endpoint), and the operator override: off is
    /// absolute, on still stops at the endpoint so it cannot leak the
    /// header onto the ollama lane that shares this provider in one daemon.
    #[test]
    fn beta_header_is_gated_on_endpoint_and_operator_override() {
        let catalog = mu_core::model_catalog::built_in();
        let on = vec![Beta::MidConversationToolChanges];
        assert!(
            beta_headers_for(&catalog, "claude-opus-5", "http://10.1.1.143:11434", None).is_empty()
        );
        assert!(
            beta_headers_for(&catalog, "claude-opus-5", "https://gateway.example", None).is_empty()
        );
        for base in ["https://api.anthropic.com/", "HTTPS://API.ANTHROPIC.COM"] {
            assert_eq!(
                beta_headers_for(&catalog, "claude-opus-5", base, None),
                on,
                "{base}"
            );
        }
        assert!(
            beta_headers_for(&catalog, "claude-opus-5", ANTHROPIC_API_BASE, Some(false)).is_empty()
        );
        assert_eq!(
            beta_headers_for(&catalog, "qwen3.6:27b", ANTHROPIC_API_BASE, Some(true)),
            on
        );
        assert!(beta_headers_for(
            &catalog,
            "qwen3.6:27b",
            "http://10.1.1.143:11434",
            Some(true)
        )
        .is_empty());
    }

    /// A 400 that names the header is the beta being refused; any other
    /// 400 is the request's own problem and must not trigger the retry.
    #[test]
    fn beta_rejection_is_told_apart_from_other_bad_requests() {
        assert!(beta_rejected(
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"Unexpected value(s) `mid-conversation-tool-changes-2026-07-01` for the `anthropic-beta` header."}}"#
        ));
        assert!(!beta_rejected(
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages: at least one message is required"}}"#
        ));
        // A body that merely echoes the header's name (a tool result quoting
        // this file, say) is not a rejection of our beta.
        assert!(!beta_rejected(
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"tool_result content too large: ... `anthropic-beta` ..."}}"#
        ));
    }

    /// The header on the wire, not just in a helper: build the request the
    /// stream path sends and read its headers back. Hermetic: the shipped
    /// catalog and an explicit no-override are injected, so neither this
    /// machine's ~/.config/mu/models.toml nor its environment can flip it.
    #[test]
    fn beta_header_lands_on_the_wire_for_a_documented_model_only() {
        let catalog = mu_core::model_catalog::built_in();
        let body = serde_json::json!({"model": "x", "messages": [], "max_tokens": 1});
        let build = |p: &AnthropicProvider| {
            p.messages_request_with(&body, &catalog, None)
                .build()
                .expect("build")
        };

        let req = build(&AnthropicProvider::new(
            "k".into(),
            "claude-fable-5-1".into(),
        ));
        assert_eq!(
            req.headers()
                .get("anthropic-beta")
                .and_then(|v| v.to_str().ok()),
            Some(Beta::MidConversationToolChanges.as_str()),
            "{:?}",
            req.headers()
        );
        assert_eq!(
            req.headers().get("anthropic-version").unwrap(),
            ANTHROPIC_VERSION
        );

        let req = build(&AnthropicProvider::new(
            "k".into(),
            "claude-sonnet-5".into(),
        ));
        assert!(
            req.headers().get("anthropic-beta").is_none(),
            "{:?}",
            req.headers()
        );

        let req = build(
            &AnthropicProvider::new("k".into(), "claude-fable-5-1".into())
                .with_api_base("http://10.1.1.143:11434".into()),
        );
        assert!(
            req.headers().get("anthropic-beta").is_none(),
            "{:?}",
            req.headers()
        );
    }

    /// The two system-message betas follow the body, not the catalog: a
    /// message carrying `clear_at` or `output_config` asks for its header on
    /// any endpoint and for any model, and nothing else asks for either —
    /// not the top-level `output_config` that `--thinking` puts on every
    /// request, not an explicit null. The shapes come from mu-anthropic's
    /// own constructors, so the crate and the transport agree on what the
    /// field looks like on the wire.
    #[test]
    fn system_message_betas_follow_the_body() {
        let catalog = mu_core::model_catalog::built_in();
        let request = |messages: Vec<AnthMessage>| {
            let mut body = serde_json::to_value(MessagesRequest::new("x", 1, messages)).unwrap();
            body["output_config"] = serde_json::json!({"effort": "high"});
            body
        };
        let plain = request(vec![AnthMessage::user("hi")]);
        assert!(body_betas(&plain).is_empty(), "{plain}");
        let mut nulled = plain.clone();
        nulled["messages"][0]["clear_at"] = Value::Null;
        assert!(body_betas(&nulled).is_empty(), "{nulled}");

        let scoped = request(vec![
            AnthMessage::user("hi"),
            AnthMessage::turn_scoped("Request independent reads in one turn."),
        ]);
        assert_eq!(
            body_betas(&scoped),
            vec![Beta::MidConversationSystemClearAt]
        );
        let effort = request(vec![
            AnthMessage::user("hi"),
            AnthMessage::assistant("ok"),
            AnthMessage::system_effort("low"),
            AnthMessage::user("more"),
        ]);
        assert_eq!(body_betas(&effort), vec![Beta::MidConversationOutputConfig]);

        // On the wire: after the catalog beta for a documented model on
        // Anthropic's endpoint; alone for a model and endpoint the catalog
        // beta never reaches; absent when nothing asks.
        let header = |p: &AnthropicProvider, body: &Value| {
            p.messages_request_with(body, &catalog, None)
                .build()
                .expect("build")
                .headers()
                .get("anthropic-beta")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        assert_eq!(
            header(
                &AnthropicProvider::new("k".into(), "claude-fable-5-1".into()),
                &scoped
            ),
            Some(format!(
                "{},{}",
                Beta::MidConversationToolChanges,
                Beta::MidConversationSystemClearAt
            ))
        );
        assert_eq!(
            header(
                &AnthropicProvider::new("k".into(), "claude-sonnet-5".into())
                    .with_api_base("https://gateway.example".into()),
                &effort
            ),
            Some(Beta::MidConversationOutputConfig.to_string())
        );
        assert_eq!(
            header(
                &AnthropicProvider::new("k".into(), "claude-sonnet-5".into()),
                &plain
            ),
            None
        );
    }

    /// The two thinking betas follow the body the same way: `display:
    /// "updates"` asks for the display beta (the other two values, including
    /// the `summarized` that `apply_thinking` sends, do not), and a
    /// `block_binding` object asks for the binding beta whichever behavior
    /// it names. Bodies come from mu-anthropic's typed config so the crate
    /// and the transport agree on the wire shape.
    #[test]
    fn thinking_betas_follow_the_body() {
        use mu_anthropic::{PrefixMismatchBehavior, ThinkingConfig, ThinkingDisplay};
        let body = |t: ThinkingConfig| {
            serde_json::to_value(
                MessagesRequest::new("x", 1, vec![AnthMessage::user("hi")]).with_thinking(t),
            )
            .unwrap()
        };
        let mut summarized = serde_json::json!({"model": "x", "max_tokens": 1, "messages": []});
        apply_thinking(&mut summarized, Some("high"));
        assert!(body_betas(&summarized).is_empty(), "{summarized}");
        assert!(body_betas(&body(
            ThinkingConfig::adaptive().with_display(ThinkingDisplay::Omitted)
        ))
        .is_empty());
        assert_eq!(
            body_betas(&body(
                ThinkingConfig::adaptive().with_display(ThinkingDisplay::Updates)
            )),
            vec![Beta::ThinkingDisplayUpdates]
        );
        assert_eq!(
            body_betas(&body(
                ThinkingConfig::enabled(2048)
                    .with_prefix_mismatch_behavior(PrefixMismatchBehavior::Error)
            )),
            vec![Beta::ThinkingBindingControls]
        );
        assert_eq!(
            body_betas(&body(
                ThinkingConfig::adaptive()
                    .with_display(ThinkingDisplay::Updates)
                    .with_prefix_mismatch_behavior(PrefixMismatchBehavior::DropBlock)
            )),
            vec![Beta::ThinkingDisplayUpdates, Beta::ThinkingBindingControls]
        );
        let mut nulled = body(ThinkingConfig::adaptive());
        nulled["thinking"]["block_binding"] = Value::Null;
        assert!(body_betas(&nulled).is_empty(), "{nulled}");
    }

    /// The fallback family follows the body too: `fallbacks` in either form
    /// asks for the server-side-fallback beta; a `fallback_credit_token`
    /// asks for the fallback-credit beta only in its object form (the bare
    /// string needs none). Bodies from the crate's typed request.
    #[test]
    fn fallback_betas_follow_the_body() {
        use mu_anthropic::{CreditRedemption, FallbackCreditToken, FallbackTarget, Fallbacks};
        let body = |r: MessagesRequest| serde_json::to_value(r).unwrap();
        let base = || MessagesRequest::new("x", 1, vec![AnthMessage::user("hi")]);
        assert!(body_betas(&body(base())).is_empty());
        assert_eq!(
            body_betas(&body(base().with_fallbacks(Fallbacks::Default))),
            vec![Beta::ServerSideFallback]
        );
        assert_eq!(
            body_betas(&body(base().with_fallbacks(Fallbacks::Models(vec![
                FallbackTarget::model("claude-opus-4-8")
            ])))),
            vec![Beta::ServerSideFallback]
        );
        assert!(body_betas(&body(
            base().with_fallback_credit_token(FallbackCreditToken::Token("fct_01".into()))
        ))
        .is_empty());
        assert_eq!(
            body_betas(&body(base().with_fallback_credit_token(
                FallbackCreditToken::WithMode {
                    token: "fct_01".into(),
                    mode: Some(CreditRedemption::BestEffort),
                }
            ))),
            vec![Beta::FallbackCredit]
        );
        let mut nulled = body(base());
        nulled["fallbacks"] = Value::Null;
        assert!(body_betas(&nulled).is_empty(), "{nulled}");

        // A thinking override inside a fallback target asks for its betas
        // like the top-level one: the attempt is validated as a direct
        // request to that model.
        use mu_anthropic::{PrefixMismatchBehavior, ThinkingConfig, ThinkingDisplay};
        let target = mu_anthropic::FallbackTarget {
            thinking: Some(
                ThinkingConfig::adaptive()
                    .with_display(ThinkingDisplay::Updates)
                    .with_prefix_mismatch_behavior(PrefixMismatchBehavior::DropBlock),
            ),
            ..FallbackTarget::model("claude-opus-5")
        };
        assert_eq!(
            body_betas(&body(
                base().with_fallbacks(Fallbacks::Models(vec![target]))
            )),
            vec![
                Beta::ThinkingDisplayUpdates,
                Beta::ThinkingBindingControls,
                Beta::ServerSideFallback
            ]
        );
    }

    /// One assembly path for the lane and the tests, in two lists that
    /// degrade differently: the refusal latch drops the catalog beta and
    /// nothing else, and the header carries catalog first, body after.
    #[test]
    fn request_betas_keep_catalog_and_body_apart() {
        let catalog = mu_core::model_catalog::built_in();
        let body = serde_json::to_value(MessagesRequest::new(
            "x",
            1,
            vec![
                AnthMessage::user("hi"),
                AnthMessage::turn_scoped("Batch your reads."),
            ],
        ))
        .unwrap();
        let live = RequestBetas::resolve(
            &catalog,
            "claude-fable-5-1",
            ANTHROPIC_API_BASE,
            None,
            false,
            &body,
        );
        assert_eq!(live.catalog, vec![Beta::MidConversationToolChanges]);
        assert_eq!(live.body, vec![Beta::MidConversationSystemClearAt]);
        assert_eq!(
            live.all(),
            vec![
                Beta::MidConversationToolChanges,
                Beta::MidConversationSystemClearAt
            ]
        );
        let latched = RequestBetas::resolve(
            &catalog,
            "claude-fable-5-1",
            ANTHROPIC_API_BASE,
            None,
            true,
            &body,
        );
        assert!(latched.catalog.is_empty());
        assert_eq!(latched.body, live.body);
        let plain = RequestBetas::resolve(
            &catalog,
            "claude-sonnet-5",
            ANTHROPIC_API_BASE,
            None,
            false,
            &serde_json::json!({"model": "x", "messages": [], "max_tokens": 1}),
        );
        assert!(plain.catalog.is_empty() && plain.body.is_empty());
    }

    /// What map_tools does when a tool is appended between turns: the
    /// `cache_control` marker moves to the new last tool and the earlier
    /// tool's definition is otherwise unchanged. That alone does NOT keep
    /// the cache warm — tools precede `system` on the wire, so the bytes
    /// under the system marker shift too — which is why the beta header
    /// exists; the live test below is what proves the cache survives.
    #[test]
    fn appending_a_tool_between_turns_only_moves_the_cache_marker() {
        let read = ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            input_schema: serde_json::json!({"type":"object","properties":{"path":{"type":"string"}}}),
            ..ToolSpec::default()
        };
        let grep = ToolSpec {
            name: "grep".into(),
            description: "search files".into(),
            input_schema: serde_json::json!({"type":"object","properties":{"pattern":{"type":"string"}}}),
            ..ToolSpec::default()
        };
        let turn1 = map_tools(std::slice::from_ref(&read), true, CacheTtl::default());
        let turn2 = map_tools(&[read, grep], true, CacheTtl::default());
        let j1: Vec<Value> = turn1
            .iter()
            .map(|t| serde_json::to_value(t).unwrap())
            .collect();
        let j2: Vec<Value> = turn2
            .iter()
            .map(|t| serde_json::to_value(t).unwrap())
            .collect();

        // Turn 1: the only tool carries the marker.
        assert!(j1[0].get("cache_control").is_some());
        // Turn 2: the marker moved to the appended tool; the earlier tool is
        // the same definition minus the marker, nothing else changed. (The
        // marker itself moving is a byte change — see the docstring.)
        assert!(j2[0].get("cache_control").is_none());
        assert!(j2[1].get("cache_control").is_some());
        let mut earlier = j1[0].clone();
        earlier.as_object_mut().unwrap().remove("cache_control");
        assert_eq!(j2[0], earlier);
    }

    // ----------------------------------------------------------------------
    // mu-anthropic-protocol-2026q3-6uqho.6: per-model request rules
    // ----------------------------------------------------------------------

    /// The catalog strings and the enum are one mapping: every quirk parses
    /// back from its own string, the strings are the snake-case variant
    /// names the catalog comment documents, an unknown string parses to
    /// nothing, and every beta's header value carries its date.
    #[test]
    fn quirk_and_beta_string_forms_round_trip() {
        for q in Quirk::iter() {
            assert_eq!(q.as_str().parse::<Quirk>(), Ok(q), "{q}");
            assert_eq!(q.to_string(), q.as_str());
            assert!(
                q.as_str()
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_'),
                "{q}"
            );
        }
        assert!("thinking_counts_against_max_tokens"
            .parse::<Quirk>()
            .is_err());
        assert!("".parse::<Quirk>().is_err());
        assert_eq!(Quirk::iter().count(), 9);
        assert_eq!(
            Quirk::RejectsThinkingDisabledAboveHighEffort.as_str(),
            "rejects_thinking_disabled_above_high_effort"
        );
        let headers: Vec<&str> = Beta::iter().map(Beta::as_str).collect();
        assert_eq!(headers.len(), 7);
        for h in &headers {
            assert!(
                h.ends_with(|c: char| c.is_ascii_digit()),
                "{h} carries no date"
            );
            assert_eq!(headers.iter().filter(|x| x == &h).count(), 1, "{h}");
        }
        assert_eq!(
            Beta::MidConversationToolChanges.to_string(),
            "mid-conversation-tool-changes-2026-07-01"
        );
    }

    /// The rule quirks the shipped catalog grants, model by model, against
    /// the 2026-09-04 spec snapshot (the thinking table on
    /// thinking-troubleshooting, the sampling note on thinking, the Fable
    /// 5.1 breaking changes, the fast-mode notes, the deprecations table),
    /// and no rule for the models those pages leave alone. Date-stamped ids
    /// inherit by prefix; the per-family output ceiling survives the split
    /// into per-model rules.
    #[test]
    fn shipped_catalog_states_each_documented_rule_and_no_other() {
        let catalog = mu_core::model_catalog::built_in();
        // Only the rule quirks: the tool-changes beta has its own test above,
        // and a local model's serving quirks are not quirks the lane knows.
        let rules = |model: &str| {
            let mut q = Quirk::resolve(&catalog, model);
            q.retain(|q| *q != Quirk::MidConversationToolChanges);
            q
        };
        let expect = |models: &[&str], quirks: &[Quirk]| {
            for model in models {
                assert_eq!(rules(model), quirks, "{model}");
            }
        };
        expect(
            &[
                "claude-fable-5-1",
                "claude-mythos-5-1",
                "claude-fable-5-1-20261001",
            ],
            &[
                Quirk::RejectsManualThinking,
                Quirk::RejectsThinkingDisabled,
                Quirk::RejectsSamplingParams,
                Quirk::RejectsForcedToolChoice,
            ],
        );
        expect(
            &["claude-fable-5", "claude-mythos-5"],
            &[
                Quirk::RejectsManualThinking,
                Quirk::RejectsThinkingDisabled,
                Quirk::RejectsSamplingParams,
            ],
        );
        expect(
            &["claude-opus-5", "claude-opus-5-20260724"],
            &[
                Quirk::RejectsManualThinking,
                Quirk::RejectsThinkingDisabledAboveHighEffort,
                Quirk::RejectsSamplingParams,
            ],
        );
        expect(
            &[
                "claude-sonnet-5",
                "claude-opus-4-8",
                "claude-opus-4-8-20260901",
            ],
            &[Quirk::RejectsManualThinking, Quirk::RejectsSamplingParams],
        );
        expect(
            &["claude-opus-4-7"],
            &[
                Quirk::RejectsManualThinking,
                Quirk::RejectsSamplingParams,
                Quirk::RejectsFastMode,
            ],
        );
        expect(&["claude-opus-4-6"], &[Quirk::IgnoresFastMode]);
        expect(
            &[
                "claude-opus-4-1",
                "claude-opus-4-1-20250805",
                "claude-opus-4-20250514",
                "claude-sonnet-4-20250514",
            ],
            &[Quirk::Retired],
        );
        expect(
            &[
                "claude-sonnet-4-6",
                "claude-haiku-4-5",
                "claude-opus-4-5-20251101",
                "qwen3.6:27b",
                "",
            ],
            &[],
        );
        for model in [
            "claude-fable-5-1",
            "claude-mythos-5-1",
            "claude-opus-5-20260724",
            "claude-opus-4-7-20260301",
            "claude-opus-4-6",
            // Retired, but a gateway may still serve them: the family
            // ceiling survives the rule that marks them.
            "claude-opus-4-1-20250805",
            "claude-opus-4-20250514",
        ] {
            assert_eq!(
                catalog.resolve_model(model).max_output_tokens,
                Some(128000),
                "{model}"
            );
        }
        assert_eq!(
            catalog
                .resolve_model("claude-sonnet-4-20250514")
                .max_output_tokens,
            Some(8192)
        );
    }

    /// Every Claude id the catalog knows, on both wire paths, at every
    /// effort the lane can be asked for: the body mu builds trips no rule.
    /// That is the wire-absence half of the contract, stated on the fields
    /// themselves too — no `tool_choice`, `speed`, sampling or `fallbacks`,
    /// and `thinking` either absent or `adaptive` — so a change that starts
    /// sending one of them has to come through the rules.
    #[test]
    fn shipped_shaping_trips_no_rule_on_any_cataloged_claude_model() {
        let catalog = mu_core::model_catalog::built_in();
        let messages = vec![AgentMessage::User {
            content: "hi".into(),
        }];
        let tools = vec![ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            input_schema: json!({"type": "object"}),
            ..Default::default()
        }];
        let projection = build_projection_with_cache_strategy(Some("sys"), &messages, &tools);
        let models: Vec<String> = catalog
            .models
            .values()
            .filter_map(|m| m.model.clone())
            .filter(|m| m.starts_with("claude-"))
            .collect();
        assert!(models.len() >= 9, "{models:?}");
        for model in &models {
            for effort in [
                None,
                Some("low"),
                Some("medium"),
                Some("high"),
                Some("xhigh"),
                Some("max"),
            ] {
                let legacy = {
                    let mut b = build_request_body_with_catalog(
                        &catalog,
                        model,
                        Some("sys"),
                        &messages,
                        &tools,
                    );
                    apply_thinking(&mut b, effort);
                    b
                };
                let projected = {
                    let mut b = build_request_body_from_projection(
                        model,
                        &projection,
                        &tools,
                        CacheTtl::default(),
                    );
                    apply_thinking(&mut b, effort);
                    b
                };
                for body in [legacy, projected] {
                    assert_eq!(body["model"], json!(model));
                    let hits = model_rule_hits(&catalog, &body, true);
                    assert!(hits.is_empty(), "{model} at {effort:?}: {hits:?}");
                    let obj = body.as_object().unwrap();
                    for key in [
                        "tool_choice",
                        "speed",
                        "temperature",
                        "top_p",
                        "top_k",
                        "fallbacks",
                    ] {
                        assert!(
                            !obj.contains_key(key),
                            "{model} at {effort:?} carries `{key}`"
                        );
                    }
                    let thinking_type = body
                        .get("thinking")
                        .and_then(|t| t.get("type"))
                        .and_then(Value::as_str);
                    assert!(
                        matches!(thinking_type, None | Some("adaptive")),
                        "{model} at {effort:?}: thinking {thinking_type:?}"
                    );
                }
            }
        }
    }

    /// Each rule fires on exactly the shape it names, on a model the catalog
    /// grants it to and not on one it does not, with the model, the field
    /// and the quirk in the message; the shape rules refuse, `retired` and
    /// `ignores_fast_mode` warn. Bodies come from mu-anthropic's typed
    /// request so the crate and the rules agree on the wire shape.
    #[test]
    fn each_rule_refuses_the_shape_it_names() {
        use mu_anthropic::{OutputConfig, Speed, ThinkingConfig, ToolChoice};
        let catalog = mu_core::model_catalog::built_in();
        let base = |model: &str| MessagesRequest::new(model, 1, vec![AnthMessage::user("hi")]);
        let hits = |req: MessagesRequest, on_api: bool| {
            model_rule_hits(&catalog, &serde_json::to_value(req).unwrap(), on_api)
        };
        let one = |req: MessagesRequest, quirk: Quirk, severity: RuleSeverity| {
            let found = hits(req, true);
            assert_eq!(found.len(), 1, "{found:?}");
            assert_eq!(found[0].quirk, quirk);
            assert_eq!(found[0].severity, severity);
            found[0].clone()
        };
        let none = |req: MessagesRequest| {
            let found = hits(req, true);
            assert!(found.is_empty(), "{found:?}");
        };
        let effort = |level: &str| OutputConfig {
            effort: Some(level.into()),
            ..Default::default()
        };

        // Forced tool choice: the 5.1 pair only; auto and none pass everywhere.
        let hit = one(
            base("claude-fable-5-1").with_tool_choice(ToolChoice::any()),
            Quirk::RejectsForcedToolChoice,
            RuleSeverity::Refuse,
        );
        assert!(
            hit.message().contains("claude-fable-5-1"),
            "{}",
            hit.message()
        );
        assert!(
            hit.message().contains("`any`") || hit.message().contains("\"any\""),
            "{}",
            hit.message()
        );
        assert!(hit
            .message()
            .contains(Quirk::RejectsForcedToolChoice.as_str()));
        one(
            base("claude-mythos-5-1").with_tool_choice(ToolChoice::tool("read")),
            Quirk::RejectsForcedToolChoice,
            RuleSeverity::Refuse,
        );
        none(base("claude-fable-5-1").with_tool_choice(ToolChoice::auto()));
        none(base("claude-fable-5-1").with_tool_choice(ToolChoice::None));
        none(base("claude-opus-5").with_tool_choice(ToolChoice::any()));
        none(base("claude-fable-5").with_tool_choice(ToolChoice::tool("read")));

        // Manual thinking: every 4.7+ model; Opus 4.6 still takes it (deprecated).
        for model in [
            "claude-sonnet-5",
            "claude-opus-4-7",
            "claude-fable-5-1",
            "claude-opus-5",
        ] {
            one(
                base(model).with_thinking(ThinkingConfig::enabled(1024)),
                Quirk::RejectsManualThinking,
                RuleSeverity::Refuse,
            );
        }
        none(base("claude-opus-4-6").with_thinking(ThinkingConfig::enabled(1024)));

        // Disabled thinking: the always-on models refuse it outright; Opus 5
        // refuses it at xhigh/max only (the API default effort is high);
        // Sonnet 5 accepts it at every effort.
        one(
            base("claude-fable-5-1").with_thinking(ThinkingConfig::Disabled),
            Quirk::RejectsThinkingDisabled,
            RuleSeverity::Refuse,
        );
        none(base("claude-opus-5").with_thinking(ThinkingConfig::Disabled));
        none(
            base("claude-opus-5")
                .with_thinking(ThinkingConfig::Disabled)
                .with_output_config(effort("high")),
        );
        for level in ["xhigh", "max"] {
            let hit = one(
                base("claude-opus-5")
                    .with_thinking(ThinkingConfig::Disabled)
                    .with_output_config(effort(level)),
                Quirk::RejectsThinkingDisabledAboveHighEffort,
                RuleSeverity::Refuse,
            );
            assert!(hit.detail.contains(level), "{}", hit.detail);
        }
        none(
            base("claude-opus-5")
                .with_thinking(ThinkingConfig::adaptive())
                .with_output_config(effort("max")),
        );
        none(
            base("claude-sonnet-5")
                .with_thinking(ThinkingConfig::Disabled)
                .with_output_config(effort("max")),
        );

        // Sampling: presence of any of the three, named in the message.
        let hit = one(
            base("claude-sonnet-5").with_temperature(0.7),
            Quirk::RejectsSamplingParams,
            RuleSeverity::Refuse,
        );
        assert!(hit.detail.contains("`temperature`"), "{}", hit.detail);
        let mut with_top_k = base("claude-opus-4-8").with_top_p(0.9);
        with_top_k.top_k = Some(40);
        let hit = one(
            with_top_k,
            Quirk::RejectsSamplingParams,
            RuleSeverity::Refuse,
        );
        assert!(hit.detail.contains("`top_p`, `top_k`"), "{}", hit.detail);
        none(base("claude-sonnet-4-6").with_temperature(0.7));

        // Fast mode: an error on Opus 4.7, disregarded on Opus 4.6, offered
        // on Opus 5; `standard` is never a hit.
        one(
            base("claude-opus-4-7").with_speed(Speed::Fast),
            Quirk::RejectsFastMode,
            RuleSeverity::Refuse,
        );
        one(
            base("claude-opus-4-6").with_speed(Speed::Fast),
            Quirk::IgnoresFastMode,
            RuleSeverity::Warn,
        );
        none(base("claude-opus-5").with_speed(Speed::Fast));
        none(base("claude-opus-4-7").with_speed(Speed::Standard));

        // Retired: a warning on Anthropic's own endpoint (the request goes;
        // Anthropic's not-found is the authority), nothing at all elsewhere.
        for model in [
            "claude-opus-4-1-20250805",
            "claude-opus-4-20250514",
            "claude-sonnet-4-20250514",
        ] {
            let hit = one(base(model), Quirk::Retired, RuleSeverity::Warn);
            assert!(hit.detail.contains("sending anyway"), "{}", hit.detail);
            assert!(hits(base(model), false).is_empty(), "{model} off the API");
        }
        none(base("claude-opus-4-8"));

        // Every other rule is endpoint-gated the way the beta header is:
        // the same body that is refused on api.anthropic.com is sent with a
        // warning from a gateway, and the warning says so.
        let found = hits(base("claude-fable-5-1").with_temperature(0.7), false);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].quirk, Quirk::RejectsSamplingParams);
        assert_eq!(found[0].severity, RuleSeverity::Warn);
        assert!(
            found[0].detail.contains("sent anyway"),
            "{}",
            found[0].detail
        );
        let found = hits(base("claude-opus-4-7").with_speed(Speed::Fast), false);
        assert_eq!(found[0].severity, RuleSeverity::Warn);

        // Several violations report as several hits, in rule order.
        let found = hits(
            base("claude-fable-5-1")
                .with_tool_choice(ToolChoice::any())
                .with_temperature(0.5),
            true,
        );
        assert_eq!(
            found.iter().map(|h| h.quirk).collect::<Vec<_>>(),
            vec![Quirk::RejectsSamplingParams, Quirk::RejectsForcedToolChoice]
        );
    }

    /// The ollama switch is the one mu-built shape a rule names: `--thinking
    /// off` on that lane sends `thinking: {type: "disabled"}`. That lane is
    /// never Anthropic's own endpoint, so on a Claude-tagged model behind it
    /// the hit is a warning and the request goes; the same body on the API
    /// itself is refused. A model that accepts `disabled` (Sonnet 5) trips
    /// nothing either way.
    #[test]
    fn ollama_switch_disabled_thinking_warns_off_the_api_and_refuses_on_it() {
        let catalog = mu_core::model_catalog::built_in();
        let messages = vec![AgentMessage::User {
            content: "hi".into(),
        }];
        let body = |model: &str| {
            let mut b = build_request_body_with_catalog(&catalog, model, None, &messages, &[]);
            apply_ollama_thinking(&mut b, Some("off"));
            assert_eq!(b["thinking"]["type"], "disabled");
            b
        };
        let off = model_rule_hits(&catalog, &body("claude-fable-5-1"), false);
        assert_eq!(off.len(), 1, "{off:?}");
        assert_eq!(off[0].quirk, Quirk::RejectsThinkingDisabled);
        assert_eq!(off[0].severity, RuleSeverity::Warn);
        let on = model_rule_hits(&catalog, &body("claude-fable-5-1"), true);
        assert_eq!(on.len(), 1, "{on:?}");
        assert_eq!(on[0].severity, RuleSeverity::Refuse);
        for on_api in [false, true] {
            let found = model_rule_hits(&catalog, &body("claude-sonnet-5"), on_api);
            assert!(found.is_empty(), "{found:?}");
        }
    }

    /// An explicit fallback target is validated like a direct request to
    /// its own model: its overrides (`thinking`, `output_config`, `speed`)
    /// replace the top level's for that check, and the fields a target
    /// cannot override (`tool_choice`, sampling) are read from the top
    /// level. `fallbacks: "default"` names no model and is not checked.
    #[test]
    fn fallback_targets_are_checked_against_their_own_model() {
        use mu_anthropic::{FallbackTarget, Fallbacks, OutputConfig, ThinkingConfig, ToolChoice};
        let catalog = mu_core::model_catalog::built_in();
        let base = |model: &str| MessagesRequest::new(model, 1, vec![AnthMessage::user("hi")]);
        let hits = |req: MessagesRequest| {
            model_rule_hits(&catalog, &serde_json::to_value(req).unwrap(), true)
        };
        let effort = |level: &str| OutputConfig {
            effort: Some(level.into()),
            ..Default::default()
        };

        // Opus 4.8 accepts forced tool use; its Fable 5.1 target does not.
        let found = hits(
            base("claude-opus-4-8")
                .with_tool_choice(ToolChoice::any())
                .with_fallbacks(Fallbacks::Models(vec![FallbackTarget::model(
                    "claude-fable-5-1",
                )])),
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].model, "claude-fable-5-1");
        assert_eq!(found[0].quirk, Quirk::RejectsForcedToolChoice);

        // Disabled thinking at max effort passes on Opus 4.8 and trips the
        // Opus 5 target, which inherits both fields.
        let found = hits(
            base("claude-opus-4-8")
                .with_thinking(ThinkingConfig::Disabled)
                .with_output_config(effort("max"))
                .with_fallbacks(Fallbacks::Models(vec![FallbackTarget::model(
                    "claude-opus-5",
                )])),
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].model, "claude-opus-5");
        assert_eq!(
            found[0].quirk,
            Quirk::RejectsThinkingDisabledAboveHighEffort
        );

        // The same target with its own effort override at high passes.
        let mut target = FallbackTarget::model("claude-opus-5");
        target.output_config = Some(effort("high"));
        let found = hits(
            base("claude-opus-4-8")
                .with_thinking(ThinkingConfig::Disabled)
                .with_output_config(effort("max"))
                .with_fallbacks(Fallbacks::Models(vec![target])),
        );
        assert!(found.is_empty(), "{found:?}");

        // Server-chosen targets are Anthropic's to validate.
        let found = hits(base("claude-fable-5-1").with_fallbacks(Fallbacks::Default));
        assert!(found.is_empty(), "{found:?}");
    }

    /// The two identity headers are read by name and only when present.
    #[test]
    fn response_identity_reads_request_and_workspace_ids() {
        use reqwest::header::{HeaderMap, HeaderValue};
        let mut headers = HeaderMap::new();
        assert_eq!(response_identity(&headers), (None, None));
        headers.insert(
            "request-id",
            HeaderValue::from_static("req_018EeWyXxfu5pfWkrYcMdjWG"),
        );
        assert_eq!(
            response_identity(&headers),
            (Some("req_018EeWyXxfu5pfWkrYcMdjWG".into()), None)
        );
        headers.insert(
            "anthropic-workspace-id",
            HeaderValue::from_static("wrkspc_01JwQvzr7rXLA5AGx3HKfFUJ"),
        );
        assert_eq!(
            response_identity(&headers),
            (
                Some("req_018EeWyXxfu5pfWkrYcMdjWG".into()),
                Some("wrkspc_01JwQvzr7rXLA5AGx3HKfFUJ".into())
            )
        );
    }

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

    /// mu-anthropic-protocol-2026q3-6uqho.1: with the mid-conversation
    /// tool-changes beta on the wire, appending a tool between two turns
    /// must still read the prompt cache written by the first turn.
    /// Only runs when MU_LIVE_ANTHROPIC=1; the model must be one the
    /// beta is documented for (MU_LIVE_ANTHROPIC_MODEL, default Opus 4.8).
    #[tokio::test]
    async fn live_tool_list_change_keeps_prompt_cache() {
        if !live_enabled() {
            eprintln!("skipping live_tool_list_change_keeps_prompt_cache (set MU_LIVE_ANTHROPIC=1 to run)");
            return;
        }
        let model =
            std::env::var("MU_LIVE_ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-opus-4-8".into());
        assert!(
            !beta_headers_for(
                mu_core::model_catalog::global(),
                &model,
                ANTHROPIC_API_BASE,
                None
            )
            .is_empty(),
            "{model} does not carry the mid_conversation_tool_changes quirk in the catalog"
        );
        let provider = AnthropicProvider::from_env(model.clone())
            .expect("ANTHROPIC_API_KEY must be set when MU_LIVE_ANTHROPIC=1");

        // A system prompt comfortably above the model's minimum cacheable
        // prefix (1024 tokens on the Opus/Fable families).
        let filler = "You are a careful assistant. Answer with one word. ".repeat(220);
        let messages = vec![AgentMessage::User {
            content: "Say ok.".into(),
        }];
        let tool = |name: &str, desc: &str| ToolSpec {
            name: name.into(),
            description: desc.into(),
            input_schema: json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
            display: None,
            when: None,
            policy: Default::default(),
            ..Default::default()
        };
        let turn1 = vec![tool("read", "Read a file")];
        let turn2 = vec![tool("read", "Read a file"), tool("grep", "Search files")];

        async fn usage_of(
            provider: &AnthropicProvider,
            model: &str,
            system: &str,
            messages: &[AgentMessage],
            tools: &[ToolSpec],
        ) -> Usage {
            let projection = build_projection_with_cache_strategy(Some(system), messages, tools);
            let body =
                build_request_body_from_projection(model, &projection, tools, CacheTtl::default());
            let resp = provider
                .messages_request_with(&body, mu_core::model_catalog::global(), None)
                .send()
                .await
                .expect("send");
            assert!(
                resp.status().is_success(),
                "anthropic returned {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            );
            let (_tx, rx) = tokio::sync::oneshot::channel();
            let mut stream = events_stream(
                resp.bytes_stream(),
                rx,
                Some(DEFAULT_MAX_TOOL_CALL_BYTES),
                None,
            );
            while let Some(event) = stream.next().await {
                match event {
                    ProviderEvent::Done(msg) => return msg.usage.expect("usage on Done"),
                    ProviderEvent::Error(e) => panic!("anthropic error: {e}"),
                    _ => {}
                }
            }
            panic!("stream ended without Done");
        }

        let first = usage_of(&provider, &model, &filler, &messages, &turn1).await;
        let second = usage_of(&provider, &model, &filler, &messages, &turn2).await;
        eprintln!("turn 1 usage: {first:?}\nturn 2 usage: {second:?}");
        assert!(
            first.cache_creation_input_tokens.unwrap_or(0) > 0
                || first.cache_read_input_tokens.unwrap_or(0) > 0,
            "turn 1 neither wrote nor read the cache: {first:?}"
        );
        assert!(
            second.cache_read_input_tokens.unwrap_or(0) > 0,
            "turn 2 read nothing from the cache after the tool list changed: {second:?}"
        );
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

// ============================================================================
// mu-c9b2l — a cut-off tool call is legible on the Messages wire too, and the
// stream stops at the cap
// ============================================================================

/// One Anthropic SSE frame: the `event:` line and its single-line `data:`
/// payload, as its own chunk so a test can count how many the accumulator
/// actually pulled.
fn cut_frame(kind: &str, value: serde_json::Value) -> Bytes {
    Bytes::from(format!("event: {kind}\ndata: {value}\n\n"))
}

fn tool_use_start_frame(index: u32, id: &str, name: &str) -> Bytes {
    cut_frame(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}
        }),
    )
}

fn input_json_frame(index: u32, partial: &str) -> Bytes {
    cut_frame(
        "content_block_delta",
        json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "input_json_delta", "partial_json": partial}
        }),
    )
}

fn message_delta_frame(stop_reason: &str) -> Bytes {
    cut_frame(
        "message_delta",
        json!({"type": "message_delta", "delta": {"stop_reason": stop_reason}}),
    )
}

fn message_stop_frame() -> Bytes {
    cut_frame("message_stop", json!({"type": "message_stop"}))
}

/// A byte source that counts the frames actually pulled out of it, so a test
/// can assert the accumulator stopped reading rather than merely stopped
/// accumulating.
fn counted_frames(
    frames: Vec<Bytes>,
) -> (
    impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let pulled = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = pulled.clone();
    let stream = futures::stream::iter(frames).map(move |frame| {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok::<_, std::io::Error>(frame)
    });
    (stream, pulled)
}

async fn drain_to_done(mut stream: BoxStream<'static, ProviderEvent>) -> AssistantMessage {
    while let Some(event) = stream.next().await {
        if let ProviderEvent::Done(msg) = event {
            return msg;
        }
    }
    panic!("stream ended without Done");
}

fn only_tool_call(msg: &AssistantMessage) -> &ToolCall {
    let calls: Vec<&ToolCall> = msg
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolCall(tc) => Some(tc),
            _ => None,
        })
        .collect();
    assert_eq!(
        calls.len(),
        1,
        "expected one tool call, got {:?}",
        msg.content
    );
    calls[0]
}

/// The measured failure, on the Messages wire: a `write` whose input runs past
/// the ceiling. The cap ends the message at the moment the block is already
/// too big — the frames after it are never pulled — and the call goes out
/// marked cut, not as `{}`.
#[tokio::test]
async fn mu_c9b2l_byte_cap_cuts_the_call_and_stops_reading() {
    const CAP: usize = 64;
    let head = r#"{"path":"/tmp/big","content":""#;
    let filler = "x".repeat(40);

    let mut frames = vec![
        tool_use_start_frame(0, "toolu_big", "write"),
        input_json_frame(0, head),
    ];
    for _ in 0..8 {
        frames.push(input_json_frame(0, &filler));
    }
    // A second block, and the terminators, that the accumulator must not reach.
    frames.push(tool_use_start_frame(1, "toolu_after", "read"));
    frames.push(input_json_frame(1, r#"{"path":"/tmp/after"}"#));
    frames.push(message_delta_frame("tool_use"));
    frames.push(message_stop_frame());
    let total_frames = frames.len();

    let (bytes, pulled) = counted_frames(frames);
    let (_tx, rx) = tokio::sync::oneshot::channel();
    let done = drain_to_done(test_events_stream_budgeted(bytes, rx, Some(CAP), None)).await;

    // start + head (30 bytes) + one 40-byte fragment crosses 64 — three
    // frames read.
    assert_eq!(
        pulled.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "the stream must stop at the cap, not read to the end ({total_frames} frames available)"
    );
    assert_eq!(done.stop_reason, StopReason::MaxTokens);

    let call = only_tool_call(&done);
    assert_eq!(call.name, "write");
    assert_eq!(call.id, "toolu_big");
    let cut = mu_core::agent::tool_call_cut::detect(call.arguments.as_value())
        .expect("the emitted call carries the cut marker, not an empty object");
    assert_eq!(cut.cause, mu_core::agent::CutCause::ByteCap);
    assert_eq!(cut.bytes, head.len() + 40);
    assert!(cut.bytes > CAP, "cut recorded at {} bytes", cut.bytes);
    // mu-c9b2l: the cap in force rides along, so the refusal quotes the
    // session's limit rather than the compile-time default.
    assert_eq!(cut.cap, Some(CAP));
}

/// `stop_reason: "max_tokens"` landing mid-input is the same event seen from
/// the model's side: it ran out of room, so the unparseable input is a cut
/// rather than a malformed call. The request's `max_tokens` — which only this
/// layer sees — rides along, so the refusal advises parts the model has room
/// to emit.
#[tokio::test]
async fn mu_c9b2l_stop_reason_max_tokens_mid_input_is_a_cut() {
    // The unknown-model floor, asserted against the DEFAULT catalog so an
    // operator's models.toml cannot move it (bead mu-nzxa).
    assert_eq!(
        crate::providers::output_limits::max_tokens_for_model_with_catalog(
            &mu_core::model_catalog::built_in(),
            "some-future-model-v9",
        ),
        4096
    );
    let budget = mu_core::agent::tool_call_cut::output_budget_bytes(4096);

    let partial = r#"{"path":"/tmp/game.py","content":"import pygame"#;
    let frames = vec![
        tool_use_start_frame(0, "toolu_len", "write"),
        input_json_frame(0, partial),
        message_delta_frame("max_tokens"),
        message_stop_frame(),
    ];
    let (bytes, _pulled) = counted_frames(frames);
    let (_tx, rx) = tokio::sync::oneshot::channel();
    let done = drain_to_done(test_events_stream_budgeted(
        bytes,
        rx,
        Some(DEFAULT_MAX_TOOL_CALL_BYTES),
        Some(budget),
    ))
    .await;

    assert_eq!(done.stop_reason, StopReason::MaxTokens);
    let call = only_tool_call(&done);
    let cut = mu_core::agent::tool_call_cut::detect(call.arguments.as_value())
        .expect("a max_tokens-truncated call is marked cut");
    assert_eq!(cut.cause, mu_core::agent::CutCause::OutputLimit);
    assert_eq!(cut.bytes, partial.len());
    assert_eq!(cut.cap, Some(DEFAULT_MAX_TOOL_CALL_BYTES));
    assert_eq!(cut.budget_bytes, Some(budget));

    let text = mu_core::agent::tool_call_cut::refusal_text("write", &cut);
    assert!(text.contains("under ~6 KB"), "{text}");
    assert!(!text.contains("~16 KB"), "{text}");
}

/// No `max_tokens` stop reason, just input that ends mid-string: serde_json
/// reports EOF, which is the same cut by another route.
#[tokio::test]
async fn mu_c9b2l_input_ending_mid_string_is_a_cut() {
    let partial = r#"{"path":"/tmp/x","content":"half a fi"#;
    let frames = vec![
        tool_use_start_frame(0, "toolu_eof", "write"),
        input_json_frame(0, partial),
        message_delta_frame("tool_use"),
        message_stop_frame(),
    ];
    let (bytes, _pulled) = counted_frames(frames);
    let (_tx, rx) = tokio::sync::oneshot::channel();
    let done = drain_to_done(test_events_stream(bytes, rx)).await;

    let call = only_tool_call(&done);
    let cut = mu_core::agent::tool_call_cut::detect(call.arguments.as_value())
        .expect("an EOF-truncated input is marked cut");
    assert_eq!(cut.cause, mu_core::agent::CutCause::TruncatedJson);
    assert_eq!(cut.bytes, partial.len());
}

/// Parity: a call under the cap streams and parses exactly as before — no
/// marker, input intact, every frame read.
#[tokio::test]
async fn mu_c9b2l_call_under_the_cap_is_unchanged() {
    let content = "y".repeat(4096);
    let input = json!({"path": "/tmp/ok.txt", "content": content}).to_string();
    assert!(input.len() < DEFAULT_MAX_TOOL_CALL_BYTES);

    let frames = vec![
        tool_use_start_frame(0, "toolu_ok", "write"),
        input_json_frame(0, &input),
        message_delta_frame("tool_use"),
        message_stop_frame(),
    ];
    let expected_frames = frames.len();
    let (bytes, pulled) = counted_frames(frames);
    let (_tx, rx) = tokio::sync::oneshot::channel();
    let done = drain_to_done(test_events_stream(bytes, rx)).await;

    assert_eq!(
        pulled.load(std::sync::atomic::Ordering::SeqCst),
        expected_frames
    );
    assert_eq!(done.stop_reason, StopReason::ToolUse);
    let call = only_tool_call(&done);
    assert!(
        mu_core::agent::tool_call_cut::detect(call.arguments.as_value()).is_none(),
        "an under-cap call carries no cut marker"
    );
    assert_eq!(call.arguments.as_value()["path"], "/tmp/ok.txt");
    assert_eq!(call.arguments.as_value()["content"], content);
}

/// `[session].max_tool_call_bytes = 0` reads to the model's own ceiling, as
/// before this bead.
#[tokio::test]
async fn mu_c9b2l_disabled_cap_reads_the_whole_oversized_call() {
    let content = "z".repeat(64 * 1024);
    let input = json!({"path": "/tmp/huge.txt", "content": content}).to_string();
    assert!(input.len() > DEFAULT_MAX_TOOL_CALL_BYTES);

    let frames = vec![
        tool_use_start_frame(0, "toolu_huge", "write"),
        input_json_frame(0, &input),
        message_delta_frame("tool_use"),
        message_stop_frame(),
    ];
    let expected_frames = frames.len();
    let (bytes, pulled) = counted_frames(frames);
    let (_tx, rx) = tokio::sync::oneshot::channel();
    let done = drain_to_done(test_events_stream_budgeted(bytes, rx, None, None)).await;

    assert_eq!(
        pulled.load(std::sync::atomic::Ordering::SeqCst),
        expected_frames,
        "no cap means no early abort"
    );
    let call = only_tool_call(&done);
    assert!(mu_core::agent::tool_call_cut::detect(call.arguments.as_value()).is_none());
    assert_eq!(call.arguments.as_value()["content"], content);
}

/// The config's `0` (cap disabled) reaches the accumulator as `None`, the
/// same way the openai-chat builder handles it.
#[test]
fn mu_c9b2l_zero_cap_disables_the_ceiling() {
    let provider = AnthropicProvider::new("k".into(), "m".into());
    assert_eq!(
        provider.max_tool_call_bytes,
        Some(DEFAULT_MAX_TOOL_CALL_BYTES)
    );
    assert_eq!(
        AnthropicProvider::new("k".into(), "m".into())
            .with_max_tool_call_bytes(Some(0))
            .max_tool_call_bytes,
        None
    );
    assert_eq!(
        AnthropicProvider::new("k".into(), "m".into())
            .with_max_tool_call_bytes(Some(4096))
            .max_tool_call_bytes,
        Some(4096)
    );
}
