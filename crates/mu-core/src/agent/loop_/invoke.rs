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
/// Ceiling on any single retry pause, server-suggested or backoff.
const MAX_RETRY_DELAY_MS: u64 = 30_000;

/// mu-197pd: liveness deadlines for a silent provider stream. A peer
/// that dies without FIN/RST (host freeze, yanked cable) leaves the
/// connection half-open: `stream.next()` neither resolves nor errors,
/// TCP keepalive is hours away, and the session sleeps forever — the
/// operator's watch never rewakes (observed 2026-08-27: box froze at
/// 16:44 mid-experiment; the mu session sat in `streaming` until found
/// by eye). The status tick converts sustained byte-silence into an
/// error like any other stream failure. Awaiting-first-token tolerates
/// more silence than mid-stream: a 200k+ prefill on a local lane
/// legitimately takes minutes.
const FIRST_TOKEN_STALL_SECS: u64 = 600;
const STREAM_STALL_SECS: u64 = 300;

fn retryable_provider_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    // Never retry: the request itself is the problem, or the quota window
    // is exhausted — a few seconds of backoff cannot help, and delaying
    // the surfaced error just hides the real cause.
    if lower.contains("usage_limit_reached")
        || lower.contains("insufficient_quota")
        || lower.contains("context_length_exceeded")
        || lower.contains("exceeds the context window")
        || lower.contains("invalid_prompt")
        || lower.contains("cyber_policy")
        || lower.contains("credentials")
    {
        return false;
    }

    // Transport/send failures: no (complete) response was received, so
    // retrying is safe when nothing has streamed to the client yet.
    if lower.contains("error sending request")
        || lower.contains("connection reset")
        || lower.contains("connection closed")
        || lower.contains("connection refused")
        || lower.contains("dns")
        || lower.contains("tls")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("stream transport error")
    {
        return true;
    }

    // Provider overload/rate-limit classes, in both the HTTP-status shape
    // ("openai returned 429 ...", now carrying the body's code —
    // `slow_down` for a traffic ramp, `server_is_overloaded` for a
    // temporary 503 — and any Retry-After as "(retry after Ns)", mu #595)
    // and the stream-error shape composed by mu-openai
    // ("rate_limit_exceeded: ... (http 429)"). Status-specific auth,
    // validation, spend, and context errors are not retryable.
    lower.contains("returned 429")
        || lower.contains("returned 500")
        || lower.contains("returned 502")
        || lower.contains("returned 503")
        || lower.contains("returned 504")
        || lower.contains("returned 529")
        || lower.contains("rate_limit")
        || lower.contains("overloaded")
        || lower.contains("slow_down")
        || lower.contains("(http 429")
        || lower.contains("(http 500")
        || lower.contains("(http 502")
        || lower.contains("(http 503")
        || lower.contains("(http 504")
        || lower.contains("(http 529")
}

/// Parse a server-suggested retry delay out of an error message. Two
/// sources, in order of trust: the canonical `(retry after 12s)` suffix
/// that mu-ai's shared renderer appends from a `Retry-After` header (mu
/// #595 — OpenAI sends it with both `429 slow_down` and `503
/// server_is_overloaded`), then the prose hint OpenAI and Azure put in
/// rate-limit bodies: "Please try again in 11.054s." / "in 28ms" / "in 35
/// seconds". None ⇒ use exponential backoff.
fn server_suggested_delay_ms(message: &str) -> Option<u64> {
    let lower = message.to_ascii_lowercase();
    // The renderer's suffix is parenthesised, so prose that merely says
    // "retry after the reset window" cannot claim this branch; when the
    // suffix is absent or does not parse, the prose hint still counts.
    lower
        .rfind("(retry after ")
        .and_then(|i| delay_from(&lower[i + "(retry after ".len()..]))
        .or_else(|| {
            let i = lower.find("try again in ")?;
            delay_from(&lower[i + "try again in ".len()..])
        })
}

/// "11.054s" / "28ms" / "35 seconds" at the head of `tail`, in ms,
/// clamped to the retry ceiling. Zero is no suggestion.
fn delay_from(tail: &str) -> Option<u64> {
    let num: String = tail
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let value: f64 = num.parse().ok()?;
    let unit = tail[num.len()..].trim_start();
    let ms = if unit.starts_with("ms") {
        value
    } else if unit.starts_with('s') {
        value * 1000.0
    } else {
        return None;
    };
    let ms = ms as u64;
    (ms > 0).then(|| ms.min(MAX_RETRY_DELAY_MS))
}

