//! Library surface of `mu-dialogue`.
//!
//! Two modules are public. `mesh`: `mu-irc-gateway` speaks the NATS agent mesh
//! through the very same `Gateway`, wire contract, and fail-closed capability
//! check that this crate's binary uses, so there is no second, drifting copy of
//! the mesh client. `presence`: the gateway reads the etcd-lease presence keys,
//! and the labels they carry, with the same reader `dialogue_peers` uses. The
//! MCP server itself (store, check) stays private to the binary in
//! `src/main.rs`.
//!
//! The binary refers to these as `mu_dialogue::mesh` and
//! `mu_dialogue::presence` — identical to any external consumer — rather than
//! binary-private modules. Compiling each file once, in the library, keeps its
//! `#[cfg(test)]` suites the single source of truth for its contract.

pub mod mesh;
pub mod presence;

/// Serializes the library's tests that mutate process environment (`HOME`,
/// `MU_CONFIG`). The binary has its own lock; the two test binaries are
/// separate processes, so neither can race the other.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
