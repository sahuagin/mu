//! mu-051 P6a: admission for commands arriving over NATS (spec mu-051 §5,
//! §6, §11).
//!
//! A command is the §5 envelope `{v, id, method, params, capability}` on
//! `mu.api.v1.<daemon>.cmd` (daemon-scoped methods) or
//! `mu.api.v1.<daemon>.<session>.cmd` (session-scoped ones). [`admit`] turns
//! one into the JSON-RPC [`Request`] `ingest` takes plus the auth state that
//! request alone runs under, or into a refusal to send straight back.
//!
//! The split: what is wrong with the message itself (not JSON, not an
//! envelope, a method not offered, the wrong subject) is refused here,
//! like a parse error on stdio. The capability is checked here too, but its
//! outcome travels into `ingest` as the request's [`AuthState`], so the
//! command is journaled and the auth gate rejects it there with a
//! `CommandRejected` receipt (spec §11: "checked in ingest before
//! dispatch"). There is no connection to hold auth state for: every request
//! carries its own capability, verified against the `[mesh].issuer_key`
//! public half for the right its method needs.

use base64::Engine as _;
use biscuit_auth::macros::authorizer;
use biscuit_auth::{Biscuit, PublicKey};
use serde::Deserialize;
use serde_json::{json, Value};

use mu_core::capability::Capability;
use mu_core::protocol::{
    AuthDenialCode, Request, SessionEventsRequest, SessionJoinRequest, SessionListRequest,
    SessionStatsRequest, JSONRPC_VERSION,
};
use mu_core::transport::codes;

use super::auth::AuthState;
use super::events_out::token;

/// The envelope version this daemon speaks.
const ENVELOPE_VERSION: u64 = 1;

/// The right a capability must carry for the read-only methods (spec §11).
pub(crate) const OBSERVE_RIGHT: &str = "observe";

/// Which subject a method is addressed on (spec §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Addressing {
    Daemon,
    Session,
}

/// The methods offered over NATS, with the right each needs and where it
/// is addressed. P6a offers the read-only ones and P6b the join (spec §9);
/// anything else is refused before `ingest`. P7 adds prompting with its own
/// right.
const OFFERED: &[(&str, &str, Addressing)] = &[
    (
        SessionListRequest::METHOD,
        OBSERVE_RIGHT,
        Addressing::Daemon,
    ),
    (
        SessionEventsRequest::METHOD,
        OBSERVE_RIGHT,
        Addressing::Session,
    ),
    (
        SessionStatsRequest::METHOD,
        OBSERVE_RIGHT,
        Addressing::Session,
    ),
    (
        SessionJoinRequest::METHOD,
        OBSERVE_RIGHT,
        Addressing::Session,
    ),
];

/// The two subjects a daemon subscribes for commands.
pub(crate) fn command_subjects(daemon_id: &str) -> [String; 2] {
    let d = token(daemon_id);
    [format!("mu.api.v1.{d}.cmd"), format!("mu.api.v1.{d}.*.cmd")]
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    v: u64,
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
    #[serde(default)]
    capability: Option<String>,
}

/// A command `ingest` can take, and the auth state it runs under.
#[derive(Debug)]
pub(crate) struct Admitted {
    pub request: Request<Value>,
    pub auth: AuthState,
}

/// A command refused before `ingest`; [`Refused::reply`] is the §5 error
/// reply to send back.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Refused {
    pub id: Value,
    pub code: i32,
    pub message: String,
}

impl Refused {
    fn new(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            id,
            code,
            message: message.into(),
        }
    }

    pub(crate) fn reply(&self) -> Value {
        json!({
            "v": ENVELOPE_VERSION,
            "id": self.id,
            "error": { "code": self.code, "message": self.message },
        })
    }
}

/// Turn the JSON-RPC response the pipeline produced into the §5 reply:
/// the same `id` and `result`/`error`, with `v` in place of `jsonrpc`.
pub(crate) fn to_reply(mut response: Value) -> Value {
    if let Some(obj) = response.as_object_mut() {
        obj.remove("jsonrpc");
        obj.insert("v".to_string(), json!(ENVELOPE_VERSION));
    }
    response
}

