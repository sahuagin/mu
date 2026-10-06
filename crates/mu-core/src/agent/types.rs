use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::wire_order_json::WireOrderJson;

// ── ToolArgs newtype (mu-gdwd) ──────────────────────────────────────

/// Validated wrapper around `serde_json::Value` for tool-call arguments.
///
/// Rejects `NaN`, `+Inf`, and `-Inf` at construction — these are
/// prohibited by RFC 8259 §6 and would break `Eq` (IEEE 754:
/// `NaN != NaN`). With the invariant enforced, `Eq` is safe to derive
/// on every container that holds `ToolArgs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "serde_json::Value", into = "serde_json::Value")]
pub struct ToolArgs(Value);

impl Eq for ToolArgs {}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ToolArgsError {
    #[error("tool arguments contain non-finite number at path {path}: {value}")]
    NonFinite { path: String, value: f64 },
}

impl ToolArgs {
    /// Construct a `ToolArgs` from a `Value`, rejecting NaN/Inf at any
    /// nesting depth.
    pub fn new(value: Value) -> Result<Self, ToolArgsError> {
        validate_value(&value, "$")?;
        Ok(Self(value))
    }

    pub fn as_value(&self) -> &Value {
        &self.0
    }
}

impl TryFrom<Value> for ToolArgs {
    type Error = ToolArgsError;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ToolArgs> for Value {
    fn from(args: ToolArgs) -> Value {
        args.0
    }
}

fn validate_value(v: &Value, path: &str) -> Result<(), ToolArgsError> {
    match v {
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if f.is_nan() || f.is_infinite() {
                    return Err(ToolArgsError::NonFinite {
                        path: path.to_string(),
                        value: f,
                    });
                }
            }
            Ok(())
        }
        Value::Array(arr) => {
            for (i, item) in arr.iter().enumerate() {
                validate_value(item, &format!("{path}[{i}]"))?;
            }
            Ok(())
        }
        Value::Object(map) => {
            for (key, val) in map {
                validate_value(val, &format!("{path}.{key}"))?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Per-conceptual-type alias for ContentBlock text payloads
/// (mu-yqeq.2). Backing storage is `Arc<str>` so structural assistant
/// content (Text + Thinking blocks) clones cheaply and can byte-share
/// with the flat [`Span.content`](crate::context::Span) via the same
/// underlying buffer once mu-yqeq.A wires
/// [`Span.blocks`](crate::context::Span) in.
pub type BlockText = Arc<str>;

/// One message in an agent's conversation context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum AgentMessage {
    User {
        content: String,
    },
    Assistant(AssistantMessage),
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
}

/// The model's response on one turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    /// Token usage for this turn, if the provider exposed it. None
    /// means the provider didn't report (or didn't yet — usage often
    /// arrives in the same final event as `stop_reason`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// Per-turn (or aggregated across turns) token usage.
///
/// `input_tokens` and `output_tokens` are always populated when
/// usage is reported at all. The remaining fields are provider-
/// specific opt-ins:
/// - `cache_read_input_tokens`: prompt cache hit (Anthropic + OpenAI)
/// - `cache_creation_input_tokens`: prompt cache write total (Anthropic)
/// - `cache_creation_5m_input_tokens`: ephemeral-5m tier write tokens (Anthropic, mu-cache-write-tier-split-umq6)
/// - `cache_creation_1h_input_tokens`: ephemeral-1h tier write tokens (Anthropic, mu-cache-write-tier-split-umq6)
/// - `reasoning_tokens`: hidden reasoning tokens (OpenAI o-series,
///   Codex; Anthropic extended thinking doesn't report this yet)
///
/// The tier fields (`_5m` / `_1h`) are present only when the provider
/// returns a `cache_creation` breakdown object. When absent the cost
/// formula falls back to the flat `cache_creation_input_tokens` total.
/// Invariant: when both tier fields are present,
/// `5m + 1h == cache_creation_input_tokens`.
///
/// `cache_attribution` and `provider_attribution_raw` are per-call only
/// (see [`CacheSpanAttribution`]); they are why `Usage` is `Clone` and not
/// `Copy`. Both are `Arc`s, so a clone is a refcount bump, and summing
/// (`Usage + &Usage`) drops them.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    /// Ephemeral-5m cache write tokens (1.25× write premium). Present only
    /// when the provider returns the per-tier breakdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_5m_input_tokens: Option<u64>,
    /// Ephemeral-1h cache write tokens (2.0× write premium). Present only
    /// when the provider returns the per-tier breakdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_1h_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// Per-span prompt-cache accounting for THIS call, when the provider
    /// reports it (today: the OpenAI codex Responses backend's
    /// `usage.attribution`). None for every other provider and for summed
    /// usage — a span split only means something for one request.
    ///
    /// Immutable after construction, so it is an `Arc<[_]>`: cloning a
    /// `Usage` shares the spans instead of deep-copying them, and the slice
    /// is one allocation (refcounts and elements inline). Serializes exactly
    /// like a `Vec` (serde `rc` feature).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_attribution: Option<Arc<[CacheSpanAttribution]>>,
    /// The provider's own attribution block for THIS call, beside the
    /// normalized fields. The normalized fields (including
    /// `cache_attribution`) are the universal view; this keeps the provider's
    /// own notation for readers who need it. Today: the OpenAI codex
    /// backend's `usage.attribution` object only (the rest of its usage block
    /// is already modelled above). None for every other provider and for
    /// summed usage — per call only, never summed.
    ///
    /// Content-complete, not byte-verbatim: it is mu-openai's re-serialization
    /// of the parsed block. Every entry, every field (including ones
    /// mu-openai does not model) and every value is kept, and the span keys
    /// of `items` / `request_fields` keep WIRE order — the one place order
    /// carries meaning (request order of the input items). Key order inside
    /// an entry and inside its nested `content` is not preserved (those pass
    /// through sorted containers); JSON gives object key order no meaning
    /// there.
    ///
    /// [`WireOrderJson`] rather than `serde_json::Value`: this workspace's
    /// `Value` sorts object keys, which would lose the span order above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_attribution_raw: Option<Arc<WireOrderJson>>,
}

/// One prompt span's share of a call's input and cached tokens.
///
/// Provider-neutral: `key` is whatever the provider names the span (a
/// server-assigned item id, or a request field name like `instructions` /
/// `tools`); `index` is mu's best mapping of an input item back to its
/// position in the request's input list. Spans are listed in the order the
/// provider reported them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheSpanAttribution {
    pub kind: CacheSpanKind,
    /// Position among the request's input items (0-based). Input items only;
    /// None for request fields. Provider-derived and possibly heuristic —
    /// see the producing provider's mapping for how it was assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
    pub key: String,
    pub input_tokens: u64,
    pub cached_tokens: u64,
    /// Output tokens the provider attributes to this span (non-zero only on
    /// an [`CacheSpanKind::OutputItem`]).
    #[serde(default)]
    pub output_tokens: u64,
    /// Cache-write tokens the provider attributes to this span, as reported.
    /// On the codex backend this has read 0 even on a call whose prefix the
    /// next call then hit, so it is recorded, not interpreted.
    #[serde(default)]
    pub cache_write_tokens: u64,
}