fn provider_retry_delay_ms(attempt: u32, error: &str) -> u64 {
    if let Some(suggested) = server_suggested_delay_ms(error) {
        return suggested;
    }
    let exp = attempt.saturating_sub(1).min(4);
    let base = PROVIDER_START_BACKOFF_BASE_MS.saturating_mul(1_u64 << exp);
    // Small deterministic jitter: enough to de-phase concurrent sessions,
    // stable enough that tests don't flake or need RNG plumbing.
    let jitter = now_unix_ms() % 125;
    base.saturating_add(jitter).min(MAX_RETRY_DELAY_MS)
}

fn retry_callout_body(error: &str, attempt: u32, delay_ms: u64) -> serde_json::Value {
    serde_json::json!({
        "attempt": attempt,
        "max_attempts": PROVIDER_START_MAX_ATTEMPTS,
        "delay_ms": delay_ms,
        "error": error,
        "boundary": "retried only while no output has streamed; \
                     errors after the first token fail the ask",
    })
}

/// Cancellation-aware backoff pause shared by the start-error and
/// stream-error retry paths. User input arriving mid-pause is buffered;
/// cancels abort the ask immediately.
async fn backoff_or_cancel(
    delay_ms: u64,
    input_rx: &mut mpsc::Receiver<AgentInput>,
    buffered: &mut Vec<AgentInput>,
) -> Result<(), Outcome> {
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
    Ok(())
}

