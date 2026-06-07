//! Tests for the queue-driven agent loop. MockProvider and MockTool
//! let tests script LLM and tool behavior precisely without spawning
//! real LLM calls or running real tools.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use futures::StreamExt as _;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use super::*;
use crate::agent::provider::{MessageInput, Provider, ProviderError, ProviderEvent};
use crate::agent::tool::{Tool, ToolResult, ToolSpec};
use crate::agent::types::{AgentMessage, AssistantMessage, ContentBlock, StopReason, ToolCall};

/// Test shim: build [`SpawnArgs`] from positional args so the existing call
/// sites convert with a single rename. Test-only ergonomics; production uses
/// the struct directly, so the lint allow lives here, on test code.
#[allow(clippy::too_many_arguments)]
fn loop_with(
    provider: Arc<dyn Provider>,
    provider_kind: Arc<str>,
    model: Arc<str>,
    tools: Vec<Arc<dyn Tool>>,
    config: AgentConfig,
    events: mpsc::Sender<AgentEvent>,
    pending_approvals: PendingApprovals,
    capability: SessionCapability,
) -> AgentLoop {
    AgentLoop::spawn(SpawnArgs {
        provider,
        provider_kind,
        model,
        tools,
        config,
        events,
        pending_approvals,
        capability,
    })
}

// ============================================================================
// MockProvider
// ============================================================================

enum MockResponse {
    Events(Vec<ProviderEvent>),
    /// Events released only after the paired gate fires. Lets a test
    /// hold a stream open while it injects input (which lands in
    /// handle_invoke_llm's `buffered`), then complete the stream
    /// deterministically (mu-wf5w).
    GatedEvents(Vec<ProviderEvent>, oneshot::Receiver<()>),
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

    /// First response is gated on the returned sender; subsequent
    /// responses stream immediately. Used to hold an ask's final
    /// stream open while the test races a follow-up input in (mu-wf5w).
    fn gated_first(
        first: Vec<ProviderEvent>,
        rest: Vec<Vec<ProviderEvent>>,
    ) -> (Self, oneshot::Sender<()>) {
        let (gate_tx, gate_rx) = oneshot::channel();
        let mut q = VecDeque::new();
        q.push_back(MockResponse::GatedEvents(first, gate_rx));
        for events in rest {
            q.push_back(MockResponse::Events(events));
        }
        (
            Self {
                responses: Mutex::new(q),
            },
            gate_tx,
        )
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
        _input: MessageInput<'_>,
        _tools: &[ToolSpec],
        _cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let chunk = self.responses.lock().expect("mutex poisoned").pop_front();
        match chunk {
            Some(MockResponse::Events(events)) => Ok(Box::pin(stream::iter(events))),
            Some(MockResponse::GatedEvents(events, gate)) => Ok(Box::pin(
                stream::once(async move {
                    let _ = gate.await;
                    stream::iter(events)
                })
                .flatten(),
            )),
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
    /// mu-bkjr: when Some, `Tool::validate` returns `Err(reason)` for
    /// every call regardless of arguments. Used to test that the
    /// dispatcher's pre-flight gate short-circuits without firing
    /// the approval round-trip.
    validate_rejection: Option<String>,
    /// mu-2e0h: when true the spec declares verbatim_result, so the
    /// tier-1 ingestion filter must bypass this tool's output.
    verbatim_result: bool,
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
            validate_rejection: None,
            verbatim_result: false,
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
            validate_rejection: None,
            verbatim_result: false,
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
            validate_rejection: None,
            verbatim_result: false,
        }
    }

    /// Set a non-default policy on this MockTool. Used by mu-029
    /// tests to mark a mock as PermissionLevel::Ask, etc.
    fn with_policy(mut self, policy: crate::agent::tool::ToolPolicy) -> Self {
        self.policy_override = Some(policy);
        self
    }

    /// mu-bkjr: configure `Tool::validate` to reject every call with
    /// this reason. The dispatcher should short-circuit the call
    /// before any approval round-trip is dispatched.
    fn with_validate_rejection(mut self, reason: impl Into<String>) -> Self {
        self.validate_rejection = Some(reason.into());
        self
    }

    /// mu-2e0h: declare verbatim_result in the spec so the tier-1
    /// ingestion filter bypasses this tool's output.
    fn with_verbatim_result(mut self) -> Self {
        self.verbatim_result = true;
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
            validate_rejection: None,
            verbatim_result: false,
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
            // mu-cvm5: the production default now FAILS CLOSED (Mutating +
            // Ask). These mocks model "a benign tool that just runs" unless
            // a test explicitly sets a stricter policy via with_policy(),
            // so they default to the read_only() opt-in — preserving the
            // pre-flip test intent (no spurious Ask gate / refusal).
            policy: self
                .policy_override
                .clone()
                .unwrap_or_else(crate::agent::tool::ToolPolicy::read_only),
            verbatim_result: self.verbatim_result,
            ..Default::default()
        }
    }

    fn validate(&self, _arguments: &Value) -> Result<(), String> {
        match &self.validate_rejection {
            Some(reason) => Err(reason.clone()),
            None => Ok(()),
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
        content: vec![ContentBlock::Text { text: text.into() }],
        stop_reason: StopReason::EndTurn,
        usage: None,
    }
}

