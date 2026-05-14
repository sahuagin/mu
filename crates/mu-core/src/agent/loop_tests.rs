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
use crate::agent::types::{AgentMessage, AssistantMessage, ContentBlock, StopReason, ToolCall};

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
        _system_prompt: Option<&str>,
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
    /// Optional non-default policy. Tests use `with_policy(...)` to
    /// mark a mock as needing approval (PermissionLevel::Ask), etc.
    policy_override: Option<crate::agent::tool::ToolPolicy>,
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
            policy_override: None,
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
            policy_override: None,
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
            policy_override: None,
        }
    }

    /// Set a non-default policy on this MockTool. Used by mu-029
    /// tests to mark a mock as PermissionLevel::Ask, etc.
    fn with_policy(mut self, policy: crate::agent::tool::ToolPolicy) -> Self {
        self.policy_override = Some(policy);
        self
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
            policy_override: None,
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
            policy: self.policy_override.clone().unwrap_or_default(),
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
        usage: None,
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
        usage: None,
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
    let approvals: PendingApprovals = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let capability: SessionCapability = Arc::new(Mutex::new(crate::capability::Capability::root()));
    let loop_ = AgentLoop::spawn(provider, tools, config, events_tx, approvals, capability);
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
        AgentEvent::InputRequired { .. } => "input_required",
        AgentEvent::ContextAssembly { .. } => "context_assembly",
        AgentEvent::CompactionAssembly { .. } => "compaction_assembly",
        AgentEvent::ProviderStatus { .. } => "provider_status",
        AgentEvent::AutonomousIterationStarted { .. } => "autonomous_iteration_started",
        AgentEvent::AutonomousIterationCompleted { .. } => "autonomous_iteration_completed",
        AgentEvent::AutonomousTerminated { .. } => "autonomous_terminated",
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
            "context_assembly", // mu-032: emitted before provider.stream
            "provider_status",  // mu-035 Phase A: AwaitingFirstToken
            "provider_status",  // mu-035 Phase A: Streaming on first token
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

/// mu-s5h: Done event must reflect the per-turn provider stop_reason
/// (here `MaxTokens`) rather than collapsing every natural-completion
/// path to EndTurn.
#[tokio::test]
async fn s5h_done_propagates_max_tokens_from_per_turn_stop_reason() {
    let assistant = AssistantMessage {
        content: vec![ContentBlock::Text {
            text: "cut off mid-".into(),
        }],
        stop_reason: StopReason::MaxTokens,
        usage: None,
    };
    let provider = MockProvider::new(vec![vec![
        ProviderEvent::TextDelta("cut off mid-".into()),
        ProviderEvent::Done(assistant),
    ]]);
    let (loop_, events_rx) = spawn_loop(provider, vec![], AgentConfig::default());

    loop_
        .send(AgentInput::UserMessage(user_msg("write a long thing")))
        .await
        .expect("send");
    let events_handle = tokio::spawn(collect_events(events_rx));
    let _outcome = loop_.join().await;
    let events = events_handle.await.expect("events drain");

    let done = events
        .iter()
        .rev()
        .find_map(|e| match e {
            AgentEvent::Done { stop_reason, .. } => Some(*stop_reason),
            _ => None,
        })
        .expect("Done event missing");
    assert_eq!(
        done,
        StopReason::MaxTokens,
        "Done.stop_reason must reflect the per-turn provider stop_reason, not collapsed to EndTurn"
    );
}

/// mu-s5h: EndTurn is still the natural-completion default when the
/// provider reports it. Regression guard so the propagation fix
/// doesn't accidentally invent a different stop_reason.
#[tokio::test]
async fn s5h_done_propagates_end_turn_when_provider_reports_end_turn() {
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
    let _outcome = loop_.join().await;
    let events = events_handle.await.expect("events drain");

    let done = events
        .iter()
        .rev()
        .find_map(|e| match e {
            AgentEvent::Done { stop_reason, .. } => Some(*stop_reason),
            _ => None,
        })
        .expect("Done event missing");
    assert_eq!(done, StopReason::EndTurn);
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
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Error { message } if message == "rate limit")),
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
            "context_assembly",    // mu-032: before provider call
            "provider_status",     // mu-035: AwaitingFirstToken
            "message_start", // assistant w/ tool call (no text first → no Streaming transition)
            "message_end",   // assistant w/ tool call
            "provider_status", // mu-035: ToolExecuting before dispatch
            "tool_call_started", // echo
            "tool_call_completed", // echo
            "turn_end",      // end turn 1
            "turn_start",    // turn 2
            "context_assembly", // mu-032: before second provider call
            "provider_status", // mu-035: AwaitingFirstToken turn 2
            "provider_status", // mu-035: Streaming on first token
            "text_delta",    // "done"
            "message_start", // assistant text
            "message_end",   // assistant text
            "turn_end",      // end turn 2
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
    let tool_call_response = vec![ProviderEvent::Done(assistant_tool_call(
        "t1",
        "echo",
        json!({}),
    ))];
    let provider = MockProvider::forever(tool_call_response);
    let tools = vec![MockTool::always_ok("echo", "ok")];

    let config = AgentConfig {
        max_turns: 3,
        system_prompt: None,
        compaction_threshold: None,
    };
    let (loop_, events_rx) = spawn_loop(provider, tools, config);

    loop_
        .send(AgentInput::UserMessage(user_msg("hello")))
        .await
        .expect("send");
    let events_handle = tokio::spawn(collect_events(events_rx));
    let outcome = loop_.join().await;
    let events = events_handle.await.expect("events drain");

    // Post mu-035 Phase A multi-turn fix: hitting the iteration cap
    // terminates the ASK, not the session. The session returns to
    // Idle and the loop only exits cleanly when join() drops the
    // sender. So Outcome::Done(EndTurn) is the new expected value;
    // the iteration-cap effect is observable via turn_count == 3 in
    // the final Done event.
    assert_eq!(outcome, Outcome::Done(StopReason::EndTurn));

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
    loop_.send(AgentInput::Cancel).await.expect("send cancel");

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
        usage: None,
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
        usage: None,
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

