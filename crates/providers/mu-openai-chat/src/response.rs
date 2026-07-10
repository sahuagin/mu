//! Response side of the Chat Completions wire: the streaming
//! `chat.completion.chunk` objects carried one-per-SSE-`data:` line.
//!
//! Deserialization contract (promoted from the hand-rolled implementation,
//! defensive by construction):
//! - every field is `#[serde(default)]`-tolerant — servers differ in which
//!   keys they include on which chunk;
//! - the reasoning channel accepts both spellings (`reasoning` and
//!   `reasoning_content` — ollama vs DeepSeek-style servers, mu-mdds);
//! - `usage` may arrive on its own final chunk (with `choices: []`, the
//!   `stream_options.include_usage` contract) or attached to the last
//!   content chunk — consumers should take latest-non-`None`-wins.

use serde::{Deserialize, Serialize};

/// One streamed chunk. (The non-streaming response shape is not modeled —
/// every consumer of this crate streams; add it as a slice when needed.)
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct ChatCompletionChunk {
    #[serde(default)]
    pub choices: Vec<ChatChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct ChatChoice {
    #[serde(default)]
    pub delta: ChatDelta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct ChatDelta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Thinking-model reasoning channel. Some servers spell it
    /// `reasoning_content`; without the alias serde silently drops it and
    /// turns end with empty text while usage shows the model reasoned.
    #[serde(
        default,
        alias = "reasoning_content",
        skip_serializing_if = "Option::is_none"
    )]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

/// A fragment of a tool call. `index` correlates fragments of the same call
/// across chunks; `id`/`function.name` arrive on the first fragment,
/// `function.arguments` accumulates as string pieces across fragments.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ToolCallDelta {
    pub index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<FunctionDelta>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct FunctionDelta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

/// Chat-completions token accounting. `prompt_tokens` is the TOTAL prompt
/// (cached tokens are a subset reported under `prompt_tokens_details`) —
/// unlike the Anthropic wire, where cache reads are additive.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct PromptTokensDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct CompletionTokensDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_delta_chunk() {
        let c: ChatCompletionChunk =
            serde_json::from_str(r#"{"choices":[{"delta":{"content":"hel"}}]}"#).unwrap();
        assert_eq!(c.choices[0].delta.content.as_deref(), Some("hel"));
        assert!(c.usage.is_none());
    }

    #[test]
    fn reasoning_both_spellings() {
        let a: ChatCompletionChunk =
            serde_json::from_str(r#"{"choices":[{"delta":{"reasoning":"hm"}}]}"#).unwrap();
        let b: ChatCompletionChunk =
            serde_json::from_str(r#"{"choices":[{"delta":{"reasoning_content":"hm"}}]}"#).unwrap();
        assert_eq!(a.choices[0].delta.reasoning.as_deref(), Some("hm"));
        assert_eq!(b.choices[0].delta.reasoning.as_deref(), Some("hm"));
    }

    #[test]
    fn tool_call_fragments() {
        let first: ChatCompletionChunk = serde_json::from_str(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_9","function":{"name":"read","arguments":"{\"pa"}}]}}]}"#,
        )
        .unwrap();
        let cont: ChatCompletionChunk = serde_json::from_str(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":1}"}}]}}]}"#,
        )
        .unwrap();
        let tc = &first.choices[0].delta.tool_calls.as_ref().unwrap()[0];
        assert_eq!(tc.id.as_deref(), Some("call_9"));
        assert_eq!(tc.function.as_ref().unwrap().name.as_deref(), Some("read"));
        let tc2 = &cont.choices[0].delta.tool_calls.as_ref().unwrap()[0];
        assert!(tc2.id.is_none());
        assert_eq!(
            tc2.function.as_ref().unwrap().arguments.as_deref(),
            Some("th\":1}")
        );
    }

    #[test]
    fn final_usage_chunk_empty_choices() {
        let c: ChatCompletionChunk = serde_json::from_str(
            r#"{"choices":[],"usage":{"prompt_tokens":120,"completion_tokens":30,"prompt_tokens_details":{"cached_tokens":100},"completion_tokens_details":{"reasoning_tokens":12}}}"#,
        )
        .unwrap();
        assert!(c.choices.is_empty());
        let u = c.usage.unwrap();
        assert_eq!(u.prompt_tokens, Some(120));
        assert_eq!(u.completion_tokens, Some(30));
        assert_eq!(u.prompt_tokens_details.unwrap().cached_tokens, Some(100));
        assert_eq!(
            u.completion_tokens_details.unwrap().reasoning_tokens,
            Some(12)
        );
    }

    #[test]
    fn unknown_fields_tolerated() {
        // Servers attach ids, timestamps, model echoes, fingerprints — all
        // ignored, never an error.
        let c: ChatCompletionChunk = serde_json::from_str(
            r#"{"id":"cmpl-1","object":"chat.completion.chunk","created":1736600000,"model":"x","system_fingerprint":"fp","choices":[{"index":0,"delta":{"content":"y"},"logprobs":null,"finish_reason":null}]}"#,
        )
        .unwrap();
        assert_eq!(c.choices[0].delta.content.as_deref(), Some("y"));
        assert_eq!(c.choices[0].finish_reason, None);
    }
}
