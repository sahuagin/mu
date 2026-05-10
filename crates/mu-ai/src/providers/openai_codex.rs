//! OpenAI-via-Codex provider — subprocess wrapper around `pi
//! --provider openai-codex` to use the user's OpenAI Pro OAuth
//! without holding tokens in mu's address space (per AGENTS.md).
//!
//! v1 limitations (intentional):
//! - No streaming. pi's `-p` mode is one-shot text.
//! - No tool support. The subprocess accepts a flat prompt; tool
//!   calls don't round-trip.
//! - Cancel doesn't actively kill the subprocess; the `wait` future
//!   completes naturally and the result is dropped. See §OOC-1.
//!
//! See spec mu-015.

use std::pin::Pin;
use std::process::Stdio;

use async_trait::async_trait;
use futures::stream::{BoxStream, Stream};
use tokio::sync::oneshot;

use mu_core::agent::{
    AgentMessage, AssistantMessage, ContentBlock, Provider, ProviderError, ProviderEvent,
    StopReason, ToolSpec,
};

/// OAuth-authenticated OpenAI access via the `pi` CLI subprocess.
pub struct OpenaiCodexProvider {
    model: String,
    thinking: String,
}

impl OpenaiCodexProvider {
    /// Defaults `thinking` to "medium" — the agent-router routing
    /// memory's documented default for codex-oauth.
    pub fn new(model: String) -> Self {
        Self {
            model,
            thinking: "medium".to_string(),
        }
    }

    pub fn with_thinking(mut self, thinking: String) -> Self {
        self.thinking = thinking;
        self
    }
}

#[async_trait]
impl Provider for OpenaiCodexProvider {
    async fn stream(
        &self,
        messages: &[AgentMessage],
        _tools: &[ToolSpec],
        cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let prompt = flatten_messages(messages);
        let pi_binary = locate_pi()?;
        let response = spawn_and_wait(
            &pi_binary,
            &self.model,
            &self.thinking,
            &prompt,
            cancel_rx,
        )
        .await?;

        // v1: emit one TextDelta + Done. No streaming.
        let text = response.clone();
        let done = AssistantMessage {
            content: vec![ContentBlock::Text { text }],
            stop_reason: StopReason::EndTurn,
        };
        let events = vec![
            ProviderEvent::TextDelta(response),
            ProviderEvent::Done(done),
        ];
        let stream: Pin<Box<dyn Stream<Item = ProviderEvent> + Send>> =
            Box::pin(futures::stream::iter(events));
        Ok(stream)
    }
}

/// Convert `&[AgentMessage]` into a single text prompt for pi's
/// `-p` mode. Lossy: assistant content blocks of types other than
/// Text are dropped; tool calls are flattened into "[tool result]"
/// pseudo-content (since pi has no tool surface in this path).
pub(crate) fn flatten_messages(messages: &[AgentMessage]) -> String {
    let mut out = String::new();
    for m in messages {
        match m {
            AgentMessage::User { content } => {
                out.push_str(&format!("User: {content}\n\n"));
            }
            AgentMessage::Assistant(a) => {
                let text: String = a
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if !text.is_empty() {
                    out.push_str(&format!("Assistant: {text}\n\n"));
                }
            }
            AgentMessage::ToolResult { content, .. } => {
                out.push_str(&format!("[tool result] {content}\n\n"));
            }
        }
    }
    out
}

/// Locate the `pi` binary. Order: `MU_PI_BINARY` env var (test
/// override), then PATH lookup.
pub(crate) fn locate_pi() -> Result<String, ProviderError> {
    if let Ok(p) = std::env::var("MU_PI_BINARY") {
        if !p.is_empty() {
            return Ok(p);
        }
    }
    let out = std::process::Command::new("which")
        .arg("pi")
        .output()
        .map_err(|e| ProviderError::Other(format!("which pi failed: {e}")))?;
    if !out.status.success() {
        return Err(ProviderError::Other(
            "pi binary not found in PATH (set MU_PI_BINARY to override)".into(),
        ));
    }
    let path = String::from_utf8(out.stdout)
        .map_err(|_| ProviderError::Other("pi path not utf-8".into()))?
        .trim()
        .to_string();
    if path.is_empty() {
        return Err(ProviderError::Other("pi binary not found in PATH".into()));
    }
    Ok(path)
}

