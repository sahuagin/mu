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

fn now_unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) async fn handle_execute_tools(
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
                arguments: call.arguments.clone().into(),
            })
            .await;

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

        // mu-8stm.1: a no-approver / dropped-channel timeout produces a
        // refusal distinct from a human denial, so the model doesn't read
        // "fail-closed, no approver" as "the user said no".
        let mut permission_refusal_reason: Option<String> = None;
        let permission_decision = if retry_refusal_reason.is_none()
            && validate_refusal_reason.is_none()
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
                                return Err(Outcome::Cancelled);
                            }
                            Some(AgentInput::CancelOutstanding { reason }) => {
                                if let Ok(mut pending) = pending_approvals.lock() {
                                    pending.remove(&request_id);
                                }
                                return Err(Outcome::OutstandingCancelled { reason });
                            }
                            Some(AgentInput::UserMessage(_))
                            | Some(AgentInput::StartAutonomous { .. })
                            | Some(AgentInput::ScheduleWakeup { .. })
                            | Some(AgentInput::SwitchProvider { .. })
                            | Some(AgentInput::WatchCompleted { .. })
                            | Some(AgentInput::MailboxMessage { .. }) => {
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
                    let mut execute_fut =
                        Box::pin(t.execute(call.arguments.clone().into(), cancel_rx));

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
                                    return Err(Outcome::Cancelled);
                                }
                                Some(AgentInput::CancelOutstanding { reason }) => {
                                    let _ = cancel_tx.send(());
                                    return Err(Outcome::OutstandingCancelled { reason });
                                }
                                Some(input @ AgentInput::UserMessage(_))
                                | Some(input @ AgentInput::StartAutonomous { .. })
                                | Some(input @ AgentInput::ScheduleWakeup { .. })
                                | Some(input @ AgentInput::SwitchProvider { .. })
                                | Some(input @ AgentInput::WatchCompleted { .. })
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
                None => ToolResult {
                    content: format!("tool not found: {}", call.name),
                    is_error: true,
                },
            }
        };

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
        let verbatim = tool.map(|t| t.spec().verbatim_result).unwrap_or(false);
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

    Ok((tool_messages, buffered))
}
