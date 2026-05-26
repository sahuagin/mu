//! Mu event types — the output schema for converted events.

use serde::{Deserialize, Serialize};

/// Mu-format event envelope. Mirrors the JSONL schema written by
/// mu-core's SessionEventLog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MuEvent {
    pub id: u64,
    pub timestamp_unix_ms: u64,
    pub actor: Actor,
    pub payload: Payload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Actor {
    System,
    User,
    Agent,
    Tool { name: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Payload {
    SessionCreated {
        provider_kind: String,
        model: String,
    },
    SessionClosed,
    UserMessage {
        content: String,
    },
    AssistantMessage {
        content: String,
        stop_reason: String,
        model: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    ToolCall {
        call_id: String,
        name: String,
        arguments: serde_json::Value,
    },
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
    AuditEvent {
        subtype: String,
        body: serde_json::Value,
    },
    Callout {
        category: String,
        title: String,
        body: serde_json::Value,
    },
}

impl Payload {
    pub fn kind(&self) -> &str {
        match self {
            Self::SessionCreated { .. } => "session_created",
            Self::SessionClosed => "session_closed",
            Self::UserMessage { .. } => "user_message",
            Self::AssistantMessage { .. } => "assistant_message",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolResult { .. } => "tool_result",
            Self::AuditEvent { .. } => "audit_event",
            Self::Callout { .. } => "callout",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

impl Usage {
    /// Total prompt size: input + cached spans.
    pub fn total_prompt_tokens(&self) -> u64 {
        self.input_tokens + self.cache_read_input_tokens + self.cache_creation_input_tokens
    }
}
