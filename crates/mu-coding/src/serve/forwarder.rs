//! Translate `AgentEvent`s from the loop into `session.*` JSON-RPC
//! notifications.
//!
//! ## Gateway re-entry seam (spec mu-046 INV-10)
//!
//! This module is the daemon-side terminus of the gateway re-entry
//! rule: slow external work (provider streams, tool execution) runs in
//! async gateways inside the agent loop, and its results re-enter the
//! session pipeline as SEQUENCED `AgentEvent`s on the loop's event
//! channel — never by mutating pipeline state from the side. The
//! forwarder consumes that one ordered stream and projects it into the
//! session's event log and the wire notification surface. Nothing
//! else writes gateway results into either projection, so within a
//! session the log order IS the processing order the loop observed.
//!
//! The same channel carries command receipts (spec mu-046 WP4): an
//! `ask_session` journaled into the session's log rides a
//! [`CommandTicket`] through the agent loop, and the terminal
//! `AgentEvent::Done` returns it here, where
//! [`forward_events`] writes the `CommandSucceeded`/`CommandFailed`
//! receipt — strict `append_command`, pairing by `command_event_id` —
//! into the same session log that holds the `CommandReceived`.
//!
//! ## Why this is split
//!
//! `translate_event` is a pure function: given a session id and an
//! AgentEvent, it returns either `None` (silently-dropped lifecycle
//! event) or a `(method_name, params_value)` pair that's ready to
//! emit. Tests target this function directly without spawning
//! anything — the queue topology test is separate (in serve_smoke).
//!
//! `forward_events` is the IO loop: queue read → translate → emit
//! via `NotificationWriter`. Thin glue.
//!
//! Lifecycle events (AgentStart, TurnStart, TurnEnd, MessageStart,
//! MessageEnd) translate to `None` — mu-001's notification surface
//! doesn't include them. We can amend mu-001 to add them when a
//! frontend needs them.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::mpsc;

use mu_core::agent::{AgentEvent, AgentMessage, StopReason, Usage};
use mu_core::command_journal::CommandTicket;
use mu_core::event_log::{EventActor, EventPayload, SessionEventLog};
use mu_core::protocol::{
    AssistantTextFinalizedEvent, AutonomousIterationCompletedEvent,
    AutonomousIterationStartedEvent, AutonomousTerminatedEvent, CalloutBody, CalloutEvent,
    DoneEvent, ErrorEvent, InputRequiredEvent, ProviderStatusEvent, TextDeltaEvent,
    ThinkingDeltaEvent, ThinkingFinalizedEvent, ToolCallCompletedEvent, ToolCallDeltaEvent,
    ToolCallStartedEvent, ToolOutcome,
};
use mu_core::session_status::SessionStatus;
use mu_core::transport::codes;
use mu_core::transport::NotificationWriter;

use super::provider_status::{ProviderCallState, ProviderStatusTracker};

/// Pure translation: AgentEvent → (method_name, params_value), or
/// None for events that don't have a wire-level representation in
/// mu-001 yet (lifecycle).
///
/// Returns None on `serde_json::to_value` failure too. In practice
/// the wire types are all `Serialize`-derived structs that can't
/// fail; the early return guards against any future struct gaining
/// a custom Serialize impl that errors.
pub fn translate_event(session_id: &str, event: AgentEvent) -> Option<(&'static str, Value)> {
    match event {
        AgentEvent::TextDelta { delta } => to_pair(
            TextDeltaEvent::METHOD,
            TextDeltaEvent {
                session_id: session_id.to_string(),
                delta,
            },
        ),
        AgentEvent::ThinkingDelta { delta } => to_pair(
            ThinkingDeltaEvent::METHOD,
            ThinkingDeltaEvent {
                session_id: session_id.to_string(),
                delta,
            },
        ),
        AgentEvent::AssistantTextFinalized { text } => to_pair(
            AssistantTextFinalizedEvent::METHOD,
            AssistantTextFinalizedEvent {
                session_id: session_id.to_string(),
                text,
            },
        ),
        AgentEvent::AssistantThinkingFinalized { text } => to_pair(
            ThinkingFinalizedEvent::METHOD,
            ThinkingFinalizedEvent {
                session_id: session_id.to_string(),
                text,
            },
        ),
        AgentEvent::ToolCallStarted {
            tool_call_id,
            tool_name,
            arguments,
        } => to_pair(
            ToolCallStartedEvent::METHOD,
            ToolCallStartedEvent {
                session_id: session_id.to_string(),
                tool_call_id,
                tool_name,
                arguments,
            },
        ),
        AgentEvent::ToolCallDelta {
            tool_call_id,
            name_delta,
            arguments_delta,
        } => to_pair(
            ToolCallDeltaEvent::METHOD,
            ToolCallDeltaEvent {
                session_id: session_id.to_string(),
                tool_call_id,
                name_delta,
                arguments_delta,
            },
        ),
        AgentEvent::ToolCallCompleted {
            tool_call_id,
            content,
            is_error,
        } => {
            let outcome = if is_error {
                ToolOutcome::Err { message: content }
            } else {
                // v1 wraps the textual result as a JSON string.
                // Real Provider impls will eventually produce
                // structured results; that's a future spec.
                ToolOutcome::Ok {
                    result: Value::String(content),
                }
            };
            to_pair(
                ToolCallCompletedEvent::METHOD,
                ToolCallCompletedEvent {
                    session_id: session_id.to_string(),
                    tool_call_id,
                    outcome,
                },
            )
        }
        AgentEvent::Done {
            stop_reason,
            usage,
            elapsed_ms,
            ..
        } => to_pair(
            DoneEvent::METHOD,
            DoneEvent {
                session_id: session_id.to_string(),
                stop_reason,
                usage,
                elapsed_ms,
            },
        ),
        AgentEvent::Error { message } => to_pair(
            ErrorEvent::METHOD,
            ErrorEvent {
                session_id: session_id.to_string(),
                message,
                detail: None,
            },
        ),
        AgentEvent::InputRequired {
            request_id,
            tool_call_id,
            tool_name,
            arguments,
            summary,
        } => to_pair(
            InputRequiredEvent::METHOD,
            InputRequiredEvent {
                session_id: session_id.to_string(),
                request_id,
                tool_call_id,
                tool_name,
                arguments,
                summary,
            },
        ),
        AgentEvent::Callout {
            category,
            title,
            body,
            theme,
            context_refs,
        } => {
            // Coerce the agent-loop-side Value into the wire-side
            // CalloutBody enum. JSON strings collapse to Text;
            // anything else stays Structured. The agent-loop's
            // `category` becomes the wire's `kind` (the agent-loop
            // already uses `kind` as the AgentEvent serde tag).
            let body_payload = match body {
                Value::String(s) => CalloutBody::Text(s),
                other => CalloutBody::Structured(other),
            };
            to_pair(
                CalloutEvent::METHOD,
                CalloutEvent {
                    session_id: session_id.to_string(),
                    kind: category,
                    title,
                    body: body_payload,
                    theme,
                    context_refs,
                },
            )
        }
        AgentEvent::ProviderStatus {
            state,
            started_at_unix_ms,
            elapsed_ms,
            bytes_received,
            tool_call_id,
        } => to_pair(
            ProviderStatusEvent::METHOD,
            ProviderStatusEvent {
                session_id: session_id.to_string(),
                kind: state,
                started_at_unix_ms,
                elapsed_ms,
                bytes_received,
                tool_call_id,
            },
        ),
        AgentEvent::AutonomousIterationStarted {
            iteration,
            motivation,
        } => to_pair(
            AutonomousIterationStartedEvent::METHOD,
            AutonomousIterationStartedEvent {
                session_id: session_id.to_string(),
                iteration,
                motivation,
            },
        ),
        AgentEvent::AutonomousIterationCompleted { iteration, outcome } => to_pair(
            AutonomousIterationCompletedEvent::METHOD,
            AutonomousIterationCompletedEvent {
                session_id: session_id.to_string(),
                iteration,
                outcome,
            },
        ),
        AgentEvent::AutonomousTerminated { reason } => to_pair(
            AutonomousTerminatedEvent::METHOD,
            AutonomousTerminatedEvent {
                session_id: session_id.to_string(),
                reason,
            },
        ),
        // Lifecycle events not in mu-001's notification surface.
        // ContextAssembly and CompactionAssembly land only in the
        // durable event log (mu-032 v1 / mu-za92 — see to_log_event);
        // wire-level exposure is a future TUI/web-ui feature when
        // there's a consumer.
        // mu-036 Phase C: schedule_wakeup parking lands durably (see
        // to_log_event) but has no wire-notification method in mu-036's
        // surface — the next AutonomousIterationStarted on wake carries
        // the wake reason as its motivation, which clients observe.
        AgentEvent::AutonomousScheduledWakeup { .. }
        | AgentEvent::AgentStart
        | AgentEvent::TurnStart
        | AgentEvent::TurnEnd
        | AgentEvent::MessageStart { .. }
        | AgentEvent::MessageEnd { .. }
        | AgentEvent::ContextAssembly { .. }
        | AgentEvent::CompactionAssembly { .. }
        | AgentEvent::ProviderSwitched { .. } => None,
    }
}

