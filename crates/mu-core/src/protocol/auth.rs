//! mu-7rk auth handshake — wire types (mu-vha: types only).
//!
//! Wire types for the connect-time auth handshake (SASL-shaped). This bead
//! adds the type surface only; handlers, dispatcher wiring, server state,
//! client-side, enforcement, and transport-close all land in later phases
//! (mu-7rk-b through -g). Nothing in the existing code paths consumes
//! these yet — that is intentional.
//!
//! Corrections from the rejected v0 attempt (jj rev `twztzykv`):
//!   - `AuthMechanism` is open (`Other(String)`) rather than a closed enum
//!     so peers running newer mechanisms don't fail to parse on older
//!     peers (codex important #4).
//!   - Every request/response struct has `#[serde(deny_unknown_fields)]`
//!     so typos and stray fields surface as deserialization errors
//!     rather than being silently accepted (codex important #9).
//!   - `Display` is implemented for `AuthMechanism` so callers can format
//!     the wire name without round-tripping through `serde_json`
//!     (codex minor #4).
//!
//! Extracted from `protocol.rs` per mu-6a8 (2026-05-18); re-exported by
//! `protocol::*` so external callers see no API change.

use serde::{Deserialize, Serialize};

/// Authentication mechanism for the connect-time handshake. Wire form is
/// a snake_case string. Unknown wire values deserialize to `Other(s)`
/// (forward compatibility); known values dispatch to named variants.
///
/// Phase 1 implements only `Bearer` (RFC 7628). Future mechanisms
/// (GSSAPI, OAUTHBEARER, TLS client cert, …) appear over the wire as
/// `Other("gssapi")` etc. until a build registers a handler for them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AuthMechanism {
    /// RFC 7628 BEARER — token submitted as the SASL "initial response."
    Bearer,
    /// Any mechanism whose wire name was not recognized at deserialize
    /// time. Holds the raw wire string verbatim so receivers can decide
    /// whether to negotiate or reject with `unsupported_mechanism`.
    Other(String),
}

impl AuthMechanism {
    /// Wire-form name (e.g. `"bearer"`). For `Other(s)`, returns `s`
    /// verbatim. Used by `Display` and the `Serialize` impl.
    pub fn as_wire_str(&self) -> &str {
        match self {
            AuthMechanism::Bearer => "bearer",
            AuthMechanism::Other(s) => s.as_str(),
        }
    }
}

impl std::fmt::Display for AuthMechanism {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_wire_str())
    }
}

impl Serialize for AuthMechanism {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_wire_str())
    }
}

impl<'de> Deserialize<'de> for AuthMechanism {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "bearer" => AuthMechanism::Bearer,
            _ => AuthMechanism::Other(s),
        })
    }
}

/// Reasons authentication may be rejected. Stable wire codes; new reasons
/// may be added in an additive fashion. Wire form is snake_case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthDenialCode {
    /// The local build did not register a handler for the requested
    /// mechanism (e.g. a peer asks for `gssapi` on a build without it).
    UnsupportedMechanism,
    /// The mechanism is supported, but the credential failed its
    /// mechanism-specific check (e.g. BEARER token not in allowlist).
    InvalidCredentials,
    /// The submitted message was syntactically valid but semantically
    /// wrong for the chosen mechanism (e.g. BEARER `initial_response`
    /// missing or empty).
    MalformedExchange,
}

/// `peer.auth_offer` request — caller asks the server which mechanisms
/// it supports for connect-time auth. No params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthOfferRequest {}

impl AuthOfferRequest {
    pub const METHOD: &'static str = "peer.auth_offer";
}

/// `peer.auth_offer` response — server's advertised mechanism list in
/// server-preferred order. Phase 1 default policy is `[Bearer]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthOfferResponse {
    pub mechanisms: Vec<AuthMechanism>,
}

/// `peer.auth_initiate` request — caller picks a mechanism and submits
/// the SASL "initial response." For BEARER, `initial_response` is the
/// token. For challenge-only mechanisms it may be omitted; the server
/// then replies with [`AuthExchangeResponse::Continue`] and the caller
/// follows up via [`AuthResponseRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthInitiateRequest {
    pub mechanism: AuthMechanism,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_response: Option<String>,
}

impl AuthInitiateRequest {
    pub const METHOD: &'static str = "peer.auth_initiate";
}

/// `peer.auth_response` request — caller's reply to a server-issued
/// challenge from a multi-step mechanism. v1 BEARER never reaches this
/// path; the wire surface is reserved for future mechanisms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthResponseRequest {
    /// Echoes the [`AuthExchangeResponse::Continue::server_state_id`]
    /// the caller received.
    pub server_state_id: String,
    /// Mechanism-specific response bytes; encoding is per-mechanism.
    pub response: String,
}

impl AuthResponseRequest {
    pub const METHOD: &'static str = "peer.auth_response";
}

/// Unified outcome of [`AuthInitiateRequest`] and [`AuthResponseRequest`].
/// Internally tagged on the `outcome` field; variant fields are snake_case
/// on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthExchangeResponse {
    /// Authentication succeeded; the server has bound the granted
    /// capability to the connection. Subsequent RPCs run under that
    /// capability until the connection closes.
    Accepted {
        /// Capability granted to the connection. v1 BEARER policy will
        /// return `Capability::default()`-shaped grants (per-token
        /// narrowing lands in mu-7rk-f).
        granted_capability: crate::capability::Capability,
    },
    /// Authentication failed. Connection-close on Denied is a transport
    /// concern handled by mu-7rk-d; this type only carries the structured
    /// rejection.
    Denied {
        code: AuthDenialCode,
        reason: String,
    },
    /// Multi-step mechanism — server has emitted a challenge; caller
    /// continues with [`AuthResponseRequest`]. v1 BEARER never returns
    /// this; reserved for future GSSAPI / multi-step variants.
    Continue {
        /// Opaque server-chosen handle the caller echoes back in the
        /// next [`AuthResponseRequest`].
        server_state_id: String,
        /// Mechanism-specific challenge bytes; mechanisms that need raw
        /// bytes encode them as base64 (the encoding contract is per
        /// mechanism).
        challenge: String,
    },
}
