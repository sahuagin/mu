//! MessagesRequest — the request ENVELOPE for `POST /v1/messages`.
//!
//! Header/payload split (financial-spec framing, per PLAN): the envelope
//! carries transport-and-interpretation fields (model, max_tokens, system,
//! tools, sampling knobs, stream, cache directives); the payload is the
//! `messages` array. Envelope points AT the payload; the payload knows nothing
//! of the envelope.
//!
//! Required fields (`/docs/en/api/messages/create`, the three parameters not
//! marked optional): `model`, `max_tokens`, `messages`.
//! Everything else is optional and OMITTED when absent (skip_serializing_if) —
//! Anthropic rejects some requests that carry `null` where a field should be
//! absent, so we never emit `null` for an unset knob.
//!
//! `system` is POLYMORPHIC exactly like message content — a bare string
//! (`/docs/en/build-with-claude/mid-conversation-effort-example § Set up the
//! loop`) or a block array (`/docs/en/build-with-claude/batch-processing
//! § Using prompt caching with Message Batches`). We reuse [`Content`] for
//! it. A `role: "system"` entry INSIDE `messages` is a different thing — a
//! mid-conversation system message, see [`Message`].

use std::collections::BTreeMap;

use crate::finite::{deserialize_option_finite, FiniteF64};
use crate::json::JsonValue;
use serde::{Deserialize, Serialize};

use crate::content::CacheControl;
use crate::message::{Content, Message};

/// A tool the model may call. Wire shape: `{name, description, input_schema}`
/// where `input_schema` is a JSON Schema object
/// (`/docs/en/build-with-claude/handling-stop-reasons § tool_use`). `cache_control`
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

/// One entry of the request `tools` array. The array is heterogeneous on the
/// wire: custom tools (a JSON-Schema `input_schema`, optionally tagged
/// `"type":"custom"`) sit alongside built-in/server tools tagged with a
/// *versioned* `type` (`web_search_20250305`, `text_editor_20250728`,
/// `mcp_toolset`, …). Modeled like [`crate::ContentBlock`]: a hand-written
/// `Deserialize` peeks at `type` and routes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolDef {
    /// A user-defined tool (`{name, description, input_schema, cache_control?}`).
    /// Reached for a type-less entry OR `"type":"custom"`; parsed STRICTLY — a
    /// malformed custom tool errors rather than silently passing through.
    Custom(Tool),
    /// Any built-in/server tool, carried generically (see [`ServerTool`]).
    /// `mcp_toolset` lands here too (family=`mcp_toolset`, version=None,
    /// `mcp_server_name` in `config`).
    Server(ServerTool),
    /// A `tools` entry that isn't a JSON object — preserved verbatim.
    Unknown(JsonValue),
}

/// A built-in/server tool, modeled generically. The versioned wire `type` is
/// normalized: `web_search_20250305` → family `web_search` + version
/// `20250305`; [`ServerTool::wire_type`] rejoins them. `name` is pulled out;
/// every other field (`max_uses`, `allowed_domains`, `mcp_server_name`,
/// `display_width_px`, …) rides in `config` verbatim, so new/unknown config
/// keys round-trip without a code change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerTool {
    pub family: String,
    pub version: Option<String>,
    pub name: Option<String>,
    pub config: BTreeMap<String, JsonValue>,
}

impl ServerTool {
    /// The wire `type`: `family` or `family_version`.
    pub fn wire_type(&self) -> String {
        match &self.version {
            Some(v) => format!("{}_{}", self.family, v),
            None => self.family.clone(),
        }
    }

    /// Split a wire `type` into `(family, version)`. The version is the trailing
    /// `_NNNN…` segment when it is non-empty and ALL ASCII digits (the
    /// `_YYYYMMDD` convention); otherwise the whole string is the family and
    /// there is no version (e.g. `mcp_toolset` → (`mcp_toolset`, None)).
    fn split_type(t: &str) -> (String, Option<String>) {
        if let Some((head, tail)) = t.rsplit_once('_') {
            if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) {
                return (head.to_string(), Some(tail.to_string()));
            }
        }
        (t.to_string(), None)
    }
}

impl From<Tool> for ToolDef {
    fn from(t: Tool) -> Self {
        ToolDef::Custom(t)
    }
}

