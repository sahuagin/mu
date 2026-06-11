//! Daemon → frontend notification event types and the small handful of
//! enums that ride with them (`ApprovalDecision`, `ProviderStatusKind`).
//!
//! These are wire-level payloads for `session.*` notifications: text
//! deltas, tool call lifecycle, done/error, input-required prompts,
//! provider-status (mu-035), and the catch-all callout (mu-016). The
//! shapes here are projections of the durable event log onto the JSON-RPC
//! surface — they're what clients see, not how the daemon stores history.
//!
//! Extracted from `protocol.rs` per mu-6a8 phase 3 (2026-05-18); re-exported
//! by `protocol::*` so external callers see no API change.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ===== Approval decisions =====

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approve,
    Deny,
}

// ===== mu-035: session.provider_status =====

/// Provider-call lifecycle states surfaced to clients (mu-035). Tags
/// stable; future additions are additive. Serialized snake_case to
/// match the rest of the wire surface.
///
/// State transitions roughly:
///   AwaitingFirstToken → Streaming  (first content token)
///   AwaitingFirstToken → Thinking   (provider opens stream but stays quiet)
///   Streaming → Thinking            (gap > idle_threshold_ms mid-stream)
///   Streaming → ToolExecuting       (model decides to call a tool)
///   ToolExecuting → AwaitingToolResult (tool dispatched, awaiting result)
///   AwaitingToolResult → Streaming  (next assistant turn begins)
///   * → Idle                        (session.done landed)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStatusKind {
    Idle,
    AwaitingFirstToken,
    Streaming,
    Thinking,
    ToolExecuting,
    AwaitingToolResult,
}

/// `session.provider_status` notification payload (mu-035). Emitted
/// periodically while the agent loop is in a non-streaming wait, and
/// on every state transition. Cumulative wall-clock per call is
/// computable by summing `elapsed_ms` across consecutive
/// ProviderStatusEvents for the same session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderStatusEvent {
    pub session_id: String,
    pub kind: ProviderStatusKind,
    /// Unix milliseconds when the session entered this kind.
    pub started_at_unix_ms: u64,
    /// Milliseconds since `started_at_unix_ms`. Re-emitted in periodic
    /// ticks (Phase B) so a watching client sees the value advance.
    pub elapsed_ms: u64,
    /// Bytes received from the provider's SSE stream so far (cumulative
    /// for this turn). None when not meaningful (Idle, AwaitingFirstToken
    /// before any bytes, or providers that don't surface byte counts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_received: Option<u64>,
    /// Set only when `kind` is ToolExecuting or AwaitingToolResult.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ProviderStatusEvent {
    pub const METHOD: &'static str = "session.provider_status";
}

// ===== Event notifications (daemon → frontend) =====

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextDeltaEvent {
    pub session_id: String,
    pub delta: String,
}

impl TextDeltaEvent {
    pub const METHOD: &'static str = "session.text_delta";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantTextFinalizedEvent {
    pub session_id: String,
    /// The final assembled text from the assistant message. This text
    /// matches what will appear in the durable AssistantMessageEvent.
    /// Emitted when streaming completes, before session.done, allowing
    /// clients to swap from streaming-text to finalized text atomically.
    /// See mu-wk2.
    pub text: String,
}

impl AssistantTextFinalizedEvent {
    pub const METHOD: &'static str = "session.assistant_text_finalized";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallStartedEvent {
    pub session_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: Value,
}

impl ToolCallStartedEvent {
    pub const METHOD: &'static str = "session.tool_call_started";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallCompletedEvent {
    pub session_id: String,
    pub tool_call_id: String,
    /// `Ok(result)` or `Err(message)` — both shapes serialize as a
    /// tagged enum so the frontend can render them differently.
    pub outcome: ToolOutcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolOutcome {
    Ok { result: Value },
    Err { message: String },
}

impl ToolCallCompletedEvent {
    pub const METHOD: &'static str = "session.tool_call_completed";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoneEvent {
    pub session_id: String,
    /// Why the loop ended — EndTurn, ToolUse (shouldn't see this on
    /// wire — Done means the chain is over), MaxTokens, Error, Aborted.
    pub stop_reason: crate::agent::StopReason,
    /// Aggregated token usage across this ask_session's turns.
    /// None means no provider in the chain reported usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::agent::Usage>,
    /// Wall time from the first turn's start to this Done emit, in
    /// milliseconds. None for clean-shutdown Dones where no turns ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

impl DoneEvent {
    pub const METHOD: &'static str = "session.done";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorEvent {
    pub session_id: String,
    pub message: String,
    /// Optional structured detail; provider-specific.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

impl ErrorEvent {
    pub const METHOD: &'static str = "session.error";
}

/// Daemon→client: "the agent is about to call this tool; should it?"
/// Emitted when a tool's policy says `PermissionLevel::Ask` (or AskOnce
/// on its first invocation per session). The daemon blocks dispatch
/// until a matching `session.respond_to_input_required` arrives.
/// See spec mu-029.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputRequiredEvent {
    pub session_id: String,
    /// Token to match in the corresponding response. Unique per
    /// pending prompt; the daemon-side registry is keyed on this.
    pub request_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: Value,
    /// Why the agent is asking — typically just a short summary of
    /// the tool + arguments rendered for the human. Frontends are
    /// free to show their own UI; this is a fallback.
    pub summary: String,
}

impl InputRequiredEvent {
    pub const METHOD: &'static str = "session.input_required";
}

/// Catch-all "the agent has something notable to say" notification.
/// Free-form `kind` and optional `theme` let new categories be added
/// without protocol changes. See spec mu-016. Documented starter
/// `kind` set: `info`, `status`, `observation`, `hint`, `warning`,
/// `memory`, `peer_message`. Documented starter `theme` set: `info`,
/// `muted`, `warning`, `danger`, `success`. Frontends fall back to
/// defaults for unknown values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalloutEvent {
    pub session_id: String,
    pub kind: String,
    pub title: String,
    pub body: CalloutBody,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// References to durable artifacts (spec IDs, memory IDs,
    /// code-index paths, beads). Body should be terse; refs let
    /// consumers fetch full context.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_refs: Vec<String>,
}

/// `CalloutEvent.body` shape. `Text` for simple cases; `Structured`
/// for richer payloads frontends may render specially.
///
/// Untagged: a Text body encodes as a bare string, a Structured
/// body as a JSON object/array/etc. This means deserializing a
/// string-as-Structured is impossible — strings always come back as
/// Text. That's intentional.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CalloutBody {
    Text(String),
    Structured(Value),
}

impl CalloutEvent {
    pub const METHOD: &'static str = "session.callout";
}