fn assistant_tool_call(id: &str, name: &str, args: Value) -> AssistantMessage {
    use crate::agent::ToolArgs;
    AssistantMessage {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: ToolArgs::new(args).unwrap(),
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
    let loop_ = loop_with(
        provider,
        Arc::from("faux"),
        Arc::from("faux"),
        tools,
        config,
        events_tx,
        approvals,
        capability,
    );
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
        AgentEvent::AssistantTextFinalized { .. } => "assistant_text_finalized",
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
        AgentEvent::AutonomousScheduledWakeup { .. } => "autonomous_scheduled_wakeup",
        AgentEvent::AutonomousTerminated { .. } => "autonomous_terminated",
        AgentEvent::ProviderSwitched { .. } => "provider_switched",
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
            "assistant_text_finalized", // mu-wk2: fires before message_start of finalized assistant text
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

/// B-6: provider error is recoverable — loop emits Error + Done(Error)
/// and stays alive for the next ask_session.
#[tokio::test]
async fn b6_provider_error_recoverable() {
    let provider = MockProvider::new(vec![
        vec![ProviderEvent::Error("rate limit".into())],
        vec![
            ProviderEvent::TextDelta("ok".into()),
            ProviderEvent::Done(assistant_text("ok")),
        ],
    ]);
    let (loop_, mut events_rx) = spawn_loop(provider, vec![], AgentConfig::default());

    loop_
        .send(AgentInput::UserMessage(user_msg("hello")))
        .await
        .expect("send");

    // Drain events until we see Done from the error recovery.
    let mut saw_error = false;
    // The loop's only non-panic exit is the Done(Error) break below, which sets
    // this true first, so the `false` init is always overwritten before the
    // assert reads it. Kept for the documenting assert.
    #[allow(unused_assignments)]
    let mut saw_done_error = false;
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(2), events_rx.recv()).await {
            Ok(Some(AgentEvent::Error { message })) if message == "rate limit" => {
                saw_error = true;
            }
            Ok(Some(AgentEvent::Done {
                stop_reason: StopReason::Error,
                ..
            })) => {
                saw_done_error = true;
                break;
            }
            Ok(Some(_)) => {}
            _ => panic!("timed out waiting for error recovery events"),
        }
    }
    assert!(saw_error, "should emit Error event");
    assert!(saw_done_error, "should emit Done(Error) after error");

    // Loop is still alive — send another message.
    loop_
        .send(AgentInput::UserMessage(user_msg("try again")))
        .await
        .expect("loop should still be alive for second ask");
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
            "message_start",            // user
            "message_end",              // user
            "turn_start",               // turn 1
            "context_assembly",         // mu-032: before provider call
            "provider_status",          // mu-035: AwaitingFirstToken
            "assistant_text_finalized", // mu-wk2: fires for every assistant turn end (empty text for tool-only)
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
            "assistant_text_finalized", // mu-wk2: fires before message_start of finalized assistant text
            "message_start",            // assistant text
            "message_end",              // assistant text
            "turn_end",                 // end turn 2
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
        project_context: None,
        compaction_policy_override: None,
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

/// mu-779s: the iteration-cap exit emits `AgentEvent::Done {
/// stop_reason: StopReason::IterationCap }`, not `EndTurn`. Distinguishing
/// the two lets the TUI/transcript surface "turn budget exhausted" rather
/// than reporting a natural conversation end.
#[tokio::test]
async fn mu_779s_iteration_cap_done_event_uses_iteration_cap_stop_reason() {
    let tool_call_response = vec![ProviderEvent::Done(assistant_tool_call(
        "t1",
        "echo",
        json!({}),
    ))];
    let provider = MockProvider::forever(tool_call_response);
    let tools = vec![MockTool::always_ok("echo", "ok")];

    let config = AgentConfig {
        max_turns: 2,
        system_prompt: None,
        compaction_threshold: None,
        project_context: None,
        compaction_policy_override: None,
    };
    let (loop_, events_rx) = spawn_loop(provider, tools, config);

    loop_
        .send(AgentInput::UserMessage(user_msg("trigger cap")))
        .await
        .expect("send");
    let events_handle = tokio::spawn(collect_events(events_rx));
    let _outcome = loop_.join().await;
    let events = events_handle.await.expect("events drain");

    let done = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Done {
                stop_reason,
                turn_count,
                ..
            } => Some((*stop_reason, *turn_count)),
            _ => None,
        })
        .last()
        .expect("expected a Done event after iteration cap");

    assert_eq!(
        done.0,
        StopReason::IterationCap,
        "cap-exit must report IterationCap, not {:?}",
        done.0
    );
    assert_eq!(done.1, 2, "turn_count in Done event should equal max_turns");
}

/// mu-779s: per-provider max_turns defaults. Anthropic stays at 20;
/// OpenAI bumps to 35 because in practice OpenAI models dispatch
/// noticeably more tool calls per task than Anthropic; openrouter
/// sits in the middle; unknown providers fall through to the
/// conservative default.
#[test]
fn default_max_turns_for_returns_provider_aware_defaults() {
    use super::default_max_turns_for;
    assert_eq!(default_max_turns_for("anthropic_api"), 20);
    assert_eq!(default_max_turns_for("anthropic_oauth"), 20);
    assert_eq!(default_max_turns_for("openai_api"), 35);
    assert_eq!(default_max_turns_for("openai_codex"), 35);
    assert_eq!(default_max_turns_for("openrouter"), 30);
    assert_eq!(default_max_turns_for("faux"), 20);
    assert_eq!(default_max_turns_for("not_a_real_provider_kind"), 20);
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
/// mu-wf5w: a follow-up user message buffered during the FINAL
/// (no-tool-call) provider stream must not suppress the completed
/// ask's `done` terminus. Pre-fix, plan_post_invoke_llm skipped
/// MaybeFinish when buffered UMs existed, so the first ask emitted no
/// Done at all (verified in the wild, session 1a7812f064510d91: the
/// client's live block never committed and the whole turn was lost
/// from scrollback) — and started_at / turn_count / aggregated_usage
/// leaked into the follow-up ask's Done.
#[tokio::test]
async fn wf5w_buffered_um_does_not_suppress_done() {
    let (provider, gate) = MockProvider::gated_first(
        vec![
            ProviderEvent::TextDelta("first".into()),
            ProviderEvent::Done(assistant_text("first")),
        ],
        vec![vec![
            ProviderEvent::TextDelta("second".into()),
            ProviderEvent::Done(assistant_text("second")),
        ]],
    );
    let (loop_, events_rx) = spawn_loop(provider, vec![], AgentConfig::default());

    loop_
        .send(AgentInput::UserMessage(user_msg("ask one")))
        .await
        .expect("send 1");
    // Let the loop enter the (gated) provider stream, then race the
    // follow-up in: it lands in handle_invoke_llm's `buffered`.
    tokio::time::sleep(Duration::from_millis(20)).await;
    loop_
        .send(AgentInput::UserMessage(user_msg("ask two")))
        .await
        .expect("send 2");
    tokio::time::sleep(Duration::from_millis(20)).await;
    let _ = gate.send(());

    let events_handle = tokio::spawn(collect_events(events_rx));
    let _outcome = loop_.join().await;
    let events = events_handle.await.expect("events drain");

    let dones: Vec<(StopReason, u32)> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Done {
                stop_reason,
                turn_count,
                ..
            } => Some((*stop_reason, *turn_count)),
            _ => None,
        })
        .collect();
    assert_eq!(
        dones.len(),
        2,
        "each ask must emit its own done terminus; got {dones:?}"
    );
    assert_eq!(dones[0], (StopReason::EndTurn, 1));
    // Per-ask state reset: the second ask's Done must not inherit the
    // first ask's turn_count.
    assert_eq!(dones[1], (StopReason::EndTurn, 1));

    // Ordering: ask one's done precedes ask two's user message_start.
    let kinds: Vec<&str> = events.iter().map(kind).collect();
    let first_done = kinds
        .iter()
        .position(|k| *k == "done")
        .expect("no done event");
    let second_user_start = events
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            matches!(
                e,
                AgentEvent::MessageStart {
                    message: AgentMessage::User { .. }
                }
            )
        })
        .nth(1)
        .map(|(i, _)| i)
        .expect("no second user message_start");
    assert!(
        first_done < second_user_start,
        "done must precede the follow-up ask's message_start; kinds={kinds:?}"
    );
}

