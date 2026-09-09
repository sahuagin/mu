//! Message — a role + its content, the element of a request's `messages` array
//! and the shape of a non-streaming assistant response's body.
//!
//! Wire fact that this type exists to model correctly (verified against
//! `specifications/llms-full.txt.xz`, 2026-09-04): `content` is POLYMORPHIC —
//! it is EITHER a bare string OR an array of [`ContentBlock`]s:
//!
//! - `{"role":"user","content":"Hello, Claude"}` (string —
//!   `/docs/en/api/messages/create`, the `messages` parameter's single-message
//!   example)
//! - `{"role":"assistant","content":[{"type":"text",...}]}` (blocks —
//!   `/docs/en/get-started § Call the API`, the response body)
//!
//! The legacy mu emitter hand-assembled this with `serde_json::json!` and in
//! places assumed a single shape; the polymorphism is the latent
//! "only one element / only one form" bug. Here it is a typed enum
//! ([`Content`]) — the wrong shape is unrepresentable.
//!
//! Roles. `user` and `assistant` alternate as always. `system` INSIDE
//! `messages` is a mid-conversation system message
//! (`/docs/en/build-with-claude/mid-conversation-system-messages § How it
//! works`): an operator-level instruction appended at the point in the
//! conversation where it becomes relevant, so the cached prefix ahead of it is
//! untouched. The top-level `system` envelope field is still where
//! instructions that apply from the first turn go. Two fields exist ONLY on a
//! system message, each behind a beta header the transport sends whenever the
//! field is present:
//!
//! - `output_config` — per-message effort, `{"effort":"low"}`; the level
//!   applies from the next `user` turn on. The documented shape is an
//!   effort-only message with EMPTY content, `"content": []`
//!   (`/docs/en/build-with-claude/effort § Per-message effort (beta)`). Beta
//!   `mid-conversation-output-config-2026-07-01`.
//! - `clear_at` — turn scope ([`ClearAt`]): `next_user_message` renders the
//!   text only until a later user message exists; from then on the message
//!   stays in the array, re-sent verbatim, and renders nothing (`§ Turn-scoped
//!   system messages`). Beta `mid-conversation-system-clear-at-2026-08-21`.
//!
//! Both are omitted from the wire when unset, so a user/assistant message
//! serializes byte-for-byte as it did before they existed.
//!
//! Placement depends on what the message carries (the same page's
//! `§ Limitations`). A system message with content — `text`, or the tool
//! change blocks — "must immediately follow a `user` turn (including a `user`
//! turn that carries `tool_result` blocks) or an `assistant` turn ending in a
//! server tool result, and must precede an `assistant` turn or end the
//! array", and "consecutive `system` messages are judged together", so a run
//! of reminders after one user turn is one group (the page's own turn-scoped
//! example ends with two). An effort-only message — empty content, just
//! `output_config.effort` — "renders nothing at its position and is accepted
//! anywhere in `messages`, including first or between an `assistant` turn and
//! a `user` turn"; the effort page's example puts it exactly there, and the
//! level takes effect from the next `user` turn. A turn-scoped message is
//! text-only, with no `output_config` and no `cache_control` on its blocks.
//! The API validates all of this server-side; this crate enforces none of it,
//! and the named constructors build the three documented shapes. mu's
//! internal `ProviderRole` additionally has `ToolResult`; mapping it (a `user`
//! message of `tool_result` blocks) and choosing between the envelope `system`
//! field and a mid-conversation system message is the mu-side `From`'s job,
//! not this crate's. See INTEGRATION.md.

use serde::{Deserialize, Serialize};

use crate::content::ContentBlock;
use crate::request::OutputConfig;

/// Wire role of a message in the `messages` array. `system` here is a
/// mid-conversation system message (module docs); the top-level `system`
/// envelope field is a separate thing and unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    System,
}

/// `clear_at` on a system message — how long its text stays in front of the
/// model (`/docs/en/build-with-claude/mid-conversation-system-messages
/// § Turn-scoped system messages`). Wire values `"never"` and
/// `"next_user_message"`; an absent field means `never`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClearAt {
    /// Renders on every request that includes it — identical to omitting the
    /// field.
    Never,
    /// Turn-scoped: renders only while no later `user` message follows it (a
    /// user message of only `tool_result` blocks counts). Once one does, the
    /// message stays in the array but renders nothing and costs no input
    /// tokens, on that request and every later one.
    NextUserMessage,
}

/// A message's `content`: either a bare string or an array of typed blocks.
/// Untagged so it serializes to exactly the wire form (no wrapper) and
/// deserializes by trying string first, then the block array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Content,
    /// Per-message output configuration — `role: "system"` only; the API
    /// rejects it on any other role. Carries `effort` (`low` … `max`);
    /// `format` stays top-level. Beta `mid-conversation-output-config-2026-07-01`.
    /// Omitted from the wire when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    /// Turn scope — `role: "system"` only. Beta
    /// `mid-conversation-system-clear-at-2026-08-21`. Omitted from the wire
    /// when unset, which the API reads as `never`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear_at: Option<ClearAt>,
}

impl Message {
    fn with_role(role: Role, content: impl Into<Content>) -> Self {
        Self {
            role,
            content: content.into(),
            output_config: None,
            clear_at: None,
        }
    }

    pub fn user(content: impl Into<Content>) -> Self {
        Self::with_role(Role::User, content)
    }

    pub fn assistant(content: impl Into<Content>) -> Self {
        Self::with_role(Role::Assistant, content)
    }

    /// A mid-conversation system message, `{"role":"system","content":...}`.
    /// No beta header; renders on every request that includes it.
    pub fn system(content: impl Into<Content>) -> Self {
        Self::with_role(Role::System, content)
    }

