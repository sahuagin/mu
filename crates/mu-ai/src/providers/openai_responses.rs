//! Mu-facing adapter for OpenAI Responses protocol types.
//!
//! `mu-openai` is intentionally standalone and knows nothing about mu. This
//! module is the interpretive layer: mu request/body shapes in, OpenAI typed
//! protocol out; OpenAI typed stream events in, mu `ProviderEvent`s out.

use std::collections::HashMap;

use futures::stream::{BoxStream, StreamExt};
use mu_core::agent::{
    AssistantMessage, ContentBlock, ProviderEvent, StopReason, ToolArgs, ToolCall, Usage,
};
use serde_json::Value;
use tokio::sync::oneshot;

const DEFAULT_INSTRUCTIONS: &str = "You are mu, a coding agent. Respond concisely. \
     When tools are provided, prefer to use them rather than asking \
     the user for information you could obtain yourself.";

/// Soft cap for Codex's `instructions` field. Public OpenAI handles larger
/// instructions, but this adapter is shared by the Codex subscription path,
/// where oversized instructions have been observed to produce empty streams.
pub(crate) const CODEX_INSTRUCTIONS_SOFT_CAP: usize = 8 * 1024;

pub(crate) fn build_request_from_legacy(
    model: &str,
    thinking: &str,
    instructions: &str,
    messages: &[mu_core::agent::AgentMessage],
    tools: &[mu_core::agent::ToolSpec],
    cap_instructions: bool,
) -> mu_openai::CreateResponseRequest {
    let (instructions_field, overflow) = split_instructions(instructions, cap_instructions);
    let mut input = Vec::new();
    if let Some(o) = overflow {
        input.push(instructions_overflow_message(o));
    }
    for m in messages {
        input.extend(input_items_from_agent_message(m));
    }
    finish_request(model, thinking, instructions_field, input, tools)
}

pub(crate) fn build_request_from_projection(
    model: &str,
    thinking: &str,
    default_instructions: &str,
    pmsgs: &mu_core::context::ProviderMessages,
    tools: &[mu_core::agent::ToolSpec],
    cap_instructions: bool,
) -> mu_openai::CreateResponseRequest {
    let (mut input, hoisted_system) = input_items_from_projection(pmsgs);
    let instructions = hoisted_system
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(default_instructions);
    let (instructions_field, overflow) = split_instructions(instructions, cap_instructions);
    if let Some(o) = overflow {
        input.insert(0, instructions_overflow_message(o));
    }
    finish_request(model, thinking, instructions_field, input, tools)
}

fn finish_request(
    model: &str,
    thinking: &str,
    instructions: &str,
    input: Vec<mu_openai::InputItem>,
    tools: &[mu_core::agent::ToolSpec],
) -> mu_openai::CreateResponseRequest {
    let mut req = mu_openai::CreateResponseRequest {
        model: model.to_string(),
        instructions: Some(instructions.to_string()),
        input,
        stream: Some(true),
        store: Some(false),
        reasoning: Some(mu_openai::Reasoning {
            effort: Some(thinking.to_string()),
            summary: Some("auto".into()),
        }),
        tools: tools.iter().map(openai_tool_from_mu).collect(),
        tool_choice: None,
        parallel_tool_calls: None,
        max_output_tokens: None,
    };
    if !req.tools.is_empty() {
        req.tool_choice = Some(mu_openai::ToolChoice::Auto);
        req.parallel_tool_calls = Some(false);
    }
    req
}

fn openai_tool_from_mu(spec: &mu_core::agent::ToolSpec) -> mu_openai::Tool {
    mu_openai::Tool::Function(mu_openai::FunctionTool {
        name: spec.name.to_string(),
        description: spec.description.to_string(),
        parameters: mu_openai::JsonValue::new(spec.input_schema.clone())
            .expect("ToolSpec schema is valid JSON"),
    })
}

fn input_items_from_agent_message(m: &mu_core::agent::AgentMessage) -> Vec<mu_openai::InputItem> {
    use mu_core::agent::AgentMessage;
    match m {
        AgentMessage::User { content } => {
            vec![mu_openai::InputItem::user_text(content.to_string())]
        }
        AgentMessage::Assistant(a) => input_items_from_blocks(&a.content),
        AgentMessage::ToolResult {
            call_id,
            content,
            is_error,
        } => vec![mu_openai::InputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: if *is_error {
                format!("[error] {content}")
            } else {
                content.to_string()
            },
        }],
    }
}

fn input_items_from_blocks(blocks: &[mu_core::agent::ContentBlock]) -> Vec<mu_openai::InputItem> {
    use mu_core::agent::ContentBlock;
    let mut out = Vec::new();
    let mut text = String::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text: t } => text.push_str(t),
            ContentBlock::ToolCall(tc) => out.push(mu_openai::InputItem::FunctionCall {
                call_id: tc.id.clone(),
                name: tc.name.clone(),
                arguments: serde_json::to_string(&tc.arguments).unwrap_or_else(|_| "{}".into()),
            }),
            ContentBlock::Thinking { .. } => {}
        }
    }
    if !text.is_empty() {
        out.insert(0, mu_openai::InputItem::assistant_text(text));
    }
    out
}