/// What a [`CacheSpanAttribution`] covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheSpanKind {
    /// One item of the request's input list (a message, tool call, …).
    InputItem,
    /// A top-level request field outside the input list (`instructions`,
    /// `tools`).
    RequestField,
    /// An entry the provider attributes to this call's own output (it
    /// reports output tokens). Kept with its figures; no input `index`.
    OutputItem,
    /// Catch-all: a kind written by a newer build, read by an older one,
    /// deserializes here instead of failing the whole event.
    #[serde(other)]
    Unknown,
}

impl std::ops::Add for Usage {
    type Output = Usage;

    /// By-value sum; delegates to `Add<&Usage>`.
    fn add(self, other: Usage) -> Usage {
        self + &other
    }
}

impl std::ops::AddAssign<&Usage> for Usage {
    fn add_assign(&mut self, other: &Usage) {
        *self = std::mem::take(self) + other;
    }
}

impl std::ops::Add<&Usage> for Usage {
    type Output = Usage;

    /// Sum two usage snapshots component-wise. Option fields are
    /// summed when both Some; if either is None, the result keeps
    /// the Some value (so partial reporting doesn't lose data).
    /// The per-call fields (`cache_attribution`,
    /// `provider_attribution_raw`) are None on a sum.
    fn add(self, other: &Usage) -> Usage {
        fn add_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
            match (a, b) {
                (Some(x), Some(y)) => Some(x + y),
                (Some(x), None) | (None, Some(x)) => Some(x),
                (None, None) => None,
            }
        }
        Usage {
            input_tokens: self.input_tokens + other.input_tokens,
            output_tokens: self.output_tokens + other.output_tokens,
            cache_read_input_tokens: add_opt(
                self.cache_read_input_tokens,
                other.cache_read_input_tokens,
            ),
            cache_creation_input_tokens: add_opt(
                self.cache_creation_input_tokens,
                other.cache_creation_input_tokens,
            ),
            cache_creation_5m_input_tokens: add_opt(
                self.cache_creation_5m_input_tokens,
                other.cache_creation_5m_input_tokens,
            ),
            cache_creation_1h_input_tokens: add_opt(
                self.cache_creation_1h_input_tokens,
                other.cache_creation_1h_input_tokens,
            ),
            reasoning_tokens: add_opt(self.reasoning_tokens, other.reasoning_tokens),
            // A sum spans several calls; per-call span splits and the raw
            // per-call provider block don't add.
            cache_attribution: None,
            provider_attribution_raw: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: BlockText,
    },
    ToolCall(ToolCall),
    /// Reasoning trace (Anthropic extended thinking, OpenAI reasoning).
    ///
    /// `text` is the human-displayable summary (may be empty). `opaque`
    /// is a PROVIDER-OWNED round-trip token that must be echoed back
    /// verbatim on the next turn to preserve chain-of-thought across
    /// tool calls. mu-core never interprets it — it only stores and
    /// projects it losslessly. The OpenAI provider packs a reasoning
    /// item (id + encrypted_content + summary) into it; other providers
    /// leave it `None` (and the OpenAI outbound path drops `None`
    /// Thinking blocks, matching pre-PR-B behavior).
    Thinking {
        text: BlockText,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        opaque: Option<BlockText>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: ToolArgs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Model stopped naturally; no tool calls in the response.
    EndTurn,
    /// Model emitted tool calls; loop should execute them and continue.
    ToolUse,
    /// Model hit a token limit mid-response.
    MaxTokens,
    /// Provider declined the request or cut generation on a safety/policy
    /// classifier (Anthropic gen-5 `stop_reason: "refusal"`). The turn is
    /// over and mu does not retry; distinct from [`StopReason::EndTurn`] so
    /// receipts and exit-reason analytics can see refusals instead of
    /// counting them as normal completions. The empty-turn auto-continue
    /// guard explicitly excludes this reason: a refusal's normal body shape
    /// IS an empty turn, and re-invoking would hammer the safety classifier
    /// with the same conversation. (mu-provider-drift-2026q3-y43la)
    Refusal,
    /// Server paused a long-running turn (`stop_reason: "pause_turn"`) and
    /// expects the client to resend the conversation to continue. mu does
    /// not implement that continuation: the ask ends, but under its own
    /// label so receipts/analytics don't misreport a paused turn as a
    /// natural completion. (mu-provider-drift-2026q3-y43la)
    PauseTurn,
    /// Provider errored; assistant message may be partial.
    Error,
    /// Cancel was requested (via AgentInput::Cancel or via cancellation
    /// signal from outside).
    Aborted,
    /// SSE stream closed without terminal message_stop event (connection
    /// drop, upstream truncation, or provider protocol violation).
    /// The message may be partial; this signals degraded completion.
    DegradedEof,
    /// Agent loop hit its `max_turns` configured ceiling and stopped
    /// without invoking the model again. The conversation is not
    /// naturally finished — distinguishing this from `EndTurn` lets the
    /// TUI/transcript surface it as "turn budget exhausted, ask a
    /// follow-up or raise --max-iterations" instead of silently
    /// terminating. (mu-779s)
    IterationCap,
    /// The session's spend ceiling was reached (mu-048): the last call
    /// took the metered spend to or past `max_usd`, so the loop stopped
    /// without invoking the model again. Like `IterationCap`, the
    /// conversation is not naturally finished — the caller is told the
    /// figure and can raise or drop the ceiling and ask again.
    BudgetCap,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `cache_attribution` is additive on the event-log wire: absent when
    /// None (old logs and non-attributing providers are byte-identical), and
    /// an old-shape usage reads back as None.
    #[test]
    fn usage_cache_attribution_serde_shape() -> Result<(), serde_json::Error> {
        let old = json!({"input_tokens": 10, "output_tokens": 2, "cache_read_input_tokens": 4});
        let u: Usage = serde_json::from_value(old.clone())?;
        assert_eq!(u.cache_attribution, None);
        assert_eq!(serde_json::to_value(&u)?, old);

        let u = Usage {
            input_tokens: 10,
            output_tokens: 2,
            cache_read_input_tokens: Some(4),
            cache_attribution: Some(Arc::from(vec![
                CacheSpanAttribution {
                    kind: CacheSpanKind::RequestField,
                    index: None,
                    key: "tools".into(),
                    input_tokens: 4,
                    cached_tokens: 4,
                    output_tokens: 0,
                    cache_write_tokens: 0,
                },
                CacheSpanAttribution {
                    kind: CacheSpanKind::InputItem,
                    index: Some(0),
                    key: "msg_1".into(),
                    input_tokens: 6,
                    cached_tokens: 0,
                    output_tokens: 0,
                    cache_write_tokens: 0,
                },
            ])),
            ..Default::default()
        };
        let v = serde_json::to_value(&u)?;
        assert_eq!(
            v,
            json!({
                "input_tokens": 10,
                "output_tokens": 2,
                "cache_read_input_tokens": 4,
                "cache_attribution": [
                    {"kind": "request_field", "key": "tools", "input_tokens": 4, "cached_tokens": 4, "output_tokens": 0, "cache_write_tokens": 0},
                    {"kind": "input_item", "index": 0, "key": "msg_1", "input_tokens": 6, "cached_tokens": 0, "output_tokens": 0, "cache_write_tokens": 0}
                ]
            })
        );
        assert_eq!(serde_json::from_value::<Usage>(v)?, u);
        // A sum spans calls, so it carries no span split.
        assert_eq!((u.clone() + u).cache_attribution, None);
        Ok(())
    }

    /// Every form of `Add` drops both per-call fields and sums the counts
    /// the same way.
    #[test]
    fn usage_sum_drops_per_call_fields() {
        let u = Usage {
            input_tokens: 10,
            output_tokens: 2,
            cache_read_input_tokens: Some(4),
            cache_attribution: Some(Arc::from(vec![CacheSpanAttribution {
                kind: CacheSpanKind::InputItem,
                index: Some(0),
                key: "msg_1".into(),
                input_tokens: 10,
                cached_tokens: 4,
                output_tokens: 0,
                cache_write_tokens: 0,
            }])),
            provider_attribution_raw: Some(Arc::new(
                WireOrderJson::from_json_str(r#"{"items": {"msg_1": {}}}"#).expect("json"),
            )),
            ..Default::default()
        };
        let expect = Usage {
            input_tokens: 20,
            output_tokens: 4,
            cache_read_input_tokens: Some(8),
            ..Default::default()
        };
        assert_eq!(u.clone() + u.clone(), expect);
        assert_eq!(u.clone() + &u, expect);
        let mut acc = u.clone();
        acc += &u;
        assert_eq!(acc, expect);
        assert_eq!(acc.cache_attribution, None);
        assert_eq!(acc.provider_attribution_raw, None);
    }

    /// A span kind this build doesn't know reads as `Unknown`, and the
    /// surrounding `Usage` still parses.
    #[test]
    fn unknown_cache_span_kind_deserializes() -> Result<(), serde_json::Error> {
        let u: Usage = serde_json::from_value(json!({
            "input_tokens": 5,
            "output_tokens": 1,
            "cache_attribution": [
                {"kind": "some_future_kind", "key": "x", "input_tokens": 5, "cached_tokens": 0},
                {"kind": "input_item", "index": 0, "key": "msg_1", "input_tokens": 0, "cached_tokens": 0}
            ]
        }))?;
        assert_eq!(u.input_tokens, 5);
        let spans = u.cache_attribution.as_deref().expect("spans");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].kind, CacheSpanKind::Unknown);
        assert_eq!(spans[0].key, "x");
        assert_eq!(spans[1].kind, CacheSpanKind::InputItem);
        Ok(())
    }

