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

pub(crate) fn request_from_value(
    v: Value,
) -> Result<mu_openai::CreateResponseRequest, serde_json::Error> {
    serde_json::from_value(v)
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
