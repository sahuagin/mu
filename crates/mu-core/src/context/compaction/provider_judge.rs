//! mu-kgu.11: Provider-backed [`Judge`] adapter — bridges the
//! synchronous [`Judge`] trait surface to the asynchronous
//! [`Provider::stream`] API so [`HashAndSummaryPolicy`] can be wired
//! to a real model (Anthropic Haiku, OpenRouter, etc.) instead of the
//! canned mock judge used in unit tests and the bench harness.
//!
//! ## Sync → async bridge
//!
//! [`Judge::judge`] is synchronous (matching the larger
//! [`CompactionPolicy::compact`] surface — see
//! `crates/mu-core/src/context/compaction.rs` for why). [`Provider::stream`]
//! is async and returns a [`futures::stream::BoxStream`]. The adapter
//! runs a **dedicated background OS thread** with its own
//! current-thread tokio runtime to host the async work, then receives
//! the result on a `std::sync::mpsc` channel.
//!
//! Why a dedicated thread instead of `Handle::current().block_on(...)`
//! or `tokio::task::block_in_place`:
//!
//! - The call site is inside the agent loop's tokio task. Calling
//!   `block_on` on the current handle would deadlock when the runtime
//!   is current-thread (which tests use).
//! - `block_in_place` requires a multi-threaded runtime; tests would
//!   panic.
//! - A dedicated thread costs ~one syscall + a fresh runtime per
//!   compaction event. Compaction events are rare (between turns;
//!   typically minutes apart in real usage), so the overhead is
//!   irrelevant. Latency is dominated by the model round-trip (~500-
//!   1500ms for Haiku).
//!
//! ## Failure model
//!
//! Per [`HashAndSummaryPolicy`]'s fail-closed contract: any error
//! here (network, model, timeout, channel breakage) returns
//! [`JudgeError::Call`]. [`HashAndSummaryPolicy::compact`] catches
//! the error and returns the original rope unchanged with a
//! [`CompactionDecision::Failed`] entry. A live judge that times out
//! or 503s degrades to "no compaction this turn" rather than corrupting
//! the rope.
//!
//! [`CompactionDecision::Failed`]: super::CompactionDecision
//! [`HashAndSummaryPolicy`]: super::hash_summary::HashAndSummaryPolicy
//! [`HashAndSummaryPolicy::compact`]: super::hash_summary::HashAndSummaryPolicy::compact

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::oneshot;

use crate::agent::types::AgentMessage;
use crate::agent::{MessageInput, Provider, ProviderEvent};

use super::hash_summary::{Judge, JudgeError};

/// Default wall-clock cap for one [`ProviderJudge::judge`] call.
/// Matches `[compaction.judge].timeout_secs` default from mu-l1z's
/// [`crate::config::CompactionJudgeConfig`].
pub const DEFAULT_JUDGE_TIMEOUT: Duration = Duration::from_secs(60);

/// Provider-backed [`Judge`].
///
/// Each [`Judge::judge`] call:
/// 1. Spawns a background thread with a fresh current-thread tokio runtime.
/// 2. In that runtime, opens a single-turn `provider.stream(None, &[user], &[], cancel_rx)`.
/// 3. Collects [`ProviderEvent::TextDelta`] payloads into a `String`.
/// 4. Returns on first [`ProviderEvent::Done`] / [`ProviderEvent::Error`] or timeout.
/// 5. Sends the result back through a `std::sync::mpsc` channel.
#[derive(Clone)]
pub struct ProviderJudge {
    provider: Arc<dyn Provider>,
    timeout: Duration,
}

impl std::fmt::Debug for ProviderJudge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderJudge")
            .field("provider", &"<dyn Provider>")
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl ProviderJudge {
    /// Construct a [`ProviderJudge`] with the default timeout
    /// ([`DEFAULT_JUDGE_TIMEOUT`]).
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self {
            provider,
            timeout: DEFAULT_JUDGE_TIMEOUT,
        }
    }

    /// Builder-style override for the per-call wall-clock cap. Useful
    /// when an operator's config sets a non-default
    /// `[compaction.judge].timeout_secs`.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl Judge for ProviderJudge {
    fn judge(&self, prompt: &str) -> Result<String, JudgeError> {
        let provider = self.provider.clone();
        let prompt = prompt.to_string();
        let timeout = self.timeout;

        let (tx, rx) = std::sync::mpsc::channel::<Result<String, JudgeError>>();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(Err(JudgeError::Call(format!(
                        "could not build judge runtime: {e}"
                    ))));
                    return;
                }
            };
            let result = rt.block_on(call_provider(provider, &prompt, timeout));
            let _ = tx.send(result);
        });

        rx.recv()
            .map_err(|e| JudgeError::Call(format!("judge thread channel closed: {e}")))?
    }
}