    #[test]
    fn agent_message_round_trips() -> Result<(), serde_json::Error> {
        let samples = [
            AgentMessage::User {
                content: "hello".to_owned(),
            },
            AgentMessage::Assistant(assistant_message()),
            AgentMessage::ToolResult {
                call_id: "call-1".to_owned(),
                content: "result".to_owned(),
                is_error: false,
            },
        ];

        for message in samples {
            let value = serde_json::to_value(&message)?;
            let decoded: AgentMessage = serde_json::from_value(value)?;
            assert_eq!(decoded, message);
        }
        Ok(())
    }

    #[test]
    fn assistant_message_round_trips() -> Result<(), serde_json::Error> {
        let message = assistant_message();

        let value = serde_json::to_value(&message)?;
        let decoded: AssistantMessage = serde_json::from_value(value)?;

        assert_eq!(decoded, message);
        Ok(())
    }

    #[test]
    fn content_block_round_trips() -> Result<(), serde_json::Error> {
        let samples = [
            ContentBlock::Text { text: "hi".into() },
            ContentBlock::ToolCall(tool_call()),
            ContentBlock::Thinking {
                text: "reasoning".into(),
                opaque: None,
            },
            ContentBlock::Thinking {
                text: "with state".into(),
                opaque: Some("opaque-token".into()),
            },
        ];

        for block in samples {
            let value = serde_json::to_value(&block)?;
            let decoded: ContentBlock = serde_json::from_value(value)?;
            assert_eq!(decoded, block);
        }
        Ok(())
    }

