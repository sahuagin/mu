use crate::JsonValue;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateResponseRequest {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub input: Vec<InputItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
}

impl CreateResponseRequest {
    pub fn text(model: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            instructions: None,
            input: vec![InputItem::user_text(text)],
            stream: None,
            store: Some(false),
            reasoning: None,
            tools: Vec::new(),
            tool_choice: None,
            parallel_tool_calls: None,
            max_output_tokens: None,
        }
    }
    pub fn streaming(mut self) -> Self {
        self.stream = Some(true);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reasoning {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputItem {
    Message {
        role: String,
        content: Vec<InputContent>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
}

impl InputItem {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self::Message {
            role: "user".into(),
            content: vec![InputContent::InputText { text: text.into() }],
        }
    }
    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self::Message {
            role: "assistant".into(),
            content: vec![InputContent::OutputText { text: text.into() }],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputContent {
    InputText {
        text: String,
    },
    OutputText {
        text: String,
    },
    #[serde(untagged)]
    Unknown(JsonValue),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Tool {
    Function(FunctionTool),
    #[serde(untagged)]
    Unknown(JsonValue),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionTool {
    pub name: String,
    pub description: String,
    pub parameters: JsonValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    None,
    Required,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn minimal_text_request_shape() {
        let req = CreateResponseRequest::text("gpt-4.1-mini", "hi");
        assert_eq!(
            serde_json::to_value(req).unwrap(),
            json!({"model":"gpt-4.1-mini","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],"store":false})
        );
    }
    #[test]
    fn function_tool_shape_is_flat_responses_api() {
        let req = CreateResponseRequest {
            model: "m".into(),
            instructions: None,
            input: vec![InputItem::user_text("hi")],
            stream: Some(true),
            store: Some(false),
            reasoning: Some(Reasoning {
                effort: Some("low".into()),
                summary: Some("auto".into()),
            }),
            tools: vec![Tool::Function(FunctionTool {
                name: "read".into(),
                description: "Read".into(),
                parameters: JsonValue::new(json!({"type":"object"})).unwrap(),
            })],
            tool_choice: Some(ToolChoice::Auto),
            parallel_tool_calls: Some(false),
            max_output_tokens: None,
        };
        let v = serde_json::to_value(req).unwrap();
        assert_eq!(
            v["tools"][0],
            json!({"type":"function","name":"read","description":"Read","parameters":{"type":"object"}})
        );
        assert_eq!(v["tool_choice"], "auto");
    }
}
