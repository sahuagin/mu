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
fn outcome_to_response(
    outcome: AuthStepOutcome,
    auth_state: &AuthStateHandle,
) -> AuthExchangeResponse {
    match outcome {
        AuthStepOutcome::Done(capability) => {
            if let Ok(mut s) = auth_state.lock() {
                *s = AuthState::Authenticated {
                    capability: capability.clone(),
                };
            }
            AuthExchangeResponse::Accepted {
                granted_capability: capability,
            }
        }
        AuthStepOutcome::Denied { code, reason } => AuthExchangeResponse::Denied { code, reason },
        AuthStepOutcome::Challenge {
            server_state_id,
            server_data,
        } => AuthExchangeResponse::Continue {
            server_state_id,
            challenge: server_data.unwrap_or_default(),
        },
    }
}
