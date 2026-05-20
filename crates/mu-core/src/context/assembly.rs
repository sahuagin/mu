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
    let mut spans: Vec<Span> = Vec::with_capacity(
        usize::from(system_prompt.is_some()) + tool_specs.len() + messages.len(),
    );

    if let Some(prompt) = system_prompt {
        spans.push(Span::new(
            "system-prompt",
            SpanKind::System,
            prompt,
            RetentionClass::Startup,
        ));
    }

    for spec in tool_specs {
        spans.push(Span::new(
            format!("tool-schema:{}", spec.name),
            SpanKind::ToolSchema,
            // Schema content: the human-visible description plus the
            // structured input schema. Renderers don't currently read
            // span.content for tool schemas (the wire request gets the
            // spec via Provider::stream's tools arg), but we record a
            // deterministic projection so /context why and rope
            // diffing have something to compare.
            format!(
                "{}\n{}",
                spec.description,
                serde_json::to_string(&spec.input_schema).unwrap_or_default(),
            ),
            RetentionClass::Hot,
        ));
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
        ),
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
            policy: Default::default(),
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
                    arguments: serde_json::json!({"path": "/x"}),
                }),
            ],
            stop_reason: StopReason::ToolUse,
            usage: None,
        })];
        let rope = assemble_rope(None, &messages, &[]);
        assert_eq!(rope.len(), 1);
        let content = &rope.spans()[0].content;
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
        assert_eq!(rope.spans()[0].content(), "error: permission denied");
        assert_eq!(rope.spans()[0].kind, SpanKind::ToolResult);
    }

    #[test]
    fn empty_inputs_produce_empty_rope() {
        let rope = assemble_rope(None, &[], &[]);
        assert!(rope.is_empty());
    }
}
