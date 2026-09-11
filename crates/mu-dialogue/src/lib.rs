//! Library surface of `mu-dialogue`.
//!
//! Only the mesh client is public: `mu-irc-gateway` speaks the NATS agent mesh
//! through the very same `Gateway`, wire contract, and fail-closed capability
//! check that this crate's binary uses, so there is no second, drifting copy of
//! the mesh client. The MCP server itself (store, presence, check, config
//! resolution) stays private to the binary in `src/main.rs`.
//!
//! The binary refers to this module as `mu_dialogue::mesh` — identical to any
//! external consumer — rather than a binary-private `mod mesh;`. Compiling the
//! file once, in the library, keeps its `#[cfg(test)]` suites the single source
//! of truth for the mesh contract.

pub mod mesh;