/// Admit one command that arrived on `subject`.
pub(crate) fn admit(
    subject: &str,
    payload: &[u8],
    daemon_id: &str,
    issuer: PublicKey,
) -> Result<Admitted, Refused> {
    let value: Value = serde_json::from_slice(payload)
        .map_err(|_| Refused::new(Value::Null, codes::PARSE_ERROR, "Parse error"))?;
    let env: Envelope = serde_json::from_value(value).map_err(|e| {
        Refused::new(
            Value::Null,
            codes::INVALID_REQUEST,
            format!("not a command envelope {{v, id, method, params, capability}}: {e}"),
        )
    })?;
    let id = env.id.clone();
    if env.v != ENVELOPE_VERSION {
        return Err(Refused::new(
            id,
            codes::INVALID_REQUEST,
            format!(
                "envelope version {} is not supported (this daemon speaks {ENVELOPE_VERSION})",
                env.v
            ),
        ));
    }
    let Some(&(_, right, addressing)) = OFFERED.iter().find(|(m, _, _)| *m == env.method) else {
        let offered: Vec<&str> = OFFERED.iter().map(|(m, _, _)| *m).collect();
        return Err(Refused::new(
            id,
            codes::METHOD_NOT_FOUND,
            format!(
                "`{}` is not offered over NATS; offered: {}",
                env.method,
                offered.join(", ")
            ),
        ));
    };

    let d = token(daemon_id);
    let addressed_session = subject
        .strip_prefix(&format!("mu.api.v1.{d}."))
        .and_then(|rest| match rest.strip_suffix(".cmd") {
            None if rest == "cmd" => Some(None),
            Some(s) if !s.is_empty() && !s.contains('.') => Some(Some(s)),
            _ => None,
        });
    match (addressing, addressed_session) {
        (Addressing::Daemon, Some(None)) => {}
        (Addressing::Session, Some(Some(subject_session))) => {
            let in_params = env.params.get("session_id").and_then(Value::as_str);
            if in_params.map(token).as_deref() != Some(subject_session) {
                return Err(Refused::new(
                    id,
                    codes::INVALID_PARAMS,
                    format!(
                        "`{}` on {subject} needs params.session_id naming that session",
                        env.method
                    ),
                ));
            }
        }
        (Addressing::Daemon, _) => {
            return Err(Refused::new(
                id,
                codes::INVALID_REQUEST,
                format!(
                    "`{}` is daemon-scoped: send it to mu.api.v1.{d}.cmd",
                    env.method
                ),
            ));
        }
        (Addressing::Session, _) => {
            return Err(Refused::new(
                id,
                codes::INVALID_REQUEST,
                format!(
                    "`{}` is session-scoped: send it to mu.api.v1.{d}.<session>.cmd",
                    env.method
                ),
            ));
        }
    }

    let auth = match env.capability.as_deref() {
        None | Some("") => AuthState::Unauthenticated,
        Some(capability) => match verify(capability, issuer, right) {
            Ok(()) => AuthState::Authenticated {
                capability: Capability::read_only(),
            },
            Err(code) => AuthState::Denied { code },
        },
    };
    Ok(Admitted {
        request: Request {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            method: env.method,
            params: env.params,
        },
        auth,
    })
}

/// Verify a base64 biscuit against `issuer` for `right`. No fail-open path:
/// a token that does not decode is malformed, one that decodes but fails
/// its signature or lacks the right is invalid.
fn verify(capability_b64: &str, issuer: PublicKey, right: &str) -> Result<(), AuthDenialCode> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(capability_b64)
        .map_err(|_| AuthDenialCode::MalformedExchange)?;
    let token = Biscuit::from(&bytes, issuer).map_err(|_| AuthDenialCode::InvalidCredentials)?;
    let mut authz = authorizer!(r#"allow if right({r});"#, r = right)
        .build(&token)
        .map_err(|_| AuthDenialCode::InvalidCredentials)?;
    authz
        .authorize()
        .map(|_| ())
        .map_err(|_| AuthDenialCode::InvalidCredentials)
}

#[cfg(test)]
mod tests {
    use super::*;
    use biscuit_auth::macros::biscuit;
    use biscuit_auth::KeyPair;

    const D: &str = "d1";

