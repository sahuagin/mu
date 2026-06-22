use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
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
}

impl std::ops::Add for Usage {
    type Output = Usage;

    /// Sum two usage snapshots component-wise. Option fields are
    /// summed when both Some; if either is None, the result keeps
    /// the Some value (so partial reporting doesn't lose data).
    fn add(self, other: Usage) -> Usage {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
