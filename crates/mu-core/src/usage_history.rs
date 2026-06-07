//! mu-pex Phase 1 — pure aggregation logic for `daemon.usage_history`.
//!
//! Two pure functions form the projection:
//!
//! * [`extract_per_session_metrics`] reduces one session's event log to
//!   a fixed-size summary plus distribution samples (per-ask wall,
//!   per-tool-call duration, per-model-call latency proxy).
//! * [`aggregate_into_rows`] groups summaries by (provider, model,
//!   time-bucket) and computes [`PercentileStats`] over each pooled
//!   sample distribution.
//!
//! Phase 1 deliberately does not populate `ttft_ms` or `streaming_ms`:
//! the underlying state-transition signal
//! (`AgentEvent::ProviderStatus`) is currently a wire-only notification
//! (mu-035), not a durable `EventPayload`. Phase 1.5 (tracked in bead
//! mu-pex) adds `EventPayload::ProviderStatusUpdate`; the aggregator
//! is structured so populating those fields is purely additive.
//!
//! The functions intentionally take `&[SessionEvent]` rather than the
//! `SessionEventLog` Mutex-protected handle: callers (the dispatch
//! layer) take a snapshot once and pass slices, keeping the
//! aggregation independent of the in-memory log type.

use crate::event_log::{EventPayload, SessionEvent};
use crate::protocol::{PercentileStats, ProviderStatusKind, UsageHistoryRow};

/// One session's contribution to the usage-history projection.
/// Distribution fields carry **all** sample values (per-ask,
/// per-model-call, per-tool-call). Sum fields are session-cumulative.
#[derive(Debug, Clone, PartialEq)]
pub struct PerSessionMetrics {
    pub provider_kind: String,
    pub model: String,
    /// Unix ms of the SessionCreated event.
    pub started_at_unix_ms: u64,
    /// One value per Done event (per ask round-trip). Empty for
    /// sessions that never completed an ask.
    pub wall_ms_samples: Vec<u64>,
    /// One value per (ContextAssembly, next AssistantMessageEvent)
    /// pair. Proxy for "model turn-around latency."
    pub model_call_latency_ms_samples: Vec<u64>,
    /// Time-to-first-token: one value per
    /// `AwaitingFirstToken→<any-other-state>` transition observed
    /// via durable ProviderStatusUpdate events (mu-pex Phase 1.5).
    pub ttft_ms_samples: Vec<u64>,
    /// Streaming-state duration: one value per
    /// `Streaming→<any-other-state>` transition. Multiple per call
    /// when Streaming is interrupted (e.g. by ToolExecuting and
    /// resumed).
    pub streaming_ms_samples: Vec<u64>,
    /// One value per (ToolCall, ToolResult) pair, matched by call_id.
    pub tool_ms_samples: Vec<u64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub reasoning_tokens: u64,
    pub tool_call_count: u64,
    pub error_count: u32,
}

