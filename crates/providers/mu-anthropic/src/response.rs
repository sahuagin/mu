//! Response types — the non-streaming `Message` body returned by
//! `POST /v1/messages` (when not streaming) and the assembled result of a
//! stream.
//!
//! Wire shape (`/docs/en/get-started § Call the API`, the response body):
//! `{id, type:"message", role, model, content:[...],
//! stop_reason, stop_sequence?, usage:{...}}`.
//!
//! Scar list encoded here (INTEGRATION.md §6):
//! - usage.cache_creation is a per-TTL-tier breakdown object
//!   (ephemeral_5m_input_tokens / ephemeral_1h_input_tokens;
//!   `/docs/en/build-with-claude/prompt-caching § 1-hour cache duration`),
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
/// body shape). The refusal fields are the ones
/// `/docs/en/build-with-claude/refusals-and-fallback § What a refusal looks
/// like` lists. Unmodeled keys round-trip via `extra` (forward-compat).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StopDetails {
    /// Refusal category on a `stop_reason: "refusal"` response. Documented
    /// values as of 2026-08: `"cyber"`, `"bio"`, and (Fable 5, 2026-06-09)
    /// `"reasoning_extraction"`. Kept as a free string — new categories must
    /// not break deserialization. (mu-provider-drift-2026q3-y43la)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Human-readable refusal description; "the text is not stable, so
    /// display it rather than parse it".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
    /// "Present only on requests that set `fallbacks`": a model to retry
    /// directly when the API skipped the fallback attempt (the fallback
    /// model was rate limited, say). A hint, not a guarantee.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_credit_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_has_prefill_claim: Option<bool>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, JsonValue>,
}

/// Per-TTL-tier cache-write breakdown (`/docs/en/build-with-claude/prompt-caching
/// § 1-hour cache duration`). Present when the request
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
    /// Inference speed mode the response was served at (`standard` or
    /// `fast`; `/docs/en/api/beta/messages/create § Returns`). A String like
    /// `service_tier` beside it — inbound, so lossless over a set the API may
    /// extend; the request side types it as [`Speed`](crate::Speed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
}

/// A code-execution container handle, echoed at the TOP LEVEL of the response
/// when a code-execution server tool ran (verified on real opus-4-8 traffic):
/// `{"id":"container_…","expires_at":"…"}`. Unmodeled keys round-trip via `extra`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Container {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, JsonValue>,
}

/// Why a replayed block was removed (`input_transformations[].reason`). The
/// four documented binding checks, listed in the precedence order the API
/// reference gives for a block that would fail several
/// (`/docs/en/api/beta/messages/create § Returns`, `input_transformations`).
/// A value not listed there is kept verbatim in
/// [`TransformationReason::Other`], so a captured response re-serializes as
/// it arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransformationReason {
    OrganizationBindingMismatch,
    EndUserBindingMismatch,
    /// Created by a model whose reasoning the requested model may not read
    /// (`/docs/en/build-with-claude/preserved-thinking § Switching models
    /// mid-conversation`).
    ModelBindingMismatch,
    /// The conversation before it differs from the one it was created in
    /// (`§ What the API does with an invalid block`); the rest of that
    /// turn's consecutive thinking blocks go with it.
    PrefixBindingMismatch,
    /// A reason this crate does not name, carried as the wire string.
    Other(String),
}

impl TransformationReason {
    /// The wire string.
    pub fn as_str(&self) -> &str {
        match self {
            Self::OrganizationBindingMismatch => "organization_binding_mismatch",
            Self::EndUserBindingMismatch => "end_user_binding_mismatch",
            Self::ModelBindingMismatch => "model_binding_mismatch",
            Self::PrefixBindingMismatch => "prefix_binding_mismatch",
            Self::Other(s) => s,
        }
    }