impl<'de> Deserialize<'de> for ToolDef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let raw = serde_json::Value::deserialize(deserializer)?;
        let obj = match raw.as_object() {
            Some(o) => o,
            // A non-object tools entry: preserve verbatim rather than error.
            None => {
                return Ok(ToolDef::Unknown(
                    JsonValue::new(raw).map_err(D::Error::custom)?,
                ))
            }
        };
        match obj.get("type").and_then(serde_json::Value::as_str) {
            // type-less or explicit "custom": a user tool, parsed strictly.
            None | Some("custom") => {
                let mut m = obj.clone();
                m.remove("type");
                let tool: Tool = serde_json::from_value(serde_json::Value::Object(m))
                    .map_err(D::Error::custom)?;
                Ok(ToolDef::Custom(tool))
            }
            // any other type: a built-in/server tool, carried generically.
            Some(t) => {
                let (family, version) = ServerTool::split_type(t);
                let name = obj
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(String::from);
                let mut config = BTreeMap::new();
                for (k, v) in obj {
                    if k == "type" || k == "name" {
                        continue;
                    }
                    config.insert(
                        k.clone(),
                        JsonValue::new(v.clone()).map_err(D::Error::custom)?,
                    );
                }
                Ok(ToolDef::Server(ServerTool {
                    family,
                    version,
                    name,
                    config,
                }))
            }
        }
    }
}

impl Serialize for ToolDef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            // Custom serializes type-less (the canonical form); an incoming
            // "type":"custom" normalizes away.
            ToolDef::Custom(tool) => tool.serialize(serializer),
            ToolDef::Unknown(v) => v.serialize(serializer),
            ToolDef::Server(s) => {
                let mut map = serde_json::Map::new();
                map.insert("type".into(), serde_json::Value::String(s.wire_type()));
                if let Some(name) = &s.name {
                    map.insert("name".into(), serde_json::Value::String(name.clone()));
                }
                for (k, v) in &s.config {
                    map.insert(k.clone(), v.as_value().clone());
                }
                serde_json::Value::Object(map).serialize(serializer)
            }
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

/// `thinking.display` — what the returned `thinking` blocks carry
/// (`/docs/en/build-with-claude/thinking § Controlling thinking display`):
/// `summarized` (a summary of the reasoning), `omitted` (empty `thinking`
/// text, signature only), and `updates` (empty reasoning text, but the short
/// progress updates some models write between tool calls come back as
/// readable text — `§ Progress updates between tool calls`; beta
/// `thinking-display-updates-2026-08-18`). Invalid with `type: "disabled"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingDisplay {
    Summarized,
    Omitted,
    Updates,
}

/// `thinking.block_binding.prefix_mismatch_behavior` — what the API does with
/// a replayed thinking block whose conversation prefix has changed
/// (`/docs/en/build-with-claude/preserved-thinking § What the API does with an
/// invalid block`): `error`, the default, is a 400 naming the first failing
/// block; `drop_block` drops it and every thinking block after it, and lists
/// the drops in the response's `input_transformations`. Beta
/// `thinking-binding-controls-2026-08-01`, whichever value is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrefixMismatchBehavior {
    Error,
    DropBlock,
}

/// `thinking.block_binding`, whose one documented field is
/// `prefix_mismatch_behavior` (`/docs/en/build-with-claude/preserved-thinking
/// § Set the mismatch behavior and read input_transformations`). Accepted
/// alongside `adaptive` and `enabled`. Unmodeled keys round-trip via `extra`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BlockBinding {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix_mismatch_behavior: Option<PrefixMismatchBehavior>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, JsonValue>,
}

/// Extended-thinking config (`thinking`). Internally tagged on `type`. Observed
/// on the live beta wire: `adaptive`; documented standard forms: `enabled`
/// (with a token budget) and `disabled`. This is an OUTBOUND type we construct,
/// so the variant set is intentionally closed — a `type` we don't model
/// deserializes as a hard error (a loud "the wire changed, update the lib"
/// signal) rather than silently mis-modeling.
///
/// `adaptive` and `enabled` take the same two optional knobs, `display`
/// ([`ThinkingDisplay`]) and `block_binding` ([`BlockBinding`]), both omitted
/// when unset, so `{"type":"adaptive"}` is still the bytes the observed wire
/// carried; `disabled` takes neither (the API rejects `display` there).
/// Build with [`ThinkingConfig::adaptive`] / [`ThinkingConfig::enabled`] and
/// the `with_*` setters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThinkingConfig {
    /// `{"type":"adaptive"}` — model self-budgets its reasoning (observed wire).
    Adaptive {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        block_binding: Option<BlockBinding>,
    },
    /// `{"type":"enabled","budget_tokens":N}` — explicit reasoning budget.
    Enabled {
        budget_tokens: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        block_binding: Option<BlockBinding>,
    },
    /// `{"type":"disabled"}` — no extended thinking.
    Disabled,
}

