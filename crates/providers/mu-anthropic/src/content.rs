//! Content blocks — the leaf of the Anthropic wire object graph.
//!
//! A message's `content` is an array of these. The wire shape is an
//! internally-tagged union keyed on `"type"`. Spec-exact field names (verified
//! against `specifications/llms-full.txt.xz`, 2026-06-13):
//!
//! - text:        `{"type":"text","text":"..."}`
//! - tool_use:    `{"type":"tool_use","id":"toolu_...","name":"...","input":{...}}`
//! - tool_result: `{"type":"tool_result","tool_use_id":"toolu_...","content":"..."}`
//! - thinking:    `{"type":"thinking","thinking":"...","signature":"..."}`
//!
//! Any block may carry `cache_control` (per-BLOCK on the wire — confirmed by
//! the spec's request-idempotency table and the legacy mu emitter). `thinking`
//! is modeled for INBOUND fidelity; mu strips it outbound (it must never echo
//! the model's reasoning back as input — see INTEGRATION.md scar list).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::json::JsonValue;

/// Anthropic prompt-cache directive attached to a content block (or to an
/// envelope position — system block / last tool spec). Wire shapes:
/// `{"type":"ephemeral"}` and `{"type":"ephemeral","ttl":"1h"}`.
///
/// `ttl` is omitted entirely when `None` (bare ephemeral = the default 5-minute
/// tier; wire-identical to pre-TTL emission). Modeled as a string so future
/// tiers don't require a code change to deserialize.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub kind: CacheKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
}

impl CacheControl {
    /// Bare ephemeral (default 5-minute tier): `{"type":"ephemeral"}`.
    pub fn ephemeral() -> Self {
        Self {
            kind: CacheKind::Ephemeral,
            ttl: None,
        }
    }

    /// Ephemeral with an explicit TTL tier, e.g. `ephemeral_with_ttl("1h")`
    /// → `{"type":"ephemeral","ttl":"1h"}`.
    pub fn ephemeral_with_ttl(ttl: impl Into<String>) -> Self {
        Self {
            kind: CacheKind::Ephemeral,
            ttl: Some(ttl.into()),
        }
    }
}

/// The `type` of a [`CacheControl`]. Only `ephemeral` exists today; an unknown
/// value deserializes to [`CacheKind::Other`] rather than erroring
/// (forward-compat).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheKind {
    Ephemeral,
    #[serde(other)]
    Other,
}

