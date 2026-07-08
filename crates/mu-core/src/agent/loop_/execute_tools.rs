//! Tool execution dispatch — gates (capability, retry, permission) + tool result collection.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;

use crate::capability::CapabilityCheck;
use crate::protocol::ApprovalDecision;

use super::super::tool::{PermissionLevel, RetryPolicy, Tool, ToolResult};
use super::super::types::{AgentMessage, ToolCall};

use super::{AgentEvent, AgentInput, Outcome, PendingApprovals, SessionCapability};

/// Bounded sliding window of recent tool dispatches per ask. The
/// `Never` retry policy refuses dispatch on two conditions:
///   1. Exact-match: same (tool_name, arguments) in the window
///      previously errored.
///   2. Consecutive-error-streak: the last `RETRY_STREAK_LIMIT`
///      calls to this tool ALL errored — regardless of arguments.
///      Catches the "model trying variants of a rejected command"
///      pattern observed in the bash strict-mode live test
///      2026-05-10.
pub const TOOL_HISTORY_WINDOW: usize = 8;
const RETRY_STREAK_LIMIT: usize = 3;

/// Monotonic counter used to generate `request_id`s for
/// `InputRequired` prompts. Combined with the tool_call_id for
/// readability + uniqueness even across sessions.
static ASK_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// mu-8stm.1: upper bound on how long the dispatch gate will wait for an
/// approval decision before failing closed. The approver lives across an
/// unsecured process boundary (remote frontend ↔ daemon), so "no response"
/// can be a silent human OR a dropped channel — both resolve the same way:
/// deny. A turn must NEVER park forever on an approver that may not exist
/// (that was the MCP `code_index` wedge: unclassified → Ask → no solo
/// approver → permanent hang). A session-declared "approver present" flag
/// (deny immediately when known-headless instead of waiting this out) is the
/// planned refinement; until then this bound is the floor.
const APPROVAL_GATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

