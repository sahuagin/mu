use serde::{Deserialize, Serialize};
use serde_json::Value;

// mu-6a8: extracted submodules. Re-exported below so external callers
// (`use mu_core::protocol::{X};`) see no API change. The remaining
// in-file sections (jsonrpc envelope, session) are extraction targets
// for follow-up phases.
mod auth;
mod autonomy;
mod events;
mod mailbox;
mod stats;
pub use auth::*;
pub use autonomy::*;
pub use events::*;
pub use mailbox::*;
pub use stats::*;

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

// ===== mu-038: projection queries (session.list, session.events, daemon.stats) =====

/// Filter for `session.list`. All fields optional; default = "all
/// local, no limit." Forward-compat additive: new fields added in
/// future revisions can be ignored by older daemons.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionListFilter {
    /// Include sessions from peer daemons (requires a federating
    /// SessionDiscovery backend like FileBackend or EtcdBackend).
    /// LocalRegistryBackend ignores this flag — it only sees local.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_remote: bool,
    /// Only sessions whose parent_session_id matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Only sessions in the given status. Default = any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<SessionStatusSummary>,
    /// Only sessions with last_activity_unix_ms >= this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_since_unix_ms: Option<u64>,
    /// Cap response size. 0 or None ⇒ no limit (use cautiously).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Derived summary of where a session is in its lifecycle. Computed
/// from the session's event log (post-mu-035, the live
/// ProviderStatusTracker is authoritative for local sessions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatusSummary {
    /// No ask in flight; last event was Done/SessionClosed or the
    /// log is empty.
    Idle,
    /// User message arrived; model call may or may not have started.
    Asking,
    /// Model is producing text (text_delta-style activity within the
    /// last ~5s).
    Streaming,
    /// A tool call is in flight (started but not yet completed).
    ToolExecuting,
    /// A session.input_required notification is outstanding; the
    /// session is blocked on a client approve/deny.
    AwaitingInputRequired,
    /// Last completed ask ended cleanly.
    Done,
    /// Last event was Error.
    Errored,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    /// Stable per-daemon identifier (UUID generated at startup). Used
    /// by federating discovery backends to disambiguate sessions
    /// across daemons.
    pub daemon_id: String,
    /// True iff this session is in a peer daemon (only ever true with
    /// include_remote + a federating backend).
    pub is_remote: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    pub provider_kind: String,
    pub model: String,
    pub status: SessionStatusSummary,
    pub started_at_unix_ms: u64,
    pub last_activity_unix_ms: u64,
    pub ask_count: u32,
    pub tool_call_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cumulative_usage: Option<crate::agent::Usage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionListRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<SessionListFilter>,
}

impl SessionListRequest {
    pub const METHOD: &'static str = "session.list";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionListResponse {
    pub sessions: Vec<SessionInfo>,
    pub snapshot_at_unix_ms: u64,
    /// Set when `include_remote=true` and one or more peer daemons
    /// failed to respond. Local results are still included.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_peers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEventsRequest {
    pub session_id: String,
    /// Resume cursor from a prior page. Returns events with id > this
    /// value. Omit to start from the beginning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_event_id: Option<u64>,
    /// Cap response size. None or 0 ⇒ a sensible default (200).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Restrict to specific payload kinds (e.g. ["text_delta",
    /// "tool_call"]). Empty/omitted ⇒ all kinds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds_filter: Vec<String>,
}

impl SessionEventsRequest {
    pub const METHOD: &'static str = "session.events";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEventsResponse {
    /// Already-serialised SessionEvent values (see event_log.rs for
    /// the shape). Returned as serde_json::Value so wire consumers
    /// can decode lazily without depending on mu-core types.
    pub events: Vec<serde_json::Value>,
    /// Cursor for the next page. None when end_of_log is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_event_id: Option<u64>,
    pub end_of_log: bool,
}

// ===== mu-035: session.cancel_outstanding =====

/// Cancel the **outstanding provider call** for a session without
/// ending the session itself (mu-035). The agent loop aborts the
/// in-flight stream and surfaces a CancelOutstanding outcome to the
/// loop's outer driver, which decides what to do next (retry on the
/// same provider, fall over to a different one, surface to a human).
///
/// Distinct from `cancel_session`: that ends the session. This kills
/// just the current provider call; the session is still addressable
/// via `ask_session` immediately after.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelOutstandingRequest {
    pub session_id: String,
    /// Free-form reason for the cancel. Logged in the event log; not
    /// otherwise interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl CancelOutstandingRequest {
    pub const METHOD: &'static str = "session.cancel_outstanding";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelOutstandingResponse {
    /// True iff a provider call was actually in flight at the time of
    /// the request. False (with `was_in: Idle`) when the call is a
    /// no-op because nothing was outstanding.
    pub canceled: bool,
    pub was_in: ProviderStatusKind,
}

