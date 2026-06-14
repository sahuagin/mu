//! Message — a role + its content, the element of a request's `messages` array
//! and the shape of a non-streaming assistant response's body.
//!
//! Wire fact that this type exists to model correctly (verified against
//! `specifications/llms-full.txt.xz`, 2026-06-13): `content` is POLYMORPHIC —
//! it is EITHER a bare string OR an array of [`ContentBlock`]s:
//!
//! - `{"role":"user","content":"Hello, Claude"}`            (string, :988)
//! - `{"role":"assistant","content":[{"type":"text",...}]}` (blocks, :92)
//!
//! The legacy mu emitter hand-assembled this with `serde_json::json!` and in
//! places assumed a single shape; the polymorphism is the latent
//! "only one element / only one form" bug. Here it is a typed enum
//! ([`Content`]) — the wrong shape is unrepresentable.
//!
//! NOTE on roles: on the Messages API the per-message `role` is only `user` or
//! `assistant`. `system` is an ENVELOPE field (top-level `system`), not a
//! message role. mu's internal `ProviderRole` additionally has `System` and
//! `ToolResult`; mapping those onto this two-role wire shape (system → envelope,
//! tool_result → a `user` message of `tool_result` blocks) is the mu-side
//! `From`'s job, not this crate's. See INTEGRATION.md.

use serde::{Deserialize, Serialize};

use crate::content::ContentBlock;

/// Wire role for a message in the `messages` array. Only two values exist on
/// the Messages API; `system` is a top-level envelope field, not a role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

/// A message's `content`: either a bare string or an array of typed blocks.
/// Untagged so it serializes to exactly the wire form (no wrapper) and
/// deserializes by trying string first, then the block array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl Content {
    /// Borrow the blocks if this is the array form; `None` for the string form.
    pub fn as_blocks(&self) -> Option<&[ContentBlock]> {
        match self {
            Content::Blocks(b) => Some(b),
            Content::Text(_) => None,
        }
    }
}

impl From<&str> for Content {
    fn from(s: &str) -> Self {
        Content::Text(s.to_owned())
    }
}

impl From<Vec<ContentBlock>> for Content {
    fn from(b: Vec<ContentBlock>) -> Self {
        Content::Blocks(b)
    }
}

/// One message in the `messages` array (request side) and the core of a
/// non-streaming assistant response body. Response-only envelope fields
/// (`id`, `model`, `stop_reason`, `usage`, …) live on the response `Message`
/// type in a later slice; this is the request/role+content core.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Content,
}

impl Message {
    pub fn user(content: impl Into<Content>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<Content>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ContentBlock;
    use serde_json::json;

    fn round_trip(m: &Message) {
        let s = serde_json::to_string(m).unwrap();
        let back: Message = serde_json::from_str(&s).unwrap();
        assert_eq!(m, &back, "round-trip mismatch via {s}");
    }

    // ----- the polymorphic content forms both work -----

    #[test]
    fn string_content_matches_spec_shape() {
        // spec :988 — {"role":"user","content":"Hello, Claude"}.
        let m = Message::user("Hello, Claude");
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            json!({"role": "user", "content": "Hello, Claude"})
        );
        round_trip(&m);
    }

    #[test]
    fn block_array_content_matches_spec_shape() {
        // spec :92 — assistant message with a content block array.
        let m = Message::assistant(vec![ContentBlock::text("Here are some strategies")]);
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            json!({
                "role": "assistant",
                "content": [{"type": "text", "text": "Here are some strategies"}]
            })
        );
        round_trip(&m);
    }

    #[test]
    fn string_form_deserializes_from_bare_string() {
        let m: Message = serde_json::from_value(json!({"role": "user", "content": "hi"})).unwrap();
        assert_eq!(m.content, Content::Text("hi".into()));
        assert!(m.content.as_blocks().is_none());
    }

    // ----- the latent bug: MULTIPLE blocks in one message -----

    #[test]
    fn multi_block_content_is_representable_and_ordered() {
        // The legacy "only one element" assumption breaks here. A single
        // assistant message can carry text + a tool_use block together, and
        // their ORDER is significant on the wire.
        let m = Message::assistant(vec![
            ContentBlock::text("Let me calculate that."),
            ContentBlock::ToolUse {
                id: "toolu_123".into(),
                name: "calculator".into(),
                input: json!({"a": 1, "b": 2}),
                cache_control: None,
            },
        ]);
        let v = serde_json::to_value(&m).unwrap();
        let blocks = v["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2, "both blocks present");
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "tool_use");
        round_trip(&m);
    }

    #[test]
    fn tool_result_message_batches_multiple_results() {
        // Anthropic's protocol: consecutive tool results go in ONE user
        // message as multiple tool_result blocks. Modeled cleanly here.
        let m = Message::user(vec![
            ContentBlock::ToolResult {
                tool_use_id: "toolu_1".into(),
                content: "a".into(),
                is_error: None,
                cache_control: None,
            },
            ContentBlock::ToolResult {
                tool_use_id: "toolu_2".into(),
                content: "b".into(),
                is_error: None,
                cache_control: None,
            },
        ]);
        let blocks = serde_json::to_value(&m).unwrap()["content"]
            .as_array()
            .unwrap()
            .len();
        assert_eq!(blocks, 2);
        round_trip(&m);
    }

    #[test]
    fn role_serializes_lowercase() {
        assert_eq!(serde_json::to_value(Role::User).unwrap(), json!("user"));
        assert_eq!(
            serde_json::to_value(Role::Assistant).unwrap(),
            json!("assistant")
        );
    }
}