    fn mint(root: &KeyPair, right: &str) -> String {
        let token = biscuit!(r#"right({r});"#, r = right)
            .build(root)
            .expect("mint");
        base64::engine::general_purpose::STANDARD.encode(token.to_vec().expect("serialize"))
    }

    fn envelope(method: &str, params: Value, capability: Option<String>) -> Vec<u8> {
        let mut env = json!({ "v": 1, "id": "c-1", "method": method, "params": params });
        if let Some(c) = capability {
            env["capability"] = json!(c);
        }
        serde_json::to_vec(&env).unwrap()
    }

    #[test]
    fn a_valid_observe_capability_admits_a_read_only_command() {
        let root = KeyPair::new();
        let payload = envelope(
            "session.events",
            json!({ "session_id": "s1", "from_end": true }),
            Some(mint(&root, OBSERVE_RIGHT)),
        );
        let a = admit("mu.api.v1.d1.s1.cmd", &payload, D, root.public()).expect("admitted");
        assert_eq!(a.request.method, "session.events");
        assert_eq!(a.request.id, json!("c-1"));
        assert_eq!(a.request.jsonrpc, JSONRPC_VERSION);
        assert!(
            matches!(a.auth, AuthState::Authenticated { .. }),
            "{:?}",
            a.auth
        );

        let list = envelope("session.list", json!({}), Some(mint(&root, OBSERVE_RIGHT)));
        let a = admit("mu.api.v1.d1.cmd", &list, D, root.public()).expect("admitted");
        assert!(matches!(a.auth, AuthState::Authenticated { .. }));

        let join = envelope(
            "session.join",
            json!({ "session_id": "s1" }),
            Some(mint(&root, OBSERVE_RIGHT)),
        );
        let a = admit("mu.api.v1.d1.s1.cmd", &join, D, root.public()).expect("admitted");
        assert_eq!(a.request.method, "session.join");
        assert!(matches!(a.auth, AuthState::Authenticated { .. }));
    }

    /// Every capability failure still reaches ingest, as an auth state the
    /// gate rejects there and journals; none of them admits.
    #[test]
    fn capability_failures_become_auth_states_the_gate_rejects() {
        let root = KeyPair::new();
        let rogue = KeyPair::new();
        let cases = [
            (None, "missing"),
            (Some("%%% not base64".to_string()), "malformed"),
            (Some(mint(&rogue, OBSERVE_RIGHT)), "wrong issuer"),
            (Some(mint(&root, "agent_dm")), "wrong right"),
        ];
        for (capability, what) in cases {
            let payload = envelope("session.list", json!({}), capability);
            let a = admit("mu.api.v1.d1.cmd", &payload, D, root.public()).expect(what);
            let expected = match what {
                "missing" => matches!(a.auth, AuthState::Unauthenticated),
                "malformed" => matches!(
                    a.auth,
                    AuthState::Denied {
                        code: AuthDenialCode::MalformedExchange
                    }
                ),
                _ => matches!(
                    a.auth,
                    AuthState::Denied {
                        code: AuthDenialCode::InvalidCredentials
                    }
                ),
            };
            assert!(expected, "{what}: {:?}", a.auth);
        }
    }

    #[test]
    fn methods_not_offered_are_refused_before_ingest() {
        let root = KeyPair::new();
        let payload = envelope(
            "ask_session",
            json!({ "session_id": "s1", "prompt": "hi" }),
            Some(mint(&root, OBSERVE_RIGHT)),
        );
        let r = admit("mu.api.v1.d1.s1.cmd", &payload, D, root.public()).unwrap_err();
        assert_eq!(r.code, codes::METHOD_NOT_FOUND);
        assert_eq!(r.id, json!("c-1"));
        assert!(r.message.contains("session.list"), "{}", r.message);
        assert_eq!(r.reply()["v"], 1);
        assert_eq!(r.reply()["error"]["code"], codes::METHOD_NOT_FOUND);
    }

    #[test]
    fn a_method_must_arrive_on_its_own_subject() {
        let root = KeyPair::new();
        let cap = Some(mint(&root, OBSERVE_RIGHT));
        let cases = [
            (
                "session.list",
                json!({}),
                "mu.api.v1.d1.s1.cmd",
                codes::INVALID_REQUEST,
            ),
            (
                "session.events",
                json!({ "session_id": "s1" }),
                "mu.api.v1.d1.cmd",
                codes::INVALID_REQUEST,
            ),
            (
                "session.events",
                json!({ "session_id": "s2" }),
                "mu.api.v1.d1.s1.cmd",
                codes::INVALID_PARAMS,
            ),
            (
                "session.stats",
                json!({}),
                "mu.api.v1.d1.s1.cmd",
                codes::INVALID_PARAMS,
            ),
            (
                "session.join",
                json!({ "session_id": "s1" }),
                "mu.api.v1.d1.cmd",
                codes::INVALID_REQUEST,
            ),
            (
                "session.list",
                json!({}),
                "mu.api.v1.other.cmd",
                codes::INVALID_REQUEST,
            ),
        ];
        for (method, params, subject, code) in cases {
            let payload = envelope(method, params, cap.clone());
            let r = admit(subject, &payload, D, root.public()).unwrap_err();
            assert_eq!(r.code, code, "{method} on {subject}: {}", r.message);
        }
    }

    #[test]
    fn malformed_messages_are_refused_with_json_rpc_codes() {
        let root = KeyPair::new();
        let subject = "mu.api.v1.d1.cmd";
        let r = admit(subject, b"}{", D, root.public()).unwrap_err();
        assert_eq!((r.code, r.id.clone()), (codes::PARSE_ERROR, Value::Null));

        let r = admit(
            subject,
            br#"{"jsonrpc":"2.0","id":1,"method":"session.list"}"#,
            D,
            root.public(),
        )
        .unwrap_err();
        assert_eq!(
            r.code,
            codes::INVALID_REQUEST,
            "stdio framing is not the envelope"
        );

        let r = admit(
            subject,
            br#"{"v":2,"id":"x","method":"session.list"}"#,
            D,
            root.public(),
        )
        .unwrap_err();
        assert_eq!((r.code, r.id), (codes::INVALID_REQUEST, json!("x")));
    }

    #[test]
    fn replies_carry_v_in_place_of_jsonrpc() {
        let reply = to_reply(json!({ "jsonrpc": "2.0", "id": "c-1", "result": { "ok": true } }));
        assert_eq!(
            reply,
            json!({ "v": 1, "id": "c-1", "result": { "ok": true } })
        );
    }

    #[test]
    fn command_subjects_cover_the_daemon_and_its_sessions() {
        assert_eq!(
            command_subjects("d.1"),
            [
                "mu.api.v1.d_1.cmd".to_string(),
                "mu.api.v1.d_1.*.cmd".to_string()
            ]
        );
    }
}
