//! Anthropic API Provider — direct API access via `ANTHROPIC_API_KEY`.
//!
//! Streams responses from `/v1/messages` with `stream: true`, parses
//! the SSE event format, translates to mu-core's `ProviderEvent`.
//!
//! See spec mu-006. v1 supports text-only responses; tools, extended
//! thinking, and image content are deferred.

use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{BoxStream, Stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use mu_core::agent::{
    AgentMessage, AssistantMessage, ContentBlock, Provider, ProviderError, ProviderEvent,
    StopReason, ToolSpec,
};

use super::sse::{SseEvent, SseStream};

const ANTHROPIC_API_BASE: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Direct API Provider. Holds an API key (ENV-sourced is fine — this
/// isn't an OAuth token).
pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    api_base: String,
}

impl AnthropicProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model,
            api_base: ANTHROPIC_API_BASE.to_string(),
        }
    }

    /// API key from `ANTHROPIC_API_KEY`. Fails if unset or empty.
    pub fn from_env(model: String) -> Result<Self, ProviderError> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ProviderError::Other("ANTHROPIC_API_KEY not set or empty".into()))?;
        Ok(Self::new(api_key, model))
    }

    /// Test hook: override the API base URL for mock servers.
    pub fn with_api_base(mut self, base: String) -> Self {
        self.api_base = base;
        self
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn stream(
        &self,
        messages: &[AgentMessage],
        tools: &[ToolSpec],
        cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let body = build_request_body(&self.model, messages, tools);

        let resp = self
            .client
            .post(format!("{}/v1/messages", self.api_base))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Other(format!("anthropic request: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Other(format!(
                "anthropic returned {status}: {text}"
            )));
        }

        let bytes = resp.bytes_stream();
        Ok(events_stream(bytes, cancel_rx))
    }
}

/// Translate a mu `ToolSpec` into Anthropic's tool descriptor shape.
pub(crate) fn translate_tool_spec(spec: &ToolSpec) -> Value {
    json!({
        "name": spec.name,
        "description": spec.description,
        "input_schema": spec.input_schema,
    })
}

/// Translate messages into Anthropic's API message shape.
///
/// Consecutive tool results are batched into a single user message
/// containing multiple `tool_result` content blocks, as required by
/// Anthropic's tool-use protocol.
pub(crate) fn translate_messages(messages: &[AgentMessage]) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len());
    let mut tool_result_buf = Vec::new();

    for message in messages {
        match message {
            AgentMessage::ToolResult {
                call_id,
                content,
                is_error,
            } => {
                tool_result_buf.push(json!({
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": content,
                    "is_error": is_error,
                }));
            }
            other => {
                if !tool_result_buf.is_empty() {
                    out.push(json!({
                        "role": "user",
                        "content": std::mem::take(&mut tool_result_buf),
                    }));
                }
                if let Some(translated) = translate_message_single(other) {
                    out.push(translated);
                }
            }
        }
    }

    if !tool_result_buf.is_empty() {
        out.push(json!({
            "role": "user",
            "content": tool_result_buf,
        }));
    }

    out
}

/// Single-message translation for non-ToolResult variants. ToolResult
/// is handled by `translate_messages` because Anthropic requires
/// consecutive tool results to be grouped in one user message.
fn translate_message_single(m: &AgentMessage) -> Option<Value> {
    match m {
        AgentMessage::User { content } => Some(json!({
            "role": "user",
            "content": content,
        })),
        AgentMessage::Assistant(a) => {
            let blocks: Vec<Value> = a
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(json!({
                        "type": "text",
                        "text": text,
                    })),
                    ContentBlock::ToolCall(tool_call) => Some(json!({
                        "type": "tool_use",
                        "id": tool_call.id,
                        "name": tool_call.name,
                        "input": tool_call.arguments,
                    })),
                    ContentBlock::Thinking { .. } => None,
                })
                .collect();
            if blocks.is_empty() {
                None
            } else {
                Some(json!({
                    "role": "assistant",
                    "content": blocks,
                }))
            }
        }
        AgentMessage::ToolResult { .. } => None,
    }
}

pub(crate) fn build_request_body(
    model: &str,
    messages: &[AgentMessage],
    tools: &[ToolSpec],
) -> Value {
    let api_messages = translate_messages(messages);
    let mut body = json!({
        "model": model,
        "max_tokens": 4096,
        "stream": true,
        "messages": api_messages,
    });
    if !tools.is_empty() {
        body["tools"] = json!(tools.iter().map(translate_tool_spec).collect::<Vec<_>>());
    }
    body
}

