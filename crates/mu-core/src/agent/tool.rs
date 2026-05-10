use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;

/// Public description of a tool, sent to the provider so the model
/// knows what tools exist.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema describing the arguments. The provider feeds this
    /// to the model.
    pub input_schema: Value,
}

/// Tool execution result. Errors are EXPRESSED via `is_error: true`
/// rather than propagated — the LLM expects to see the error text and
/// react to it, not get a "the tool failed" rejection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

#[async_trait]
pub trait Tool: Send + Sync {
    /// What the model sees about this tool.
    fn spec(&self) -> ToolSpec;

    /// Execute the tool. The Tool impl owns `cancel_rx` and must
    /// abort when it fires.
    async fn execute(
        &self,
        arguments: Value,
        cancel_rx: oneshot::Receiver<()>,
    ) -> ToolResult;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_spec_round_trips() -> Result<(), serde_json::Error> {
        let spec = ToolSpec {
            name: "echo".to_owned(),
            description: "Echo input".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" }
                },
                "required": ["text"]
            }),
        };

        let value = serde_json::to_value(&spec)?;
        let decoded: ToolSpec = serde_json::from_value(value)?;

        assert_eq!(decoded, spec);
        Ok(())
    }

    #[test]
    fn tool_result_round_trips() -> Result<(), serde_json::Error> {
        let samples = [
            ToolResult {
                content: "done".to_owned(),
                is_error: false,
            },
            ToolResult {
                content: "boom".to_owned(),
                is_error: true,
            },
        ];

        for result in samples {
            let value = serde_json::to_value(&result)?;
            let decoded: ToolResult = serde_json::from_value(value)?;
            assert_eq!(decoded, result);
        }
        Ok(())
    }

    #[test]
    fn tool_trait_is_send_and_sync() {
        fn assert_send<T: Send + Sync + ?Sized>() {}
        assert_send::<dyn Tool>();
    }
}
