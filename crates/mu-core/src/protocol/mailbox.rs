//! mu-lho (mu-037 Phase 1): peer-discovery + mailbox wire types.
//!
//! Wire surface for cross-session messaging: peer.hello handshake +
//! mailbox.post/list/consume + the `session.mailbox_message` notification.
//!
//! Extracted from `protocol.rs` per mu-6a8 (2026-05-18); re-exported by
//! `protocol::*` so external callers see no API change.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Identity advertised by the requesting session in [`PeerHelloRequest`].
/// `daemon_id` is the responding daemon's perspective on which daemon
/// the caller belongs to; Phase 1 is single-daemon so `daemon_id` is
/// always the local daemon's id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerIdentity {
    pub daemon_id: String,
    pub session_id: String,
    /// Free-form capability advertisement — informational; v1 uses
    /// it only for human-readable logging. Real authorization comes
    /// from the [`PeerHandle`] returned by [`PeerHelloResponse::Accepted`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub advertised_capabilities: Vec<String>,
}

/// What the caller is asking permission to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerWant {
    /// RPC method name the caller wants to invoke against the target
    /// session. Phase 1 only `"mailbox.post"` is accepted by the
    /// default v1 policy.
    pub method: String,
    /// Optional free-form scope description (e.g. `"grader_result"`,
    /// `"spec-summary"`). Carried for the target session's policy to
    /// inspect; v1 policy ignores it but logs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// `peer.hello` request — A asks B for a peer handle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerHelloRequest {
    /// The target session A wants to talk to.
    pub to_session_id: String,
    pub from: PeerIdentity,
    pub want: PeerWant,
}

impl PeerHelloRequest {
    pub const METHOD: &'static str = "peer.hello";
}

/// `peer.hello` response shape (the bead's "peer.reply" terminology —
/// it's the response, not a separate method). Phase 1: single
/// accept-or-deny exchange, no challenge round-trip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PeerHelloResponse {
    Accepted {
        /// Opaque token A uses on subsequent `mailbox.*` calls
        /// against B. v1 generates as 32 lowercase hex chars from
        /// `rand::random::<u128>()`.
        peer_handle: String,
        /// Allowed RPC method names — never wider than `want.method`.
        /// Phase 1 default policy emits `["mailbox.post"]` on accept.
        allowed_methods: Vec<String>,
        /// Wall-clock expiration of the handle. None = no expiry.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at_unix_ms: Option<u64>,
    },
    Denied {
        /// Human-readable reason; informational.
        reason: String,
    },
}

/// `mailbox.post` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailboxPostRequest {
    pub to_session_id: String,
    /// Peer handle A obtained from `peer.hello` against `to_session_id`.
    pub peer_handle: String,
    pub from: PeerOriginIdentity,
    pub kind: String,
    pub subject: String,
    pub body: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_unix_ms: Option<u64>,
}

impl MailboxPostRequest {
    pub const METHOD: &'static str = "mailbox.post";
}

/// Compact origin identity carried inside [`MailboxPostRequest`] and
/// the [`MailboxMessageEvent`] wire notification. Distinct from
/// [`PeerIdentity`] because it omits the capability advertisement
/// (which is `peer.hello`'s concern, not per-message).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerOriginIdentity {
    pub daemon_id: String,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailboxPostResponse {
    pub posted: bool,
    /// Per-target-session monotonic sequence number assigned at
    /// dispatch time. Stable identifier for `mailbox.consume`.
    pub seq: u64,
}

/// `mailbox.list` request — read the target session's mailbox.
/// `peer_handle` is `None` only for self-access (a session listing
/// its own mailbox); cross-session listing requires a handle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailboxListRequest {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_handle: Option<String>,
    /// Lower-bound filter — only messages with `seq >= since_seq` are
    /// returned. Use for incremental polling. None = return everything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_seq: Option<u64>,
    /// If false (default), `mailbox.consume`d entries are filtered out.
    /// If true, returns both consumed and un-consumed.
    #[serde(default)]
    pub include_consumed: bool,
}

impl MailboxListRequest {
    pub const METHOD: &'static str = "mailbox.list";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailboxListResponse {
    pub messages: Vec<MailboxMessageView>,
}

/// One mailbox message as seen by readers. Mirrors
/// [`crate::event_log::EventPayload::MailboxMessagePosted`] plus the
/// derived `consumed` projection. Field name is `kind` on the wire
/// (the EventPayload's `message_kind` was renamed only to avoid
/// colliding with the enum's `#[serde(tag = "kind")]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailboxMessageView {
    pub seq: u64,
    pub from_daemon_id: String,
    pub from_session_id: String,
    pub kind: String,
    pub subject: String,
    pub body: Value,
    pub posted_at_unix_ms: u64,
    pub consumed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_unix_ms: Option<u64>,
}

/// `mailbox.read` request — fetch the full body of a single message
/// by seq. Separates body retrieval from `mailbox.list` so list can
/// return metadata-only (subject, kind, seq, consumed) without
/// stuffing potentially large bodies into every list call.
/// Self-access doesn't require a handle; cross-session read does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailboxReadRequest {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_handle: Option<String>,
    pub seq: u64,
}

impl MailboxReadRequest {
    pub const METHOD: &'static str = "mailbox.read";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailboxReadResponse {
    pub message: Option<MailboxMessageView>,
}

/// `mailbox.consume` request — mark a list of `seq`s as consumed.
/// Idempotent: consuming an already-consumed seq is a no-op (not an
/// error). Consuming an unknown seq is silently skipped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailboxConsumeRequest {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_handle: Option<String>,
    pub seqs: Vec<u64>,
}

impl MailboxConsumeRequest {
    pub const METHOD: &'static str = "mailbox.consume";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailboxConsumeResponse {
    /// Number of seqs that transitioned from un-consumed to consumed.
    /// Already-consumed and unknown seqs do not contribute.
    pub consumed_count: u32,
}

/// `session.mailbox_message` wire notification — emitted by the
/// forwarder when a `MailboxMessagePosted` event lands in the
/// recipient's event log. Phase 4 TUI (F9 mailbox view) subscribes
/// to this for live updates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailboxMessageEvent {
    /// Recipient session — the one whose mailbox just gained the
    /// message.
    pub session_id: String,
    pub seq: u64,
    pub from_daemon_id: String,
    pub from_session_id: String,
    pub kind: String,
    pub subject: String,
    pub body: Value,
    pub posted_at_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_unix_ms: Option<u64>,
}

impl MailboxMessageEvent {
    pub const METHOD: &'static str = "session.mailbox_message";
}