// ============================================================================
// ToolHistory — RetryPolicy::Never enforcement backing store
// ============================================================================

#[test]
fn tool_history_empty_has_no_match() {
    let h = ToolHistory::default();
    assert!(!h.errored_match("bash", &json!({"command": "echo hi"})));
}

#[test]
fn tool_history_matches_errored_call_exactly() {
    let mut h = ToolHistory::default();
    h.record("bash".into(), json!({"command": "ls; rm /"}), true);
    assert!(h.errored_match("bash", &json!({"command": "ls; rm /"})));
    // Same tool, different args — no match.
    assert!(!h.errored_match("bash", &json!({"command": "ls"})));
    // Different tool, same args — no match.
    assert!(!h.errored_match("edit", &json!({"command": "ls; rm /"})));
}

#[test]
fn tool_history_does_not_match_succeeded_calls() {
    let mut h = ToolHistory::default();
    // Successful call — not in the "should refuse retry" set.
    h.record("read".into(), json!({"path": "/etc/hosts"}), false);
    assert!(!h.errored_match("read", &json!({"path": "/etc/hosts"})));
}

#[test]
fn tool_history_window_evicts_oldest() {
    let mut h = ToolHistory::default();
    // Fill past the window; oldest should evict.
    for i in 0..(TOOL_HISTORY_WINDOW + 3) {
        h.record("bash".into(), json!({"command": format!("cmd{i}")}), true);
    }
    // Window is capped.
    assert_eq!(h.entries.len(), TOOL_HISTORY_WINDOW);
    // Earliest entries dropped off — first command should not match.
    assert!(!h.errored_match("bash", &json!({"command": "cmd0"})));
    // Recent entries still there.
    let last = TOOL_HISTORY_WINDOW + 2;
    assert!(h.errored_match("bash", &json!({"command": format!("cmd{last}")})));
}

#[test]
fn tool_history_clear_empties_window() {
    let mut h = ToolHistory::default();
    h.record("bash".into(), json!({"command": "x"}), true);
    assert!(h.errored_match("bash", &json!({"command": "x"})));
    h.clear();
    assert!(!h.errored_match("bash", &json!({"command": "x"})));
    assert!(h.entries.is_empty());
}

#[test]
fn tool_history_streak_counts_consecutive_errors_per_tool() {
    let mut h = ToolHistory::default();
    // Three different bash commands, all errored. The streak from
    // bash's perspective is 3, regardless of args.
    h.record("bash".into(), json!({"command": "a"}), true);
    h.record("bash".into(), json!({"command": "b"}), true);
    h.record("bash".into(), json!({"command": "c"}), true);
    assert_eq!(h.consecutive_errors_for("bash"), 3);
    // Other tools at zero.
    assert_eq!(h.consecutive_errors_for("read"), 0);
}

#[test]
fn tool_history_streak_breaks_on_success() {
    let mut h = ToolHistory::default();
    h.record("bash".into(), json!({"command": "a"}), true);
    h.record("bash".into(), json!({"command": "b"}), true);
    // Success in the middle breaks the streak.
    h.record("bash".into(), json!({"command": "c"}), false);
    h.record("bash".into(), json!({"command": "d"}), true);
    // From the latest entry walking back: error (d), then success
    // (c) — streak is 1.
    assert_eq!(h.consecutive_errors_for("bash"), 1);
}

#[test]
fn tool_history_streak_skips_other_tools_without_breaking() {
    let mut h = ToolHistory::default();
    h.record("bash".into(), json!({"command": "a"}), true);
    h.record("read".into(), json!({"path": "/x"}), false); // unrelated, skipped
    h.record("bash".into(), json!({"command": "b"}), true);
    // bash streak from newest: error, [skip read], error — count 2.
    assert_eq!(h.consecutive_errors_for("bash"), 2);
}

// ============================================================================
// mu-029 PermissionLevel::Ask approval flow
// ============================================================================