    /// `Thinking.opaque` round-trips through serde in both states:
    /// absent (None → field omitted on the wire) and present (Some →
    /// echoed verbatim). The provider-owned token must survive
    /// persistence so chain-of-thought is preserved across tool calls.
    #[test]
    fn thinking_opaque_round_trips_present_and_absent() -> Result<(), serde_json::Error> {
        // Absent: the field is omitted entirely (skip_serializing_if).
        let none = ContentBlock::Thinking {
            text: "summary".into(),
            opaque: None,
        };
        let value = serde_json::to_value(&none)?;
        assert!(
            value.get("opaque").is_none(),
            "opaque=None must be omitted on the wire, got {value}"
        );
        assert_eq!(serde_json::from_value::<ContentBlock>(value)?, none);

        // Present: the token is carried verbatim.
        let some = ContentBlock::Thinking {
            text: String::new().into(),
            opaque: Some("{\"id\":\"rs_1\",\"encrypted_content\":\"enc==\"}".into()),
        };
        let value = serde_json::to_value(&some)?;
        assert_eq!(
            value["opaque"],
            serde_json::json!("{\"id\":\"rs_1\",\"encrypted_content\":\"enc==\"}")
        );
        assert_eq!(serde_json::from_value::<ContentBlock>(value)?, some);
        Ok(())
    }

