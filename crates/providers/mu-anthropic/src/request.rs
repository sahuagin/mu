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

use std::collections::BTreeMap;

use crate::finite::{deserialize_option_finite, FiniteF64};
use crate::json::JsonValue;
use serde::{Deserialize, Serialize};

use crate::content::CacheControl;
use crate::message::{Content, Message};

/// A tool the model may call. Wire shape: `{name, description, input_schema}`
/// where `input_schema` is a JSON Schema object (spec :8990). `cache_control`
/// may be attached to the LAST tool to cache the tool block (legacy mu marks
/// the last spec); modeled as optional per-tool for fidelity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: JsonValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

impl Tool {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: JsonValue,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            cache_control: None,
        }
    }
}

/// How the model selects (or is forced to select) a tool. Wire shape: an
/// object internally tagged on `type` (spec values: `auto`, `any`, `tool`,
/// `none`). `disable_parallel_tool_use` is optional and applies to
/// `auto`/`any`/`tool` (omitted when absent); `none` carries no fields.
///
/// Variant names mirror the wire `type` tag 1:1 (`None` → `{"type":"none"}`)
/// so a protocol reader maps code to wire without a lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    /// Model decides whether to call a tool (the default when tools are present).
    Auto {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    /// Model must call one of the provided tools (its choice which).
    Any {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    /// Model must call the specifically named tool.
    Tool {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    /// Model will not call any tool (no fields).
    None,
}

impl ToolChoice {
    /// `{"type":"auto"}` — model decides.
    pub fn auto() -> Self {
        ToolChoice::Auto {
            disable_parallel_tool_use: None,
        }
    }
    /// `{"type":"any"}` — model must use some tool.
    pub fn any() -> Self {
        ToolChoice::Any {
            disable_parallel_tool_use: None,
        }
    }
    /// `{"type":"tool","name":...}` — model must use the named tool.
    pub fn tool(name: impl Into<String>) -> Self {
        ToolChoice::Tool {
            name: name.into(),
            disable_parallel_tool_use: None,
        }
    }
}

/// Request-level `metadata`. The live beta wire carries `user_id`; the spec
/// docs also mention `external_user_id`/`input_file`. The observed `user_id` is
/// typed; any other key round-trips verbatim through `extra` (forward-compat).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Metadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Any metadata key we don't model, preserved across a round-trip. An empty
    /// map flattens to nothing (no key emitted).
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, JsonValue>,
}

/// Extended-thinking config (`thinking`). Internally tagged on `type`. Observed
/// on the live beta wire: `adaptive`; documented standard forms: `enabled`
/// (with a token budget) and `disabled`. This is an OUTBOUND type we construct,
/// so the variant set is intentionally closed — a `type` we don't model
/// deserializes as a hard error (a loud "the wire changed, update the lib"
/// signal) rather than silently mis-modeling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThinkingConfig {
    /// `{"type":"adaptive"}` — model self-budgets its reasoning (observed wire).
    Adaptive,
    /// `{"type":"enabled","budget_tokens":N}` — explicit reasoning budget.
    Enabled { budget_tokens: u32 },
    /// `{"type":"disabled"}` — no extended thinking.
    Disabled,
}

/// Server-side context-editing directives (`context_management`). Observed:
/// `{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ContextManagement {
    pub edits: Vec<ContextEdit>,
}

/// One context-editing directive. `type` is a *versioned* identifier (e.g.
/// `clear_thinking_20251015`) so it stays a `String`, not an enum — the version
/// suffix is open-ended. Unmodeled keys round-trip via `extra`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextEdit {
    #[serde(rename = "type")]
    pub edit_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<String>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, JsonValue>,
}

/// Output controls (`output_config`). Observed: `{"effort":"high"}`. `effort`
/// is a `String` (preserves the value losslessly; the value-space is small but
/// unconfirmed from one capture). Unmodeled keys round-trip via `extra`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OutputConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, JsonValue>,
}