/// Build a MockProvider scripted to issue a single tool call then
/// stop. Useful for end-to-end Ask-flow tests.
fn mock_provider_one_tool_call(tool_name: &str, args: Value) -> MockProvider {
    let call = ToolCall {
        id: "call_under_test".to_string(),
        name: tool_name.to_string(),
        arguments: args,
    };
    // First provider call: emit the tool call.
    let first_turn = vec![ProviderEvent::Done(AssistantMessage {
        content: vec![ContentBlock::ToolCall(call)],
        stop_reason: StopReason::ToolUse,
        usage: None,
    })];
    // Second provider call (after the tool result): emit text + EndTurn.
    let second_turn = vec![
        ProviderEvent::TextDelta("ok".to_string()),
        ProviderEvent::Done(AssistantMessage {
            content: vec![ContentBlock::Text {
                text: "ok".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: None,
        }),
    ];
    let mut q = VecDeque::new();
    q.push_back(MockResponse::Events(first_turn));
    q.push_back(MockResponse::Events(second_turn));
    MockProvider {
        responses: Mutex::new(q),
    }
}

#[tokio::test]
async fn ask_permission_emits_input_required_and_dispatches_on_approve() {
    let provider = mock_provider_one_tool_call("gated", json!({"x": 1}));
    let tool = MockTool::ok("gated", "tool ran").with_policy(crate::agent::tool::ToolPolicy {
        permission: crate::agent::tool::PermissionLevel::Ask,
        ..Default::default()
    });
    let approvals: PendingApprovals = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let cap: SessionCapability = Arc::new(Mutex::new(crate::capability::Capability::root()));
    let (events_tx, mut events_rx) = mpsc::channel(64);
    let loop_ = AgentLoop::spawn(
        Arc::new(provider),
        vec![Arc::new(tool) as Arc<dyn Tool>],
        AgentConfig::default(),
        events_tx,
        approvals.clone(),
        cap,
    );
    loop_
        .send(AgentInput::UserMessage(user_msg("please use gated")))
        .await
        .expect("send");

    // Drain events until InputRequired arrives; capture request_id.
    let mut request_id: Option<String> = None;
    let mut tool_call_started_seen = false;
    let mut all_events: Vec<AgentEvent> = Vec::new();
    while let Some(ev) = events_rx.recv().await {
        match &ev {
            AgentEvent::ToolCallStarted { .. } => tool_call_started_seen = true,
            AgentEvent::InputRequired {
                request_id: rid, ..
            } => {
                request_id = Some(rid.clone());
                break;
            }
            _ => {}
        }
        all_events.push(ev);
    }
    let rid = request_id.expect("InputRequired event should fire for Ask policy");
    assert!(
        tool_call_started_seen,
        "ToolCallStarted should fire before InputRequired"
    );

    // Approve.
    let sender = approvals
        .lock()
        .unwrap()
        .remove(&rid)
        .expect("approvals registry should have an entry under request_id");
    sender
        .send(ApprovalDecision::Approve)
        .expect("send approve");

    // Drain the rest. Expect ToolCallCompleted with non-error
    // (the mock returned "tool ran" as success).
    let mut got_tool_completed_ok = false;
    let mut got_done = false;
    while let Some(ev) = events_rx.recv().await {
        match ev {
            AgentEvent::ToolCallCompleted {
                is_error, content, ..
            } => {
                got_tool_completed_ok = !is_error && content.contains("tool ran");
            }
            AgentEvent::Done { .. } => {
                got_done = true;
                break;
            }
            _ => {}
        }
    }
    assert!(
        got_tool_completed_ok,
        "expected non-error ToolCallCompleted after approval"
    );
    assert!(got_done, "expected Done event at end");
}

#[tokio::test]
async fn capability_refuses_tool_outside_allowed_set() {
    use crate::capability::Capability;
    use std::collections::HashSet;

    // Provider scripted to call "blocked" tool (not in the
    // session's capability set).
    let provider = mock_provider_one_tool_call("blocked", json!({"x": 1}));
    // A real tool by that name, so absence is not the reason for
    // failure — capability check is.
    let tool = MockTool::ok("blocked", "this should not run");
    // Session is attenuated to allow only "echo".
    let mut allowed = HashSet::new();
    allowed.insert("echo".to_string());
    let cap: SessionCapability = Arc::new(Mutex::new(Capability {
        allowed_tools: Some(allowed),
        ..Default::default()
    }));
    let approvals: PendingApprovals = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let (events_tx, mut events_rx) = mpsc::channel(64);
    let loop_ = AgentLoop::spawn(
        Arc::new(provider),
        vec![Arc::new(tool) as Arc<dyn Tool>],
        AgentConfig::default(),
        events_tx,
        approvals,
        cap,
    );
    loop_
        .send(AgentInput::UserMessage(user_msg("try blocked")))
        .await
        .unwrap();

    // Drain events. Expect: a Callout (warning) for the refusal,
    // then ToolCallCompleted with is_error=true, then Done.
    let mut got_capability_callout = false;
    let mut completed_content: Option<String> = None;
    let mut completed_is_error = false;
    while let Some(ev) = events_rx.recv().await {
        match ev {
            AgentEvent::Callout {
                category, title, ..
            } if category == "warning" && title.contains("capability refused") => {
                got_capability_callout = true;
            }
            AgentEvent::ToolCallCompleted {
                content, is_error, ..
            } => {
                completed_content = Some(content);
                completed_is_error = is_error;
            }
            AgentEvent::Done { .. } => break,
            _ => {}
        }
    }
    assert!(
        got_capability_callout,
        "expected a 'capability refused' callout"
    );
    let content = completed_content.expect("ToolCallCompleted should fire");
    assert!(completed_is_error, "capability refusal => is_error");
    assert!(
        content.contains("session capability"),
        "refusal message should name capability; got: {content}"
    );
    assert!(
        !content.contains("this should not run"),
        "tool body must not have executed; got: {content}"
    );
}

#[tokio::test]
async fn capability_refuses_tool_missing_required_aws_capability() {
    use crate::agent::tool::{PermissionLevel, RetryPolicy, SideEffects, ToolPolicy};
    use crate::capability::Capability;

    let provider =
        mock_provider_one_tool_call("aws_recon", json!({"capability": "aws.scout.readonly"}));
    let tool = MockTool::ok("aws_recon", "this should not run").with_policy(ToolPolicy {
        side_effects: SideEffects::External,
        permission: PermissionLevel::Allow,
        retry: RetryPolicy::ModelDecides,
        required_aws_capability: Some("aws.scout.readonly".to_string()),
        idempotent: false,
    });
    let cap: SessionCapability = Arc::new(Mutex::new(Capability::root()));
    let approvals: PendingApprovals = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let (events_tx, mut events_rx) = mpsc::channel(64);
    let loop_ = AgentLoop::spawn(
        Arc::new(provider),
        vec![Arc::new(tool) as Arc<dyn Tool>],
        AgentConfig::default(),
        events_tx,
        approvals,
        cap,
    );
    loop_
        .send(AgentInput::UserMessage(user_msg("run aws recon")))
        .await
        .unwrap();

    let mut got_capability_callout = false;
    let mut completed_content: Option<String> = None;
    let mut completed_is_error = false;
    while let Some(ev) = events_rx.recv().await {
        match ev {
            AgentEvent::Callout {
                category, title, ..
            } if category == "warning" && title.contains("capability refused") => {
                got_capability_callout = true;
            }
            AgentEvent::ToolCallCompleted {
                content, is_error, ..
            } => {
                completed_content = Some(content);
                completed_is_error = is_error;
            }
            AgentEvent::Done { .. } => break,
            _ => {}
        }
    }

    assert!(
        got_capability_callout,
        "expected capability refusal callout"
    );
    let content = completed_content.expect("ToolCallCompleted should fire");
    assert!(completed_is_error, "missing AWS cap => is_error");
    assert!(
        content.contains("missing required AWS capability `aws.scout.readonly`"),
        "refusal should name missing AWS cap; got: {content}"
    );
    assert!(!content.contains("this should not run"));
}

#[tokio::test]
async fn capability_allows_tool_when_required_aws_capability_is_held() {
    use crate::agent::tool::{PermissionLevel, RetryPolicy, SideEffects, ToolPolicy};
    use crate::capability::{AwsCapability, Capability};
    use std::collections::HashSet;

    let provider =
        mock_provider_one_tool_call("aws_recon", json!({"capability": "aws.scout.readonly"}));
    let tool = MockTool::ok("aws_recon", "aws recon ran").with_policy(ToolPolicy {
        side_effects: SideEffects::External,
        permission: PermissionLevel::Allow,
        retry: RetryPolicy::ModelDecides,
        required_aws_capability: Some("aws.scout.readonly".to_string()),
        idempotent: false,
    });
    let cap: SessionCapability = Arc::new(Mutex::new(Capability {
        aws: HashSet::from([AwsCapability {
            name: "aws.scout.readonly".to_string(),
            session_policy: None,
        }]),
        ..Default::default()
    }));
    let approvals: PendingApprovals = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let (events_tx, mut events_rx) = mpsc::channel(64);
    let loop_ = AgentLoop::spawn(
        Arc::new(provider),
        vec![Arc::new(tool) as Arc<dyn Tool>],
        AgentConfig::default(),
        events_tx,
        approvals,
        cap,
    );
    loop_
        .send(AgentInput::UserMessage(user_msg("run aws recon")))
        .await
        .unwrap();

    let mut completed_content: Option<String> = None;
    let mut completed_is_error = true;
    while let Some(ev) = events_rx.recv().await {
        match ev {
            AgentEvent::ToolCallCompleted {
                content, is_error, ..
            } => {
                completed_content = Some(content);
                completed_is_error = is_error;
            }
            AgentEvent::Done { .. } => break,
            _ => {}
        }
    }

    assert!(!completed_is_error, "held AWS cap should allow dispatch");
    assert_eq!(completed_content.as_deref(), Some("aws recon ran"));
}

#[tokio::test]
async fn ask_permission_deny_synthesizes_error_result_without_running_tool() {
    let provider = mock_provider_one_tool_call("gated", json!({"x": 1}));
    let tool = MockTool::ok("gated", "this should not appear").with_policy(
        crate::agent::tool::ToolPolicy {
            permission: crate::agent::tool::PermissionLevel::Ask,
            ..Default::default()
        },
    );
    let approvals: PendingApprovals = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let cap: SessionCapability = Arc::new(Mutex::new(crate::capability::Capability::root()));
    let (events_tx, mut events_rx) = mpsc::channel(64);
    let loop_ = AgentLoop::spawn(
        Arc::new(provider),
        vec![Arc::new(tool) as Arc<dyn Tool>],
        AgentConfig::default(),
        events_tx,
        approvals.clone(),
        cap,
    );
    loop_
        .send(AgentInput::UserMessage(user_msg("please use gated")))
        .await
        .unwrap();

    // Wait for InputRequired, then DENY.
    let mut request_id: Option<String> = None;
    while let Some(ev) = events_rx.recv().await {
        if let AgentEvent::InputRequired {
            request_id: rid, ..
        } = ev
        {
            request_id = Some(rid);
            break;
        }
    }
    let rid = request_id.expect("InputRequired");
    let sender = approvals.lock().unwrap().remove(&rid).unwrap();
    sender.send(ApprovalDecision::Deny).unwrap();

    // The tool should NOT have been invoked; ToolCallCompleted
    // should report is_error=true with a "denied by user" message.
    let mut completed_ev: Option<(bool, String)> = None;
    while let Some(ev) = events_rx.recv().await {
        if let AgentEvent::ToolCallCompleted {
            is_error, content, ..
        } = ev
        {
            completed_ev = Some((is_error, content));
            break;
        }
    }
    let (is_error, content) = completed_ev.expect("ToolCallCompleted after deny");
    assert!(is_error, "denial should produce is_error=true");
    assert!(
        content.contains("denied"),
        "denial result content should mention 'denied'; got: {content}"
    );
    // The mock's "this should not appear" string must NOT be in
    // the completed event content — proving the tool didn't run.
    assert!(
        !content.contains("this should not appear"),
        "tool body must not have executed after deny; got: {content}"
    );
}

// ============================================================================
// mu-036 Phase B (mu-3ao): autonomous-mode tests
// ============================================================================
//
// These exercise the agent loop's RunMode::Autonomous path driven by
// AgentInput::StartAutonomous. They verify:
//   - iteration cap enforcement from the session's Capability (INV-2)
//   - SelfReport goal-status early termination
//   - INV-7: AutonomousTerminated is the LAST autonomy-namespace event
//   - defensive callout when capability is Disallowed at StartAutonomous
//     time (dispatch already gates this; the loop-side check is a
//     belt-and-braces re-verification)

fn spawn_loop_with_autonomy(
    provider: MockProvider,
    tools: Vec<MockTool>,
    config: AgentConfig,
    autonomy: crate::capability::AutonomyCapability,
) -> (AgentLoop, mpsc::Receiver<AgentEvent>) {
    let (events_tx, events_rx) = mpsc::channel(64);
    let provider: Arc<dyn Provider> = Arc::new(provider);
    let tools: Vec<Arc<dyn Tool>> = tools
        .into_iter()
        .map(|t| Arc::new(t) as Arc<dyn Tool>)
        .collect();
    let approvals: PendingApprovals = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let mut cap = crate::capability::Capability::root();
    cap.autonomy = autonomy;
    let capability: SessionCapability = Arc::new(Mutex::new(cap));
    let loop_ = AgentLoop::spawn(provider, tools, config, events_tx, approvals, capability);
    (loop_, events_rx)
}

fn autonomy_allowed(max_iter: u32) -> crate::capability::AutonomyCapability {
    crate::capability::AutonomyCapability::Allowed {
        max_iterations: max_iter,
        max_wall_clock_ms: 60_000,
        max_total_tool_calls_in_autonomy: 100,
        allow_schedule_wakeup: false,
        allow_delegate_grader: false,
    }
}

fn last_autonomy_event(events: &[AgentEvent]) -> Option<&AgentEvent> {
    events.iter().rev().find(|e| {
        matches!(
            e,
            AgentEvent::AutonomousIterationStarted { .. }
                | AgentEvent::AutonomousIterationCompleted { .. }
                | AgentEvent::AutonomousTerminated { .. }
        )
    })
}

/// A-1: iteration cap enforcement.
///
/// Capability says max_iterations: 3. MockProvider responds with an
/// assistant message that lacks a `goal_status` marker → outcome is
/// always Continue. After 3 iterations, MaybeFinish trips the
/// IterationCap branch and emits AutonomousTerminated{IterationCap}.
#[tokio::test]
async fn a1_iteration_cap_terminates_at_capability_bound() {
    let provider = MockProvider::forever(vec![ProviderEvent::Done(assistant_text("working"))]);
    let (loop_, events_rx) = spawn_loop_with_autonomy(
        provider,
        vec![],
        AgentConfig::default(),
        autonomy_allowed(3),
    );

    loop_
        .send(AgentInput::StartAutonomous {
            goal: "drive 3 iterations".to_owned(),
            options: crate::protocol::AutonomyOptions::default(),
        })
        .await
        .expect("send");
    let events_handle = tokio::spawn(collect_events(events_rx));
    let _ = loop_.join().await;
    let events = events_handle.await.expect("events drain");

    let starts = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::AutonomousIterationStarted { .. }))
        .count();
    let completes = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::AutonomousIterationCompleted { .. }))
        .count();
    let terminates: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::AutonomousTerminated { .. }))
        .collect();
    assert_eq!(
        starts,
        3,
        "expected 3 iteration starts, kinds={:?}",
        events.iter().map(kind).collect::<Vec<_>>()
    );
    assert_eq!(completes, 3, "expected 3 iteration completes");
    assert_eq!(terminates.len(), 1, "expected exactly one terminate");
    match terminates[0] {
        AgentEvent::AutonomousTerminated { reason } => {
            assert!(
                matches!(reason, AutonomousTerminationReason::IterationCap),
                "expected IterationCap, got {reason:?}"
            );
        }
        other => panic!("unexpected: {other:?}"),
    }

    // INV-7: AutonomousTerminated is the LAST autonomy-namespace event.
    let last = last_autonomy_event(&events).expect("at least one autonomy event");
    assert!(
        matches!(last, AgentEvent::AutonomousTerminated { .. }),
        "INV-7: last autonomy event must be AutonomousTerminated; got {:?}",
        kind(last)
    );
}