async fn spawn_and_wait(
    pi: &str,
    model: &str,
    thinking: &str,
    prompt: &str,
    cancel_rx: oneshot::Receiver<()>,
) -> Result<String, ProviderError> {
    let mut cmd = tokio::process::Command::new(pi);
    cmd.arg("--provider")
        .arg("openai-codex")
        .arg("--model")
        .arg(model)
        .arg("-p")
        .arg(prompt)
        .arg("--thinking")
        .arg(thinking)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = cmd
        .spawn()
        .map_err(|e| ProviderError::Other(format!("spawn pi: {e}")))?;

    let wait_fut = child.wait_with_output();

    tokio::select! {
        out = wait_fut => {
            let out = out.map_err(|e| ProviderError::Other(format!("wait pi: {e}")))?;
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                return Err(ProviderError::Other(
                    format!("pi exited {}: {stderr}", out.status),
                ));
            }
            String::from_utf8(out.stdout)
                .map_err(|_| ProviderError::Other("pi stdout not utf-8".into()))
        }
        _ = cancel_rx => {
            // v1: best-effort cancel. We've moved the child into
            // wait_with_output so we can't `start_kill()` it from
            // here. The subprocess will finish naturally; we just
            // bail with an error. Future spec can split the wait
            // and keep a `Child` reference for active termination.
            Err(ProviderError::Other("cancelled".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b1_flatten_user_message() {
        let msgs = vec![AgentMessage::User {
            content: "hi".into(),
        }];
        assert_eq!(flatten_messages(&msgs), "User: hi\n\n");
    }

    #[test]
    fn b2_flatten_multi_turn() {
        let msgs = vec![
            AgentMessage::User {
                content: "hello".into(),
            },
            AgentMessage::Assistant(AssistantMessage {
                content: vec![ContentBlock::Text {
                    text: "hi back".into(),
                }],
                stop_reason: StopReason::EndTurn,
            }),
            AgentMessage::User {
                content: "more".into(),
            },
        ];
        let flat = flatten_messages(&msgs);
        assert_eq!(
            flat,
            "User: hello\n\nAssistant: hi back\n\nUser: more\n\n"
        );
    }

    #[test]
    fn b3_flatten_tool_result() {
        let msgs = vec![AgentMessage::ToolResult {
            call_id: "c1".into(),
            content: "foo".into(),
            is_error: false,
        }];
        assert_eq!(flatten_messages(&msgs), "[tool result] foo\n\n");
    }

    #[test]
    fn b4_locate_pi_env_var_override() {
        // Save original, set ours, check, restore.
        let original = std::env::var("MU_PI_BINARY").ok();
        // Use a sentinel path so we know the override took effect even
        // when pi exists in PATH.
        std::env::set_var("MU_PI_BINARY", "/sentinel/pi/path");
        let result = locate_pi();
        // Restore.
        match original {
            Some(v) => std::env::set_var("MU_PI_BINARY", v),
            None => std::env::remove_var("MU_PI_BINARY"),
        }
        assert_eq!(result.unwrap(), "/sentinel/pi/path");
    }

    #[test]
    fn b6_locate_pi_failure_message() {
        // Force lookup to fail by using a definitely-bogus PATH AND no
        // env override. Save original env and PATH; restore at end.
        let original_mu_pi = std::env::var("MU_PI_BINARY").ok();
        let original_path = std::env::var("PATH").ok();

        std::env::remove_var("MU_PI_BINARY");
        std::env::set_var("PATH", "/nonexistent/path/only");

        let result = locate_pi();

        // Restore.
        match original_mu_pi {
            Some(v) => std::env::set_var("MU_PI_BINARY", v),
            None => std::env::remove_var("MU_PI_BINARY"),
        }
        match original_path {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }

        match result {
            Ok(_) => panic!("expected error when pi not in PATH"),
            Err(ProviderError::Other(msg)) => {
                assert!(
                    msg.contains("pi binary not found") || msg.contains("which pi failed"),
                    "unexpected error message: {msg}"
                );
            }
            Err(other) => panic!("expected ProviderError::Other, got: {other:?}"),
        }
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use futures::StreamExt;

    fn live_enabled() -> bool {
        std::env::var("MU_LIVE_OPENAI_CODEX").ok().as_deref() == Some("1")
    }

    /// B-7 (live OpenAI Codex via pi). Gated on MU_LIVE_OPENAI_CODEX=1.
    #[tokio::test]
    async fn b7_live_openai_codex_smoke() {
        if !live_enabled() {
            eprintln!("skipping b7_live_openai_codex_smoke (set MU_LIVE_OPENAI_CODEX=1 to run)");
            return;
        }

        let provider = OpenaiCodexProvider::new("gpt-5.5".to_string());
        let messages = vec![AgentMessage::User {
            content: "Reply with the single word 'hello' and nothing else.".into(),
        }];
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let mut stream = provider
            .stream(&messages, &[], rx)
            .await
            .expect("provider.stream");

        let mut text = String::new();
        let mut got_done = false;
        while let Some(event) = stream.next().await {
            match event {
                ProviderEvent::TextDelta(d) => text.push_str(&d),
                ProviderEvent::Done(_) => {
                    got_done = true;
                    break;
                }
                ProviderEvent::Error(e) => panic!("openai-codex error: {e}"),
                _ => {}
            }
        }
        assert!(got_done, "expected Done event");
        eprintln!("live openai-codex smoke text: {text:?}");
        assert!(
            text.to_lowercase().contains("hello"),
            "expected response to contain 'hello', got: {text:?}"
        );
    }
}