// ============================================================================
// Anthropic SSE event types — minimal subset we care about
// ============================================================================

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
#[allow(dead_code)] // Some fields are present for future use; keep deserializer faithful to API.
enum AnthropicEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: AnthropicMessageMeta },
    #[serde(rename = "content_block_start")]
    ContentBlockStart { index: u32, content_block: AnthropicBlock },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: u32, delta: AnthropicDelta },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: u32 },
    #[serde(rename = "message_delta")]
    MessageDelta { delta: AnthropicMessageDelta },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "error")]
    Error { error: AnthropicErrorBody },
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct AnthropicMessageMeta {
    id: Option<String>,
    role: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
#[allow(dead_code)]
enum AnthropicBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        // ignored in v1
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AnthropicDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { /* tool args */ },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct AnthropicMessageDelta {
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicErrorBody {
    #[serde(rename = "type")]
    err_type: Option<String>,
    message: Option<String>,
}

fn map_stop_reason(s: Option<&str>) -> StopReason {
    match s {
        Some("end_turn") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("stop_sequence") => StopReason::EndTurn,
        Some(other) => {
            tracing::warn!(stop_reason = %other, "unrecognized anthropic stop_reason");
            StopReason::EndTurn
        }
        None => StopReason::EndTurn,
    }
}

// ============================================================================
// SSE → ProviderEvent translation
// ============================================================================

/// Build a stream of `ProviderEvent`s from a stream of bytes (the raw
/// HTTP body). Owns `cancel_rx`; when it fires, the stream terminates.
fn events_stream(
    bytes: impl Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
    cancel_rx: oneshot::Receiver<()>,
) -> BoxStream<'static, ProviderEvent> {
    // Box::pin the input to satisfy SseStream's `S: Unpin` bound;
    // reqwest::Response::bytes_stream() returns a !Unpin type.
    let bytes: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>> = Box::pin(bytes);
    let sse = SseStream::new(bytes);
    let state = StreamState {
        sse: Box::pin(sse),
        accumulated_text: String::new(),
        stop_reason: None,
        cancel_rx: Some(cancel_rx),
        finished: false,
        emitted_done: false,
    };
    Box::pin(futures::stream::unfold(state, next_event))
}

struct StreamState {
    sse: Pin<Box<dyn Stream<Item = SseEvent> + Send>>,
    accumulated_text: String,
    stop_reason: Option<String>,
    cancel_rx: Option<oneshot::Receiver<()>>,
    finished: bool,
    emitted_done: bool,
}

async fn next_event(mut state: StreamState) -> Option<(ProviderEvent, StreamState)> {
    if state.finished {
        return None;
    }

    loop {
        // Cancel?
        if let Some(rx) = state.cancel_rx.as_mut() {
            // Try to check cancel without blocking.
            match rx.try_recv() {
                Ok(_) => {
                    state.finished = true;
                    state.cancel_rx = None;
                    return Some((
                        ProviderEvent::Done(AssistantMessage {
                            content: assistant_content(&state.accumulated_text),
                            stop_reason: StopReason::Aborted,
                        }),
                        state,
                    ));
                }
                Err(oneshot::error::TryRecvError::Empty) => {} // continue
                Err(oneshot::error::TryRecvError::Closed) => {
                    state.cancel_rx = None;
                }
            }
        }

        // Pull next SSE event.
        let sse_event = match state.sse.next().await {
            Some(e) => e,
            None => {
                // Stream ended without message_stop. Emit Done if we
                // haven't yet.
                state.finished = true;
                if !state.emitted_done {
                    state.emitted_done = true;
                    let stop = map_stop_reason(state.stop_reason.as_deref());
                    return Some((
                        ProviderEvent::Done(AssistantMessage {
                            content: assistant_content(&state.accumulated_text),
                            stop_reason: stop,
                        }),
                        state,
                    ));
                }
                return None;
            }
        };

        // Parse the JSON payload as an Anthropic event.
        let parsed: Result<AnthropicEvent, _> = serde_json::from_str(&sse_event.data);
        let parsed = match parsed {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, data = %sse_event.data, "failed to parse anthropic event");
                continue;
            }
        };

        match parsed {
            AnthropicEvent::ContentBlockDelta {
                delta: AnthropicDelta::TextDelta { text },
                ..
            } => {
                state.accumulated_text.push_str(&text);
                return Some((ProviderEvent::TextDelta(text), state));
            }
            AnthropicEvent::ContentBlockStart { .. } => {
                // No-op for v1.
            }
            AnthropicEvent::ContentBlockStop { .. } => {
                // No-op.
            }
            AnthropicEvent::MessageDelta { delta } => {
                state.stop_reason = delta.stop_reason;
            }
            AnthropicEvent::MessageStop => {
                state.finished = true;
                state.emitted_done = true;
                let stop = map_stop_reason(state.stop_reason.as_deref());
                return Some((
                    ProviderEvent::Done(AssistantMessage {
                        content: assistant_content(&state.accumulated_text),
                        stop_reason: stop,
                    }),
                    state,
                ));
            }
            AnthropicEvent::Error { error } => {
                state.finished = true;
                state.emitted_done = true;
                let msg = format!(
                    "anthropic stream error ({}): {}",
                    error.err_type.unwrap_or_else(|| "unknown".to_string()),
                    error.message.unwrap_or_else(|| "(no message)".to_string()),
                );
                return Some((ProviderEvent::Error(msg), state));
            }
            AnthropicEvent::Ping | AnthropicEvent::MessageStart { .. } => {
                // No-op for v1. (MessageStart carries metadata we don't use yet.)
            }
            AnthropicEvent::ContentBlockDelta { .. } => {
                // Non-text delta types (input_json_delta for tool args). Future.
            }
        }
    }
}

