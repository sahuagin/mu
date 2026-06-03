//! Context-window attribution derived from session events JSONL.
//!
//! Walks a session's event log and produces a typed snapshot of where
//! tokens are going — message counts from ContextAssembly events,
//! token usage from Done events, and per-tool call attribution from
//! ToolCall/ToolResult pairs.

use std::collections::HashMap;

use crate::agent::Usage;
use crate::event_log::{EventPayload, SessionEvent};

/// Top-level attribution of a session's context window usage.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextAttribution {
    /// Provider's stated context window size, if known.
    pub window_max: Option<u64>,
    /// Cumulative input tokens across all model calls (sum of Done.usage.input_tokens).
    pub total_input_tokens: u64,
    /// Cumulative output tokens.
    pub total_output_tokens: u64,
    /// Cache read tokens (cumulative).
    pub cache_read_tokens: u64,
    /// Cache creation tokens (cumulative).
    pub cache_creation_tokens: u64,
    /// Per-model-call snapshots (ContextAssembly + paired Done).
    pub model_calls: Vec<ModelCallAttribution>,
    /// Per-tool-name aggregated call counts and result sizes.
    pub tool_attribution: Vec<ToolAttribution>,
    /// Number of user messages in the session.
    pub user_message_count: u32,
    /// Number of assistant messages in the session.
    pub assistant_message_count: u32,
    /// Number of tool result events in the session.
    pub tool_result_count: u32,
    /// Provider + model from SessionCreated, if present.
    pub provider_model: Option<(String, String)>,
}

/// Attribution for a single model call (one ContextAssembly→Done pair).
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCallAttribution {
    pub model_call_id: u32,
    pub message_count: u32,
    pub user_message_count: u32,
    pub assistant_message_count: u32,
    pub tool_result_count: u32,
    pub tool_count: u32,
    pub token_count_estimate: Option<u64>,
    pub provider_kind: String,
    pub model: String,
    pub renderer: Option<String>,
    pub cache_strategy: Option<String>,
    pub span_count: Option<u32>,
    pub cache_boundary_count: Option<u32>,
    /// Usage reported by the Done event paired with this assembly.
    pub usage: Option<Usage>,
}

/// Aggregated tool call statistics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolAttribution {
    pub tool_name: String,
    pub call_count: u32,
    /// Sum of result content lengths (bytes) across all calls.
    pub total_result_bytes: u64,
    /// Number of calls that returned is_error=true.
    pub error_count: u32,
}