/// A-2: SelfReport goal_status:satisfied early termination.
///
/// Iteration 1: provider yields a plain message (no marker → Continue).
/// Iteration 2: provider yields a message containing the terse marker
/// `goal_status:satisfied`. extract_goal_status finds it, MaybeFinish
/// emits AutonomousTerminated{GoalMet}.
#[tokio::test]
async fn a2_self_report_goal_met_early_termination() {
    let provider = MockProvider::new(vec![
        vec![ProviderEvent::Done(assistant_text("iteration 1 work"))],
        vec![ProviderEvent::Done(assistant_text(
            "iteration 2 done. goal_status:satisfied",
        ))],
    ]);
    let (loop_, events_rx) = spawn_loop_with_autonomy(
        provider,
        vec![],
        AgentConfig::default(),
        autonomy_allowed(10),
    );

    loop_
        .send(AgentInput::StartAutonomous {
            goal: "stop at iteration 2".to_owned(),
            options: crate::protocol::AutonomyOptions::default(),
        })
        .await
        .expect("send");
    let events_handle = tokio::spawn(collect_events(events_rx));
    let _ = loop_.join().await;
    let events = events_handle.await.expect("events drain");

    let starts = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::AutonomousIterationStarted { .. }))
        .count();
    assert_eq!(
        starts, 2,
        "expected exactly 2 iteration starts (early term)"
    );

    let terminates: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::AutonomousTerminated { .. }))
        .collect();
    assert_eq!(terminates.len(), 1, "expected one terminate");
    match terminates[0] {
        AgentEvent::AutonomousTerminated { reason } => {
            assert!(
                matches!(reason, AutonomousTerminationReason::GoalMet { .. }),
                "expected GoalMet, got {reason:?}"
            );
        }
        other => panic!("unexpected: {other:?}"),
    }

    // INV-7: AutonomousTerminated is the LAST autonomy-namespace event.
    let last = last_autonomy_event(&events).expect("at least one autonomy event");
    assert!(
        matches!(last, AgentEvent::AutonomousTerminated { .. }),
        "INV-7: last autonomy event must be AutonomousTerminated; got {:?}",
        kind(last)
    );
}

