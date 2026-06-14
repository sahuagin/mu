//! MessagesRequest — the request ENVELOPE for `POST /v1/messages`.
//!
//! Header/payload split (financial-spec framing, per PLAN): the envelope
//! carries transport-and-interpretation fields (model, max_tokens, system,
//! tools, sampling knobs, stream, cache directives); the payload is the
//! `messages` array. Envelope points AT the payload; the payload knows nothing
//! of the envelope.
//!
//! Required fields (spec :985 minimal body): `model`, `max_tokens`, `messages`.
//! Everything else is optional and OMITTED when absent (skip_serializing_if) —
//! Anthropic rejects some requests that carry `null` where a field should be
//! absent, so we never emit `null` for an unset knob.
//!
//! `system` is POLYMORPHIC exactly like message content — a bare string
//! (:45088) or a block array (:6921). We reuse [`Content`] for it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::content::CacheControl;
use crate::message::{Content, Message};

/// A tool the model may call. Wire shape: `{name, description, input_schema}`
/// where `input_schema` is a JSON Schema object (spec :8990). `cache_control`
/// may be attached to the LAST tool to cache the tool block (legacy mu marks
/// the last spec); modeled as optional per-tool for fidelity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

impl Tool {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            cache_control: None,
        }
    }
}

/// The request body for `POST /v1/messages`.
///
/// Construct via [`MessagesRequest::new`] (the three required fields) then the
/// builder-style setters for optional envelope fields. The result is immutable
/// once built and serializes to the exact wire shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u32,
    pub messages: Vec<Message>,

    /// Top-level system prompt. String or block array (polymorphic).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<Content>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,

    /// Whether the response is streamed (SSE). Anthropic always streams large
    /// requests; mu's production path sets this true. Omitted when None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,

    // ---- sampling knobs (all optional, omitted when absent) ----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
}

impl MessagesRequest {
    /// The three required fields. Optional envelope fields default to absent
    /// and are added via the `with_*` setters.
    pub fn new(model: impl Into<String>, max_tokens: u32, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            max_tokens,
            messages,
            system: None,
            tools: Vec::new(),
            stream: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: Vec::new(),
        }
    }

    pub fn with_system(mut self, system: impl Into<Content>) -> Self {
        self.system = Some(system.into());
        self
    }

    pub fn with_tools(mut self, tools: Vec<Tool>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_stream(mut self, stream: bool) -> Self {
        self.stream = Some(stream);
        self
    }

    pub fn with_temperature(mut self, t: f64) -> Self {
        self.temperature = Some(t);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ContentBlock;
    use crate::message::Message;
    use serde_json::json;

    fn round_trip(r: &MessagesRequest) {
        let s = serde_json::to_string(r).unwrap();
        let back: MessagesRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(r, &back, "round-trip mismatch via {s}");
    }

    #[test]
    fn minimal_request_matches_spec_shape() {
        // spec :985 — the documented minimal body.
        let r = MessagesRequest::new("claude-fable-5", 1024, vec![Message::user("Hello, Claude")]);
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({
                "model": "claude-fable-5",
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": "Hello, Claude"}]
            })
        );
        round_trip(&r);
    }

    #[test]
    fn optional_fields_omitted_when_absent_never_null() {
        // The minimal request must NOT carry system/tools/stream/temperature
        // as null — Anthropic rejects some null-bearing requests.
        let r = MessagesRequest::new("m", 10, vec![Message::user("hi")]);
        let v = serde_json::to_value(&r).unwrap();
        for absent in [
            "system",
            "tools",
            "stream",
            "temperature",
            "top_p",
            "top_k",
            "stop_sequences",
        ] {
            assert!(
                v.get(absent).is_none(),
                "{absent} must be omitted, not null"
            );
        }
    }

    #[test]
    fn system_string_form() {
        // spec :45088 — system as a bare string.
        let r = MessagesRequest::new("m", 10, vec![Message::user("hi")])
            .with_system("You are a helpful general-purpose agent.");
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            v["system"],
            json!("You are a helpful general-purpose agent.")
        );
        round_trip(&r);
    }

    #[test]
    fn system_block_array_form_with_cache_control() {
        // spec :6921 — system as a block array, with cache_control for prompt
        // caching of a large system prompt.
        let r = MessagesRequest::new("m", 10, vec![Message::user("hi")]).with_system(vec![
            ContentBlock::Text {
                text: "<book>".into(),
                cache_control: Some(CacheControl::ephemeral()),
            },
        ]);
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            v["system"],
            json!([{"type": "text", "text": "<book>", "cache_control": {"type": "ephemeral"}}])
        );
        round_trip(&r);
    }

    #[test]
    fn tools_match_spec_shape() {
        // spec :8990 — get_weather tool.
        let r = MessagesRequest::new("m", 10, vec![Message::user("weather?")]).with_tools(vec![
            Tool::new(
                "get_weather",
                "Get current weather for a location",
                json!({
                    "type": "object",
                    "properties": {"location": {"type": "string"}},
                    "required": ["location"]
                }),
            ),
        ]);
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            v["tools"],
            json!([{
                "name": "get_weather",
                "description": "Get current weather for a location",
                "input_schema": {
                    "type": "object",
                    "properties": {"location": {"type": "string"}},
                    "required": ["location"]
                }
            }])
        );
        round_trip(&r);
    }

    #[test]
    fn stream_and_sampling_serialize_when_set() {
        let r = MessagesRequest::new("m", 10, vec![Message::user("hi")])
            .with_stream(true)
            .with_temperature(0.7);
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["stream"], json!(true));
        assert_eq!(v["temperature"], json!(0.7));
        round_trip(&r);
    }
}
