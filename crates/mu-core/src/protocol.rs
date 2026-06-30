//! mu-core's JSON-RPC wire surface.
//!
//! Per mu-6a8 (phases 1–6, 2026-05-16 → 2026-05-18), every type lives in
//! a topic-scoped submodule under `protocol/`. This module file declares
//! those submodules and re-exports them flat — so external callers say
//! `use mu_core::protocol::{Request, CreateSessionRequest, …};` without
//! caring which submodule each type lives in.
//!
//! The remaining content in this file is the integration test module,
//! which exercises round-trips that cross submodule boundaries.

mod auth;
mod autonomy;
mod discovery;
mod events;
mod jsonrpc;
mod mailbox;
mod session;
mod stats;
pub use auth::*;
pub use autonomy::*;
pub use discovery::*;
pub use events::*;
pub use jsonrpc::*;
pub use mailbox::*;
pub use session::*;
pub use stats::*;

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
    fn round_trip_daemon_mcp_status() -> Result<(), serde_json::Error> {
        let response = DaemonMcpStatusResponse {
            snapshot_at_unix_ms: 42,
            enabled: true,
            servers: vec![McpServerStatus {
                name: "code-index".to_string(),
                url: "http://127.0.0.1:7622/mcp".to_string(),
                configured_tools: Some(vec!["code_status".to_string()]),
                prefix: None,
                side_effects: Some(crate::agent::SideEffects::ReadOnly),
                tool_side_effects: std::collections::HashMap::new(),
                state: McpServerConnectionState::Connected,
                imported_tools: vec![McpImportedToolStatus {
                    remote_name: "code_status".to_string(),
                    local_name: "code_status".to_string(),
                    side_effects: crate::agent::SideEffects::ReadOnly,
                    permission: crate::agent::PermissionLevel::Allow,
                    classified: true,
                    registered: true,
                }],
                last_error: None,
                elapsed_ms: Some(7),
            }],
        };

        let value = serde_json::to_value(&response)?;
        let decoded: DaemonMcpStatusResponse = serde_json::from_value(value)?;

        assert_eq!(decoded, response);
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
        assert_eq!(CapabilitiesDiscoverRequest::METHOD, "capabilities/discover");
    }

    #[test]
    fn capabilities_discover_request_round_trip() -> Result<(), serde_json::Error> {
        let req = CapabilitiesDiscoverRequest {
            session_id: "s1".into(),
            intent: "search file contents".into(),
            limit: Some(5),
        };
        let value = serde_json::to_value(&req)?;
        let decoded: CapabilitiesDiscoverRequest = serde_json::from_value(value)?;
        assert_eq!(decoded, req);
        // limit omitted => None
        let no_limit: CapabilitiesDiscoverRequest =
            serde_json::from_value(json!({ "session_id": "s1", "intent": "x" }))?;
        assert_eq!(no_limit.limit, None);
        Ok(())
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
                max_side_effects: None,
            }),
            cwd: None,
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
            cwd: None,
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
