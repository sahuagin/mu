//! Queue-driven agent loop.
//!
//! See spec mu-003 for the full design. Briefly:
//!
//! - The loop processes `Action`s from a `VecDeque`.
//! - External callers push `AgentInput` via `AgentLoop::send`; the
//!   loop wraps them as `Action::External` and processes in order.
//! - Long-running actions (`InvokeLlm`, `ExecuteTools`) `select!`
//!   between their own work and `input_rx.recv()`, buffering
//!   `UserMessage`s for later and short-circuiting on `Cancel`.
//! - Termination via no-tool-calls assistant message, iteration cap,
//!   `Cancel`, or unrecoverable error.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::provider::{Provider, ProviderEvent};
use super::types::Usage;
use super::tool::{RetryPolicy, Tool, ToolResult, ToolSpec};
use super::types::{AgentMessage, AssistantMessage, ContentBlock, StopReason, ToolCall};

/// External inputs callers push to a running agent loop.
#[derive(Debug, Clone)]
pub enum AgentInput {
    /// Add a message to the conversation. Loop runs the LLM after.
    UserMessage(AgentMessage),
    /// Stop. In-flight provider stream and tool execution are
    /// cancelled; loop returns `Outcome::Cancelled`.
    Cancel,
}

/// Output events emitted by the loop. Mirrors mu-001's `session.*`
/// notifications in shape; mu-coding does the typed-enum → JSON-RPC
/// translation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    AgentStart,
    TurnStart,
    MessageStart {
        message: AgentMessage,
    },
    TextDelta {
        delta: String,
    },
    ToolCallStarted {
        tool_call_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },
    ToolCallCompleted {
        tool_call_id: String,
        content: String,
        is_error: bool,
    },
    MessageEnd {
        message: AgentMessage,
    },
    TurnEnd,
    Done {
        stop_reason: StopReason,
        turn_count: u32,
        /// Aggregated token usage across this ask_session's turns.
        /// `None` if no provider in the chain reported usage.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        /// Wall time from the first turn's start to this Done emit.
        /// Captures multi-turn tool-use loops; resets per ask_session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        elapsed_ms: Option<u64>,
    },
    Error {
        message: String,
    },
    /// Catch-all "the agent says something notable" event. See spec
    /// mu-016. Free-form `category`/`theme`. The forwarder
    /// translates to `session.callout` notifications, where this
    /// field becomes the wire-level `kind`.
    ///
    /// (We use `category` here because AgentEvent's serde tag is
    /// already named `kind` — the discriminator. The wire surface
    /// in mu-001's `CalloutEvent` keeps the user-facing `kind` name.)
    Callout {
        category: String,
        title: String,
        /// Either a JSON string (text body) or any structured value.
        /// Body shape is preserved end-to-end; the wire layer (mu-001's
        /// `CalloutBody`) interprets it as Text-or-Structured at
        /// translation time.
        body: serde_json::Value,
        theme: Option<String>,
        context_refs: Vec<String>,
    },
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Cap on assistant-message turns. The loop emits
    /// `AgentEvent::Done(EndTurn)` and returns `Outcome::IterationCap`
    /// when this is reached. Default 20.
    pub max_turns: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self { max_turns: 20 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Done(StopReason),
    IterationCap,
    Cancelled,
    Error(String),
}

/// Internal action queue. Callers push `AgentInput` via `AgentLoop::send`;
/// the run function wraps `AgentInput::UserMessage` as
/// `Action::External(...)` and pushes it to the queue. Internal state
/// transitions (`InvokeLlm`, `ExecuteTools`, `MaybeFinish`) are private.
#[derive(Debug)]
enum Action {
    External(AgentInput),
    InvokeLlm,
    ExecuteTools(Vec<ToolCall>),
    MaybeFinish,
}

// ============================================================================
// Pure planners
// ============================================================================
//
// Logic between queue-mediated steps gets extracted as pure functions
// here. The async I/O parts of the loop call these to decide what to
// queue / emit, then perform the side effects themselves.
//
// Tests can target the planners directly without async machinery — the
// queue-flow integration is covered by the existing behavior tests
// (B-1..B-7) using mock providers and tools.

/// Output of `plan_post_invoke_llm`.
struct PostInvokeLlmPlan {
    /// True iff the loop should emit `AgentEvent::TurnEnd` before
    /// pushing actions. False when tool calls are queued — TurnEnd
    /// then gets emitted by `plan_post_execute_tools` after tools
    /// complete.
    emit_turn_end: bool,
    /// Actions to push to the back of the queue, in order.
    actions: Vec<Action>,
}