/// One content block. Internally tagged on `"type"`.
///
/// `Unknown` is the forward-compat fallback: any block whose `type` we do not
/// model (e.g. the server-tool result family, `search_result`, a future
/// addition) deserializes here instead of failing the whole message. It
/// preserves the raw JSON so a consumer can still inspect or round-trip it.
///
/// IMPORTANT: `Unknown` is reached ONLY when the `type` tag itself is one we
/// do not model. A KNOWN `type` with missing/invalid fields is a hard error —
/// it does NOT silently degrade to `Unknown` (that would swallow real wire
/// breakage, e.g. a renamed required field). This is enforced by a hand-written
/// `Deserialize` that dispatches on `type` first. See [`KnownContentBlock`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentBlock {
    Text {
        text: String,
        /// Source citations the model attached to this text (web search / RAG /
        /// documents). Shapes vary per citation kind (`web_search_result_location`,
        /// `char_location`, `page_location`, …), so they're preserved verbatim as
        /// JSON. Empty when absent. (Found by the drift canary on real web-search
        /// traffic — text blocks carry `citations`.)
        citations: Vec<JsonValue>,
        cache_control: Option<CacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: JsonValue,
        cache_control: Option<CacheControl>,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: Option<bool>,
        cache_control: Option<CacheControl>,
    },
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    /// Encrypted reasoning the API does not return in cleartext. Wire:
    /// `{"type":"redacted_thinking","data":"..."}`.
    RedactedThinking { data: String },
    /// A server-executed tool invocation (e.g. `web_search`). Shape mirrors
    /// `tool_use`: `{"type":"server_tool_use","id":"srvtoolu_...","name":...,
    /// "input":{...}}`. (The matching server *result* family —
    /// `web_search_tool_result` et al. — is not yet modeled; those still land in
    /// `Unknown`.)
    ServerToolUse {
        id: String,
        name: String,
        input: JsonValue,
        cache_control: Option<CacheControl>,
    },
    /// Marks a point in `content` where one model handed off to another
    /// (server-side fallback): `{"type":"fallback","from":{"model":...},
    /// "to":{"model":...}}`.
    Fallback {
        from: FallbackModel,
        to: FallbackModel,
    },
    /// An image input block: `{"type":"image","source":{...}}`. `source` is
    /// polymorphic (base64 / url / file — see [`Source`]).
    Image {
        source: Source,
        cache_control: Option<CacheControl>,
    },
    /// A document input block: `{"type":"document","source":{...},"title"?,
    /// "context"?}`. `source` adds a `text` kind for plain-text docs.
    ///
    /// NOTE: `citations` config and the `content`-kind source are NOT yet
    /// modeled — a document carrying them would lose those fields on round-trip.
    /// Explicitly deferred (not silently handled); model when real traffic shows
    /// them.
    Document {
        source: Source,
        title: Option<String>,
        context: Option<String>,
        cache_control: Option<CacheControl>,
    },
    /// A server-tool RESULT block — the inline result of a `server_tool_use`,
    /// e.g. `web_search_tool_result`, `bash_code_execution_tool_result`,
    /// `web_fetch_tool_result`, `mcp_tool_result`. Modeled GENERICALLY (verified
    /// against real opus-4-8 traffic): the exact wire tag is kept in
    /// `result_type`, and `content` — whose shape varies per family (an ARRAY of
    /// results for web_search, a single result OBJECT for code-exec, an error
    /// object on failure) — is preserved verbatim as JSON. Dispatched by the
    /// `_tool_result` type suffix. Any other per-block field (e.g. web_search's
    /// `caller`) round-trips via `extra`.
    ServerToolResult {
        result_type: String,
        tool_use_id: String,
        content: JsonValue,
        extra: BTreeMap<String, JsonValue>,
    },
    /// Forward-compat fallback: the `type` tag is one we do not model
    /// (e.g. `connector_text`, `search_result`, a future addition). Captures the
    /// whole object verbatim so a consumer can inspect or round-trip it. Reached
    /// ONLY for unknown tags, never for a known tag with broken fields.
    Unknown(JsonValue),
}

/// One endpoint of a [`ContentBlock::Fallback`] marker — the model on either
/// side of the handoff. Wire: `{"model":"..."}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FallbackModel {
    pub model: String,
}

/// The media `source` of an [`ContentBlock::Image`] / [`ContentBlock::Document`].
/// Internally tagged on `type`; covers the documented source kinds. These are
/// outbound (mu constructs them), so the set is closed — an unmodeled source
/// `type` errors loudly rather than mis-modeling. (`base64` and `text` share a
/// `{media_type, data}` shape but differ by tag: base64 bytes vs plain text.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Source {
    /// `{"type":"base64","media_type":"image/png"|"application/pdf",...,"data":...}`.
    Base64 { media_type: String, data: String },
    /// `{"type":"url","url":"https://..."}`.
    Url { url: String },
    /// `{"type":"text","media_type":"text/plain","data":...}` (plain-text doc).
    Text { media_type: String, data: String },
    /// `{"type":"file","file_id":"..."}` (Files API reference).
    File { file_id: String },
}

/// The set of `type` tags we model, used to decide known-vs-unknown dispatch.
/// The actual field shapes live in [`ContentBlock`]; this mirror exists only so
/// serde's derive can do the strict per-variant field validation, while the
/// hand-written `Deserialize` on `ContentBlock` routes unknown tags to
/// `Unknown` instead of erroring.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum KnownContentBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        citations: Vec<JsonValue>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: JsonValue,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
    ServerToolUse {
        id: String,
        name: String,
        input: JsonValue,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Fallback {
        from: FallbackModel,
        to: FallbackModel,
    },
    Image {
        source: Source,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Document {
        source: Source,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}

const KNOWN_TAGS: &[&str] = &[
    "text",
    "tool_use",
    "tool_result",
    "thinking",
    "redacted_thinking",
    "server_tool_use",
    "fallback",
    "image",
    "document",
];

impl From<KnownContentBlock> for ContentBlock {
    fn from(k: KnownContentBlock) -> Self {
        match k {
            KnownContentBlock::Text {
                text,
                citations,
                cache_control,
            } => ContentBlock::Text {
                text,
                citations,
                cache_control,
            },
            KnownContentBlock::ToolUse {
                id,
                name,
                input,
                cache_control,
            } => ContentBlock::ToolUse {
                id,
                name,
                input,
                cache_control,
            },
            KnownContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                cache_control,
            } => ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                cache_control,
            },
            KnownContentBlock::Thinking {
                thinking,
                signature,
            } => ContentBlock::Thinking {
                thinking,
                signature,
            },
            KnownContentBlock::RedactedThinking { data } => ContentBlock::RedactedThinking { data },
            KnownContentBlock::ServerToolUse {
                id,
                name,
                input,
                cache_control,
            } => ContentBlock::ServerToolUse {
                id,
                name,
                input,
                cache_control,
            },
            KnownContentBlock::Fallback { from, to } => ContentBlock::Fallback { from, to },
            KnownContentBlock::Image {
                source,
                cache_control,
            } => ContentBlock::Image {
                source,
                cache_control,
            },
            KnownContentBlock::Document {
                source,
                title,
                context,
                cache_control,
            } => ContentBlock::Document {
                source,
                title,
                context,
                cache_control,
            },
        }
    }
}

