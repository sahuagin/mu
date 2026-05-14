use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::oneshot;

use crate::context::{CacheStrategy, FauxProviderRenderer, NoCacheStrategy, ProviderRenderer};

use super::tool::ToolSpec;
use super::types::{AgentMessage, AssistantMessage};

/// Events from a provider's streaming response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProviderEvent {
    /// Streaming text chunk.
    TextDelta(String),
    /// Streaming reasoning chunk (Anthropic extended thinking, OpenAI o1).
    /// Optional — providers without reasoning never emit this.
    ThinkingDelta(String),
    /// Streaming partial tool call. Provider may emit multiple deltas
    /// before the call is finalized in the Done payload.
    ToolCallDelta {
        id: String,
        name_delta: Option<String>,
        arguments_delta: Option<String>,
    },
    /// Stream ended successfully. Final assistant message attached.
    Done(AssistantMessage),
    /// Stream errored. Caller should map this to Outcome::Error.
    Error(String),
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("provider: {0}")]
    Other(String),
}

/// LLM provider abstraction.
///
/// Concrete implementations live in mu-ai. mu-core only knows the
/// trait. This is the seam for cancel propagation: callers pass a
/// `oneshot::Receiver<()>`; the provider awaits it via `select!` and
/// terminates the stream when it fires.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Open a streaming response.
    ///
    /// Implementations OWN `cancel_rx`. When the matching sender
    /// fires, the implementation must terminate the stream
    /// promptly — emit `ProviderEvent::Done` with
    /// `stop_reason: StopReason::Aborted` if a partial message is
    /// available, otherwise `ProviderEvent::Error`.
    async fn stream(
        &self,
        // mu-n48: optional system prompt. Each impl decides how to
        // render it in its provider-specific request shape (Anthropic
        // has a top-level `system` field; OpenAI-style providers
        // prepend a {role: "system"} message). None preserves the
        // pre-mu-n48 behavior of "no system prompt sent."
        system_prompt: Option<&str>,
        messages: &[AgentMessage],
        tools: &[ToolSpec],
        cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError>;

    /// mu-fb0: the [`ProviderRenderer`] this provider uses to project a
    /// `RetainedRope` into provider-shaped messages. The agent loop
    /// builds the rope from session state and renders it before each
    /// model call so `ContextAssembly` events carry rope-derived
    /// provenance. Default: [`FauxProviderRenderer`] — appropriate
    /// for providers that have not yet declared a renderer (the wire
    /// request itself still goes through `stream()` with raw
    /// `AgentMessage`s; the renderer drives the rope projection and
    /// the per-call `ContextAssembly` event).
    fn renderer(&self) -> Arc<dyn ProviderRenderer> {
        Arc::new(FauxProviderRenderer::new())
    }

    /// mu-fb0: the [`CacheStrategy`] this provider uses to derive
    /// cache-boundary positions from the rope. Default:
    /// [`NoCacheStrategy`] — correct for providers without prompt
    /// caching support. Anthropic overrides to
    /// `AnthropicCacheStrategy`.
    fn cache_strategy(&self) -> Arc<dyn CacheStrategy> {
        Arc::new(NoCacheStrategy::new())
    }

    /// mu-fb0: short stable identifier of the provider's renderer +
    /// cache-strategy pair. Surfaces in `AgentEvent::ContextAssembly`
    /// so consumers can group calls by render policy without
    /// parsing trait-object type names. The default `"faux"` is the
    /// no-op pair (FauxProviderRenderer + NoCacheStrategy).
    fn provider_label(&self) -> &'static str {
        "faux"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::types::{ContentBlock, StopReason};

    #[test]
    fn provider_event_round_trips() -> Result<(), serde_json::Error> {
        let samples = [
            ProviderEvent::TextDelta("hello".to_owned()),
            ProviderEvent::ThinkingDelta("reasoning".to_owned()),
            ProviderEvent::ToolCallDelta {
                id: "call-1".to_owned(),
                name_delta: Some("echo".to_owned()),
                arguments_delta: Some("{\"text\":\"hi\"}".to_owned()),
            },
            ProviderEvent::Done(AssistantMessage {
                content: vec![ContentBlock::Text {
                    text: "done".to_owned(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: None,
            }),
            ProviderEvent::Error("rate limit".to_owned()),
        ];

        for event in samples {
            let value = serde_json::to_value(&event)?;
            let decoded: ProviderEvent = serde_json::from_value(value.clone())?;
            let decoded_value = serde_json::to_value(decoded)?;
            assert_eq!(decoded_value, value);
        }
        Ok(())
    }

    #[test]
    fn provider_trait_is_send_and_sync() {
        fn assert_send<T: Send + Sync + ?Sized>() {}
        assert_send::<dyn Provider>();
    }
}