/// mu-2e0h: tool results pass through the tier-1 ingestion filter at
/// the execute_tools seam — the SAME filtered content reaches the
/// ToolCallCompleted event (log + wire) and the ToolResult message
/// (provider context). A spammy result collapses; a tool that
/// declares verbatim_result bypasses the filter entirely.
#[tokio::test]
async fn mu_2e0h_tool_result_filter_applies_unless_verbatim() {
    let spammy = "spam\nspam\nspam\nspam\ndone";
    for (verbatim, expected) in [
        (false, "spam\n[line repeated 3 more times]\ndone"),
        (true, spammy),
    ] {
        let provider = MockProvider::new(vec![
            vec![ProviderEvent::Done(assistant_tool_call(
                "t1",
                "echo",
                json!({}),
            ))],
            vec![
                ProviderEvent::TextDelta("ok".into()),
                ProviderEvent::Done(assistant_text("ok")),
            ],
        ]);
        let tool = if verbatim {
            MockTool::ok("echo", spammy).with_verbatim_result()
        } else {
            MockTool::ok("echo", spammy)
        };
        let (loop_, events_rx) = spawn_loop(provider, vec![tool], AgentConfig::default());
        loop_
            .send(AgentInput::UserMessage(user_msg("go")))
            .await
            .expect("send");
        let events_handle = tokio::spawn(collect_events(events_rx));
        let _ = loop_.join().await;
        let events = events_handle.await.expect("events drain");

        let event_content = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolCallCompleted { content, .. } => Some(content.clone()),
                _ => None,
            })
            .expect("no ToolCallCompleted");
        assert_eq!(event_content, expected, "verbatim={verbatim} (event)");

        let msg_content = events.iter().find_map(|e| match e {
            AgentEvent::MessageEnd {
                message: AgentMessage::ToolResult { content, .. },
            } => Some(content.clone()),
            _ => None,
        });
        // ToolResult messages may not surface via MessageEnd; the
        // event is the log of record. When present, it must match.
        if let Some(m) = msg_content {
            assert_eq!(m, expected, "verbatim={verbatim} (message)");
        }
    }
}

// Behavior tests (B-1..B-7 above) cover the integrated flow with
// mock providers/tools. These complement by hitting the planning
// logic directly with edge-case inputs.

fn assistant_text_msg(text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::Text { text: text.into() }],
        stop_reason: StopReason::EndTurn,
        usage: None,
    }
}

