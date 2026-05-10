//! Tests for the queue-driven agent loop. MockProvider and MockTool
//! let tests script LLM and tool behavior precisely without spawning
//! real LLM calls or running real tools.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use super::*;
use crate::agent::provider::{Provider, ProviderError, ProviderEvent};
use crate::agent::tool::{Tool, ToolResult, ToolSpec};
use crate::agent::types::{
    AgentMessage, AssistantMessage, ContentBlock, StopReason, ToolCall,
};

// ============================================================================
// MockProvider
// ============================================================================

enum MockResponse {
    Events(Vec<ProviderEvent>),
    /// Stream that never produces events. Used to simulate a long-
    /// running provider call for cancel and queue-ordering tests.
    Pending,
}

struct MockProvider {
    responses: Mutex<VecDeque<MockResponse>>,
}

impl MockProvider {
    fn new(responses: Vec<Vec<ProviderEvent>>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().map(MockResponse::Events).collect()),
        }
    }

    fn pending() -> Self {
        let mut q = VecDeque::new();
        q.push_back(MockResponse::Pending);
        Self {
            responses: Mutex::new(q),
        }
    }

    /// MockProvider that returns the same response repeatedly. Used
    /// for the iteration-cap test where we don't know how many times
    /// the loop will call us.
    fn forever(events: Vec<ProviderEvent>) -> Self {
        let mut q = VecDeque::new();
        for _ in 0..100 {
            q.push_back(MockResponse::Events(events.clone()));
        }
        Self {
            responses: Mutex::new(q),
        }
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn stream(
        &self,
        _messages: &[AgentMessage],
        _tools: &[ToolSpec],
        _cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let chunk = self.responses.lock().expect("mutex poisoned").pop_front();
        match chunk {
            Some(MockResponse::Events(events)) => Ok(Box::pin(stream::iter(events))),
            Some(MockResponse::Pending) => Ok(Box::pin(stream::pending())),
            None => Ok(Box::pin(stream::iter(Vec::<ProviderEvent>::new()))),
        }
    }
}

// ============================================================================
// MockTool
// ============================================================================

struct MockTool {
    name: String,
    /// FIFO queue of (delay, result) pairs. Each execute() call pops one.
    /// If the queue is empty, returns a default error.
    responses: Mutex<VecDeque<(Duration, ToolResult)>>,
}

impl MockTool {
    fn ok(name: &str, content: &str) -> Self {
        let mut q = VecDeque::new();
        q.push_back((
            Duration::from_millis(0),
            ToolResult {
                content: content.to_owned(),
                is_error: false,
            },
        ));
        Self {
            name: name.to_owned(),
            responses: Mutex::new(q),
        }
    }

    fn err(name: &str, content: &str) -> Self {
        let mut q = VecDeque::new();
        q.push_back((
            Duration::from_millis(0),
            ToolResult {
                content: content.to_owned(),
                is_error: true,
            },
        ));
        Self {
            name: name.to_owned(),
            responses: Mutex::new(q),
        }
    }

    fn always_ok(name: &str, content: &str) -> Self {
        let mut q = VecDeque::new();
        for _ in 0..100 {
            q.push_back((
                Duration::from_millis(0),
                ToolResult {
                    content: content.to_owned(),
                    is_error: false,
                },
            ));
        }
        Self {
            name: name.to_owned(),
            responses: Mutex::new(q),
        }
    }

    fn delayed(name: &str, content: &str, delay: Duration) -> Self {
        let mut q = VecDeque::new();
        q.push_back((
            delay,
            ToolResult {
                content: content.to_owned(),
                is_error: false,
            },
        ));
        Self {
            name: name.to_owned(),
            responses: Mutex::new(q),
        }
    }
}

#[async_trait]
impl Tool for MockTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: format!("Mock tool: {}", self.name),
            input_schema: json!({"type": "object"}),
        }
    }

    async fn execute(&self, _arguments: Value, _cancel_rx: oneshot::Receiver<()>) -> ToolResult {
        let entry = self.responses.lock().expect("mutex poisoned").pop_front();
        let (delay, result) = entry.unwrap_or_else(|| {
            (
                Duration::from_millis(0),
                ToolResult {
                    content: "no response queued".to_owned(),
                    is_error: true,
                },
            )
        });
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        result
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn user_msg(content: &str) -> AgentMessage {
    AgentMessage::User {
        content: content.to_owned(),
    }
}

fn assistant_text(text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
        stop_reason: StopReason::EndTurn,
    }
}