/// Reduce a session's event log to a [`PerSessionMetrics`]. Returns
/// `None` when there is no `SessionCreated` event (cannot identify
/// the (provider, model) the session ran against).
pub fn extract_per_session_metrics(events: &[SessionEvent]) -> Option<PerSessionMetrics> {
    let (provider_kind, model, started_at_unix_ms) =
        events.iter().find_map(|ev| match &ev.payload {
            EventPayload::SessionCreated {
                provider_kind,
                model,
                ..
            } => Some((provider_kind.clone(), model.clone(), ev.timestamp_unix_ms)),
            _ => None,
        })?;

    let mut m = PerSessionMetrics {
        provider_kind,
        model,
        started_at_unix_ms,
        wall_ms_samples: Vec::new(),
        model_call_latency_ms_samples: Vec::new(),
        ttft_ms_samples: Vec::new(),
        streaming_ms_samples: Vec::new(),
        tool_ms_samples: Vec::new(),
        input_tokens: 0,
        output_tokens: 0,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
        reasoning_tokens: 0,
        tool_call_count: 0,
        error_count: 0,
    };

    // Pair ToolCall with the ToolResult sharing its call_id.
    let mut open_tool_calls: std::collections::HashMap<&str, u64> =
        std::collections::HashMap::new();
    // Most recent unmatched ContextAssembly timestamp (pairs with the
    // next AssistantMessageEvent).
    let mut pending_context_assembly_ts: Option<u64> = None;
    // mu-pex Phase 1.5: track the open ProviderStatus state period —
    // the (state, started_at_unix_ms) of the most recent transition
    // we've observed. The next emission that introduces a NEW
    // started_at_unix_ms closes out the prior period, contributing
    // a duration sample if the prior state is one we measure
    // (AwaitingFirstToken → TTFT, Streaming → streaming_ms).
    //
    // The "fresh started_at_unix_ms" signal is more robust than
    // `elapsed_ms == 0`: the agent loop emits state transitions
    // with elapsed_ms set to the time-in-call (loop_.rs:880), not
    // 0. Periodic ticks within a period reuse the period's
    // started_at_unix_ms (so a tick is detectable as "same started
    // as last seen") and are correctly skipped.
    //
    // A trailing open period at the end of an ask is closed out by
    // the subsequent Done event (using the Done's timestamp as the
    // period's end). Without that, the Streaming state of one ask
    // would leak into the inter-ask gap before the next AFT entry.
    let mut last_provider_status_transition: Option<(ProviderStatusKind, u64)> = None;

    for ev in events {
        match &ev.payload {
            EventPayload::ToolCall { call_id, .. } => {
                open_tool_calls.insert(call_id.as_str(), ev.timestamp_unix_ms);
                m.tool_call_count = m.tool_call_count.saturating_add(1);
            }
            EventPayload::ToolResult { call_id, .. } => {
                if let Some(start) = open_tool_calls.remove(call_id.as_str()) {
                    m.tool_ms_samples
                        .push(ev.timestamp_unix_ms.saturating_sub(start));
                }
            }
            EventPayload::ContextAssembly { .. } => {
                pending_context_assembly_ts = Some(ev.timestamp_unix_ms);
            }
            EventPayload::AssistantMessageEvent { .. } => {
                if let Some(ca_ts) = pending_context_assembly_ts.take() {
                    m.model_call_latency_ms_samples
                        .push(ev.timestamp_unix_ms.saturating_sub(ca_ts));
                }
            }
            EventPayload::Done {
                usage, elapsed_ms, ..
            } => {
                // Close out any pending provider-status period —
                // the agent loop doesn't emit a Streaming→Idle
                // transition at end-of-ask, so without this, the
                // last ask's Streaming state would bleed into the
                // gap before the next AFT entry. Use the Done's
                // timestamp as the period's end.
                if let Some((prev_state, prev_started_at)) =
                    last_provider_status_transition.take()
                {
                    let duration = ev.timestamp_unix_ms.saturating_sub(prev_started_at);
                    match prev_state {
                        ProviderStatusKind::AwaitingFirstToken => {
                            m.ttft_ms_samples.push(duration);
                        }
                        ProviderStatusKind::Streaming => {
                            m.streaming_ms_samples.push(duration);
                        }
                        _ => {}
                    }
                }
                if let Some(em) = elapsed_ms {
                    m.wall_ms_samples.push(*em);
                }
                if let Some(u) = usage {
                    m.input_tokens = m.input_tokens.saturating_add(u.input_tokens);
                    m.output_tokens = m.output_tokens.saturating_add(u.output_tokens);
                    if let Some(c) = u.cache_read_input_tokens {
                        m.cache_read_input_tokens =
                            m.cache_read_input_tokens.saturating_add(c);
                    }
                    if let Some(c) = u.cache_creation_input_tokens {
                        m.cache_creation_input_tokens =
                            m.cache_creation_input_tokens.saturating_add(c);
                    }
                    if let Some(r) = u.reasoning_tokens {
                        m.reasoning_tokens = m.reasoning_tokens.saturating_add(r);
                    }
                }
            }
            EventPayload::Error { .. } => {
                m.error_count = m.error_count.saturating_add(1);
            }
            EventPayload::ProviderStatusUpdate {
                state,
                started_at_unix_ms,
                ..
            } => {
                match last_provider_status_transition {
                    Some((_, prev_started_at))
                        if *started_at_unix_ms == prev_started_at =>
                    {
                        // Same period — periodic tick. Skip.
                    }
                    Some((prev_state, prev_started_at)) => {
                        // New period entered; close out the prior one.
                        let duration = started_at_unix_ms.saturating_sub(prev_started_at);
                        match prev_state {
                            ProviderStatusKind::AwaitingFirstToken => {
                                m.ttft_ms_samples.push(duration);
                            }
                            ProviderStatusKind::Streaming => {
                                m.streaming_ms_samples.push(duration);
                            }
                            _ => {}
                        }
                        last_provider_status_transition =
                            Some((*state, *started_at_unix_ms));
                    }
                    None => {
                        // First-ever ProviderStatusUpdate for this session.
                        last_provider_status_transition =
                            Some((*state, *started_at_unix_ms));
                    }
                }
            }
            // Variants that don't carry usage-history signal: skip.
            EventPayload::UserMessage { .. }
            | EventPayload::Callout { .. }
            | EventPayload::SessionCreated { .. }
            | EventPayload::SessionClosed
            // mu-036: autonomous-loop bookkeeping is its own
            // observability surface; the per-call cost figures here
            // come from the same Done events they always have.
            | EventPayload::AutonomousIterationStarted { .. }
            | EventPayload::AutonomousIterationCompleted { .. }
            | EventPayload::AutonomousScheduledWakeup { .. }
            | EventPayload::AutonomousTerminated { .. }
            // mu-lho: mailbox events are inter-session coordination,
            // not per-call usage. The mailbox view projects from
            // these directly; they don't feed token/latency metrics.
            | EventPayload::MailboxMessagePosted { .. }
            | EventPayload::MailboxMessageConsumed { .. }
            // mu-5g7i: TaskTelemetry is the forensics-axis projection.
            // Its tokens are sourced from the same Done events we
            // already aggregate above, so re-counting here would double.
            | EventPayload::TaskTelemetry { .. }
            // mu-gdwd: boundary-validation failures are logged for
            // postmortem but don't carry usage-history signal.
            | EventPayload::ErrorInvalidMessage { .. }
            // mu-za92: compaction audit is context-composition
            // signal, not usage/timing signal.
            | EventPayload::CompactionAssembly { .. }
            | EventPayload::ProviderSwitched { .. }
            // mu-slat: worker lifecycle events are supervisor-side
            // bookkeeping, not per-call usage signals.
            | EventPayload::WorkerSpawned { .. }
            | EventPayload::WorkerExited { .. }
            | EventPayload::WorkerFailed { .. }
            | EventPayload::WorkerTimeout { .. }
            // mu-operator-mark-5mwr: operator quality judgment, not a
            // usage/timing signal.
            | EventPayload::OperatorMark { .. }
            // mu-mh4: a compensating tombstone over a poisoned record,
            // or a live-head attach (resume) — neither is a usage/timing
            // signal.
            | EventPayload::Tombstone { .. }
            | EventPayload::HeadAttached { .. } => {}
        }
    }

    Some(m)
}

