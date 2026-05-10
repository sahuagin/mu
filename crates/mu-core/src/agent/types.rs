use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One message in an agent's conversation context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum AgentMessage {
    User { content: String },
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    ToolCall(ToolCall),
    /// Reasoning trace (Anthropic extended thinking, OpenAI reasoning).
    Thinking { text: String },
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