fn assistant_tool_call(id: &str, name: &str, args: Value) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: args,
        })],
        stop_reason: StopReason::ToolUse,
    }
}

fn spawn_loop(
    provider: MockProvider,
    tools: Vec<MockTool>,
    config: AgentConfig,
) -> (AgentLoop, mpsc::Receiver<AgentEvent>) {
    let (events_tx, events_rx) = mpsc::channel(64);
    let provider: Arc<dyn Provider> = Arc::new(provider);
    let tools: Vec<Arc<dyn Tool>> = tools
        .into_iter()
        .map(|t| Arc::new(t) as Arc<dyn Tool>)
        .collect();
    let loop_ = AgentLoop::spawn(provider, tools, config, events_tx);
    (loop_, events_rx)
}

async fn collect_events(mut rx: mpsc::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(e) = rx.recv().await {
        events.push(e);
    }
    events
}

/// Match an event against a "kind" pattern. Used to assert ordering
/// without inspecting full payloads.
fn kind(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::AgentStart => "agent_start",
        AgentEvent::TurnStart => "turn_start",
        AgentEvent::MessageStart { .. } => "message_start",
        AgentEvent::TextDelta { .. } => "text_delta",
        AgentEvent::ToolCallStarted { .. } => "tool_call_started",
        AgentEvent::ToolCallCompleted { .. } => "tool_call_completed",
        AgentEvent::MessageEnd { .. } => "message_end",
        AgentEvent::TurnEnd => "turn_end",
        AgentEvent::Done { .. } => "done",
        AgentEvent::Error { .. } => "error",
        AgentEvent::Callout { .. } => "callout",
    }
}

// ============================================================================
// Behavior tests
// ============================================================================

/// B-1: single-turn no-tools.
#[tokio::test]
async fn b1_single_turn_no_tools() {
    let provider = MockProvider::new(vec![vec![
        ProviderEvent::TextDelta("hi".into()),
        ProviderEvent::Done(assistant_text("hi")),
    ]]);
    let (loop_, events_rx) = spawn_loop(provider, vec![], AgentConfig::default());

    loop_
        .send(AgentInput::UserMessage(user_msg("hello")))
        .await
        .expect("send");
    let events_handle = tokio::spawn(collect_events(events_rx));
    let outcome = loop_.join().await;
    let events = events_handle.await.expect("events drain");

    assert_eq!(outcome, Outcome::Done(StopReason::EndTurn));

    let kinds: Vec<&str> = events.iter().map(kind).collect();
    assert_eq!(
        kinds,
        vec![
            "agent_start",
            "message_start",
            "message_end",
            "turn_start",
            "text_delta",
            "message_start",
            "message_end",
            "turn_end",
            "done",
        ],
        "unexpected event sequence"
    );

    if let AgentEvent::Done { turn_count, .. } = events.last().unwrap() {
        assert_eq!(*turn_count, 1);
    } else {
        panic!("last event not Done");
    }
}

/// B-6 (run before B-2 so we trust the error path before stacking tools).
#[tokio::test]
async fn b6_provider_error_terminates() {
    let provider = MockProvider::new(vec![vec![ProviderEvent::Error("rate limit".into())]]);
    let (loop_, events_rx) = spawn_loop(provider, vec![], AgentConfig::default());

    loop_
        .send(AgentInput::UserMessage(user_msg("hello")))
        .await
        .expect("send");
    let events_handle = tokio::spawn(collect_events(events_rx));
    let outcome = loop_.join().await;
    let events = events_handle.await.expect("events drain");

    assert_eq!(outcome, Outcome::Error("rate limit".into()));

    // Should see an error event before termination.
    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::Error { message } if message == "rate limit")),
        "missing error event in {:?}",
        events.iter().map(kind).collect::<Vec<_>>()
    );
    // Should NOT see Done.
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Done { .. })));
}

