//! mu-core: agent loop, JSON-RPC protocol, transport, and state.
//!
//! This crate is provider-agnostic and frontend-agnostic. It owns the
//! agent's *state machine* — receive request, dispatch tool, fold the
//! result back, optionally call the LLM, repeat — and the wire-format
//! types that frontends and the daemon speak.
//!
//! ## Module layout (planned, not yet implemented)
//!
//! - `protocol` — serde request/response types. The contract.
//! - `transport` — trait + impls for stdio / unix-socket / in-process.
//! - `loop` — the agent state machine.
//! - `state` — session/conversation state owned by the daemon.
//!
//! Module files will land as the corresponding milestones do.

#![deny(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod agent;
pub mod auditor;
pub mod aws;
pub mod capability;
pub mod config;
pub mod context;
pub mod context_attribution;
pub mod context_renderer;
pub mod event_log;
pub mod forensics;
pub mod pricing;
pub mod protocol;
pub mod route_catalog;
pub mod session_status;
pub mod skill;
pub mod t4c_source; // mu-kex4.6 phase 3: project tools+skills into t4c's RegistrySource
pub mod tool_registry;
pub mod transport;
pub mod usage_history;

/// Returns the crate version. Wired up so the workspace `cargo build`
/// produces something callable from day one.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_nonempty() {
        assert!(!version().is_empty());
    }
}
