//! `mu-irc-gateway` — a standalone single-nick IRC frontend to the mu agent
//! mesh (see `specs/plans/mu-irc-gateway-v0.md`).
//!
//! The crate was built in reviewable capability slices: each one a pure,
//! testable *decision* module with no socket and no live mesh, and then a final
//! increment that runs them against a real server. The split survives in the
//! structure — [`routing`] decides what to put on IRC, [`outbound`] decides what
//! to publish, [`membership`] decides who is present, and only [`bridge`] and
//! [`transport`] touch the network — which is why every rule below can be tested
//! without either server.
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
//! Increment **2b** — the adapter, membership, and mesh→IRC routing:
//!
//! - [`adapter`] — a single-connection registration/capability state machine
//!   over a small [`adapter::Transport`] seam: CAP negotiation, mandatory SASL
//!   PLAIN when configured (refused over cleartext, failing closed rather than
//!   registering unauthenticated, chunked per IRCv3, and never retained in any
//!   printable state), optional message-tags/account capabilities, and live
//!   `CASEMAPPING`/`CHANNELLEN` from ISUPPORT.
//! - [`membership`] — disposable channel membership folded under the live
//!   CASEMAPPING, reconciled from generation-scoped NAMES against intervening
//!   JOIN/PART/KICK/QUIT/NICK; the sole authority on human presence; plus a
//!   channel reconciler that decides which channels to be in and backs off a
//!   refused JOIN.
//! - [`routing`] — mesh→IRC decisions behind the fail-closed
//!   `mesh::verify_and_decode_dm` ingress: exactly-once endpoint/observer
//!   overlap handling over a bounded [`recent`] window, channel delivery to
//!   agents whose exact peer id is discovered (with a collision label when the
//!   channel is shared), lobby fallbacks naming the intended target, and
//!   exclusive human precedence (remembered channel / private / body-free
//!   notice) against current observed membership.
//! - [`recent`] — the one bounded retention policy the gateway's
//!   "have I seen this id?" memories share.
//!
//! Increment **3** — the IRC→mesh direction:
//!
//! - [`outbound`] — a human's IRC line becomes a mesh publish decision: an
//!   explicit `role:id:` address, the agent a channel maps to, or a fan-out
//!   across every discovered agent under ONE caller-supplied minted id; with
//!   one sender-authorization check ahead of all destination logic,
//!   destination/ambiguity/human refusals, routing-memory writes only for
//!   specifically-addressed lines, and both loop guards (the gateway's own
//!   nick, and its own minted ids / `human:` senders recorded before publication
//!   can race observation, in the bounded [`recent`] window).
//!
//! Integration — what actually runs:
//!
//! - [`transport`] — one TCP connection, TLS by default, split into a line
//!   reader and a bounded line writer. Deliberately NOT an IRC client crate:
//!   [`adapter`] already owns registration, and what was missing is a socket.
//! - [`bridge`] — where they are run. Its [mesh side](bridge::mesh_side) is one
//!   task per input (the subscriptions behind an ingress gate, an ordered
//!   presence worker, an ordered per-session publish worker, the discovery sweep,
//!   and the NATS connection watcher), with connection-aware dropping that
//!   replays nothing across a reconnect on either side. Its [session
//!   half](bridge::run) is one IRC connection around one state-owning select
//!   loop, with reconnect backoff; `main.rs` is configuration, signals, and a
//!   call to [`bridge::run`].
//!
//! The textual bot verbs (`mu peers`, `mu say …`) remain absent, landing in
//! their own independently reviewed increment.

pub mod adapter;
pub mod bridge;
pub mod config;
pub mod framing;
pub mod mapping;
pub mod membership;
pub mod outbound;
pub mod recent;
pub mod routing;
pub mod transport;

pub use adapter::{
    AdapterError, Clock, ConnectRequest, Diagnostic, FixedClock, IrcMessage, IsupportSettings,
    Negotiated, Registration, Step, SystemClock, Transport, DEFAULT_CHANNELLEN,
};
pub use config::{
    default_config_path, load, load_irc, validate_nick, ConfigError, GatewayConfig, IrcConfig,
    NickFault, SaslCreds, Secret, NICK_MAX_LEN,
};
pub use framing::{
    frame_privmsg, validate_target, FrameParams, FramingError, CONTINUATION_MARKER, LINE_BUDGET,
};
pub use mapping::{
    channel_for, fold_nick, human_identity, human_peer, peer_alias, resolve_channel, CaseMapping,
    HumanIdentity, Resolved, SelfNick,
};
pub use membership::{ChannelEffect, ChannelReconciler, HumanEffect, Member, Membership};
pub use outbound::{MemoryUpdate, OutDrop, OutEnv, Outbound, OutboundDecision, RefuseReason};
pub use recent::{RecentSet, DEFAULT_CAPACITY};
pub use routing::{
    DropReason, IngressRejected, OversizedField, RouteDecision, RouteEnv, Router,
    MAX_DESTINATION_LEN, MAX_FIELD_LEN,
};

// The shared mesh config type, re-exported so a consumer configures the mesh
// side through this crate without a second dependency edge.
pub use mu_dialogue::mesh::MeshConfig;

// The verified-DM types the routing ingress produces and consumes, re-exported
// so a consumer stays on this crate's surface. Verification is unchanged — the
// SAME `mesh::verify_and_decode_dm` the daemon runs.
pub use mu_dialogue::mesh::{DmRejected, MeshDmEvent, Reception};