#[allow(clippy::too_many_arguments)] // same shape as handle_execute_tools
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
    // mu-htbz0: caller-owned so each exit path can decide what happens to
    // driver inputs drained off `input_rx` during the stream. The caller
    // requeues them on OutstandingCancelled and Error (the ask aborts, the
    // inputs start the next ask); on full Cancel the session is ending and
    // they are dropped with it (an orphaned ticket there is INV-4-allowed).
    // Returning the vec only on the Ok path silently dropped inputs buffered
    // before a narrow-cancel or stream error.
    buffered: &mut Vec<AgentInput>,
) -> Result<AssistantMessage, Outcome> {
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

    // (mu-htbz0: `buffered` is the caller's vec — see the signature note.)
    // One attempt budget covers BOTH failure surfaces: provider.stream()
    // refusing to start (HTTP-level, bead mu-tds4) and an error event
    // arriving on the stream before any output token (SSE-level, bead
    // mu-openai-stream-retry-y0dw). Once anything has streamed, errors
    // fail the ask — a re-run would duplicate already-emitted deltas.
    let mut attempt: u32 = 1;
    let mut tick_interval =
        tokio::time::interval(std::time::Duration::from_millis(PROVIDER_STATUS_TICK_MS));
    tick_interval.tick().await;
    let mut input_drained = false;

    'attempt: loop {
        let (cancel_tx, mut stream) = {
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
                            || !retryable_provider_error(&message)
                        {
                            return Err(Outcome::Error(message));
                        }
                        let delay_ms = provider_retry_delay_ms(attempt, &message);
                        let _ = events
                            .send(AgentEvent::Callout {
                                category: "warning".to_owned(),
                                title: "provider request retrying".to_owned(),
                                body: retry_callout_body(&message, attempt, delay_ms),
                                theme: Some("warning".to_owned()),
                                context_refs: vec!["bead:mu-tds4".to_owned()],
                            })
                            .await;
                        backoff_or_cancel(delay_ms, input_rx, &mut *buffered).await?;
                        attempt += 1;
                    }
                }
            }
        };

        let mut bytes_received: u64 = 0;
        let mut seen_first_token = false;
        let mut current_state = ProviderStatusKind::AwaitingFirstToken;
        // mu-197pd: byte-progress snapshot for the stall check in the
        // tick arm. tokio Instant so paused-time tests can drive it.
        let mut last_bytes_seen: u64 = 0;
        let mut quiet_since = tokio::time::Instant::now();
        // Per-attempt clocks: a retried call's awaiting-first-token wait
        // is measured from ITS start, not the original call's (attempt 1
        // is within a tick of call_started_at, so nothing shifts there).
        let mut state_started_at = Instant::now();
        let mut state_started_unix_ms = now_unix_ms();

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
                        return Ok(msg);
                    }
                    Some(super::super::provider::ProviderEvent::Error(e)) => {
                        let _ = cancel_tx.send(());
                        if seen_first_token
                            || attempt >= PROVIDER_START_MAX_ATTEMPTS
                            || !retryable_provider_error(&e)
                        {
                            return Err(Outcome::Error(e));
                        }
                        // Release the dead stream (and its connection)
                        // before sleeping — the backoff can be tens of
                        // seconds.
                        drop(stream);
                        let delay_ms = provider_retry_delay_ms(attempt, &e);
                        let _ = events
                            .send(AgentEvent::Callout {
                                category: "warning".to_owned(),
                                title: "provider stream retrying".to_owned(),
                                body: retry_callout_body(&e, attempt, delay_ms),
                                theme: Some("warning".to_owned()),
                                context_refs: vec!["bead:mu-openai-stream-retry-y0dw".to_owned()],
                            })
                            .await;
                        backoff_or_cancel(delay_ms, input_rx, &mut *buffered).await?;
                        attempt += 1;
                        // Fresh call: put the status projection back to
                        // awaiting-first-token, clock starting now.
                        let _ = events
                            .send(AgentEvent::ProviderStatus {
                                state: ProviderStatusKind::AwaitingFirstToken,
                                started_at_unix_ms: now_unix_ms(),
                                elapsed_ms: 0,
                                bytes_received: None,
                                tool_call_id: None,
                            })
                            .await;
                        continue 'attempt;
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
                        // tool-only turn may produce no text at all). Name deltas
                        // count too: Anthropic opens every tool_use block with a
                        // name-only delta, and the mu-197pd stall watchdog keys
                        // liveness on bytes_received — progress that doesn't
                        // count is progress the watchdog can misread as silence
                        // (ci-aipr panel finding).
                        if let Some(name) = name_delta.as_deref() {
                            bytes_received = bytes_received.saturating_add(name.len() as u64);
                        }
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
                    | Some(input @ AgentInput::MailboxMessage { .. })
                    | Some(input @ AgentInput::ClearContext { .. }) => {
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
                    // mu-197pd: stall check. Byte progress since the
                    // last tick resets the clock; sustained silence past
                    // the phase deadline is treated as a stream failure —
                    // a half-open connection to a dead peer produces
                    // exactly this shape and would otherwise hang the
                    // session forever.
                    if bytes_received != last_bytes_seen {
                        last_bytes_seen = bytes_received;
                        quiet_since = tokio::time::Instant::now();
                    } else {
                        let limit_secs = if seen_first_token {
                            STREAM_STALL_SECS
                        } else {
                            FIRST_TOKEN_STALL_SECS
                        };
                        if quiet_since.elapsed().as_secs() >= limit_secs {
                            let message = format!(
                                "provider stream stalled: no bytes for {limit_secs}s \
                                 ({} phase) — treating the connection as dead",
                                if seen_first_token { "streaming" } else { "awaiting-first-token" },
                            );
                            let _ = cancel_tx.send(());
                            if seen_first_token || attempt >= PROVIDER_START_MAX_ATTEMPTS {
                                return Err(Outcome::Error(message));
                            }
                            drop(stream);
                            let delay_ms = provider_retry_delay_ms(attempt, &message);
                            let _ = events
                                .send(AgentEvent::Callout {
                                    category: "warning".to_owned(),
                                    title: "provider stream stalled — retrying".to_owned(),
                                    body: retry_callout_body(&message, attempt, delay_ms),
                                    theme: Some("warning".to_owned()),
                                    context_refs: vec!["bead:mu-197pd".to_owned()],
                                })
                                .await;
                            backoff_or_cancel(delay_ms, input_rx, &mut *buffered).await?;
                            attempt += 1;
                            let _ = events
                                .send(AgentEvent::ProviderStatus {
                                    state: ProviderStatusKind::AwaitingFirstToken,
                                    started_at_unix_ms: now_unix_ms(),
                                    elapsed_ms: 0,
                                    bytes_received: None,
                                    tool_call_id: None,
                                })
                                .await;
                            continue 'attempt;
                        }
                    }
                },
            }
        }
    } // 'attempt
}

