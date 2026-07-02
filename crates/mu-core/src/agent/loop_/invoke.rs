//! Provider invocation — stream handling + status emission.

use std::time::{Duration, Instant};

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

const PROVIDER_START_MAX_ATTEMPTS: u32 = 3;
const PROVIDER_START_BACKOFF_BASE_MS: u64 = 250;
const PROVIDER_STATUS_TICK_MS: u64 = 1000;

fn retryable_provider_start_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    // Transport/send failures: request never reached a durable response, so
    // retrying is safe at this boundary (no stream has started yet).
    if lower.contains("error sending request")
        || lower.contains("connection reset")
        || lower.contains("connection closed")
        || lower.contains("connection refused")
        || lower.contains("dns")
        || lower.contains("tls")
        || lower.contains("timed out")
        || lower.contains("timeout")
    {
        return true;
    }

    // Provider overload/rate-limit classes. Status-specific auth, validation,
    // spend, and context errors (4xx other than 429) are not retryable.
    lower.contains("returned 429")
        || lower.contains("returned 500")
        || lower.contains("returned 502")
        || lower.contains("returned 503")
        || lower.contains("returned 504")
        || lower.contains("returned 529")
}

fn provider_start_retry_delay_ms(attempt: u32) -> u64 {
    let exp = attempt.saturating_sub(1).min(4);
    let base = PROVIDER_START_BACKOFF_BASE_MS.saturating_mul(1_u64 << exp);
    // Small deterministic jitter: enough to de-phase concurrent sessions,
    // stable enough that tests don't flake or need RNG plumbing.
    let jitter = now_unix_ms() % 125;
    base.saturating_add(jitter)
}

fn retry_callout_body(error: &str, attempt: u32, delay_ms: u64) -> serde_json::Value {
    serde_json::json!({
        "attempt": attempt,
        "max_attempts": PROVIDER_START_MAX_ATTEMPTS,
        "delay_ms": delay_ms,
        "error": error,
        "boundary": "provider.stream returned before first token; mid-stream errors are not retried",
    })
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

    let mut buffered: Vec<AgentInput> = Vec::new();
    let (cancel_tx, mut stream) = {
        let mut attempt = 1;
        loop {
            let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
            // mu-yqeq.8: the cache-annotated `ProviderMessages` projection is
            // the canonical agent-loop → provider input. Per-provider
            // adapters consume it via `MessageInput::Projected` and produce
            // byte-equivalent wire JSON to the pre-cutover Legacy path (plus
            // cache_control driven by the projection's cache_marker flags).
            match provider
                .stream(
                    system_prompt,
                    effort,
                    MessageInput::Projected(projection),
                    tool_specs,
                    cancel_rx,
                )
                .await
            {
                Ok(stream) => break (cancel_tx, stream),
                Err(e) => {
                    let message = e.to_string();
                    if attempt >= PROVIDER_START_MAX_ATTEMPTS
                        || !retryable_provider_start_error(&message)
                    {
                        return Err(Outcome::Error(message));
                    }
                    let delay_ms = provider_start_retry_delay_ms(attempt);
                    let _ = events
                        .send(AgentEvent::Callout {
                            category: "warning".to_owned(),
                            title: "provider request retrying".to_owned(),
                            body: retry_callout_body(&message, attempt, delay_ms),
                            theme: Some("warning".to_owned()),
                            context_refs: vec!["bead:mu-tds4".to_owned()],
                        })
                        .await;

                    let mut slept = false;
                    while !slept {
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {
                                slept = true;
                            }
                            input_opt = input_rx.recv() => match input_opt {
                                Some(AgentInput::Cancel) => {
                                    return Err(Outcome::Cancelled);
                                }
                                Some(AgentInput::CancelOutstanding { reason }) => {
                                    return Err(Outcome::OutstandingCancelled { reason });
                                }
                                Some(input) => buffered.push(input),
                                None => {
                                    // Input side closed; still retry this provider call so
                                    // daemon shutdown semantics stay compatible with the old
                                    // path, which only noticed EOF while streaming.
                                    slept = true;
                                }
                            }
                        }
                    }
                    attempt += 1;
                }
            }
        }
    };

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
                            ContentBlock::Thinking {
                                text: block_text, ..
                            } => thinking.push_str(block_text),
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
                | Some(input @ AgentInput::DialogueMessage { .. })
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

#[cfg(test)]
mod tests {
    use super::retryable_provider_start_error;

    #[test]
    fn retryable_start_error_classification_is_narrow() {
        assert!(retryable_provider_start_error(
            "anthropic request: error sending request for url"
        ));
        assert!(retryable_provider_start_error(
            "openrouter returned 529: overloaded"
        ));
        assert!(retryable_provider_start_error(
            "openai returned 429: rate limit"
        ));

        assert!(!retryable_provider_start_error(
            "anthropic returned 402: insufficient credits"
        ));
        assert!(!retryable_provider_start_error(
            "codex returned 401: unauthorized"
        ));
        assert!(!retryable_provider_start_error(
            "openrouter returned 400: bad request"
        ));
    }
}
