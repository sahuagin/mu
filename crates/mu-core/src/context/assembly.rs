//! Project the agent loop's session state into a [`RetainedRope`].
//!
//! mu-fb0: the live agent loop owns a `Vec<AgentMessage>` and an
//! optional system prompt + tool schemas. [`assemble_rope`] is the
//! projection function that turns those into a rope: one span per
//! message (and one span per tool schema and one System span for the
//! system prompt) with retention/cacheability tagged so a downstream
//! [`super::CacheStrategy`] can derive boundary positions.
//!
//! The rope is the *controlled variable* fed to a
//! [`super::ProviderRenderer`]. Renderers are per-provider; this
//! projection is provider-neutral.
//!
//! ## Why a function rather than a method?
//!
//! The agent loop already maintains `messages: Vec<AgentMessage>` as
//! the canonical session state (external input lands there; the
//! provider's `stream()` signature still consumes it). The rope is a
//! per-turn projection, not a stored field — building it from the
//! source of truth keeps the new path equivalence-preserving with the
//! existing wire path.

use crate::agent::tool::ToolSpec;
use crate::agent::types::{AgentMessage, ContentBlock};

use super::recall::{ProjectContext, RecallSource};
use super::{RetainedRope, RetentionClass, Span, SpanKind};

/// Build a rope from the agent loop's per-call inputs.
///
/// Span layout (in order):
/// 1. Optional `System` span from `system_prompt` — `Startup`
///    retention (always present), cacheable.
/// 2. One `ToolSchema` span per `tool_specs` entry — `Hot`
///    retention (registered for the session, stable for cache
///    purposes), cacheable.
/// 3. One span per `messages` entry, in order:
///    - `User` → `SpanKind::User`, `Hot`, uncacheable (volatile).
///    - `Assistant` → `SpanKind::Assistant`, `Hot`, uncacheable. The
///      assistant's text + tool-call blocks are flattened to a JSON
///      content string — same projection used by every renderer.
///    - `ToolResult` → `SpanKind::ToolResult`, `Warm`, uncacheable.
///
/// Retention tagging mirrors `specs/architecture/event-sourced-
/// context.md` lines 567-578: system + tool-schema spans form the
/// stable cacheable prefix; conversational turns are volatile and
/// fall after the cache boundary. The Anthropic strategy in mu-bn4
/// places its `ephemeral` marker on the last stable+cacheable span
/// — i.e., the final tool schema (or the system prompt if no tools).
pub fn assemble_rope(
    system_prompt: Option<&str>,
    messages: &[AgentMessage],
    tool_specs: &[ToolSpec],
) -> RetainedRope {
    assemble_rope_with_context(system_prompt, None, messages, tool_specs)
}

