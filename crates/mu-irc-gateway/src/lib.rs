//! `mu-irc-gateway` — a standalone single-nick IRC frontend to the mu agent
//! mesh (see `specs/plans/mu-irc-gateway-v0.md`).
//!
//! This crate is built in reviewable capability slices, all **offline** — a
//! library of pure, testable capabilities with no socket, no live mesh, and no
//! runnable bridge. The real TLS/IRC transport, the mesh subscriptions, and the
//! event loop that executes the effects these capabilities *decide* are the
//! integration increment's job.
//!
//! Increment **2a** — configuration and pure mapping/framing:
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
//! Increment **2b**, first slice — the adapter:
//!
//! - [`adapter`] — a single-connection registration/capability state machine
//!   over a small [`adapter::Transport`] seam: CAP negotiation, mandatory SASL
//!   PLAIN when configured (refused over cleartext, failing closed rather than
//!   registering unauthenticated, chunked per IRCv3, and never retained in any
//!   printable state), optional message-tags/account capabilities, and live
//!   `CASEMAPPING`/`CHANNELLEN` from ISUPPORT.
//!
//! Membership and mesh→IRC routing complete 2b and the IRC→mesh direction is
//! increment 3; the maintained IRC client and network execution (integration,
//! increment 5) and the textual bot verbs (increment 4) are deliberately
//! absent, each landing in its own independently reviewed increment.

pub mod adapter;
pub mod config;
pub mod framing;
pub mod mapping;

pub use adapter::{
    AdapterError, Clock, ConnectRequest, Diagnostic, FixedClock, IrcMessage, IsupportSettings,
    Negotiated, Registration, Step, SystemClock, Transport, DEFAULT_CHANNELLEN,
};
pub use config::{
    default_config_path, load, load_irc, validate_nick, ConfigError, GatewayConfig, IrcConfig,
    NickFault, SaslCreds, Secret, NICK_MAX_LEN,
};
pub use framing::{frame_privmsg, FrameParams, FramingError, CONTINUATION_MARKER, LINE_BUDGET};
pub use mapping::{
    channel_for, fold_nick, human_identity, human_peer, peer_alias, resolve_channel, CaseMapping,
    HumanIdentity, Resolved,
};

// The shared mesh config type, re-exported so a consumer configures the mesh
// side through this crate without a second dependency edge.
pub use mu_dialogue::mesh::MeshConfig;