fn assistant_content(text: &str) -> Vec<ContentBlock> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![ContentBlock::Text {
            text: text.to_string(),
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_core::agent::ToolCall;

    #[test]
    fn b1_translate_user_message() {
        let m = AgentMessage::User {
            content: "hi".into(),
        };
        let v = translate_message_single(&m).expect("translates");
        assert_eq!(v["role"], "user");
        assert_eq!(v["content"], "hi");
    }

    #[test]
    fn b2_translate_assistant_message() {
        let m = AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::Text { text: "hi".into() }],
            stop_reason: StopReason::EndTurn,
        });
        let v = translate_message_single(&m).expect("translates");
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "hi");
    }

    #[test]
    fn translate_message_single_skips_tool_result() {
        let m = AgentMessage::ToolResult {
            call_id: "x".into(),
            content: "out".into(),
            is_error: false,
        };
        assert!(translate_message_single(&m).is_none());
    }

    #[test]
    fn build_request_body_basics() {
        let messages = vec![AgentMessage::User {
            content: "hi".into(),
        }];
        let body = build_request_body("claude-test", &messages, &[]);
        assert_eq!(body["model"], "claude-test");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn b1_translate_tool_spec_shape() {
        let spec = ToolSpec {
            name: "read".into(),
            description: "Read a file".into(),
            input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        };
        assert_eq!(translate_tool_spec(&spec), json!({
            "name":"read",
            "description":"Read a file",
            "input_schema":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}
        }));
    }

    #[test]
    fn b2_translate_messages_preserves_order() {
        let messages = vec![
            AgentMessage::User { content: "first".into() },
            assistant_text("second"),
            AgentMessage::User { content: "third".into() },
            assistant_text("fourth"),
        ];
        let translated = translate_messages(&messages);
        assert_eq!(translated.len(), 4);
        assert_eq!(translated[0]["role"], "user");
        assert_eq!(translated[0]["content"], "first");
        assert_eq!(translated[1]["role"], "assistant");
        assert_eq!(translated[1]["content"][0]["text"], "second");
        assert_eq!(translated[2]["role"], "user");
        assert_eq!(translated[2]["content"], "third");
        assert_eq!(translated[3]["role"], "assistant");
        assert_eq!(translated[3]["content"][0]["text"], "fourth");
    }

    #[test]
    fn b3_consecutive_tool_results_group_into_one_user_message() {
        let messages = vec![
            AgentMessage::User { content: "read both".into() },
            AgentMessage::Assistant(AssistantMessage {
                content: vec![tool_call("toolu_a", "a.txt"), tool_call("toolu_b", "b.txt")],
                stop_reason: StopReason::ToolUse,
            }),
            AgentMessage::ToolResult { call_id: "toolu_a".into(), content: "a contents".into(), is_error: false },
            AgentMessage::ToolResult { call_id: "toolu_b".into(), content: "b failed".into(), is_error: true },
            assistant_text("done"),
        ];

        let translated = translate_messages(&messages);
        assert_eq!(translated.len(), 4);
        assert_eq!(translated[0]["role"], "user");
        assert_eq!(translated[1]["role"], "assistant");
        assert_eq!(translated[1]["content"].as_array().map(Vec::len), Some(2));
        assert_eq!(translated[1]["content"][0]["type"], "tool_use");
        assert_eq!(translated[1]["content"][0]["id"], "toolu_a");
        assert_eq!(translated[1]["content"][0]["input"], json!({ "path": "a.txt" }));
        assert_eq!(translated[1]["content"][1]["type"], "tool_use");
        assert_eq!(translated[1]["content"][1]["id"], "toolu_b");
        assert_eq!(translated[2]["role"], "user");
        let tool_results = translated[2]["content"].as_array();
        assert_eq!(tool_results.map(Vec::len), Some(2));
        assert_eq!(translated[2]["content"][0]["type"], "tool_result");
        assert_eq!(translated[2]["content"][0]["tool_use_id"], "toolu_a");
        assert_eq!(translated[2]["content"][0]["content"], "a contents");
        assert_eq!(translated[2]["content"][0]["is_error"], false);
        assert_eq!(translated[2]["content"][1]["type"], "tool_result");
        assert_eq!(translated[2]["content"][1]["tool_use_id"], "toolu_b");
        assert_eq!(translated[2]["content"][1]["content"], "b failed");
        assert_eq!(translated[2]["content"][1]["is_error"], true);
        assert_eq!(translated[3]["role"], "assistant");
    }

    #[test]
    fn b4_build_request_body_includes_tools_when_present() {
        let messages = vec![AgentMessage::User { content: "hi".into() }];
        let tools = vec![ToolSpec {
            name: "read".into(),
            description: "Read a file".into(),
            input_schema: json!({ "type": "object" }),
        }];
        let body = build_request_body("claude-test", &messages, &tools);
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["tools"][0]["name"], "read");
    }

    #[test]
    fn b5_build_request_body_omits_tools_when_empty() {
        let messages = vec![AgentMessage::User { content: "hi".into() }];
        let body = build_request_body("claude-test", &messages, &[]);
        assert!(body.get("tools").is_none());
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(1));
    }

    fn assistant_text(text: &str) -> AgentMessage {
        AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            stop_reason: StopReason::EndTurn,
        })
    }

    fn tool_call(id: &str, path: &str) -> ContentBlock {
        ContentBlock::ToolCall(ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: json!({ "path": path }),
        })
    }

    #[tokio::test]
    async fn b4_sse_to_provider_events() {
        // Build a fake SSE byte stream that mimics Anthropic's shape.
        let raw = concat!(
            r#"event: message_start"#, "\n",
            r#"data: {"type":"message_start","message":{"id":"m_1","role":"assistant"}}"#, "\n\n",
            r#"event: content_block_start"#, "\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#, "\n\n",
            r#"event: content_block_delta"#, "\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#, "\n\n",
            r#"event: content_block_delta"#, "\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}"#, "\n\n",
            r#"event: content_block_stop"#, "\n",
            r#"data: {"type":"content_block_stop","index":0}"#, "\n\n",
            r#"event: message_delta"#, "\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#, "\n\n",
            r#"event: message_stop"#, "\n",
            r#"data: {"type":"message_stop"}"#, "\n\n",
        );
        let bytes = futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::copy_from_slice(
            raw.as_bytes(),
        ))]);
        // events_stream takes Stream<Item = reqwest::Result<Bytes>>;
        // we adapt by mapping our io::Error to reqwest's. Since we
        // don't have access to a reqwest::Error constructor, build a
        // separate adapter for tests.
        let bytes = bytes.map(|r| r.map_err(|_| panic!("test stream errored")));
        // Wrap so the stream type matches what events_stream expects
        // (reqwest::Result<Bytes>). The simplest path: change
        // events_stream to be generic over any Stream<Item =
        // Result<Bytes, _>>, so tests can use io::Error. Refactor
        // below in test_events_stream.
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let mut stream = test_events_stream(bytes, rx);

        let mut events = Vec::new();
        while let Some(e) = stream.next().await {
            events.push(e);
        }

        // Expected: TextDelta("hello"), TextDelta(" world"),
        // Done(AssistantMessage { content: [Text("hello world")], EndTurn })
        assert_eq!(events.len(), 3);
        match &events[0] {
            ProviderEvent::TextDelta(t) => assert_eq!(t, "hello"),
            other => panic!("expected TextDelta, got {other:?}"),
        }
        match &events[1] {
            ProviderEvent::TextDelta(t) => assert_eq!(t, " world"),
            other => panic!("expected TextDelta, got {other:?}"),
        }
        match &events[2] {
            ProviderEvent::Done(msg) => {
                assert_eq!(msg.stop_reason, StopReason::EndTurn);
                match &msg.content[0] {
                    ContentBlock::Text { text } => assert_eq!(text, "hello world"),
                    other => panic!("expected Text block, got {other:?}"),
                }
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    /// Test-only variant of events_stream that accepts a stream with
    /// any Result error type, not specifically reqwest::Result.
    fn test_events_stream(
        bytes: impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
        cancel_rx: oneshot::Receiver<()>,
    ) -> BoxStream<'static, ProviderEvent> {
        let bytes: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> =
            Box::pin(bytes);
        let sse = SseStream::new(bytes);
        let state = StreamState {
            sse: Box::pin(sse),
            accumulated_text: String::new(),
            stop_reason: None,
            cancel_rx: Some(cancel_rx),
            finished: false,
            emitted_done: false,
        };
        Box::pin(futures::stream::unfold(state, next_event))
    }

    #[tokio::test]
    async fn anthropic_error_event_terminates_with_provider_error() {
        let raw = concat!(
            r#"event: error"#, "\n",
            r#"data: {"type":"error","error":{"type":"rate_limit_error","message":"too many"}}"#, "\n\n",
        );
        let bytes = futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::copy_from_slice(
            raw.as_bytes(),
        ))]);
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let mut stream = test_events_stream(bytes, rx);
        let event = stream.next().await.expect("expected error event");
        match event {
            ProviderEvent::Error(msg) => {
                assert!(msg.contains("rate_limit_error"));
                assert!(msg.contains("too many"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
        // No more events.
        assert!(stream.next().await.is_none());
    }

    #[test]
    fn map_stop_reason_known_and_unknown() {
        assert_eq!(map_stop_reason(Some("end_turn")), StopReason::EndTurn);
        assert_eq!(map_stop_reason(Some("tool_use")), StopReason::ToolUse);
        assert_eq!(map_stop_reason(Some("max_tokens")), StopReason::MaxTokens);
        assert_eq!(map_stop_reason(Some("weird")), StopReason::EndTurn);
        assert_eq!(map_stop_reason(None), StopReason::EndTurn);
    }
}

// ============================================================================
// Live integration test (gated on MU_LIVE_ANTHROPIC env var)
// ============================================================================

#[cfg(test)]
mod live_tests {
    use super::*;
    use mu_core::agent::AgentMessage;

    fn live_enabled() -> bool {
        std::env::var("MU_LIVE_ANTHROPIC")
            .ok()
            .as_deref()
            .map(|v| v == "1")
            .unwrap_or(false)
    }

    /// B-7 (live API smoke). Only runs when MU_LIVE_ANTHROPIC=1.
    #[tokio::test]
    async fn b7_live_anthropic_smoke() {
        if !live_enabled() {
            eprintln!("skipping b7_live_anthropic_smoke (set MU_LIVE_ANTHROPIC=1 to run)");
            return;
        }

        let provider = AnthropicProvider::from_env("claude-haiku-4-5-20251001".into())
            .expect("ANTHROPIC_API_KEY must be set when MU_LIVE_ANTHROPIC=1");

        let messages = vec![AgentMessage::User {
            content: "Reply with the single word 'hello' and nothing else.".into(),
        }];
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let mut stream = provider
            .stream(&messages, &[], rx)
            .await
            .expect("provider.stream");

        let mut text = String::new();
        let mut done_payload: Option<AssistantMessage> = None;
        while let Some(event) = stream.next().await {
            match event {
                ProviderEvent::TextDelta(d) => text.push_str(&d),
                ProviderEvent::Done(msg) => {
                    done_payload = Some(msg);
                    break;
                }
                ProviderEvent::Error(e) => panic!("anthropic error: {e}"),
                _ => {}
            }
        }

        let done = done_payload.expect("expected Done");
        let final_text = match &done.content[..] {
            [ContentBlock::Text { text }] => text.clone(),
            other => panic!("unexpected content blocks: {other:?}"),
        };
        eprintln!("live anthropic smoke text: {final_text:?}");
        assert!(
            final_text.to_lowercase().contains("hello"),
            "expected response to contain 'hello', got: {final_text:?}"
        );
        // Sanity: streamed text matches accumulated text.
        assert_eq!(text, final_text);
    }
}
