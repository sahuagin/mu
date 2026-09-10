//! `mu-irc-gateway` — a standalone single-nick IRC frontend to the mu agent
//! mesh (see `specs/plans/mu-irc-gateway-v0.md`).
//!
//! This crate is built in reviewable capability slices. **Increment 2a**, the
//! present slice, is a library of pure, offline-testable capabilities only:
//!
//! - [`config`] — gateway-local `[irc]` configuration and validation, with
//!   mesh-config loading delegated to the shared `mu_dialogue::mesh::load`.
//! - [`mapping`] — CASEMAPPING-aware human identity folding, role aliases,
//!   deterministic CHANNELLEN-limited channel names, and CASEMAPPING-aware
//!   reverse resolution against a supplied current-peer snapshot.
//! - [`framing`] — UTF-8-safe PRIVMSG framing within the 512-byte line budget,
//!   with marked continuations, CR/LF injection prevention for the target, the
//!   body and the mesh id alike, and the `+mu.id` client tag only when
//!   message-tags is negotiated.
//!
//! There is deliberately no IRC client, adapter, membership, or routing here,
//! and no runnable bridge: those land in later increments, each independently
//! reviewed, so this slice stays below the review cap.

pub mod config;
pub mod framing;
pub mod mapping;

pub use config::{
    default_config_path, load, load_irc, ConfigError, GatewayConfig, IrcConfig, SaslCreds, Secret,
};
pub use framing::{frame_privmsg, FrameParams, FramingError, CONTINUATION_MARKER, LINE_BUDGET};
pub use mapping::{
    channel_for, fold_nick, human_identity, human_peer, peer_alias, resolve_channel, CaseMapping,
    HumanIdentity, Resolved,
};

// The shared mesh config type, re-exported so a consumer configures the mesh
// side through this crate without a second dependency edge.
pub use mu_dialogue::mesh::MeshConfig;
