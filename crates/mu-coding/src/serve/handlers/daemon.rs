//! Daemon-domain request handlers (daemon.*).

use serde_json::Value;

use mu_core::protocol::{
    DaemonOutstandingCallsResponse, DaemonStatsRequest, DaemonStatsResponse,
    DaemonUsageHistoryRequest, DaemonUsageHistoryResponse, Request, Response, SessionStatusSummary,
};
use mu_core::transport::{codes, err_response, ok_response};
use mu_core::usage_history::{aggregate_into_rows, extract_per_session_metrics};

use crate::serve::daemon_info::DaemonInfo;
use crate::serve::discovery;
use crate::serve::sessions::Sessions;

use super::to_value_or_null;

pub fn handle_outstanding_calls(request: Request<Value>, sessions: Sessions) -> Response<Value> {
    // mu-035 Phase D: `daemon.outstanding_calls` — fleet view of every
    // in-flight provider call across all sessions on this daemon. Used
    // by the TUI command-centre view. Each session's tracker is updated
    // write-through by the forwarder; this handler just snapshots the
    // registry and computes per-call `elapsed_ms` against a single
    // `now_unix_ms` so all rows in one response are consistent.
    let now = discovery::now_unix_ms();
    let calls = sessions.snapshot_outstanding_calls(now);
    let resp = DaemonOutstandingCallsResponse {
        calls,
        snapshot_at_unix_ms: now,
    };
    ok_response(request.id, to_value_or_null(resp))
}

pub use handle_outstanding_calls as handle_daemon_outstanding_calls;

pub fn handle_daemon_stats(
    request: Request<Value>,
    sessions: Sessions,
    daemon_info: DaemonInfo,
) -> Response<Value> {
    let _params: DaemonStatsRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("daemon.stats: invalid params: {e}"),
            );
        }
    };

    let now_ms = discovery::now_unix_ms();
    let snapshot = sessions.snapshot_for_listing();
    let session_count = snapshot.len() as u32;
    let mut active_session_count: u32 = 0;
    let mut total_events: u64 = 0;
    let mut total_tool_calls: u64 = 0;
    let mut total_input_tokens: u64 = 0;
    let mut total_output_tokens: u64 = 0;
    let mut in_flight_calls_count: u32 = 0;

    for (_sid, log, _parent) in snapshot.iter() {
        let events = log.snapshot();
        total_events = total_events.saturating_add(events.len() as u64);
        total_tool_calls = total_tool_calls.saturating_add(log.tool_call_count() as u64);
        if let Some(u) = log.cumulative_usage() {
            total_input_tokens = total_input_tokens.saturating_add(u.input_tokens);
            total_output_tokens = total_output_tokens.saturating_add(u.output_tokens);
        }
        let status = discovery::derive_status_from_events(&events, now_ms);
        if matches!(
            status,
            SessionStatusSummary::Asking
                | SessionStatusSummary::Streaming
                | SessionStatusSummary::ToolExecuting
                | SessionStatusSummary::AwaitingInputRequired
        ) {
            active_session_count = active_session_count.saturating_add(1);
            if matches!(
                status,
                SessionStatusSummary::Asking
                    | SessionStatusSummary::Streaming
                    | SessionStatusSummary::ToolExecuting
            ) {
                in_flight_calls_count = in_flight_calls_count.saturating_add(1);
            }
        }
    }

    let _ = discovery::derive_status; // keep import live; status is computed via derive_status_from_events above
    let resp = DaemonStatsResponse {
        daemon_id: daemon_info.daemon_id().to_string(),
        version: daemon_info.version().to_string(),
        started_at_unix_ms: daemon_info.started_at_unix_ms(),
        uptime_ms: daemon_info.uptime_ms(),
        session_count,
        active_session_count,
        total_events,
        total_tool_calls,
        total_input_tokens,
        total_output_tokens,
        in_flight_calls_count,
    };
    ok_response(request.id, to_value_or_null(resp))
}

/// mu-pex Phase 1 — historical roll-up of timing and token usage
/// across in-memory sessions (live + retained-recently-closed),
/// grouped by (provider, model, time-bucket).
pub fn handle_daemon_usage_history(request: Request<Value>, sessions: Sessions) -> Response<Value> {
    let params: DaemonUsageHistoryRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("daemon.usage_history: invalid params: {e}"),
            );
        }
    };

    let snapshot = sessions.snapshot_for_listing();
    let mut per_session = Vec::with_capacity(snapshot.len());
    let mut considered: u32 = 0;
    for (_sid, log, _parent) in snapshot.iter() {
        let events = log.snapshot();
        let Some(metrics) = extract_per_session_metrics(&events) else {
            continue;
        };
        considered = considered.saturating_add(1);
        if let Some(since) = params.since_unix_ms {
            if metrics.started_at_unix_ms < since {
                continue;
            }
        }
        if let Some(until) = params.until_unix_ms {
            if metrics.started_at_unix_ms >= until {
                continue;
            }
        }
        per_session.push(metrics);
    }

    let rows = aggregate_into_rows(per_session, params.time_bucket_ms);
    let resp = DaemonUsageHistoryResponse {
        rows,
        session_count_total: considered,
        snapshot_at_unix_ms: discovery::now_unix_ms(),
    };
    ok_response(request.id, to_value_or_null(resp))
}

/// mu-k56u: return the route catalog — available provider×model combinations.
pub fn handle_list_routes(request: Request<Value>, daemon_info: DaemonInfo) -> Response<Value> {
    let catalog = daemon_info.route_catalog();
    let resp = mu_core::protocol::DaemonListRoutesResponse {
        routes: catalog.entries().to_vec(),
    };
    ok_response(request.id, to_value_or_null(resp))
}