#[derive(Debug, Default)]
pub(crate) struct ToolHistory {
    pub(crate) entries: VecDeque<ToolHistoryEntry>,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolHistoryEntry {
    pub tool_name: String,
    pub arguments: serde_json::Value,
    pub is_error: bool,
}

impl ToolHistory {
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Record a completed dispatch. Drops the oldest if over capacity.
    pub fn record(&mut self, tool_name: String, arguments: serde_json::Value, is_error: bool) {
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
    pub(crate) fn errored_match(&self, tool_name: &str, arguments: &serde_json::Value) -> bool {
        self.entries
            .iter()
            .any(|e| e.is_error && e.tool_name == tool_name && &e.arguments == arguments)
    }

    /// Count consecutive errors for `tool_name` starting from the
    /// most recent entry. A non-error call breaks the streak; calls
    /// to other tools are skipped (not break, not count).
    pub(crate) fn consecutive_errors_for(&self, tool_name: &str) -> usize {
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

/// Best-effort extraction of a panic payload's message. Panics carry
/// `&str` or `String` in practice; anything else gets a placeholder.
pub(crate) fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_owned()
    }
}

fn now_unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) enum ExecuteToolsExit {
    Completed {
        tool_messages: Vec<AgentMessage>,
        buffered: Vec<AgentInput>,
    },
    /// The current ask was narrow-cancelled while one or more assistant
    /// tool calls were outstanding. `tool_messages` contains synthetic
    /// is_error ToolResult messages for every unanswered call, so the
    /// durable log and in-memory history never retain a dangling function
    /// call (OpenAI/Codex rejects that shape on the next turn).
    OutstandingCancelled {
        reason: String,
        tool_messages: Vec<AgentMessage>,
    },
    /// The whole session was cancelled while tool calls were outstanding.
    /// We still return synthetic tool results for log/history hygiene before
    /// the outer loop terminates.
    Cancelled { tool_messages: Vec<AgentMessage> },
}

async fn emit_tool_call_started(events: &mpsc::Sender<AgentEvent>, call: &ToolCall) {
    let _ = events
        .send(AgentEvent::ToolCallStarted {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            arguments: call.arguments.clone().into(),
        })
        .await;
}

async fn finish_tool_call(
    events: &mpsc::Sender<AgentEvent>,
    history: &mut ToolHistory,
    tool_messages: &mut Vec<AgentMessage>,
    call: ToolCall,
    result: ToolResult,
    verbatim: bool,
) {
    history.record(
        call.name.clone(),
        call.arguments.clone().into(),
        result.is_error,
    );

    // mu-2e0h tier 1: deterministic ingestion hygiene (ANSI strip,
    // repeat collapse, line cap, dump truncation) applied ONCE,
    // here — so the provider context, the durable event log, and
    // the wire all carry the same content. Event-log-decides
    // depends on the log being the truth of what the model saw;
    // filtering only the context entry would silently diverge
    // them. Clean content passes through borrowed (no copy).
    // Tools that declare `verbatim_result` (read-like tools whose
    // output must stay byte-identical to disk for exact-match
    // editing) bypass the filter entirely.
    let content = if verbatim {
        result.content
    } else {
        match super::super::tool_result_filter::filter_tool_result(&result.content) {
            std::borrow::Cow::Borrowed(_) => result.content,
            std::borrow::Cow::Owned(filtered) => filtered,
        }
    };

    let _ = events
        .send(AgentEvent::ToolCallCompleted {
            tool_call_id: call.id.clone(),
            content: content.clone(),
            is_error: result.is_error,
        })
        .await;

    tool_messages.push(AgentMessage::ToolResult {
        call_id: call.id,
        content,
        is_error: result.is_error,
    });
}

fn cancelled_tool_result(
    call: &ToolCall,
    reason: &str,
    source: &str,
    already_running: bool,
) -> ToolResult {
    let state = if already_running {
        "cancelled before completion"
    } else {
        "not executed because the ask was cancelled"
    };
    ToolResult {
        content: format!("tool call `{}` {state} by {source}: {reason}", call.name),
        is_error: true,
    }
}

async fn cancel_current_and_remaining(
    events: &mpsc::Sender<AgentEvent>,
    history: &mut ToolHistory,
    tool_messages: &mut Vec<AgentMessage>,
    current: ToolCall,
    remaining: Vec<ToolCall>,
    reason: &str,
    source: &str,
) {
    finish_tool_call(
        events,
        history,
        tool_messages,
        current.clone(),
        cancelled_tool_result(&current, reason, source, true),
        false,
    )
    .await;

    for call in remaining {
        // The assistant message already contains this tool_call block; emit
        // a started+completed tombstone pair so live clients and the durable
        // log see an answered call rather than a silent gap.
        emit_tool_call_started(events, &call).await;
        finish_tool_call(
            events,
            history,
            tool_messages,
            call.clone(),
            cancelled_tool_result(&call, reason, source, false),
            false,
        )
        .await;
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_execute_tools(
    tools: &[Arc<dyn Tool>],
    calls: Vec<ToolCall>,
    input_rx: &mut mpsc::Receiver<AgentInput>,
    events: &mpsc::Sender<AgentEvent>,
    history: &mut ToolHistory,
    pending_approvals: &PendingApprovals,
    capability: &SessionCapability,
    hooks: Option<&crate::hooks::HookEngine>,
) -> Result<ExecuteToolsExit, Outcome> {
    let mut buffered: Vec<AgentInput> = Vec::new();
    let mut tool_messages: Vec<AgentMessage> = Vec::new();
    let mut calls: VecDeque<ToolCall> = calls.into_iter().collect();

    while let Some(call) = calls.pop_front() {
        let _ = events
            .send(AgentEvent::ProviderStatus {
                state: crate::protocol::ProviderStatusKind::ToolExecuting,
                started_at_unix_ms: now_unix_ms(),
                elapsed_ms: 0,
                bytes_received: None,
                tool_call_id: Some(call.id.clone()),
            })
            .await;
        emit_tool_call_started(events, &call).await;

        let tool = tools.iter().find(|t| t.spec().name == call.name);

        let capability_refusal_reason: Option<String> = {
            let cap = capability.lock().ok();
            cap.as_ref().and_then(|c| match c.check_allow(&call.name) {
                CapabilityCheck::Allowed => {
                    // mu-8stm.2 (1b): the STRUCTURED appropriateness gate
                    // (canonical successor to mu-n25a's linear ceiling). Check
                    // the tool's canonical Effects against the session's
                    // per-axis constraints via the SAME `disallowed_by`
                    // predicate the discovery surface uses (single source of
                    // truth), BEFORE the AWS + permission gates so a
                    // `permission: Allow` tool cannot free-ride a restrictive
                    // posture (the SELF-CLASSIFIED-AUTHORITY bug class, mu-usfj).
                    // Unconstrained sessions (no ceiling) allow everything
                    // (back-compat). A missing tool falls through to the
                    // not-found path below. Unannotated effects fail closed —
                    // dormant today (`derived_effects()` is total over every
                    // dispatchable tool), it bites only a future unclassified
                    // dispatchable source.
                    //
                    // Use `derived_effects()` — the SAME projection discovery
                    // uses (including the aws->network/spend reach) — so the gate
                    // and `allowed_by_session` agree exactly, and an AWS-gated
                    // tool can't slip its network/spend reach past a
                    // no-network/no-spend posture just because the grant is held.
                    // The AWS-grant gate below is an ADDITIONAL check, not a
                    // substitute for the posture (review: gpt-5.5).
                    if let Some(t) = tool.as_ref() {
                        let effects = t.spec().policy.derived_effects();
                        if let CapabilityCheck::DeniedInappropriate { reason } =
                            c.check_effects(Some(&effects))
                        {
                            return Some(reason);
                        }
                    }
                    let required_aws = tool
                        .as_ref()
                        .and_then(|t| t.spec().policy.required_aws_capability.clone());
                    match required_aws {
                        Some(required) if !c.aws.iter().any(|aws_cap| aws_cap.name == required) => {
                            Some(format!("missing required AWS capability `{required}`"))
                        }
                        _ => None,
                    }
                }
                CapabilityCheck::DeniedToolNotAllowed => {
                    Some("tool not in session's capability".to_owned())
                }
                CapabilityCheck::DeniedExpired => Some("session capability has expired".to_owned()),
                CapabilityCheck::DeniedBudgetExhausted => {
                    Some("session capability's tool-call budget exhausted".to_owned())
                }
                CapabilityCheck::DeniedAutonomyDisallowed
                | CapabilityCheck::DeniedSideEffectsExceeded { .. }
                | CapabilityCheck::DeniedInappropriate { .. } => None,
            })
        };

        let retry_refusal_reason: Option<&'static str> = match tool {
            Some(t) => {
                let policy = t.spec().policy;
                if !matches!(policy.retry, RetryPolicy::Never) {
                    None
                } else if history.errored_match(&call.name, call.arguments.as_value()) {
                    Some("exact-match retry of a previously-errored call")
                } else if history.consecutive_errors_for(&call.name) >= RETRY_STREAK_LIMIT {
                    Some("error streak — the last several calls to this tool all errored")
                } else {
                    None
                }
            }
            None => None,
        };

        // mu-bkjr: argument-aware pre-flight check. Tools that reject
        // specific argument shapes (e.g. bash's allowlist) can fail the
        // call here, BEFORE the PermissionLevel::Ask gate dispatches a
        // session.input_required modal. Without this, the user would be
        // asked to approve a call that the tool will reject anyway.
        //
        // Only run when no higher-priority refusal applies — keeps the
        // refusal-reason ordering stable (capability > retry > validate >
        // permission-denied > execute).
        let validate_refusal_reason: Option<String> =
            if capability_refusal_reason.is_none() && retry_refusal_reason.is_none() {
                tool.as_ref()
                    .and_then(|t| t.validate(call.arguments.as_value()).err())
            } else {
                None
            };

        // mu-bb2v: operator PreToolUse hook gate. Runs after the
        // runtime's own gates (capability/retry/validate) and BEFORE the
        // PermissionLevel::Ask modal — a human is never asked to approve
        // a call an operator hook will deny anyway. Only an explicit
        // deny blocks; hook errors and timeouts fail open (see
        // `crate::hooks`).
        let hook_refusal_reason: Option<String> = if capability_refusal_reason.is_none()
            && retry_refusal_reason.is_none()
            && validate_refusal_reason.is_none()
        {
            match hooks {
                Some(h) => {
                    h.gate_pre_tool_use(&call.name, call.arguments.as_value())
                        .await
                }
                None => None,
            }
        } else {
            None
        };

        // mu-8stm.1: a no-approver / dropped-channel timeout produces a
        // refusal distinct from a human denial, so the model doesn't read
        // "fail-closed, no approver" as "the user said no".
        let mut permission_refusal_reason: Option<String> = None;
        let permission_decision = if retry_refusal_reason.is_none()
            && validate_refusal_reason.is_none()
            && hook_refusal_reason.is_none()
        {
            match tool.as_ref().map(|t| t.spec().policy.permission) {
                Some(PermissionLevel::Ask) | Some(PermissionLevel::AskOnce) => {
                    let request_id = format!(
                        "ask-{}-{}",
                        call.id,
                        ASK_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    );
                    let (decision_tx, decision_rx) = tokio::sync::oneshot::channel();
                    if let Ok(mut pending) = pending_approvals.lock() {
                        pending.insert(request_id.clone(), decision_tx);
                    }
                    let _ = events
                        .send(AgentEvent::InputRequired {
                            request_id: request_id.clone(),
                            tool_call_id: call.id.clone(),
                            tool_name: call.name.clone(),
                            arguments: call.arguments.clone().into(),
                            summary: format!(
                                "{}({})",
                                call.name,
                                serde_json::to_string(&call.arguments)
                                    .unwrap_or_else(|_| "?".into())
                            ),
                        })
                        .await;
                    let decision = tokio::select! {
                        d = decision_rx => d.ok(),
                        _ = tokio::time::sleep(APPROVAL_GATE_TIMEOUT) => {
                            // No approver responded in time — fail closed.
                            // Drop the pending entry so a late responder
                            // can't fire a stale oneshot.
                            if let Ok(mut pending) = pending_approvals.lock() {
                                pending.remove(&request_id);
                            }
                            permission_refusal_reason = Some(format!(
                                "tool `{}` required approval but no approver responded within \
                                 {}s — denied (fail-closed). This session has no interactive \
                                 approver, or the approval channel dropped; this is NOT a human \
                                 denial. Classify the tool/server (side_effects) or attach an \
                                 approver.",
                                call.name,
                                APPROVAL_GATE_TIMEOUT.as_secs()
                            ));
                            Some(ApprovalDecision::Deny)
                        }
                        input_opt = input_rx.recv() => match input_opt {
                            Some(AgentInput::Cancel) => {
                                if let Ok(mut pending) = pending_approvals.lock() {
                                    pending.remove(&request_id);
                                }
                                let reason = "session cancelled while awaiting tool approval";
                                let remaining = calls.drain(..).collect();
                                cancel_current_and_remaining(
                                    events,
                                    history,
                                    &mut tool_messages,
                                    call.clone(),
                                    remaining,
                                    reason,
                                    "session.cancel",
                                )
                                .await;
                                return Ok(ExecuteToolsExit::Cancelled { tool_messages });
                            }
                            Some(AgentInput::CancelOutstanding { reason }) => {
                                if let Ok(mut pending) = pending_approvals.lock() {
                                    pending.remove(&request_id);
                                }
                                let remaining = calls.drain(..).collect();
                                cancel_current_and_remaining(
                                    events,
                                    history,
                                    &mut tool_messages,
                                    call.clone(),
                                    remaining,
                                    &reason,
                                    "session.cancel_outstanding",
                                )
                                .await;
                                return Ok(ExecuteToolsExit::OutstandingCancelled {
                                    reason,
                                    tool_messages,
                                });
                            }
                            Some(AgentInput::UserMessage(..))
                            | Some(AgentInput::StartAutonomous { .. })
                            | Some(AgentInput::ScheduleWakeup { .. })
                            | Some(AgentInput::SwitchProvider { .. })
                            | Some(AgentInput::WatchCompleted { .. })
                            | Some(AgentInput::DialogueMessage { .. })
                            | Some(AgentInput::MailboxMessage { .. }) => {
                                if let Ok(mut pending) = pending_approvals.lock() {
                                    pending.remove(&request_id);
                                }
                                let reason = "tool approval interrupted by another session input";
                                let remaining = calls.drain(..).collect();
                                cancel_current_and_remaining(
                                    events,
                                    history,
                                    &mut tool_messages,
                                    call.clone(),
                                    remaining,
                                    reason,
                                    "agent loop",
                                )
                                .await;
                                return Ok(ExecuteToolsExit::Cancelled { tool_messages });
                            }
                            None => {
                                if let Ok(mut pending) = pending_approvals.lock() {
                                    pending.remove(&request_id);
                                }
                                let reason = "input channel closed while awaiting tool approval";
                                let remaining = calls.drain(..).collect();
                                cancel_current_and_remaining(
                                    events,
                                    history,
                                    &mut tool_messages,
                                    call.clone(),
                                    remaining,
                                    reason,
                                    "session shutdown",
                                )
                                .await;
                                return Ok(ExecuteToolsExit::Cancelled { tool_messages });
                            }
                        },
                    };
                    Some(decision.unwrap_or(ApprovalDecision::Deny))
                }
                Some(PermissionLevel::Deny) => Some(ApprovalDecision::Deny),
                _ => None,
            }
        } else {
            None
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
        } else if let Some(reason) = validate_refusal_reason {
            // mu-bkjr: tool's pre-flight check rejected the arguments.
            // No InputRequired was dispatched — the user was never asked
            // to approve a call that would fail. The reason string is
            // already user-facing (e.g. bash's allowlist message).
            ToolResult {
                content: reason,
                is_error: true,
            }
        } else if let Some(reason) = hook_refusal_reason {
            // mu-bb2v: an operator PreToolUse hook denied this call.
            let _ = events
                .send(AgentEvent::Callout {
                    category: "warning".to_owned(),
                    title: format!("hook denied {}", call.name),
                    body: serde_json::json!({
                        "tool": call.name,
                        "reason": reason,
                    }),
                    theme: Some("warning".to_owned()),
                    context_refs: vec!["spec:hooks".to_owned()],
                })
                .await;
            ToolResult {
                content: format!(
                    "runtime refused: tool `{}` denied by an operator PreToolUse \
                     hook: {reason}. Do not retry the same call unchanged; adjust \
                     the approach to satisfy the operator's policy or report the \
                     obstacle.",
                    call.name
                ),
                is_error: true,
            }
        } else if let Some(reason) = permission_refusal_reason {
            // mu-8stm.1: bounded approval gate timed out (no approver /
            // dropped channel) — fail closed, loudly, distinct from a human
            // denial so the model treats it as an obstacle to route around,
            // not a "the user said no".
            ToolResult {
                content: reason,
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
            if let Ok(mut cap) = capability.lock() {
                cap.consume_tool_call();
            }
            match tool {
                Some(t) => {
                    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
                    // mu-mu-solo-loop-terminate-5ek5: tool futures are
                    // awaited IN the loop task, so before this wrap a
                    // panicking tool unwound the whole agent loop —
                    // the input channel closed and every later ask
                    // got "session loop has terminated" (the 2026-06-07
                    // incident class). spawn_blocking-based tools were
                    // already isolated (panic → JoinError → error
                    // result); this extends the same contract to the
                    // async path: a panic is an is_error ToolResult,
                    // never a loop-killing condition.
                    use futures::FutureExt as _;
                    let tool_name_for_panic = call.name.clone();
                    let mut execute_fut = Box::pin(
                        std::panic::AssertUnwindSafe(
                            t.execute(call.arguments.clone().into(), cancel_rx),
                        )
                        .catch_unwind()
                        .map(move |res| {
                            res.unwrap_or_else(|panic| ToolResult {
                                content: format!(
                                    "tool `{tool_name_for_panic}` panicked: {}",
                                    panic_message(panic.as_ref())
                                ),
                                is_error: true,
                            })
                        }),
                    );

                    let tool_call_started_at = Instant::now();
                    let tool_state_started_unix_ms = now_unix_ms();
                    let mut tool_tick =
                        tokio::time::interval(std::time::Duration::from_millis(1000));
                    tool_tick.tick().await;

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
                                    let reason = "session cancelled while tool was executing";
                                    let remaining = calls.drain(..).collect();
                                    cancel_current_and_remaining(
                                        events,
                                        history,
                                        &mut tool_messages,
                                        call.clone(),
                                        remaining,
                                        reason,
                                        "session.cancel",
                                    )
                                    .await;
                                    return Ok(ExecuteToolsExit::Cancelled { tool_messages });
                                }
                                Some(AgentInput::CancelOutstanding { reason }) => {
                                    let _ = cancel_tx.send(());
                                    let remaining = calls.drain(..).collect();
                                    cancel_current_and_remaining(
                                        events,
                                        history,
                                        &mut tool_messages,
                                        call.clone(),
                                        remaining,
                                        &reason,
                                        "session.cancel_outstanding",
                                    )
                                    .await;
                                    return Ok(ExecuteToolsExit::OutstandingCancelled {
                                        reason,
                                        tool_messages,
                                    });
                                }
                                Some(input @ AgentInput::UserMessage(..))
                                | Some(input @ AgentInput::StartAutonomous { .. })
                                | Some(input @ AgentInput::ScheduleWakeup { .. })
                                | Some(input @ AgentInput::SwitchProvider { .. })
                                | Some(input @ AgentInput::WatchCompleted { .. })
                                | Some(input @ AgentInput::DialogueMessage { .. })
                                | Some(input @ AgentInput::MailboxMessage { .. }) => {
                                    buffered.push(input);
                                }
                                None => {
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
                // mu-uz0n layer 2: the moment the model invents a tool
                // name is the one moment it's receptive to discovery —
                // rank the bad name against the real surface and name
                // the near-misses in the error itself.
                None => ToolResult {
                    content: match crate::context::capability_hints::suggest_for_unknown_tool(
                        tools, &call.name,
                    ) {
                        Some(near) => format!(
                            "tool not found: {}. closest available: {near} — \
                             call `discover` with your intent for the full ranked list",
                            call.name
                        ),
                        None => format!("tool not found: {}", call.name),
                    },
                    is_error: true,
                },
            }
        };

        let verbatim = tool.map(|t| t.spec().verbatim_result).unwrap_or(false);
        finish_tool_call(events, history, &mut tool_messages, call, result, verbatim).await;
    }

    Ok(ExecuteToolsExit::Completed {
        tool_messages,
        buffered,
    })
}
