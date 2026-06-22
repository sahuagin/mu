//! `POST /v1/responses` request body — the OUTBOUND envelope + payload.
//!
//! `CreateResponseRequest` is the envelope (model, instructions, sampling,
//! tools, reasoning knobs); `input: Vec<InputItem>` is the payload (the
//! conversation so far). We only ever *construct* requests, so the enums here
//! are closed (no `Unknown` fallback) — the inbound-robustness fallbacks live on
//! the response/stream types.
//!
//! Reasoning threading (the o-series / gpt-5 Responses contract): a `reasoning`
//! item returned in a prior response's `output` must be fed back verbatim as an
//! `InputItem::Reasoning` on the next turn (alongside `include:
//! ["reasoning.encrypted_content"]` when stateless, `store=false`), or the model
//! loses its chain-of-thought across tool calls and can stall. The crate models
//! the shape; the mu provider does the round-trip.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::finite::{deserialize_option_finite, FiniteF64};
use crate::JsonValue;

/// `POST /v1/responses` request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateResponseRequest {
    pub model: String,
    /// System/developer context. A top-level field, not a message item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// The conversation so far (messages, tool calls/outputs, reasoning items).
    pub input: Vec<InputItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Whether the backend persists the response server-side. mu runs stateless
    /// (`store=false`) and threads context via `input` + reasoning items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_option_finite"
    )]
    pub temperature: Option<FiniteF64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_option_finite"
    )]
    pub top_p: Option<FiniteF64>,
    /// Stateful threading alternative to resending `input` (we don't use it, but
    /// model it for completeness).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    /// Extra data to include in the response, e.g. `reasoning.encrypted_content`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

impl CreateResponseRequest {
    /// A minimal request: model + a list of input items, stateless.
    pub fn new(model: impl Into<String>, input: Vec<InputItem>) -> Self {
        Self {
            model: model.into(),
            instructions: None,
            input,
            tools: Vec::new(),
            tool_choice: None,
            reasoning: None,
            max_output_tokens: None,
            stream: None,
            store: Some(false),
            parallel_tool_calls: None,
            temperature: None,
            top_p: None,
            previous_response_id: None,
            include: Vec::new(),
            metadata: BTreeMap::new(),
        }
    }

    /// Convenience: a single-user-message request.
    pub fn text(model: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(model, vec![InputItem::user_text(text)])
    }

    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }
    pub fn with_tools(mut self, tools: Vec<Tool>) -> Self {
        self.tools = tools;
        self
    }
    pub fn with_reasoning(mut self, reasoning: Reasoning) -> Self {
        self.reasoning = Some(reasoning);
        self
    }
    pub fn streaming(mut self) -> Self {
        self.stream = Some(true);
        self
    }
}

/// Reasoning knobs. `effort` is the model's vocabulary (e.g. minimal/low/medium/
/// high); kept a `String` so an out-of-vocabulary level is a provider concern,
/// not a crate-level parse failure. `summary` is auto/concise/detailed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reasoning {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// One item in the `input` array.
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
        /// Arguments are a JSON *string* on the wire (the model's emitted JSON).
        arguments: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
    /// A reasoning item threaded back from a prior response's `output`. Echoed
    /// verbatim (id + encrypted_content + summary) so the model keeps its
    /// chain-of-thought across tool calls.
    Reasoning {
        id: String,
        /// REQUIRED on the wire even when empty: the Responses backend rejects a
        /// reasoning input item without `summary` (`missing_required_parameter`
        /// for `input[N].summary`). Always serialize it (as `[]` when empty —
        /// encrypted-only reasoning has no summary text but still threads via
        /// `encrypted_content`). Hence NO `skip_serializing_if` here.
        #[serde(default)]
        summary: Vec<JsonValue>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        content: Vec<JsonValue>,
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

/// A content part within a message input item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputContent {
    InputText { text: String },
    OutputText { text: String },
}

/// A tool the model may call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Tool {
    Function(FunctionTool),
}

/// A function tool (Responses API "flat" shape: name/description/parameters at
/// the tool's top level, not nested under `function`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionTool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for the arguments.
    pub parameters: JsonValue,
    /// Strict-schema adherence. Optional on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