/// A-3: defensive Disallowed callout.
///
/// Dispatch already gates start_autonomous on AutonomyCapability::Allowed;
/// the loop-side defensive check ensures that if a capability is
/// revoked (or the test bypasses dispatch), no autonomous events fire
/// and a warning callout surfaces. Verifies INV-1's belt-and-braces
/// enforcement.
#[tokio::test]
async fn a3_disallowed_defensive_callout_no_autonomy_events() {
    let provider = MockProvider::forever(vec![ProviderEvent::Done(assistant_text(
        "should not be reached",
    ))]);
    let (loop_, events_rx) = spawn_loop_with_autonomy(
        provider,
        vec![],
        AgentConfig::default(),
        crate::capability::AutonomyCapability::Disallowed,
    );

    loop_
        .send(AgentInput::StartAutonomous {
            goal: "should be refused".to_owned(),
            options: crate::protocol::AutonomyOptions::default(),
        })
        .await
        .expect("send");
    let events_handle = tokio::spawn(collect_events(events_rx));
    let _ = loop_.join().await;
    let events = events_handle.await.expect("events drain");

    // No autonomous-namespace events at all.
    let any_autonomy = events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::AutonomousIterationStarted { .. }
                | AgentEvent::AutonomousIterationCompleted { .. }
                | AgentEvent::AutonomousTerminated { .. }
        )
    });
    assert!(
        !any_autonomy,
        "no autonomy events expected on Disallowed; saw {:?}",
        events.iter().map(kind).collect::<Vec<_>>()
    );

    // A warning Callout with the INV-1 reason was emitted.
    let warning_seen = events.iter().any(|e| match e {
        AgentEvent::Callout { category, body, .. } => {
            category == "warning"
                && body
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .map(|s| s.contains("Disallowed") || s.contains("INV-1"))
                    .unwrap_or(false)
        }
        _ => false,
    });
    assert!(
        warning_seen,
        "expected a warning callout citing Disallowed / INV-1; events={:?}",
        events.iter().map(kind).collect::<Vec<_>>()
    );
}

