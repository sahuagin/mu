use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use tokio::sync::oneshot;

use mu_core::agent::{
    AgentMessage, AssistantMessage, ContentBlock, MessageInput, Provider, ProviderError,
    ProviderEvent, StopReason, ToolSpec,
};
use mu_core::context::{ProviderMessages, ProviderRole};

/// What a single FauxProvider::stream() call should produce.
#[derive(Debug, Clone)]
pub enum FauxResponse {
    /// Emit these events, in order.
    Script(Vec<ProviderEvent>),
    /// Echo the most recent user message back as a single TextDelta
    /// followed by Done(text + EndTurn).
    Echo,
}

/// Concrete Provider impl for testing and dev mode.
///
/// Two construction patterns:
/// - `scripted([resp1, resp2, ...])`: each `stream()` call pops the
///   next response off a FIFO queue. Out of responses → empty stream.
/// - `echo()`: every `stream()` call uses Echo mode.
pub struct FauxProvider {
    responses: Mutex<VecDeque<FauxResponse>>,
    /// If non-None, this is used when the queue is empty (instead of
    /// the empty-stream fallback).
    fallback: Option<FauxResponse>,
}

impl FauxProvider {
    pub fn scripted(responses: Vec<FauxResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            fallback: None,
        }
    }

    /// Echo always. Convenient default for `mu serve` smoke tests.
    pub fn echo() -> Self {
        Self {
            responses: Mutex::new(VecDeque::new()),
            fallback: Some(FauxResponse::Echo),
        }
    }

    /// Pop the next response off the queue, or fall back to
    /// `self.fallback` when the queue is empty.
    fn pop_response(&self) -> Result<Option<FauxResponse>, ProviderError> {
        let mut q = self
            .responses
            .lock()
            .map_err(|_| ProviderError::Other("faux provider mutex poisoned".to_owned()))?;
        Ok(q.pop_front().or_else(|| self.fallback.clone()))
    }
}

#[async_trait]
impl Provider for FauxProvider {
    async fn stream(
        &self,
        // mu-n48: faux ignores system_prompt — it's a deterministic
        // echo / scripted provider; no need to thread it through the
        // synthetic event sequence.
        _system_prompt: Option<&str>,
        // mu-vcbm: faux has no reasoning knob — effort is ignored.
        _effort: Option<&str>,
        input: MessageInput<'_>,
        _tools: &[ToolSpec],
        _cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        // mu-yqeq.7: sealed-enum dispatch (Legacy + Projected). The
        // `_` arm remains for forward-compat with future MessageInput
        // variants — adding one will compile-warn here for review.
        //
        // Input validation happens BEFORE queue pop so an unrecognized
        // variant doesn't consume a scripted response. Scripted
        // responses are independent of input shape; only the Echo
        // path reads the input to find the last User message.
        let events = match input {
            MessageInput::Legacy(msgs) => match self.pop_response()? {
                None => Vec::new(),
                Some(FauxResponse::Script(es)) => es,
                Some(FauxResponse::Echo) => echo_events(msgs),
            },
            MessageInput::Projected(pmsgs) => match self.pop_response()? {
                None => Vec::new(),
                Some(FauxResponse::Script(es)) => es,
                Some(FauxResponse::Echo) => echo_events_from_projection(pmsgs),
            },
            _ => {
                return Err(ProviderError::Other(
                    "FauxProvider: unrecognized MessageInput variant".to_string(),
                ));
            }
        };
        Ok(Box::pin(stream::iter(events)))
    }
}

fn echo_events(messages: &[AgentMessage]) -> Vec<ProviderEvent> {
    let text = messages
        .iter()
        .rev()
        .find_map(|m| match m {
            AgentMessage::User { content } => Some(content.clone()),
            _ => None,
        })
        .unwrap_or_default();
    build_echo_events(text)
}

/// mu-yqeq.7: same echo semantics against the projected
/// `ProviderMessages` shape — find the most recent `User`-role
/// message and emit a TextDelta + Done pair carrying its content.
fn echo_events_from_projection(pmsgs: &ProviderMessages) -> Vec<ProviderEvent> {
    let text = pmsgs
        .messages
        .iter()
        .rev()
        .find_map(|m| match m.role() {
            ProviderRole::User => Some(m.content().to_string()),
            _ => None,
        })
        .unwrap_or_default();
    build_echo_events(text)
}