/// Decide what to do after the assistant message comes back from a
/// successful `InvokeLlm`. Pure — given the assistant message and any
/// `UserMessage`s that were buffered during the LLM stream, produces
/// the actions to enqueue and whether to emit TurnEnd.
fn plan_post_invoke_llm(
    assistant_msg: &AssistantMessage,
    buffered: Vec<AgentInput>,
) -> PostInvokeLlmPlan {
    let tool_calls: Vec<ToolCall> = assistant_msg
        .content
        .iter()
        .filter_map(|c| match c {
            ContentBlock::ToolCall(tc) => Some(tc.clone()),
            _ => None,
        })
        .collect();

    let had_buffered = !buffered.is_empty();
    let mut actions = Vec::new();

    if tool_calls.is_empty() {
        // No tool calls — TurnEnd here, then drain buffered. Push
        // MaybeFinish only if no buffered UMs; if there ARE buffered
        // ones, their handlers will push InvokeLlm and the loop
        // continues naturally.
        for input in buffered {
            actions.push(Action::External(input));
        }
        if !had_buffered {
            actions.push(Action::MaybeFinish);
        }
        PostInvokeLlmPlan {
            emit_turn_end: true,
            actions,
        }
    } else {
        // Tool calls — defer TurnEnd until after ExecuteTools.
        actions.push(Action::ExecuteTools(tool_calls));
        for input in buffered {
            actions.push(Action::External(input));
        }
        PostInvokeLlmPlan {
            emit_turn_end: false,
            actions,
        }
    }
}

/// Decide what to enqueue after `ExecuteTools` completes. Pure.
/// Buffered UMs come first so they land in `messages` before the
/// next InvokeLlm runs.
fn plan_post_execute_tools(buffered: Vec<AgentInput>) -> Vec<Action> {
    let mut actions = Vec::with_capacity(buffered.len() + 1);
    for input in buffered {
        actions.push(Action::External(input));
    }
    actions.push(Action::InvokeLlm);
    actions
}

/// Pure dedup check: should we push `InvokeLlm` after processing a
/// UserMessage? Yes unless one is already queued (back-to-back UMs
/// share one LLM call).
fn should_push_invoke_llm(queue: &VecDeque<Action>) -> bool {
    !queue.iter().any(|a| matches!(a, Action::InvokeLlm))
}

/// Handle to a running agent loop.
#[derive(Debug)]
pub struct AgentLoop {
    tx: mpsc::Sender<AgentInput>,
    handle: JoinHandle<Outcome>,
}

impl AgentLoop {
    /// Spawn a new agent loop on the current tokio runtime.
    pub fn spawn(
        provider: Arc<dyn Provider>,
        tools: Vec<Arc<dyn Tool>>,
        config: AgentConfig,
        events: mpsc::Sender<AgentEvent>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(32);
        let handle = tokio::spawn(run(provider, tools, config, events, rx));
        Self { tx, handle }
    }

    /// Push input. Returns `Err` with the input if the loop has terminated.
    pub async fn send(&self, input: AgentInput) -> Result<(), AgentInput> {
        self.tx.send(input).await.map_err(|e| e.0)
    }

    /// Clone the input sender. Used by the daemon's session manager
    /// to drive the loop without holding the AgentLoop value, so
    /// sync-locked map lookups can clone-and-drop the lock before
    /// awaiting on the send.
    pub fn sender(&self) -> mpsc::Sender<AgentInput> {
        self.tx.clone()
    }

    /// Wait for the loop to finish.
    pub async fn join(self) -> Outcome {
        self.handle
            .await
            .unwrap_or_else(|_| Outcome::Error("loop task panicked".into()))
    }
}

