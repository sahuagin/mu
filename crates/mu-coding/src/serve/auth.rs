//! mu-7rk (mu-yox): server-side mechanism-handler trait, registry, and
//! BEARER implementation for the SASL-shaped connect-time auth
//! handshake.
//!
//! Scope is sharply bounded by the mu-7rk decomposition: this bead
//! ships the handler + dispatcher plumbing only. Enforcement
//! (`AuthState`-consuming gates on session/mailbox RPCs) is mu-fnn
//! (mu-7rk-c); transport close on denial is mu-1p6 (mu-7rk-d);
//! per-token capability narrowing is mu-7rk-f. Until those land, this
//! module RECORDS successful auth in the per-connection [`AuthState`]
//! but no other handler consumes that state.
//!
//! Corrections from the codex review of the v0 attempt:
//!
//! - **Constant-time token comparison** (important #1): tokens are
//!   never compared as raw `String`s. The handler stores SHA-256
//!   digests of allowlisted tokens and uses [`subtle::ConstantTimeEq`]
//!   on the 32-byte digests. The whole allowlist is scanned regardless
//!   of match position, so credential timing is data-independent.
//! - **Token length cap** (important #2): a candidate token longer
//!   than [`MAX_BEARER_TOKEN_LEN`] is rejected with `MalformedExchange`
//!   *before* any hashing — no length-dependent timing leak and no
//!   pathological-input CPU burn.
//! - **Duplicate-mechanism detection** (minor #1): [`AuthRegistry::new`]
//!   returns `Err(DuplicateMechanismError)` if two handlers report the
//!   same [`AuthMechanism`]. Silent overwrite hid a class of
//!   configuration / wiring bugs.
//! - (Indirectly) the v0 test antipattern that pinned unauthenticated
//!   session.* as allowed is dropped from `tests/auth_smoke.rs` — see
//!   that file's header.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use mu_core::capability::Capability;
use mu_core::config::AuthConfig;
use mu_core::protocol::{AuthDenialCode, AuthMechanism};

/// Hard cap on the byte length of a BEARER `initial_response` value.
/// Tokens above this size are rejected with `MalformedExchange`
/// *before* digest computation — preventing both length-dependent
/// timing leaks and adversarial-input CPU burn through arbitrarily
/// large allocation/hashing.
pub const MAX_BEARER_TOKEN_LEN: usize = 4096;

/// Outcome of one mechanism-handler step.
///
/// BEARER's [`AuthMechanismHandler::step_initial`] always returns
/// `Done(_)` or `Denied { … }`. The `Challenge` variant is shaped for
/// future multi-step mechanisms (GSSAPI / OAUTHBEARER); the in-flight
/// challenge-state registry that consumes it lands in mu-oeo
/// (mu-7rk-g).
#[derive(Debug, Clone)]
pub enum AuthStepOutcome {
    /// Authentication completed; caller (the dispatcher) should record
    /// `capability` against the connection.
    Done(Capability),
    /// Authentication denied for the given reason.
    Denied {
        code: AuthDenialCode,
        reason: String,
    },
    /// Multi-step mechanism — the caller continues with a follow-up
    /// `peer.auth_response`. Reserved; v1 BEARER never emits this.
    Challenge {
        server_state_id: String,
        server_data: Option<String>,
    },
}

/// Pluggable server-side mechanism handler. Implementors validate the
/// mechanism-specific credential payload and return an
/// [`AuthStepOutcome`].
///
/// This is the extension point for future mechanisms: a build with an
/// `auth-gssapi` feature flag would register a `GssapiHandler` at
/// server start; the dispatcher does not otherwise change.
pub trait AuthMechanismHandler {
    /// Mechanism this handler implements. Used by [`AuthRegistry`] to
    /// route `peer.auth_initiate.mechanism` to the right handler.
    fn mechanism(&self) -> AuthMechanism;

    /// Process the SASL initial response. For BEARER, this is the
    /// token. For challenge-only mechanisms, may be `None`.
    fn step_initial(&self, initial_response: Option<&str>) -> AuthStepOutcome;

    /// Continue a multi-step mechanism with the caller's response to a
    /// previously-emitted challenge. Default impl rejects with
    /// `MalformedExchange` — single-step mechanisms (BEARER) inherit
    /// this without override.
    fn step_response(&self, _: &str) -> AuthStepOutcome {
        AuthStepOutcome::Denied {
            code: AuthDenialCode::MalformedExchange,
            reason: "multi-step exchange not supported by this mechanism".into(),
        }
    }
}

/// BEARER (RFC 7628) handler.
///
/// Stores SHA-256 digests of the configured allowlist tokens; an
/// incoming token is digested and compared via
/// [`subtle::ConstantTimeEq`] against every stored digest. An empty
/// allowlist denies every token (the safe default for a daemon with no
/// operator-supplied auth config).
pub struct BearerHandler {
    /// Pre-computed SHA-256 digests of the configured tokens.
    digests: Vec<[u8; 32]>,
}

impl BearerHandler {
    /// Build a handler that accepts exactly the supplied tokens. An
    /// empty `tokens` vec creates a handler that denies every
    /// `step_initial` with `InvalidCredentials`.
    pub fn new(tokens: Vec<String>) -> Self {
        let digests = tokens.into_iter().map(|t| sha256(t.as_bytes())).collect();
        Self { digests }
    }
}

impl AuthMechanismHandler for BearerHandler {
    fn mechanism(&self) -> AuthMechanism {
        AuthMechanism::Bearer
    }