/// Build a `ContextAttribution` from a slice of session events.
///
/// Events should be in log order (monotonic id). This is a pure
/// function — it takes an already-loaded event slice, not a file path.
/// For file-based loading, use `SessionEventLog::from_jsonl()` first
/// then pass `.snapshot()`.
pub fn attribute(events: &[SessionEvent]) -> ContextAttribution {
    let mut total_input: u64 = 0;
    let mut total_output: u64 = 0;
    let mut cache_read: u64 = 0;
    let mut cache_creation: u64 = 0;
    let mut user_msg_count: u32 = 0;
    let mut asst_msg_count: u32 = 0;
    let mut tool_result_count: u32 = 0;
    let mut provider_model: Option<(String, String)> = None;

    // Collect ContextAssembly events keyed by model_call_id.
    let mut assemblies: HashMap<u32, ModelCallAttribution> = HashMap::new();
    // Track ordering for stable output.
    let mut assembly_order: Vec<u32> = Vec::new();

    // Per-tool aggregation, keyed by tool name.
    let mut tool_stats: HashMap<String, ToolAttribution> = HashMap::new();
    // Track call_id→tool_name for pairing ToolResult with ToolCall.
    let mut pending_calls: HashMap<String, String> = HashMap::new();

    // Which model_call_id is "current" — the most recent ContextAssembly
    // not yet paired with a Done.
    let mut current_assembly_id: Option<u32> = None;

    for event in events {
        match &event.payload {
            EventPayload::SessionCreated {
                provider_kind,
                model,
                ..
            } if provider_model.is_none() => {
                provider_model = Some((provider_kind.clone(), model.clone()));
            }

            EventPayload::UserMessage { .. } => {
                user_msg_count += 1;
            }

            EventPayload::AssistantMessageEvent { .. } => {
                asst_msg_count += 1;
            }

            EventPayload::ContextAssembly {
                model_call_id,
                message_count,
                user_message_count: u_count,
                assistant_message_count: a_count,
                tool_result_count: tr_count,
                tool_count,
                token_count_estimate,
                provider_kind,
                model,
                renderer,
                cache_strategy,
                span_count,
                cache_boundary_count,
                ..
            } => {
                let attr = ModelCallAttribution {
                    model_call_id: *model_call_id,
                    message_count: *message_count,
                    user_message_count: *u_count,
                    assistant_message_count: *a_count,
                    tool_result_count: *tr_count,
                    tool_count: *tool_count,
                    token_count_estimate: *token_count_estimate,
                    provider_kind: provider_kind.clone(),
                    model: model.clone(),
                    renderer: renderer.clone(),
                    cache_strategy: cache_strategy.clone(),
                    span_count: *span_count,
                    cache_boundary_count: *cache_boundary_count,
                    usage: None,
                };
                assembly_order.push(*model_call_id);
                assemblies.insert(*model_call_id, attr);
                current_assembly_id = Some(*model_call_id);
            }

            EventPayload::Done { usage: Some(u), .. } => {
                total_input += u.input_tokens;
                total_output += u.output_tokens;
                cache_read += u.cache_read_input_tokens.unwrap_or(0);
                cache_creation += u.cache_creation_input_tokens.unwrap_or(0);

                // Pair with the most recent unpaired ContextAssembly.
                if let Some(ca_id) = current_assembly_id.take() {
                    if let Some(attr) = assemblies.get_mut(&ca_id) {
                        attr.usage = Some(*u);
                    }
                }
            }

            EventPayload::ToolCall { call_id, name, .. } => {
                pending_calls.insert(call_id.clone(), name.clone());
                let entry = tool_stats
                    .entry(name.clone())
                    .or_insert_with(|| ToolAttribution {
                        tool_name: name.clone(),
                        call_count: 0,
                        total_result_bytes: 0,
                        error_count: 0,
                    });
                entry.call_count += 1;
            }

            EventPayload::ToolResult {
                call_id,
                content,
                is_error,
            } => {
                tool_result_count += 1;
                let tool_name = pending_calls
                    .remove(call_id)
                    .unwrap_or_else(|| "unknown".to_string());
                let entry =
                    tool_stats
                        .entry(tool_name.clone())
                        .or_insert_with(|| ToolAttribution {
                            tool_name,
                            call_count: 0,
                            total_result_bytes: 0,
                            error_count: 0,
                        });
                entry.total_result_bytes += content.len() as u64;
                if *is_error {
                    entry.error_count += 1;
                }
            }

            _ => {}
        }
    }

    // Build model_calls in log order.
    let model_calls: Vec<ModelCallAttribution> = assembly_order
        .iter()
        .filter_map(|id| assemblies.remove(id))
        .collect();

    // Sort tool attribution by call count descending for "top consumers."
    let mut tool_attribution: Vec<ToolAttribution> = tool_stats.into_values().collect();
    tool_attribution.sort_by_key(|t| std::cmp::Reverse(t.call_count));

    ContextAttribution {
        window_max: None,
        total_input_tokens: total_input,
        total_output_tokens: total_output,
        cache_read_tokens: cache_read,
        cache_creation_tokens: cache_creation,
        model_calls,
        tool_attribution,
        user_message_count: user_msg_count,
        assistant_message_count: asst_msg_count,
        tool_result_count,
        provider_model,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AssistantMessage, ContentBlock, StopReason, Usage};
    use crate::event_log::{EventActor, EventPayload, SessionEvent, SessionEventLog};
    use serde_json::json;

    fn usage(input: u64, output: u64, cache_read: u64, cache_creation: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: if cache_read > 0 {
                Some(cache_read)
            } else {
                None
            },
            cache_creation_input_tokens: if cache_creation > 0 {
                Some(cache_creation)
            } else {
                None
            },
            reasoning_tokens: None,
        }
    }

    fn build_session_events() -> Vec<SessionEvent> {
        let log = SessionEventLog::new("test-session");

        // SessionCreated
        log.append(
            EventActor::System,
            EventPayload::SessionCreated {
                provider_kind: "anthropic_api".into(),
                model: "claude-opus-4-7".into(),
                parent_session_id: None,
                branched_at_parent_event_id: None,
            },
        );

        // User message
        log.append(
            EventActor::User,
            EventPayload::UserMessage {
                content: "Hello, explain this code".into(),
            },
        );

        // ContextAssembly for first model call
        log.append(
            EventActor::System,
            EventPayload::ContextAssembly {
                model_call_id: 1,
                message_count: 1,
                user_message_count: 1,
                assistant_message_count: 0,
                tool_result_count: 0,
                tool_count: 5,
                token_count_estimate: None,
                token_breakdown: Default::default(),
                provider_kind: "anthropic_api".into(),
                model: "claude-opus-4-7".into(),
                renderer: Some("anthropic".into()),
                cache_strategy: Some("sliding_window".into()),
                span_count: Some(12),
                cache_boundary_count: Some(2),
                first_span_ids: vec!["sys-0".into()],
            },
        );

        // Assistant responds with a tool call
        log.append(
            EventActor::Agent,
            EventPayload::AssistantMessageEvent {
                message: AssistantMessage {
                    content: vec![ContentBlock::Text {
                        text: "Let me read that file.".into(),
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: Some(usage(1500, 200, 800, 100)),
                },
            },
        );

        // ToolCall: read
        log.append(
            EventActor::Agent,
            EventPayload::ToolCall {
                call_id: "call-1".into(),
                name: "read".into(),
                arguments: json!({"path": "/src/main.rs"}),
            },
        );

        // ToolResult: read
        log.append(
            EventActor::Tool {
                name: "read".into(),
            },
            EventPayload::ToolResult {
                call_id: "call-1".into(),
                content: "fn main() { println!(\"hello\"); }".into(),
                is_error: false,
            },
        );

        // Done for first ask round-trip
        log.append(
            EventActor::Agent,
            EventPayload::Done {
                stop_reason: StopReason::ToolUse,
                turn_count: 1,
                usage: Some(usage(1500, 200, 800, 100)),
                elapsed_ms: Some(2500),
            },
        );

        // ContextAssembly for second model call
        log.append(
            EventActor::System,
            EventPayload::ContextAssembly {
                model_call_id: 2,
                message_count: 3,
                user_message_count: 1,
                assistant_message_count: 1,
                tool_result_count: 1,
                tool_count: 5,
                token_count_estimate: None,
                token_breakdown: Default::default(),
                provider_kind: "anthropic_api".into(),
                model: "claude-opus-4-7".into(),
                renderer: Some("anthropic".into()),
                cache_strategy: Some("sliding_window".into()),
                span_count: Some(15),
                cache_boundary_count: Some(2),
                first_span_ids: vec!["sys-0".into()],
            },
        );

        // Another tool call: grep
        log.append(
            EventActor::Agent,
            EventPayload::ToolCall {
                call_id: "call-2".into(),
                name: "grep".into(),
                arguments: json!({"pattern": "TODO"}),
            },
        );

        // ToolResult: grep
        log.append(
            EventActor::Tool {
                name: "grep".into(),
            },
            EventPayload::ToolResult {
                call_id: "call-2".into(),
                content: "src/lib.rs:42: // TODO: refactor".into(),
                is_error: false,
            },
        );

        // Second read call
        log.append(
            EventActor::Agent,
            EventPayload::ToolCall {
                call_id: "call-3".into(),
                name: "read".into(),
                arguments: json!({"path": "/src/lib.rs"}),
            },
        );

        // ToolResult: read (error this time)
        log.append(
            EventActor::Tool {
                name: "read".into(),
            },
            EventPayload::ToolResult {
                call_id: "call-3".into(),
                content: "file not found".into(),
                is_error: true,
            },
        );

        // Done for second round-trip
        log.append(
            EventActor::Agent,
            EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 2,
                usage: Some(usage(3200, 450, 1800, 200)),
                elapsed_ms: Some(4100),
            },
        );

        log.snapshot()
    }

    #[test]
    fn attribute_basic_session() {
        let events = build_session_events();
        let attr = attribute(&events);

        // Provider/model from SessionCreated
        assert_eq!(
            attr.provider_model,
            Some(("anthropic_api".into(), "claude-opus-4-7".into()))
        );

        // Cumulative usage from 2 Done events
        assert_eq!(attr.total_input_tokens, 1500 + 3200);
        assert_eq!(attr.total_output_tokens, 200 + 450);
        assert_eq!(attr.cache_read_tokens, 800 + 1800);
        assert_eq!(attr.cache_creation_tokens, 100 + 200);

        // Message counts from direct event counting
        assert_eq!(attr.user_message_count, 1);
        assert_eq!(attr.assistant_message_count, 1);
        assert_eq!(attr.tool_result_count, 3);

        // 2 model calls
        assert_eq!(attr.model_calls.len(), 2);
        assert_eq!(attr.model_calls[0].model_call_id, 1);
        assert_eq!(attr.model_calls[0].message_count, 1);
        assert_eq!(attr.model_calls[0].usage.unwrap().input_tokens, 1500);
        assert_eq!(attr.model_calls[1].model_call_id, 2);
        assert_eq!(attr.model_calls[1].message_count, 3);
        assert_eq!(attr.model_calls[1].usage.unwrap().input_tokens, 3200);

        // Tool attribution: 2 reads, 1 grep
        assert_eq!(attr.tool_attribution.len(), 2);
        let read_attr = attr
            .tool_attribution
            .iter()
            .find(|t| t.tool_name == "read")
            .unwrap();
        assert_eq!(read_attr.call_count, 2);
        assert_eq!(read_attr.error_count, 1);
        let grep_attr = attr
            .tool_attribution
            .iter()
            .find(|t| t.tool_name == "grep")
            .unwrap();
        assert_eq!(grep_attr.call_count, 1);
        assert_eq!(grep_attr.error_count, 0);
    }

    #[test]
    fn attribute_empty_session() {
        let attr = attribute(&[]);
        assert_eq!(attr.total_input_tokens, 0);
        assert_eq!(attr.total_output_tokens, 0);
        assert_eq!(attr.cache_read_tokens, 0);
        assert_eq!(attr.cache_creation_tokens, 0);
        assert_eq!(attr.model_calls.len(), 0);
        assert_eq!(attr.tool_attribution.len(), 0);
        assert_eq!(attr.user_message_count, 0);
        assert_eq!(attr.provider_model, None);
    }

    #[test]
    fn attribute_done_without_usage() {
        let log = SessionEventLog::new("no-usage");
        log.append(
            EventActor::System,
            EventPayload::ContextAssembly {
                model_call_id: 1,
                message_count: 1,
                user_message_count: 1,
                assistant_message_count: 0,
                tool_result_count: 0,
                tool_count: 0,
                token_count_estimate: None,
                token_breakdown: Default::default(),
                provider_kind: "faux".into(),
                model: "test".into(),
                renderer: None,
                cache_strategy: None,
                span_count: None,
                cache_boundary_count: None,
                first_span_ids: Vec::new(),
            },
        );
        log.append(
            EventActor::Agent,
            EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 1,
                usage: None,
                elapsed_ms: Some(100),
            },
        );
        let events = log.snapshot();
        let attr = attribute(&events);
        assert_eq!(attr.total_input_tokens, 0);
        assert_eq!(attr.model_calls.len(), 1);
        assert!(attr.model_calls[0].usage.is_none());
    }

    #[test]
    fn attribute_tool_result_without_matching_call() {
        let log = SessionEventLog::new("orphan-result");
        log.append(
            EventActor::Tool {
                name: "read".into(),
            },
            EventPayload::ToolResult {
                call_id: "orphan-1".into(),
                content: "some content".into(),
                is_error: false,
            },
        );
        let events = log.snapshot();
        let attr = attribute(&events);
        assert_eq!(attr.tool_result_count, 1);
        let unknown = attr
            .tool_attribution
            .iter()
            .find(|t| t.tool_name == "unknown")
            .unwrap();
        assert_eq!(unknown.call_count, 0);
        assert_eq!(unknown.total_result_bytes, 12);
    }

    #[test]
    fn attribute_from_jsonl_round_trip() {
        let log = SessionEventLog::new("jsonl-rt");
        log.append(
            EventActor::System,
            EventPayload::SessionCreated {
                provider_kind: "anthropic_api".into(),
                model: "claude-opus-4-7".into(),
                parent_session_id: None,
                branched_at_parent_event_id: None,
            },
        );
        log.append(
            EventActor::User,
            EventPayload::UserMessage {
                content: "test".into(),
            },
        );
        log.append(
            EventActor::System,
            EventPayload::ContextAssembly {
                model_call_id: 1,
                message_count: 1,
                user_message_count: 1,
                assistant_message_count: 0,
                tool_result_count: 0,
                tool_count: 3,
                token_count_estimate: Some(500),
                token_breakdown: Default::default(),
                provider_kind: "anthropic_api".into(),
                model: "claude-opus-4-7".into(),
                renderer: None,
                cache_strategy: None,
                span_count: None,
                cache_boundary_count: None,
                first_span_ids: Vec::new(),
            },
        );
        log.append(
            EventActor::Agent,
            EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 1,
                usage: Some(usage(500, 80, 300, 50)),
                elapsed_ms: Some(1200),
            },
        );

        // Write to temp JSONL
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        log.attach_disk_writer(&path).unwrap();
        // Re-append to force disk write (existing events were pre-attach)
        // Instead, write the snapshot manually
        let events = log.snapshot();
        let mut file = std::fs::File::create(&path).unwrap();
        for ev in &events {
            let line = serde_json::to_string(ev).unwrap();
            std::io::Write::write_all(&mut file, line.as_bytes()).unwrap();
            std::io::Write::write_all(&mut file, b"\n").unwrap();
        }
        drop(file);

        // Read back and attribute
        let (reloaded, malformed) = SessionEventLog::from_jsonl(&path).unwrap();
        assert_eq!(malformed, 0);
        let reloaded_events = reloaded.snapshot();
        let attr = attribute(&reloaded_events);
        assert_eq!(attr.total_input_tokens, 500);
        assert_eq!(attr.cache_read_tokens, 300);
        assert_eq!(attr.model_calls.len(), 1);
        assert_eq!(attr.model_calls[0].token_count_estimate, Some(500));
    }

    #[test]
    fn attribute_multiple_assemblies_ordering() {
        let log = SessionEventLog::new("ordering");
        for id in 1..=5u32 {
            log.append(
                EventActor::System,
                EventPayload::ContextAssembly {
                    model_call_id: id,
                    message_count: id,
                    user_message_count: 1,
                    assistant_message_count: 0,
                    tool_result_count: 0,
                    tool_count: 0,
                    token_count_estimate: None,
                    token_breakdown: Default::default(),
                    provider_kind: "test".into(),
                    model: "test".into(),
                    renderer: None,
                    cache_strategy: None,
                    span_count: None,
                    cache_boundary_count: None,
                    first_span_ids: Vec::new(),
                },
            );
            log.append(
                EventActor::Agent,
                EventPayload::Done {
                    stop_reason: StopReason::EndTurn,
                    turn_count: 1,
                    usage: Some(usage(100 * id as u64, 10 * id as u64, 0, 0)),
                    elapsed_ms: None,
                },
            );
        }
        let events = log.snapshot();
        let attr = attribute(&events);
        assert_eq!(attr.model_calls.len(), 5);
        for (i, mc) in attr.model_calls.iter().enumerate() {
            assert_eq!(mc.model_call_id, (i + 1) as u32);
            assert_eq!(mc.usage.unwrap().input_tokens, 100 * (i + 1) as u64);
        }
    }

    #[test]
    fn tool_attribution_sorted_by_call_count() {
        let log = SessionEventLog::new("tool-sort");
        // 3 reads, 1 grep, 2 edits
        for (name, count) in [("read", 3), ("grep", 1), ("edit", 2)] {
            for i in 0..count {
                let call_id = format!("{name}-{i}");
                log.append(
                    EventActor::Agent,
                    EventPayload::ToolCall {
                        call_id: call_id.clone(),
                        name: name.into(),
                        arguments: json!({}),
                    },
                );
                log.append(
                    EventActor::Tool { name: name.into() },
                    EventPayload::ToolResult {
                        call_id,
                        content: "ok".into(),
                        is_error: false,
                    },
                );
            }
        }
        let events = log.snapshot();
        let attr = attribute(&events);
        assert_eq!(attr.tool_attribution[0].tool_name, "read");
        assert_eq!(attr.tool_attribution[0].call_count, 3);
        assert_eq!(attr.tool_attribution[1].tool_name, "edit");
        assert_eq!(attr.tool_attribution[1].call_count, 2);
        assert_eq!(attr.tool_attribution[2].tool_name, "grep");
        assert_eq!(attr.tool_attribution[2].call_count, 1);
    }
}