async fn run(
    provider: Arc<dyn Provider>,
    tools: Vec<Arc<dyn Tool>>,
    config: AgentConfig,
    events: mpsc::Sender<AgentEvent>,
    mut input_rx: mpsc::Receiver<AgentInput>,
) -> Outcome {
    let mut messages: Vec<AgentMessage> = Vec::new();
    let mut queue: VecDeque<Action> = VecDeque::new();
    let mut turn_count: u32 = 0;
    // Per-ask accounting. Set on first transition into InvokeLlm,
    // emitted in Done, reset on Done emit. Cumulative across turns
    // within one ask_session; resets per ask.
    let mut aggregated_usage: Option<Usage> = None;
    let mut started_at: Option<Instant> = None;
    // Per-ask tool-history. Used by the RetryPolicy::Never enforcement
    // path in handle_execute_tools. Reset on Done.
    let mut tool_history = ToolHistory::default();

    let _ = events.send(AgentEvent::AgentStart).await;

    loop {
        // Drain external input into the back of the queue. Cancel
        // short-circuits.
        while let Ok(input) = input_rx.try_recv() {
            match input {
                AgentInput::Cancel => return Outcome::Cancelled,
                AgentInput::UserMessage(_) => {
                    queue.push_back(Action::External(input));
                }
            }
        }

        // Pop next action; await blocking if queue empty.
        let action = if let Some(a) = queue.pop_front() {
            a
        } else {
            match input_rx.recv().await {
                Some(AgentInput::Cancel) => return Outcome::Cancelled,
                Some(input) => Action::External(input),
                None => break, // all senders dropped — clean exit
            }
        };

        match action {
            Action::External(AgentInput::UserMessage(msg)) => {
                let _ = events
                    .send(AgentEvent::MessageStart { message: msg.clone() })
                    .await;
                messages.push(msg.clone());
                let _ = events
                    .send(AgentEvent::MessageEnd { message: msg })
                    .await;
                if should_push_invoke_llm(&queue) {
                    queue.push_back(Action::InvokeLlm);
                }
            }
            Action::External(AgentInput::Cancel) => {
                // Defensive: Cancel is short-circuited at drain time, so
                // this branch is normally unreachable.
                return Outcome::Cancelled;
            }
            Action::InvokeLlm => {
                if turn_count >= config.max_turns {
                    let elapsed_ms = started_at.map(|t| t.elapsed().as_millis() as u64);
                    let _ = events
                        .send(AgentEvent::Done {
                            stop_reason: StopReason::EndTurn,
                            turn_count,
                            usage: aggregated_usage.take(),
                            elapsed_ms,
                        })
                        .await;
                    tool_history.clear();
                    return Outcome::IterationCap;
                }
                if started_at.is_none() {
                    started_at = Some(Instant::now());
                }
                turn_count += 1;
                let _ = events.send(AgentEvent::TurnStart).await;

                let tool_specs: Vec<ToolSpec> = tools.iter().map(|t| t.spec()).collect();

                match handle_invoke_llm(
                    provider.as_ref(),
                    &messages,
                    &tool_specs,
                    &mut input_rx,
                    &events,
                )
                .await
                {
                    Ok((assistant_msg, buffered)) => {
                        if let Some(u) = assistant_msg.usage {
                            aggregated_usage = Some(match aggregated_usage {
                                Some(prev) => prev.add(u),
                                None => u,
                            });
                        }
                        let assistant = AgentMessage::Assistant(assistant_msg.clone());
                        let _ = events
                            .send(AgentEvent::MessageStart {
                                message: assistant.clone(),
                            })
                            .await;
                        messages.push(assistant.clone());
                        let _ = events
                            .send(AgentEvent::MessageEnd { message: assistant })
                            .await;

                        let plan = plan_post_invoke_llm(&assistant_msg, buffered);
                        if plan.emit_turn_end {
                            let _ = events.send(AgentEvent::TurnEnd).await;
                        }
                        for action in plan.actions {
                            queue.push_back(action);
                        }
                    }
                    Err(outcome) => {
                        if let Outcome::Error(ref m) = outcome {
                            let _ = events
                                .send(AgentEvent::Error { message: m.clone() })
                                .await;
                        }
                        return outcome;
                    }
                }
            }
            Action::ExecuteTools(calls) => {
                match handle_execute_tools(
                    &tools,
                    calls,
                    &mut input_rx,
                    &events,
                    &mut tool_history,
                )
                .await
                {
                    Ok((tool_results, buffered)) => {
                        for r in tool_results {
                            messages.push(r);
                        }
                        let _ = events.send(AgentEvent::TurnEnd).await;
                        for action in plan_post_execute_tools(buffered) {
                            queue.push_back(action);
                        }
                    }
                    Err(outcome) => {
                        if let Outcome::Error(ref m) = outcome {
                            let _ = events
                                .send(AgentEvent::Error { message: m.clone() })
                                .await;
                        }
                        return outcome;
                    }
                }
            }
            Action::MaybeFinish => {
                // Race window: a UM may have arrived between the
                // InvokeLlm handler's "no buffered" check and now.
                // Drain once more before deciding.
                while let Ok(input) = input_rx.try_recv() {
                    match input {
                        AgentInput::Cancel => return Outcome::Cancelled,
                        AgentInput::UserMessage(_) => {
                            queue.push_back(Action::External(input));
                        }
                    }
                }

                if !queue.is_empty() {
                    // Pending external input — skip termination.
                    continue;
                }

                let elapsed_ms = started_at.map(|t| t.elapsed().as_millis() as u64);
                let _ = events
                    .send(AgentEvent::Done {
                        stop_reason: StopReason::EndTurn,
                        turn_count,
                        usage: aggregated_usage.take(),
                        elapsed_ms,
                    })
                    .await;
                tool_history.clear();
                return Outcome::Done(StopReason::EndTurn);
            }
        }
    }

    // input channel closed and no work pending — clean termination.
    let elapsed_ms = started_at.map(|t| t.elapsed().as_millis() as u64);
    let _ = events
        .send(AgentEvent::Done {
            stop_reason: StopReason::EndTurn,
            turn_count,
            usage: aggregated_usage.take(),
            elapsed_ms,
        })
        .await;
    tool_history.clear();
    Outcome::Done(StopReason::EndTurn)
}