/// Group session summaries by (provider, model, time-bucket) and
/// produce one [`UsageHistoryRow`] per group. `time_bucket_ms = None`
/// collapses all sessions for a given (provider, model) into a single
/// row.
///
/// Rows are returned sorted by (provider, model, bucket_start) for
/// deterministic output regardless of input order.
pub fn aggregate_into_rows(
    metrics: Vec<PerSessionMetrics>,
    time_bucket_ms: Option<u64>,
) -> Vec<UsageHistoryRow> {
    use std::collections::BTreeMap;

    // Group key: (provider, model, bucket_start_ms). BTreeMap gives
    // sorted iteration order for free, which makes output stable.
    let mut groups: BTreeMap<(String, String, u64), Vec<PerSessionMetrics>> = BTreeMap::new();

    for m in metrics {
        let bucket_start = match time_bucket_ms {
            Some(b) if b > 0 => (m.started_at_unix_ms / b) * b,
            _ => 0,
        };
        let key = (m.provider_kind.clone(), m.model.clone(), bucket_start);
        groups.entry(key).or_default().push(m);
    }

    groups
        .into_iter()
        .map(|((provider_kind, model, bucket_start_unix_ms), members)| {
            let session_count = members.len() as u32;

            // Pool distribution samples across all sessions in the
            // group. Each session contributes its own per-ask /
            // per-call samples; the row's percentiles are over the
            // pooled set.
            let mut wall: Vec<u64> = Vec::new();
            let mut model_call_lat: Vec<u64> = Vec::new();
            let mut ttft: Vec<u64> = Vec::new();
            let mut streaming: Vec<u64> = Vec::new();
            let mut tool: Vec<u64> = Vec::new();
            let mut input_tokens_sum: u64 = 0;
            let mut output_tokens_sum: u64 = 0;
            let mut cache_read_sum: u64 = 0;
            let mut cache_creation_sum: u64 = 0;
            let mut reasoning_sum: u64 = 0;
            let mut tool_call_count_sum: u64 = 0;
            let mut error_count: u32 = 0;
            let mut effective_bucket_start = bucket_start_unix_ms;
            let mut min_started_at = u64::MAX;

            for m in members {
                wall.extend(m.wall_ms_samples);
                model_call_lat.extend(m.model_call_latency_ms_samples);
                ttft.extend(m.ttft_ms_samples);
                streaming.extend(m.streaming_ms_samples);
                tool.extend(m.tool_ms_samples);
                input_tokens_sum = input_tokens_sum.saturating_add(m.input_tokens);
                output_tokens_sum = output_tokens_sum.saturating_add(m.output_tokens);
                cache_read_sum = cache_read_sum.saturating_add(m.cache_read_input_tokens);
                cache_creation_sum =
                    cache_creation_sum.saturating_add(m.cache_creation_input_tokens);
                reasoning_sum = reasoning_sum.saturating_add(m.reasoning_tokens);
                tool_call_count_sum = tool_call_count_sum.saturating_add(m.tool_call_count);
                error_count = error_count.saturating_add(m.error_count);
                min_started_at = min_started_at.min(m.started_at_unix_ms);
            }

            // No bucket → expose the earliest session's start ms
            // instead of an unhelpful zero.
            if time_bucket_ms.is_none() && min_started_at != u64::MAX {
                effective_bucket_start = min_started_at;
            }

            UsageHistoryRow {
                provider_kind,
                model,
                bucket_start_unix_ms: effective_bucket_start,
                session_count,
                ttft_ms: percentile_stats(ttft),
                streaming_ms: percentile_stats(streaming),
                model_call_latency_ms: percentile_stats(model_call_lat),
                tool_total_ms: percentile_stats(tool).unwrap_or(PercentileStats {
                    median: 0,
                    p95: 0,
                    count: 0,
                }),
                wall_ms: percentile_stats(wall).unwrap_or(PercentileStats {
                    median: 0,
                    p95: 0,
                    count: 0,
                }),
                input_tokens_sum,
                output_tokens_sum,
                cache_read_input_tokens_sum: cache_read_sum,
                cache_creation_input_tokens_sum: cache_creation_sum,
                reasoning_tokens_sum: reasoning_sum,
                tool_call_count_sum,
                error_count,
            }
        })
        .collect()
}