fn input_items_from_projection(
    pmsgs: &mu_core::context::ProviderMessages,
) -> (Vec<mu_openai::InputItem>, Option<String>) {
    use mu_core::context::ProviderRole;
    let mut out = Vec::new();
    let mut system = None::<String>;
    for msg in &pmsgs.messages {
        match msg.role() {
            ProviderRole::System => {
                let is_tool_schema = msg
                    .source_span_ids()
                    .first()
                    .map(|sid| sid.as_ref().starts_with("tool-schema:"))
                    .unwrap_or(false);
                if !is_tool_schema && !msg.content().is_empty() {
                    match system.as_mut() {
                        Some(s) => {
                            s.push_str("\n\n");
                            s.push_str(msg.content());
                        }
                        None => system = Some(msg.content().to_string()),
                    }
                }
            }
            ProviderRole::User => {
                out.push(mu_openai::InputItem::user_text(msg.content().to_string()))
            }
            ProviderRole::Assistant => {
                if let Some(blocks) = msg.blocks() {
                    out.extend(input_items_from_blocks(blocks));
                }
            }
            ProviderRole::ToolResult => {
                let call_id = msg
                    .source_span_ids()
                    .first()
                    .and_then(|sid| mu_core::context::extract_call_id_from_span_id(sid.as_ref()))
                    .unwrap_or("");
                let output = match msg.content().strip_prefix("error: ") {
                    Some(stripped) => format!("[error] {stripped}"),
                    None => msg.content().to_string(),
                };
                out.push(mu_openai::InputItem::FunctionCallOutput {
                    call_id: call_id.into(),
                    output,
                });
            }
        }
    }
    (out, system)
}

fn split_instructions(instructions: &str, cap: bool) -> (&str, Option<&str>) {
    if cap && instructions.len() > CODEX_INSTRUCTIONS_SOFT_CAP {
        (DEFAULT_INSTRUCTIONS, Some(instructions))
    } else {
        (instructions, None)
    }
}

fn instructions_overflow_message(content: &str) -> mu_openai::InputItem {
    mu_openai::InputItem::user_text(format!(
        "[System context — too large for the instructions field. Treat this as your standing instructions and project context, not as a question to respond to directly.]\n\n{content}"
    ))
}

#[derive(Default)]
struct ToolCallBuilder {
    call_id: String,
    name: String,
    args_json: String,
}

#[derive(Default)]
struct Accum {
    text: String,
    tools: HashMap<u32, ToolCallBuilder>,
    order: Vec<u32>,
    usage: Option<Usage>,
    status: Option<String>,
    incomplete_reason: Option<String>,
}

pub(crate) fn events_from_openai_stream(
    stream: BoxStream<'static, Result<mu_openai::ResponseStreamEvent, String>>,
    cancel_rx: oneshot::Receiver<()>,
) -> BoxStream<'static, ProviderEvent> {
    let state = (stream, Accum::default(), Some(cancel_rx), false);
    Box::pin(futures::stream::unfold(state, |mut st| async move {
        let (stream, acc, cancel, done) = &mut st;
        if *done {
            return None;
        }
        loop {
            if let Some(rx) = cancel.as_mut() {
                match rx.try_recv() {
                    Ok(_) => {
                        *done = true;
                        *cancel = None;
                        return Some((
                            ProviderEvent::Done(done_message(acc, StopReason::Aborted)),
                            st,
                        ));
                    }
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
                    Err(tokio::sync::oneshot::error::TryRecvError::Closed) => *cancel = None,
                }
            }
            let ev = match stream.next().await {
                Some(Ok(ev)) => ev,
                Some(Err(e)) => {
                    *done = true;
                    return Some((ProviderEvent::Error(e), st));
                }
                None => {
                    *done = true;
                    return Some((ProviderEvent::Done(done_message(acc, map_stop(acc))), st));
                }
            };
            match ev {
                mu_openai::ResponseStreamEvent::OutputTextDelta { delta, .. } => {
                    if !delta.is_empty() {
                        acc.text.push_str(&delta);
                        return Some((ProviderEvent::TextDelta(delta), st));
                    }
                }
                mu_openai::ResponseStreamEvent::OutputItemAdded {
                    output_index, item, ..
                } => handle_output_item(acc, output_index, item, false),
                mu_openai::ResponseStreamEvent::OutputItemDone {
                    output_index, item, ..
                } => handle_output_item(acc, output_index, item, true),
                mu_openai::ResponseStreamEvent::FunctionCallArgumentsDelta {
                    output_index,
                    delta,
                    ..
                }
                | mu_openai::ResponseStreamEvent::FunctionCallArgumentsDeltaCompat {
                    output_index,
                    delta,
                    ..
                } => {
                    let e = tool_entry(acc, output_index);
                    e.args_json.push_str(&delta);
                    return Some((
                        ProviderEvent::ToolCallDelta {
                            id: e.call_id.clone(),
                            name_delta: None,
                            arguments_delta: Some(delta),
                        },
                        st,
                    ));
                }
                mu_openai::ResponseStreamEvent::FunctionCallArgumentsDone {
                    output_index,
                    name,
                    arguments,
                    ..
                } => {
                    let e = tool_entry(acc, output_index);
                    if let Some(n) = name {
                        e.name = n;
                    }
                    e.args_json = arguments;
                }
                mu_openai::ResponseStreamEvent::ReasoningSummaryTextDelta { delta, .. }
                | mu_openai::ResponseStreamEvent::ReasoningTextDelta { delta, .. } => {
                    if !delta.is_empty() {
                        return Some((ProviderEvent::ThinkingDelta(delta), st));
                    }
                }
                mu_openai::ResponseStreamEvent::Completed { response, .. } => {
                    acc.status = Some("completed".into());
                    acc.usage = response.usage.map(to_mu_usage);
                    *done = true;
                    return Some((ProviderEvent::Done(done_message(acc, map_stop(acc))), st));
                }
                mu_openai::ResponseStreamEvent::Incomplete { response, .. } => {
                    acc.status = Some("incomplete".into());
                    acc.incomplete_reason = response.incomplete_details.and_then(|d| d.reason);
                    acc.usage = response.usage.map(to_mu_usage);
                    *done = true;
                    return Some((ProviderEvent::Done(done_message(acc, map_stop(acc))), st));
                }
                mu_openai::ResponseStreamEvent::Failed { response, .. } => {
                    *done = true;
                    let msg = response
                        .error
                        .and_then(|e| e.message)
                        .unwrap_or_else(|| "openai response failed".into());
                    return Some((ProviderEvent::Error(msg), st));
                }
                mu_openai::ResponseStreamEvent::Error { message, code, .. } => {
                    *done = true;
                    return Some((
                        ProviderEvent::Error(
                            message
                                .or(code)
                                .unwrap_or_else(|| "openai stream error".into()),
                        ),
                        st,
                    ));
                }
                _ => {}
            }
        }
    }))
}