/// B-2: single tool call followed by a text response.
#[tokio::test]
async fn b2_single_tool_call() {
    let provider = MockProvider::new(vec![
        vec![ProviderEvent::Done(assistant_tool_call(
            "t1",
            "echo",
            json!({"x": 1}),
        ))],
        vec![
            ProviderEvent::TextDelta("done".into()),
            ProviderEvent::Done(assistant_text("done")),
        ],
    ]);
    let tools = vec![MockTool::ok("echo", "echoed")];

    let (loop_, events_rx) = spawn_loop(provider, tools, AgentConfig::default());

    loop_
        .send(AgentInput::UserMessage(user_msg("hello")))
        .await
        .expect("send");
    let events_handle = tokio::spawn(collect_events(events_rx));
    let outcome = loop_.join().await;
    let events = events_handle.await.expect("events drain");

    assert_eq!(outcome, Outcome::Done(StopReason::EndTurn));

    let kinds: Vec<&str> = events.iter().map(kind).collect();
    assert_eq!(
        kinds,
        vec![
            "agent_start",
            "message_start",       // user
            "message_end",         // user
            "turn_start",          // turn 1
            "message_start",       // assistant w/ tool call
            "message_end",         // assistant w/ tool call
            "tool_call_started",   // echo
            "tool_call_completed", // echo
            "turn_end",            // end turn 1
            "turn_start",          // turn 2
            "text_delta",          // "done"
            "message_start",       // assistant text
            "message_end",         // assistant text
            "turn_end",            // end turn 2
            "done",
        ]
    );

    if let AgentEvent::Done { turn_count, .. } = events.last().unwrap() {
        assert_eq!(*turn_count, 2);
    }
}

/// B-5: tool error doesn't terminate; loop continues with the error
/// surfaced to the next LLM call.
#[tokio::test]
async fn b5_tool_error_continues() {
    let provider = MockProvider::new(vec![
        vec![ProviderEvent::Done(assistant_tool_call(
            "t1",
            "echo",
            json!({}),
        ))],
        vec![ProviderEvent::Done(assistant_text("acknowledged"))],
    ]);
    let tools = vec![MockTool::err("echo", "boom")];

    let (loop_, events_rx) = spawn_loop(provider, tools, AgentConfig::default());

    loop_
        .send(AgentInput::UserMessage(user_msg("hello")))
        .await
        .expect("send");
    let events_handle = tokio::spawn(collect_events(events_rx));
    let outcome = loop_.join().await;
    let events = events_handle.await.expect("events drain");

    assert_eq!(outcome, Outcome::Done(StopReason::EndTurn));

    // Tool completed with is_error: true.
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolCallCompleted {
            content,
            is_error: true,
            ..
        } if content == "boom"
    )));
    // Two TurnStart events (proves loop continued past the error).
    let turn_starts = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::TurnStart))
        .count();
    assert_eq!(turn_starts, 2);
}

/// B-3: iteration cap. Provider always tool-calls; loop stops at max_turns.
#[tokio::test]
async fn b3_iteration_cap() {
    let tool_call_response =
        vec![ProviderEvent::Done(assistant_tool_call("t1", "echo", json!({})))];
    let provider = MockProvider::forever(tool_call_response);
    let tools = vec![MockTool::always_ok("echo", "ok")];

    let config = AgentConfig { max_turns: 3 };
    let (loop_, events_rx) = spawn_loop(provider, tools, config);

    loop_
        .send(AgentInput::UserMessage(user_msg("hello")))
        .await
        .expect("send");
    let events_handle = tokio::spawn(collect_events(events_rx));
    let outcome = loop_.join().await;
    let events = events_handle.await.expect("events drain");

    assert_eq!(outcome, Outcome::IterationCap);

    let turn_starts = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::TurnStart))
        .count();
    assert_eq!(turn_starts, 3, "expected exactly 3 TurnStart events");

    if let Some(AgentEvent::Done { turn_count, .. }) = events.last() {
        assert_eq!(*turn_count, 3);
    } else {
        panic!("last event not Done; got {:?}", events.last().map(kind));
    }
}

