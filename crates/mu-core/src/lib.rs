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