fn assistant_tool_msg(id: &str, name: &str) -> AssistantMessage {
    use crate::agent::ToolArgs;
    AssistantMessage {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: ToolArgs::new(serde_json::json!({})).unwrap(),
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
    // mu-wf5w: MaybeFinish comes FIRST so the completed ask emits its
    // `done` terminus before the buffered UM starts the next ask. The
    // pre-fix shape (External only, no MaybeFinish) suppressed the
    // terminus entirely — the client's live block never committed and
    // the whole turn was lost from scrollback.
    assert!(plan.emit_turn_end);
    assert_eq!(plan.actions.len(), 2);
    assert!(matches!(plan.actions[0], Action::MaybeFinish));
    assert!(matches!(
        plan.actions[1],
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
    use crate::agent::ToolArgs;
    let call = ToolCall {
        id: "call_under_test".to_string(),
        name: tool_name.to_string(),
        arguments: ToolArgs::new(args).unwrap(),
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
            content: vec![ContentBlock::Text { text: "ok".into() }],
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
    let loop_ = loop_with(
        Arc::new(provider),
        Arc::from("faux"),
        Arc::from("faux"),
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

/// mu-bkjr: the dispatcher's argument-aware pre-flight (`Tool::validate`)
/// must short-circuit a call BEFORE firing `InputRequired`. This pins the
/// fix for mu-20l: previously, a tool with `PermissionLevel::Ask` whose
/// validate would reject would still dispatch an approval modal, wasting
/// a click on a doomed call.
#[tokio::test]
async fn mu_bkjr_validate_rejection_short_circuits_before_input_required() {
    let provider = mock_provider_one_tool_call("gated", json!({"x": 1}));
    let tool = MockTool::ok("gated", "tool would have run but...")
        .with_policy(crate::agent::tool::ToolPolicy {
            permission: crate::agent::tool::PermissionLevel::Ask,
            ..Default::default()
        })
        .with_validate_rejection("mock validate refused: bad argument shape");
    let approvals: PendingApprovals = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let cap: SessionCapability = Arc::new(Mutex::new(crate::capability::Capability::root()));
    let (events_tx, events_rx) = mpsc::channel(64);
    let loop_ = loop_with(
        Arc::new(provider),
        Arc::from("faux"),
        Arc::from("faux"),
        vec![Arc::new(tool) as Arc<dyn Tool>],
        AgentConfig::default(),
        events_tx,
        approvals.clone(),
        cap,
    );
    loop_
        .send(AgentInput::UserMessage(user_msg("trigger gated")))
        .await
        .expect("send");
    let events_handle = tokio::spawn(collect_events(events_rx));
    let _outcome = loop_.join().await;
    let events = events_handle.await.expect("events drain");

    let mut got_input_required = false;
    let mut got_tool_completed_with_rejection = false;
    let mut got_done = false;
    for ev in &events {
        match ev {
            AgentEvent::InputRequired { .. } => got_input_required = true,
            AgentEvent::ToolCallCompleted {
                is_error, content, ..
            } => {
                if *is_error && content.contains("mock validate refused") {
                    got_tool_completed_with_rejection = true;
                }
            }
            AgentEvent::Done { .. } => got_done = true,
            _ => {}
        }
    }

    assert!(
        !got_input_required,
        "InputRequired must NOT fire when validate rejected (mu-bkjr)"
    );
    assert!(
        got_tool_completed_with_rejection,
        "ToolCallCompleted must carry the validate rejection reason"
    );
    assert!(got_done, "Done must fire after rejection");
    assert!(
        approvals.lock().unwrap().is_empty(),
        "no pending approval entries should remain — none were inserted"
    );
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
    let loop_ = loop_with(
        Arc::new(provider),
        Arc::from("faux"),
        Arc::from("faux"),
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
    let loop_ = loop_with(
        Arc::new(provider),
        Arc::from("faux"),
        Arc::from("faux"),
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
    let loop_ = loop_with(
        Arc::new(provider),
        Arc::from("faux"),
        Arc::from("faux"),
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

// ── mu-n25a: side-effects ceiling enforcement at dispatch ─────────

/// Run one scripted tool call under a given capability and return
/// `(saw_capability_callout, completed_content, completed_is_error)`.
/// The tool declares `declared` side-effects but `permission: Allow`
/// (the free-ride shape the gate must close) — so if it runs, the gate
/// failed.
async fn run_one_tool_with_side_effects(
    cap: crate::capability::Capability,
    declared: crate::agent::tool::SideEffects,
    tool_body: &str,
) -> (bool, Option<String>, bool) {
    use crate::agent::tool::{PermissionLevel, RetryPolicy, ToolPolicy};

    let provider = mock_provider_one_tool_call("effecter", json!({"x": 1}));
    let tool = MockTool::ok("effecter", tool_body).with_policy(ToolPolicy {
        side_effects: declared,
        permission: PermissionLevel::Allow, // would free-ride without the gate
        retry: RetryPolicy::ModelDecides,
        required_aws_capability: None,
        idempotent: false,
    });
    let cap: SessionCapability = Arc::new(Mutex::new(cap));
    let approvals: PendingApprovals = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let (events_tx, mut events_rx) = mpsc::channel(64);
    let loop_ = loop_with(
        Arc::new(provider),
        Arc::from("faux"),
        Arc::from("faux"),
        vec![Arc::new(tool) as Arc<dyn Tool>],
        AgentConfig::default(),
        events_tx,
        approvals,
        cap,
    );
    loop_
        .send(AgentInput::UserMessage(user_msg("call effecter")))
        .await
        .unwrap();

    let mut saw_callout = false;
    let mut completed_content: Option<String> = None;
    let mut completed_is_error = false;
    while let Some(ev) = events_rx.recv().await {
        match ev {
            AgentEvent::Callout {
                category, title, ..
            } if category == "warning" && title.contains("capability refused") => {
                saw_callout = true;
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
    (saw_callout, completed_content, completed_is_error)
}

#[tokio::test]
async fn read_only_ceiling_refuses_execute_tool() {
    use crate::agent::tool::SideEffects;
    use crate::capability::Capability;

    let cap = Capability {
        max_side_effects: Some(SideEffects::ReadOnly),
        ..Default::default()
    };
    let (callout, content, is_error) =
        run_one_tool_with_side_effects(cap, SideEffects::Execute, "DANGER: this should not run")
            .await;

    assert!(callout, "expected a capability-refused callout");
    let content = content.expect("ToolCallCompleted should fire");
    assert!(is_error, "side-effects refusal => is_error");
    assert!(
        content.contains("side-effects") && content.contains("max_side_effects"),
        "refusal should name the side-effects ceiling; got: {content}"
    );
    assert!(
        !content.contains("DANGER"),
        "the Execute tool body must NOT have executed; got: {content}"
    );
}

#[tokio::test]
async fn read_only_ceiling_refuses_mutating_tool() {
    use crate::agent::tool::SideEffects;
    use crate::capability::Capability;

    let cap = Capability {
        max_side_effects: Some(SideEffects::ReadOnly),
        ..Default::default()
    };
    let (callout, content, is_error) =
        run_one_tool_with_side_effects(cap, SideEffects::Mutating, "wrote a file").await;

    assert!(callout, "expected a capability-refused callout");
    let content = content.expect("ToolCallCompleted should fire");
    assert!(is_error, "Mutating > ReadOnly ceiling => refused");
    assert!(
        !content.contains("wrote a file"),
        "the Mutating tool body must NOT have executed; got: {content}"
    );
}

#[tokio::test]
async fn root_ceiling_allows_execute_and_mutating_tools() {
    use crate::agent::tool::SideEffects;
    use crate::capability::Capability;

    // root() has max_side_effects: None (unrestricted) — back-compat.
    let (callout, content, is_error) =
        run_one_tool_with_side_effects(Capability::root(), SideEffects::Execute, "exec ran").await;
    assert!(!callout, "root session must NOT be refused");
    assert!(!is_error, "root ceiling allows Execute");
    assert_eq!(content.as_deref(), Some("exec ran"));

    let (callout, content, is_error) =
        run_one_tool_with_side_effects(Capability::root(), SideEffects::Mutating, "mutated").await;
    assert!(!callout, "root session must NOT be refused");
    assert!(!is_error, "root ceiling allows Mutating");
    assert_eq!(content.as_deref(), Some("mutated"));
}

#[tokio::test]
async fn mutating_ceiling_allows_mutating_but_refuses_execute() {
    use crate::agent::tool::SideEffects;
    use crate::capability::Capability;

    let mutating_cap = || Capability {
        max_side_effects: Some(SideEffects::Mutating),
        ..Default::default()
    };

    // At the ceiling: allowed.
    let (_, content, is_error) =
        run_one_tool_with_side_effects(mutating_cap(), SideEffects::Mutating, "edit ok").await;
    assert!(!is_error, "Mutating == ceiling is allowed");
    assert_eq!(content.as_deref(), Some("edit ok"));

    // Above the ceiling: refused.
    let (callout, content, is_error) =
        run_one_tool_with_side_effects(mutating_cap(), SideEffects::Execute, "nope").await;
    assert!(callout && is_error, "Execute > Mutating ceiling => refused");
    assert!(!content.unwrap().contains("nope"));
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
    let loop_ = loop_with(
        Arc::new(provider),
        Arc::from("faux"),
        Arc::from("faux"),
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
    let loop_ = loop_with(
        provider,
        Arc::from("faux"),
        Arc::from("faux"),
        tools,
        config,
        events_tx,
        approvals,
        capability,
    );
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
                span_id: s.id.to_string(),
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
        input: MessageInput<'_>,
        tools: &[ToolSpec],
        cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        self.inner
            .stream(system_prompt, input, tools, cancel_rx)
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
    let loop_ = loop_with(
        provider,
        Arc::from("faux"),
        Arc::from("faux"),
        vec![],
        config,
        events_tx,
        approvals,
        capability,
    );
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
        compaction_policy_override: None,
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
        decisions,
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
        !decisions.is_empty(),
        "evict-half drops at least one span on a non-trivial rope; got {} decisions",
        decisions.len()
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
        compaction_policy_override: None,
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

/// mu-8bkf proof test — HashAndSummaryPolicy (with KeepHalfJudge canned judge,
/// no model spend) fires a CompactionAssembly with policy_id matching
/// HashAndSummaryPolicy::policy_label() == DEFAULT_POLICY_ID.
///
/// Uses compaction_policy_override so the test bypasses the serve path's
/// resolve_compaction_policy and directly exercises the loop's compaction
/// dispatch with the wired policy type.  The serve-path selector arm
/// is tested separately in mu_coding::serve::handlers::session::tests.
///
/// Two-turn structure (mu-8bkf fix-round): HashAndSummaryPolicy.is_async()==true,
/// so the loop SPAWNS the compact() call as a background task in turn 1 and
/// only picks up the CompactionAssembly result at the START of turn 2 via
/// bg_compaction.try_take().  A single-turn test can therefore never observe
/// the event.  We drive two turns by watching for TurnEnd on the event stream
/// and only then sending the second UserMessage, guaranteeing it arrives after
/// the loop has gone back to blocking recv (i.e., truly a separate turn).
/// KeepHalfJudge is pure-CPU / synchronous, so the background tokio task
/// completes well before the loop begins turn 2.
#[tokio::test]
async fn mu_8bkf_hash_and_summary_policy_fires_compaction_assembly_with_correct_policy_id() {
    use crate::context::compaction::bench::KeepHalfJudge;
    use crate::context::compaction::hash_summary::{HashAndSummaryPolicy, DEFAULT_POLICY_ID};

    let judge = Arc::new(KeepHalfJudge::new());
    let hash_and_summary: Arc<dyn crate::context::CompactionPolicy> =
        Arc::new(HashAndSummaryPolicy::new(judge));

    // Two scripted responses: turn 1 triggers bg compaction; turn 2 picks
    // it up and fires CompactionAssembly before calling the provider.
    let inner = MockProvider::new(vec![
        vec![
            ProviderEvent::TextDelta("response one".into()),
            ProviderEvent::Done(assistant_text("response one")),
        ],
        vec![
            ProviderEvent::TextDelta("response two".into()),
            ProviderEvent::Done(assistant_text("response two")),
        ],
    ]);
    let provider: Arc<dyn Provider> = Arc::new(MockProviderWithCompaction {
        inner,
        // MockProviderWithCompaction.compaction_policy() drives the label
        // returned by the loop's try_take() path (line ~1034 of loop_/mod.rs:
        // `provider.compaction_policy().policy_label()`).  Wire the same
        // hash_and_summary so that path also returns DEFAULT_POLICY_ID.
        policy: Arc::clone(&hash_and_summary),
    });

    let config = AgentConfig {
        // Threshold of 1 token → any non-empty rope crosses it immediately.
        compaction_threshold: Some(1),
        compaction_policy_override: Some(Arc::clone(&hash_and_summary)),
        ..AgentConfig::default()
    };

    let (events_tx, events_rx) = mpsc::channel(64);
    let approvals: PendingApprovals = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let capability: SessionCapability = Arc::new(Mutex::new(crate::capability::Capability::root()));
    let loop_ = loop_with(
        provider,
        Arc::from("faux"),
        Arc::from("faux"),
        vec![],
        config,
        events_tx,
        approvals,
        capability,
    );

    // Send message 1 immediately.
    loop_
        .send(AgentInput::UserMessage(user_msg(
            "first message: long enough to guarantee at least one token and trigger compaction",
        )))
        .await
        .expect("send first");

    // Spawn a task that watches the event stream for TurnEnd (turn 1
    // complete) and then sends message 2 on the loop's input channel.
    // This ensures message 2 arrives AFTER the loop has returned to
    // blocking recv — guaranteeing a genuine second turn.
    let loop_tx = loop_.sender();
    let watcher = tokio::spawn(async move {
        let mut rx = events_rx;
        let mut events = Vec::new();
        let mut loop_tx_opt = Some(loop_tx);
        while let Some(e) = rx.recv().await {
            if let Some(tx) = loop_tx_opt.take() {
                if matches!(e, AgentEvent::TurnEnd) {
                    // Turn 1 is done; send message 2, then DROP the sender
                    // so the loop sees channel-close after processing msg 2.
                    let _ = tx
                        .send(AgentInput::UserMessage(user_msg(
                            "second message: turn 2 picks up bg compaction",
                        )))
                        .await;
                    // tx is dropped here — loop_tx_opt.take() consumed it.
                } else {
                    // TurnEnd not yet; put the sender back.
                    loop_tx_opt = Some(tx);
                }
            }
            events.push(e);
        }
        events
    });

    let outcome = timeout(Duration::from_secs(5), loop_.join())
        .await
        .expect("loop did not complete within 5 seconds");
    let events = watcher.await.expect("watcher drain");

    assert_eq!(outcome, Outcome::Done(StopReason::EndTurn));

    // Turn 2 must have picked up the bg compaction result and emitted
    // CompactionAssembly.  Collect all such events — we expect at least one.
    let compaction_events: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::CompactionAssembly { .. }))
        .collect();

    assert!(
        !compaction_events.is_empty(),
        "expected at least one CompactionAssembly event after two turns with \
         HashAndSummaryPolicy+threshold=1; saw zero.  Events: {:?}",
        events.iter().map(kind).collect::<Vec<_>>(),
    );

    for ev in &compaction_events {
        if let AgentEvent::CompactionAssembly { policy_id, .. } = ev {
            assert_eq!(
                policy_id.as_str(),
                DEFAULT_POLICY_ID,
                "CompactionAssembly policy_id must match HashAndSummaryPolicy::policy_label()"
            );
        }
    }

    // The policy_label static value must match the constant.
    assert_eq!(
        hash_and_summary.policy_label(),
        DEFAULT_POLICY_ID,
        "HashAndSummaryPolicy::policy_label() must equal DEFAULT_POLICY_ID"
    );
}

// ============================================================================
// mu-yqeq.8 Phase D smoke test
//
// Spec mu-044 §"Phase D end-to-end smoke test" calls for a recorded
// fixture-driven smoke covering the behavioral surface of the
// cutover. The recorded-fixture path is deferred (filed as a
// follow-up bead — no fixture infrastructure in-tree yet); this
// inline smoke covers the spec's core requirement: confirm the
// agent loop now invokes provider.stream with MessageInput::Projected
// (not Legacy), and that the structural decisions of a tool-call
// round-trip survive the cutover.
// ============================================================================

/// A Provider wrapper that records the MessageInput variant + a
/// structural snapshot of each stream() call's input. Otherwise
/// behaves like MockProvider — pops scripted events from a queue.
struct RecordingProvider {
    responses: Mutex<VecDeque<Vec<ProviderEvent>>>,
    records: Arc<Mutex<Vec<InputRecord>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InputVariant {
    Legacy,
    Projected,
}

#[derive(Debug, Clone)]
struct InputRecord {
    variant: InputVariant,
    /// Number of messages in the projection (or legacy slice).
    message_count: usize,
    /// First five source span ids in the projection — a structural
    /// fingerprint that catches reshape regressions.
    first_span_ids: Vec<String>,
}

impl RecordingProvider {
    fn new(responses: Vec<Vec<ProviderEvent>>) -> (Arc<Self>, Arc<Mutex<Vec<InputRecord>>>) {
        let records = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(Self {
            responses: Mutex::new(responses.into_iter().collect()),
            records: records.clone(),
        });
        (provider, records)
    }
}

#[async_trait]
impl Provider for RecordingProvider {
    async fn stream(
        &self,
        _system_prompt: Option<&str>,
        input: MessageInput<'_>,
        _tools: &[ToolSpec],
        _cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let record = match input {
            MessageInput::Legacy(msgs) => InputRecord {
                variant: InputVariant::Legacy,
                message_count: msgs.len(),
                first_span_ids: Vec::new(),
            },
            MessageInput::Projected(pmsgs) => InputRecord {
                variant: InputVariant::Projected,
                message_count: pmsgs.messages.len(),
                first_span_ids: pmsgs
                    .messages
                    .iter()
                    .take(5)
                    .filter_map(|m| m.source_span_ids().first().map(|s| s.as_ref().to_string()))
                    .collect(),
            },
        };
        self.records.lock().expect("records mutex").push(record);

        let chunk = self.responses.lock().expect("mutex poisoned").pop_front();
        Ok(Box::pin(stream::iter(chunk.unwrap_or_default())))
    }
}

/// mu-yqeq.8: a tool-call round-trip survives the cutover. Two
/// provider calls fire (assistant tool-call → tool result → assistant
/// text); BOTH receive MessageInput::Projected (not Legacy), and the
/// projection's structural shape grows by exactly the expected number
/// of new spans between turns (one for each new AgentMessage). This
/// is the inline-fixture stand-in for the recorded-smoke acceptance
/// in spec mu-044 §"Phase D end-to-end smoke test".
#[tokio::test]
async fn yqeq8_phase_d_smoke_tool_call_round_trip_uses_projected_path() {
    let (provider, records) = RecordingProvider::new(vec![
        // Turn 1: assistant calls a tool.
        vec![ProviderEvent::Done(assistant_tool_call(
            "t1",
            "echo",
            json!({"x": 1}),
        ))],
        // Turn 2: assistant text → end.
        vec![
            ProviderEvent::TextDelta("done".into()),
            ProviderEvent::Done(assistant_text("done")),
        ],
    ]);
    let tools = vec![MockTool::ok("echo", "echoed")];
    let tools_arc: Vec<Arc<dyn Tool>> = tools
        .into_iter()
        .map(|t| Arc::new(t) as Arc<dyn Tool>)
        .collect();
    let (events_tx, events_rx) = mpsc::channel(64);
    let approvals: PendingApprovals = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let capability: SessionCapability = Arc::new(Mutex::new(crate::capability::Capability::root()));
    let loop_ = loop_with(
        provider as Arc<dyn Provider>,
        Arc::from("faux"),
        Arc::from("faux"),
        tools_arc,
        AgentConfig::default(),
        events_tx,
        approvals,
        capability,
    );

    loop_
        .send(AgentInput::UserMessage(user_msg("call the tool")))
        .await
        .expect("send");
    let events_handle = tokio::spawn(collect_events(events_rx));
    let outcome = loop_.join().await;
    let _events = events_handle.await.expect("events drain");

    assert_eq!(outcome, Outcome::Done(StopReason::EndTurn));

    let records = records.lock().expect("records mutex").clone();

    // Both provider calls fired (one per turn).
    assert_eq!(
        records.len(),
        2,
        "expected exactly 2 provider calls (assistant tool-call + assistant text)",
    );

    // EVERY call received MessageInput::Projected — the Phase D
    // cutover invariant.
    for (i, rec) in records.iter().enumerate() {
        assert_eq!(
            rec.variant,
            InputVariant::Projected,
            "provider call {i}: expected MessageInput::Projected, got {:?}",
            rec.variant,
        );
    }

    // Turn 1 projection: tool-schema span + user message → 2 spans.
    // assemble_rope inserts a span per ToolSpec before the message
    // spans (mu-ktq); the "echo" tool spec becomes "tool-schema:echo".
    assert_eq!(
        records[0].message_count, 2,
        "turn 1 projection should carry tool-schema(echo) + user message",
    );
    assert_eq!(
        records[0].first_span_ids,
        vec!["tool-schema:echo", "msg-0-user"],
    );

    // Turn 2 projection: tool-schema + user + assistant(tool_call) +
    // tool_result → 4 spans. Assemble_rope appends each new
    // AgentMessage as a span with id `msg-{idx}-{kind}`; tool-result
    // spans carry the call_id in the id (`msg-{idx}-tool-result:{call_id}`).
    assert_eq!(
        records[1].message_count, 4,
        "turn 2 projection should carry the tool-call round-trip context",
    );
    assert_eq!(
        records[1].first_span_ids,
        vec![
            "tool-schema:echo",
            "msg-0-user",
            "msg-1-assistant",
            "msg-2-tool-result:t1",
        ],
    );
}

// =============================================================================
// mu-wsgx: feedback-predictor compaction trigger
// =============================================================================
//
// The raw renderer estimate ran ~15% low on Anthropic (uncounted
// request framing + chars/4 vs BPE), so the 150K trigger fired at
// ~176K actual provider tokens. The trigger measure is now
// `predicted_prompt_total`: the provider's exact reported total for
// the previous call plus the renderer-estimate delta of new spans.

#[test]
fn wsgx_predictor_without_anchor_is_estimate_plus_overhead() {
    assert_eq!(
        predicted_prompt_total(None, 10_000),
        10_000 + ESTIMATE_FALLBACK_OVERHEAD_TOKENS
    );
}

#[test]
fn wsgx_predictor_anchors_on_actual_plus_delta() {
    let anchor = FeedbackAnchor {
        actual_prompt_total: 170_000,
        rope_estimate: 150_000,
    };
    // 2K of new spans since the anchor call → actual + 2K.
    assert_eq!(predicted_prompt_total(Some(&anchor), 152_000), 172_000);
    // Unchanged rope → exactly the provider's number.
    assert_eq!(predicted_prompt_total(Some(&anchor), 150_000), 170_000);
}

#[test]
fn wsgx_predictor_shrunk_rope_falls_back_to_estimate() {
    // A compaction landed between calls: the rope now estimates BELOW
    // the anchor's estimate, so the anchor's (pre-compaction) actual
    // would over-predict and re-trigger a compaction storm. Fall back.
    let anchor = FeedbackAnchor {
        actual_prompt_total: 170_000,
        rope_estimate: 150_000,
    };
    assert_eq!(
        predicted_prompt_total(Some(&anchor), 80_000),
        80_000 + ESTIMATE_FALLBACK_OVERHEAD_TOKENS
    );
}

/// End-to-end: provider-reported usage drives the trigger. Call 1's
/// response reports a 200K prompt total (way above the tiny rope's
/// raw estimate). The tool follow-up call must therefore cross the
/// 150K threshold and fire compaction — even though the raw estimate
/// never gets anywhere near it. Raw-estimate triggering would NEVER
/// fire here; only the feedback anchor can.
#[tokio::test]
async fn wsgx_provider_feedback_triggers_compaction_raw_estimate_would_not() {
    let mut call1 = assistant_tool_call("c1", "echo", serde_json::json!({}));
    call1.usage = Some(Usage {
        input_tokens: 200_000,
        output_tokens: 5,
        ..Default::default()
    });
    let inner = MockProvider::new(vec![
        vec![ProviderEvent::Done(call1)],
        vec![
            ProviderEvent::TextDelta("done".into()),
            ProviderEvent::Done(assistant_text("done")),
        ],
    ]);
    let provider: Arc<dyn Provider> = Arc::new(MockProviderWithCompaction {
        inner,
        policy: Arc::new(EvictHalfPolicy),
    });
    let config = AgentConfig {
        compaction_threshold: Some(150_000),
        ..AgentConfig::default()
    };

    let (events_tx, events_rx) = mpsc::channel(64);
    let approvals: PendingApprovals = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let capability: SessionCapability = Arc::new(Mutex::new(crate::capability::Capability::root()));
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(MockTool::ok("echo", "echoed"))];
    let loop_ = loop_with(
        provider,
        Arc::from("faux"),
        Arc::from("faux"),
        tools,
        config,
        events_tx,
        approvals,
        capability,
    );

    loop_
        .send(AgentInput::UserMessage(user_msg("use the tool")))
        .await
        .expect("send");
    let events_handle = tokio::spawn(collect_events(events_rx));
    let outcome = loop_.join().await;
    let events = events_handle.await.expect("events drain");

    assert_eq!(outcome, Outcome::Done(StopReason::EndTurn));

    let compactions: Vec<u32> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::CompactionAssembly { model_call_id, .. } => Some(*model_call_id),
            _ => None,
        })
        .collect();
    assert_eq!(
        compactions,
        vec![2],
        "exactly one compaction, on the tool follow-up call, anchored \
         on call 1's 200K reported prompt total; events: {:?}",
        events.iter().map(kind).collect::<Vec<_>>(),
    );
}

// ============================================================================
// mu-036 Phase C (mu-7zn): schedule_wakeup + RunMode::Sleeping
// ============================================================================
//
// These exercise the agent loop's wakeup-parking path driven by
// AgentInput::ScheduleWakeup. They verify the spec's Wake-up tests:
//   - parks the session and resumes the autonomous run at iteration N+1
//     with the wake reason as that iteration's motivation (INV-5/INV-6)
//   - no provider call fires before the park (the iteration that chose
//     to sleep does no model work)
//   - bounds are re-checked on wake: a session that slept past
//     max_wall_clock_ms terminates with WallClockExpired instead of
//     resuming (INV-2/INV-5)
//   - schedule_wakeup outside autonomous mode is declined with a
//     warning callout rather than improvising a new semantic

fn autonomy_allowed_wakeup(
    max_iter: u32,
    max_wall_clock_ms: u64,
) -> crate::capability::AutonomyCapability {
    crate::capability::AutonomyCapability::Allowed {
        max_iterations: max_iter,
        max_wall_clock_ms,
        max_total_tool_calls_in_autonomy: 100,
        allow_schedule_wakeup: true,
        allow_delegate_grader: false,
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Drain events, holding the loop alive, until `AutonomousTerminated`
/// (inclusive) or the events channel closes. Keeping the loop's input
/// sender alive is what lets the in-flight `schedule_wakeup` sleep fire
/// naturally rather than being interrupted by channel-close.
async fn collect_until_terminated(rx: &mut mpsc::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        let stop = matches!(ev, AgentEvent::AutonomousTerminated { .. });
        events.push(ev);
        if stop {
            break;
        }
    }
    events
}

/// C-1: schedule_wakeup parks the autonomous loop and resumes at N+1.
///
/// Capability allows wakeup, max_iterations: 2, generous wall-clock.
/// Iteration 1 immediately schedules a short wakeup (before any model
/// call). The loop brackets iteration 1 with a Completed, emits
/// AutonomousScheduledWakeup, parks on a real (short) sleep, then wakes
/// and resumes at iteration 2 with the wake reason as the motivation.
/// Iteration 2 runs the provider once and the run terminates at the
/// iteration cap.
#[tokio::test]
async fn c1_schedule_wakeup_parks_and_resumes_at_n_plus_1() {
    let provider = MockProvider::forever(vec![ProviderEvent::Done(assistant_text("working"))]);
    let (loop_, mut events_rx) = spawn_loop_with_autonomy(
        provider,
        vec![],
        AgentConfig::default(),
        autonomy_allowed_wakeup(2, 60_000),
    );

    loop_
        .send(AgentInput::StartAutonomous {
            goal: "watch then continue".to_owned(),
            options: crate::protocol::AutonomyOptions::default(),
        })
        .await
        .expect("send start");
    let wake_at = now_unix_ms() + 50;
    let reason = "recheck CI status".to_owned();
    loop_
        .send(AgentInput::ScheduleWakeup {
            wake_at_unix_ms: wake_at,
            reason: reason.clone(),
        })
        .await
        .expect("send wakeup");

    let events = timeout(
        Duration::from_secs(5),
        collect_until_terminated(&mut events_rx),
    )
    .await
    .expect("loop terminated within timeout");
    drop(loop_);

    let kinds: Vec<&str> = events.iter().map(kind).collect();

    // Exactly one park marker.
    let wakeups: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::AutonomousScheduledWakeup { .. }))
        .collect();
    assert_eq!(
        wakeups.len(),
        1,
        "expected one scheduled-wakeup; got {kinds:?}"
    );
    match wakeups[0] {
        AgentEvent::AutonomousScheduledWakeup {
            wake_at_unix_ms,
            reason: r,
        } => {
            assert_eq!(*wake_at_unix_ms, wake_at, "wake time round-trips");
            assert_eq!(r, &reason, "wake reason round-trips");
        }
        other => panic!("unexpected: {other:?}"),
    }

    // Two iteration starts; the second's motivation is the wake reason.
    let starts: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::AutonomousIterationStarted { .. }))
        .collect();
    assert_eq!(
        starts.len(),
        2,
        "expected 2 iteration starts; got {kinds:?}"
    );
    match starts[1] {
        AgentEvent::AutonomousIterationStarted {
            iteration,
            motivation,
        } => {
            assert_eq!(*iteration, 2, "resumes at iteration 2");
            assert_eq!(
                motivation, &reason,
                "wake reason becomes iteration 2's motivation"
            );
        }
        other => panic!("unexpected: {other:?}"),
    }

    // INV-6: iteration 1 is bracketed with a Completed before the park.
    let completes: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::AutonomousIterationCompleted { .. }))
        .collect();
    assert!(
        completes.len() >= 2,
        "both iterations completed; got {kinds:?}"
    );

    // INV-5 proxy: no provider call (TurnStart) fires before the park —
    // the iteration that scheduled the wakeup did no model work.
    let wakeup_idx = kinds
        .iter()
        .position(|k| *k == "autonomous_scheduled_wakeup")
        .expect("scheduled wakeup present");
    let first_turn_idx = kinds.iter().position(|k| *k == "turn_start");
    if let Some(turn_idx) = first_turn_idx {
        assert!(
            turn_idx > wakeup_idx,
            "first provider TurnStart must come AFTER the park (no model \
             work during the sleeping iteration); kinds={kinds:?}"
        );
    }

    // Ordering: park marker precedes the resumed iteration's start.
    let second_start_idx = kinds
        .iter()
        .enumerate()
        .filter(|(_, k)| **k == "autonomous_iteration_started")
        .map(|(i, _)| i)
        .nth(1)
        .expect("two iteration starts");
    assert!(
        wakeup_idx < second_start_idx,
        "scheduled wakeup precedes the resumed iteration start; kinds={kinds:?}"
    );

    // INV-7: terminates at the iteration cap, and that is the last
    // autonomy-namespace event.
    let last = last_autonomy_event(&events).expect("at least one autonomy event");
    match last {
        AgentEvent::AutonomousTerminated { reason } => assert!(
            matches!(reason, AutonomousTerminationReason::IterationCap),
            "expected IterationCap, got {reason:?}"
        ),
        other => panic!("INV-7: last autonomy event must be terminate; got {other:?}"),
    }
}

/// C-2: bounds are re-checked on wake — a session that sleeps past
/// max_wall_clock_ms terminates with WallClockExpired instead of
/// resuming. With max_wall_clock_ms: 1 and a ~50ms sleep, the wake-time
/// bound check trips before iteration 2 ever starts.
#[tokio::test]
async fn c2_wakeup_past_wall_clock_terminates_on_wake() {
    let provider = MockProvider::forever(vec![ProviderEvent::Done(assistant_text("working"))]);
    let (loop_, mut events_rx) = spawn_loop_with_autonomy(
        provider,
        vec![],
        AgentConfig::default(),
        autonomy_allowed_wakeup(10, 1),
    );

    loop_
        .send(AgentInput::StartAutonomous {
            goal: "sleep past the wall clock".to_owned(),
            options: crate::protocol::AutonomyOptions::default(),
        })
        .await
        .expect("send start");
    loop_
        .send(AgentInput::ScheduleWakeup {
            wake_at_unix_ms: now_unix_ms() + 50,
            reason: "should never resume".to_owned(),
        })
        .await
        .expect("send wakeup");

    let events = timeout(
        Duration::from_secs(5),
        collect_until_terminated(&mut events_rx),
    )
    .await
    .expect("loop terminated within timeout");
    drop(loop_);

    let kinds: Vec<&str> = events.iter().map(kind).collect();

    // Parked once, but never resumed: exactly one iteration start.
    let starts = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::AutonomousIterationStarted { .. }))
        .count();
    assert_eq!(
        starts, 1,
        "must not resume after wall-clock trips; got {kinds:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::AutonomousScheduledWakeup { .. })),
        "parked once; got {kinds:?}"
    );

    let last = last_autonomy_event(&events).expect("at least one autonomy event");
    match last {
        AgentEvent::AutonomousTerminated { reason } => assert!(
            matches!(reason, AutonomousTerminationReason::WallClockExpired),
            "expected WallClockExpired on wake, got {reason:?}"
        ),
        other => panic!("expected terminate; got {other:?}"),
    }
}

/// C-3: schedule_wakeup outside autonomous mode is declined with a
/// warning callout (spec-boundary discipline — no improvised
/// sleep-then-idle semantic), and does not park the session.
#[tokio::test]
async fn c3_schedule_wakeup_outside_autonomous_is_declined() {
    let provider = MockProvider::forever(vec![ProviderEvent::Done(assistant_text("idle"))]);
    let (loop_, events_rx) = spawn_loop_with_autonomy(
        provider,
        vec![],
        AgentConfig::default(),
        autonomy_allowed_wakeup(5, 60_000),
    );

    loop_
        .send(AgentInput::ScheduleWakeup {
            wake_at_unix_ms: now_unix_ms() + 1_000,
            reason: "no autonomous run in progress".to_owned(),
        })
        .await
        .expect("send wakeup");

    let events_handle = tokio::spawn(collect_events(events_rx));
    let _ = loop_.join().await;
    let events = events_handle.await.expect("events drain");

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::AutonomousScheduledWakeup { .. })),
        "must not park when not autonomous; kinds={:?}",
        events.iter().map(kind).collect::<Vec<_>>()
    );
    let declined = events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::Callout { title, .. } if title == "schedule_wakeup ignored"
        )
    });
    assert!(
        declined,
        "expected a 'schedule_wakeup ignored' warning callout; kinds={:?}",
        events.iter().map(kind).collect::<Vec<_>>()
    );
}