// ============================================================================
// Per-ask tool history — backs RetryPolicy::Never enforcement
// ============================================================================

/// Bounded sliding window of recent tool dispatches per ask. The
/// `Never` retry policy refuses a new (tool_name, args) call if any
/// entry in the window has the same shape AND errored.
const TOOL_HISTORY_WINDOW: usize = 8;

#[derive(Debug, Default)]
struct ToolHistory {
    entries: VecDeque<ToolHistoryEntry>,
}

#[derive(Debug, Clone)]
struct ToolHistoryEntry {
    tool_name: String,
    arguments: serde_json::Value,
    is_error: bool,
}

impl ToolHistory {
    fn clear(&mut self) {
        self.entries.clear();
    }

    /// Record a completed dispatch. Drops the oldest if over capacity.
    fn record(&mut self, tool_name: String, arguments: serde_json::Value, is_error: bool) {
        self.entries.push_back(ToolHistoryEntry {
            tool_name,
            arguments,
            is_error,
        });
        while self.entries.len() > TOOL_HISTORY_WINDOW {
            self.entries.pop_front();
        }
    }

    /// Has a matching (tool_name, arguments) call in the window
    /// errored? Used by RetryPolicy::Never enforcement.
    fn errored_match(&self, tool_name: &str, arguments: &serde_json::Value) -> bool {
        self.entries
            .iter()
            .any(|e| e.is_error && e.tool_name == tool_name && &e.arguments == arguments)
    }
}

async fn handle_invoke_llm(
    provider: &dyn Provider,
    messages: &[AgentMessage],
    tool_specs: &[ToolSpec],
    input_rx: &mut mpsc::Receiver<AgentInput>,
    events: &mpsc::Sender<AgentEvent>,
) -> Result<(AssistantMessage, Vec<AgentInput>), Outcome> {
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let mut stream = provider
        .stream(messages, tool_specs, cancel_rx)
        .await
        .map_err(|e| Outcome::Error(e.to_string()))?;

    let mut buffered: Vec<AgentInput> = Vec::new();

    loop {
        tokio::select! {
            event = stream.next() => match event {
                Some(ProviderEvent::TextDelta(d)) => {
                    let _ = events.send(AgentEvent::TextDelta { delta: d }).await;
                }
                Some(ProviderEvent::Done(msg)) => {
                    // Best-effort signal that we're done with the stream.
                    let _ = cancel_tx.send(());
                    return Ok((msg, buffered));
                }
                Some(ProviderEvent::Error(e)) => {
                    let _ = cancel_tx.send(());
                    return Err(Outcome::Error(e));
                }
                Some(ProviderEvent::ThinkingDelta(_)) => {
                    // Future: emit a thinking event. v1 ignores.
                }
                Some(ProviderEvent::ToolCallDelta { .. }) => {
                    // Future: emit incremental tool-call events. v1
                    // ignores; final calls land in the Done payload.
                }
                None => {
                    let _ = cancel_tx.send(());
                    return Err(Outcome::Error(
                        "provider stream ended without Done".into(),
                    ));
                }
            },
            input_opt = input_rx.recv() => match input_opt {
                Some(AgentInput::Cancel) => {
                    let _ = cancel_tx.send(());
                    return Err(Outcome::Cancelled);
                }
                Some(input @ AgentInput::UserMessage(_)) => {
                    buffered.push(input);
                }
                None => {
                    // Input channel closed mid-stream. Treat as cancel.
                    let _ = cancel_tx.send(());
                    return Err(Outcome::Cancelled);
                }
            },
        }
    }
}