    fn from_wire(s: &str) -> Self {
        match s {
            "organization_binding_mismatch" => Self::OrganizationBindingMismatch,
            "end_user_binding_mismatch" => Self::EndUserBindingMismatch,
            "model_binding_mismatch" => Self::ModelBindingMismatch,
            "prefix_binding_mismatch" => Self::PrefixBindingMismatch,
            other => Self::Other(other.to_owned()),
        }
    }
}

impl Serialize for TransformationReason {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for TransformationReason {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Self::from_wire(&s))
    }
}

/// One entry of the response's top-level `input_transformations` — a change
/// the API made to the request's input before showing it to the model, in
/// request order (`/docs/en/build-with-claude/preserved-thinking § Set the
/// mismatch behavior and read input_transformations`; beta
/// `thinking-binding-controls-2026-08-01`). The one documented entry type is
/// `thinking_dropped`, whose `path` is the `messages.{i}.content.{j}` form
/// error messages use. The reference says to ignore entry types you do not
/// recognize, so an entry of another `type` lands in
/// [`InputTransformation::Unknown`] and round-trips verbatim — but a
/// `thinking_dropped` entry is parsed STRICTLY, and a malformed one is a hard
/// error rather than a silent `Unknown` (the [`ContentBlock`] discipline: wire
/// breakage on a known type must be loud). Keys this crate does not model on
/// a `thinking_dropped` entry ride in `extra`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputTransformation {
    ThinkingDropped {
        path: String,
        reason: TransformationReason,
        extra: BTreeMap<String, JsonValue>,
    },
    Unknown(JsonValue),
}

/// The typed body of a `thinking_dropped` entry, minus the `type` tag.
#[derive(Serialize, Deserialize)]
struct ThinkingDroppedEntry {
    path: String,
    reason: TransformationReason,
    #[serde(flatten, default)]
    extra: BTreeMap<String, JsonValue>,
}

impl<'de> Deserialize<'de> for InputTransformation {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let raw = serde_json::Value::deserialize(deserializer)?;
        match raw.get("type").and_then(serde_json::Value::as_str) {
            Some("thinking_dropped") => {
                let mut m = raw.as_object().cloned().unwrap_or_default();
                m.remove("type");
                let e: ThinkingDroppedEntry = serde_json::from_value(serde_json::Value::Object(m))
                    .map_err(D::Error::custom)?;
                Ok(InputTransformation::ThinkingDropped {
                    path: e.path,
                    reason: e.reason,
                    extra: e.extra,
                })
            }
            _ => Ok(InputTransformation::Unknown(
                JsonValue::new(raw).map_err(D::Error::custom)?,
            )),
        }
    }
}