// ============================================================================
// mu-watch-tool-wakeup-o03p: AgentInput::WatchCompleted
// ============================================================================
//
// The EVENT sibling of ScheduleWakeup's TIMER. A finished watch wakes an
// IDLE session over the same input channel: the loop synthesizes a user
// message carrying the watch result INLINE and runs the LLM, so the
// result lands as the woken turn's motivation (no autonomous run, no
// go-read-your-mailbox indirection).

/// W-1: a WatchCompleted input wakes an idle session, injects the result
/// as a user message, and drives exactly one LLM turn.
#[tokio::test]
async fn w1_watch_completed_wakes_idle_session_with_inline_result() {
    let provider = MockProvider::new(vec![vec![
        ProviderEvent::TextDelta("on it".into()),
        ProviderEvent::Done(assistant_text("on it")),
    ]]);
    let (loop_, events_rx) = spawn_loop(provider, vec![], AgentConfig::default());

    loop_
        .send(AgentInput::WatchCompleted {
            note: "CI for PR 42".to_owned(),
            summary: "Exit status: 0\nstdout:\nall checks passed".to_owned(),
        })
        .await
        .expect("send watch-completed");

    let events_handle = tokio::spawn(collect_events(events_rx));
    let outcome = loop_.join().await;
    let events = events_handle.await.expect("events drain");

    assert_eq!(outcome, Outcome::Done(StopReason::EndTurn));

    // The synthesized wake message carries the note + summary inline.
    let woke_with_result = events.iter().any(|e| match e {
        AgentEvent::MessageStart {
            message: AgentMessage::User { content },
        } => content.contains("CI for PR 42") && content.contains("all checks passed"),
        _ => false,
    });
    assert!(
        woke_with_result,
        "watch result must be injected inline as a user message; kinds={:?}",
        events.iter().map(kind).collect::<Vec<_>>()
    );

    // The wake drove exactly one provider turn (the loop actually ran the
    // LLM rather than just buffering the input).
    let turns = events.iter().filter(|e| kind(e) == "turn_start").count();
    assert_eq!(turns, 1, "exactly one LLM turn after the wake");
    if let Some(AgentEvent::Done { turn_count, .. }) = events.last() {
        assert_eq!(*turn_count, 1);
    } else {
        panic!("last event must be Done");
    }
}
