//! Handlers for the `peer.auth_*` RPC family (mu-7rk / mu-yox).
//!
//! Connect-time SASL-shaped auth handshake:
//!   - `peer.auth_offer`    — server lists supported mechanisms
//!   - `peer.auth_initiate` — caller picks mechanism + submits initial creds
//!
//! `peer.auth_response` is reserved on the wire (mu-vha) but no
//! multi-step state registry exists yet — that's mu-oeo (mu-7rk-g).
//! Until then, the dispatcher does NOT route the method (it falls
//! through to METHOD_NOT_FOUND), keeping the surface honest.
//!
//! On `Accepted`, the per-connection [`AuthState`] is updated to
//! `Authenticated { capability }`. Nothing in *this* bead reads that
//! state — enforcement on session.*/mailbox.* RPCs is mu-fnn (mu-7rk-c).

use serde_json::Value;

use mu_core::protocol::{
    AuthDenialCode, AuthExchangeResponse, AuthInitiateRequest, AuthOfferRequest, AuthOfferResponse,
    Request, Response,
};
use mu_core::transport::{codes, err_response, ok_response};

use super::super::auth::{AuthRegistry, AuthState, AuthStateHandle, AuthStepOutcome};
use super::to_value_or_null;

pub fn handle_auth_offer(request: Request<Value>, auth_registry: &AuthRegistry) -> Response<Value> {
    let _params: AuthOfferRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("peer.auth_offer: invalid params: {e}"),
            );
        }
    };
    let resp = AuthOfferResponse {
        mechanisms: auth_registry.offered(),
    };
    ok_response(request.id, to_value_or_null(resp))
}

pub fn handle_auth_initiate(
    request: Request<Value>,
    auth_registry: &AuthRegistry,
    auth_state: &AuthStateHandle,
) -> Response<Value> {
    let params: AuthInitiateRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("peer.auth_initiate: invalid params: {e}"),
            );
        }
    };
    let handler = match auth_registry.get(&params.mechanism) {
        Some(h) => h,
        None => {
            let resp = AuthExchangeResponse::Denied {
                code: AuthDenialCode::UnsupportedMechanism,
                reason: format!("no handler registered for mechanism `{}`", params.mechanism),
            };
            return ok_response(request.id, to_value_or_null(resp));
        }
    };
    let outcome = handler.step_initial(params.initial_response.as_deref());
    let resp = outcome_to_response(outcome, auth_state);
    ok_response(request.id, to_value_or_null(resp))
}

/// Convert a handler step outcome into the wire response. On
/// `Done(capability)`, the per-connection `AuthState` is updated to
/// `Authenticated { capability }`. Recording-only in this bead — no
/// other arm in this dispatcher reads `AuthState` yet (mu-fnn).
///
/// mu-m84: when the per-connection mutex is poisoned at the moment a
/// `Done(_)` outcome arrives, the connection's `AuthState` cannot be
/// safely transitioned to `Authenticated`. Answering `Accepted`
/// regardless (pre-fix behavior) is a lying response — the client
/// believes it is authenticated while the server's state stays at
/// `Unauthenticated`, so every subsequent protected RPC (once mu-fnn
/// lands enforcement) would surface `auth_required` and trigger a
/// retry loop. We instead surface the lock failure as
/// `Denied { MalformedExchange, .. }`. `MalformedExchange` is the
/// closest existing variant; adding a new `AuthDenialCode` for
/// "internal state error" is mu-fnn surface, not mu-m84's.
fn outcome_to_response(
    outcome: AuthStepOutcome,
    auth_state: &AuthStateHandle,
) -> AuthExchangeResponse {
    match outcome {
        AuthStepOutcome::Done(capability) => match auth_state.lock() {
            Ok(mut s) => {
                *s = AuthState::Authenticated {
                    capability: capability.clone(),
                };
                AuthExchangeResponse::Accepted {
                    granted_capability: capability,
                }
            }
            Err(_poisoned) => AuthExchangeResponse::Denied {
                code: AuthDenialCode::MalformedExchange,
                reason: "internal state error".into(),
            },
        },
        AuthStepOutcome::Denied { code, reason } => {
            // mu-fnn: denial is terminal at the connection level. The
            // dispatcher's enforcement gate uses `AuthState::Denied` to
            // reject all subsequent RPCs (including auth retries) with
            // `auth_denied`. If the lock is poisoned we fall through
            // without mutating state — the wire response still carries
            // the denial, and the gate rejects everything-but-
            // Authenticated, so the connection remains safe.
            if let Ok(mut s) = auth_state.lock() {
                *s = AuthState::Denied { code };
            }
            AuthExchangeResponse::Denied { code, reason }
        }
        AuthStepOutcome::Challenge {
            server_state_id,
            server_data,
        } => AuthExchangeResponse::Continue {
            server_state_id,
            challenge: server_data.unwrap_or_default(),
        },
    }
}

#[cfg(test)]
mod tests {
    //! mu-m84: poisoned-mutex regression coverage for
    //! `outcome_to_response`. Lives inline because the function is
    //! private to this module; integration tests in
    //! `crates/mu-coding/tests/auth_smoke.rs` would require exposing
    //! it as `pub`, which is API-surface creep for a test-only need.

    use super::*;
    use std::sync::{Arc, Mutex};

    use mu_core::capability::Capability;

    /// mu-m84: when the per-connection `AuthState` mutex is poisoned
    /// at the moment a `Done(_)` outcome arrives, the dispatcher must
    /// NOT answer `Accepted` (a lying success) — it must answer
    /// `Denied { MalformedExchange, .. }`. Pre-fix, the lock failure
    /// was silently swallowed by `if let Ok(...)` and `Accepted` was
    /// returned regardless, leaving the state at `Unauthenticated`
    /// while the client believed it was in.
    #[test]
    fn bearer_done_under_lock_poison_does_not_respond_accepted() {
        let handle: AuthStateHandle = Arc::new(Mutex::new(AuthState::Unauthenticated));

        // Poison the mutex by panicking a background thread while it
        // holds the lock. `.join()` returns `Err(_)` once the panic
        // unwinds; we ignore it — the side effect we care about is
        // the now-poisoned state of `handle`.
        let poison = Arc::clone(&handle);
        let join_result = std::thread::spawn(move || {
            let _g = poison
                .lock()
                .expect("test setup: acquire lock to intentionally poison");
            panic!("mu-m84 test setup: intentional poison");
        })
        .join();
        assert!(
            join_result.is_err(),
            "test setup: poisoner thread must have panicked",
        );
        assert!(
            handle.is_poisoned(),
            "test setup: mutex must be poisoned after the panicking holder",
        );

        let outcome = AuthStepOutcome::Done(Capability::root());
        let resp = outcome_to_response(outcome, &handle);

        match resp {
            AuthExchangeResponse::Denied { code, .. } => {
                assert_eq!(
                    code,
                    AuthDenialCode::MalformedExchange,
                    "poisoned-lock denial must reuse MalformedExchange, not a new variant",
                );
            }
            other => {
                panic!("expected Denied{{MalformedExchange}} under poisoned lock; got {other:?}")
            }
        }

        // State must remain `Unauthenticated` — we surfaced the
        // failure to the client and did not unilaterally upgrade the
        // session.
        let guard = handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            matches!(*guard, AuthState::Unauthenticated),
            "AuthState must stay Unauthenticated after poisoned-lock denial; got {:?}",
            *guard,
        );
    }
}