fn to_pair<T: serde::Serialize>(method: &'static str, value: T) -> Option<(&'static str, Value)> {
    serde_json::to_value(value).ok().map(|v| (method, v))
}

/// mu-035 Phase D: mirror the AgentEvent into the shared
/// ProviderStatusTracker so `daemon.outstanding_calls` and the
/// `was_in` field on `session.cancel_outstanding` see live state.
///
/// - `ProviderStatus` sets the tracker.
/// - `Done` / `Error` clears it — the ask is over and no provider
///   call is in flight until the next emit.
///
/// Other events are pass-through.
fn update_provider_status(event: &AgentEvent, tracker: &Arc<Mutex<ProviderStatusTracker>>) {
    match event {
        AgentEvent::ProviderStatus {
            state,
            started_at_unix_ms,
            tool_call_id,
            ..
        } => {
            if let Ok(mut t) = tracker.lock() {
                t.enter(ProviderCallState {
                    kind: *state,
                    started_at_unix_ms: *started_at_unix_ms,
                    tool_call_id: tool_call_id.clone(),
                });
            }
        }
        AgentEvent::Done { .. } | AgentEvent::Error { .. } => {
            if let Ok(mut t) = tracker.lock() {
                t.clear();
            }
        }
        _ => {}
    }
}

fn update_autonomy_active(event: &AgentEvent, active: &Arc<AtomicBool>) {
    match event {
        AgentEvent::AutonomousIterationStarted { .. }
        | AgentEvent::AutonomousIterationCompleted { .. }
        | AgentEvent::AutonomousScheduledWakeup { .. } => {
            active.store(true, Ordering::Release);
        }
        AgentEvent::AutonomousTerminated { .. } => {
            active.store(false, Ordering::Release);
        }
        _ => {}
    }
}

/// Shared live projections maintained by the forwarder for dispatch-time
/// queries/gates. Stored outside the event log so handlers can make cheap,
/// race-closing decisions; the event log remains the audit source.
#[derive(Clone)]
pub struct ForwarderLiveState {
    pub provider_status: Arc<Mutex<ProviderStatusTracker>>,
    pub autonomy_active: Arc<AtomicBool>,
}

/// IO loop: read events from `events_rx`, append durable events to
/// `event_log`, and emit wire notifications via `notif`. Exits when
/// `events_rx` closes.
///
/// The wire projection (notifications) and the durable projection
/// (event log) are derived from the same `AgentEvent` source. They
/// don't share rows — the wire wants per-delta granularity; the log
/// wants per-significant-event granularity.
#[allow(clippy::too_many_arguments)]
pub async fn forward_events(
    session_id: String,
    mut events_rx: mpsc::Receiver<AgentEvent>,
    notif: NotificationWriter,
    event_log: Arc<SessionEventLog>,
    live_state: ForwarderLiveState,
    daemon_id: String,
    status_watch_tx: Option<tokio::sync::watch::Sender<Option<SessionStatus>>>,
    hooks: Option<Arc<mu_core::hooks::HookEngine>>,
) {
    // spec mu-046 WP4: the most recent AgentEvent::Error message. The
    // loop emits Error then its terminal Done(Error) on the same
    // ordered channel, so consuming this at the Done gives the
    // CommandFailed receipt the real failure text instead of a
    // generic one. Local to this one ordered stream — not a side
    // table; correlation still comes from the ticket.
    let mut last_error_message: Option<String> = None;
    // mu-bb2v: tool_call_id → (tool_name, arguments), populated from
    // ToolCallStarted so the PostToolUse hook payload can carry the
    // tool's identity (ToolCallCompleted has only the id). Entries are
    // removed at Completed; the loop synthesizes Completed tombstones
    // for cancelled calls, so the map drains rather than leaks.
    let mut hook_tool_index: std::collections::HashMap<String, (String, Value)> =
        std::collections::HashMap::new();
    while let Some(event) = events_rx.recv().await {
        // mu-035 Phase D: mirror provider-call lifecycle into the
        // shared tracker so dispatch handlers (cancel_outstanding,
        // daemon.outstanding_calls) see authoritative live state.
        // The lock is held only for the sync mutate; no `.await`
        // runs while it's held.
        update_provider_status(&event, &live_state.provider_status);
        update_autonomy_active(&event, &live_state.autonomy_active);

        // mu-bb2v: observational lifecycle hooks ride the forwarder —
        // the one ordered stream that already sees every AgentEvent —
        // so the agent loop stays hook-free except for its PreToolUse
        // gate. Dispatch is fire-and-forget inside the engine; nothing
        // here blocks on a hook subprocess.
        if let Some(h) = &hooks {
            match &event {
                AgentEvent::ToolCallStarted {
                    tool_call_id,
                    tool_name,
                    arguments,
                } => {
                    if h.listens(mu_core::hooks::HookEventKind::PostToolUse) {
                        hook_tool_index
                            .insert(tool_call_id.clone(), (tool_name.clone(), arguments.clone()));
                    }
                }
                AgentEvent::ToolCallCompleted {
                    tool_call_id,
                    content,
                    is_error,
                } => {
                    if let Some((tool_name, tool_input)) = hook_tool_index.remove(tool_call_id) {
                        h.observe_post_tool_use(&tool_name, Some(&tool_input), content, *is_error);
                    }
                }
                AgentEvent::Done {
                    stop_reason,
                    turn_count,
                    ..
                } => {
                    // snake_case wire form ("end_turn"), falling back to
                    // Debug if serialization ever fails.
                    let reason = serde_json::to_value(stop_reason)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_else(|| format!("{stop_reason:?}"));
                    h.observe_stop(&reason, *turn_count);
                }
                _ => {}
            }
        }

        if let AgentEvent::Error { message } = &event {
            last_error_message = Some(message.clone());
        }

        let recompute_status = should_recompute_status(&event);

        // Downstream record, not intake: commands are journaled
        // upstream — fsync'd before processing (spec mu-046). What
        // lands here are gateway RESULTS and state transitions
        // flowing back into the session's log as sequenced inputs
        // (INV-10). Streaming deltas + lifecycle ticks are dropped.
        if let Some((actor, mut payload)) = to_log_event(&event) {
            // ContextAssembly payload doesn't know the session's
            // provider/model when produced (the AgentLoop has the
            // info but doesn't pass it through). Patch it in here
            // by reading the SessionCreated event we already
            // recorded at session-start.
            if let EventPayload::ContextAssembly {
                provider_kind,
                model,
                ..
            } = &mut payload
            {
                if let Some((kind, m)) = event_log.provider_info() {
                    *provider_kind = kind;
                    *model = m;
                }
            }
            event_log.append(actor, payload);
        }
        // spec mu-046 WP4: completion receipts for journaled asks.
        // Each ticket the terminal Done carries becomes one receipt
        // row in the SAME session log that holds its CommandReceived
        // — CommandSucceeded on a normal stop, CommandFailed on
        // Aborted/Error. Written via the strict append_command
        // (receipts are outcomes, but a fsync'd receipt before the
        // wire notification keeps the log self-consistent); an append
        // failure is logged and the CommandReceived stays an orphan —
        // the legible marker (INV-4).
        if let AgentEvent::Done {
            stop_reason,
            turn_count,
            usage,
            elapsed_ms,
            command_receipts,
        } = &event
        {
            for ticket in command_receipts {
                let payload = ask_receipt_for(
                    ticket,
                    *stop_reason,
                    *turn_count,
                    *usage,
                    *elapsed_ms,
                    last_error_message.as_deref(),
                    now_unix_ms(),
                );
                if let Err(err) = event_log.append_command(EventActor::System, payload) {
                    tracing::error!(
                        %err,
                        session_id = %session_id,
                        command_event_id = ticket.command_event_id,
                        "ask receipt append failed; command stays an orphan in the session log"
                    );
                }
            }
            last_error_message = None;
        }
        // mu-5g7i / spec mu-040: at every task termination (Done or
        // Error AgentEvent), emit one TaskTelemetry envelope. This is
        // the forensics-axis foundation — downstream classifier
        // (mu-8alb) and analytics sink (mu-8ypx) project from these.
        if let Some(telemetry) = task_telemetry_for(&session_id, &event, event_log.provider_info())
        {
            event_log.append(EventActor::System, telemetry);
        }

        // MCP status projection: recompute SessionStatus and push
        // to watch subscribers on status-changing events.
        if recompute_status {
            if let Some(ref tx) = status_watch_tx {
                let status = compute_status(
                    &session_id,
                    &daemon_id,
                    &event_log,
                    &live_state.provider_status,
                );
                let _ = tx.send(Some(status));
            }
        }

        // Wire projection: translate to mu-001 notification surface.
        if let Some((method, params)) = translate_event(&session_id, event) {
            let _ = notif.emit(method, params).await;
        }
    }

    // If the agent task dies after `session.start_autonomous` claimed the
    // live gate but before it emits a normal AutonomousTerminated event,
    // closing the event channel is our last chance to avoid wedging the
    // session as permanently busy.
    live_state.autonomy_active.store(false, Ordering::Release);
}