/// mu-phl v0 (bead mu-vm81): build a rope including session-start memory
/// recall + project-file injection.
///
/// Extends [`assemble_rope`] by inserting recalled items between the
/// System span and the ToolSchema spans, preserving the stable cacheable
/// prefix. Each [`super::recall::RecalledItem`] lands as either a
/// `MemoryInjection` or `FileLoad` span (chosen by its
/// [`super::recall::RecallSource`] variant), at `RetentionClass::Startup`
/// (cacheable + stable).
///
/// Span layout (when `project_context` is `Some(...)`):
///
/// ```text
/// [System] | [MemoryInjection: agent memory ...] |
///          | [FileLoad: CLAUDE.md, AGENTS.md, ...] |
///          | [ToolSchema...] | [Conversation...]
/// ```
///
/// When `project_context` is `None`, span layout is identical to
/// [`assemble_rope`] (i.e., `[System] | [ToolSchema...] | [Messages...]`).
///
/// Per `specs/architecture/cache-discipline.md`, session-start injection
/// of immutable program-region content is the safe shape: it expands
/// the cacheable prefix and does not invalidate the cache on each turn.
/// Mid-session re-recall (mu-fb0 live-loop) is out of v0 scope; if
/// landed later it must append-to-conversation per cache-discipline,
/// not rewrite the cached prefix.
pub fn assemble_rope_with_context(
    system_prompt: Option<&str>,
    project_context: Option<&ProjectContext>,
    messages: &[AgentMessage],
    tool_specs: &[ToolSpec],
) -> RetainedRope {
    let recall_count = project_context.map(|c| c.items.len()).unwrap_or(0);
    let mut spans: Vec<Span> = Vec::with_capacity(
        usize::from(system_prompt.is_some()) + recall_count + tool_specs.len() + messages.len(),
    );

    if let Some(prompt) = system_prompt {
        spans.push(Span::new(
            "system-prompt",
            SpanKind::System,
            prompt,
            RetentionClass::Startup,
        ));
    }

    // Recall spans (Memory + ProjectFile) between System and ToolSchemas.
    // Both kinds get RetentionClass::Startup (stable + cacheable), so
    // they extend the cacheable prefix downstream cache strategies
    // can mark — see mu-bn4's AnthropicCacheStrategy.
    if let Some(ctx) = project_context {
        for item in &ctx.items {
            let (kind, id) = match &item.source {
                RecallSource::Memory => (
                    SpanKind::MemoryInjection,
                    format!("memory-recall:{}", item.stable_id),
                ),
                RecallSource::ProjectFile { path } => (
                    SpanKind::FileLoad,
                    format!("project-file:{}", path.display()),
                ),
            };
            spans.push(Span::new(
                id,
                kind,
                item.content.clone(),
                RetentionClass::Startup,
            ));
        }
    }

    if tool_specs.is_empty() {
        spans.push(Span::new(
            "no-tools-clause",
            SpanKind::System,
            "You have no tools available in this session. Do not attempt to use tools or emit tool calls. Answer the user's question directly using only the context provided above.",
            RetentionClass::Startup,
        ));
    } else {
        for spec in tool_specs {
            spans.push(Span::new(
                format!("tool-schema:{}", spec.name),
                SpanKind::ToolSchema,
                format!(
                    "{}\n{}",
                    spec.description,
                    serde_json::to_string(&spec.input_schema).unwrap_or_default(),
                ),
                RetentionClass::Hot,
            ));
        }
    }

    for (idx, message) in messages.iter().enumerate() {
        spans.push(message_to_span(idx, message));
    }

    RetainedRope::from_spans(spans)
}

/// mu-kgu.8: extend a baseline rope with spans for messages that
/// were appended since the baseline was captured.
///
/// `baseline` was produced by a synchronous or background
/// [`crate::context::CompactionPolicy::compact`] call against the
/// first `messages_at_baseline` messages. The agent loop's per-turn
/// render needs the full conversation up to the current turn, so any
/// messages appended since the baseline are appended as fresh spans
/// with indices continuing from `messages_at_baseline`.
///
/// Span ids continue the `msg-{idx}-...` scheme from
/// [`assemble_rope`] so there's no collision with the baseline rope's
/// existing spans (those have indices `[0, messages_at_baseline)`).
pub fn append_messages_to_baseline(
    baseline: &RetainedRope,
    messages_at_baseline: usize,
    messages: &[AgentMessage],
) -> RetainedRope {
    let mut spans: Vec<Span> = baseline.spans().to_vec();
    for (offset, message) in messages.iter().enumerate().skip(messages_at_baseline) {
        spans.push(message_to_span(offset, message));
    }
    RetainedRope::from_spans(spans)
}

fn message_to_span(idx: usize, message: &AgentMessage) -> Span {
    match message {
        AgentMessage::User { content } => Span::with_cacheable(
            format!("msg-{idx}-user"),
            SpanKind::User,
            content.clone(),
            RetentionClass::Hot,
            false,
        ),
        AgentMessage::Assistant(assistant) => Span::with_cacheable(
            format!("msg-{idx}-assistant"),
            SpanKind::Assistant,
            flatten_assistant(&assistant.content),
            RetentionClass::Hot,
            false,
        )
        .with_blocks(assistant.content.clone()),
        AgentMessage::ToolResult {
            call_id,
            content,
            is_error,
        } => {
            let body = if *is_error {
                format!("error: {content}")
            } else {
                content.clone()
            };
            Span::with_cacheable(
                format!("msg-{idx}-tool-result:{call_id}"),
                SpanKind::ToolResult,
                body,
                RetentionClass::Warm,
                false,
            )
        }
    }
}