/// Open the provider stream, collect text deltas, return on Done /
/// Error / timeout. Tool calls and thinking deltas are ignored: this
/// is a single-turn instruction-following call with no tool surface,
/// and reasoning content (if any) doesn't belong in the keep+summary
/// JSON shape.
async fn call_provider(
    provider: Arc<dyn Provider>,
    prompt: &str,
    timeout: Duration,
) -> Result<String, JudgeError> {
    let messages = vec![AgentMessage::User {
        content: prompt.to_string(),
    }];
    let (_cancel_tx, cancel_rx) = oneshot::channel();

    let work = async {
        let mut stream = provider
            .stream(None, MessageInput::Legacy(&messages), &[], cancel_rx)
            .await
            .map_err(|e| JudgeError::Call(format!("provider.stream open: {e}")))?;
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            match event {
                ProviderEvent::TextDelta(d) => text.push_str(&d),
                ProviderEvent::Done(msg) => {
                    // Some providers (Anthropic) deliver final content
                    // in the Done message rather than via TextDelta.
                    // Walk msg.content for any text blocks and append.
                    for block in &msg.content {
                        if let crate::agent::ContentBlock::Text { text: t } = block {
                            // Avoid duplication if it was already in
                            // deltas: a Done's final text often equals
                            // the concat of deltas. Treat "fallback":
                            // only append when no deltas arrived.
                            if text.is_empty() {
                                text.push_str(t);
                            }
                        }
                    }
                    return Ok(text);
                }
                ProviderEvent::Error(e) => {
                    return Err(JudgeError::Call(format!("provider stream error: {e}")));
                }
                ProviderEvent::ThinkingDelta(_) | ProviderEvent::ToolCallDelta { .. } => {
                    // Single-turn instruction-following call — tools
                    // and reasoning are not part of the keep+summary
                    // contract.
                }
            }
        }
        // Stream ended without Done — treat as error.
        Err(JudgeError::Call(
            "provider stream ended without Done".into(),
        ))
    };

    match tokio::time::timeout(timeout, work).await {
        Ok(r) => r,
        Err(_) => Err(JudgeError::Call(format!(
            "judge timed out after {:?}",
            timeout
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use futures::stream;
    use std::sync::Mutex;

    use crate::agent::tool::ToolSpec;
    use crate::agent::types::{AssistantMessage, ContentBlock, StopReason};
    use crate::agent::{Provider, ProviderError, ProviderEvent};

    /// Minimal Provider for tests: hand it a `Vec<ProviderEvent>` and
    /// each `stream()` call emits the events verbatim and finishes.
    /// Avoids a dev-dep on mu-ai (which would create a cargo cycle
    /// via mu-ai → mu-core; see Cargo.toml). The bench harness'
    /// `--judge live` path uses the real AnthropicProvider; this
    /// scaffold is for in-process unit tests only.
    struct ScriptedProvider {
        events: Mutex<Option<Vec<ProviderEvent>>>,
    }

    impl ScriptedProvider {
        fn new(events: Vec<ProviderEvent>) -> Self {
            Self {
                events: Mutex::new(Some(events)),
            }
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        async fn stream(
            &self,
            _system_prompt: Option<&str>,
            _input: MessageInput<'_>,
            _tools: &[ToolSpec],
            _cancel_rx: tokio::sync::oneshot::Receiver<()>,
        ) -> Result<futures::stream::BoxStream<'static, ProviderEvent>, ProviderError> {
            let events = self
                .events
                .lock()
                .ok()
                .and_then(|mut g| g.take())
                .unwrap_or_default();
            Ok(Box::pin(stream::iter(events)))
        }
    }

    fn done_text(text: &str) -> ProviderEvent {
        ProviderEvent::Done(AssistantMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            stop_reason: StopReason::EndTurn,
            usage: None,
        })
    }

    /// ProviderJudge should concatenate TextDelta payloads into a
    /// single string, returning on Done.
    #[test]
    fn provider_judge_collects_text_deltas_from_stream() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            ProviderEvent::TextDelta("{\"keep\":[\"abc\"".into()),
            ProviderEvent::TextDelta(",\"def\"],\"".into()),
            ProviderEvent::TextDelta("summary\":\"ok\"}".into()),
            done_text("{\"keep\":[\"abc\",\"def\"],\"summary\":\"ok\"}"),
        ]));

        let judge = ProviderJudge::new(provider);
        let text = judge.judge("dummy prompt").expect("judge returns Ok");
        assert_eq!(text, "{\"keep\":[\"abc\",\"def\"],\"summary\":\"ok\"}");
    }

    /// When the provider emits no TextDelta but the Done message
    /// carries a final Text content block, ProviderJudge falls back
    /// to that text. Some providers (Anthropic non-streaming-like
    /// transcripts) deliver the full message in Done instead of via
    /// deltas; the adapter must work in both shapes.
    #[test]
    fn provider_judge_falls_back_to_done_text_when_no_deltas() {
        let provider = Arc::new(ScriptedProvider::new(vec![done_text(
            "{\"keep\":[],\"summary\":\"empty\"}",
        )]));

        let judge = ProviderJudge::new(provider);
        let text = judge.judge("dummy prompt").expect("judge returns Ok");
        assert_eq!(text, "{\"keep\":[],\"summary\":\"empty\"}");
    }

    /// ProviderEvent::Error during the stream surfaces as
    /// JudgeError::Call. The fail-closed contract in
    /// HashAndSummaryPolicy turns this into an unchanged rope.
    #[test]
    fn provider_judge_surfaces_stream_error() {
        let provider = Arc::new(ScriptedProvider::new(vec![ProviderEvent::Error(
            "503 from upstream".into(),
        )]));

        let judge = ProviderJudge::new(provider);
        let err = judge.judge("dummy").expect_err("error should surface");
        match err {
            JudgeError::Call(s) => {
                assert!(
                    s.contains("503 from upstream"),
                    "error should carry upstream message; got {s}",
                );
            }
        }
    }

    /// An empty event sequence simulates a stream that ends without
    /// Done. Treated as a JudgeError::Call so the policy fails closed.
    #[test]
    fn provider_judge_errors_on_stream_without_done() {
        let provider = Arc::new(ScriptedProvider::new(vec![]));
        let judge = ProviderJudge::new(provider);
        let err = judge.judge("dummy").expect_err("no Done → err");
        match err {
            JudgeError::Call(s) => {
                assert!(s.contains("without Done"), "unexpected msg: {s}");
            }
        }
    }

    /// End-to-end smoke: HashAndSummaryPolicy + ProviderJudge with a
    /// well-formed JSON response produces a non-Failed decision set.
    /// Confirms the adapter integrates with the policy.
    #[test]
    fn hash_and_summary_with_provider_judge_produces_non_failed_output() {
        use super::super::hash_summary::{Blake3Hasher, HashAndSummaryPolicy, Judge, SpanHasher};
        use super::super::CompactionPolicy;
        use crate::context::rope::{RetainedRope, RetentionClass, Span, SpanKind};

        let rope = RetainedRope::from_spans(vec![
            Span::new("u1", SpanKind::User, "user msg 1", RetentionClass::Hot),
            Span::new(
                "a1",
                SpanKind::Assistant,
                "assistant reply 1",
                RetentionClass::Hot,
            ),
            Span::new(
                "t1",
                SpanKind::ToolResult,
                "tool result body",
                RetentionClass::Warm,
            ),
        ]);
        let hash_u1 = Blake3Hasher.hash(&rope.spans()[0], 8);

        let response =
            format!("{{\"keep\":[\"{hash_u1}\"],\"summary\":\"early-context summary\"}}");
        let provider = Arc::new(ScriptedProvider::new(vec![
            ProviderEvent::TextDelta(response.clone()),
            done_text(&response),
        ]));

        let judge: Arc<dyn Judge> = Arc::new(ProviderJudge::new(provider));
        let policy = HashAndSummaryPolicy::new(judge);

        let result = policy.compact(&rope, 1000);
        let had_failure = result
            .decisions
            .iter()
            .any(|d| matches!(d, super::super::CompactionDecision::Failed { .. }));
        assert!(
            !had_failure,
            "policy should NOT fail-close with valid live-judge response; got {:?}",
            result.decisions,
        );
    }
}