async fn handle_execute_tools(
    tools: &[Arc<dyn Tool>],
    calls: Vec<ToolCall>,
    input_rx: &mut mpsc::Receiver<AgentInput>,
    events: &mpsc::Sender<AgentEvent>,
    history: &mut ToolHistory,
) -> Result<(Vec<AgentMessage>, Vec<AgentInput>), Outcome> {
    let mut buffered: Vec<AgentInput> = Vec::new();
    let mut tool_messages: Vec<AgentMessage> = Vec::new();

    for call in calls {
        let _ = events
            .send(AgentEvent::ToolCallStarted {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                arguments: call.arguments.clone(),
            })
            .await;

        // Look up the tool + its policy.
        let tool = tools.iter().find(|t| t.spec().name == call.name);

        // Retry guard. If the tool's policy is Never and this same
        // (name, args) errored in the recent history, refuse the
        // dispatch without running the tool. Emit a callout so the
        // UI/log records the refusal alongside the synthesized
        // tool result.
        let retry_refused = match tool {
            Some(t) => {
                let policy = t.spec().policy;
                matches!(policy.retry, RetryPolicy::Never)
                    && history.errored_match(&call.name, &call.arguments)
            }
            None => false,
        };

        let result = if retry_refused {
            let msg = format!(
                "runtime refused: tool `{}` was just called with the same arguments \
                 and errored. Its retry policy is Never — do not retry with the same \
                 input. Try a different approach, a different tool, or report the \
                 obstacle to the user instead.",
                call.name
            );
            // Surface a structured callout for the UI/log. This is
            // visible at the wire layer too (session.callout
            // notification).
            let _ = events
                .send(AgentEvent::Callout {
                    category: "warning".to_owned(),
                    title: format!("retry refused for {}", call.name),
                    body: serde_json::json!({
                        "tool": call.name,
                        "arguments": call.arguments,
                        "reason": "RetryPolicy::Never matched a prior errored call",
                    }),
                    theme: Some("warning".to_owned()),
                    context_refs: vec!["spec:capability-delegation".to_owned()],
                })
                .await;
            ToolResult {
                content: msg,
                is_error: true,
            }
        } else {
            match tool {
                Some(t) => {
                    let (cancel_tx, cancel_rx) = oneshot::channel();
                    let mut execute_fut =
                        Box::pin(t.execute(call.arguments.clone(), cancel_rx));

                    loop {
                        tokio::select! {
                            result = &mut execute_fut => break result,
                            input_opt = input_rx.recv() => match input_opt {
                                Some(AgentInput::Cancel) => {
                                    let _ = cancel_tx.send(());
                                    return Err(Outcome::Cancelled);
                                }
                                Some(input @ AgentInput::UserMessage(_)) => {
                                    buffered.push(input);
                                }
                                None => {
                                    let _ = cancel_tx.send(());
                                    return Err(Outcome::Cancelled);
                                }
                            },
                        }
                    }
                }
                None => ToolResult {
                    content: format!("tool not found: {}", call.name),
                    is_error: true,
                },
            }
        };

        // Record the dispatch outcome for future retry checks. We
        // record even refused calls — if the same call lands again
        // the refusal still counts as evidence the model should
        // change strategy.
        history.record(call.name.clone(), call.arguments.clone(), result.is_error);

        let _ = events
            .send(AgentEvent::ToolCallCompleted {
                tool_call_id: call.id.clone(),
                content: result.content.clone(),
                is_error: result.is_error,
            })
            .await;

        tool_messages.push(AgentMessage::ToolResult {
            call_id: call.id,
            content: result.content,
            is_error: result.is_error,
        });
    }

    Ok((tool_messages, buffered))
}

#[cfg(test)]
#[path = "loop_tests.rs"]
mod tests;