/// `tool_choice`: either a mode string (`auto`/`none`/`required`) or a named
/// function selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolChoice {
    Mode(ToolChoiceMode),
    Named(NamedToolChoice),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoiceMode {
    Auto,
    None,
    Required,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedToolChoice {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
}

impl ToolChoice {
    pub fn auto() -> Self {
        Self::Mode(ToolChoiceMode::Auto)
    }
    pub fn required() -> Self {
        Self::Mode(ToolChoiceMode::Required)
    }
    pub fn function(name: impl Into<String>) -> Self {
        Self::Named(NamedToolChoice {
            kind: "function".into(),
            name: name.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_trip(req: &CreateResponseRequest) {
        let v = serde_json::to_value(req).unwrap();
        let back: CreateResponseRequest = serde_json::from_value(v).unwrap();
        assert_eq!(&back, req);
    }

    #[test]
    fn minimal_text_request_shape() {
        let req = CreateResponseRequest::text("gpt-4.1-mini", "hi");
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            json!({
                "model": "gpt-4.1-mini",
                "input": [{"type": "message", "role": "user",
                           "content": [{"type": "input_text", "text": "hi"}]}],
                "store": false
            })
        );
        round_trip(&req);
    }

    #[test]
    fn optional_fields_omitted_not_null() {
        let req = CreateResponseRequest::text("m", "hi");
        let v = serde_json::to_value(&req).unwrap();
        for absent in [
            "instructions",
            "tools",
            "tool_choice",
            "reasoning",
            "temperature",
            "top_p",
            "include",
            "metadata",
            "previous_response_id",
        ] {
            assert!(v.get(absent).is_none(), "{absent} should be omitted");
        }
    }

    #[test]
    fn function_tool_is_flat_with_optional_strict() {
        let req =
            CreateResponseRequest::text("m", "hi").with_tools(vec![Tool::Function(FunctionTool {
                name: "read".into(),
                description: Some("Read a file".into()),
                parameters: JsonValue::new(json!({"type": "object"})).unwrap(),
                strict: Some(true),
            })]);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(
            v["tools"][0],
            json!({"type": "function", "name": "read", "description": "Read a file",
                   "parameters": {"type": "object"}, "strict": true})
        );
        round_trip(&req);
    }

    #[test]
    fn reasoning_and_sampling_and_include() {
        let mut req = CreateResponseRequest::text("m", "hi")
            .with_reasoning(Reasoning {
                effort: Some("low".into()),
                summary: Some("auto".into()),
            })
            .streaming();
        req.temperature = FiniteF64::new(0.7);
        req.top_p = FiniteF64::new(0.9);
        req.include = vec!["reasoning.encrypted_content".into()];
        req.tool_choice = Some(ToolChoice::auto());
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["reasoning"], json!({"effort": "low", "summary": "auto"}));
        assert_eq!(v["temperature"], json!(0.7));
        assert_eq!(v["top_p"], json!(0.9));
        assert_eq!(v["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(v["tool_choice"], json!("auto"));
        assert_eq!(v["stream"], json!(true));
        round_trip(&req);
    }

    #[test]
    fn tool_choice_named_function() {
        let v = serde_json::to_value(ToolChoice::function("read")).unwrap();
        assert_eq!(v, json!({"type": "function", "name": "read"}));
        let back: ToolChoice = serde_json::from_value(v).unwrap();
        assert_eq!(back, ToolChoice::function("read"));
        // Mode string still round-trips distinctly from the named form.
        let m = serde_json::to_value(ToolChoice::required()).unwrap();
        assert_eq!(m, json!("required"));
        assert_eq!(
            serde_json::from_value::<ToolChoice>(m).unwrap(),
            ToolChoice::required()
        );
    }

    #[test]
    fn function_call_and_output_and_reasoning_items_round_trip() {
        let req = CreateResponseRequest::new(
            "m",
            vec![
                InputItem::user_text("call read"),
                InputItem::FunctionCall {
                    call_id: "call_1".into(),
                    name: "read".into(),
                    arguments: "{\"path\":\"a\"}".into(),
                    id: Some("fc_1".into()),
                },
                InputItem::FunctionCallOutput {
                    call_id: "call_1".into(),
                    output: "contents".into(),
                },
                InputItem::Reasoning {
                    id: "rs_1".into(),
                    summary: vec![JsonValue::new(json!({"type": "summary_text",
                                                        "text": "thinking"}))
                    .unwrap()],
                    encrypted_content: Some("enc==".into()),
                    content: Vec::new(),
                },
            ],
        );
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["input"][1]["type"], "function_call");
        assert_eq!(v["input"][2]["type"], "function_call_output");
        assert_eq!(v["input"][3]["type"], "reasoning");
        assert_eq!(v["input"][3]["encrypted_content"], "enc==");
        round_trip(&req);
    }

    #[test]
    fn reasoning_input_item_always_emits_summary_even_when_empty() {
        // The Responses backend rejects a reasoning input item that omits
        // `summary` (400: `input[N].summary` missing_required_parameter). An
        // encrypted-only reasoning item has no summary text but must still send
        // `summary: []`.
        let item = InputItem::Reasoning {
            id: "rs_1".into(),
            summary: Vec::new(),
            encrypted_content: Some("enc==".into()),
            content: Vec::new(),
        };
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(v["type"], "reasoning");
        assert_eq!(
            v["summary"],
            json!([]),
            "summary must serialize as []; got {v}"
        );
        assert_eq!(v["encrypted_content"], "enc==");
        // content stays omitted when empty (it is genuinely optional).
        assert!(v.get("content").is_none());
    }
}
