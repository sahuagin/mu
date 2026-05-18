use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One message in an agent's conversation context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
/// - `cache_creation_input_tokens`: prompt cache write (Anthropic)
/// - `reasoning_tokens`: hidden reasoning tokens (OpenAI o-series,
///   Codex; Anthropic extended thinking doesn't report this yet)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
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
            reasoning_tokens: add_opt(self.reasoning_tokens, other.reasoning_tokens),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolCall(ToolCall),
    /// Reasoning trace (Anthropic extended thinking, OpenAI reasoning).
    Thinking {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
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
            ContentBlock::Text {
                text: "hi".to_owned(),
            },
            ContentBlock::ToolCall(tool_call()),
            ContentBlock::Thinking {
                text: "reasoning".to_owned(),
            },
        ];

        for block in samples {
            let value = serde_json::to_value(&block)?;
            let decoded: ContentBlock = serde_json::from_value(value)?;
            assert_eq!(decoded, block);
        }
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
                    text: "hello".to_owned(),
                },
                ContentBlock::ToolCall(tool_call()),
                ContentBlock::Thinking {
                    text: "thinking".to_owned(),
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
            arguments: json!({ "text": "hello" }),
        }
    }
}