fn should_recompute_status(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::ProviderStatus { .. }
            | AgentEvent::Done { .. }
            | AgentEvent::Error { .. }
            | AgentEvent::ToolCallStarted { .. }
            | AgentEvent::ToolCallCompleted { .. }
            | AgentEvent::MessageEnd { .. }
    )
}

fn compute_status(
    session_id: &str,
    daemon_id: &str,
    event_log: &Arc<SessionEventLog>,
    provider_status: &Arc<Mutex<ProviderStatusTracker>>,
) -> SessionStatus {
    let (provider_kind, model) = event_log.provider_info().unwrap_or_default();
    // Use live_usage (per-model-call) for real-time token counts,
    // rather than cumulative_usage (per-ask Done) which only updates
    // when the full ask completes.
    let (usage, last_call_input) = event_log.live_usage();
    let snap = provider_status
        .lock()
        .ok()
        .and_then(|t| t.snapshot())
        .map(|s| mu_core::session_status::ProviderSnapshot {
            kind: s.kind,
            started_at_unix_ms: s.started_at_unix_ms,
            now_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        });

    // Soft/hard limits come from the SessionConfigResolved event the
    // session handler recorded at creation (and re-recorded on a live
    // route change). No hardcoded window table, no 150k magic constant:
    // this projection reads the same resolved config the compaction
    // trigger uses, so the meter denominator and the compaction point are
    // guaranteed to be the same number. The fill is the last model call's
    // total input tokens; pressure% (= fill / soft) is computed in
    // `SessionStatus::compute`. See `mu_core::session_status` for the
    // soft-limit / hard-limit / fill vocabulary.
    let (context_soft_limit, context_hard_limit) = event_log
        .context_limits()
        .map_or((None, None), |(soft, hard, _max_output)| (Some(soft), hard));

    SessionStatus::compute(mu_core::session_status::StatusInputs {
        session_id,
        daemon_id,
        provider_kind: &provider_kind,
        model: &model,
        cumulative_usage: usage.as_ref(),
        ask_count: event_log.ask_count(),
        tool_call_count: event_log.tool_call_count(),
        elapsed_total_ms: event_log.elapsed_total_ms(),
        provider_status: snap,
        context_soft_limit,
        context_hard_limit,
        context_used_tokens: last_call_input,
    })
}