impl<'de> Deserialize<'de> for ContentBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Buffer the object, peek at `type`. Known tag → strict per-variant
        // parse (errors propagate). Unknown/absent tag → Unknown(raw).
        let raw = Value::deserialize(deserializer)?;
        let tag = raw.get("type").and_then(Value::as_str);
        match tag {
            Some(t) if KNOWN_TAGS.contains(&t) => {
                let known: KnownContentBlock =
                    serde_json::from_value(raw).map_err(serde::de::Error::custom)?;
                Ok(known.into())
            }
            // The server-tool result family (web_search_tool_result,
            // bash_code_execution_tool_result, mcp_tool_result, …): modeled
            // generically by the `_tool_result` suffix. Requires the canonical
            // {tool_use_id, content} pair; a malformed one falls back to Unknown.
            Some(t) if t.ends_with("_tool_result") => {
                let parsed = raw.as_object().and_then(|o| {
                    let id = o.get("tool_use_id")?.as_str()?.to_string();
                    let content = o.get("content")?.clone();
                    let extra: serde_json::Map<String, Value> = o
                        .iter()
                        .filter(|(k, _)| !matches!(k.as_str(), "type" | "tool_use_id" | "content"))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    Some((id, content, extra))
                });
                match parsed {
                    Some((tool_use_id, content, extra_map)) => {
                        let mut extra = BTreeMap::new();
                        for (k, v) in extra_map {
                            extra.insert(k, JsonValue::new(v).map_err(serde::de::Error::custom)?);
                        }
                        Ok(ContentBlock::ServerToolResult {
                            result_type: t.to_string(),
                            tool_use_id,
                            content: JsonValue::new(content).map_err(serde::de::Error::custom)?,
                            extra,
                        })
                    }
                    None => Ok(ContentBlock::Unknown(
                        JsonValue::new(raw).map_err(serde::de::Error::custom)?,
                    )),
                }
            }
            _ => Ok(ContentBlock::Unknown(
                JsonValue::new(raw).map_err(serde::de::Error::custom)?,
            )),
        }
    }
}