impl ThinkingConfig {
    /// `{"type":"adaptive"}`.
    pub fn adaptive() -> Self {
        ThinkingConfig::Adaptive {
            display: None,
            block_binding: None,
        }
    }

    /// `{"type":"enabled","budget_tokens":N}`.
    pub fn enabled(budget_tokens: u32) -> Self {
        ThinkingConfig::Enabled {
            budget_tokens,
            display: None,
            block_binding: None,
        }
    }

    /// Set `display`. `disabled` has no such field (the API rejects it
    /// there), so this leaves a `Disabled` config unchanged.
    pub fn with_display(mut self, value: ThinkingDisplay) -> Self {
        if let ThinkingConfig::Adaptive { display, .. } | ThinkingConfig::Enabled { display, .. } =
            &mut self
        {
            *display = Some(value);
        }
        self
    }

    /// Set `block_binding.prefix_mismatch_behavior` (keeping any other
    /// `block_binding` keys). Leaves a `Disabled` config unchanged.
    pub fn with_prefix_mismatch_behavior(mut self, behavior: PrefixMismatchBehavior) -> Self {
        if let ThinkingConfig::Adaptive { block_binding, .. }
        | ThinkingConfig::Enabled { block_binding, .. } = &mut self
        {
            block_binding
                .get_or_insert_with(BlockBinding::default)
                .prefix_mismatch_behavior = Some(behavior);
        }
        self
    }
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

/// An MCP server the request exposes to the model (spec: MCP connector,
/// `mcp-client-2025-11-20`). Wire: `{"type":"url","url":...,"name":...,
/// "authorization_token"?:...,"tool_configuration"?:{...}}`. `type` is `url`
/// today; kept as a String for forward-compat. Unmodeled keys round-trip via
/// `extra`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServer {
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_configuration: Option<McpToolConfiguration>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, JsonValue>,
}

impl McpServer {
    /// A `url`-type MCP server with no auth token / tool config.
    pub fn url(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            kind: "url".into(),
            url: url.into(),
            name: name.into(),
            authorization_token: None,
            tool_configuration: None,
            extra: BTreeMap::new(),
        }
    }
}

/// Which of an MCP server's tools are exposed (spec). `enabled: false` disables
/// the server's tools; `allowed_tools` whitelists by name. Both optional.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct McpToolConfiguration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_tools: Vec<String>,
}

/// `speed` — inference speed mode (`/docs/en/api/beta/messages/create § Body
/// Parameters`): "`fast` provides significantly faster output token generation
/// at premium pricing. Not all models support `fast`; invalid combinations are
/// rejected at create time." Which models is the catalog's business.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Speed {
    Standard,
    Fast,
}

/// One entry of an explicit `fallbacks` list
/// (`/docs/en/build-with-claude/refusals-and-fallback § Naming your own
/// fallback models`): "Each entry names a `model` and can override
/// `max_tokens`, `thinking`, `output_config`, and `speed` for that attempt
/// only." Entries are tried in order, must be distinct from each other and
/// from the requested model, and must be among the requested model's
/// permitted targets (`allowed_fallback_models` in the Models API); all of
/// that is validated server-side. Unmodeled keys round-trip via `extra`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FallbackTarget {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<Speed>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, JsonValue>,
}

impl FallbackTarget {
    /// `{"model": ...}` with no per-attempt overrides.
    pub fn model(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            max_tokens: None,
            thinking: None,
            output_config: None,
            speed: None,
            extra: BTreeMap::new(),
        }
    }
}

