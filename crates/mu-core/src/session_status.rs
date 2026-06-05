//! `SessionStatus` — the standard message for session observability.
//!
//! Extensible struct (stable core + Optional tail) exposed as an MCP
//! resource. Computed from existing data sources: `SessionEventLog`,
//! `ProviderStatusTracker` snapshot, and `pricing`. No new storage —
//! this is a projection.

use serde::{Deserialize, Serialize};

use crate::agent::types::Usage;
use crate::pricing;
use crate::protocol::ProviderStatusKind;

/// Stable core + extensible tail. New metrics land as `Option<T>` fields
/// at the end — old consumers ignore them via `serde(default)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStatus {
    // ── identity (stable within a session's lifetime) ──
    pub session_id: String,
    pub daemon_id: String,
    pub provider_kind: String,
    pub model: String,

    // ── live state (changes on every provider-status tick) ──
    pub phase: String,
    pub phase_elapsed_ms: u64,

    // ── cumulative metrics (monotonically increasing) ──
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub ask_count: u32,
    pub tool_call_count: u32,
    pub elapsed_total_ms: u64,

    // ── extensible tail (Option = absent until computed) ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_pressure_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_call_context_tokens: Option<u64>,
}

/// Inputs for computing a `SessionStatus`. Avoids coupling to the
/// concrete types in mu-coding (SessionEventLog, ProviderStatusTracker)
/// so this module stays in mu-core.
#[derive(Debug)]
pub struct StatusInputs<'a> {
    pub session_id: &'a str,
    pub daemon_id: &'a str,
    pub provider_kind: &'a str,
    pub model: &'a str,
    pub cumulative_usage: Option<&'a Usage>,
    pub ask_count: u32,
    pub tool_call_count: u32,
    pub elapsed_total_ms: u64,
    pub provider_status: Option<ProviderSnapshot>,
}

/// Snapshot of the current provider call state (if any). Mirrors
/// `ProviderCallState` from mu-coding but without the dependency.
#[derive(Debug)]
pub struct ProviderSnapshot {
    pub kind: ProviderStatusKind,
    pub started_at_unix_ms: u64,
    pub now_unix_ms: u64,
}