/// Create a new "child" session that's lineage-aware of `parent_session_id`
/// (mu-031). The child session is fully independent at the runtime
/// level — own agent loop, own event log, own pending-approvals
/// registry — but carries a reference to its parent for audit, and
/// optionally a narrowed `Capability` derived from the parent's
/// (mu-033). v1: the child starts with empty message history;
/// `branched_at_parent_event_id` is recorded for audit/replay but
/// doesn't affect runtime state.
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
    /// Optional capability attenuations (mu-033). The child's
    /// effective capability is the intersection of the parent's
    /// capability with this. Any field omitted is "no further
    /// narrowing on this axis from this request." If absent
    /// entirely, the child inherits the parent's capability
    /// unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attenuations: Option<crate::capability::CapabilityAttenuations>,
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
        assert_eq!(
            nested_error_data(&with_value),
            Some(&json!({ "reason": "example" }))
        );
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
        assert!(!obj.contains_key("theme"), "theme: None should be omitted");
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
            attenuations: Some(crate::capability::CapabilityAttenuations {
                allowed_tools: Some(vec!["read".into(), "grep".into()]),
                expires_in_seconds: Some(300),
                max_tool_calls: Some(10),
                autonomy: crate::capability::AutonomyCapability::default(),
                aws: None,
            }),
        };
        let value = serde_json::to_value(&req)?;
        let decoded: DelegateSessionRequest = serde_json::from_value(value)?;
        assert_eq!(decoded, req);
        Ok(())
    }

    #[test]
    fn delegate_session_request_optional_branch_point_omitted_when_none(
    ) -> Result<(), serde_json::Error> {
        let req = DelegateSessionRequest {
            parent_session_id: "session-7".into(),
            provider: ProviderSelector::AnthropicApi { model: "x".into() },
            branched_at_parent_event_id: None,
            attenuations: None,
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

    // ===== mu-7rk auth handshake (mu-vha) =====

    #[test]
    fn bearer_serializes_as_lowercase() -> Result<(), serde_json::Error> {
        let v = serde_json::to_value(AuthMechanism::Bearer)?;
        assert_eq!(v, json!("bearer"));
        Ok(())
    }

    #[test]
    fn other_mechanism_deserializes_from_unknown_string() -> Result<(), serde_json::Error> {
        let m: AuthMechanism = serde_json::from_value(json!("gssapi"))?;
        assert_eq!(m, AuthMechanism::Other("gssapi".into()));
        Ok(())
    }

    #[test]
    fn roundtrip_other_mechanism() -> Result<(), serde_json::Error> {
        let original = AuthMechanism::Other("oauth_bearer".into());
        let v = serde_json::to_value(&original)?;
        assert_eq!(v, json!("oauth_bearer"));
        let back: AuthMechanism = serde_json::from_value(v)?;
        assert_eq!(back, original);
        Ok(())
    }

    #[test]
    fn auth_offer_rejects_unknown_field() {
        let result: Result<AuthOfferRequest, _> =
            serde_json::from_value(json!({ "extra": "nope" }));
        assert!(
            result.is_err(),
            "AuthOfferRequest must reject unknown fields"
        );
    }

    #[test]
    fn auth_initiate_rejects_unknown_field() {
        let result: Result<AuthInitiateRequest, _> = serde_json::from_value(json!({
            "mechanism": "bearer",
            "initial_response": "hunter2",
            "extra": "nope",
        }));
        assert!(
            result.is_err(),
            "AuthInitiateRequest must reject unknown fields"
        );
    }

    #[test]
    fn auth_response_rejects_unknown_field() {
        let result: Result<AuthResponseRequest, _> = serde_json::from_value(json!({
            "server_state_id": "state-1",
            "response": "cmVzcG9uc2U=",
            "extra": "nope",
        }));
        assert!(
            result.is_err(),
            "AuthResponseRequest must reject unknown fields"
        );
    }

    #[test]
    fn auth_denial_code_snake_case() -> Result<(), serde_json::Error> {
        assert_eq!(
            serde_json::to_value(AuthDenialCode::InvalidCredentials)?,
            json!("invalid_credentials")
        );
        assert_eq!(
            serde_json::to_value(AuthDenialCode::UnsupportedMechanism)?,
            json!("unsupported_mechanism")
        );
        assert_eq!(
            serde_json::to_value(AuthDenialCode::MalformedExchange)?,
            json!("malformed_exchange")
        );
        Ok(())
    }

    #[test]
    fn auth_mechanism_display_matches_wire() -> Result<(), serde_json::Error> {
        for m in [
            AuthMechanism::Bearer,
            AuthMechanism::Other("gssapi".into()),
            AuthMechanism::Other("oauth_bearer".into()),
        ] {
            let wire = serde_json::to_value(&m)?;
            let wire_str = wire.as_str().expect("AuthMechanism serializes as string");
            assert_eq!(format!("{m}"), wire_str);
        }
        Ok(())
    }

    // ===== mu-bys: response-shape locking tests =====
    //
    // Lock the wire shape of the auth response types so a future
    // accidental change to internal/external tagging, field renaming,
    // or `deny_unknown_fields` removal surfaces as a test failure
    // rather than as a silent client breakage.

    #[test]
    fn auth_mechanism_bearer_deserializes_from_lowercase() -> Result<(), serde_json::Error> {
        let m: AuthMechanism = serde_json::from_value(json!("bearer"))?;
        assert_eq!(m, AuthMechanism::Bearer);
        Ok(())
    }

    #[test]
    fn auth_exchange_response_accepted_wire_shape() -> Result<(), serde_json::Error> {
        let resp = AuthExchangeResponse::Accepted {
            granted_capability: crate::capability::Capability::default(),
        };
        let v = serde_json::to_value(&resp)?;
        assert_eq!(v["outcome"], "accepted");
        assert!(
            v.get("granted_capability").is_some(),
            "Accepted variant must carry granted_capability"
        );
        let back: AuthExchangeResponse = serde_json::from_value(v)?;
        assert_eq!(back, resp);
        Ok(())
    }

    #[test]
    fn auth_exchange_response_denied_wire_shape() -> Result<(), serde_json::Error> {
        let resp = AuthExchangeResponse::Denied {
            code: AuthDenialCode::InvalidCredentials,
            reason: "token not in allowlist".into(),
        };
        let v = serde_json::to_value(&resp)?;
        assert_eq!(
            v,
            json!({
                "outcome": "denied",
                "code": "invalid_credentials",
                "reason": "token not in allowlist",
            })
        );
        let back: AuthExchangeResponse = serde_json::from_value(v)?;
        assert_eq!(back, resp);
        Ok(())
    }

    #[test]
    fn auth_exchange_response_continue_wire_shape() -> Result<(), serde_json::Error> {
        let resp = AuthExchangeResponse::Continue {
            server_state_id: "state-abc".into(),
            challenge: "Y2hhbGxlbmdl".into(),
        };
        let v = serde_json::to_value(&resp)?;
        assert_eq!(
            v,
            json!({
                "outcome": "continue",
                "server_state_id": "state-abc",
                "challenge": "Y2hhbGxlbmdl",
            })
        );
        let back: AuthExchangeResponse = serde_json::from_value(v)?;
        assert_eq!(back, resp);
        Ok(())
    }

    #[test]
    fn auth_exchange_response_accepted_rejects_unknown_field() {
        let v: Value = json!({
            "outcome": "accepted",
            "granted_capability": {"autonomy": {"kind": "disallowed"}},
            "extra": true,
        });
        let result: Result<AuthExchangeResponse, _> = serde_json::from_value(v);
        assert!(
            result.is_err(),
            "AuthExchangeResponse::Accepted must reject unknown fields"
        );
    }

    #[test]
    fn auth_exchange_response_denied_rejects_unknown_field() {
        let v: Value = json!({
            "outcome": "denied",
            "code": "invalid_credentials",
            "reason": "nope",
            "extra": true,
        });
        let result: Result<AuthExchangeResponse, _> = serde_json::from_value(v);
        assert!(
            result.is_err(),
            "AuthExchangeResponse::Denied must reject unknown fields"
        );
    }

    #[test]
    fn auth_exchange_response_continue_rejects_unknown_field() {
        let v: Value = json!({
            "outcome": "continue",
            "server_state_id": "state-1",
            "challenge": "Y2g=",
            "extra": true,
        });
        let result: Result<AuthExchangeResponse, _> = serde_json::from_value(v);
        assert!(
            result.is_err(),
            "AuthExchangeResponse::Continue must reject unknown fields"
        );
    }

    #[test]
    fn auth_offer_response_wire_shape_mixed_mechanisms() -> Result<(), serde_json::Error> {
        let resp = AuthOfferResponse {
            mechanisms: vec![AuthMechanism::Bearer, AuthMechanism::Other("gssapi".into())],
        };
        let v = serde_json::to_value(&resp)?;
        assert_eq!(v, json!({ "mechanisms": ["bearer", "gssapi"] }));
        let back: AuthOfferResponse = serde_json::from_value(v)?;
        assert_eq!(back, resp);
        Ok(())
    }

    #[test]
    fn auth_offer_response_rejects_unknown_field() {
        let result: Result<AuthOfferResponse, _> = serde_json::from_value(json!({
            "mechanisms": ["bearer"],
            "extra": true,
        }));
        assert!(
            result.is_err(),
            "AuthOfferResponse must reject unknown fields"
        );
    }
}