/// B-4: cancel during a long stream returns Outcome::Cancelled promptly.
#[tokio::test]
async fn b4_cancel_during_stream() {
    let provider = MockProvider::pending();
    let (loop_, events_rx) = spawn_loop(provider, vec![], AgentConfig::default());

    loop_
        .send(AgentInput::UserMessage(user_msg("hello")))
        .await
        .expect("send user");
    // Give the loop a beat to enter the stream.
    tokio::time::sleep(Duration::from_millis(20)).await;
    loop_
        .send(AgentInput::Cancel)
        .await
        .expect("send cancel");

    let events_handle = tokio::spawn(collect_events(events_rx));
    let outcome = timeout(Duration::from_millis(500), loop_.join())
        .await
        .expect("loop did not terminate within 500ms");
    let _events = events_handle.await.expect("events drain");

    assert_eq!(outcome, Outcome::Cancelled);
}

/// B-7: UserMessage during tool execution lands AFTER tool completion,
/// BEFORE the next TurnStart.
#[tokio::test]
async fn b7_user_message_during_tool_pushes_to_back() {
    let provider = MockProvider::new(vec![
        vec![ProviderEvent::Done(assistant_tool_call(
            "t1",
            "slow",
            json!({}),
        ))],
        vec![ProviderEvent::Done(assistant_text("done"))],
    ]);
    let tools = vec![MockTool::delayed(
        "slow",
        "tool result",
        Duration::from_millis(50),
    )];

    let (loop_, events_rx) = spawn_loop(provider, tools, AgentConfig::default());

    loop_
        .send(AgentInput::UserMessage(user_msg("first")))
        .await
        .expect("send first");
    // Wait for the tool to start, then send a UserMessage.
    tokio::time::sleep(Duration::from_millis(20)).await;
    loop_
        .send(AgentInput::UserMessage(user_msg("second")))
        .await
        .expect("send second");

    let events_handle = tokio::spawn(collect_events(events_rx));
    let outcome = loop_.join().await;
    let events = events_handle.await.expect("events drain");

    assert_eq!(outcome, Outcome::Done(StopReason::EndTurn));

    // Find the indices of key events. We assert ordering relative to each other.
    let tool_completed_idx = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolCallCompleted { .. }))
        .expect("missing tool_call_completed");

    // The MessageStart for the "second" user message must come AFTER
    // tool_call_completed.
    let second_user_idx = events
        .iter()
        .enumerate()
        .find_map(|(i, e)| match e {
            AgentEvent::MessageStart {
                message: AgentMessage::User { content },
            } if content == "second" => Some(i),
            _ => None,
        })
        .expect("missing MessageStart for 'second'");
    assert!(
        second_user_idx > tool_completed_idx,
        "'second' user message should appear AFTER tool_call_completed (got idx {} vs {})",
        second_user_idx,
        tool_completed_idx
    );

    // The next TurnStart after second_user_idx must be from the second
    // InvokeLlm (which sees both the tool result and the second user
    // message). That TurnStart must appear AFTER second's MessageEnd.
    let second_user_end_idx = events
        .iter()
        .enumerate()
        .skip(second_user_idx)
        .find_map(|(i, e)| match e {
            AgentEvent::MessageEnd {
                message: AgentMessage::User { content },
            } if content == "second" => Some(i),
            _ => None,
        })
        .expect("missing MessageEnd for 'second'");

    let next_turn_start_idx = events
        .iter()
        .enumerate()
        .skip(second_user_end_idx)
        .find(|(_, e)| matches!(e, AgentEvent::TurnStart))
        .map(|(i, _)| i)
        .expect("missing TurnStart after 'second' user message");

    assert!(
        next_turn_start_idx > second_user_end_idx,
        "TurnStart for the next turn must come AFTER second user MessageEnd"
    );
}

// ============================================================================
// Pure-planner tests
// ============================================================================
//
// These hit the queue-mediated logic without spawning anything.
// Behavior tests (B-1..B-7 above) cover the integrated flow with
// mock providers/tools. These complement by hitting the planning
// logic directly with edge-case inputs.