impl SessionStatus {
    pub fn compute(inputs: StatusInputs<'_>) -> Self {
        let (phase, phase_elapsed_ms) = match inputs.provider_status {
            Some(ref snap) => {
                let elapsed = snap.now_unix_ms.saturating_sub(snap.started_at_unix_ms);
                let phase = match snap.kind {
                    ProviderStatusKind::Idle => "idle",
                    ProviderStatusKind::AwaitingFirstToken => "awaiting_first_token",
                    ProviderStatusKind::Streaming => "streaming",
                    ProviderStatusKind::Thinking => "thinking",
                    ProviderStatusKind::ToolExecuting => "tool_executing",
                    ProviderStatusKind::AwaitingToolResult => "awaiting_tool_result",
                };
                (phase.to_string(), elapsed)
            }
            None => ("idle".to_string(), 0),
        };

        let (input_tokens, output_tokens, cache_read, cache_creation) =
            match inputs.cumulative_usage {
                Some(u) => (
                    u.input_tokens,
                    u.output_tokens,
                    u.cache_read_input_tokens,
                    u.cache_creation_input_tokens,
                ),
                None => (0, 0, None, None),
            };

        let cost_usd = pricing::for_model(inputs.provider_kind, inputs.model)
            .map(|p| {
                p.cost(&Usage {
                    input_tokens,
                    output_tokens,
                    cache_read_input_tokens: cache_read,
                    cache_creation_input_tokens: cache_creation,
                    cache_creation_5m_input_tokens: None,
                    cache_creation_1h_input_tokens: None,
                    reasoning_tokens: None,
                })
            })
            .unwrap_or(0.0);

        SessionStatus {
            session_id: inputs.session_id.to_string(),
            daemon_id: inputs.daemon_id.to_string(),
            provider_kind: inputs.provider_kind.to_string(),
            model: inputs.model.to_string(),
            phase,
            phase_elapsed_ms,
            input_tokens,
            output_tokens,
            cost_usd,
            ask_count: inputs.ask_count,
            tool_call_count: inputs.tool_call_count,
            elapsed_total_ms: inputs.elapsed_total_ms,
            cache_read_tokens: cache_read,
            cache_creation_tokens: cache_creation,
            context_pressure_pct: None,
            context_window_size: None,
            last_call_context_tokens: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_idle_no_usage() {
        let status = SessionStatus::compute(StatusInputs {
            session_id: "s1",
            daemon_id: "d1",
            provider_kind: "anthropic_api",
            model: "claude-opus-4-7",
            cumulative_usage: None,
            ask_count: 0,
            tool_call_count: 0,
            elapsed_total_ms: 0,
            provider_status: None,
        });
        assert_eq!(status.phase, "idle");
        assert_eq!(status.phase_elapsed_ms, 0);
        assert_eq!(status.input_tokens, 0);
        assert_eq!(status.cost_usd, 0.0);
        assert!(status.cache_read_tokens.is_none());
    }

    #[test]
    fn compute_with_usage_and_cost() {
        let usage = Usage {
            input_tokens: 10_000,
            output_tokens: 2_000,
            cache_read_input_tokens: Some(5_000),
            cache_creation_input_tokens: Some(1_000),
            reasoning_tokens: None,
        };
        let status = SessionStatus::compute(StatusInputs {
            session_id: "s1",
            daemon_id: "d1",
            provider_kind: "anthropic_api",
            model: "claude-opus-4-7",
            cumulative_usage: Some(&usage),
            ask_count: 3,
            tool_call_count: 7,
            elapsed_total_ms: 45_000,
            provider_status: None,
        });
        assert_eq!(status.input_tokens, 10_000);
        assert_eq!(status.output_tokens, 2_000);
        assert_eq!(status.ask_count, 3);
        assert_eq!(status.tool_call_count, 7);
        assert!(status.cost_usd > 0.0);
        assert_eq!(status.cache_read_tokens, Some(5_000));
        assert_eq!(status.cache_creation_tokens, Some(1_000));
    }

    #[test]
    fn compute_streaming_phase() {
        let status = SessionStatus::compute(StatusInputs {
            session_id: "s1",
            daemon_id: "d1",
            provider_kind: "anthropic_api",
            model: "claude-opus-4-7",
            cumulative_usage: None,
            ask_count: 1,
            tool_call_count: 0,
            elapsed_total_ms: 3_000,
            provider_status: Some(ProviderSnapshot {
                kind: ProviderStatusKind::Streaming,
                started_at_unix_ms: 1000,
                now_unix_ms: 4200,
            }),
        });
        assert_eq!(status.phase, "streaming");
        assert_eq!(status.phase_elapsed_ms, 3200);
    }

    #[test]
    fn compute_unknown_provider_zero_cost() {
        let usage = Usage {
            input_tokens: 10_000,
            output_tokens: 2_000,
            ..Default::default()
        };
        let status = SessionStatus::compute(StatusInputs {
            session_id: "s1",
            daemon_id: "d1",
            provider_kind: "openai_codex",
            model: "gpt-5.5",
            cumulative_usage: Some(&usage),
            ask_count: 1,
            tool_call_count: 0,
            elapsed_total_ms: 1_000,
            provider_status: None,
        });
        assert_eq!(status.cost_usd, 0.0);
        assert_eq!(status.input_tokens, 10_000);
    }

    #[test]
    fn serialization_skips_none_tail_fields() {
        let status = SessionStatus::compute(StatusInputs {
            session_id: "s1",
            daemon_id: "d1",
            provider_kind: "faux",
            model: "faux",
            cumulative_usage: None,
            ask_count: 0,
            tool_call_count: 0,
            elapsed_total_ms: 0,
            provider_status: None,
        });
        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains("context_pressure_pct"));
        assert!(!json.contains("context_window_size"));
        assert!(!json.contains("last_call_context_tokens"));
        assert!(!json.contains("cache_read_tokens"));
    }
}
