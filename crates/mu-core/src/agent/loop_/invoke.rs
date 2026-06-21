//! Provider invocation — stream handling + status emission.

use std::time::Instant;

use futures::StreamExt;
use tokio::sync::mpsc;

use crate::context::ProviderMessages;

use super::super::provider::{MessageInput, Provider};
use super::super::tool::ToolSpec;
use super::super::types::{AssistantMessage, ContentBlock};

use super::{AgentEvent, AgentInput, Outcome};

fn now_unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) async fn handle_invoke_llm(
    provider: &dyn Provider,
    system_prompt: Option<&str>,
    // mu-vcbm: the session's current `/effort` selection, forwarded to
    // `Provider::stream` for this call. `None` ⇒ the provider's
    // construction-time default.
    effort: Option<&str>,
    projection: &ProviderMessages,
    tool_specs: &[ToolSpec],
    input_rx: &mut mpsc::Receiver<AgentInput>,
    events: &mpsc::Sender<AgentEvent>,
) -> Result<(AssistantMessage, Vec<AgentInput>), Outcome> {
    use crate::protocol::ProviderStatusKind;

    const PROVIDER_STATUS_TICK_MS: u64 = 1000;
    let call_started_at = Instant::now();
    let call_started_unix_ms = now_unix_ms();
    let _ = events
        .send(AgentEvent::ProviderStatus {
            state: ProviderStatusKind::AwaitingFirstToken,
            started_at_unix_ms: call_started_unix_ms,
            elapsed_ms: 0,
            bytes_received: None,
            tool_call_id: None,
        })
        .await;

    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    // mu-yqeq.8: the cache-annotated `ProviderMessages` projection is
    // the canonical agent-loop → provider input. Per-provider
    // adapters consume it via `MessageInput::Projected` and produce
    // byte-equivalent wire JSON to the pre-cutover Legacy path (plus
    // cache_control driven by the projection's cache_marker flags).
    let mut stream = provider
        .stream(
            system_prompt,
            effort,
            MessageInput::Projected(projection),
            tool_specs,
            cancel_rx,
        )
        .await
        .map_err(|e| Outcome::Error(e.to_string()))?;

    let mut buffered: Vec<AgentInput> = Vec::new();
    let mut bytes_received: u64 = 0;
    let mut seen_first_token = false;
    let mut current_state = ProviderStatusKind::AwaitingFirstToken;
    let mut state_started_at = call_started_at;
    let mut state_started_unix_ms = call_started_unix_ms;
    let mut tick_interval =
        tokio::time::interval(std::time::Duration::from_millis(PROVIDER_STATUS_TICK_MS));
    tick_interval.tick().await;
    let mut input_drained = false;

    loop {
        tokio::select! {
            event = stream.next() => match event {
                Some(super::super::provider::ProviderEvent::TextDelta(d)) => {
                    bytes_received = bytes_received.saturating_add(d.len() as u64);
                    if !seen_first_token {
                        seen_first_token = true;
                        current_state = ProviderStatusKind::Streaming;
                        state_started_at = Instant::now();
                        state_started_unix_ms = now_unix_ms();
                        let _ = events
                            .send(AgentEvent::ProviderStatus {
                                state: current_state,
                                started_at_unix_ms: state_started_unix_ms,
                                elapsed_ms: call_started_at.elapsed().as_millis() as u64,
                                bytes_received: Some(bytes_received),
                                tool_call_id: None,
                            })
                            .await;
                    }
                    let _ = events.send(AgentEvent::TextDelta { delta: d }).await;
                }
                Some(super::super::provider::ProviderEvent::Done(msg)) => {
                    // mu-wk2: extract text from the message's content blocks and
                    // emit AssistantTextFinalized before signalling done, so
                    // clients can swap from the streaming accumulator to the
                    // finalized text atomically. mu-upk2: do the same for the
                    // reasoning channel — collect Thinking blocks and emit
                    // AssistantThinkingFinalized when the turn produced any.
                    let mut text = String::new();
                    let mut thinking = String::new();
                    for block in &msg.content {
                        match block {
                            ContentBlock::Text { text: block_text } => text.push_str(block_text),
                            ContentBlock::Thinking { text: block_text } => {
                                thinking.push_str(block_text)
                            }
                            ContentBlock::ToolCall(_) => {}
                        }
                    }
                    let _ = events
                        .send(AgentEvent::AssistantTextFinalized { text })
                        .await;
                    if !thinking.is_empty() {
                        let _ = events
                            .send(AgentEvent::AssistantThinkingFinalized { text: thinking })
                            .await;
                    }
                    let _ = cancel_tx.send(());
                    return Ok((msg, buffered));
                }
                Some(super::super::provider::ProviderEvent::Error(e)) => {
                    let _ = cancel_tx.send(());
                    return Err(Outcome::Error(e));
                }
                Some(super::super::provider::ProviderEvent::ThinkingDelta(d)) => {
                    // Reasoning streams just like text: count its bytes and,
                    // since reasoning models emit thinking BEFORE any answer
                    // text, treat it as the first token so the session leaves
                    // AwaitingFirstToken instead of looking stalled.
                    bytes_received = bytes_received.saturating_add(d.len() as u64);
                    if !seen_first_token {
                        seen_first_token = true;
                        current_state = ProviderStatusKind::Streaming;
                        state_started_at = Instant::now();
                        state_started_unix_ms = now_unix_ms();
                        let _ = events
                            .send(AgentEvent::ProviderStatus {
                                state: current_state,
                                started_at_unix_ms: state_started_unix_ms,
                                elapsed_ms: call_started_at.elapsed().as_millis() as u64,
                                bytes_received: Some(bytes_received),
                                tool_call_id: None,
                            })
                            .await;
                    }
                    let _ = events.send(AgentEvent::ThinkingDelta { delta: d }).await;
                }
                Some(super::super::provider::ProviderEvent::ToolCallDelta {
                    id,
                    name_delta,
                    arguments_delta,
                }) => {
                    // Partial tool-call args also count as streaming output (a
                    // tool-only turn may produce no text at all).
                    if let Some(args) = arguments_delta.as_deref() {
                        bytes_received = bytes_received.saturating_add(args.len() as u64);
                    }
                    if !seen_first_token {
                        seen_first_token = true;
                        current_state = ProviderStatusKind::Streaming;
                        state_started_at = Instant::now();
                        state_started_unix_ms = now_unix_ms();
                        let _ = events
                            .send(AgentEvent::ProviderStatus {
                                state: current_state,
                                started_at_unix_ms: state_started_unix_ms,
                                elapsed_ms: call_started_at.elapsed().as_millis() as u64,
                                bytes_received: Some(bytes_received),
                                tool_call_id: Some(id.clone()),
                            })
                            .await;
                    }
                    let _ = events
                        .send(AgentEvent::ToolCallDelta {
                            tool_call_id: id,
                            name_delta,
                            arguments_delta,
                        })
                        .await;
                }
                None => {
                    let _ = cancel_tx.send(());
                    return Err(Outcome::Error(
                        "provider stream ended without Done".into(),
                    ));
                }
            },
            input_opt = async {
                if input_drained {
                    std::future::pending::<Option<AgentInput>>().await
                } else {
                    input_rx.recv().await
                }
            } => match input_opt {
                Some(AgentInput::Cancel) => {
                    let _ = cancel_tx.send(());
                    return Err(Outcome::Cancelled);
                }
                Some(AgentInput::CancelOutstanding { reason }) => {
                    let _ = cancel_tx.send(());
                    return Err(Outcome::OutstandingCancelled { reason });
                }
                Some(input @ AgentInput::UserMessage(..))
                | Some(input @ AgentInput::StartAutonomous { .. })
                | Some(input @ AgentInput::ScheduleWakeup { .. })
                | Some(input @ AgentInput::SwitchProvider { .. })
                | Some(input @ AgentInput::WatchCompleted { .. })
                | Some(input @ AgentInput::MailboxMessage { .. }) => {
                    buffered.push(input);
                }
                None => {
                    input_drained = true;
                }
            },
            _ = tick_interval.tick() => {
                if !matches!(current_state, ProviderStatusKind::Streaming) {
                    let elapsed_ms = state_started_at.elapsed().as_millis() as u64;
                    let _ = events
                        .send(AgentEvent::ProviderStatus {
                            state: current_state,
                            started_at_unix_ms: state_started_unix_ms,
                            elapsed_ms,
                            bytes_received: if bytes_received > 0 {
                                Some(bytes_received)
                            } else {
                                None
                            },
                            tool_call_id: None,
                        })
                        .await;
                }
            },
        }
    }
}