#[cfg(test)]
mod tests {
    use super::{retryable_provider_error, server_suggested_delay_ms};

    #[test]
    fn retryable_error_classification_is_narrow() {
        assert!(retryable_provider_error(
            "anthropic request: error sending request for url"
        ));
        assert!(retryable_provider_error(
            "openrouter returned 529: overloaded"
        ));
        assert!(retryable_provider_error("openai returned 429: rate limit"));
        // Stream-error shapes composed by mu-openai (#540).
        assert!(retryable_provider_error(
            "rate_limit_exceeded: Rate limit reached. Please try again in 11.054s. (http 429)"
        ));
        assert!(retryable_provider_error("server_is_overloaded (http 503)"));
        // The shared renderer's shapes (mu #595): code after the status,
        // Retry-After as a suffix.
        assert!(retryable_provider_error(
            "openai returned 429 Too Many Requests slow_down: Traffic is ramping too fast. (retry after 12s)"
        ));
        assert!(retryable_provider_error(
            "openrouter returned 503 Service Unavailable server_is_overloaded: The model is temporarily overloaded. (retry after 4s)"
        ));
        assert!(retryable_provider_error(
            "anthropic returned 529 <unknown status code> overloaded_error: Overloaded (retry after 3s)"
        ));
        assert!(retryable_provider_error(
            "openai stream transport error: connection reset by peer"
        ));

        assert!(!retryable_provider_error(
            "anthropic returned 402: insufficient credits"
        ));
        assert!(!retryable_provider_error(
            "codex returned 401: unauthorized"
        ));
        assert!(!retryable_provider_error(
            "openrouter returned 400: bad request"
        ));
        // Quota-window and request-shape errors: backoff cannot help.
        assert!(!retryable_provider_error(
            "usage_limit_reached: The usage limit has been reached (http 429)"
        ));
        assert!(!retryable_provider_error(
            "context_length_exceeded: Your input exceeds the context window of this model. (http 400)"
        ));
        assert!(!retryable_provider_error("openai stream error"));
    }

    #[test]
    fn server_suggested_delay_parses_openai_and_azure_shapes() {
        assert_eq!(
            server_suggested_delay_ms("Rate limit reached. Please try again in 11.054s. Visit x"),
            Some(11054)
        );
        assert_eq!(
            server_suggested_delay_ms("Please try again in 28ms."),
            Some(28)
        );
        assert_eq!(
            server_suggested_delay_ms("Rate limit exceeded. Try again in 25 seconds."),
            Some(25_000)
        );
        // Ceiling: a server asking for minutes is capped.
        assert_eq!(
            server_suggested_delay_ms("try again in 300s"),
            Some(super::MAX_RETRY_DELAY_MS)
        );
        assert_eq!(server_suggested_delay_ms("no delay here"), None);
    }

    #[test]
    fn retry_after_suffix_wins_over_body_prose_and_is_capped() {
        // The header-derived suffix is authoritative even when the body's
        // prose says otherwise.
        assert_eq!(
            server_suggested_delay_ms(
                "openai returned 429 Too Many Requests slow_down: Please try again in 2s. (retry after 12s)"
            ),
            Some(12_000)
        );
        assert_eq!(
            server_suggested_delay_ms("openrouter returned 503 Service Unavailable server_is_overloaded: busy (retry after 4s)"),
            Some(4_000)
        );
        // Zero is no suggestion (the renderer never emits it, but a body
        // could): back off instead of firing at once.
        assert_eq!(server_suggested_delay_ms("x (retry after 0s)"), None);
        assert_eq!(server_suggested_delay_ms("please try again in 0s"), None);
        // Prose that only mentions the phrase does not eat the real hint.
        assert_eq!(
            server_suggested_delay_ms(
                "Rate limit reached; please retry after the reset window. Please try again in 11.054s."
            ),
            Some(11_054)
        );
        // A server asking for minutes is capped like the prose form.
        assert_eq!(
            server_suggested_delay_ms("x (retry after 120s)"),
            Some(super::MAX_RETRY_DELAY_MS)
        );
    }
}