/// mu-5g7i: build a `TaskTelemetry` payload for terminal `AgentEvent`s
/// (Done, Error). Non-terminal events return None. Kept pure so tests
/// can verify envelope shape across exit paths without spinning up the
/// full forwarder loop.
///
/// `provider_info` is what `SessionEventLog::provider_info()` returns at
/// emit time (Some((kind, model)) once SessionCreated has been recorded;
/// None before then — should not happen at task-end in practice, but we
/// emit defensively with empty strings if it does).
pub(crate) fn task_telemetry_for(
    session_id: &str,
    event: &AgentEvent,
    provider_info: Option<(String, String)>,
) -> Option<EventPayload> {
    use mu_core::agent::StopReason;
    use mu_core::event_log::TaskExitReason;

    let now_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let task_id = format!(
        "task-{:020}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );

    let (exit_reason, wall_clock_ms, usage) = match event {
        AgentEvent::Done {
            stop_reason,
            usage,
            elapsed_ms,
            ..
        } => {
            let reason = match stop_reason {
                StopReason::Aborted => TaskExitReason::Cancelled,
                _ => TaskExitReason::Done,
            };
            (reason, *elapsed_ms, *usage)
        }
        AgentEvent::Error { .. } => (TaskExitReason::Error, None, None),
        _ => return None,
    };

    let (provider_kind, model) = provider_info.unwrap_or_default();

    Some(EventPayload::TaskTelemetry {
        task_id,
        session_id: session_id.to_owned(),
        parent_task_id: None,
        provider_kind,
        model,
        model_version: None,
        started_at_unix_ms: None, // session-local timing not wired yet (mu-040 MVP)
        ended_at_unix_ms: now_unix_ms,
        wall_clock_ms,
        prompt_tokens: usage.map(|u| u.input_tokens),
        completion_tokens: usage.map(|u| u.output_tokens),
        cache_read_tokens: usage.and_then(|u| u.cache_read_input_tokens),
        cache_write_tokens: usage.and_then(|u| u.cache_creation_input_tokens),
        cache_write_5m_tokens: usage.and_then(|u| u.cache_creation_5m_input_tokens),
        cache_write_1h_tokens: usage.and_then(|u| u.cache_creation_1h_input_tokens),
        tools_granted: Vec::new(),
        tools_actually_called: Vec::new(),
        exit_reason,
        max_budget_usd: None,
        actual_spend_usd: None,
        local_hour: None,
        day_of_week: None,
        tz: None,
    })
}

/// spec mu-046 WP4: project one [`CommandTicket`] + the terminal
/// Done's outcome into the session-log receipt for that ask. Pure for
/// testability; `forward_events` appends the result via the strict
/// `append_command` path.
///
/// Mapping (spec "Receipt semantics", documented decisions):
/// - normal stops (`EndTurn`, `IterationCap`, …) → `CommandSucceeded`
///   wrapping the original ask; `result` summarizes the turn the same
///   way the wire `session.done` does.
/// - `StopReason::Error` → `CommandFailed`, carrying the loop's last
///   `AgentEvent::Error` message when available.
/// - `StopReason::Aborted` (cancel_session / cancel_outstanding) →
///   `CommandFailed`, NOT `CommandRejected`: the ask was accepted and
///   entered processing — abort is a processing outcome, and Rejected
///   is reserved for pre-handler refusals (auth/validation/routing).
pub(crate) fn ask_receipt_for(
    ticket: &CommandTicket,
    stop_reason: StopReason,
    turn_count: u32,
    usage: Option<Usage>,
    elapsed_ms: Option<u64>,
    error_message: Option<&str>,
    now_unix_ms: u64,
) -> EventPayload {
    // Receipt elapsed = border crossing → completion (not just the
    // turn's wall time — a queued ask waited first).
    let receipt_elapsed_ms = now_unix_ms.saturating_sub(ticket.received_at_unix_ms);
    match stop_reason {
        StopReason::Aborted => EventPayload::CommandFailed {
            command_event_id: ticket.command_event_id,
            command: ticket.echo.clone(),
            code: codes::INTERNAL_ERROR,
            message: "ask aborted before completion (cancelled)".to_string(),
            elapsed_ms: receipt_elapsed_ms,
        },
        StopReason::Error => EventPayload::CommandFailed {
            command_event_id: ticket.command_event_id,
            command: ticket.echo.clone(),
            code: codes::INTERNAL_ERROR,
            message: error_message
                .unwrap_or("ask terminated with an error")
                .to_string(),
            elapsed_ms: receipt_elapsed_ms,
        },
        _ => EventPayload::CommandSucceeded {
            command_event_id: ticket.command_event_id,
            command: ticket.echo.clone(),
            result: serde_json::json!({
                "stop_reason": stop_reason,
                "turn_count": turn_count,
                "usage": usage,
                "elapsed_ms": elapsed_ms,
            }),
            elapsed_ms: receipt_elapsed_ms,
        },
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Translate an `AgentEvent` into a durable-log entry, or None if the
/// event isn't worth recording (text deltas, lifecycle ticks). Kept
/// pure for testability.
pub(crate) fn to_log_event(event: &AgentEvent) -> Option<(EventActor, EventPayload)> {
    match event {
        AgentEvent::MessageEnd { message } => match message {
            AgentMessage::User { content } => Some((
                EventActor::User,
                EventPayload::UserMessage {
                    content: content.clone(),
                },
            )),
            AgentMessage::Assistant(am) => Some((
                EventActor::Agent,
                EventPayload::AssistantMessageEvent {
                    message: am.clone(),
                },
            )),
            AgentMessage::ToolResult { .. } => {
                // Tool results are recorded via the dedicated
                // ToolCallCompleted event; skip the duplicate.
                None
            }
        },
        AgentEvent::ToolCallStarted {
            tool_call_id,
            tool_name,
            arguments,
        } => Some((
            EventActor::Agent,
            EventPayload::ToolCall {
                call_id: tool_call_id.clone(),
                name: tool_name.clone(),
                arguments: arguments.clone(),
            },
        )),
        AgentEvent::ToolCallCompleted {
            tool_call_id,
            content,
            is_error,
        } => Some((
            EventActor::Tool {
                name: "unknown".to_owned(), // tool name not on the event; future hardening
            },
            EventPayload::ToolResult {
                call_id: tool_call_id.clone(),
                content: content.clone(),
                is_error: *is_error,
            },
        )),
        AgentEvent::Done {
            stop_reason,
            turn_count,
            usage,
            elapsed_ms,
            // command_receipts are NOT part of the durable Done row —
            // each ticket becomes its own receipt row (see
            // forward_events), which carries the correlation.
            ..
        } => Some((
            EventActor::Agent,
            EventPayload::Done {
                stop_reason: *stop_reason,
                turn_count: *turn_count,
                usage: *usage,
                elapsed_ms: *elapsed_ms,
            },
        )),
        AgentEvent::Error { message } => Some((
            EventActor::Agent,
            EventPayload::Error {
                message: message.clone(),
            },
        )),
        AgentEvent::Callout {
            category,
            title,
            body,
            theme,
            context_refs,
        } => Some((
            EventActor::Agent,
            EventPayload::Callout {
                category: category.clone(),
                title: title.clone(),
                body: body.clone(),
                theme: theme.clone(),
                context_refs: context_refs.clone(),
            },
        )),
        AgentEvent::ContextAssembly {
            model_call_id,
            message_count,
            user_message_count,
            assistant_message_count,
            tool_result_count,
            tool_count,
            context_sizes,
            renderer,
            cache_strategy,
            span_count,
            cache_boundary_count,
            first_span_ids,
            prefix_hash,
            prefix_span_hashes,
        } => {
            // ContextAssembly is recorded with the session's
            // provider/model from the log's SessionCreated event.
            // We need access to the log here to look that up; the
            // forwarder receives `event_log` as a parameter so we
            // could pass it down — but to_log_event is a pure
            // function for testability. Solution: the caller
            // (forward_events loop) injects the provider info into
            // the payload right before append. Here we just emit a
            // "placeholder" payload; the actual log append happens
            // in forward_events with provider info filled in.
            //
            // For the wire-level projection, we also need to emit
            // a notification — that happens via translate_event
            // which doesn't go through to_log_event. So the wire
            // surface is separate.
            //
            // Encode as ContextAssembly with empty provider info;
            // forward_events will replace before storing.
            Some((
                EventActor::System,
                EventPayload::ContextAssembly {
                    model_call_id: *model_call_id,
                    message_count: *message_count,
                    user_message_count: *user_message_count,
                    assistant_message_count: *assistant_message_count,
                    tool_result_count: *tool_result_count,
                    tool_count: *tool_count,
                    // mu-heqf: total + per-section estimate measured
                    // by the loop's renderer at assembly time.
                    token_count_estimate: context_sizes.as_ref().map(|s| s.total),
                    token_breakdown: context_sizes
                        .as_ref()
                        .map(|s| s.by_kind.clone())
                        .unwrap_or_default(),
                    provider_kind: String::new(),
                    model: String::new(),
                    renderer: renderer.clone(),
                    cache_strategy: cache_strategy.clone(),
                    span_count: *span_count,
                    cache_boundary_count: *cache_boundary_count,
                    first_span_ids: first_span_ids.clone(),
                    prefix_hash: prefix_hash.clone(),
                    prefix_span_hashes: prefix_span_hashes.clone(),
                },
            ))
        }
        AgentEvent::ProviderStatus {
            state,
            started_at_unix_ms,
            elapsed_ms,
            bytes_received,
            tool_call_id,
        } => Some((
            EventActor::System,
            EventPayload::ProviderStatusUpdate {
                state: *state,
                started_at_unix_ms: *started_at_unix_ms,
                elapsed_ms: *elapsed_ms,
                bytes_received: *bytes_received,
                tool_call_id: tool_call_id.clone(),
            },
        )),
        AgentEvent::AutonomousIterationStarted { iteration, motivation } => Some((
            EventActor::Agent,
            EventPayload::AutonomousIterationStarted {
                iteration: *iteration,
                motivation: motivation.clone(),
            },
        )),
        AgentEvent::AutonomousIterationCompleted { iteration, outcome } => Some((
            EventActor::Agent,
            EventPayload::AutonomousIterationCompleted {
                iteration: *iteration,
                outcome: outcome.clone(),
            },
        )),
        AgentEvent::AutonomousScheduledWakeup {
            wake_at_unix_ms,
            reason,
        } => Some((
            EventActor::Agent,
            EventPayload::AutonomousScheduledWakeup {
                wake_at_unix_ms: *wake_at_unix_ms,
                reason: reason.clone(),
            },
        )),
        AgentEvent::AutonomousTerminated { reason } => Some((
            EventActor::Agent,
            EventPayload::AutonomousTerminated {
                reason: reason.clone(),
            },
        )),
        AgentEvent::TextDelta { .. }
        // mu-wk2: AssistantTextFinalized is a transient wire-level
        // notification to prevent TUI flicker. The durable event is
        // the AssistantMessageEvent from MessageEnd.
        | AgentEvent::AssistantTextFinalized { .. }
        // mu-upk2: thinking + partial-tool-call streaming are transient
        // wire-level notifications too. Finalized reasoning persists
        // durably inside the AssistantMessageEvent (its ContentBlock::
        // Thinking blocks), and the finalized tool call lands as the
        // ToolCall payload (ToolCallStarted, above) — so the deltas and
        // the thinking-finalized signal carry no extra durable record.
        | AgentEvent::ThinkingDelta { .. }
        | AgentEvent::AssistantThinkingFinalized { .. }
        | AgentEvent::ToolCallDelta { .. }
        | AgentEvent::MessageStart { .. }
        | AgentEvent::AgentStart
        | AgentEvent::TurnStart
        | AgentEvent::TurnEnd
        // InputRequired is a transient wire-level prompt; the
        // resulting ToolCall (approved) or ToolResult (denied)
        // already lands in the log.
        | AgentEvent::InputRequired { .. } => None,
        // mu-za92: compaction lands durably, decisions and all. The
        // in-memory rope log this used to defer to vanishes on
        // process exit — the JSONL is the only record that survives
        // to answer "what was ejected and kept?"
        AgentEvent::CompactionAssembly {
            model_call_id,
            policy_id,
            tokens_before,
            tokens_after,
            decisions,
            wall_clock_us,
            predicted_tokens,
            compaction_threshold,
            output_reserve,
        } => Some((
            EventActor::System,
            EventPayload::CompactionAssembly {
                model_call_id: *model_call_id,
                policy_id: policy_id.clone(),
                tokens_before: *tokens_before as u64,
                tokens_after: *tokens_after as u64,
                decisions: decisions.clone(),
                wall_clock_us: *wall_clock_us,
                // mu-a79g: carry the trigger inputs onto the durable
                // event so the effective compaction point is
                // reconstructable from the JSONL alone.
                predicted_tokens: *predicted_tokens as u64,
                compaction_threshold: *compaction_threshold as u64,
                output_reserve: *output_reserve as u64,
            },
        )),
        AgentEvent::ProviderSwitched {
            old_provider_kind,
            old_model,
            new_provider_kind,
            new_model,
            usage_semantics,
        } => Some((
            EventActor::System,
            EventPayload::ProviderSwitched {
                old_provider_kind: old_provider_kind.clone(),
                old_model: old_model.clone(),
                new_provider_kind: new_provider_kind.clone(),
                new_model: new_model.clone(),
                context_soft_limit: None,
                context_hard_limit: None,
                // mu-rf9x: the new provider's accounting convention,
                // re-registered durably at the switch.
                usage_semantics: Some(*usage_semantics),
            },
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use mu_core::agent::AgentEvent;

    #[test]
    fn translate_text_delta() {
        let (method, params) = translate_event("s1", AgentEvent::TextDelta { delta: "hi".into() })
            .expect("expected Some");
        assert_eq!(method, "session.text_delta");
        assert_eq!(params["session_id"], "s1");
        assert_eq!(params["delta"], "hi");
    }

    #[test]
    fn translate_callout_text_body() {
        let (method, params) = translate_event(
            "s2",
            AgentEvent::Callout {
                category: "observation".into(),
                title: "spotted".into(),
                body: json!("line 5"),
                theme: Some("info".into()),
                context_refs: vec!["spec:mu-016".into()],
            },
        )
        .expect("expected Some");
        assert_eq!(method, "session.callout");
        assert_eq!(params["kind"], "observation");
        assert_eq!(params["title"], "spotted");
        assert_eq!(params["body"], "line 5");
        assert_eq!(params["theme"], "info");
        assert_eq!(params["context_refs"][0], "spec:mu-016");
    }

    #[test]
    fn translate_callout_structured_body() {
        let (method, params) = translate_event(
            "s3",
            AgentEvent::Callout {
                category: "memory".into(),
                title: "recalled".into(),
                body: json!({"id": "abc", "preview": "..."}),
                theme: None,
                context_refs: vec![],
            },
        )
        .expect("expected Some");
        assert_eq!(method, "session.callout");
        assert_eq!(params["body"]["id"], "abc");
        // Optionals omitted when empty/None.
        assert!(params.as_object().unwrap().get("theme").is_none());
        assert!(params.as_object().unwrap().get("context_refs").is_none());
    }

    #[test]
    fn translate_tool_call_completed_ok() {
        let (method, params) = translate_event(
            "s",
            AgentEvent::ToolCallCompleted {
                tool_call_id: "t1".into(),
                content: "result".into(),
                is_error: false,
            },
        )
        .expect("expected Some");
        assert_eq!(method, "session.tool_call_completed");
        assert_eq!(params["outcome"]["kind"], "ok");
        assert_eq!(params["outcome"]["result"], "result");
    }

    #[test]
    fn translate_tool_call_completed_err() {
        let (_, params) = translate_event(
            "s",
            AgentEvent::ToolCallCompleted {
                tool_call_id: "t1".into(),
                content: "boom".into(),
                is_error: true,
            },
        )
        .expect("expected Some");
        assert_eq!(params["outcome"]["kind"], "err");
        assert_eq!(params["outcome"]["message"], "boom");
    }

    #[test]
    fn translate_lifecycle_events_drop() {
        for event in [
            AgentEvent::AgentStart,
            AgentEvent::TurnStart,
            AgentEvent::TurnEnd,
            AgentEvent::MessageStart {
                message: mu_core::agent::AgentMessage::User {
                    content: "x".into(),
                },
            },
            AgentEvent::MessageEnd {
                message: mu_core::agent::AgentMessage::User {
                    content: "x".into(),
                },
            },
        ] {
            assert!(
                translate_event("s", event).is_none(),
                "lifecycle events should translate to None"
            );
        }
    }

    #[test]
    fn translate_thinking_and_tool_call_delta_events() {
        // ThinkingDelta → session.thinking_delta
        let (method, params) = translate_event(
            "s1",
            AgentEvent::ThinkingDelta {
                delta: "pondering".into(),
            },
        )
        .expect("thinking delta translates");
        assert_eq!(method, "session.thinking_delta");
        assert_eq!(params["session_id"], "s1");
        assert_eq!(params["delta"], "pondering");

        // AssistantThinkingFinalized → session.thinking_finalized
        let (method, params) = translate_event(
            "s1",
            AgentEvent::AssistantThinkingFinalized {
                text: "final reasoning".into(),
            },
        )
        .expect("thinking finalized translates");
        assert_eq!(method, "session.thinking_finalized");
        assert_eq!(params["text"], "final reasoning");

        // ToolCallDelta(start: name only) → session.tool_call_delta, args omitted.
        let (method, params) = translate_event(
            "s1",
            AgentEvent::ToolCallDelta {
                tool_call_id: "toolu_1".into(),
                name_delta: Some("read".into()),
                arguments_delta: None,
            },
        )
        .expect("tool call delta translates");
        assert_eq!(method, "session.tool_call_delta");
        assert_eq!(params["tool_call_id"], "toolu_1");
        assert_eq!(params["name_delta"], "read");
        assert!(params.get("arguments_delta").is_none());

        // ToolCallDelta(args fragment) → arguments_delta present, name omitted.
        let (_method, params) = translate_event(
            "s1",
            AgentEvent::ToolCallDelta {
                tool_call_id: "toolu_1".into(),
                name_delta: None,
                arguments_delta: Some("{\"path\":".into()),
            },
        )
        .expect("tool call delta translates");
        assert_eq!(params["arguments_delta"], "{\"path\":");
        assert!(params.get("name_delta").is_none());
    }

    // ─── event-log projection tests ──────────────────────────────

    #[test]
    fn log_drops_thinking_and_tool_call_delta_events() {
        for ev in [
            AgentEvent::ThinkingDelta { delta: "x".into() },
            AgentEvent::AssistantThinkingFinalized { text: "x".into() },
            AgentEvent::ToolCallDelta {
                tool_call_id: "t".into(),
                name_delta: None,
                arguments_delta: Some("{".into()),
            },
        ] {
            assert!(
                to_log_event(&ev).is_none(),
                "thinking/tool-delta events carry no durable log row: {ev:?}"
            );
        }
    }

    #[test]
    fn log_drops_streaming_and_lifecycle_events() {
        // None of these should produce a log entry.
        let cases = vec![
            AgentEvent::TextDelta { delta: "x".into() },
            AgentEvent::AgentStart,
            AgentEvent::TurnStart,
            AgentEvent::TurnEnd,
            AgentEvent::MessageStart {
                message: mu_core::agent::AgentMessage::User {
                    content: "x".into(),
                },
            },
            AgentEvent::InputRequired {
                request_id: "req-x".into(),
                tool_call_id: "call-x".into(),
                tool_name: "edit".into(),
                arguments: json!({}),
                summary: "...".into(),
            },
        ];
        for ev in cases {
            assert!(
                to_log_event(&ev).is_none(),
                "expected no log entry for {ev:?}"
            );
        }
    }

    #[test]
    fn log_records_provider_status_update() {
        // mu-pex Phase 1.5: AgentEvent::ProviderStatus is now durable
        // (previously dropped). The shape carries through with all
        // fields preserved and EventActor::System.
        let ev = AgentEvent::ProviderStatus {
            state: mu_core::protocol::ProviderStatusKind::Streaming,
            started_at_unix_ms: 1_000_000,
            elapsed_ms: 0,
            bytes_received: Some(512),
            tool_call_id: None,
        };
        let (actor, payload) = to_log_event(&ev).expect("ProviderStatus → log");
        assert!(matches!(actor, EventActor::System));
        match payload {
            EventPayload::ProviderStatusUpdate {
                state,
                started_at_unix_ms,
                elapsed_ms,
                bytes_received,
                tool_call_id,
            } => {
                assert_eq!(state, mu_core::protocol::ProviderStatusKind::Streaming);
                assert_eq!(started_at_unix_ms, 1_000_000);
                assert_eq!(elapsed_ms, 0);
                assert_eq!(bytes_received, Some(512));
                assert!(tool_call_id.is_none());
            }
            other => panic!("expected ProviderStatusUpdate, got {other:?}"),
        }
    }

    #[test]
    fn log_records_user_message_from_message_end() {
        let ev = AgentEvent::MessageEnd {
            message: mu_core::agent::AgentMessage::User {
                content: "hello".into(),
            },
        };
        let (actor, payload) = to_log_event(&ev).expect("user MessageEnd → log");
        assert!(matches!(actor, EventActor::User));
        assert!(matches!(
            payload,
            EventPayload::UserMessage { ref content } if content == "hello"
        ));
    }

    #[test]
    fn log_records_assistant_message_with_usage_intact() {
        use mu_core::agent::{AssistantMessage, ContentBlock, StopReason, Usage};
        let am = AssistantMessage {
            content: vec![ContentBlock::Text {
                text: "hi back".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: Some(Usage {
                input_tokens: 100,
                output_tokens: 5,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                cache_creation_5m_input_tokens: None,
                cache_creation_1h_input_tokens: None,
                reasoning_tokens: None,
            }),
        };
        let ev = AgentEvent::MessageEnd {
            message: mu_core::agent::AgentMessage::Assistant(am.clone()),
        };
        let (actor, payload) = to_log_event(&ev).expect("assistant MessageEnd → log");
        assert!(matches!(actor, EventActor::Agent));
        match payload {
            EventPayload::AssistantMessageEvent { message } => {
                assert_eq!(message, am);
            }
            other => panic!("expected AssistantMessageEvent, got {other:?}"),
        }
    }

    #[test]
    fn log_records_tool_call_started_as_tool_call() {
        let ev = AgentEvent::ToolCallStarted {
            tool_call_id: "call_xyz".into(),
            tool_name: "bash".into(),
            arguments: json!({"command": "echo hi"}),
        };
        let (actor, payload) = to_log_event(&ev).expect("ToolCallStarted → log");
        assert!(matches!(actor, EventActor::Agent));
        match payload {
            EventPayload::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                assert_eq!(call_id, "call_xyz");
                assert_eq!(name, "bash");
                assert_eq!(arguments["command"], "echo hi");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn log_records_tool_call_completed_as_tool_result() {
        let ev = AgentEvent::ToolCallCompleted {
            tool_call_id: "call_xyz".into(),
            content: "hi\nelapsed: 7ms".into(),
            is_error: false,
        };
        let (actor, payload) = to_log_event(&ev).expect("ToolCallCompleted → log");
        // Tool actor — we know the result came from a tool but the
        // tool's name isn't on the AgentEvent (future hardening
        // could thread it through).
        assert!(matches!(actor, EventActor::Tool { .. }));
        match payload {
            EventPayload::ToolResult {
                call_id,
                content,
                is_error,
            } => {
                assert_eq!(call_id, "call_xyz");
                assert!(content.contains("hi"));
                assert!(!is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn log_records_tool_error_with_is_error_true() {
        let ev = AgentEvent::ToolCallCompleted {
            tool_call_id: "call_abc".into(),
            content: "bash: command not allowed".into(),
            is_error: true,
        };
        let (_actor, payload) = to_log_event(&ev).expect("error tool result → log");
        match payload {
            EventPayload::ToolResult { is_error, .. } => {
                assert!(is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn log_full_tool_loop_round_trip() {
        // Simulate a complete tool-use turn: text → tool call →
        // (later) tool result. Confirm the log captures both
        // sides of the loop in order.
        let log = Arc::new(SessionEventLog::new("tool-loop"));

        // 1. Agent decides to call a tool.
        let started = AgentEvent::ToolCallStarted {
            tool_call_id: "call_1".into(),
            tool_name: "bash".into(),
            arguments: json!({"command": "echo audit"}),
        };
        if let Some((actor, payload)) = to_log_event(&started) {
            log.append(actor, payload);
        }

        // 2. Tool returns.
        let completed = AgentEvent::ToolCallCompleted {
            tool_call_id: "call_1".into(),
            content: "audit\nelapsed: 5ms".into(),
            is_error: false,
        };
        if let Some((actor, payload)) = to_log_event(&completed) {
            log.append(actor, payload);
        }

        // Verify both sides landed, in order, with matching call_id.
        let entries = log.snapshot();
        assert_eq!(entries.len(), 2);
        match &entries[0].payload {
            EventPayload::ToolCall { call_id, name, .. } => {
                assert_eq!(call_id, "call_1");
                assert_eq!(name, "bash");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
        match &entries[1].payload {
            EventPayload::ToolResult {
                call_id, is_error, ..
            } => {
                assert_eq!(call_id, "call_1");
                assert!(!is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
        assert_eq!(log.tool_call_count(), 1);
    }

    #[test]
    fn log_records_done_with_full_payload() {
        use mu_core::agent::{StopReason, Usage};
        let ev = AgentEvent::Done {
            stop_reason: StopReason::EndTurn,
            turn_count: 2,
            usage: Some(Usage {
                input_tokens: 200,
                output_tokens: 50,
                cache_read_input_tokens: Some(15),
                cache_creation_input_tokens: None,
                cache_creation_5m_input_tokens: None,
                cache_creation_1h_input_tokens: None,
                reasoning_tokens: None,
            }),
            elapsed_ms: Some(1234),
            command_receipts: Vec::new(),
        };
        let (actor, payload) = to_log_event(&ev).expect("Done → log");
        assert!(matches!(actor, EventActor::Agent));
        match payload {
            EventPayload::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 2,
                usage: Some(u),
                elapsed_ms: Some(1234),
            } => {
                assert_eq!(u.input_tokens, 200);
                assert_eq!(u.output_tokens, 50);
                assert_eq!(u.cache_read_input_tokens, Some(15));
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn log_projection_over_a_full_event_sequence() {
        use mu_core::agent::{AssistantMessage, ContentBlock, StopReason, Usage};

        let log = Arc::new(SessionEventLog::new("test-session"));

        // Hand-run the log-projection step the forwarder takes
        // internally. (The wire-projection branch is exercised by
        // the serve_smoke integration tests; here we focus on the
        // log + cumulative-usage path.)
        let sequence = vec![
            AgentEvent::AgentStart,
            AgentEvent::TurnStart,
            AgentEvent::TextDelta { delta: "hi".into() },
            AgentEvent::MessageEnd {
                message: mu_core::agent::AgentMessage::Assistant(AssistantMessage {
                    content: vec![ContentBlock::Text { text: "hi".into() }],
                    stop_reason: StopReason::EndTurn,
                    usage: Some(Usage {
                        input_tokens: 42,
                        output_tokens: 7,
                        cache_read_input_tokens: None,
                        cache_creation_input_tokens: None,
                        cache_creation_5m_input_tokens: None,
                        cache_creation_1h_input_tokens: None,
                        reasoning_tokens: None,
                    }),
                }),
            },
            AgentEvent::TurnEnd,
            AgentEvent::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 1,
                usage: Some(Usage {
                    input_tokens: 42,
                    output_tokens: 7,
                    cache_read_input_tokens: None,
                    cache_creation_input_tokens: None,
                    cache_creation_5m_input_tokens: None,
                    cache_creation_1h_input_tokens: None,
                    reasoning_tokens: None,
                }),
                elapsed_ms: Some(123),
                command_receipts: Vec::new(),
            },
        ];
        for ev in sequence {
            if let Some((actor, payload)) = to_log_event(&ev) {
                log.append(actor, payload);
            }
        }

        // Log projection: only the significant events landed.
        let entries = log.snapshot();
        let kinds: Vec<&str> = entries.iter().map(|e| e.payload.kind_str()).collect();
        assert_eq!(kinds, vec!["assistant_message_event", "done"]);

        // Cumulative usage derivation.
        let cumulative = log.cumulative_usage().expect("Done had usage");
        assert_eq!(cumulative.input_tokens, 42);
        assert_eq!(cumulative.output_tokens, 7);
        assert_eq!(log.ask_count(), 1);
        assert_eq!(log.elapsed_total_ms(), 123);
    }

    #[test]
    fn log_cumulative_sums_across_multiple_asks() {
        use mu_core::agent::{StopReason, Usage};

        let log = Arc::new(SessionEventLog::new("multi-ask"));
        // Three asks. Cumulative input = 100+50+25 = 175.
        for (input, output, elapsed) in [(100u64, 30u64, 500u64), (50, 12, 400), (25, 8, 200)] {
            let ev = AgentEvent::Done {
                stop_reason: StopReason::EndTurn,
                turn_count: 1,
                usage: Some(Usage {
                    input_tokens: input,
                    output_tokens: output,
                    cache_read_input_tokens: None,
                    cache_creation_input_tokens: None,
                    cache_creation_5m_input_tokens: None,
                    cache_creation_1h_input_tokens: None,
                    reasoning_tokens: None,
                }),
                elapsed_ms: Some(elapsed),
                command_receipts: Vec::new(),
            };
            let (actor, payload) = to_log_event(&ev).unwrap();
            log.append(actor, payload);
        }

        let cumulative = log.cumulative_usage().unwrap();
        assert_eq!(cumulative.input_tokens, 175);
        assert_eq!(cumulative.output_tokens, 50);
        assert_eq!(log.ask_count(), 3);
        assert_eq!(log.elapsed_total_ms(), 1100);
    }

    // ─── mu-5g7i / spec mu-040: TaskTelemetry envelope emission ──────────

    /// Done with EndTurn → TaskExitReason::Done; usage/wall propagate.
    #[test]
    fn mu_5g7i_telemetry_done_endturn_carries_envelope() {
        use mu_core::agent::{StopReason, Usage};
        use mu_core::event_log::TaskExitReason;

        let event = AgentEvent::Done {
            stop_reason: StopReason::EndTurn,
            turn_count: 1,
            usage: Some(Usage {
                input_tokens: 2400,
                output_tokens: 17,
                cache_read_input_tokens: Some(100),
                cache_creation_input_tokens: Some(50),
                cache_creation_5m_input_tokens: None,
                cache_creation_1h_input_tokens: None,
                reasoning_tokens: None,
            }),
            elapsed_ms: Some(1234),
            command_receipts: Vec::new(),
        };
        let payload = task_telemetry_for(
            "session-abc",
            &event,
            Some((
                "openrouter".to_owned(),
                "deepseek/deepseek-v4-flash".to_owned(),
            )),
        )
        .expect("Done should yield TaskTelemetry");

        match payload {
            EventPayload::TaskTelemetry {
                session_id,
                exit_reason,
                wall_clock_ms,
                prompt_tokens,
                completion_tokens,
                cache_read_tokens,
                cache_write_tokens,
                cache_write_5m_tokens,
                cache_write_1h_tokens,
                provider_kind,
                model,
                task_id,
                ended_at_unix_ms,
                started_at_unix_ms,
                tools_granted,
                tools_actually_called,
                max_budget_usd,
                actual_spend_usd,
                local_hour,
                day_of_week,
                tz,
                parent_task_id,
                model_version,
            } => {
                assert_eq!(session_id, "session-abc");
                assert_eq!(exit_reason, TaskExitReason::Done);
                assert_eq!(wall_clock_ms, Some(1234));
                assert_eq!(prompt_tokens, Some(2400));
                assert_eq!(completion_tokens, Some(17));
                assert_eq!(cache_read_tokens, Some(100));
                assert_eq!(cache_write_tokens, Some(50));
                // No tier breakdown in this fixture (no cache_creation object).
                assert_eq!(cache_write_5m_tokens, None);
                assert_eq!(cache_write_1h_tokens, None);
                assert_eq!(provider_kind, "openrouter");
                assert_eq!(model, "deepseek/deepseek-v4-flash");
                assert!(task_id.starts_with("task-"), "task_id: {task_id}");
                assert!(ended_at_unix_ms > 0, "ended_at_unix_ms should be set");
                // MVP-Nones — explicit so a future bead that populates these
                // makes us update the assertions intentionally.
                assert_eq!(started_at_unix_ms, None);
                assert!(tools_granted.is_empty());
                assert!(tools_actually_called.is_empty());
                assert_eq!(max_budget_usd, None);
                assert_eq!(actual_spend_usd, None);
                assert_eq!(local_hour, None);
                assert_eq!(day_of_week, None);
                assert_eq!(tz, None);
                assert_eq!(parent_task_id, None);
                assert_eq!(model_version, None);
            }
            other => panic!("expected TaskTelemetry, got {other:?}"),
        }
    }

    /// Done with Aborted stop_reason → TaskExitReason::Cancelled (the
    /// cancel_session / operator-stop code path).
    #[test]
    fn mu_5g7i_telemetry_done_aborted_maps_to_cancelled() {
        use mu_core::agent::StopReason;
        use mu_core::event_log::TaskExitReason;

        let event = AgentEvent::Done {
            stop_reason: StopReason::Aborted,
            turn_count: 0,
            usage: None,
            elapsed_ms: None,
            command_receipts: Vec::new(),
        };
        let payload = task_telemetry_for(
            "session-xyz",
            &event,
            Some(("anthropic_api".to_owned(), "claude-haiku-4-5".to_owned())),
        )
        .expect("Aborted Done should yield TaskTelemetry");

        match payload {
            EventPayload::TaskTelemetry {
                exit_reason,
                wall_clock_ms,
                prompt_tokens,
                completion_tokens,
                ..
            } => {
                assert_eq!(exit_reason, TaskExitReason::Cancelled);
                assert_eq!(wall_clock_ms, None);
                assert_eq!(prompt_tokens, None);
                assert_eq!(completion_tokens, None);
            }
            other => panic!("expected TaskTelemetry, got {other:?}"),
        }
    }

    /// Error AgentEvent → TaskExitReason::Error; provider info still
    /// carried so error postmortems can attribute by provider/model.
    #[test]
    fn mu_5g7i_telemetry_error_carries_provider_info() {
        use mu_core::event_log::TaskExitReason;

        let event = AgentEvent::Error {
            message: "provider stream closed unexpectedly".into(),
        };
        let payload = task_telemetry_for(
            "session-err",
            &event,
            Some(("openai_api".to_owned(), "gpt-5.5-codex".to_owned())),
        )
        .expect("Error should yield TaskTelemetry");

        match payload {
            EventPayload::TaskTelemetry {
                session_id,
                exit_reason,
                provider_kind,
                model,
                wall_clock_ms,
                ..
            } => {
                assert_eq!(session_id, "session-err");
                assert_eq!(exit_reason, TaskExitReason::Error);
                assert_eq!(provider_kind, "openai_api");
                assert_eq!(model, "gpt-5.5-codex");
                // Errors don't carry a Done-style elapsed_ms — leave None
                // rather than fabricate a duration.
                assert_eq!(wall_clock_ms, None);
            }
            other => panic!("expected TaskTelemetry, got {other:?}"),
        }
    }

    /// Done with per-tier cache write tokens → TaskTelemetry carries
    /// cache_write_5m_tokens / cache_write_1h_tokens. Exercises the
    /// Some-path through `task_telemetry_for` that the None-only fixtures
    /// above leave untested. mu-cache-write-tier-split-umq6.
    #[test]
    fn mu_umq6_telemetry_done_carries_per_tier_cache_write_tokens() {
        use mu_core::agent::{StopReason, Usage};
        use mu_core::event_log::TaskExitReason;

        let event = AgentEvent::Done {
            stop_reason: StopReason::EndTurn,
            turn_count: 1,
            usage: Some(Usage {
                input_tokens: 1_000,
                output_tokens: 50,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: Some(300),
                cache_creation_5m_input_tokens: Some(100),
                cache_creation_1h_input_tokens: Some(200),
                reasoning_tokens: None,
            }),
            elapsed_ms: Some(500),
            command_receipts: Vec::new(),
        };
        let payload = task_telemetry_for(
            "session-tier",
            &event,
            Some(("anthropic_api".to_owned(), "claude-sonnet-4-6".to_owned())),
        )
        .expect("Done should yield TaskTelemetry");

        match payload {
            EventPayload::TaskTelemetry {
                exit_reason,
                cache_write_5m_tokens,
                cache_write_1h_tokens,
                cache_write_tokens,
                ..
            } => {
                assert_eq!(exit_reason, TaskExitReason::Done);
                assert_eq!(
                    cache_write_5m_tokens,
                    Some(100),
                    "cache_write_5m_tokens should be Some(100)"
                );
                assert_eq!(
                    cache_write_1h_tokens,
                    Some(200),
                    "cache_write_1h_tokens should be Some(200)"
                );
                // Flat total still threaded through (legacy field).
                assert_eq!(cache_write_tokens, Some(300));
            }
            other => panic!("expected TaskTelemetry, got {other:?}"),
        }
    }

    /// Non-terminal events return None — no spurious telemetry emission.
    #[test]
    fn mu_5g7i_telemetry_skips_non_terminal_events() {
        let event = AgentEvent::TextDelta { delta: "hi".into() };
        assert!(
            task_telemetry_for("session-x", &event, Some(("p".into(), "m".into()))).is_none(),
            "TextDelta is not terminal — should not produce TaskTelemetry"
        );
    }

    /// When provider_info is None (unusual — would mean no SessionCreated
    /// yet), still emit; provider/model fall back to empty strings rather
    /// than skipping the telemetry event (emission is not optional).
    #[test]
    fn mu_5g7i_telemetry_emits_with_empty_provider_info() {
        use mu_core::agent::StopReason;

        let event = AgentEvent::Done {
            stop_reason: StopReason::EndTurn,
            turn_count: 1,
            usage: None,
            elapsed_ms: None,
            command_receipts: Vec::new(),
        };
        let payload = task_telemetry_for("session-no-info", &event, None).expect("must still emit");
        match payload {
            EventPayload::TaskTelemetry {
                provider_kind,
                model,
                ..
            } => {
                assert_eq!(provider_kind, "");
                assert_eq!(model, "");
            }
            other => panic!("expected TaskTelemetry, got {other:?}"),
        }
    }

    // ─── mu-za92 / mu-heqf: compaction + context-size durability ─────────

    /// CompactionAssembly reaches the durable log with the full
    /// decision audit — the mu-za92 fix. Pre-fix, to_log_event
    /// mapped it to None and compaction was invisible in the JSONL
    /// (the mu-compaction-not-firing-self-hosted-ooil trap: absence
    /// of compaction events proved nothing).
    #[test]
    fn mu_za92_compaction_assembly_lands_durably_with_decisions() {
        use mu_core::context::CompactionDecision;

        let ev = AgentEvent::CompactionAssembly {
            model_call_id: 7,
            policy_id: "heuristic-span-family-drop".into(),
            tokens_before: 160_000,
            tokens_after: 78_000,
            decisions: vec![
                CompactionDecision::Kept {
                    span_id: "sys-1".into(),
                },
                CompactionDecision::Dropped {
                    span_id: "file-load-3".into(),
                    reason: "stale file-load".into(),
                },
            ],
            wall_clock_us: 1234,
            // mu-a79g: trigger inputs must round-trip onto the durable
            // event so the effective compaction point (threshold -
            // reserve) is reconstructable from the JSONL.
            predicted_tokens: 158_000,
            compaction_threshold: 150_000,
            output_reserve: 16_000,
        };
        let (actor, payload) = to_log_event(&ev).expect("CompactionAssembly → durable log");
        assert_eq!(actor, EventActor::System);
        match payload {
            EventPayload::CompactionAssembly {
                model_call_id,
                policy_id,
                tokens_before,
                tokens_after,
                decisions,
                wall_clock_us,
                predicted_tokens,
                compaction_threshold,
                output_reserve,
            } => {
                assert_eq!(model_call_id, 7);
                assert_eq!(policy_id, "heuristic-span-family-drop");
                assert_eq!(tokens_before, 160_000);
                assert_eq!(tokens_after, 78_000);
                assert_eq!(wall_clock_us, 1234);
                assert_eq!(predicted_tokens, 158_000);
                assert_eq!(compaction_threshold, 150_000);
                assert_eq!(output_reserve, 16_000);
                assert_eq!(decisions.len(), 2, "full audit, not a count");
                match &decisions[1] {
                    CompactionDecision::Dropped { span_id, reason } => {
                        assert_eq!(span_id, "file-load-3");
                        assert_eq!(reason, "stale file-load");
                    }
                    other => panic!("expected Dropped, got {other:?}"),
                }
            }
            other => panic!("expected CompactionAssembly, got {other:?}"),
        }
        // Durable JSONL encoding: tagged kind + tagged decision
        // actions, so log scanners can grep for what was ejected.
        let json = serde_json::to_value(to_log_event(&ev).unwrap().1).unwrap();
        assert_eq!(json["kind"], "compaction_assembly");
        assert_eq!(json["decisions"][1]["action"], "dropped");
        assert_eq!(json["decisions"][1]["reason"], "stale file-load");
    }

    /// ContextAssembly's durable payload now carries the renderer's
    /// token estimate (total + per-SpanKind breakdown) — mu-heqf.
    /// token_count_estimate existed since v1 but was always None.
    #[test]
    fn mu_heqf_context_assembly_carries_token_sizes() {
        use std::collections::BTreeMap;

        let mut by_kind = BTreeMap::new();
        by_kind.insert("system".to_owned(), 1_200u64);
        by_kind.insert("tool_result".to_owned(), 14_000u64);
        by_kind.insert("user".to_owned(), 800u64);
        let ev = AgentEvent::ContextAssembly {
            model_call_id: 3,
            message_count: 5,
            user_message_count: 2,
            assistant_message_count: 2,
            tool_result_count: 1,
            tool_count: 10,
            context_sizes: Some(mu_core::context::ContextSizes {
                total: 16_000,
                by_kind: by_kind.clone(),
            }),
            renderer: Some("faux".into()),
            cache_strategy: Some("faux".into()),
            span_count: Some(17),
            cache_boundary_count: Some(0),
            first_span_ids: vec!["sys-1".into()],
            prefix_hash: None,
            prefix_span_hashes: Vec::new(),
        };
        let (_actor, payload) = to_log_event(&ev).expect("ContextAssembly → durable log");
        match payload {
            EventPayload::ContextAssembly {
                token_count_estimate,
                token_breakdown,
                ..
            } => {
                assert_eq!(token_count_estimate, Some(16_000));
                assert_eq!(token_breakdown, by_kind);
                assert_eq!(
                    token_count_estimate.unwrap(),
                    token_breakdown.values().sum::<u64>(),
                    "sections sum to the total"
                );
            }
            other => panic!("expected ContextAssembly, got {other:?}"),
        }
    }
}