// ============================================================================
// mu-kgu.4: threshold-triggered compaction-policy dispatch
// ============================================================================
//
// These tests exercise the per-turn threshold check + policy dispatch
// the loop runs between `assemble_rope` and `renderer.render`. The
// integration contract (per the mu-kgu.4 bead):
//   - Default `NoCompactionPolicy` → no `CompactionAssembly` event
//     ever fires, no matter how large the rope.
//   - A real policy that returns a non-identity rope → exactly one
//     `CompactionAssembly` fires per render where the renderer-
//     estimated token cost crosses the configured threshold.
//   - The compaction call MUST NOT block a turn: identity returns
//     and policy failures are silent on the wire path.

use crate::context::{
    CompactionDecision, CompactionPolicy, CompactionResult, RetainedRope as ContextRope,
};

/// Mock compaction policy that drops the second half of the rope's
/// spans. Records `Dropped` decisions for each removed span. Used to
/// drive the "exactly one CompactionAssembly fires" test path.
struct EvictHalfPolicy;

impl CompactionPolicy for EvictHalfPolicy {
    fn compact(&self, rope: &ContextRope, _target_tokens: usize) -> CompactionResult {
        let spans = rope.spans();
        let keep = spans.len() / 2;
        let kept: Vec<_> = spans.iter().take(keep).cloned().collect();
        let decisions: Vec<CompactionDecision> = spans
            .iter()
            .skip(keep)
            .map(|s| CompactionDecision::Dropped {
                span_id: s.id.clone(),
                reason: "evict-half mock".to_owned(),
            })
            .collect();
        CompactionResult {
            rope: ContextRope::from_spans(kept),
            decisions,
            tokens_before: 0,
            tokens_after: 0,
            wall_clock_us: 0,
        }
    }

