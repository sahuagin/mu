//! Response types — the non-streaming `Message` body returned by
//! `POST /v1/messages` (when not streaming) and the assembled result of a
//! stream.
//!
//! Wire shape (spec :88): `{id, type:"message", role, model, content:[...],
//! stop_reason, stop_sequence?, usage:{...}}`.
//!
//! Scar list encoded here (INTEGRATION.md §6):
//! - usage.cache_creation is a per-TTL-tier breakdown object
//!   (ephemeral_5m_input_tokens / ephemeral_1h_input_tokens, spec :55971),
//!   distinct from the flat cache_creation_input_tokens total
//!   (mu-cache-write-tier-split-umq6).
//! - the mu-yz48 'usage at top level of message_delta' scar belongs to the
//!   STREAMING slice (slice 5); here usage is nested in the response body as
//!   documented. A test pins that the non-streaming location is the body.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::content::ContentBlock;
use crate::json::JsonValue;
use crate::message::Role;

/// Why the model stopped. Unknown values degrade to [`StopReason::Other`]
/// rather than erroring (forward-compat: e.g. `pause_turn` and future
/// reasons).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
    Refusal,
    PauseTurn,
    #[serde(other)]
    Other,
}

/// Stop-reason detail (`stop_details`). Present on essentially every wire
/// message (usually with null fields); populated on a `refusal` with
/// fallback-credit info (spec: fallback-credit beta — `fallback_credit_token`
/// is an opaque one-time credit; `fallback_has_prefill_claim` picks the retry
/// body shape). Unmodeled keys round-trip via `extra` (forward-compat).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StopDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_credit_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_has_prefill_claim: Option<bool>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, JsonValue>,
}

/// Per-TTL-tier cache-write breakdown (spec :55971). Present when the request
/// wrote into named tiers. mu-cache-write-tier-split-umq6.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CacheCreation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral_5m_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral_1h_input_tokens: Option<u64>,
}

/// Output-token breakdown (`usage.output_tokens_details`). `thinking_tokens` is
/// the reasoning portion of `output_tokens` (≤ output_tokens; observability
/// only — `output_tokens` remains the billed total). spec: extended-thinking
/// usage (`{"output_tokens_details":{"thinking_tokens":N}}`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OutputTokensDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_tokens: Option<u64>,
}

/// Server-side tool usage counters (`usage.server_tool_use`), e.g. web-search
/// request counts. The known counter is typed; any other counter round-trips
/// via `extra` (forward-compat).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ServerToolUseUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search_requests: Option<u64>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, JsonValue>,
}

/// One entry of `usage.iterations` — a per-iteration token breakdown the wire
/// emits on the final `message_delta` (observed on real opus-4-8 traffic). Each
/// mirrors the top-level usage buckets plus a `type` tag; unmodeled keys
/// round-trip via `extra`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct IterationUsage {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<CacheCreation>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, JsonValue>,
}

/// Token accounting. `input_tokens`/`output_tokens` are the disjoint buckets;
/// cache read/creation are separate (Anthropic-style usage semantics). The
/// flat `cache_creation_input_tokens` is the total write; `cache_creation` is
/// the per-tier split when available.
///
/// The trailing fields are observed on real opus-4-8 wire traffic (ahead of the
/// pinned spec snapshot): `service_tier`/`inference_geo` (echoed routing) on
/// `message_start`, and `output_tokens_details`/`iterations` on the final
/// `message_delta`. `server_tool_use` carries server-tool request counts.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<CacheCreation>,

    /// Service tier echoed on the response (e.g. `standard`). String —
    /// lossless + forward-compat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    /// Inference geography echoed on the response (e.g. `not_available`,
    /// `global`, `us`). String — `not_available` rules out a global/us enum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference_geo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens_details: Option<OutputTokensDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<ServerToolUseUsage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub iterations: Vec<IterationUsage>,
}