fn tool_entry(acc: &mut Accum, idx: u32) -> &mut ToolCallBuilder {
    acc.tools.entry(idx).or_insert_with(|| {
        acc.order.push(idx);
        ToolCallBuilder::default()
    })
}

fn handle_output_item(acc: &mut Accum, idx: u32, item: mu_openai::OutputItem, final_item: bool) {
    if let mu_openai::OutputItem::FunctionCall {
        call_id,
        name,
        arguments,
        ..
    } = item
    {
        let e = tool_entry(acc, idx);
        if let Some(v) = call_id {
            e.call_id = v;
        }
        if let Some(v) = name {
            e.name = v;
        }
        if final_item {
            if let Some(v) = arguments {
                if !v.is_empty() {
                    e.args_json = v;
                }
            }
        } else if let Some(v) = arguments {
            if !v.is_empty() {
                e.args_json.push_str(&v);
            }
        }
    }
}

fn done_message(acc: &Accum, stop_reason: StopReason) -> AssistantMessage {
    let mut content = Vec::new();
    if !acc.text.is_empty() {
        content.push(ContentBlock::Text {
            text: acc.text.as_str().into(),
        });
    }
    for idx in &acc.order {
        if let Some(t) = acc.tools.get(idx) {
            content.push(ContentBlock::ToolCall(ToolCall {
                id: t.call_id.clone(),
                name: t.name.clone(),
                arguments: parse_tool_args(&t.args_json),
            }));
        }
    }
    AssistantMessage {
        content,
        stop_reason,
        usage: acc.usage,
    }
}

fn map_stop(acc: &Accum) -> StopReason {
    if acc.incomplete_reason.as_deref() == Some("max_output_tokens") {
        StopReason::MaxTokens
    } else if !acc.tools.is_empty() {
        StopReason::ToolUse
    } else {
        match acc.status.as_deref() {
            Some("incomplete") => StopReason::MaxTokens,
            Some("failed") => StopReason::Error,
            _ => StopReason::EndTurn,
        }
    }
}

fn parse_tool_args(s: &str) -> ToolArgs {
    let v = if s.is_empty() {
        Value::Object(Default::default())
    } else {
        serde_json::from_str::<Value>(s)
            .ok()
            .filter(|v| v.is_object())
            .unwrap_or_else(|| Value::Object(Default::default()))
    };
    ToolArgs::new(v).unwrap_or_else(|_| ToolArgs::new(Value::Object(Default::default())).unwrap())
}

fn to_mu_usage(u: mu_openai::Usage) -> Usage {
    Usage {
        input_tokens: u.input_tokens.unwrap_or(0),
        output_tokens: u.output_tokens.unwrap_or(0),
        cache_read_input_tokens: u.input_tokens_details.and_then(|d| d.cached_tokens),
        cache_creation_input_tokens: None,
        cache_creation_5m_input_tokens: None,
        cache_creation_1h_input_tokens: None,
        reasoning_tokens: u.output_tokens_details.and_then(|d| d.reasoning_tokens),
    }
}