impl Serialize for ContentBlock {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            ContentBlock::Unknown(v) => v.serialize(serializer),
            ContentBlock::ServerToolResult {
                result_type,
                tool_use_id,
                content,
                extra,
            } => {
                let mut map = serde_json::Map::new();
                map.insert("type".into(), Value::String(result_type.clone()));
                map.insert("tool_use_id".into(), Value::String(tool_use_id.clone()));
                map.insert("content".into(), content.as_value().clone());
                for (k, v) in extra {
                    map.insert(k.clone(), v.as_value().clone());
                }
                Value::Object(map).serialize(serializer)
            }
            other => {
                // Re-express modeled variants via the derived KnownContentBlock
                // so the wire shape (tag + skip_serializing_if) stays identical.
                let known = match other.clone() {
                    ContentBlock::Text {
                        text,
                        citations,
                        cache_control,
                    } => KnownContentBlock::Text {
                        text,
                        citations,
                        cache_control,
                    },
                    ContentBlock::ToolUse {
                        id,
                        name,
                        input,
                        cache_control,
                    } => KnownContentBlock::ToolUse {
                        id,
                        name,
                        input,
                        cache_control,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        cache_control,
                    } => KnownContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        cache_control,
                    },
                    ContentBlock::Thinking {
                        thinking,
                        signature,
                    } => KnownContentBlock::Thinking {
                        thinking,
                        signature,
                    },
                    ContentBlock::RedactedThinking { data } => {
                        KnownContentBlock::RedactedThinking { data }
                    }
                    ContentBlock::ServerToolUse {
                        id,
                        name,
                        input,
                        cache_control,
                    } => KnownContentBlock::ServerToolUse {
                        id,
                        name,
                        input,
                        cache_control,
                    },
                    ContentBlock::Fallback { from, to } => KnownContentBlock::Fallback { from, to },
                    ContentBlock::Image {
                        source,
                        cache_control,
                    } => KnownContentBlock::Image {
                        source,
                        cache_control,
                    },
                    ContentBlock::Document {
                        source,
                        title,
                        context,
                        cache_control,
                    } => KnownContentBlock::Document {
                        source,
                        title,
                        context,
                        cache_control,
                    },
                    ContentBlock::Unknown(_) | ContentBlock::ServerToolResult { .. } => {
                        unreachable!("handled above")
                    }
                };
                known.serialize(serializer)
            }
        }
    }
}