/// `fallbacks` — server-side retry on substitute models when the requested
/// model declines for policy reasons (`/docs/en/build-with-claude/refusals-and-fallback
/// § Server-side fallback`; beta `server-side-fallback-2026-07-01`). Two wire
/// forms: the string `"default"` (`§ Making the request`: the requested
/// model's server-defined routing picks the fallback by refusal category)
/// or a list of up to three [`FallbackTarget`]s (`§ Naming your own fallback
/// models`). Only a safety-classifier decline triggers it; the response side
/// — the `fallback` content block, `usage.iterations`, the top-level `model`
/// — is modeled in `content.rs` / `response.rs`. An outbound type, so any
/// other string is a hard error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fallbacks {
    Default,
    Models(Vec<FallbackTarget>),
}

impl Serialize for Fallbacks {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Fallbacks::Default => serializer.serialize_str("default"),
            Fallbacks::Models(targets) => targets.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Fallbacks {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let raw = serde_json::Value::deserialize(deserializer)?;
        match raw {
            serde_json::Value::String(s) if s == "default" => Ok(Fallbacks::Default),
            serde_json::Value::Array(_) => serde_json::from_value(raw)
                .map(Fallbacks::Models)
                .map_err(D::Error::custom),
            other => Err(D::Error::custom(format!(
                "fallbacks: expected \"default\" or a list of targets, got {other}"
            ))),
        }
    }
}

/// How a failing `fallback_credit_token` affects the retry
/// (`/docs/en/api/beta/messages/create § Body Parameters`,
/// `fallback_credit_token`): `strict` (the default, and the bare-string
/// behavior) makes a failing redemption a 400; `best_effort` serves the retry
/// either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CreditRedemption {
    Strict,
    BestEffort,
}