/// The request body for `POST /v1/messages`.
///
/// Construct via [`MessagesRequest::new`] (the three required fields) then the
/// builder-style setters for optional envelope fields. The result is immutable
/// once built and serializes to the exact wire shape.
// Eq holds: temperature/top_p are Option<FiniteF64> (finiteness guaranteed by
// construction), so no raw f64 blocks the derive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u32,
    pub messages: Vec<Message>,

    /// Top-level system prompt. String or block array (polymorphic).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<Content>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,

    /// Tool-selection policy. Omitted when absent (the API then defaults to
    /// `auto` if tools are present). See [`ToolChoice`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,

    /// Whether the response is streamed (SSE). Anthropic always streams large
    /// requests; mu's production path sets this true. Omitted when None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,

    // ---- sampling knobs (all optional, omitted when absent) ----
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,

    // ---- wire-ahead envelope fields (observed on /v1/messages?beta=true,
    //      ahead of the pinned spec snapshot). All optional, omitted-when-absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_management: Option<ContextManagement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
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
            tool_choice: None,
            stream: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: Vec::new(),
            metadata: None,
            thinking: None,
            context_management: None,
            output_config: None,
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

    pub fn with_tool_choice(mut self, tool_choice: ToolChoice) -> Self {
        self.tool_choice = Some(tool_choice);
        self
    }

    pub fn with_stream(mut self, stream: bool) -> Self {
        self.stream = Some(stream);
        self
    }

    pub fn with_metadata(mut self, metadata: Metadata) -> Self {
        self.metadata = Some(metadata);
        self
    }

    pub fn with_thinking(mut self, thinking: ThinkingConfig) -> Self {
        self.thinking = Some(thinking);
        self
    }

    pub fn with_context_management(mut self, context_management: ContextManagement) -> Self {
        self.context_management = Some(context_management);
        self
    }

    pub fn with_output_config(mut self, output_config: OutputConfig) -> Self {
        self.output_config = Some(output_config);
        self
    }

    /// Set temperature. Non-finite (NaN/±Inf) coerces to absent.
    pub fn with_temperature(mut self, t: f64) -> Self {
        self.temperature = FiniteF64::new(t);
        self
    }

    /// Set top_p. Non-finite (NaN/±Inf) coerces to absent.
    pub fn with_top_p(mut self, p: f64) -> Self {
        self.top_p = FiniteF64::new(p);
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
            "tool_choice",
            "stream",
            "temperature",
            "top_p",
            "top_k",
            "stop_sequences",
            "metadata",
            "thinking",
            "context_management",
            "output_config",
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
                JsonValue::new(json!({
                    "type": "object",
                    "properties": {"location": {"type": "string"}},
                    "required": ["location"]
                }))
                .unwrap(),
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
    fn tool_choice_variants_match_spec_shape() {
        // spec values: auto | any | tool | none. disable_parallel_tool_use is
        // omitted unless set.
        let base = || MessagesRequest::new("m", 10, vec![Message::user("hi")]);

        let auto = base().with_tool_choice(ToolChoice::auto());
        assert_eq!(
            serde_json::to_value(&auto).unwrap()["tool_choice"],
            json!({"type": "auto"})
        );
        round_trip(&auto);

        let any = base().with_tool_choice(ToolChoice::any());
        assert_eq!(
            serde_json::to_value(&any).unwrap()["tool_choice"],
            json!({"type": "any"})
        );
        round_trip(&any);

        let named = base().with_tool_choice(ToolChoice::tool("get_weather"));
        assert_eq!(
            serde_json::to_value(&named).unwrap()["tool_choice"],
            json!({"type": "tool", "name": "get_weather"})
        );
        round_trip(&named);

        let none = base().with_tool_choice(ToolChoice::None);
        assert_eq!(
            serde_json::to_value(&none).unwrap()["tool_choice"],
            json!({"type": "none"})
        );
        round_trip(&none);
    }

    #[test]
    fn tool_choice_disable_parallel_serializes_only_when_set() {
        let r = MessagesRequest::new("m", 10, vec![Message::user("hi")]).with_tool_choice(
            ToolChoice::Any {
                disable_parallel_tool_use: Some(true),
            },
        );
        assert_eq!(
            serde_json::to_value(&r).unwrap()["tool_choice"],
            json!({"type": "any", "disable_parallel_tool_use": true})
        );
        round_trip(&r);
        // and absent by default
        let bare = serde_json::to_value(ToolChoice::any()).unwrap();
        assert!(
            bare.get("disable_parallel_tool_use").is_none(),
            "must omit, not null"
        );
    }

    #[test]
    fn request_with_tool_result_turn_round_trips_through_envelope() {
        // The request-side tool-call cycle: assistant asks (tool_use), then a
        // user turn feeds results back as tool_result blocks (one errored).
        // Exercises ToolResult on the INBOUND-to-the-model request path
        // end-to-end through MessagesRequest — the path real traffic uses, here
        // covered synthetically until a captured request fixture exists.
        let req = MessagesRequest::new(
            "claude-fable-5",
            1024,
            vec![
                Message::user("what's the weather and the time?"),
                Message::assistant(vec![ContentBlock::ToolUse {
                    id: "toolu_w".into(),
                    name: "get_weather".into(),
                    input: JsonValue::new(json!({"location": "Paris"})).unwrap(),
                    cache_control: None,
                }]),
                Message::user(vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "toolu_w".into(),
                        content: "18C".into(),
                        is_error: None,
                        cache_control: None,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "toolu_t".into(),
                        content: "tool not found".into(),
                        is_error: Some(true),
                        cache_control: None,
                    },
                ]),
            ],
        );
        let v = serde_json::to_value(&req).unwrap();
        let last = &v["messages"][2];
        assert_eq!(last["role"], "user");
        assert_eq!(last["content"][0]["type"], "tool_result");
        assert_eq!(last["content"][1]["is_error"], json!(true));
        round_trip(&req);
    }

    #[test]
    fn wire_ahead_fields_match_observed_beta_shapes() {
        // Exact shapes captured from a real claude-opus-4-8 request on
        // /v1/messages?beta=true (2026-06-13). These are ahead of the pinned
        // spec snapshot; modeled for OUTBOUND completeness.
        let req = MessagesRequest::new("claude-opus-4-8", 64000, vec![Message::user("hi")])
            .with_metadata(Metadata {
                user_id: Some("usr_x".into()),
                extra: BTreeMap::new(),
            })
            .with_thinking(ThinkingConfig::Adaptive)
            .with_context_management(ContextManagement {
                edits: vec![ContextEdit {
                    edit_type: "clear_thinking_20251015".into(),
                    keep: Some("all".into()),
                    extra: BTreeMap::new(),
                }],
            })
            .with_output_config(OutputConfig {
                effort: Some("high".into()),
                extra: BTreeMap::new(),
            });
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["metadata"], json!({"user_id": "usr_x"}));
        assert_eq!(v["thinking"], json!({"type": "adaptive"}));
        assert_eq!(
            v["context_management"],
            json!({"edits": [{"type": "clear_thinking_20251015", "keep": "all"}]})
        );
        assert_eq!(v["output_config"], json!({"effort": "high"}));
        round_trip(&req);
    }

    #[test]
    fn thinking_enabled_carries_budget_and_round_trips() {
        let req = MessagesRequest::new("m", 10, vec![Message::user("hi")]).with_thinking(
            ThinkingConfig::Enabled {
                budget_tokens: 4096,
            },
        );
        assert_eq!(
            serde_json::to_value(&req).unwrap()["thinking"],
            json!({"type": "enabled", "budget_tokens": 4096})
        );
        round_trip(&req);
    }

    #[test]
    fn wire_ahead_unknown_keys_round_trip_via_extra() {
        // A future metadata/output_config key we don't model must survive a
        // round-trip rather than being silently dropped.
        let raw = json!({
            "model": "m",
            "max_tokens": 10,
            "messages": [{"role": "user", "content": "hi"}],
            "metadata": {"user_id": "u", "external_user_id": "ext"},
            "output_config": {"effort": "high", "verbosity": "low"}
        });
        let req: MessagesRequest = serde_json::from_value(raw.clone()).unwrap();
        // unmodeled keys landed in extra…
        assert_eq!(
            req.metadata.as_ref().unwrap().extra["external_user_id"].as_value(),
            &json!("ext")
        );
        assert_eq!(
            req.output_config.as_ref().unwrap().extra["verbosity"].as_value(),
            &json!("low")
        );
        // …and re-serialize verbatim.
        assert_eq!(serde_json::to_value(&req).unwrap(), raw);
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