/// `None` when `samples` is empty. Median uses the conventional
/// midpoint (or average of two middles for even N); p95 uses
/// nearest-rank (ceil(0.95 * N), clamped to N).
fn percentile_stats(mut samples: Vec<u64>) -> Option<PercentileStats> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    let n = samples.len();
    let median = if n % 2 == 1 {
        samples[n / 2]
    } else {
        // Use u128 to avoid overflow on (large+large)/2.
        ((samples[n / 2 - 1] as u128 + samples[n / 2] as u128) / 2) as u64
    };
    // Nearest-rank: ceil(0.95 * N), 1-indexed. Clamped to N.
    let p95_idx = ((0.95_f64 * n as f64).ceil() as usize)
        .saturating_sub(1)
        .min(n - 1);
    let p95 = samples[p95_idx];
    Some(PercentileStats {
        median,
        p95,
        count: n as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::types::{AssistantMessage, StopReason};
    use crate::agent::Usage;
    use crate::event_log::{EventActor, EventPayload, SessionEvent};

    fn ev(id: u64, ts: u64, payload: EventPayload) -> SessionEvent {
        SessionEvent {
            id,
            session_id: "s1".into(),
            parent_event_ids: Vec::new(),
            timestamp_unix_ms: ts,
            actor: EventActor::System,
            payload,
        }
    }

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            cache_creation_5m_input_tokens: None,
            cache_creation_1h_input_tokens: None,
            reasoning_tokens: None,
        }
    }

    #[test]
    fn extraction_requires_session_created() {
        let events = vec![ev(
            1,
            100,
            EventPayload::Error {
                message: "x".into(),
            },
        )];
        assert!(extract_per_session_metrics(&events).is_none());
    }

    #[test]
    fn extraction_pulls_provider_model_started() {
        let events = vec![ev(
            1,
            12345,
            EventPayload::SessionCreated {
                provider_kind: "anthropic_api".into(),
                model: "claude-haiku-4-5".into(),
                parent_session_id: None,
                branched_at_parent_event_id: None,
                usage_semantics: None,
            },
        )];
        let m = extract_per_session_metrics(&events).expect("session_created drives identity");
        assert_eq!(m.provider_kind, "anthropic_api");
        assert_eq!(m.model, "claude-haiku-4-5");
        assert_eq!(m.started_at_unix_ms, 12345);
        assert_eq!(m.wall_ms_samples, Vec::<u64>::new());
        assert_eq!(m.tool_call_count, 0);
    }

    #[test]
    fn extraction_pairs_tool_call_with_result() {
        let events = vec![
            ev(
                1,
                100,
                EventPayload::SessionCreated {
                    provider_kind: "anthropic_api".into(),
                    model: "m".into(),
                    parent_session_id: None,
                    branched_at_parent_event_id: None,
                    usage_semantics: None,
                },
            ),
            ev(
                2,
                200,
                EventPayload::ToolCall {
                    call_id: "c1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                },
            ),
            ev(
                3,
                350,
                EventPayload::ToolResult {
                    call_id: "c1".into(),
                    content: "ok".into(),
                    is_error: false,
                },
            ),
        ];
        let m = extract_per_session_metrics(&events).unwrap();
        assert_eq!(m.tool_call_count, 1);
        assert_eq!(m.tool_ms_samples, vec![150]);
    }

    #[test]
    fn extraction_pairs_context_assembly_with_next_assistant_msg() {
        let events = vec![
            ev(
                1,
                100,
                EventPayload::SessionCreated {
                    provider_kind: "p".into(),
                    model: "m".into(),
                    parent_session_id: None,
                    branched_at_parent_event_id: None,
                    usage_semantics: None,
                },
            ),
            ev(
                2,
                200,
                EventPayload::ContextAssembly {
                    model_call_id: 1,
                    message_count: 1,
                    user_message_count: 1,
                    assistant_message_count: 0,
                    tool_result_count: 0,
                    tool_count: 0,
                    token_count_estimate: None,
                    token_breakdown: Default::default(),
                    provider_kind: "p".into(),
                    model: "m".into(),
                    renderer: None,
                    cache_strategy: None,
                    span_count: None,
                    cache_boundary_count: None,
                    first_span_ids: Vec::new(),
                    prefix_hash: None,
                    prefix_span_hashes: Vec::new(),
                },
            ),
            ev(
                3,
                275,
                EventPayload::AssistantMessageEvent {
                    message: AssistantMessage {
                        content: Vec::new(),
                        stop_reason: StopReason::EndTurn,
                        usage: None,
                    },
                },
            ),
            // A second model call (CA → AME pair):
            ev(
                4,
                400,
                EventPayload::ContextAssembly {
                    model_call_id: 2,
                    message_count: 2,
                    user_message_count: 1,
                    assistant_message_count: 1,
                    tool_result_count: 0,
                    tool_count: 0,
                    token_count_estimate: None,
                    token_breakdown: Default::default(),
                    provider_kind: "p".into(),
                    model: "m".into(),
                    renderer: None,
                    cache_strategy: None,
                    span_count: None,
                    cache_boundary_count: None,
                    first_span_ids: Vec::new(),
                    prefix_hash: None,
                    prefix_span_hashes: Vec::new(),
                },
            ),
            ev(
                5,
                550,
                EventPayload::AssistantMessageEvent {
                    message: AssistantMessage {
                        content: Vec::new(),
                        stop_reason: StopReason::EndTurn,
                        usage: None,
                    },
                },
            ),
        ];
        let m = extract_per_session_metrics(&events).unwrap();
        assert_eq!(m.model_call_latency_ms_samples, vec![75, 150]);
    }

    #[test]
    fn extraction_sums_done_usage_and_wall() {
        let events = vec![
            ev(
                1,
                100,
                EventPayload::SessionCreated {
                    provider_kind: "p".into(),
                    model: "m".into(),
                    parent_session_id: None,
                    branched_at_parent_event_id: None,
                    usage_semantics: None,
                },
            ),
            ev(
                2,
                200,
                EventPayload::Done {
                    stop_reason: StopReason::EndTurn,
                    turn_count: 1,
                    usage: Some(usage(100, 50)),
                    elapsed_ms: Some(500),
                },
            ),
            ev(
                3,
                800,
                EventPayload::Done {
                    stop_reason: StopReason::EndTurn,
                    turn_count: 1,
                    usage: Some(usage(40, 20)),
                    elapsed_ms: Some(700),
                },
            ),
            ev(
                4,
                900,
                EventPayload::Done {
                    stop_reason: StopReason::EndTurn,
                    turn_count: 1,
                    usage: None,
                    elapsed_ms: None,
                },
            ),
        ];
        let m = extract_per_session_metrics(&events).unwrap();
        assert_eq!(m.wall_ms_samples, vec![500, 700]);
        assert_eq!(m.input_tokens, 140);
        assert_eq!(m.output_tokens, 70);
    }

    // ===== mu-pex Phase 1.5: ProviderStatusUpdate-driven samples =====

    fn ps(
        id: u64,
        ts: u64,
        state: ProviderStatusKind,
        started_at: u64,
        elapsed_ms: u64,
    ) -> SessionEvent {
        ev(
            id,
            ts,
            EventPayload::ProviderStatusUpdate {
                state,
                started_at_unix_ms: started_at,
                elapsed_ms,
                bytes_received: None,
                tool_call_id: None,
            },
        )
    }

    fn session_started_at(ts: u64) -> SessionEvent {
        ev(
            1,
            ts,
            EventPayload::SessionCreated {
                provider_kind: "anthropic_api".into(),
                model: "haiku".into(),
                parent_session_id: None,
                branched_at_parent_event_id: None,
                usage_semantics: None,
            },
        )
    }

    #[test]
    fn extraction_computes_ttft_from_awaiting_first_token_transition() {
        // AwaitingFirstToken @ 1000 → Streaming @ 1250 ⇒ TTFT = 250.
        let events = vec![
            session_started_at(900),
            ps(2, 1000, ProviderStatusKind::AwaitingFirstToken, 1000, 0),
            ps(3, 1250, ProviderStatusKind::Streaming, 1250, 0),
        ];
        let m = extract_per_session_metrics(&events).unwrap();
        assert_eq!(m.ttft_ms_samples, vec![250]);
        assert_eq!(m.streaming_ms_samples, Vec::<u64>::new());
    }

    #[test]
    fn extraction_periodic_ticks_do_not_close_a_state_period() {
        // Three AwaitingFirstToken events: transition + two ticks
        // (elapsed_ms > 0). Then Streaming closes the period.
        // TTFT should be 2000 (3000 − 1000), not 800 or 1700.
        let events = vec![
            session_started_at(900),
            ps(2, 1000, ProviderStatusKind::AwaitingFirstToken, 1000, 0),
            ps(3, 1800, ProviderStatusKind::AwaitingFirstToken, 1000, 800),
            ps(4, 2700, ProviderStatusKind::AwaitingFirstToken, 1000, 1700),
            ps(5, 3000, ProviderStatusKind::Streaming, 3000, 0),
        ];
        let m = extract_per_session_metrics(&events).unwrap();
        assert_eq!(m.ttft_ms_samples, vec![2000]);
    }

    #[test]
    fn extraction_records_streaming_periods_separately() {
        // Streaming → ToolExecuting → AwaitingToolResult → Streaming
        // → Idle ⇒ two streaming periods (interrupted by a tool).
        let events = vec![
            session_started_at(900),
            ps(2, 1000, ProviderStatusKind::AwaitingFirstToken, 1000, 0),
            ps(3, 1100, ProviderStatusKind::Streaming, 1100, 0),
            ps(4, 1400, ProviderStatusKind::ToolExecuting, 1400, 0),
            ps(5, 1500, ProviderStatusKind::AwaitingToolResult, 1500, 0),
            ps(6, 1800, ProviderStatusKind::Streaming, 1800, 0),
            ps(7, 2200, ProviderStatusKind::Idle, 2200, 0),
        ];
        let m = extract_per_session_metrics(&events).unwrap();
        // TTFT: 1100 − 1000 = 100.
        assert_eq!(m.ttft_ms_samples, vec![100]);
        // Streaming periods: (1100→1400) = 300, (1800→2200) = 400.
        assert_eq!(m.streaming_ms_samples, vec![300, 400]);
    }

    #[test]
    fn extraction_streaming_entry_with_nonzero_elapsed_still_counts() {
        // Discovered via hand-test: the agent loop emits the
        // AwaitingFirstToken→Streaming transition with elapsed_ms set
        // to time-in-call (loop_.rs:880), not 0. The extractor must
        // detect new periods by `started_at_unix_ms` uniqueness, not
        // by `elapsed_ms == 0`.
        let events = vec![
            session_started_at(900),
            ps(2, 1000, ProviderStatusKind::AwaitingFirstToken, 1000, 0),
            // Note: elapsed_ms = 250 here, NOT 0. This is what the
            // real agent loop sends.
            ps(3, 1250, ProviderStatusKind::Streaming, 1250, 250),
            ev(
                4,
                1500,
                EventPayload::Done {
                    stop_reason: StopReason::EndTurn,
                    turn_count: 1,
                    usage: None,
                    elapsed_ms: Some(500),
                },
            ),
        ];
        let m = extract_per_session_metrics(&events).unwrap();
        assert_eq!(m.ttft_ms_samples, vec![250]);
        // Done closes out the open Streaming period: 1500 - 1250 = 250.
        assert_eq!(m.streaming_ms_samples, vec![250]);
    }

    #[test]
    fn extraction_done_closes_out_pending_state_between_asks() {
        // Multi-ask: ask #1 leaves Streaming open at Done time; without
        // close-out, the next AFT entry would attribute the inter-ask
        // gap to Streaming. With close-out, each ask gets clean
        // per-ask samples.
        let events = vec![
            session_started_at(900),
            // Ask #1
            ps(2, 1000, ProviderStatusKind::AwaitingFirstToken, 1000, 0),
            ps(3, 1100, ProviderStatusKind::Streaming, 1100, 100),
            ev(
                4,
                1400,
                EventPayload::Done {
                    stop_reason: StopReason::EndTurn,
                    turn_count: 1,
                    usage: None,
                    elapsed_ms: Some(400),
                },
            ),
            // Ask #2 starts after a ~600ms gap.
            ps(5, 2000, ProviderStatusKind::AwaitingFirstToken, 2000, 0),
            ps(6, 2200, ProviderStatusKind::Streaming, 2200, 200),
            ev(
                7,
                2500,
                EventPayload::Done {
                    stop_reason: StopReason::EndTurn,
                    turn_count: 1,
                    usage: None,
                    elapsed_ms: Some(500),
                },
            ),
        ];
        let m = extract_per_session_metrics(&events).unwrap();
        // TTFT per ask: 1100−1000=100, 2200−2000=200.
        assert_eq!(m.ttft_ms_samples, vec![100, 200]);
        // streaming closed by each Done: 1400−1100=300, 2500−2200=300.
        // Crucially: the 600ms gap between asks is NOT in any sample.
        assert_eq!(m.streaming_ms_samples, vec![300, 300]);
    }

    #[test]
    fn extraction_unterminated_state_period_is_dropped() {
        // AwaitingFirstToken with no follow-up transition (e.g. session
        // still in flight or aborted before first token). The opening
        // transition shouldn't contribute a sample.
        let events = vec![
            session_started_at(900),
            ps(2, 1000, ProviderStatusKind::AwaitingFirstToken, 1000, 0),
        ];
        let m = extract_per_session_metrics(&events).unwrap();
        assert!(m.ttft_ms_samples.is_empty());
        assert!(m.streaming_ms_samples.is_empty());
    }

    #[test]
    fn aggregation_populates_ttft_and_streaming_in_row() {
        let mut m = PerSessionMetrics {
            provider_kind: "p".into(),
            model: "m".into(),
            started_at_unix_ms: 1_000,
            wall_ms_samples: vec![],
            model_call_latency_ms_samples: vec![],
            ttft_ms_samples: vec![100, 250, 400],
            streaming_ms_samples: vec![300, 500],
            tool_ms_samples: vec![],
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reasoning_tokens: 0,
            tool_call_count: 0,
            error_count: 0,
        };
        let rows = aggregate_into_rows(vec![m.clone()], None);
        assert_eq!(rows.len(), 1);
        let ttft = rows[0].ttft_ms.as_ref().expect("ttft populated");
        assert_eq!(ttft.median, 250);
        assert_eq!(ttft.count, 3);
        let streaming = rows[0].streaming_ms.as_ref().expect("streaming populated");
        assert_eq!(streaming.median, 400); // (300+500)/2
        assert_eq!(streaming.count, 2);

        // And: when there are no provider-status samples in the
        // group, the row's ttft_ms and streaming_ms stay None — same
        // shape as Phase 1 behavior.
        m.ttft_ms_samples.clear();
        m.streaming_ms_samples.clear();
        let rows = aggregate_into_rows(vec![m], None);
        assert!(rows[0].ttft_ms.is_none());
        assert!(rows[0].streaming_ms.is_none());
    }

    #[test]
    fn percentile_n1() {
        let s = percentile_stats(vec![42]).unwrap();
        assert_eq!(s.median, 42);
        assert_eq!(s.p95, 42);
        assert_eq!(s.count, 1);
    }

    #[test]
    fn percentile_n2_even_uses_midpoint_avg() {
        let s = percentile_stats(vec![10, 20]).unwrap();
        assert_eq!(s.median, 15);
        // p95: ceil(0.95 * 2) - 1 = 2 - 1 = 1 → samples[1] = 20.
        assert_eq!(s.p95, 20);
    }

    #[test]
    fn percentile_n3_odd_middle() {
        let s = percentile_stats(vec![10, 50, 100]).unwrap();
        assert_eq!(s.median, 50);
        // p95: ceil(0.95 * 3) - 1 = 3 - 1 = 2 → samples[2] = 100.
        assert_eq!(s.p95, 100);
    }

    #[test]
    fn percentile_n100_evenly_spread() {
        // [1..=100]; median between 50 and 51 = 50; p95 = ceil(95) - 1
        // = 94 → 95th element (1-indexed) = 95.
        let v: Vec<u64> = (1..=100).collect();
        let s = percentile_stats(v).unwrap();
        assert_eq!(s.median, 50);
        assert_eq!(s.p95, 95);
        assert_eq!(s.count, 100);
    }

    #[test]
    fn percentile_empty_is_none() {
        assert!(percentile_stats(Vec::new()).is_none());
    }

    #[test]
    fn aggregate_groups_by_provider_model_bucket() {
        let m1 = PerSessionMetrics {
            provider_kind: "anthropic_api".into(),
            model: "haiku".into(),
            started_at_unix_ms: 1_000,
            wall_ms_samples: vec![100, 200],
            model_call_latency_ms_samples: vec![50],
            ttft_ms_samples: vec![],
            streaming_ms_samples: vec![],
            tool_ms_samples: vec![],
            input_tokens: 10,
            output_tokens: 5,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reasoning_tokens: 0,
            tool_call_count: 0,
            error_count: 0,
        };
        let m2 = PerSessionMetrics {
            provider_kind: "anthropic_api".into(),
            model: "haiku".into(),
            started_at_unix_ms: 1_500,
            wall_ms_samples: vec![300],
            model_call_latency_ms_samples: vec![60],
            ttft_ms_samples: vec![],
            streaming_ms_samples: vec![],
            tool_ms_samples: vec![25],
            input_tokens: 7,
            output_tokens: 3,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reasoning_tokens: 0,
            tool_call_count: 1,
            error_count: 0,
        };
        let m3 = PerSessionMetrics {
            provider_kind: "openai_codex".into(),
            model: "gpt-x".into(),
            started_at_unix_ms: 1_200,
            wall_ms_samples: vec![400],
            model_call_latency_ms_samples: vec![],
            ttft_ms_samples: vec![],
            streaming_ms_samples: vec![],
            tool_ms_samples: vec![],
            input_tokens: 8,
            output_tokens: 4,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reasoning_tokens: 0,
            tool_call_count: 0,
            error_count: 1,
        };

        // No bucket → one row per (provider, model).
        let rows = aggregate_into_rows(vec![m1.clone(), m2.clone(), m3.clone()], None);
        assert_eq!(rows.len(), 2);

        let anthropic = rows
            .iter()
            .find(|r| r.provider_kind == "anthropic_api")
            .unwrap();
        assert_eq!(anthropic.model, "haiku");
        assert_eq!(anthropic.session_count, 2);
        assert_eq!(anthropic.wall_ms.count, 3); // 100, 200, 300
        assert_eq!(anthropic.wall_ms.median, 200);
        assert_eq!(anthropic.input_tokens_sum, 17);
        assert_eq!(anthropic.tool_call_count_sum, 1);
        // bucket_start_unix_ms reflects min(started_at) when bucket is None.
        assert_eq!(anthropic.bucket_start_unix_ms, 1_000);
        assert!(anthropic.ttft_ms.is_none()); // Phase 1
        assert!(anthropic.streaming_ms.is_none()); // Phase 1
        assert!(anthropic.model_call_latency_ms.is_some());

        let openai = rows
            .iter()
            .find(|r| r.provider_kind == "openai_codex")
            .unwrap();
        assert_eq!(openai.session_count, 1);
        assert_eq!(openai.error_count, 1);
        assert!(openai.model_call_latency_ms.is_none()); // No samples.
    }

    #[test]
    fn aggregate_buckets_by_time_window() {
        // bucket_ms = 1000 → started_at 500 → bucket 0; started_at
        // 1500 → bucket 1000; same (provider, model).
        let same_model = |started: u64| PerSessionMetrics {
            provider_kind: "p".into(),
            model: "m".into(),
            started_at_unix_ms: started,
            wall_ms_samples: vec![100],
            model_call_latency_ms_samples: vec![],
            ttft_ms_samples: vec![],
            streaming_ms_samples: vec![],
            tool_ms_samples: vec![],
            input_tokens: 1,
            output_tokens: 1,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reasoning_tokens: 0,
            tool_call_count: 0,
            error_count: 0,
        };
        let rows = aggregate_into_rows(
            vec![same_model(500), same_model(1500), same_model(1800)],
            Some(1000),
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].bucket_start_unix_ms, 0);
        assert_eq!(rows[0].session_count, 1);
        assert_eq!(rows[1].bucket_start_unix_ms, 1000);
        assert_eq!(rows[1].session_count, 2);
    }
}
