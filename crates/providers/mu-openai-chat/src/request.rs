//! Request side of the Chat Completions wire: the JSON body POSTed to
//! `/v1/chat/completions` (or a proxy path like OpenRouter's
//! `/api/v1/chat/completions` — the path is transport, not wire shape).
//!
//! Serialization contract: optional fields serialize *absent*, never `null`,
//! so a request built with all options unset is byte-minimal — matching the
//! hand-rolled `json!` bodies this crate was promoted from (their stability
//! guarantees, e.g. mu-13ve/mu-y8gp "no key when unset", carry over).
//! Key *order* within objects is not part of the contract (servers parse
//! JSON, not bytes); equality tests compare `serde_json::Value`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One entry in the request `messages` array.
///
/// Chat Completions expresses every conversation element as a role-tagged
/// message: the system prompt is a leading `role:"system"` message (there is
/// no top-level `system` field on this wire), tool results are `role:"tool"`
/// messages keyed back to their call by `tool_call_id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ChatMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        /// Concatenated assistant text. Absent when the turn was
        /// tool-calls-only. (The wire requires at least one of
        /// `content`/`tool_calls`; constructing neither is a caller bug —
        /// the hand-rolled path elided such messages entirely.)
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<ToolCallRef>>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

/// A completed tool call echoed back in an assistant message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRef {
    pub id: String,
    /// Always `"function"` on this wire.
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionRef,
}

impl ToolCallRef {
    pub fn function(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments_json: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: "function".to_string(),
            function: FunctionRef {
                name: name.into(),
                arguments: arguments_json.into(),
            },
        }
    }
}

/// The function name + arguments of an echoed tool call. `arguments` is a
/// *string-encoded* JSON object — that's the wire's shape, not an escaping
/// accident.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionRef {
    pub name: String,
    pub arguments: String,
}

/// A tool made available to the model (`tools` array entry).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    /// Always `"function"` on this wire.
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionDef,
}

impl Tool {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Self {
        Self {
            kind: "function".to_string(),
            function: FunctionDef {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

/// Function declaration: name, description, and a JSON-Schema `parameters`
/// object (kept as raw [`Value`] — the schema is caller-defined passthrough).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// `stream_options` — request a final usage chunk on streaming responses.
/// Without `include_usage: true` most OpenAI-compatible backends omit usage
/// from streams entirely.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

/// OpenRouter's normalized `reasoning` request field: `effort` ∈
/// {`low`,`medium`,`high`}, mapped by the server to whatever the backing
/// model supports. Plain OpenAI-compatible local servers ignore it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reasoning {
    pub effort: String,
}

/// The POST body for a (streaming) chat completion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    /// Output-token cap. The promoted implementation always sends it
    /// (resolved per-model); optional here so non-mu consumers can omit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
}

impl ChatCompletionRequest {
    /// A streaming request with usage reporting on — the shape every
    /// mu dispatch sends. Optional knobs start unset (absent on the wire).
    pub fn streaming(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        Self {
            model: model.into(),
            max_tokens: None,
            stream: true,
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
            messages,
            tools: None,
            reasoning: None,
            temperature: None,
            top_p: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn system_user_roundtrip_shape() {
        let msgs = vec![
            ChatMessage::System {
                content: "be brief".into(),
            },
            ChatMessage::User {
                content: "hi".into(),
            },
        ];
        let v = serde_json::to_value(&msgs).unwrap();
        assert_eq!(
            v,
            json!([
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "hi"}
            ])
        );
    }

    #[test]
    fn assistant_tool_calls_matches_handrolled_shape() {
        // Byte-matched (as Value) to the shape openrouter.rs::translate_message
        // emits for an assistant turn with one tool call and no text.
        let m = ChatMessage::Assistant {
            content: None,
            tool_calls: Some(vec![ToolCallRef::function(
                "call_1",
                "read",
                r#"{"path":"/tmp/x"}"#,
            )]),
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(
            v,
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "read", "arguments": "{\"path\":\"/tmp/x\"}"}
                }]
            })
        );
    }

    #[test]
    fn tool_result_shape() {
        let m = ChatMessage::Tool {
            tool_call_id: "call_1".into(),
            content: "[error] no such file".into(),
        };
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "[error] no such file"})
        );
    }

    #[test]
    fn request_minimal_has_no_null_keys() {
        let req = ChatCompletionRequest::streaming(
            "ornith-q4-r0",
            vec![ChatMessage::User {
                content: "hi".into(),
            }],
        );
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(
            v,
            json!({
                "model": "ornith-q4-r0",
                "stream": true,
                "stream_options": {"include_usage": true},
                "messages": [{"role": "user", "content": "hi"}]
            })
        );
    }

    #[test]
    fn request_full_matches_handrolled_shape() {
        // The full body the promoted implementation builds: max_tokens,
        // tools, reasoning (mu-13ve), sampling (mu-y8gp).
        let mut req = ChatCompletionRequest::streaming(
            "qwen3.6:27b",
            vec![ChatMessage::System {
                content: "sys".into(),
            }],
        );
        req.max_tokens = Some(8192);
        req.tools = Some(vec![Tool::function(
            "grep",
            "search files",
            json!({"type": "object", "properties": {"pattern": {"type": "string"}}}),
        )]);
        req.reasoning = Some(Reasoning {
            effort: "high".into(),
        });
        req.temperature = Some(0.6);
        req.top_p = Some(0.95);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(
            v,
            json!({
                "model": "qwen3.6:27b",
                "max_tokens": 8192,
                "stream": true,
                "stream_options": {"include_usage": true},
                "messages": [{"role": "system", "content": "sys"}],
                "tools": [{
                    "type": "function",
                    "function": {
                        "name": "grep",
                        "description": "search files",
                        "parameters": {"type": "object", "properties": {"pattern": {"type": "string"}}}
                    }
                }],
                "reasoning": {"effort": "high"},
                "temperature": 0.6,
                "top_p": 0.95
            })
        );
    }
}