fn assistant_text_msg(text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
        stop_reason: StopReason::EndTurn,
    }
}

fn assistant_tool_msg(id: &str, name: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: serde_json::json!({}),
        })],
        stop_reason: StopReason::ToolUse,
    }
}

#[test]
fn plan_post_invoke_llm_text_only_no_buffered() {
    let plan = plan_post_invoke_llm(&assistant_text_msg("done"), vec![]);
    assert!(plan.emit_turn_end);
    assert_eq!(plan.actions.len(), 1);
    assert!(matches!(plan.actions[0], Action::MaybeFinish));
}

#[test]
fn plan_post_invoke_llm_text_only_with_buffered() {
    let buffered = vec![AgentInput::UserMessage(user_msg("more"))];
    let plan = plan_post_invoke_llm(&assistant_text_msg("ok"), buffered);
    // Even with text-only, buffered UMs go into the queue. No
    // MaybeFinish — the UM handler will push InvokeLlm.
    assert!(plan.emit_turn_end);
    assert_eq!(plan.actions.len(), 1);
    assert!(matches!(
        plan.actions[0],
        Action::External(AgentInput::UserMessage(_))
    ));
}

#[test]
fn plan_post_invoke_llm_with_tools_no_buffered() {
    let plan = plan_post_invoke_llm(&assistant_tool_msg("t1", "echo"), vec![]);
    // TurnEnd defers until ExecuteTools completes.
    assert!(!plan.emit_turn_end);
    assert_eq!(plan.actions.len(), 1);
    match &plan.actions[0] {
        Action::ExecuteTools(calls) => {
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].id, "t1");
            assert_eq!(calls[0].name, "echo");
        }
        other => panic!("expected ExecuteTools, got {other:?}"),
    }
}

#[test]
fn plan_post_invoke_llm_with_tools_and_buffered() {
    // Tool calls + buffered UMs: both go on the queue, ExecuteTools
    // first so tools run before the user's queued message is
    // processed.
    let buffered = vec![AgentInput::UserMessage(user_msg("inject"))];
    let plan = plan_post_invoke_llm(&assistant_tool_msg("t1", "echo"), buffered);
    assert_eq!(plan.actions.len(), 2);
    assert!(matches!(plan.actions[0], Action::ExecuteTools(_)));
    assert!(matches!(
        plan.actions[1],
        Action::External(AgentInput::UserMessage(_))
    ));
}

#[test]
fn plan_post_execute_tools_basic() {
    let actions = plan_post_execute_tools(vec![]);
    assert_eq!(actions.len(), 1);
    assert!(matches!(actions[0], Action::InvokeLlm));
}

#[test]
fn plan_post_execute_tools_with_buffered_orders_inputs_first() {
    let buffered = vec![
        AgentInput::UserMessage(user_msg("first")),
        AgentInput::UserMessage(user_msg("second")),
    ];
    let actions = plan_post_execute_tools(buffered);
    // External(first), External(second), InvokeLlm — buffered go
    // first so their context is available when the LLM runs.
    assert_eq!(actions.len(), 3);
    assert!(matches!(
        actions[0],
        Action::External(AgentInput::UserMessage(_))
    ));
    assert!(matches!(
        actions[1],
        Action::External(AgentInput::UserMessage(_))
    ));
    assert!(matches!(actions[2], Action::InvokeLlm));
}

#[test]
fn should_push_invoke_llm_empty_queue_yes() {
    let q: VecDeque<Action> = VecDeque::new();
    assert!(should_push_invoke_llm(&q));
}

#[test]
fn should_push_invoke_llm_already_queued_no() {
    let mut q: VecDeque<Action> = VecDeque::new();
    q.push_back(Action::InvokeLlm);
    assert!(!should_push_invoke_llm(&q));
}

#[test]
fn should_push_invoke_llm_other_actions_yes() {
    let mut q: VecDeque<Action> = VecDeque::new();
    q.push_back(Action::MaybeFinish);
    q.push_back(Action::External(AgentInput::UserMessage(user_msg("x"))));
    assert!(should_push_invoke_llm(&q));
}