/// A non-streaming response body. `kind` is the literal `"message"` tag the
/// API stamps; kept for fidelity / round-trip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub role: Role,
    pub model: String,
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
    /// Refusal/fallback-credit detail (see [`StopDetails`]). On the wire this
    /// rides nearly every message (often all-null); omitted when absent here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_details: Option<StopDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_documented_response_body() {
        // spec :88 verbatim.
        let raw = json!({
            "id": "msg_013mHbppMPd2PrVJzGMZPt2D",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-4-8",
            "content": [{"type": "text", "text": "Here are some effective search strategies"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 21, "output_tokens": 305}
        });
        let m: Message = serde_json::from_value(raw.clone()).expect("parse");
        assert_eq!(m.id, "msg_013mHbppMPd2PrVJzGMZPt2D");
        assert_eq!(m.role, Role::Assistant);
        assert_eq!(m.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(m.usage.as_ref().unwrap().input_tokens, Some(21));
        assert_eq!(m.usage.as_ref().unwrap().output_tokens, Some(305));
        assert_eq!(m.content.len(), 1);
        // round-trips back to the same JSON.
        assert_eq!(serde_json::to_value(&m).unwrap(), raw);
    }

    #[test]
    fn cache_creation_tier_split_parses() {
        // SCAR mu-cache-write-tier-split-umq6 — spec :55971.
        let raw = json!({
            "input_tokens": 412,
            "output_tokens": 264,
            "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 248,
            "cache_creation": {
                "ephemeral_5m_input_tokens": 148,
                "ephemeral_1h_input_tokens": 100
            }
        });
        let u: Usage = serde_json::from_value(raw).unwrap();
        let cc = u.cache_creation.unwrap();
        assert_eq!(cc.ephemeral_5m_input_tokens, Some(148));
        assert_eq!(cc.ephemeral_1h_input_tokens, Some(100));
        // the flat total is preserved alongside the split.
        assert_eq!(u.cache_creation_input_tokens, Some(248));
    }

    #[test]
    fn usage_models_service_tier_and_inference_geo() {
        // Exact message_start.usage from real opus-4-8 wire (2026-06-13) —
        // previously these were ignored as unmodeled extras; now typed.
        let raw = json!({
            "input_tokens": 4076,
            "cache_creation_input_tokens": 47548,
            "cache_read_input_tokens": 0,
            "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 47548},
            "output_tokens": 4,
            "service_tier": "standard",
            "inference_geo": "not_available"
        });
        let u: Usage = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(u.service_tier.as_deref(), Some("standard"));
        assert_eq!(u.inference_geo.as_deref(), Some("not_available"));
        assert_eq!(serde_json::to_value(&u).unwrap(), raw, "round-trips");
    }

    #[test]
    fn usage_models_output_tokens_details_and_iterations() {
        // Exact message_delta.usage from real wire.
        let raw = json!({
            "input_tokens": 4076,
            "cache_creation_input_tokens": 47548,
            "cache_read_input_tokens": 0,
            "output_tokens": 4,
            "output_tokens_details": {"thinking_tokens": 0},
            "iterations": [{
                "input_tokens": 4076, "output_tokens": 4, "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 47548,
                "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 47548},
                "type": "message"
            }]
        });
        let u: Usage = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(
            u.output_tokens_details.as_ref().unwrap().thinking_tokens,
            Some(0)
        );
        assert_eq!(u.iterations.len(), 1);
        assert_eq!(u.iterations[0].kind.as_deref(), Some("message"));
        assert_eq!(u.iterations[0].input_tokens, Some(4076));
        assert_eq!(serde_json::to_value(&u).unwrap(), raw, "round-trips");
    }

    #[test]
    fn usage_server_tool_use_counter_and_unknown_round_trip() {
        // Known counter typed; an unmodeled counter survives via extra.
        let raw = json!({
            "input_tokens": 10, "output_tokens": 5,
            "server_tool_use": {"web_search_requests": 3, "web_fetch_requests": 1}
        });
        let u: Usage = serde_json::from_value(raw.clone()).unwrap();
        let stu = u.server_tool_use.as_ref().unwrap();
        assert_eq!(stu.web_search_requests, Some(3));
        assert_eq!(stu.extra["web_fetch_requests"].as_value(), &json!(1));
        assert_eq!(
            serde_json::to_value(&u).unwrap(),
            raw,
            "round-trips verbatim"
        );
    }

    #[test]
    fn usage_lives_in_response_body_not_elsewhere() {
        // Pins the NON-streaming location: usage is a sibling of content in
        // the body. (The streaming mu-yz48 scar — usage at top level of
        // message_delta — is slice 5's concern.)
        let m: Message = serde_json::from_value(json!({
            "id": "x", "type": "message", "role": "assistant", "model": "m",
            "content": [], "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 2}
        }))
        .unwrap();
        assert!(m.usage.is_some(), "usage must parse from the body position");
    }

    #[test]
    fn stop_details_refusal_credit_parses_and_round_trips() {
        // spec: fallback-credit beta — a refusal carries the credit token in
        // stop_details. Models the two documented fields + unknown-key passthrough.
        let raw = json!({
            "id": "msg_x", "type": "message", "role": "assistant", "model": "m",
            "content": [], "stop_reason": "refusal",
            "stop_details": {
                "fallback_credit_token": "fct_abc",
                "fallback_has_prefill_claim": false,
                "some_future_key": 1
            }
        });
        let m: Message = serde_json::from_value(raw.clone()).unwrap();
        let sd = m.stop_details.as_ref().unwrap();
        assert_eq!(sd.fallback_credit_token.as_deref(), Some("fct_abc"));
        assert_eq!(sd.fallback_has_prefill_claim, Some(false));
        assert_eq!(sd.extra["some_future_key"].as_value(), &json!(1));
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            raw,
            "round-trips verbatim"
        );
    }

    #[test]
    fn stop_details_absent_is_omitted() {
        let m: Message = serde_json::from_value(json!({
            "id": "x", "type": "message", "role": "assistant", "model": "m",
            "content": [], "stop_reason": "end_turn"
        }))
        .unwrap();
        assert!(m.stop_details.is_none());
        assert!(serde_json::to_value(&m)
            .unwrap()
            .get("stop_details")
            .is_none());
    }

    #[test]
    fn unknown_stop_reason_degrades_to_other() {
        let m: Message = serde_json::from_value(json!({
            "id": "x", "type": "message", "role": "assistant", "model": "m",
            "content": [], "stop_reason": "some_future_reason"
        }))
        .unwrap();
        assert_eq!(m.stop_reason, Some(StopReason::Other));
    }

    #[test]
    fn all_documented_stop_reasons_parse() {
        for (s, want) in [
            ("end_turn", StopReason::EndTurn),
            ("max_tokens", StopReason::MaxTokens),
            ("stop_sequence", StopReason::StopSequence),
            ("tool_use", StopReason::ToolUse),
            ("refusal", StopReason::Refusal),
            ("pause_turn", StopReason::PauseTurn),
        ] {
            let got: StopReason = serde_json::from_value(json!(s)).unwrap();
            assert_eq!(got, want, "stop_reason {s}");
        }
    }
}