    fn policy_label(&self) -> &'static str {
        "evict-half-mock"
    }
}

/// Provider wrapper that adds a custom compaction policy on top of an
/// inner `MockProvider`. The wire path (`stream()`, `renderer()`,
/// `cache_strategy()`) delegates verbatim; only `compaction_policy()`
/// differs.
struct MockProviderWithCompaction {
    inner: MockProvider,
    policy: Arc<dyn CompactionPolicy>,
}

#[async_trait]
impl Provider for MockProviderWithCompaction {
    async fn stream(
        &self,
        system_prompt: Option<&str>,
        messages: &[AgentMessage],
        tools: &[ToolSpec],
        cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        self.inner
            .stream(system_prompt, messages, tools, cancel_rx)
            .await
    }

    fn compaction_policy(&self) -> Arc<dyn CompactionPolicy> {
        Arc::clone(&self.policy)
    }
}

fn spawn_loop_with_provider(
    provider: Arc<dyn Provider>,
    config: AgentConfig,
) -> (AgentLoop, mpsc::Receiver<AgentEvent>) {
    let (events_tx, events_rx) = mpsc::channel(64);
    let approvals: PendingApprovals = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let capability: SessionCapability = Arc::new(Mutex::new(crate::capability::Capability::root()));
    let loop_ = AgentLoop::spawn(provider, vec![], config, events_tx, approvals, capability);
    (loop_, events_rx)
}

/// mu-kgu.4 — default `NoCompactionPolicy` MUST NOT emit
/// `CompactionAssembly` even when the threshold is set low enough that
/// any non-trivial rope crosses it. The dispatch fires; the
/// no-op result has the same shape as the input. The contract holds
/// REGARDLESS of threshold — what matters is that the loop never
/// observed a compaction event, because the default policy's
/// `policy_label() == "no-compaction"` is the signal that "the loop
/// chose to skip emitting nothing useful." Today the loop emits
/// regardless of policy_label when threshold is crossed, BUT
/// `AgentConfig::default().compaction_threshold = None` means
/// `DEFAULT_COMPACTION_THRESHOLD = 150_000` — which any reasonable
/// test conversation falls well under. So the simplest expression of
/// "default behavior" is: with default config, no
/// CompactionAssembly events appear in normal-sized conversations.
#[tokio::test]
async fn kgu4_default_config_does_not_emit_compaction_assembly() {
    let provider = MockProvider::new(vec![vec![
        ProviderEvent::TextDelta("hi".into()),
        ProviderEvent::Done(assistant_text("hi")),
    ]]);
    let (loop_, events_rx) = spawn_loop(provider, vec![], AgentConfig::default());

    loop_
        .send(AgentInput::UserMessage(user_msg("hello there")))
        .await
        .expect("send");
    let events_handle = tokio::spawn(collect_events(events_rx));
    let outcome = loop_.join().await;
    let events = events_handle.await.expect("events drain");

    assert_eq!(outcome, Outcome::Done(StopReason::EndTurn));
    let compaction_events: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::CompactionAssembly { .. }))
        .collect();
    assert!(
        compaction_events.is_empty(),
        "default AgentConfig + default NoCompactionPolicy must produce no \
         CompactionAssembly events; saw {} ({:?})",
        compaction_events.len(),
        events.iter().map(kind).collect::<Vec<_>>(),
    );
}