fn build_echo_events(text: String) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::TextDelta(text.clone()),
        ProviderEvent::Done(AssistantMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            stop_reason: StopReason::EndTurn,
            usage: None,
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[tokio::test]
    async fn echo_returns_last_user_message() -> TestResult {
        let provider = FauxProvider::echo();
        let messages = vec![
            AgentMessage::User {
                content: "first".to_owned(),
            },
            AgentMessage::Assistant(AssistantMessage {
                content: vec![ContentBlock::Text {
                    text: "assistant".into(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: None,
            }),
            AgentMessage::User {
                content: "hello".to_owned(),
            },
        ];
        let (_cancel_tx, cancel_rx) = oneshot::channel();

        let events: Vec<ProviderEvent> = provider
            .stream(None, None, MessageInput::Legacy(&messages), &[], cancel_rx)
            .await?
            .collect()
            .await;

        assert_eq!(events.len(), 2);
        assert!(
            matches!(events.first(), Some(ProviderEvent::TextDelta(delta)) if delta == "hello")
        );
        assert!(matches!(
            events.get(1),
            Some(ProviderEvent::Done(AssistantMessage {
                content,
                stop_reason: StopReason::EndTurn,
                usage: _,
            })) if content == &vec![ContentBlock::Text { text: "hello".into() }]
        ));
        Ok(())
    }

    #[tokio::test]
    async fn scripted_drains_in_fifo_order() -> TestResult {
        let provider = FauxProvider::scripted(vec![
            FauxResponse::Script(vec![ProviderEvent::TextDelta("one".to_owned())]),
            FauxResponse::Script(vec![ProviderEvent::TextDelta("two".to_owned())]),
        ]);

        let first = collect_stream(&provider).await?;
        let second = collect_stream(&provider).await?;
        let third = collect_stream(&provider).await?;

        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert!(third.is_empty());
        assert!(matches!(first.first(), Some(ProviderEvent::TextDelta(delta)) if delta == "one"));
        assert!(matches!(second.first(), Some(ProviderEvent::TextDelta(delta)) if delta == "two"));
        Ok(())
    }

    #[tokio::test]
    async fn out_of_responses_returns_empty_stream() -> TestResult {
        let provider = FauxProvider::scripted(vec![]);

        let first = collect_stream(&provider).await?;
        let second = collect_stream(&provider).await?;

        assert!(first.is_empty());
        assert!(second.is_empty());
        Ok(())
    }

    async fn collect_stream(provider: &FauxProvider) -> Result<Vec<ProviderEvent>, ProviderError> {
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        let stream = provider
            .stream(None, None, MessageInput::Legacy(&[]), &[], cancel_rx)
            .await?;
        Ok(stream.collect().await)
    }

    // ========================================================================
    // mu-yqeq.7 parity tests — Projected echo matches Legacy echo, and
    // scripted responses are unaffected by the input shape.
    // ========================================================================

    /// Serialize a `ProviderEvent` slice to JSON for cross-path
    /// comparison — `ProviderEvent` doesn't derive `PartialEq`, but
    /// it does derive `Serialize`, so JSON equality is the practical
    /// stand-in.
    fn events_as_json(events: &[ProviderEvent]) -> serde_json::Value {
        serde_json::to_value(events).expect("ProviderEvent serializes")
    }

    #[tokio::test]
    async fn yqeq7_echo_from_projected_matches_legacy() -> TestResult {
        use mu_core::context::{
            assemble_rope, FauxProviderRenderer, ProjectionTarget, ProviderRenderer,
        };

        let messages = vec![
            AgentMessage::User {
                content: "first".to_owned(),
            },
            AgentMessage::Assistant(AssistantMessage {
                content: vec![ContentBlock::Text {
                    text: "assistant".into(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: None,
            }),
            AgentMessage::User {
                content: "hello".to_owned(),
            },
        ];

        // Legacy path.
        let legacy_provider = FauxProvider::echo();
        let (_tx_l, rx_l) = oneshot::channel();
        let legacy_events: Vec<ProviderEvent> = legacy_provider
            .stream(None, None, MessageInput::Legacy(&messages), &[], rx_l)
            .await?
            .collect()
            .await;

        // Projected path with the same scenario.
        let rope = assemble_rope(None, &messages, &[]);
        let projection = FauxProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
        let projected_provider = FauxProvider::echo();
        let (_tx_p, rx_p) = oneshot::channel();
        let projected_events: Vec<ProviderEvent> = projected_provider
            .stream(None, None, MessageInput::Projected(&projection), &[], rx_p)
            .await?
            .collect()
            .await;

        assert_eq!(
            events_as_json(&legacy_events),
            events_as_json(&projected_events),
            "Legacy echo events != Projected echo events",
        );
        Ok(())
    }

    #[tokio::test]
    async fn yqeq7_scripted_still_works_with_projected_input() -> TestResult {
        use mu_core::context::{
            assemble_rope, FauxProviderRenderer, ProjectionTarget, ProviderRenderer,
        };

        let provider =
            FauxProvider::scripted(vec![FauxResponse::Script(vec![ProviderEvent::TextDelta(
                "scripted-reply".to_owned(),
            )])]);

        // Build a projection from any input — content is irrelevant
        // to a scripted response.
        let messages = vec![AgentMessage::User {
            content: "ignored".to_owned(),
        }];
        let rope = assemble_rope(None, &messages, &[]);
        let projection = FauxProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);

        let (_tx, rx) = oneshot::channel();
        let events: Vec<ProviderEvent> = provider
            .stream(None, None, MessageInput::Projected(&projection), &[], rx)
            .await?
            .collect()
            .await;

        assert_eq!(events.len(), 1);
        assert!(
            matches!(events.first(), Some(ProviderEvent::TextDelta(d)) if d == "scripted-reply"),
            "scripted response should pass through regardless of input shape: {events:?}",
        );
        Ok(())
    }
}