impl ContentBlock {
    /// Convenience: a plain text block, no cache directive.
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text {
            text: text.into(),
            citations: Vec::new(),
            cache_control: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ----- Tier 1: round-trip (de(ser(x)) == x) -----

    fn round_trip(block: &ContentBlock) {
        let s = serde_json::to_string(block).expect("serialize");
        let back: ContentBlock = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(block, &back, "round-trip mismatch via {s}");
    }

    #[test]
    fn round_trips_every_modeled_variant() {
        round_trip(&ContentBlock::text("hello"));
        round_trip(&ContentBlock::ToolUse {
            id: "toolu_123".into(),
            name: "calculator".into(),
            input: JsonValue::new(json!({"operation": "add", "a": 1, "b": 2})).unwrap(),
            cache_control: None,
        });
        round_trip(&ContentBlock::ToolResult {
            tool_use_id: "toolu_123".into(),
            content: "6912".into(),
            is_error: None,
            cache_control: None,
        });
        round_trip(&ContentBlock::Thinking {
            thinking: "let me reason".into(),
            signature: Some("sig".into()),
        });
    }

    #[test]
    fn redacted_thinking_server_tool_use_and_fallback_round_trip() {
        round_trip(&ContentBlock::RedactedThinking {
            data: "EncRypTed==".into(),
        });
        round_trip(&ContentBlock::ServerToolUse {
            id: "srvtoolu_1".into(),
            name: "web_search".into(),
            input: JsonValue::new(json!({"query": "claude shannon"})).unwrap(),
            cache_control: None,
        });
        round_trip(&ContentBlock::Fallback {
            from: FallbackModel {
                model: "claude-fable-5".into(),
            },
            to: FallbackModel {
                model: "claude-opus-4-8".into(),
            },
        });
    }

    #[test]
    fn new_block_types_match_spec_shape_and_are_not_unknown() {
        // redacted_thinking: {"type":"redacted_thinking","data":"..."}
        let rt = ContentBlock::RedactedThinking { data: "abc".into() };
        assert_eq!(
            serde_json::to_value(&rt).unwrap(),
            json!({"type": "redacted_thinking", "data": "abc"})
        );
        // server_tool_use mirrors tool_use
        let stu = ContentBlock::ServerToolUse {
            id: "srvtoolu_abc123".into(),
            name: "web_search".into(),
            input: JsonValue::new(json!({"query": "x"})).unwrap(),
            cache_control: None,
        };
        assert_eq!(
            serde_json::to_value(&stu).unwrap(),
            json!({"type": "server_tool_use", "id": "srvtoolu_abc123",
                   "name": "web_search", "input": {"query": "x"}})
        );
        // fallback: {"type":"fallback","from":{"model":..},"to":{"model":..}}
        let fb = ContentBlock::Fallback {
            from: FallbackModel {
                model: "claude-fable-5".into(),
            },
            to: FallbackModel {
                model: "claude-opus-4-8".into(),
            },
        };
        assert_eq!(
            serde_json::to_value(&fb).unwrap(),
            json!({"type": "fallback", "from": {"model": "claude-fable-5"},
                   "to": {"model": "claude-opus-4-8"}})
        );

        // And each now deserializes to its MODELLED variant, not Unknown.
        for raw in [
            json!({"type": "redacted_thinking", "data": "abc"}),
            json!({"type": "server_tool_use", "id": "s", "name": "n", "input": {}}),
            json!({"type": "fallback", "from": {"model": "a"}, "to": {"model": "b"}}),
        ] {
            let b: ContentBlock = serde_json::from_value(raw.clone()).unwrap();
            assert!(
                !matches!(b, ContentBlock::Unknown(_)),
                "{raw} should be modelled, not Unknown"
            );
        }
    }

    #[test]
    fn image_block_sources_match_spec_and_round_trip() {
        // base64 source (spec: image bytes)
        let b64 = ContentBlock::Image {
            source: Source::Base64 {
                media_type: "image/png".into(),
                data: "iVBOR==".into(),
            },
            cache_control: None,
        };
        assert_eq!(
            serde_json::to_value(&b64).unwrap(),
            json!({"type": "image", "source": {"type": "base64",
                   "media_type": "image/png", "data": "iVBOR=="}})
        );
        round_trip(&b64);

        // url + file sources
        round_trip(&ContentBlock::Image {
            source: Source::Url {
                url: "https://example.com/a.jpg".into(),
            },
            cache_control: Some(CacheControl::ephemeral()),
        });
        round_trip(&ContentBlock::Image {
            source: Source::File {
                file_id: "file_123".into(),
            },
            cache_control: None,
        });
    }

    #[test]
    fn document_block_matches_spec_and_round_trips() {
        // spec: plain-text document with title + context.
        let doc = ContentBlock::Document {
            source: Source::Text {
                media_type: "text/plain".into(),
                data: "The grass is green.".into(),
            },
            title: Some("My Document".into()),
            context: Some("This is a trustworthy document.".into()),
            cache_control: None,
        };
        assert_eq!(
            serde_json::to_value(&doc).unwrap(),
            json!({
                "type": "document",
                "source": {"type": "text", "media_type": "text/plain",
                           "data": "The grass is green."},
                "title": "My Document",
                "context": "This is a trustworthy document."
            })
        );
        round_trip(&doc);
        // PDF document via base64
        round_trip(&ContentBlock::Document {
            source: Source::Base64 {
                media_type: "application/pdf".into(),
                data: "JVBERi0=".into(),
            },
            title: None,
            context: None,
            cache_control: None,
        });
    }

    #[test]
    fn image_and_document_are_not_unknown() {
        for raw in [
            json!({"type": "image", "source": {"type": "url", "url": "https://x"}}),
            json!({"type": "document", "source": {"type": "file", "file_id": "f"}}),
        ] {
            let b: ContentBlock = serde_json::from_value(raw.clone()).unwrap();
            assert!(!matches!(b, ContentBlock::Unknown(_)), "{raw} modelled");
        }
    }

    #[test]
    fn server_tool_result_family_is_modeled_generically() {
        // Shapes verified against real opus-4-8 traffic (2026-06-14).
        // web_search_tool_result: content is an ARRAY of result items, plus a
        // `caller` field (preserved via extra).
        let ws = json!({
            "type": "web_search_tool_result",
            "tool_use_id": "srvtoolu_1",
            "content": [{"type": "web_search_result", "title": "T", "url": "https://x",
                         "encrypted_content": "ENC", "page_age": null}],
            "caller": {"type": "direct"}
        });
        let b: ContentBlock = serde_json::from_value(ws.clone()).unwrap();
        match &b {
            ContentBlock::ServerToolResult {
                result_type,
                tool_use_id,
                content,
                extra,
            } => {
                assert_eq!(result_type, "web_search_tool_result");
                assert_eq!(tool_use_id, "srvtoolu_1");
                assert!(
                    content.as_value().is_array(),
                    "web_search content is an array"
                );
                assert_eq!(extra["caller"].as_value(), &json!({"type": "direct"}));
            }
            other => panic!("expected ServerToolResult, got {other:?}"),
        }
        assert_eq!(
            serde_json::to_value(&b).unwrap(),
            ws,
            "round-trips verbatim"
        );

        // bash_code_execution_tool_result: content is a single OBJECT.
        let ce = json!({
            "type": "bash_code_execution_tool_result",
            "tool_use_id": "srvtoolu_2",
            "content": {"type": "bash_code_execution_result", "stdout": "ok\n",
                        "stderr": "", "return_code": 0, "content": []}
        });
        let b2: ContentBlock = serde_json::from_value(ce.clone()).unwrap();
        match &b2 {
            ContentBlock::ServerToolResult { content, .. } => {
                assert!(
                    content.as_value().is_object(),
                    "code-exec content is an object"
                )
            }
            other => panic!("expected ServerToolResult, got {other:?}"),
        }
        assert_eq!(
            serde_json::to_value(&b2).unwrap(),
            ce,
            "object-content round-trips"
        );
    }

    #[test]
    fn malformed_tool_result_without_ids_degrades_to_unknown() {
        // A `_tool_result` lacking the canonical {tool_use_id, content} can't be
        // modeled generically, so it's preserved via Unknown rather than erroring.
        let raw = json!({"type": "weird_tool_result", "foo": 1});
        let b: ContentBlock = serde_json::from_value(raw.clone()).unwrap();
        assert!(matches!(b, ContentBlock::Unknown(_)));
        assert_eq!(serde_json::to_value(&b).unwrap(), raw);
    }

    // ----- Tier 2: spec-conformance (our bytes == documented example) -----
    // Examples lifted verbatim from specifications/llms-full.txt.xz (2026-06-13).

    #[test]
    fn text_matches_spec_shape() {
        let block = ContentBlock::text("Here's the result");
        let v = serde_json::to_value(&block).unwrap();
        assert_eq!(v, json!({"type": "text", "text": "Here's the result"}));
    }

    #[test]
    fn tool_use_matches_spec_shape() {
        // spec llms-full.txt.xz:2660 — calculator example.
        let block = ContentBlock::ToolUse {
            id: "toolu_123".into(),
            name: "calculator".into(),
            input: JsonValue::new(json!({"operation": "add", "a": 1234, "b": 5678})).unwrap(),
            cache_control: None,
        };
        let v = serde_json::to_value(&block).unwrap();
        assert_eq!(
            v,
            json!({
                "type": "tool_use",
                "id": "toolu_123",
                "name": "calculator",
                "input": {"operation": "add", "a": 1234, "b": 5678}
            })
        );
    }

    #[test]
    fn tool_result_uses_tool_use_id_not_id() {
        // SCAR-ADJACENT: the field is `tool_use_id`, NOT `id`. spec :2670.
        let block = ContentBlock::ToolResult {
            tool_use_id: "toolu_123".into(),
            content: "6912".into(),
            is_error: None,
            cache_control: None,
        };
        let v = serde_json::to_value(&block).unwrap();
        assert_eq!(
            v,
            json!({"type": "tool_result", "tool_use_id": "toolu_123", "content": "6912"})
        );
        // and it must NOT emit a bare `id`.
        assert!(v.get("id").is_none(), "tool_result must not carry `id`");
    }

    #[test]
    fn tool_result_is_error_serializes_when_set_and_round_trips() {
        // An errored tool result carries `"is_error": true`; the field is
        // omitted (never null) when absent. spec: tool_result error reporting.
        let errored = ContentBlock::ToolResult {
            tool_use_id: "toolu_9".into(),
            content: "boom".into(),
            is_error: Some(true),
            cache_control: None,
        };
        let v = serde_json::to_value(&errored).unwrap();
        assert_eq!(
            v,
            json!({
                "type": "tool_result",
                "tool_use_id": "toolu_9",
                "content": "boom",
                "is_error": true
            })
        );
        round_trip(&errored);

        // absent is_error must not serialize as null
        let ok = ContentBlock::ToolResult {
            tool_use_id: "toolu_9".into(),
            content: "fine".into(),
            is_error: None,
            cache_control: None,
        };
        assert!(
            serde_json::to_value(&ok).unwrap().get("is_error").is_none(),
            "absent is_error must be omitted, not null"
        );
    }

    // ----- cache_control: per-block, omitted when absent -----

    #[test]
    fn cache_control_omitted_when_none() {
        let v = serde_json::to_value(ContentBlock::text("x")).unwrap();
        assert!(
            v.get("cache_control").is_none(),
            "absent cache_control must not serialize as null"
        );
    }

    #[test]
    fn text_block_citations_round_trip() {
        // Real web-search shape (the drift canary's first catch): text blocks
        // carry a `citations` array. Preserved verbatim; omitted when empty.
        let raw = json!({
            "type": "text",
            "text": "Rust 1.95 is the current stable.",
            "citations": [{
                "type": "web_search_result_location",
                "cited_text": "Stable: 1.95.0", "url": "https://x", "title": "T"
            }]
        });
        let b: ContentBlock = serde_json::from_value(raw.clone()).unwrap();
        match &b {
            ContentBlock::Text { citations, .. } => {
                assert_eq!(citations.len(), 1);
                assert_eq!(
                    citations[0].as_value()["type"],
                    "web_search_result_location"
                );
            }
            other => panic!("expected Text, got {other:?}"),
        }
        assert_eq!(
            serde_json::to_value(&b).unwrap(),
            raw,
            "round-trips verbatim"
        );
        // a plain text block omits citations entirely (not an empty array)
        assert!(serde_json::to_value(ContentBlock::text("hi"))
            .unwrap()
            .get("citations")
            .is_none());
    }

    #[test]
    fn cache_control_ephemeral_matches_spec() {
        // spec llms-full.txt.xz:6929 — {"type":"ephemeral"}.
        let block = ContentBlock::Text {
            text: "<book>".into(),
            citations: Vec::new(),
            cache_control: Some(CacheControl::ephemeral()),
        };
        let v = serde_json::to_value(&block).unwrap();
        assert_eq!(
            v,
            json!({
                "type": "text",
                "text": "<book>",
                "cache_control": {"type": "ephemeral"}
            })
        );
    }

    #[test]
    fn cache_control_with_ttl_matches_legacy_1h_shape() {
        // legacy mu emitter: OneHour => {"type":"ephemeral","ttl":"1h"}.
        let cc = CacheControl::ephemeral_with_ttl("1h");
        let v = serde_json::to_value(&cc).unwrap();
        assert_eq!(v, json!({"type": "ephemeral", "ttl": "1h"}));
    }

    // ----- forward-compat: unknown block types don't error -----

    #[test]
    fn unknown_block_type_deserializes_to_unknown() {
        // A `search_result` block (not yet modeled) must NOT fail deserialization.
        let raw = json!({
            "type": "search_result",
            "source": "https://x", "title": "t", "content": []
        });
        let block: ContentBlock =
            serde_json::from_value(raw.clone()).expect("unknown type must not error");
        match &block {
            ContentBlock::Unknown(v) => assert_eq!(v.as_value(), &raw),
            other => panic!("expected Unknown, got {other:?}"),
        }
        // and it round-trips byte-for-byte.
        assert_eq!(serde_json::to_value(&block).unwrap(), raw);
    }

    #[test]
    fn unknown_cache_kind_absorbs_to_other() {
        // A future cache tier must deserialize to Other, not error.
        let kind: CacheKind = serde_json::from_value(json!("some_future_tier")).unwrap();
        assert_eq!(kind, CacheKind::Other);
        // And a whole CacheControl carrying an unknown type survives.
        let cc: CacheControl = serde_json::from_value(json!({"type": "some_future_tier"})).unwrap();
        assert_eq!(cc.kind, CacheKind::Other);
    }
}

#[cfg(test)]
mod fallback_strictness {
    use super::*;
    use serde_json::json;

    #[test]
    fn malformed_known_block_errors_not_degrades() {
        // A tool_use missing required `name`/`input` is wire BREAKAGE and must
        // be a hard error — it must NOT silently become Unknown (regression:
        // the old #[serde(untagged)] fallback swallowed this).
        let raw = json!({"type": "tool_use", "id": "toolu_1"});
        let r: Result<ContentBlock, _> = serde_json::from_value(raw);
        assert!(r.is_err(), "malformed known block must error, got {r:?}");
    }

    #[test]
    fn unknown_tag_still_degrades_to_unknown() {
        let raw = json!({"type": "search_result", "source": "s", "content": []});
        let b: ContentBlock = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(
            b,
            ContentBlock::Unknown(JsonValue::new(raw.clone()).unwrap())
        );
        assert_eq!(serde_json::to_value(&b).unwrap(), raw); // round-trips
    }

    #[test]
    fn block_with_no_type_tag_degrades_to_unknown() {
        let raw = json!({"foo": "bar"});
        let b: ContentBlock = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(b, ContentBlock::Unknown(JsonValue::new(raw).unwrap()));
    }
}
