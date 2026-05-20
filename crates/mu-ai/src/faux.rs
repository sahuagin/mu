use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use tokio::sync::oneshot;

use mu_core::agent::{
    AgentMessage, AssistantMessage, ContentBlock, Provider, ProviderError, ProviderEvent,
    StopReason, ToolSpec,
};

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
}

#[async_trait]
impl Provider for FauxProvider {
    async fn stream(
        &self,
        // mu-n48: faux ignores system_prompt — it's a deterministic
        // echo / scripted provider; no need to thread it through the
        // synthetic event sequence.
        _system_prompt: Option<&str>,
        messages: &[AgentMessage],
        _tools: &[ToolSpec],
        _cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let response = {
            let mut q = self
                .responses
                .lock()
                .map_err(|_| ProviderError::Other("faux provider mutex poisoned".to_owned()))?;
            q.pop_front().or_else(|| self.fallback.clone())
        };
        let events = match response {
            None => Vec::new(),
            Some(FauxResponse::Script(es)) => es,
            Some(FauxResponse::Echo) => echo_events(messages),
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
            .stream(None, &messages, &[], cancel_rx)
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
        let stream = provider.stream(None, &[], &[], cancel_rx).await?;
        Ok(stream.collect().await)
    }
}