    fn step_initial(&self, initial_response: Option<&str>) -> AuthStepOutcome {
        let token = match initial_response {
            Some(t) if !t.is_empty() => t,
            _ => {
                return AuthStepOutcome::Denied {
                    code: AuthDenialCode::MalformedExchange,
                    reason: "BEARER requires a non-empty `initial_response` field".into(),
                };
            }
        };

        if token.len() > MAX_BEARER_TOKEN_LEN {
            return AuthStepOutcome::Denied {
                code: AuthDenialCode::MalformedExchange,
                reason: format!("BEARER token exceeds maximum length {MAX_BEARER_TOKEN_LEN} bytes",),
            };
        }

        let candidate = sha256(token.as_bytes());

        let mut matched = subtle::Choice::from(0u8);
        for stored in &self.digests {
            matched |= stored.ct_eq(&candidate);
        }

        if bool::from(matched) {
            AuthStepOutcome::Done(Capability::root())
        } else {
            AuthStepOutcome::Denied {
                code: AuthDenialCode::InvalidCredentials,
                reason: "BEARER token rejected".into(),
            }
        }
    }
}

/// Error returned by [`AuthRegistry::new`] when two handlers in the
/// supplied list report the same [`AuthMechanism`]. Silent overwrite
/// hid a class of wiring bugs in v0 (codex minor #1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateMechanismError(pub AuthMechanism);

impl std::fmt::Display for DuplicateMechanismError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "duplicate mechanism `{}` in handler registry", self.0,)
    }
}

impl std::error::Error for DuplicateMechanismError {}

/// Dispatch table mapping each [`AuthMechanism`] to its registered
/// handler. Mechanisms with no registered handler are reported as
/// `unsupported_mechanism` to the caller.
///
/// Construction order is preserved for [`AuthRegistry::offered`] so
/// `peer.auth_offer` responses and test output are deterministic.
#[derive(Default)]
pub struct AuthRegistry {
    handlers: HashMap<AuthMechanism, Box<dyn AuthMechanismHandler + Send + Sync>>,
    order: Vec<AuthMechanism>,
}

impl AuthRegistry {
    /// Build a registry from a list of handlers. Returns
    /// [`DuplicateMechanismError`] if any two handlers report the same
    /// [`AuthMechanism`].
    pub fn new(
        handlers: Vec<Box<dyn AuthMechanismHandler + Send + Sync>>,
    ) -> Result<Self, DuplicateMechanismError> {
        let mut map: HashMap<AuthMechanism, Box<dyn AuthMechanismHandler + Send + Sync>> =
            HashMap::with_capacity(handlers.len());
        let mut order: Vec<AuthMechanism> = Vec::with_capacity(handlers.len());
        for h in handlers {
            let m = h.mechanism();
            if map.contains_key(&m) {
                return Err(DuplicateMechanismError(m));
            }
            order.push(m.clone());
            map.insert(m, h);
        }
        Ok(Self {
            handlers: map,
            order,
        })
    }

    /// Look up the handler for `mech`, or `None` if no handler is
    /// registered. Dispatcher consumers should respond
    /// `Denied { code: UnsupportedMechanism, .. }` on `None`.
    pub fn get(&self, mech: &AuthMechanism) -> Option<&dyn AuthMechanismHandler> {
        self.handlers
            .get(mech)
            .map(|b| &**b as &dyn AuthMechanismHandler)
    }

    /// Mechanisms with registered handlers, in registration order.
    /// This is the wire payload of `peer.auth_offer`.
    pub fn offered(&self) -> Vec<AuthMechanism> {
        self.order.clone()
    }
}

/// Per-connection auth state. Until the connection completes a
/// successful handshake, this is `Unauthenticated`; afterwards, it
/// carries the granted [`Capability`].
///
/// Held as `Arc<Mutex<…>>` so the dispatcher updates it from one
/// request-handling task while other tasks observe the resulting state
/// (per-connection state is shared across the connection's concurrent
/// tasks).
///
/// Per-connection authentication state. The dispatcher consults this
/// for every RPC: only `Authenticated { .. }` passes the enforcement
/// gate (mu-fnn). `Denied { code }` is terminal — once a connection
/// has been denied, every subsequent method (including re-attempts of
/// pre-auth methods) is rejected with `auth_denied` until the
/// connection is closed (which is mu-1p6 / mu-7rk-d's job).
#[derive(Debug, Clone, Default)]
pub enum AuthState {
    /// No successful handshake yet. Pre-auth methods (`peer.auth_*`)
    /// are still allowed; everything else is rejected with
    /// `auth_required`.
    #[default]
    Unauthenticated,
    /// Handshake succeeded; subsequent RPCs run under this capability.
    Authenticated { capability: Capability },
    /// Terminal denial. All subsequent RPCs (including auth retries)
    /// are rejected with `auth_denied`. The connection-close on denial
    /// is mu-1p6 (mu-7rk-d), separate.
    Denied { code: AuthDenialCode },
}

/// Shared handle to a connection's [`AuthState`]. The dispatcher
/// receives one of these per connection.
pub type AuthStateHandle = Arc<Mutex<AuthState>>;

/// Build the v1 [`AuthRegistry`] from the daemon's [`AuthConfig`].
///
/// Currently registers exactly one [`BearerHandler`] whose allowlist
/// is the config-supplied tokens. Construction can't fail (no
/// duplicates by structural construction); a future config variant
/// that combined multiple mechanisms could surface the
/// [`DuplicateMechanismError`].
pub fn registry_from_config(auth: &AuthConfig) -> AuthRegistry {
    match auth {
        AuthConfig::Bearer { tokens } => {
            let h: Box<dyn AuthMechanismHandler + Send + Sync> =
                Box::new(BearerHandler::new(tokens.clone()));
            AuthRegistry::new(vec![h]).unwrap_or_default()
        }
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}
