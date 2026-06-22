//! The non-streaming `Response` body (and the shared output-item / usage types
//! the streaming events also carry).
//!
//! INBOUND types: we deserialize these. Unknown fields are dropped by serde's
//! default — deliberately: the drift canary re-serializes a captured response
//! and diffs, so a field OpenAI adds that we don't model shows up as DROPPED and
//! trips the alarm. (That is the whole point of the canary; do NOT add a
//! catch-all `extra` here, which would silently absorb additions.)

use serde::{Deserialize, Serialize};

use crate::JsonValue;

/// `GET/POST /v1/responses` response object (also embedded in the streaming
/// lifecycle events).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ResponseStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output: Vec<OutputItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incomplete_details: Option<IncompleteDetails>,
}

impl Response {
    /// Concatenate all assistant `output_text` parts (the SDK convenience).
    pub fn output_text(&self) -> String {
        let mut out = String::new();
        for item in &self.output {
            if let OutputItem::Message { content, .. } = item {
                for part in content {
                    if let OutputContent::OutputText { text, .. } = part {
                        out.push_str(text);
                    }
                }
            }
        }
        out
    }
}

/// `response.status`. `Other` keeps an unrecognized status from failing the
/// whole parse (forward-compat); the drift canary still sees it round-trip
/// distinctly since the known variants serialize to their exact strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseStatus {
    Completed,
    Failed,
    Incomplete,
    InProgress,
    Queued,
    Cancelled,
    #[serde(other)]
    Other,
}

/// One item in a response's `output` array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputItem {
    Message {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        content: Vec<OutputContent>,
    },
    FunctionCall {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arguments: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },
    /// Reasoning item — modeled fully (not just `id`) so the mu provider can
    /// thread it back into the next request's `input` verbatim (encrypted_content
    /// + summary), preserving chain-of-thought across tool calls.
    Reasoning {
        id: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        summary: Vec<JsonValue>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        content: Vec<JsonValue>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },
    /// A whole output item of a kind we don't model (e.g. a non-text tool call).
    /// Round-trips losslessly; never errors the parse.
    #[serde(untagged)]
    Unknown(JsonValue),
}

/// A content part within an assistant `message` output item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputContent {
    OutputText {
        text: String,
        // Always emitted: the Responses API includes `annotations` (possibly
        // empty) on every output_text part, so we round-trip it verbatim rather
        // than skipping when empty.
        #[serde(default)]
        annotations: Vec<JsonValue>,
    },
    Refusal {
        refusal: String,
    },
    #[serde(untagged)]
    Unknown(JsonValue),
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens_details: Option<UsageInputDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens_details: Option<UsageOutputDetails>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UsageInputDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UsageOutputDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseError {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncompleteDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_trip(v: serde_json::Value) {
        let r: Response = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            v,
            "response must round-trip"
        );
    }

    #[test]
    fn completed_text_response_round_trips() {
        let v = json!({
            "id": "resp_1",
            "status": "completed",
            "output": [{
                "id": "msg_1", "type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": "hello", "annotations": []}]
            }],
            "usage": {
                "input_tokens": 3, "output_tokens": 2, "total_tokens": 5,
                "input_tokens_details": {"cached_tokens": 1},
                "output_tokens_details": {"reasoning_tokens": 1}
            }
        });
        let r: Response = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(r.output_text(), "hello");
        let u = r.usage.clone().unwrap();
        assert_eq!(u.total_tokens, Some(5));
        assert_eq!(u.output_tokens_details.unwrap().reasoning_tokens, Some(1));
        round_trip(v);
    }

    #[test]
    fn reasoning_output_item_carries_threading_fields() {
        let v = json!({
            "id": "resp_2",
            "status": "completed",
            "output": [{
                "id": "rs_1", "type": "reasoning",
                "summary": [{"type": "summary_text", "text": "plan"}],
                "encrypted_content": "enc=="
            }, {
                "id": "fc_1", "type": "function_call", "call_id": "c1",
                "name": "read", "arguments": "{}"
            }]
        });
        let r: Response = serde_json::from_value(v.clone()).unwrap();
        match &r.output[0] {
            OutputItem::Reasoning {
                encrypted_content,
                summary,
                ..
            } => {
                assert_eq!(encrypted_content.as_deref(), Some("enc=="));
                assert_eq!(summary.len(), 1);
            }
            other => panic!("expected reasoning, got {other:?}"),
        }
        round_trip(v);
    }

    #[test]
    fn cancelled_status_and_unknown_status_parse() {
        let r: Response =
            serde_json::from_value(json!({"id": "r", "status": "cancelled"})).unwrap();
        assert_eq!(r.status, Some(ResponseStatus::Cancelled));
        let r2: Response =
            serde_json::from_value(json!({"id": "r", "status": "some_new_status"})).unwrap();
        assert_eq!(r2.status, Some(ResponseStatus::Other));
    }

    #[test]
    fn unknown_output_item_kind_degrades_not_errors() {
        let r: Response = serde_json::from_value(json!({
            "id": "r", "status": "completed",
            "output": [{"type": "web_search_call", "id": "ws_1", "status": "completed"}]
        }))
        .unwrap();
        assert!(matches!(r.output[0], OutputItem::Unknown(_)));
    }
}