/// `fallback_credit_token` — the credit from a prior refusal's `stop_details`
/// (see [`StopDetails`](crate::response::StopDetails)), presented on the retry
/// so its cache-creation tokens bill at the cache-read rate. Two wire forms
/// (same reference entry): the bare string, or `{"token", "mode"?}` — the
/// object form needs beta `fallback-credit-2026-07-01`, and a mode-less
/// object equals the bare string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FallbackCreditToken {
    Token(String),
    WithMode {
        token: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<CreditRedemption>,
    },
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
    pub tools: Vec<ToolDef>,

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

    /// Request service tier (spec, e.g. `auto`/`standard_only`). Kept as a
    /// String — lossless + forward-compat over the small value set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    /// Code-execution container id to reuse (spec). Opaque string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// MCP servers exposed to the model (spec: MCP connector). Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServer>,

    /// Server-side fallback on a policy refusal (see [`Fallbacks`]); beta
    /// `server-side-fallback-2026-07-01`. Omitted when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallbacks: Option<Fallbacks>,
    /// Inference speed mode (see [`Speed`]). Omitted when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<Speed>,
    /// A prior refusal's credit, redeemed on the retry (see
    /// [`FallbackCreditToken`]). Omitted when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_credit_token: Option<FallbackCreditToken>,
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
            service_tier: None,
            container: None,
            mcp_servers: Vec::new(),
            fallbacks: None,
            speed: None,
            fallback_credit_token: None,
        }
    }

    pub fn with_fallbacks(mut self, fallbacks: Fallbacks) -> Self {
        self.fallbacks = Some(fallbacks);
        self
    }

    pub fn with_speed(mut self, speed: Speed) -> Self {
        self.speed = Some(speed);
        self
    }

    pub fn with_fallback_credit_token(mut self, token: FallbackCreditToken) -> Self {
        self.fallback_credit_token = Some(token);
        self
    }

    pub fn with_system(mut self, system: impl Into<Content>) -> Self {
        self.system = Some(system.into());
        self
    }

    pub fn with_tools(mut self, tools: Vec<ToolDef>) -> Self {
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

    pub fn with_service_tier(mut self, service_tier: impl Into<String>) -> Self {
        self.service_tier = Some(service_tier.into());
        self
    }

    pub fn with_container(mut self, container: impl Into<String>) -> Self {
        self.container = Some(container.into());
        self
    }

    pub fn with_mcp_servers(mut self, mcp_servers: Vec<McpServer>) -> Self {
        self.mcp_servers = mcp_servers;
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
    use crate::message::{ClearAt, Message, Role};
    use serde_json::json;

    fn round_trip(r: &MessagesRequest) {
        let s = serde_json::to_string(r).unwrap();
        let back: MessagesRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(r, &back, "round-trip mismatch via {s}");
    }

    #[test]
    fn minimal_request_matches_spec_shape() {
        // /docs/en/api/messages/create — the three required parameters and
        // the `messages` parameter's single-message example.
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
            "service_tier",
            "container",
            "mcp_servers",
        ] {
            assert!(
                v.get(absent).is_none(),
                "{absent} must be omitted, not null"
            );
        }
    }

    #[test]
    fn system_string_form() {
        // /docs/en/build-with-claude/mid-conversation-effort-example § Set up
        // the loop — system as a bare string.
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
        // /docs/en/build-with-claude/batch-processing § Using prompt caching
        // with Message Batches — system as a block array, with cache_control
        // for prompt caching of a large system prompt.
        let r = MessagesRequest::new("m", 10, vec![Message::user("hi")]).with_system(vec![
            ContentBlock::Text {
                text: "<book>".into(),
                citations: Vec::new(),
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
    fn per_message_effort_request_matches_spec_shape() {
        // /docs/en/build-with-claude/effort § Per-message effort (beta) — the
        // documented request: top-level `high`, then an effort-only system
        // message drops to `low` for the routine follow-up.
        let r = MessagesRequest::new(
            "claude-fable-5-1",
            4096,
            vec![
                Message::user("Plan a migration from SQLite to PostgreSQL in three short steps."),
                Message::assistant(
                    "1. Export the SQLite data. 2. Create the PostgreSQL schema. 3. Import the data and verify row counts.",
                ),
                Message::system_effort("low"),
                Message::user("Summarize the plan in one sentence."),
            ],
        )
        .with_output_config(OutputConfig {
            effort: Some("high".into()),
            ..OutputConfig::default()
        });
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({
                "model": "claude-fable-5-1",
                "max_tokens": 4096,
                "output_config": {"effort": "high"},
                "messages": [
                    {"role": "user", "content": "Plan a migration from SQLite to PostgreSQL in three short steps."},
                    {"role": "assistant", "content": "1. Export the SQLite data. 2. Create the PostgreSQL schema. 3. Import the data and verify row counts."},
                    {"role": "system", "content": [], "output_config": {"effort": "low"}},
                    {"role": "user", "content": "Summarize the plan in one sentence."}
                ]
            })
        );
        round_trip(&r);
    }

    #[test]
    fn turn_scoped_reminders_parse_from_the_documented_agent_loop() {
        // /docs/en/build-with-claude/mid-conversation-system-messages
        // § Turn-scoped system messages — a later step of an agent loop: the
        // cleared reminder (messages[3]) left in place verbatim, two live ones
        // ending the array, thinking blocks with empty text, and the cache
        // breakpoint on the user turn before the reminders, not on them.
        let raw = json!({
            "model": "claude-fable-5-1",
            "max_tokens": 16000,
            "messages": [
                { "role": "user", "content": "Fix the failing test." },
                {
                    "role": "assistant",
                    "content": [
                        { "type": "thinking", "thinking": "", "signature": "..." },
                        {
                            "type": "tool_use",
                            "id": "toolu_01",
                            "name": "read_file",
                            "input": { "path": "test_auth.py" }
                        }
                    ]
                },
                {
                    "role": "user",
                    "content": [{ "type": "tool_result", "tool_use_id": "toolu_01", "content": "..." }]
                },
                {
                    "role": "system",
                    "clear_at": "next_user_message",
                    "content": "Request independent reads in one turn."
                },
                {
                    "role": "assistant",
                    "content": [
                        { "type": "thinking", "thinking": "", "signature": "..." },
                        {
                            "type": "tool_use",
                            "id": "toolu_02",
                            "name": "read_file",
                            "input": { "path": "auth.py" }
                        },
                        {
                            "type": "tool_use",
                            "id": "toolu_03",
                            "name": "read_file",
                            "input": { "path": "tokens.py" }
                        }
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        { "type": "tool_result", "tool_use_id": "toolu_02", "content": "..." },
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_03",
                            "content": "...",
                            "cache_control": { "type": "ephemeral" }
                        }
                    ]
                },
                {
                    "role": "system",
                    "clear_at": "next_user_message",
                    "content": "Request independent reads in one turn."
                },
                {
                    "role": "system",
                    "clear_at": "next_user_message",
                    "content": "The shell exited with status 137."
                }
            ]
        });
        let r: MessagesRequest = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(
            r.messages.iter().map(|m| m.role).collect::<Vec<_>>(),
            [
                Role::User,
                Role::Assistant,
                Role::User,
                Role::System,
                Role::Assistant,
                Role::User,
                Role::System,
                Role::System,
            ]
        );
        for i in [3, 6, 7] {
            assert_eq!(
                r.messages[i].clear_at,
                Some(ClearAt::NextUserMessage),
                "{i}"
            );
            assert!(r.messages[i].output_config.is_none(), "{i}");
        }
        assert_eq!(
            r.messages[3],
            Message::turn_scoped("Request independent reads in one turn.")
        );
        assert_eq!(r.messages[3], r.messages[6], "re-sent verbatim");
        // The whole document re-serializes to itself: nothing dropped, nothing
        // added, so the cleared message really does go back out unchanged.
        assert_eq!(serde_json::to_value(&r).unwrap(), raw);
    }

    #[test]
    fn fallbacks_match_the_documented_requests() {
        // /docs/en/build-with-claude/refusals-and-fallback § Making the
        // request — the string form.
        let r = MessagesRequest::new("claude-fable-5", 1024, vec![Message::user("Hello, Claude")])
            .with_fallbacks(Fallbacks::Default);
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({
                "model": "claude-fable-5",
                "max_tokens": 1024,
                "fallbacks": "default",
                "messages": [{"role": "user", "content": "Hello, Claude"}]
            })
        );
        round_trip(&r);
        // § Naming your own fallback models — the explicit list; the only
        // difference from the default-routing request.
        let r = MessagesRequest::new("claude-fable-5", 1024, vec![Message::user("Hello, Claude")])
            .with_fallbacks(Fallbacks::Models(vec![FallbackTarget::model(
                "claude-opus-4-8",
            )]));
        assert_eq!(
            serde_json::to_value(&r).unwrap()["fallbacks"],
            json!([{"model": "claude-opus-4-8"}])
        );
        round_trip(&r);
        // Per-attempt overrides ride on the entry, and only when set.
        let target = FallbackTarget {
            max_tokens: Some(2048),
            thinking: Some(ThinkingConfig::adaptive()),
            output_config: Some(OutputConfig {
                effort: Some("low".into()),
                ..OutputConfig::default()
            }),
            speed: Some(Speed::Fast),
            ..FallbackTarget::model("claude-opus-5")
        };
        assert_eq!(
            serde_json::to_value(&target).unwrap(),
            json!({
                "model": "claude-opus-5", "max_tokens": 2048,
                "thinking": {"type": "adaptive"},
                "output_config": {"effort": "low"}, "speed": "fast"
            })
        );
        round_trip(
            &MessagesRequest::new("m", 1, vec![Message::user("hi")])
                .with_fallbacks(Fallbacks::Models(vec![target])),
        );
        // An outbound type: the only string the API takes is "default".
        assert!(serde_json::from_value::<Fallbacks>(json!("recommended")).is_err());
        assert!(serde_json::from_value::<Fallbacks>(json!({"model": "x"})).is_err());
    }

    #[test]
    fn speed_and_fallback_credit_token_match_the_reference_shapes() {
        // /docs/en/api/beta/messages/create § Body Parameters — `speed`, and
        // the two forms of `fallback_credit_token`.
        let r = MessagesRequest::new("m", 1, vec![Message::user("hi")])
            .with_speed(Speed::Fast)
            .with_fallback_credit_token(FallbackCreditToken::Token("fct_01".into()));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["speed"], json!("fast"));
        assert_eq!(v["fallback_credit_token"], json!("fct_01"));
        round_trip(&r);
        let r = MessagesRequest::new("m", 1, vec![Message::user("hi")]).with_fallback_credit_token(
            FallbackCreditToken::WithMode {
                token: "fct_01".into(),
                mode: Some(CreditRedemption::BestEffort),
            },
        );
        assert_eq!(
            serde_json::to_value(&r).unwrap()["fallback_credit_token"],
            json!({"token": "fct_01", "mode": "best_effort"})
        );
        round_trip(&r);
        // The mode-less object is the reference's "equals the bare string"
        // form; it stays an object on the wire.
        let r = MessagesRequest::new("m", 1, vec![Message::user("hi")]).with_fallback_credit_token(
            FallbackCreditToken::WithMode {
                token: "fct_01".into(),
                mode: None,
            },
        );
        assert_eq!(
            serde_json::to_value(&r).unwrap()["fallback_credit_token"],
            json!({"token": "fct_01"})
        );
        round_trip(&r);
    }

    #[test]
    fn tools_match_spec_shape() {
        // /docs/en/build-with-claude/handling-stop-reasons § tool_use — the
        // get_weather tool.
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
            )
            .into(),
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
    fn server_tool_splits_version_and_round_trips() {
        // web_search_20250305 -> family "web_search" + version "20250305";
        // config (max_uses) preserved.
        let raw = json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 5});
        let td: ToolDef = serde_json::from_value(raw.clone()).unwrap();
        match &td {
            ToolDef::Server(s) => {
                assert_eq!(s.family, "web_search");
                assert_eq!(s.version.as_deref(), Some("20250305"));
                assert_eq!(s.name.as_deref(), Some("web_search"));
                assert_eq!(s.config["max_uses"].as_value(), &json!(5));
                assert_eq!(s.wire_type(), "web_search_20250305");
            }
            other => panic!("expected Server, got {other:?}"),
        }
        assert_eq!(
            serde_json::to_value(&td).unwrap(),
            raw,
            "round-trips verbatim"
        );
    }

    #[test]
    fn mcp_toolset_is_a_server_tool_with_no_version() {
        // mcp_toolset has no _NNNN date, so version=None and mcp_server_name
        // rides in config — no special variant needed.
        let raw = json!({"type": "mcp_toolset", "mcp_server_name": "example-mcp"});
        let td: ToolDef = serde_json::from_value(raw.clone()).unwrap();
        match &td {
            ToolDef::Server(s) => {
                assert_eq!(s.family, "mcp_toolset");
                assert_eq!(s.version, None);
                assert_eq!(
                    s.config["mcp_server_name"].as_value(),
                    &json!("example-mcp")
                );
            }
            other => panic!("expected Server, got {other:?}"),
        }
        assert_eq!(serde_json::to_value(&td).unwrap(), raw);
    }

    #[test]
    fn custom_tool_typeless_and_explicit_both_parse_to_custom() {
        let schema = json!({"type": "object", "properties": {}});
        // type-less
        let tl: ToolDef = serde_json::from_value(
            json!({"name": "t", "description": "d", "input_schema": schema}),
        )
        .unwrap();
        assert!(matches!(tl, ToolDef::Custom(_)));
        // explicit "type":"custom" normalizes to the same Custom (type-less out)
        let ex: ToolDef = serde_json::from_value(
            json!({"type": "custom", "name": "t", "description": "d", "input_schema": schema}),
        )
        .unwrap();
        assert_eq!(tl, ex, "explicit custom normalizes to type-less custom");
        assert_eq!(
            serde_json::to_value(&ex).unwrap(),
            json!({"name": "t", "description": "d", "input_schema": {"type": "object", "properties": {}}})
        );
    }

    #[test]
    fn malformed_custom_tool_errors_not_silently_passed() {
        // type-less but missing input_schema => a broken custom tool => error
        // (NOT a silent Unknown/Server passthrough).
        let r: Result<ToolDef, _> = serde_json::from_value(json!({"name": "t"}));
        assert!(r.is_err(), "malformed custom tool must error, got {r:?}");
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
            .with_thinking(ThinkingConfig::adaptive())
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
    fn mcp_servers_match_spec_shape_and_round_trip() {
        // spec MCP connector: {"type":"url","url":..,"name":..,
        // "authorization_token"?:..,"tool_configuration"?:{enabled,allowed_tools}}.
        let bare = MessagesRequest::new("m", 10, vec![Message::user("hi")]).with_mcp_servers(vec![
            McpServer::url("example-mcp", "https://example.modelcontextprotocol.io/sse"),
        ]);
        assert_eq!(
            serde_json::to_value(&bare).unwrap()["mcp_servers"],
            json!([{
                "type": "url",
                "url": "https://example.modelcontextprotocol.io/sse",
                "name": "example-mcp"
            }])
        );
        round_trip(&bare);

        // full server with auth + tool_configuration
        let full = MessagesRequest::new("m", 10, vec![Message::user("hi")]).with_mcp_servers(vec![
            McpServer {
                kind: "url".into(),
                url: "https://mcp.example.com/sse".into(),
                name: "mcp-1".into(),
                authorization_token: Some("TOK".into()),
                tool_configuration: Some(McpToolConfiguration {
                    enabled: Some(true),
                    allowed_tools: vec!["tool1".into(), "tool2".into()],
                }),
                extra: BTreeMap::new(),
            },
        ]);
        assert_eq!(
            serde_json::to_value(&full).unwrap()["mcp_servers"][0],
            json!({
                "type": "url", "url": "https://mcp.example.com/sse", "name": "mcp-1",
                "authorization_token": "TOK",
                "tool_configuration": {"enabled": true, "allowed_tools": ["tool1", "tool2"]}
            })
        );
        round_trip(&full);
    }

    #[test]
    fn service_tier_and_container_serialize_when_set() {
        let r = MessagesRequest::new("m", 10, vec![Message::user("hi")])
            .with_service_tier("standard_only")
            .with_container("cntr_abc");
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["service_tier"], json!("standard_only"));
        assert_eq!(v["container"], json!("cntr_abc"));
        round_trip(&r);
    }

    #[test]
    fn thinking_enabled_carries_budget_and_round_trips() {
        let req = MessagesRequest::new("m", 10, vec![Message::user("hi")])
            .with_thinking(ThinkingConfig::enabled(4096));
        assert_eq!(
            serde_json::to_value(&req).unwrap()["thinking"],
            json!({"type": "enabled", "budget_tokens": 4096})
        );
        round_trip(&req);
    }

    #[test]
    fn thinking_display_matches_the_documented_shapes() {
        let thinking = |t: ThinkingConfig| {
            serde_json::to_value(
                MessagesRequest::new("m", 16000, vec![Message::user("hi")]).with_thinking(t),
            )
            .unwrap()["thinking"]
                .clone()
        };
        // /docs/en/build-with-claude/thinking § Streaming thinking — the
        // documented request, and the shape mu's lane sends today.
        assert_eq!(
            thinking(ThinkingConfig::adaptive().with_display(ThinkingDisplay::Summarized)),
            json!({"type": "adaptive", "display": "summarized"})
        );
        // § Progress updates between tool calls — display "updates" (beta).
        assert_eq!(
            thinking(ThinkingConfig::adaptive().with_display(ThinkingDisplay::Updates)),
            json!({"type": "adaptive", "display": "updates"})
        );
        // `enabled` carries the knob too.
        assert_eq!(
            thinking(ThinkingConfig::enabled(4096).with_display(ThinkingDisplay::Omitted)),
            json!({"type": "enabled", "budget_tokens": 4096, "display": "omitted"})
        );
        // `disabled` has nowhere to put it: the setter leaves the config
        // alone and the bytes stay the documented {"type":"disabled"}.
        assert_eq!(
            thinking(ThinkingConfig::Disabled.with_display(ThinkingDisplay::Summarized)),
            json!({"type": "disabled"})
        );
        // Closed value set: an undocumented display is a hard error, and a
        // config that carries display + binding round-trips.
        assert!(serde_json::from_value::<ThinkingConfig>(
            json!({"type": "adaptive", "display": "raw"})
        )
        .is_err());
        round_trip(
            &MessagesRequest::new("m", 1, vec![Message::user("hi")]).with_thinking(
                ThinkingConfig::enabled(2048)
                    .with_display(ThinkingDisplay::Updates)
                    .with_prefix_mismatch_behavior(PrefixMismatchBehavior::Error),
            ),
        );
    }

    #[test]
    fn thinking_block_binding_matches_the_documented_request() {
        // /docs/en/build-with-claude/preserved-thinking § Set the mismatch
        // behavior and read input_transformations — the documented request
        // that opts into dropping rather than rejecting.
        let req = MessagesRequest::new(
            "claude-fable-5-1",
            16000,
            vec![Message::user(
                "What is the greatest common divisor of 1071 and 462?",
            )],
        )
        .with_thinking(
            ThinkingConfig::adaptive()
                .with_prefix_mismatch_behavior(PrefixMismatchBehavior::DropBlock),
        );
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            json!({
                "model": "claude-fable-5-1",
                "max_tokens": 16000,
                "thinking": {
                    "type": "adaptive",
                    "block_binding": {"prefix_mismatch_behavior": "drop_block"}
                },
                "messages": [
                    {"role": "user", "content": "What is the greatest common divisor of 1071 and 462?"}
                ]
            })
        );
        round_trip(&req);
        assert_eq!(
            serde_json::to_value(PrefixMismatchBehavior::Error).unwrap(),
            json!("error")
        );
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
