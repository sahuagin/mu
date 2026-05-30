//! Handler for `capabilities/discover` (mu-kex4.6.4).
//!
//! Projects the calling session's live, permission-attenuated capability
//! manifest — registered tools (filtered by the session's capability) plus
//! discovered skills — and ranks it against a free-text intent. This is the
//! in-process Layer-1 `t4c find` exposed over the daemon's RPC surface; the
//! result rows are the same `CapabilityView`s `t4c_source::discover_view`
//! produces in-process.

use std::sync::Arc;

use serde_json::Value;

use mu_core::agent::Tool;
use mu_core::protocol::{
    CapabilitiesDiscoverRequest, CapabilitiesDiscoverResponse, Request, Response,
};
use mu_core::skill::loader::LoadedSkill;
use mu_core::transport::{codes, err_response, ok_response};

use super::to_value_or_null;
use crate::serve::sessions::Sessions;

/// Default top-k when the request omits `limit`.
const DEFAULT_LIMIT: usize = 20;

pub fn handle_capabilities_discover(
    request: Request<Value>,
    sessions: Sessions,
    tools: Arc<Vec<Arc<dyn Tool>>>,
    skills: Arc<Vec<LoadedSkill>>,
) -> Response<Value> {
    let params: CapabilitiesDiscoverRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("capabilities/discover: invalid params: {e}"),
            );
        }
    };

    // Discovery tracks permission: project only the tools this session's
    // capability allows. A poisoned capability lock fails closed to the
    // default (deny-ish) capability rather than leaking a stale snapshot.
    let cap = match sessions.capability(&params.session_id) {
        Some(handle) => handle.lock().map(|c| c.clone()).unwrap_or_default(),
        None => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("session not found: {}", params.session_id),
            );
        }
    };

    let registry = mu_core::t4c_source::build_manifest_for_tools(&tools, &cap, &skills);
    let tree = match registry.build() {
        Ok(t) => t,
        Err(e) => {
            return err_response(
                request.id,
                codes::INTERNAL_ERROR,
                format!("capabilities/discover: building manifest: {e}"),
            );
        }
    };

    let limit = params.limit.map(|n| n as usize).unwrap_or(DEFAULT_LIMIT);
    let results = mu_core::t4c_source::discover_view(&tree, &params.intent, limit);
    ok_response(
        request.id,
        to_value_or_null(CapabilitiesDiscoverResponse { results }),
    )
}