impl Serialize for InputTransformation {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            InputTransformation::ThinkingDropped {
                path,
                reason,
                extra,
            } => {
                let mut map = serde_json::Map::new();
                map.insert("type".into(), "thinking_dropped".into());
                map.insert("path".into(), path.clone().into());
                map.insert("reason".into(), reason.as_str().into());
                for (k, v) in extra {
                    map.insert(k.clone(), v.as_value().clone());
                }
                serde_json::Value::Object(map).serialize(serializer)
            }
            InputTransformation::Unknown(v) => v.serialize(serializer),
        }
    }
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
    /// Code-execution container handle (present when a code-exec tool ran).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<Container>,
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
    /// Blocks the API removed from the request before the model saw it
    /// (beta `thinking-binding-controls-2026-08-01`). Present on every such
    /// response, as `[]` when nothing was changed, so `Some(vec![])` and
    /// absent stay distinct and re-serialize as they arrived. When streaming
    /// it is final on `message_start`; only a mid-stream server-side fallback
    /// makes the final `message_delta` carry a replacement (see
    /// [`MessageDeltaBody`](crate::MessageDeltaBody)).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_transformations: Option<Vec<InputTransformation>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_documented_response_body() {
        // /docs/en/get-started § Call the API — the documented response body,
        // verbatim.
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
    fn container_parses_on_code_execution_response() {
        // Real opus-4-8 code-execution response carries a top-level container.
        let raw = json!({
            "id": "msg_x", "type": "message", "role": "assistant", "model": "m",
            "content": [],
            "container": {"id": "container_01Sm", "expires_at": "2026-06-14T08:14:57.946777Z"},
            "stop_reason": "end_turn"
        });
        let m: Message = serde_json::from_value(raw.clone()).unwrap();
        let c = m.container.as_ref().unwrap();
        assert_eq!(c.id, "container_01Sm");
        assert_eq!(c.expires_at.as_deref(), Some("2026-06-14T08:14:57.946777Z"));
        assert_eq!(serde_json::to_value(&m).unwrap(), raw, "round-trips");
        // absent on a normal response
        let plain: Message = serde_json::from_value(json!({
            "id": "x", "type": "message", "role": "assistant", "model": "m", "content": []
        }))
        .unwrap();
        assert!(plain.container.is_none());
    }

    #[test]
    fn input_transformations_match_the_documented_entry() {
        // /docs/en/build-with-claude/preserved-thinking § Switching models
        // mid-conversation — a block dropped by the model check, at the
        // path form error messages use.
        let raw = json!({
            "id": "msg_01", "type": "message", "role": "assistant",
            "model": "claude-opus-5", "content": [],
            "input_transformations": [
                {
                    "type": "thinking_dropped",
                    "path": "messages.3.content.0",
                    "reason": "model_binding_mismatch"
                }
            ]
        });
        let m: Message = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(
            m.input_transformations,
            Some(vec![InputTransformation::ThinkingDropped {
                path: "messages.3.content.0".into(),
                reason: TransformationReason::ModelBindingMismatch,
                extra: BTreeMap::new(),
            }])
        );
        assert_eq!(serde_json::to_value(&m).unwrap(), raw);
    }

    #[test]
    fn malformed_thinking_dropped_errors_not_degrades() {
        // A known entry type with a missing or mistyped field is wire
        // breakage and must be loud, not an `Unknown` a consumer matching
        // on ThinkingDropped would never see.
        for bad in [
            json!({"type": "thinking_dropped", "path": "messages.0.content.0"}),
            json!({"type": "thinking_dropped", "reason": "model_binding_mismatch"}),
            json!({"type": "thinking_dropped", "path": {"i": 0}, "reason": "model_binding_mismatch"}),
        ] {
            assert!(
                serde_json::from_value::<InputTransformation>(bad.clone()).is_err(),
                "{bad}"
            );
        }
        // An unmodeled key on a well-formed entry rides in `extra` and comes
        // back out, so a capture that gains a field still round-trips.
        let raw = json!({
            "type": "thinking_dropped", "path": "messages.0.content.0",
            "reason": "prefix_binding_mismatch", "turn": 3
        });
        let e: InputTransformation = serde_json::from_value(raw.clone()).unwrap();
        match &e {
            InputTransformation::ThinkingDropped { extra, .. } => {
                assert_eq!(
                    extra.get("turn").map(|v| v.as_value().clone()),
                    Some(json!(3))
                );
            }
            other => panic!("expected ThinkingDropped, got {other:?}"),
        }
        assert_eq!(serde_json::to_value(&e).unwrap(), raw);
    }

    #[test]
    fn input_transformations_keep_empty_absent_and_unknown_apart() {
        // The reference: present as `[]` on every response under the beta
        // when nothing was changed; absent without the beta. The two must
        // not collapse, or a re-serialized capture drifts.
        let base =
            json!({"id": "m", "type": "message", "role": "assistant", "model": "x", "content": []});
        let absent: Message = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(absent.input_transformations, None);
        assert!(serde_json::to_value(&absent)
            .unwrap()
            .get("input_transformations")
            .is_none());
        let mut with_empty = base.clone();
        with_empty["input_transformations"] = json!([]);
        let empty: Message = serde_json::from_value(with_empty.clone()).unwrap();
        assert_eq!(empty.input_transformations, Some(vec![]));
        assert_eq!(serde_json::to_value(&empty).unwrap(), with_empty);
        // An entry type we do not recognize is kept verbatim (the reference
        // asks for it to be ignored, not lost), and a reason we do not
        // recognize is carried as its wire string; the whole response
        // re-serializes as it arrived.
        let mut odd = base;
        odd["input_transformations"] = json!([
            {"type": "something_new", "detail": 1},
            {"type": "thinking_dropped", "path": "messages.0.content.0", "reason": "later_check"}
        ]);
        let m: Message = serde_json::from_value(odd.clone()).unwrap();
        let entries = m.input_transformations.as_deref().unwrap();
        assert!(matches!(entries[0], InputTransformation::Unknown(_)));
        match &entries[1] {
            InputTransformation::ThinkingDropped { reason, .. } => {
                assert_eq!(*reason, TransformationReason::Other("later_check".into()));
                assert_eq!(reason.as_str(), "later_check");
            }
            other => panic!("expected ThinkingDropped, got {other:?}"),
        }
        assert_eq!(serde_json::to_value(&m).unwrap(), odd);
    }

    #[test]
    fn usage_speed_parses_from_the_reference_example() {
        // /docs/en/api/beta/messages/create § Returns — the example's usage
        // carries service_tier and speed side by side.
        let u: Usage = serde_json::from_value(json!({
            "input_tokens": 2095, "output_tokens": 503,
            "service_tier": "standard", "speed": "standard"
        }))
        .unwrap();
        assert_eq!(u.speed.as_deref(), Some("standard"));
        assert_eq!(u.service_tier.as_deref(), Some("standard"));
        let back = serde_json::to_value(&u).unwrap();
        assert_eq!(back["speed"], json!("standard"));
        let u: Usage =
            serde_json::from_value(json!({"input_tokens": 1, "output_tokens": 1})).unwrap();
        assert_eq!(u.speed, None);
        assert!(serde_json::to_value(&u).unwrap().get("speed").is_none());
    }

    #[test]
    fn refusal_stop_details_fields_parse_typed() {
        // /docs/en/build-with-claude/refusals-and-fallback § What a refusal
        // looks like — category, explanation, recommended_model (the last
        // only on requests that set fallbacks); nulls are normal values.
        let d: StopDetails = serde_json::from_value(json!({
            "category": "cyber",
            "explanation": "The request asked for working exploit code.",
            "recommended_model": "claude-opus-4-8",
            "fallback_credit_token": null,
            "fallback_has_prefill_claim": null
        }))
        .unwrap();
        assert_eq!(d.category.as_deref(), Some("cyber"));
        assert_eq!(
            d.explanation.as_deref(),
            Some("The request asked for working exploit code.")
        );
        assert_eq!(d.recommended_model.as_deref(), Some("claude-opus-4-8"));
        assert!(
            d.extra.is_empty(),
            "the typed fields no longer land in extra: {:?}",
            d.extra
        );
        let d: StopDetails =
            serde_json::from_value(json!({"category": null, "explanation": null})).unwrap();
        assert_eq!(d, StopDetails::default());
    }

    #[test]
    fn cache_creation_tier_split_parses() {
        // SCAR mu-cache-write-tier-split-umq6 —
        // /docs/en/build-with-claude/prompt-caching § 1-hour cache duration.
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
    fn stop_details_category_parses_typed() {
        // Gen-5 refusal category (docs: "cyber", "bio", "reasoning_extraction")
        // is a typed field; an undocumented future category is still a plain
        // string, not a break. (mu-provider-drift-2026q3-y43la)
        let raw = json!({
            "id": "msg_x", "type": "message", "role": "assistant", "model": "m",
            "content": [], "stop_reason": "refusal",
            "stop_details": { "category": "reasoning_extraction" }
        });
        let m: Message = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(
            m.stop_details.as_ref().unwrap().category.as_deref(),
            Some("reasoning_extraction")
        );
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
