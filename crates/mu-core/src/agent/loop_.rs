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

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::capability::{Capability, CapabilityCheck};
use crate::protocol::ApprovalDecision;

use super::provider::{Provider, ProviderEvent};
use super::types::Usage;
use super::tool::{PermissionLevel, RetryPolicy, Tool, ToolResult, ToolSpec};
use super::types::{AgentMessage, AssistantMessage, ContentBlock, StopReason, ToolCall};

/// Map of outstanding `session.input_required` prompts, keyed by
/// `request_id`. Owned by the daemon's `Sessions` registry but
/// shared with the AgentLoop so it can both insert pending approvals
/// (before emitting `AgentEvent::InputRequired`) and have its
/// counterpart in the daemon's dispatch handler take entries out
/// when responses arrive.
pub type PendingApprovals = Arc<Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>>;

/// Shared handle to the session's `Capability` (mu-033). Wrapped in
/// a `Mutex` so the agent loop can both check it (read) and consume
/// tool-call budget (mutate). The Arc lets the daemon's
/// `Sessions::insert` and the AgentLoop hold the same instance.
pub type SessionCapability = Arc<Mutex<Capability>>;

/// External inputs callers push to a running agent loop.
#[derive(Debug, Clone)]
pub enum AgentInput {
    /// Add a message to the conversation. Loop runs the LLM after.
    UserMessage(AgentMessage),
    /// Stop. In-flight provider stream and tool execution are
    /// cancelled; loop returns `Outcome::Cancelled`.
    Cancel,
    /// Narrow-cancel (mu-035 Phase C): abort the current provider
    /// stream / tool dispatch, emit a Done(Aborted) for the ask, but
    /// keep the session alive for subsequent ask_sessions. Distinct
    /// from `Cancel`, which terminates the entire agent loop.
    CancelOutstanding { reason: String },
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
    /// A tool whose policy is `PermissionLevel::Ask` is about to
    /// dispatch; the agent loop is blocked waiting for a
    /// `session.respond_to_input_required` matching `request_id`
    /// before it proceeds. See spec mu-029.
    InputRequired {
        request_id: String,
        tool_call_id: String,
        tool_name: String,
        arguments: serde_json::Value,
        summary: String,
    },
    /// Prompt assembly snapshot. Emitted by the agent loop BEFORE
    /// each `provider.stream()` call. The forwarder lands it in
    /// the durable event log as `EventPayload::ContextAssembly`.
    /// See spec mu-032 and
    /// `specs/architecture/event-sourced-context.md`.
    ContextAssembly {
        model_call_id: u32,
        message_count: u32,
        user_message_count: u32,
        assistant_message_count: u32,
        tool_result_count: u32,
        tool_count: u32,
    },
    /// Provider-call lifecycle marker (mu-035 Phase A). Emitted on
    /// state transitions; Phase B will additionally emit periodic
    /// ticks while in non-streaming waits so a stalled provider
    /// remains visible to a watching client.
    ///
    /// The forwarder translates this to `session.provider_status`
    /// notifications.
    ///
    /// Field is `state` (not `kind`) because the enum's serde tag is
    /// already `kind` (the variant discriminator); reusing the name
    /// causes a serde naming collision.
    ProviderStatus {
        state: crate::protocol::ProviderStatusKind,
        /// Unix milliseconds the session entered this state.
        started_at_unix_ms: u64,
        /// Milliseconds since `started_at_unix_ms` at emit time.
        elapsed_ms: u64,
        /// Cumulative bytes from the provider's stream so far on
        /// this call. None when not meaningful.
        bytes_received: Option<u64>,
        /// Set only when `state` is ToolExecuting or AwaitingToolResult.
        tool_call_id: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Cap on assistant-message turns. The loop emits
    /// `AgentEvent::Done(EndTurn)` and returns `Outcome::IterationCap`
    /// when this is reached. Default 20.
    pub max_turns: u32,
    /// mu-n48: optional system prompt forwarded to every
    /// `Provider::stream` call in this session. None ⇒ no system
    /// content sent (pre-mu-n48 behavior). When set, providers render
    /// it appropriately (Anthropic top-level `system` field, OpenAI-
    /// style prepended {role:"system"} message), and Anthropic
    /// additionally tags it `cache_control: ephemeral` to amortize
    /// its tokens across asks in the session.
    pub system_prompt: Option<String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_turns: 20,
            system_prompt: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Done(StopReason),
    IterationCap,
    Cancelled,
    Error(String),
    /// mu-035 Phase C narrow-cancel: the current ask was aborted via
    /// `AgentInput::CancelOutstanding`, but the SESSION is still
    /// alive. The outer run() loop catches this from the inner
    /// handlers, emits a Done(Aborted) event for the ask, resets
    /// per-ask state, and continues to wait for the next ask. Not
    /// returned by run() itself — purely an internal sentinel.
    OutstandingCancelled { reason: String },
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
    ///
    /// `pending_approvals` is the shared registry the loop uses when
    /// dispatching tools with `PermissionLevel::Ask`: it inserts a
    /// fresh oneshot under a generated `request_id`, emits
    /// `AgentEvent::InputRequired`, then awaits the oneshot. The
    /// daemon's dispatch handler for `session.respond_to_input_required`
    /// is responsible for taking the oneshot out and sending the
    /// decision.
    pub fn spawn(
        provider: Arc<dyn Provider>,
        tools: Vec<Arc<dyn Tool>>,
        config: AgentConfig,
        events: mpsc::Sender<AgentEvent>,
        pending_approvals: PendingApprovals,
        capability: SessionCapability,
    ) -> Self {
        let (tx, rx) = mpsc::channel(32);
        let handle = tokio::spawn(run(
            provider,
            tools,
            config,
            events,
            rx,
            pending_approvals,
            capability,
        ));
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
    ///
    /// As of mu-035 Phase A (multi-turn fix), the agent loop no
    /// longer terminates after one ask. It runs until its input
    /// channel closes (all senders dropped) or it receives Cancel.
    /// `join` therefore drops the owned `tx` BEFORE awaiting the
    /// handle, so the loop sees its sole sender close and exits
    /// cleanly. If the daemon's session manager holds a cloned
    /// sender (via `sender()`), the loop will wait for that to
    /// also drop — which is what we want: the session is alive as
    /// long as the daemon has a way to talk to it.
    pub async fn join(self) -> Outcome {
        let Self { tx, handle } = self;
        drop(tx); // close the owned input sender so the loop can exit
        handle
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
    pending_approvals: PendingApprovals,
    capability: SessionCapability,
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
    // Monotonic per-session counter, incremented before each
    // provider.stream() call. Used to link a ContextAssembly event
    // to the AssistantMessage/Done it produced.
    let mut model_call_id: u32 = 0;

    let _ = events.send(AgentEvent::AgentStart).await;

    loop {
        // Drain external input into the back of the queue. Cancel
        // short-circuits.
        while let Ok(input) = input_rx.try_recv() {
            match input {
                AgentInput::Cancel => return Outcome::Cancelled,
                AgentInput::CancelOutstanding { .. } => {
                    // Nothing in-flight (we're between asks); narrow-
                    // cancel is a no-op. Drop silently.
                }
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
                Some(AgentInput::CancelOutstanding { .. }) => {
                    // Same: idle, no-op. Continue waiting for real
                    // input.
                    continue;
                }
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
            Action::External(AgentInput::CancelOutstanding { .. }) => {
                // Same: short-circuited at drain time, unreachable.
                continue;
            }
            Action::InvokeLlm => {
                if turn_count >= config.max_turns {
                    // Hit the per-ask iteration cap. Same finalize-
                    // and-continue pattern as MaybeFinish: this
                    // terminates the ask, not the session. The user
                    // can `ask_session` again — perhaps with a
                    // different prompt that needs fewer turns.
                    let elapsed_ms = started_at.map(|t| t.elapsed().as_millis() as u64);
                    let _ = events
                        .send(AgentEvent::Done {
                            stop_reason: StopReason::EndTurn,
                            turn_count,
                            usage: aggregated_usage.take(),
                            elapsed_ms,
                        })
                        .await;
                    started_at = None;
                    turn_count = 0;
                    tool_history.clear();
                    // Drop any remaining queue entries for this ask
                    // (e.g. tool calls the model was about to make).
                    queue.clear();
                    continue;
                }
                if started_at.is_none() {
                    started_at = Some(Instant::now());
                }
                turn_count += 1;
                let _ = events.send(AgentEvent::TurnStart).await;

                let tool_specs: Vec<ToolSpec> = tools.iter().map(|t| t.spec()).collect();

                // Emit ContextAssembly BEFORE the provider call so
                // the durable log records what the model was about
                // to see. (mu-032.)
                model_call_id += 1;
                let (user_count, assistant_count, tool_result_count) = count_message_roles(&messages);
                let _ = events
                    .send(AgentEvent::ContextAssembly {
                        model_call_id,
                        message_count: messages.len() as u32,
                        user_message_count: user_count,
                        assistant_message_count: assistant_count,
                        tool_result_count,
                        tool_count: tool_specs.len() as u32,
                    })
                    .await;

                match handle_invoke_llm(
                    provider.as_ref(),
                    config.system_prompt.as_deref(),
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
                    Err(Outcome::OutstandingCancelled { reason }) => {
                        // mu-035 Phase C: narrow-cancel of the
                        // current ask. Emit a Callout explaining
                        // why, finalize the ask with Done(Aborted),
                        // reset per-ask state, and continue the
                        // outer loop — the session stays addressable.
                        let _ = events
                            .send(AgentEvent::Callout {
                                category: "info".into(),
                                title: "outstanding call cancelled".into(),
                                body: serde_json::json!({ "reason": reason }),
                                theme: Some("info".into()),
                                context_refs: vec!["spec:mu-035".into()],
                            })
                            .await;
                        let elapsed_ms =
                            started_at.map(|t| t.elapsed().as_millis() as u64);
                        let _ = events
                            .send(AgentEvent::Done {
                                stop_reason: StopReason::Aborted,
                                turn_count,
                                usage: aggregated_usage.take(),
                                elapsed_ms,
                            })
                            .await;
                        started_at = None;
                        turn_count = 0;
                        tool_history.clear();
                        queue.clear();
                        continue;
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
                    &pending_approvals,
                    &capability,
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
                    Err(Outcome::OutstandingCancelled { reason }) => {
                        // Same finalize-and-continue pattern as the
                        // InvokeLlm arm above. Tool was mid-flight.
                        let _ = events
                            .send(AgentEvent::Callout {
                                category: "info".into(),
                                title: "outstanding call cancelled".into(),
                                body: serde_json::json!({ "reason": reason }),
                                theme: Some("info".into()),
                                context_refs: vec!["spec:mu-035".into()],
                            })
                            .await;
                        let elapsed_ms =
                            started_at.map(|t| t.elapsed().as_millis() as u64);
                        let _ = events
                            .send(AgentEvent::Done {
                                stop_reason: StopReason::Aborted,
                                turn_count,
                                usage: aggregated_usage.take(),
                                elapsed_ms,
                            })
                            .await;
                        started_at = None;
                        turn_count = 0;
                        tool_history.clear();
                        queue.clear();
                        continue;
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
                        AgentInput::CancelOutstanding { .. } => {
                            // No ask in flight at this point; no-op.
                        }
                        AgentInput::UserMessage(_) => {
                            queue.push_back(Action::External(input));
                        }
                    }
                }

                if !queue.is_empty() {
                    // Pending external input — skip the ask-finalization.
                    continue;
                }

                // Finalize the current ask: emit Done, then RESET
                // per-ask accounting and re-enter the loop. The
                // session stays alive for subsequent ask_sessions.
                // Termination only happens when all senders drop
                // (clean exit), cancel arrives, or an unrecoverable
                // error fires — handled outside this arm.
                let elapsed_ms = started_at.map(|t| t.elapsed().as_millis() as u64);
                let _ = events
                    .send(AgentEvent::Done {
                        stop_reason: StopReason::EndTurn,
                        turn_count,
                        usage: aggregated_usage.take(),
                        elapsed_ms,
                    })
                    .await;
                // Reset per-ask state. `messages` keeps the
                // conversation history — multi-turn requires it.
                started_at = None;
                turn_count = 0;
                tool_history.clear();
                // Continue: next pop_front will block on input_rx.recv()
                // for the next ask_session.
            }
        }
    }

    // Input channel closed and no work pending — clean shutdown.
    // MaybeFinish already emitted a Done for the last ask (post
    // multi-turn fix), so we do NOT emit another Done here; doing
    // so would double-emit on every clean shutdown. The Outcome
    // returned via the JoinHandle is still useful for callers that
    // care.
    tool_history.clear();
    Outcome::Done(StopReason::EndTurn)
}

// ============================================================================
// Per-ask tool history — backs RetryPolicy::Never enforcement
// ============================================================================

/// Count the number of User, Assistant, and ToolResult messages
/// in a slice. Used by the ContextAssembly emit path (mu-032) to
/// summarize the prompt being sent to the provider.
fn count_message_roles(messages: &[AgentMessage]) -> (u32, u32, u32) {
    let mut u = 0u32;
    let mut a = 0u32;
    let mut t = 0u32;
    for m in messages {
        match m {
            AgentMessage::User { .. } => u += 1,
            AgentMessage::Assistant(_) => a += 1,
            AgentMessage::ToolResult { .. } => t += 1,
        }
    }
    (u, a, t)
}

/// Bounded sliding window of recent tool dispatches per ask. The
/// `Never` retry policy refuses dispatch on two conditions:
///   1. Exact-match: same (tool_name, arguments) in the window
///      previously errored.
///   2. Consecutive-error-streak: the last `RETRY_STREAK_LIMIT`
///      calls to this tool ALL errored — regardless of arguments.
///      Catches the "model trying variants of a rejected command"
///      pattern observed in the bash strict-mode live test
///      2026-05-10.
const TOOL_HISTORY_WINDOW: usize = 8;
const RETRY_STREAK_LIMIT: usize = 3;

/// Monotonic counter used to generate `request_id`s for
/// `InputRequired` prompts. Combined with the tool_call_id for
/// readability + uniqueness even across sessions.
static ASK_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

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

    /// Count consecutive errors for `tool_name` starting from the
    /// most recent entry. A non-error call breaks the streak; calls
    /// to other tools are skipped (not break, not count).
    fn consecutive_errors_for(&self, tool_name: &str) -> usize {
        let mut streak = 0;
        for e in self.entries.iter().rev() {
            if e.tool_name != tool_name {
                continue;
            }
            if e.is_error {
                streak += 1;
            } else {
                break;
            }
        }
        streak
    }
}

fn now_unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

async fn handle_invoke_llm(
    provider: &dyn Provider,
    system_prompt: Option<&str>,
    messages: &[AgentMessage],
    tool_specs: &[ToolSpec],
    input_rx: &mut mpsc::Receiver<AgentInput>,
    events: &mpsc::Sender<AgentEvent>,
) -> Result<(AssistantMessage, Vec<AgentInput>), Outcome> {
    use crate::protocol::ProviderStatusKind;

    // mu-035 Phase A: emit AwaitingFirstToken just before opening
    // the stream. Phase B adds periodic re-emission while in
    // non-streaming waits — see PROVIDER_STATUS_TICK_MS below.
    //
    // INV-4 (the load-bearing property of the whole spec): the
    // emit-tick runs on a tokio interval timer that is INDEPENDENT
    // of the provider stream future. If the stream is wedged on a
    // syscall waiting for bytes from a stalled backend, the timer
    // still fires and the client still sees status. (Verified live
    // 2026-05-11 — codex backend was unresponsive for ~14 hours
    // overnight with zero diagnostic data on our side; this primitive
    // exists so that NEVER happens silently again.)
    const PROVIDER_STATUS_TICK_MS: u64 = 1000;
    let call_started_at = Instant::now();
    let call_started_unix_ms = now_unix_ms();
    let _ = events
        .send(AgentEvent::ProviderStatus {
            state: ProviderStatusKind::AwaitingFirstToken,
            started_at_unix_ms: call_started_unix_ms,
            elapsed_ms: 0,
            bytes_received: None,
            tool_call_id: None,
        })
        .await;

    let (cancel_tx, cancel_rx) = oneshot::channel();
    let mut stream = provider
        .stream(system_prompt, messages, tool_specs, cancel_rx)
        .await
        .map_err(|e| Outcome::Error(e.to_string()))?;

    let mut buffered: Vec<AgentInput> = Vec::new();
    // Track byte count + whether we've transitioned out of
    // AwaitingFirstToken yet.
    let mut bytes_received: u64 = 0;
    let mut seen_first_token = false;
    // Periodic-tick state: the current ProviderStatusKind the agent
    // loop is conceptually in, plus when we entered it. Updated on
    // transitions (e.g. AwaitingFirstToken → Streaming on first
    // token). The tick arm uses these to compose the periodic emit.
    let mut current_state = ProviderStatusKind::AwaitingFirstToken;
    let mut state_started_at = call_started_at;
    let mut state_started_unix_ms = call_started_unix_ms;
    // Tokio interval timer for the periodic emit. Skip the first
    // immediate tick (interval() fires at t=0 by default) — we
    // already emitted at the transition.
    let mut tick_interval =
        tokio::time::interval(std::time::Duration::from_millis(PROVIDER_STATUS_TICK_MS));
    tick_interval.tick().await;
    // Once the input channel closes (all senders dropped), we want
    // the in-flight stream to complete naturally — NOT be treated
    // as a cancel. This was the pre-multi-turn behavior (Outcome::
    // Cancelled on input None), and it broke `join()` semantics
    // when the loop was made to survive past Done. Now we just
    // stop polling input_rx via `std::future::pending` after seeing
    // its first None.
    let mut input_drained = false;

    loop {
        tokio::select! {
            event = stream.next() => match event {
                Some(ProviderEvent::TextDelta(d)) => {
                    bytes_received = bytes_received.saturating_add(d.len() as u64);
                    if !seen_first_token {
                        seen_first_token = true;
                        // Transition: AwaitingFirstToken → Streaming.
                        // Re-anchor state_started_* so the next tick
                        // measures Streaming-duration from here.
                        current_state = ProviderStatusKind::Streaming;
                        state_started_at = Instant::now();
                        state_started_unix_ms = now_unix_ms();
                        let _ = events
                            .send(AgentEvent::ProviderStatus {
                                state: current_state,
                                started_at_unix_ms: state_started_unix_ms,
                                elapsed_ms: call_started_at.elapsed().as_millis() as u64,
                                bytes_received: Some(bytes_received),
                                tool_call_id: None,
                            })
                            .await;
                    }
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
            input_opt = async {
                if input_drained {
                    // Senders are gone; don't poll the receiver any
                    // more. `std::future::pending` parks this branch
                    // of the select forever, letting the stream
                    // arm drain to completion.
                    std::future::pending::<Option<AgentInput>>().await
                } else {
                    input_rx.recv().await
                }
            } => match input_opt {
                Some(AgentInput::Cancel) => {
                    let _ = cancel_tx.send(());
                    return Err(Outcome::Cancelled);
                }
                Some(AgentInput::CancelOutstanding { reason }) => {
                    // mu-035 Phase C: abort the in-flight provider
                    // call but keep the session alive. The outer
                    // loop will catch this and emit Done(Aborted).
                    let _ = cancel_tx.send(());
                    return Err(Outcome::OutstandingCancelled { reason });
                }
                Some(input @ AgentInput::UserMessage(_)) => {
                    buffered.push(input);
                }
                None => {
                    // All senders dropped. Let the stream finish
                    // — emit Done naturally — and on the next
                    // outer-loop iteration the main recv() will
                    // also return None and trigger a clean exit.
                    input_drained = true;
                }
            },
            // mu-035 Phase B: periodic provider_status emit during
            // non-streaming waits. Independent of stream.next() —
            // INV-4: a stalled provider still produces status here.
            _ = tick_interval.tick() => {
                // Only emit while in a wait state. Once streaming
                // has begun, text_delta is its own implicit
                // heartbeat; we don't need periodic ticks.
                if !matches!(current_state, ProviderStatusKind::Streaming) {
                    let elapsed_ms = state_started_at.elapsed().as_millis() as u64;
                    let _ = events
                        .send(AgentEvent::ProviderStatus {
                            state: current_state,
                            started_at_unix_ms: state_started_unix_ms,
                            elapsed_ms,
                            bytes_received: if bytes_received > 0 {
                                Some(bytes_received)
                            } else {
                                None
                            },
                            tool_call_id: None,
                        })
                        .await;
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
    pending_approvals: &PendingApprovals,
    capability: &SessionCapability,
) -> Result<(Vec<AgentMessage>, Vec<AgentInput>), Outcome> {
    let mut buffered: Vec<AgentInput> = Vec::new();
    let mut tool_messages: Vec<AgentMessage> = Vec::new();

    for call in calls {
        // mu-035 Phase A: emit ToolExecuting just before dispatch.
        // Client UIs render "tool: NAME (Xs)" while waiting on the
        // tool to return.
        let _ = events
            .send(AgentEvent::ProviderStatus {
                state: crate::protocol::ProviderStatusKind::ToolExecuting,
                started_at_unix_ms: now_unix_ms(),
                elapsed_ms: 0,
                bytes_received: None,
                tool_call_id: Some(call.id.clone()),
            })
            .await;
        let _ = events
            .send(AgentEvent::ToolCallStarted {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                arguments: call.arguments.clone(),
            })
            .await;

        // Look up the tool + its policy.
        let tool = tools.iter().find(|t| t.spec().name == call.name);

        // Capability gate (mu-033). If the session is operating
        // under an attenuated capability and this tool isn't in
        // its allowed set (or the capability has expired / budget
        // is exhausted), refuse dispatch.
        let capability_refusal_reason: Option<&'static str> = {
            let cap = capability.lock().ok();
            cap.as_ref().and_then(|c| match c.check_allow(&call.name) {
                CapabilityCheck::Allowed => None,
                CapabilityCheck::DeniedToolNotAllowed => Some("tool not in session's capability"),
                CapabilityCheck::DeniedExpired => Some("session capability has expired"),
                CapabilityCheck::DeniedBudgetExhausted => {
                    Some("session capability's tool-call budget exhausted")
                }
                // mu-036: DeniedAutonomyDisallowed only applies to
                // session.start_autonomous (where it's checked by
                // handle_start_autonomous, not here). A tool dispatch
                // never produces this — but match arm required for
                // exhaustiveness.
                CapabilityCheck::DeniedAutonomyDisallowed => None,
            })
        };

        // Retry guard. If the tool's policy is Never, refuse on
        // either of:
        //   (a) exact-match: same (name, args) errored in window
        //   (b) error streak: last RETRY_STREAK_LIMIT calls to
        //       this tool all errored, regardless of args
        // (b) catches the "variants of a rejected command" pattern.
        let retry_refusal_reason: Option<&'static str> = match tool {
            Some(t) => {
                let policy = t.spec().policy;
                if !matches!(policy.retry, RetryPolicy::Never) {
                    None
                } else if history.errored_match(&call.name, &call.arguments) {
                    Some("exact-match retry of a previously-errored call")
                } else if history.consecutive_errors_for(&call.name) >= RETRY_STREAK_LIMIT {
                    Some("error streak — the last several calls to this tool all errored")
                } else {
                    None
                }
            }
            None => None,
        };

        // Permission gate. If the tool's PermissionLevel is Ask,
        // emit an InputRequired event with a fresh request_id,
        // register a oneshot in the pending-approvals map, and
        // await the decision. Approve continues to dispatch; Deny
        // synthesizes an is_error result. (AskOnce/Always
        // remembering is reserved for v2.)
        let permission_decision = if !retry_refusal_reason.is_some() {
            match tool.as_ref().map(|t| t.spec().policy.permission) {
                Some(PermissionLevel::Ask) | Some(PermissionLevel::AskOnce) => {
                    // AskOnce currently treated as Ask in v1; future
                    // work persists the "approved once" decision so
                    // subsequent calls skip the prompt.
                    let request_id = format!("ask-{}-{}", call.id, ASK_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
                    let (decision_tx, decision_rx) = oneshot::channel();
                    if let Ok(mut pending) = pending_approvals.lock() {
                        pending.insert(request_id.clone(), decision_tx);
                    }
                    let _ = events
                        .send(AgentEvent::InputRequired {
                            request_id: request_id.clone(),
                            tool_call_id: call.id.clone(),
                            tool_name: call.name.clone(),
                            arguments: call.arguments.clone(),
                            summary: format!(
                                "{}({})",
                                call.name,
                                serde_json::to_string(&call.arguments)
                                    .unwrap_or_else(|_| "?".into())
                            ),
                        })
                        .await;
                    // Race the decision against input_rx for cancel.
                    let decision = tokio::select! {
                        d = decision_rx => d.ok(),
                        input_opt = input_rx.recv() => match input_opt {
                            Some(AgentInput::Cancel) => {
                                // Clear the pending entry on cancel
                                // so the daemon doesn't hold a
                                // stale sender.
                                if let Ok(mut pending) = pending_approvals.lock() {
                                    pending.remove(&request_id);
                                }
                                return Err(Outcome::Cancelled);
                            }
                            Some(AgentInput::CancelOutstanding { reason }) => {
                                // mu-035 Phase C narrow-cancel during
                                // approval wait.
                                if let Ok(mut pending) = pending_approvals.lock() {
                                    pending.remove(&request_id);
                                }
                                return Err(Outcome::OutstandingCancelled { reason });
                            }
                            Some(AgentInput::UserMessage(_)) => {
                                // User sent a message mid-prompt.
                                // We can't easily buffer + still
                                // await; treat as implicit cancel
                                // of this turn.
                                if let Ok(mut pending) = pending_approvals.lock() {
                                    pending.remove(&request_id);
                                }
                                return Err(Outcome::Cancelled);
                            }
                            None => {
                                if let Ok(mut pending) = pending_approvals.lock() {
                                    pending.remove(&request_id);
                                }
                                return Err(Outcome::Cancelled);
                            }
                        },
                    };
                    Some(decision.unwrap_or(ApprovalDecision::Deny))
                }
                Some(PermissionLevel::Deny) => Some(ApprovalDecision::Deny),
                _ => None, // Allow or no tool — no gate
            }
        } else {
            None // retry guard takes precedence
        };

        let permission_denied = matches!(permission_decision, Some(ApprovalDecision::Deny));

        let result = if let Some(cap_reason) = capability_refusal_reason {
            let msg = format!(
                "runtime refused: tool `{}` blocked by session capability ({cap_reason}). \
                 This session has been delegated a narrower scope than the root; \
                 the requested tool falls outside it. Use a different tool, ask the \
                 user to widen scope, or report the obstacle.",
                call.name
            );
            let _ = events
                .send(AgentEvent::Callout {
                    category: "warning".to_owned(),
                    title: format!("capability refused {}", call.name),
                    body: serde_json::json!({
                        "tool": call.name,
                        "reason": cap_reason,
                    }),
                    theme: Some("warning".to_owned()),
                    context_refs: vec!["spec:capability-delegation".to_owned()],
                })
                .await;
            ToolResult {
                content: msg,
                is_error: true,
            }
        } else if let Some(reason) = retry_refusal_reason {
            let msg = format!(
                "runtime refused: tool `{}` blocked by RetryPolicy::Never ({reason}). \
                 Do not retry with variants of the same approach. Switch tools, \
                 change strategy materially, or report the obstacle to the user.",
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
                        "reason": reason,
                    }),
                    theme: Some("warning".to_owned()),
                    context_refs: vec!["spec:capability-delegation".to_owned()],
                })
                .await;
            ToolResult {
                content: msg,
                is_error: true,
            }
        } else if permission_denied {
            ToolResult {
                content: format!(
                    "tool `{}` denied by user via session.respond_to_input_required",
                    call.name
                ),
                is_error: true,
            }
        } else {
            // About to actually dispatch — consume one tool-call
            // budget unit, if the session's capability has one.
            // Doing this BEFORE dispatch so cancel/error paths
            // still count the call (the model attempted it).
            if let Ok(mut cap) = capability.lock() {
                cap.consume_tool_call();
            }
            match tool {
                Some(t) => {
                    let (cancel_tx, cancel_rx) = oneshot::channel();
                    let mut execute_fut =
                        Box::pin(t.execute(call.arguments.clone(), cancel_rx));

                    // mu-035 Phase B: periodic ToolExecuting status
                    // emit. Same INV-4 motivation as the LLM stream
                    // — a tool that hangs (e.g. waiting on
                    // approval, slow IO, network timeout) stays
                    // visible to clients via these ticks.
                    let tool_call_started_at = Instant::now();
                    let tool_state_started_unix_ms = now_unix_ms();
                    let mut tool_tick =
                        tokio::time::interval(std::time::Duration::from_millis(1000));
                    tool_tick.tick().await; // skip the t=0 immediate tick

                    // Same "let work finish, don't cancel on
                    // sender-drop" pattern as handle_invoke_llm
                    // (mu-035 Phase A multi-turn fix).
                    let mut input_drained_local = false;
                    loop {
                        tokio::select! {
                            result = &mut execute_fut => break result,
                            input_opt = async {
                                if input_drained_local {
                                    std::future::pending::<Option<AgentInput>>().await
                                } else {
                                    input_rx.recv().await
                                }
                            } => match input_opt {
                                Some(AgentInput::Cancel) => {
                                    let _ = cancel_tx.send(());
                                    return Err(Outcome::Cancelled);
                                }
                                Some(AgentInput::CancelOutstanding { reason }) => {
                                    // mu-035 Phase C narrow-cancel
                                    // mid-tool. Abort the tool, surface
                                    // the OutstandingCancelled to the
                                    // outer loop.
                                    let _ = cancel_tx.send(());
                                    return Err(Outcome::OutstandingCancelled { reason });
                                }
                                Some(input @ AgentInput::UserMessage(_)) => {
                                    buffered.push(input);
                                }
                                None => {
                                    // Senders dropped. Let the
                                    // tool finish naturally — the
                                    // outer loop's recv() will
                                    // catch up on the next idle.
                                    input_drained_local = true;
                                }
                            },
                            _ = tool_tick.tick() => {
                                let elapsed_ms =
                                    tool_call_started_at.elapsed().as_millis() as u64;
                                let _ = events
                                    .send(AgentEvent::ProviderStatus {
                                        state: crate::protocol::ProviderStatusKind::ToolExecuting,
                                        started_at_unix_ms: tool_state_started_unix_ms,
                                        elapsed_ms,
                                        bytes_received: None,
                                        tool_call_id: Some(call.id.clone()),
                                    })
                                    .await;
                            }
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