/// Recover the `call_id` embedded in a `ToolResult` span's id.
///
/// `assemble_rope` (and `append_messages_to_baseline`) build
/// `ToolResult` span ids as `msg-{idx}-tool-result:{call_id}`
/// (see [`message_to_span`]). Phase C wire adapters need to recover
/// the `call_id` to bind a tool result back to its originating tool
/// call without re-walking `Vec<AgentMessage>`. Returns the suffix
/// after `tool-result:` for `ToolResult` span ids; `None` for any
/// other id shape.
pub fn extract_call_id_from_span_id(span_id: &str) -> Option<&str> {
    let (_, rest) = span_id.split_once("-tool-result:")?;
    Some(rest)
}

/// Flatten assistant content blocks into a deterministic JSON-ish
/// projection. Renderers that need block structure can re-derive it
/// from the original [`crate::agent::types::AssistantMessage`]; this
/// projection is for rope provenance and equivalence-test comparison.
fn flatten_assistant(blocks: &[ContentBlock]) -> String {
    let pieces: Vec<String> = blocks
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text } => text.to_string(),
            ContentBlock::ToolCall(tc) => format!(
                "[tool_call:{}({})]",
                tc.name,
                serde_json::to_string(&tc.arguments).unwrap_or_default(),
            ),
            ContentBlock::Thinking { .. } => String::new(),
        })
        .collect();
    pieces.join("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::ToolSpec;
    use crate::agent::types::{AssistantMessage, ContentBlock, StopReason, ToolCall};

    fn sample_tool() -> ToolSpec {
        ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
            }),
            ..Default::default()
        }
    }

    #[test]
    fn assembles_system_then_tools_then_messages_in_order() {
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
        let rope = assemble_rope(Some("you are mu"), &messages, &[sample_tool()]);

        let kinds: Vec<SpanKind> = rope.iter().map(|s| s.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                SpanKind::System,
                SpanKind::ToolSchema,
                SpanKind::User,
                SpanKind::Assistant,
            ]
        );
    }

    #[test]
    fn no_system_prompt_omits_system_span() {
        let rope = assemble_rope(None, &[], &[sample_tool()]);
        assert_eq!(rope.spans()[0].kind, SpanKind::ToolSchema);
    }

    #[test]
    fn system_and_tool_schema_spans_are_cacheable_stable_prefix() {
        let rope = assemble_rope(
            Some("hi"),
            &[AgentMessage::User {
                content: "u".into(),
            }],
            &[sample_tool()],
        );
        // First two spans (system + tool schema) are stable+cacheable;
        // the user message is not. mu-bn4's AnthropicCacheStrategy
        // boundary lands at index 1 (the last tool schema).
        assert!(rope.spans()[0].retention.is_stable() && rope.spans()[0].cacheable);
        assert!(rope.spans()[1].retention.is_stable() && rope.spans()[1].cacheable);
        assert!(!rope.spans()[2].cacheable);
    }

    #[test]
    fn assistant_tool_call_blocks_flatten_into_span_content() {
        let messages = vec![AgentMessage::Assistant(AssistantMessage {
            content: vec![
                ContentBlock::Text {
                    text: "thinking…".into(),
                },
                ContentBlock::ToolCall(ToolCall {
                    id: "c-1".into(),
                    name: "read".into(),
                    arguments: crate::agent::ToolArgs::new(serde_json::json!({"path": "/x"}))
                        .unwrap(),
                }),
            ],
            stop_reason: StopReason::ToolUse,
            usage: None,
        })];
        let rope = assemble_rope(None, &messages, &[]);
        assert_eq!(rope.len(), 2);
        let content = &rope.spans()[1].content;
        assert!(content.contains("thinking"));
        assert!(content.contains("[tool_call:read("));
    }

    #[test]
    fn tool_result_with_error_prefixed() {
        let messages = vec![AgentMessage::ToolResult {
            call_id: "c-1".into(),
            content: "permission denied".into(),
            is_error: true,
        }];
        let rope = assemble_rope(None, &messages, &[]);
        assert_eq!(rope.spans()[1].content(), "error: permission denied");
        assert_eq!(rope.spans()[1].kind, SpanKind::ToolResult);
    }

    #[test]
    fn empty_inputs_produce_no_tools_clause_only() {
        let rope = assemble_rope(None, &[], &[]);
        assert_eq!(rope.len(), 1);
        assert_eq!(rope.spans()[0].id(), "no-tools-clause");
    }

    #[test]
    fn no_tools_injects_no_tools_clause() {
        let rope = assemble_rope(Some("you are mu"), &[], &[]);
        assert_eq!(rope.len(), 2);
        let spans = rope.spans();
        assert_eq!(spans[0].kind, SpanKind::System);
        assert_eq!(spans[1].kind, SpanKind::System);
        assert_eq!(spans[1].id(), "no-tools-clause");
        assert!(spans[1].content().contains("no tools available"));
        assert!(spans[1].cacheable);
        assert!(spans[1].retention.is_stable());
    }

    #[test]
    fn tools_present_suppresses_no_tools_clause() {
        let rope = assemble_rope(Some("you are mu"), &[], &[sample_tool()]);
        let has_clause = rope.spans().iter().any(|s| s.id() == "no-tools-clause");
        assert!(!has_clause);
    }

    #[test]
    fn extract_call_id_recovers_suffix_for_tool_result_span_ids() {
        // mu-yqeq.3 acceptance: Phase C wire adapters can recover the
        // tool-call binding from the synthesized span id without
        // re-walking Vec<AgentMessage>.
        assert_eq!(
            extract_call_id_from_span_id("msg-2-tool-result:toolu_abc123"),
            Some("toolu_abc123"),
        );
        // Embedded colons in the call_id survive (everything after the
        // first "tool-result:" is the call_id).
        assert_eq!(
            extract_call_id_from_span_id("msg-0-tool-result:call:with:colons"),
            Some("call:with:colons"),
        );
        // Non-ToolResult span ids return None.
        assert_eq!(extract_call_id_from_span_id("msg-0-user"), None);
        assert_eq!(extract_call_id_from_span_id("msg-1-assistant"), None);
        assert_eq!(extract_call_id_from_span_id("system-prompt"), None);
        assert_eq!(
            extract_call_id_from_span_id("tool-schema:read"),
            None,
            "tool-SCHEMA, not tool-RESULT — distinct id space",
        );
    }

    #[test]
    fn assistant_span_blocks_reconstruct_original_content_byte_for_byte() {
        // mu-yqeq.3 round-trip acceptance: assemble_rope populates the
        // Span.blocks field with the original AssistantMessage.content
        // verbatim. The flat content remains the flattened projection;
        // the blocks field preserves the structured shape for Phase C
        // wire adapters.
        let original_blocks = vec![
            ContentBlock::Text {
                text: "I'll read the file.".into(),
            },
            ContentBlock::ToolCall(ToolCall {
                id: "toolu_42".into(),
                name: "read".into(),
                arguments: crate::agent::ToolArgs::new(
                    serde_json::json!({"path": "/etc/hostname"}),
                )
                .unwrap(),
            }),
            ContentBlock::Thinking {
                text: "user wants the hostname".into(),
            },
        ];
        let messages = vec![AgentMessage::Assistant(AssistantMessage {
            content: original_blocks.clone(),
            stop_reason: StopReason::ToolUse,
            usage: None,
        })];
        let rope = assemble_rope(None, &messages, &[]);

        // The assistant span carries the original blocks intact.
        // (spans()[0] is the no-tools clause; assistant is spans()[1])
        let assistant_span = &rope.spans()[1];
        assert_eq!(assistant_span.kind(), &SpanKind::Assistant);
        let blocks = assistant_span.blocks().expect("assistant span has blocks");
        assert_eq!(blocks, original_blocks.as_slice());

        // Flat content remains the existing flattened projection — both
        // views coexist.
        assert!(assistant_span.content().contains("I'll read the file."));
        assert!(assistant_span.content().contains("[tool_call:read("));
    }

    #[test]
    fn rendered_provider_message_carries_blocks_through() {
        use crate::context::{FauxProviderRenderer, ProjectionTarget, ProviderRenderer};

        let original_blocks = vec![
            ContentBlock::Text { text: "ok".into() },
            ContentBlock::ToolCall(ToolCall {
                id: "toolu_1".into(),
                name: "bash".into(),
                arguments: crate::agent::ToolArgs::new(serde_json::json!({"command": "ls"}))
                    .unwrap(),
            }),
        ];
        let messages = vec![AgentMessage::Assistant(AssistantMessage {
            content: original_blocks.clone(),
            stop_reason: StopReason::ToolUse,
            usage: None,
        })];
        let rope = assemble_rope(None, &messages, &[]);
        let projection = FauxProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);

        // projection[0] is the no-tools clause (System); projection[1] is
        // the assistant message with blocks identical to the original.
        assert_eq!(projection.messages.len(), 2);
        let msg_blocks = projection.messages[1]
            .blocks()
            .expect("assistant ProviderMessage has blocks");
        assert_eq!(msg_blocks, original_blocks.as_slice());
    }

    #[test]
    fn assemble_rope_with_context_injects_memory_and_file_spans_in_stable_prefix() {
        // mu-phl v0 acceptance (bead mu-vm81): when project_context is
        // Some(...), recalled items land between System and ToolSchema
        // as MemoryInjection and FileLoad spans, all at Startup retention
        // and cacheable. When project_context is None, behavior is
        // identical to assemble_rope (covered by the existing tests).
        use super::super::recall::{ProjectContext, RecallSource, RecalledItem};
        use std::path::PathBuf;

        let project_context = ProjectContext {
            items: vec![
                RecalledItem {
                    source: RecallSource::Memory,
                    content: "## Active Memory Context\n…".into(),
                    stable_id: "abc123".into(),
                },
                RecalledItem {
                    source: RecallSource::ProjectFile {
                        path: PathBuf::from("/home/u/CLAUDE.md"),
                    },
                    content: "# CLAUDE.md\nsome content".into(),
                    stable_id: "def456".into(),
                },
            ],
        };

        let messages = vec![AgentMessage::User {
            content: "hi".into(),
        }];

        let rope = assemble_rope_with_context(
            Some("you are mu"),
            Some(&project_context),
            &messages,
            &[sample_tool()],
        );

        let kinds: Vec<SpanKind> = rope.iter().map(|s| s.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                SpanKind::System,
                SpanKind::MemoryInjection,
                SpanKind::FileLoad,
                SpanKind::ToolSchema,
                SpanKind::User,
            ],
            "[System] | [Memory] | [Files] | [ToolSchemas] | [Messages]",
        );

        // Recall spans extend the cacheable prefix: they're cacheable +
        // stable like the surrounding program-region content.
        let spans = rope.spans();
        for (idx, expected_kind) in [(1, SpanKind::MemoryInjection), (2, SpanKind::FileLoad)] {
            assert_eq!(spans[idx].kind, expected_kind);
            assert!(
                spans[idx].retention.is_stable(),
                "recall span {idx} must be at stable retention",
            );
            assert!(spans[idx].cacheable, "recall span {idx} must be cacheable",);
        }
    }

    #[test]
    fn assemble_rope_with_context_none_matches_assemble_rope() {
        // Equivalence: assemble_rope_with_context(sp, None, msgs, tools)
        // produces the same kinds + count as assemble_rope. The wrapper
        // is a no-op when project_context is None.
        let messages = vec![AgentMessage::User {
            content: "hi".into(),
        }];

        let a = assemble_rope(Some("sys"), &messages, &[sample_tool()]);
        let b = assemble_rope_with_context(Some("sys"), None, &messages, &[sample_tool()]);

        let kinds_a: Vec<SpanKind> = a.iter().map(|s| s.kind.clone()).collect();
        let kinds_b: Vec<SpanKind> = b.iter().map(|s| s.kind.clone()).collect();
        assert_eq!(kinds_a, kinds_b);
    }

    #[test]
    fn non_assistant_spans_have_no_blocks() {
        let messages = vec![
            AgentMessage::User {
                content: "hi".into(),
            },
            AgentMessage::ToolResult {
                call_id: "toolu_x".into(),
                content: "ok".into(),
                is_error: false,
            },
        ];
        let rope = assemble_rope(Some("sys"), &messages, &[sample_tool()]);

        // system, tool-schema, user, tool-result: all None.
        for span in rope.spans() {
            assert!(
                span.blocks().is_none(),
                "non-assistant span (kind={:?}, id={}) must have blocks=None",
                span.kind(),
                span.id(),
            );
        }
    }
}
