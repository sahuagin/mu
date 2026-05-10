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

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::provider::{Provider, ProviderEvent};
use super::tool::{Tool, ToolResult, ToolSpec};
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
    },
    Error {
        message: String,
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
enum Action {
    External(AgentInput),
    InvokeLlm,
    ExecuteTools(Vec<ToolCall>),
    MaybeFinish,
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
                // Only push InvokeLlm if there isn't already one queued.
                // Multiple back-to-back UMs share one LLM call.
                if !queue.iter().any(|a| matches!(a, Action::InvokeLlm)) {
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
                    let _ = events
                        .send(AgentEvent::Done {
                            stop_reason: StopReason::EndTurn,
                            turn_count,
                        })
                        .await;
                    return Outcome::IterationCap;
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

                        let tool_calls: Vec<ToolCall> = assistant_msg
                            .content
                            .iter()
                            .filter_map(|c| {
                                if let ContentBlock::ToolCall(tc) = c {
                                    Some(tc.clone())
                                } else {
                                    None
                                }
                            })
                            .collect();

                        if tool_calls.is_empty() {
                            let _ = events.send(AgentEvent::TurnEnd).await;
                            let had_buffered = !buffered.is_empty();
                            for input in buffered {
                                queue.push_back(Action::External(input));
                            }
                            // MaybeFinish only if no buffered UMs;
                            // otherwise the External(UM) handler will
                            // push its own InvokeLlm and the loop continues.
                            if !had_buffered {
                                queue.push_back(Action::MaybeFinish);
                            }
                        } else {
                            queue.push_back(Action::ExecuteTools(tool_calls));
                            for input in buffered {
                                queue.push_back(Action::External(input));
                            }
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
                match handle_execute_tools(&tools, calls, &mut input_rx, &events).await {
                    Ok((tool_results, buffered)) => {
                        for r in tool_results {
                            messages.push(r);
                        }
                        let _ = events.send(AgentEvent::TurnEnd).await;
                        // Buffered UMs first so they're added to context
                        // BEFORE the next InvokeLlm runs.
                        for input in buffered {
                            queue.push_back(Action::External(input));
                        }
                        queue.push_back(Action::InvokeLlm);
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

                let _ = events
                    .send(AgentEvent::Done {
                        stop_reason: StopReason::EndTurn,
                        turn_count,
                    })
                    .await;
                return Outcome::Done(StopReason::EndTurn);
            }
        }
    }

    // input channel closed and no work pending — clean termination.
    let _ = events
        .send(AgentEvent::Done {
            stop_reason: StopReason::EndTurn,
            turn_count,
        })
        .await;
    Outcome::Done(StopReason::EndTurn)
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

        let tool = tools.iter().find(|t| t.spec().name == call.name);
        let result = match tool {
            Some(t) => {
                let (cancel_tx, cancel_rx) = oneshot::channel();
                let mut execute_fut = Box::pin(t.execute(call.arguments.clone(), cancel_rx));

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
        };

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