    /// The documented per-message effort shape: `{"role":"system",
    /// "content":[],"output_config":{"effort":<level>}}`. Accepted anywhere
    /// in `messages`, the documented place being between an `assistant` turn
    /// and the next `user` turn; the level applies from that user turn until
    /// another system message changes it. Beta
    /// `mid-conversation-output-config-2026-07-01`.
    pub fn system_effort(effort: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Content::Blocks(Vec::new()),
            output_config: Some(OutputConfig {
                effort: Some(effort.into()),
                ..OutputConfig::default()
            }),
            clear_at: None,
        }
    }

    /// A turn-scoped system message, `{"role":"system","clear_at":
    /// "next_user_message","content":...}` — the per-turn reminder. Text
    /// only. Beta `mid-conversation-system-clear-at-2026-08-21`.
    pub fn turn_scoped(content: impl Into<Content>) -> Self {
        Self {
            clear_at: Some(ClearAt::NextUserMessage),
            ..Self::with_role(Role::System, content)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ContentBlock;
    use crate::json::JsonValue;
    use serde_json::json;

    fn round_trip(m: &Message) {
        let s = serde_json::to_string(m).unwrap();
        let back: Message = serde_json::from_str(&s).unwrap();
        assert_eq!(m, &back, "round-trip mismatch via {s}");
    }

    // ----- the polymorphic content forms both work -----

    #[test]
    fn string_content_matches_spec_shape() {
        // /docs/en/api/messages/create — the `messages` parameter's
        // single-message example: {"role":"user","content":"Hello, Claude"}.
        let m = Message::user("Hello, Claude");
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            json!({"role": "user", "content": "Hello, Claude"})
        );
        round_trip(&m);
    }

    #[test]
    fn block_array_content_matches_spec_shape() {
        // /docs/en/get-started § Call the API — the assistant message of the
        // response body, a content block array.
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
                input: JsonValue::new(json!({"a": 1, "b": 2})).unwrap(),
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
        assert_eq!(serde_json::to_value(Role::System).unwrap(), json!("system"));
    }

    // ----- mid-conversation system messages -----

    #[test]
    fn system_message_matches_spec_shape() {
        // /docs/en/build-with-claude/mid-conversation-system-messages § How it
        // works — the instruction appended after the last user turn.
        let m = Message::system(
            "From now on, every suggestion must include explicit type annotations.",
        );
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            json!({
                "role": "system",
                "content": "From now on, every suggestion must include explicit type annotations."
            })
        );
        round_trip(&m);
    }

    #[test]
    fn effort_only_system_message_matches_spec_shape() {
        // /docs/en/build-with-claude/effort § Per-message effort (beta) —
        // empty content, the new level in output_config.
        let m = Message::system_effort("low");
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            json!({"role": "system", "content": [], "output_config": {"effort": "low"}})
        );
        round_trip(&m);
    }

    #[test]
    fn turn_scoped_system_message_matches_spec_shape() {
        // /docs/en/build-with-claude/mid-conversation-system-messages
        // § Turn-scoped system messages — the per-turn reminder.
        let m = Message::turn_scoped("Request independent reads in one turn.");
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            json!({
                "role": "system",
                "clear_at": "next_user_message",
                "content": "Request independent reads in one turn."
            })
        );
        round_trip(&m);
    }

    #[test]
    fn clear_at_values_match_the_documented_strings() {
        // The reference's own validation text: "Input should be
        // 'next_user_message' or 'never'".
        assert_eq!(
            serde_json::to_value(ClearAt::Never).unwrap(),
            json!("never")
        );
        assert_eq!(
            serde_json::to_value(ClearAt::NextUserMessage).unwrap(),
            json!("next_user_message")
        );
        assert!(serde_json::from_value::<ClearAt>(json!("always")).is_err());
    }

    #[test]
    fn user_and_assistant_messages_carry_no_system_fields() {
        // The two system-only fields are absent from the wire unless set, so
        // the pre-existing shapes are byte-identical (mu-ai's byte-parity
        // suite depends on that), and a parsed legacy message reads them as
        // unset.
        for m in [Message::user("hi"), Message::assistant("yo")] {
            let v = serde_json::to_value(&m).unwrap();
            assert_eq!(v.as_object().unwrap().len(), 2, "{v}");
            assert!(m.output_config.is_none() && m.clear_at.is_none());
        }
        let m: Message = serde_json::from_value(json!({"role": "user", "content": "hi"})).unwrap();
        assert_eq!(m, Message::user("hi"));
    }

    #[test]
    fn system_message_after_tool_results_parses_from_the_documented_loop() {
        // /docs/en/build-with-claude/mid-conversation-system-messages
        // § Placement after tool results — the system message goes after the
        // user message that delivers the tool results.
        let msgs: Vec<Message> = serde_json::from_value(json!([
            { "role": "user", "content": "Run the test suite and fix any failures." },
            {
                "role": "assistant",
                "content": [{ "type": "tool_use", "id": "toolu_01", "name": "run_tests", "input": {} }]
            },
            {
                "role": "user",
                "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_01", "content": "12 passed, 0 failed" }
                ]
            },
            {
                "role": "system",
                "content": "The user sent the following message while you were working: also update the changelog before you finish."
            }
        ]))
        .unwrap();
        assert_eq!(
            msgs.iter().map(|m| m.role).collect::<Vec<_>>(),
            [Role::User, Role::Assistant, Role::User, Role::System]
        );
        assert_eq!(
            msgs[3],
            Message::system(
                "The user sent the following message while you were working: also update the changelog before you finish."
            )
        );
    }
}
