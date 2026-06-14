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

use serde::{Deserialize, Serialize};

use crate::content::ContentBlock;
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

/// Per-TTL-tier cache-write breakdown (spec :55971). Present when the request
/// wrote into named tiers. mu-cache-write-tier-split-umq6.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CacheCreation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral_5m_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral_1h_input_tokens: Option<u64>,
}

/// Token accounting. `input_tokens`/`output_tokens` are the disjoint buckets;
/// cache read/creation are separate (Anthropic-style usage semantics). The
/// flat `cache_creation_input_tokens` is the total write; `cache_creation` is
/// the per-tier split when available.
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
}

/// A non-streaming response body. `kind` is the literal `"message"` tag the
/// API stamps; kept for fidelity / round-trip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
