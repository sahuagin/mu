use serde::{Deserialize, Serialize};
use serde_json::Value;

// ===== JSON-RPC 2.0 envelope =====

pub const JSONRPC_VERSION: &str = "2.0";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request<P> {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    pub params: P,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Response<R> {
    Ok {
        jsonrpc: String,
        id: Value,
        result: R,
    },
    Err {
        jsonrpc: String,
        id: Value,
        error: ErrorObject,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification<P> {
    pub jsonrpc: String,
    pub method: String,
    pub params: P,
}

// ===== Methods =====

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PingRequest;

impl PingRequest {
    pub const METHOD: &'static str = "ping";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PingResponse {
    pub pong: bool,
    pub server_version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    pub provider: ProviderSelector,
    /// Optional system prompt override. None → daemon default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

impl CreateSessionRequest {
    pub const METHOD: &'static str = "create_session";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateSessionResponse {
    pub session_id: String,
}

/// Provider selection at session-create time. Tagged enum so the wire
/// format is `{ "kind": "anthropic_api", "model": "claude-..." }`.
///
/// As of mu-019, `openai_codex` is the canonical name for OAuth-based
/// access to OpenAI via the Codex backend. Earlier protocol drafts
/// used `openai_oauth`; the rename happened when mu started talking
/// to `chatgpt.com/backend-api/codex/responses` directly instead of
/// shelling out to pi.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderSelector {
    AnthropicApi { model: String },
    AnthropicOauth { model: String },
    OpenaiApi { model: String },
    OpenaiCodex { model: String },
    Openrouter { model: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AskSessionRequest {
    pub session_id: String,
    pub user_message: String,
}

impl AskSessionRequest {
    pub const METHOD: &'static str = "ask_session";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AskSessionResponse {
    /// Acknowledgement that the request was accepted; the actual content
    /// is delivered via `session.*` notifications. Final terminator is
    /// the `session.done` notification.
    pub accepted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelSessionRequest {
    pub session_id: String,
}

impl CancelSessionRequest {
    pub const METHOD: &'static str = "cancel_session";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelSessionResponse {
    pub cancelled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloseSessionRequest {
    pub session_id: String,
}

impl CloseSessionRequest {
    pub const METHOD: &'static str = "close_session";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloseSessionResponse {
    pub closed: bool,
}

/// Query a session's running totals (mu-027). The result is a
/// snapshot, derived from the session's durable event log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatsRequest {
    pub session_id: String,
}

impl SessionStatsRequest {
    pub const METHOD: &'static str = "session.stats";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatsResponse {
    pub session_id: String,
    /// Provider kind from the wire protocol (e.g. "openai_codex").
    /// None if no SessionCreated event has been recorded (shouldn't
    /// happen in normal use; defensive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Unix ms of the first event (typically SessionCreated).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at_unix_ms: Option<u64>,
    /// Unix ms of the most recent event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity_unix_ms: Option<u64>,
    /// Total event count in the log.
    pub event_count: u32,
    /// Number of completed ask_session round-trips.
    pub ask_count: u32,
    /// Sum of Done.turn_count across all asks.
    pub total_turn_count: u32,
    /// Number of tool invocations.
    pub tool_call_count: u32,
    /// Sum of Done.elapsed_ms across all asks.
    pub elapsed_total_ms: u64,
    /// Aggregated usage across all asks. None if no Done event
    /// reported usage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::agent::Usage>,
}

/// Create a new "child" session that's lineage-aware of `parent_session_id`
/// (mu-031). The child session is fully independent at the runtime
/// level — own agent loop, own event log, own pending-approvals
/// registry — but carries a reference to its parent for audit and
/// (future) tree-rollup queries. v1: the child starts with empty
/// message history; `branched_at_parent_event_id` is recorded for
/// audit/replay but doesn't affect runtime state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelegateSessionRequest {
    pub parent_session_id: String,
    /// Provider for the child. Independent of the parent's — a child
    /// can use a different provider/model than its parent.
    pub provider: ProviderSelector,
    /// Optional: which event in the parent's log this branched from.
    /// For audit; v1 doesn't act on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branched_at_parent_event_id: Option<u64>,
}

impl DelegateSessionRequest {
    pub const METHOD: &'static str = "session.delegate";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelegateSessionResponse {
    pub child_session_id: String,
}

/// Respond to an outstanding `session.input_required` notification
/// (mu-029). The daemon blocks the corresponding tool call until
/// the client sends this back. `request_id` identifies which prompt
/// is being answered; `decision` is approve or deny.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RespondToInputRequiredRequest {
    pub session_id: String,
    pub request_id: String,
    pub decision: ApprovalDecision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approve,
    Deny,
}

impl RespondToInputRequiredRequest {
    pub const METHOD: &'static str = "session.respond_to_input_required";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RespondToInputRequiredResponse {
    /// True if the daemon found the pending request and relayed
    /// the decision. False if the request_id was unknown (already
    /// answered, timed out, or never existed).
    pub accepted: bool,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn round_trip_request() -> Result<(), serde_json::Error> {
        let request = Request {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: json!(1),
            method: PingRequest::METHOD.to_owned(),
            params: PingRequest,
        };

        let value = serde_json::to_value(&request)?;
        let decoded: Request<PingRequest> = serde_json::from_value(value)?;

        assert_eq!(decoded, request);
        Ok(())
    }

    #[test]
    fn round_trip_response_ok() -> Result<(), serde_json::Error> {
        let response = Response::Ok {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: json!("req-1"),
            result: PingResponse {
                pong: true,
                server_version: "0.1.0".to_owned(),
            },
        };

        let value = serde_json::to_value(&response)?;
        let decoded: Response<PingResponse> = serde_json::from_value(value)?;

        assert_eq!(decoded, response);
        Ok(())
    }

    #[test]
    fn round_trip_response_err() -> Result<(), serde_json::Error> {
        let response: Response<()> = Response::Err {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: json!("req-2"),
            error: ErrorObject {
                code: -32601,
                message: "method not found".to_owned(),
                data: Some(json!({ "method": "missing" })),
            },
        };

        let value = serde_json::to_value(&response)?;
        let decoded: Response<()> = serde_json::from_value(value)?;

        assert_eq!(decoded, response);
        Ok(())
    }

    #[test]
    fn round_trip_notification() -> Result<(), serde_json::Error> {
        let notification = Notification {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            method: TextDeltaEvent::METHOD.to_owned(),
            params: TextDeltaEvent {
                session_id: "session-1".to_owned(),
                delta: "hello".to_owned(),
            },
        };

        let value = serde_json::to_value(&notification)?;
        let decoded: Notification<TextDeltaEvent> = serde_json::from_value(value)?;

        assert_eq!(decoded, notification);
        Ok(())
    }

    #[test]
    fn encoded_jsonrpc_version_is_two_point_zero() -> Result<(), serde_json::Error> {
        let request = Request {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: json!(1),
            method: PingRequest::METHOD.to_owned(),
            params: PingRequest,
        };
        let notification = Notification {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            method: TextDeltaEvent::METHOD.to_owned(),
            params: TextDeltaEvent {
                session_id: "session-1".to_owned(),
                delta: "hello".to_owned(),
            },
        };

        let request_value = serde_json::to_value(request)?;
        let notification_value = serde_json::to_value(notification)?;

        assert_eq!(request_value.get("jsonrpc"), Some(&json!(JSONRPC_VERSION)));
        assert_eq!(
            notification_value.get("jsonrpc"),
            Some(&json!(JSONRPC_VERSION))
        );
        Ok(())
    }

    #[test]
    fn notification_encoding_has_no_id() -> Result<(), serde_json::Error> {
        let notification = Notification {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            method: TextDeltaEvent::METHOD.to_owned(),
            params: TextDeltaEvent {
                session_id: "session-1".to_owned(),
                delta: "hello".to_owned(),
            },
        };

        let value = serde_json::to_value(notification)?;

        assert!(value.get("id").is_none());
        Ok(())
    }

    #[test]
    fn request_id_preserves_number_and_string_shapes() -> Result<(), serde_json::Error> {
        for id in [json!(7), json!("a-uuid")] {
            let request = Request {
                jsonrpc: JSONRPC_VERSION.to_owned(),
                id: id.clone(),
                method: PingRequest::METHOD.to_owned(),
                params: PingRequest,
            };

            let value = serde_json::to_value(&request)?;
            let decoded: Request<PingRequest> = serde_json::from_value(value)?;

            assert_eq!(decoded.id, id);
            assert_eq!(decoded, request);
        }
        Ok(())
    }

    #[test]
    fn provider_selector_uses_tagged_snake_case_wire_format() -> Result<(), serde_json::Error> {
        let samples = [
            (
                ProviderSelector::AnthropicApi {
                    model: "x".to_owned(),
                },
                json!({ "kind": "anthropic_api", "model": "x" }),
            ),
            (
                ProviderSelector::AnthropicOauth {
                    model: "x".to_owned(),
                },
                json!({ "kind": "anthropic_oauth", "model": "x" }),
            ),
            (
                ProviderSelector::OpenaiApi {
                    model: "x".to_owned(),
                },
                json!({ "kind": "openai_api", "model": "x" }),
            ),
            (
                ProviderSelector::OpenaiCodex {
                    model: "x".to_owned(),
                },
                json!({ "kind": "openai_codex", "model": "x" }),
            ),
            (
                ProviderSelector::Openrouter {
                    model: "x".to_owned(),
                },
                json!({ "kind": "openrouter", "model": "x" }),
            ),
        ];

        for (selector, expected) in samples {
            let value = serde_json::to_value(&selector)?;
            let decoded: ProviderSelector = serde_json::from_value(value.clone())?;

            assert_eq!(value, expected);
            assert_eq!(decoded, selector);
        }
        Ok(())
    }

    #[test]
    fn error_response_optional_data_field_presence() -> Result<(), serde_json::Error> {
        let without_data: Response<()> = Response::Err {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: json!(1),
            error: ErrorObject {
                code: -32000,
                message: "no detail".to_owned(),
                data: None,
            },
        };
        let with_data: Response<()> = Response::Err {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: json!(2),
            error: ErrorObject {
                code: -32001,
                message: "has detail".to_owned(),
                data: Some(json!({ "reason": "example" })),
            },
        };

        let without_value = serde_json::to_value(without_data)?;
        let with_value = serde_json::to_value(with_data)?;

        assert_eq!(nested_error_data(&without_value), None);
        assert_eq!(nested_error_data(&with_value), Some(&json!({ "reason": "example" })));
        Ok(())
    }

    #[test]
    fn method_constants_match_wire_names() {
        assert_eq!(PingRequest::METHOD, "ping");
        assert_eq!(CreateSessionRequest::METHOD, "create_session");
        assert_eq!(AskSessionRequest::METHOD, "ask_session");
        assert_eq!(CancelSessionRequest::METHOD, "cancel_session");
        assert_eq!(CloseSessionRequest::METHOD, "close_session");
        assert_eq!(TextDeltaEvent::METHOD, "session.text_delta");
        assert_eq!(ToolCallStartedEvent::METHOD, "session.tool_call_started");
        assert_eq!(
            ToolCallCompletedEvent::METHOD,
            "session.tool_call_completed"
        );
        assert_eq!(DoneEvent::METHOD, "session.done");
        assert_eq!(ErrorEvent::METHOD, "session.error");
        assert_eq!(CalloutEvent::METHOD, "session.callout");
    }

    #[test]
    fn callout_text_body_round_trip() -> Result<(), serde_json::Error> {
        let event = CalloutEvent {
            session_id: "s1".to_owned(),
            kind: "observation".to_owned(),
            title: "spotted typo".to_owned(),
            body: CalloutBody::Text("line 5".to_owned()),
            theme: Some("info".to_owned()),
            context_refs: vec!["spec:mu-016".to_owned()],
        };
        let value = serde_json::to_value(&event)?;
        let decoded: CalloutEvent = serde_json::from_value(value.clone())?;
        assert_eq!(decoded, event);
        // Untagged enum: body should encode as a bare string.
        assert_eq!(value["body"], json!("line 5"));
        Ok(())
    }

    #[test]
    fn callout_structured_body_round_trip() -> Result<(), serde_json::Error> {
        let event = CalloutEvent {
            session_id: "s1".to_owned(),
            kind: "memory".to_owned(),
            title: "recalled".to_owned(),
            body: CalloutBody::Structured(json!({"id": "abc123", "preview": "..."})),
            theme: None,
            context_refs: vec![],
        };
        let value = serde_json::to_value(&event)?;
        let decoded: CalloutEvent = serde_json::from_value(value.clone())?;
        assert_eq!(decoded, event);
        // Untagged enum: structured body encodes as the object.
        assert_eq!(value["body"]["id"], "abc123");
        Ok(())
    }

    #[test]
    fn callout_skips_empty_optionals_in_encoding() -> Result<(), serde_json::Error> {
        let event = CalloutEvent {
            session_id: "s1".to_owned(),
            kind: "info".to_owned(),
            title: "hi".to_owned(),
            body: CalloutBody::Text("body".to_owned()),
            theme: None,
            context_refs: vec![],
        };
        let value = serde_json::to_value(&event)?;
        let obj = value.as_object().expect("object");
        assert!(
            !obj.contains_key("theme"),
            "theme: None should be omitted"
        );
        assert!(
            !obj.contains_key("context_refs"),
            "empty context_refs should be omitted"
        );
        Ok(())
    }

    fn nested_error_data(value: &Value) -> Option<&Value> {
        match value.get("error") {
            Some(Value::Object(error)) => error.get("data"),
            _ => None,
        }
    }

    // ===== mu-029 session.input_required round-trips =====

    #[test]
    fn input_required_event_round_trips() -> Result<(), serde_json::Error> {
        let event = InputRequiredEvent {
            session_id: "s1".into(),
            request_id: "req-42".into(),
            tool_call_id: "call_x".into(),
            tool_name: "bash".into(),
            arguments: json!({ "command": "rm -rf /tmp/scratch" }),
            summary: "bash: rm -rf /tmp/scratch".into(),
        };
        let value = serde_json::to_value(&event)?;
        let decoded: InputRequiredEvent = serde_json::from_value(value)?;
        assert_eq!(decoded, event);
        Ok(())
    }

    #[test]
    fn respond_to_input_required_round_trip_approve() -> Result<(), serde_json::Error> {
        let req = RespondToInputRequiredRequest {
            session_id: "s1".into(),
            request_id: "req-42".into(),
            decision: ApprovalDecision::Approve,
        };
        let value = serde_json::to_value(&req)?;
        assert_eq!(value["decision"], "approve");
        let decoded: RespondToInputRequiredRequest = serde_json::from_value(value)?;
        assert_eq!(decoded, req);
        Ok(())
    }

    #[test]
    fn respond_to_input_required_round_trip_deny() -> Result<(), serde_json::Error> {
        let req = RespondToInputRequiredRequest {
            session_id: "s1".into(),
            request_id: "req-42".into(),
            decision: ApprovalDecision::Deny,
        };
        let value = serde_json::to_value(&req)?;
        assert_eq!(value["decision"], "deny");
        let decoded: RespondToInputRequiredRequest = serde_json::from_value(value)?;
        assert_eq!(decoded, req);
        Ok(())
    }

    #[test]
    fn input_required_event_method_constant() {
        assert_eq!(InputRequiredEvent::METHOD, "session.input_required");
        assert_eq!(
            RespondToInputRequiredRequest::METHOD,
            "session.respond_to_input_required"
        );
    }

    // ===== mu-031 session.delegate round-trips =====

    #[test]
    fn delegate_session_request_round_trip() -> Result<(), serde_json::Error> {
        let req = DelegateSessionRequest {
            parent_session_id: "session-7".into(),
            provider: ProviderSelector::OpenaiCodex {
                model: "gpt-5.5".into(),
            },
            branched_at_parent_event_id: Some(42),
        };
        let value = serde_json::to_value(&req)?;
        let decoded: DelegateSessionRequest = serde_json::from_value(value)?;
        assert_eq!(decoded, req);
        Ok(())
    }

    #[test]
    fn delegate_session_request_optional_branch_point_omitted_when_none() -> Result<(), serde_json::Error> {
        let req = DelegateSessionRequest {
            parent_session_id: "session-7".into(),
            provider: ProviderSelector::AnthropicApi {
                model: "x".into(),
            },
            branched_at_parent_event_id: None,
        };
        let value = serde_json::to_value(&req)?;
        let obj = value.as_object().unwrap();
        assert!(
            !obj.contains_key("branched_at_parent_event_id"),
            "None branch-point should be omitted from wire"
        );
        Ok(())
    }

    #[test]
    fn delegate_session_method_constant() {
        assert_eq!(DelegateSessionRequest::METHOD, "session.delegate");
    }
}