    #[test]
    fn tool_call_round_trips() -> Result<(), serde_json::Error> {
        let call = tool_call();

        let value = serde_json::to_value(&call)?;
        let decoded: ToolCall = serde_json::from_value(value)?;

        assert_eq!(decoded, call);
        Ok(())
    }

    #[test]
    fn stop_reason_round_trips() -> Result<(), serde_json::Error> {
        let samples = [
            StopReason::EndTurn,
            StopReason::ToolUse,
            StopReason::MaxTokens,
            StopReason::Error,
            StopReason::Aborted,
            StopReason::DegradedEof,
            StopReason::IterationCap,
            StopReason::BudgetCap,
        ];

        for reason in samples {
            let value = serde_json::to_value(reason)?;
            let decoded: StopReason = serde_json::from_value(value)?;
            assert_eq!(decoded, reason);
        }
        Ok(())
    }

    fn assistant_message() -> AssistantMessage {
        AssistantMessage {
            content: vec![
                ContentBlock::Text {
                    text: "hello".into(),
                },
                ContentBlock::ToolCall(tool_call()),
                ContentBlock::Thinking {
                    text: "thinking".into(),
                    opaque: None,
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: None,
        }
    }

    fn tool_call() -> ToolCall {
        ToolCall {
            id: "call-1".to_owned(),
            name: "echo".to_owned(),
            arguments: ToolArgs::new(json!({ "text": "hello" })).unwrap(),
        }
    }

    // ── ToolArgs tests (mu-gdwd) ────────────────────────────────

    #[test]
    fn tool_args_accepts_finite_numbers() {
        let v = json!({"x": 1, "y": 2.5, "z": -0.0, "w": 0});
        assert!(ToolArgs::new(v).is_ok());
    }

    #[test]
    fn tool_args_accepts_strings_bools_nulls() {
        let v = json!({"s": "hello", "b": true, "n": null, "arr": [1, "a", null]});
        assert!(ToolArgs::new(v).is_ok());
    }

    #[test]
    fn tool_args_serde_json_rejects_nan_at_parse_time() {
        // serde_json::Number::from_f64 returns None for NaN/Inf,
        // so NaN can never reach ToolArgs through JSON parsing.
        // This test confirms the serde_json guarantee holds.
        assert!(serde_json::Number::from_f64(f64::NAN).is_none());
        assert!(serde_json::Number::from_f64(f64::INFINITY).is_none());
        assert!(serde_json::Number::from_f64(f64::NEG_INFINITY).is_none());
    }

    #[test]
    fn tool_args_try_from_round_trip() {
        let v = json!({"a": 1.5});
        let args = ToolArgs::new(v.clone()).unwrap();
        let back: Value = args.into();
        assert_eq!(back, v);
    }

    #[test]
    fn tool_args_serde_round_trip() {
        let v = json!({"nested": {"arr": [1, 2, 3], "s": "hi"}});
        let args = ToolArgs::new(v.clone()).unwrap();
        let serialized = serde_json::to_value(&args).unwrap();
        let deserialized: ToolArgs = serde_json::from_value(serialized).unwrap();
        assert_eq!(deserialized.as_value(), &v);
    }

    #[test]
    fn tool_args_eq_is_derived() {
        let a = ToolArgs::new(json!({"x": 1})).unwrap();
        let b = ToolArgs::new(json!({"x": 1})).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn tool_args_deeply_nested_valid() {
        let v = json!({"a": {"b": {"c": {"d": [1, 2, {"e": 1.5}]}}}});
        assert!(ToolArgs::new(v).is_ok());
    }

    /// cc full-fidelity unification (mu-cc-event-unification-lkma.1, WS1): a cc
    /// assistant turn — thinking + text + tool_use, Anthropic-shaped usage with
    /// the 5m/1h cache-write tier split, `ToolUse` stop — maps onto the EXISTING
    /// schema and survives a JSONL round-trip with no information loss. This
    /// pins WS1's finding: no mu-core schema change is needed for cc turn-level
    /// fidelity (the gap was the emitter, not the schema). See
    /// specs/architecture/cc-event-mapping.md.
    #[test]
    fn cc_shaped_assistant_turn_round_trips() -> Result<(), serde_json::Error> {
        let message = AssistantMessage {
            content: vec![
                // cc `thinking{thinking,signature}` -> text-only (signature dropped,
                // consistent with mu's own native handling).
                ContentBlock::Thinking {
                    text: "Let me read the file before claiming it's fixed.".into(),
                    opaque: None,
                },
                ContentBlock::Text {
                    text: "I'll check it now.".into(),
                },
                // cc `tool_use{id,name,input,caller}` -> ToolCall (caller deferred).
                ContentBlock::ToolCall(ToolCall {
                    id: "toolu_01YNTDGSQpMUKPRBJ9HLsgbW".to_owned(),
                    name: "Read".to_owned(),
                    arguments: ToolArgs::new(json!({ "file_path": "/x" })).unwrap(),
                }),
            ],
            // cc `stop_reason:"tool_use"` -> ToolUse (and stop_sequence -> EndTurn,
            // matching anthropic.rs:678).
            stop_reason: StopReason::ToolUse,
            usage: Some(Usage {
                input_tokens: 1234,
                output_tokens: 56,
                cache_read_input_tokens: Some(9000),
                cache_creation_input_tokens: Some(800),
                // cc `usage.cache_creation.{ephemeral_5m,ephemeral_1h}` -> tier split.
                cache_creation_5m_input_tokens: Some(500),
                cache_creation_1h_input_tokens: Some(300),
                reasoning_tokens: None,
                cache_attribution: None,
                provider_attribution_raw: None,
            }),
        };
        // Round-trip through the JSONL string form the event log persists.
        let line = serde_json::to_string(&message)?;
        let decoded: AssistantMessage = serde_json::from_str(&line)?;
        assert_eq!(decoded, message);
        // Tier split survives and stays self-consistent (5m + 1h == total).
        let u = decoded.usage.expect("usage present");
        assert_eq!(
            u.cache_creation_5m_input_tokens.unwrap() + u.cache_creation_1h_input_tokens.unwrap(),
            u.cache_creation_input_tokens.unwrap()
        );
        Ok(())
    }
}