/// mu-kgu.4 — with a real policy (`EvictHalfPolicy`) and a threshold
/// low enough that the initial rope crosses it, the loop MUST emit
/// exactly one `CompactionAssembly` event per render where the
/// threshold is crossed. The post-compaction rope shrinks; subsequent
/// renders may or may not re-trigger depending on size. For this
/// single-turn ask, the count is exactly one.
#[tokio::test]
async fn kgu4_evict_half_policy_fires_compaction_assembly_when_threshold_crossed() {
    let inner = MockProvider::new(vec![vec![
        ProviderEvent::TextDelta("hi".into()),
        ProviderEvent::Done(assistant_text("hi")),
    ]]);
    let provider: Arc<dyn Provider> = Arc::new(MockProviderWithCompaction {
        inner,
        policy: Arc::new(EvictHalfPolicy),
    });
    // Threshold set well below any non-trivial rope. The single user
    // message alone projects to multiple spans; the chars-per-4
    // estimator easily exceeds 1 token.
    let config = AgentConfig {
        compaction_threshold: Some(1),
        ..AgentConfig::default()
    };
    let (loop_, events_rx) = spawn_loop_with_provider(provider, config);
    loop_
        .send(AgentInput::UserMessage(user_msg(
            "this user message has enough text to estimate non-zero tokens",
        )))
        .await
        .expect("send");
    let events_handle = tokio::spawn(collect_events(events_rx));
    let outcome = loop_.join().await;
    let events = events_handle.await.expect("events drain");

    assert_eq!(outcome, Outcome::Done(StopReason::EndTurn));

    let compaction_events: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::CompactionAssembly { .. }))
        .collect();
    assert_eq!(
        compaction_events.len(),
        1,
        "exactly one CompactionAssembly expected on a single-turn ask \
         with threshold crossed; saw {} ({:?})",
        compaction_events.len(),
        events.iter().map(kind).collect::<Vec<_>>(),
    );
    let AgentEvent::CompactionAssembly {
        policy_id,
        tokens_before,
        tokens_after,
        decisions_count,
        ..
    } = compaction_events[0]
    else {
        unreachable!("filter guaranteed CompactionAssembly variant")
    };
    assert_eq!(policy_id, "evict-half-mock", "policy_label propagated");
    assert!(
        *tokens_before > 0,
        "tokens_before should reflect a non-empty rope"
    );
    assert!(
        tokens_after <= tokens_before,
        "evict-half must not grow the rope ({tokens_after} > {tokens_before})"
    );
    assert!(
        *decisions_count > 0,
        "evict-half drops at least one span on a non-trivial rope; got {decisions_count}"
    );

    // CompactionAssembly precedes ContextAssembly on the wire (their
    // shared model_call_id pairs them in the operator view).
    let positions: Vec<usize> = events
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match e {
            AgentEvent::CompactionAssembly { .. } | AgentEvent::ContextAssembly { .. } => Some(i),
            _ => None,
        })
        .collect();
    assert_eq!(
        positions.len(),
        2,
        "expected one CompactionAssembly and one ContextAssembly; got {positions:?}"
    );
    assert!(
        matches!(events[positions[0]], AgentEvent::CompactionAssembly { .. }),
        "CompactionAssembly must precede ContextAssembly"
    );
    assert!(
        matches!(events[positions[1]], AgentEvent::ContextAssembly { .. }),
        "ContextAssembly must follow CompactionAssembly"
    );
}

/// mu-kgu.4 — with a real policy but a threshold set high enough that
/// the rope is well below, the dispatch MUST be skipped and no
/// `CompactionAssembly` event fires.
#[tokio::test]
async fn kgu4_evict_half_policy_does_not_fire_when_threshold_not_crossed() {
    let inner = MockProvider::new(vec![vec![
        ProviderEvent::TextDelta("hi".into()),
        ProviderEvent::Done(assistant_text("hi")),
    ]]);
    let provider: Arc<dyn Provider> = Arc::new(MockProviderWithCompaction {
        inner,
        policy: Arc::new(EvictHalfPolicy),
    });
    let config = AgentConfig {
        compaction_threshold: Some(1_000_000),
        ..AgentConfig::default()
    };
    let (loop_, events_rx) = spawn_loop_with_provider(provider, config);
    loop_
        .send(AgentInput::UserMessage(user_msg("hi")))
        .await
        .expect("send");
    let events_handle = tokio::spawn(collect_events(events_rx));
    let outcome = loop_.join().await;
    let events = events_handle.await.expect("events drain");

    assert_eq!(outcome, Outcome::Done(StopReason::EndTurn));
    let compaction_seen = events
        .iter()
        .any(|e| matches!(e, AgentEvent::CompactionAssembly { .. }));
    assert!(
        !compaction_seen,
        "threshold not crossed → no CompactionAssembly even with a real policy; \
         saw {:?}",
        events.iter().map(kind).collect::<Vec<_>>(),
    );
}
