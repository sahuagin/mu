//! mesh → IRC routing.
//!
//! An offline decision function: given a capability-verified inbound DM and the
//! gateway's *current* view (discovered agents, observed membership, per-human
//! remembered-channel memory), it decides the one IRC output — a channel
//! delivery, a private delivery, a body-free notice, or a deliberate drop — and
//! frames it through [`crate::framing::frame_privmsg`]. It publishes nothing and
//! subscribes to nothing; the executor runs the returned decision.
//!
//! Two boundaries matter:
//!
//! - **Fail-closed ingress.** [`Router::accept`] runs the shared
//!   `mesh::verify_and_decode_dm` — the same issuer-signed `agent_dm` check the
//!   daemon uses, on *both* the endpoint and observer paths — before any body is
//!   held. A rejection is body-free ([`IngressRejected`]). That decoder is a
//!   decoder, not a policy: it bounds no field length, so the ingress also
//!   refuses an envelope whose remote-controlled `id`, `session` or destination
//!   subject exceeds this crate's documented caps ([`MAX_FIELD_LEN`],
//!   [`MAX_DESTINATION_LEN`]), before any routing logic sees it.
//! - **Exactly-once overlap.** The observer wildcard overlaps the gateway's own
//!   human endpoints, so one minted id can arrive twice. De-duplication keys on
//!   `(id, destination, session)`, never the id alone, so the overlap collapses
//!   to one delivery while genuinely distinct destinations that share a fan-out
//!   id are each delivered. What is RETAINED is a fixed-size blake3 digest of
//!   that tuple, not the tuple itself: the window bounds how many keys are held,
//!   and all three parts are remote text, so keeping them would have let the
//!   sender choose what "4096 entries" costs. Only an event that reaches a
//!   delivery decision consumes a slot of that bounded window: a drop costs
//!   nothing, so traffic to destinations that address nobody — which any
//!   `agent_dm` holder can mint at will — cannot evict the keys real deliveries
//!   depend on.
//! - **One framed output.** Every line handed to the executor is built by
//!   [`crate::framing::frame_privmsg`], the body-free notice included. The nick
//!   in that notice comes from a subject token and `PeerId::parse` is total, so
//!   the notice is exactly where unchecked remote text would otherwise reach the
//!   wire; it is held to the same target rules, control-character rejection and
//!   line budget as any delivery, and a nick that fails them makes the event a
//!   body-free drop rather than a notice.
//!
//! Privacy is enforced by making *current observed membership the sole
//! authority*: a private body is disclosed to a human only where that human is
//! observed present right now. An absent human gets a body-free notice, once per
//! withdrawal, never the body — and that per-withdrawal suppression is itself a
//! bounded window ([`crate::recent`]), since who is addressed is the sender's
//! choice, not the gateway's. A destination that names the `human` role but no
//! nick addresses nobody and is dropped body-free — it is not an agent, and
//! routing it as one would put a human-addressed body in the public lobby.
//!
//! Agent presence is decided by exact peer-id membership in the discovered set.
//! Folded channel collisions are a separate question, used only to decide
//! whether a body delivered to a PRESENT agent needs a disambiguating label.

use std::collections::HashMap;

use biscuit_auth::PublicKey;
use mu_peer::{PeerId, MESH_SUBJECT_PREFIX, ROLE_HUMAN};

use mu_dialogue::mesh::{verify_and_decode_dm, DmRejected, MeshDmEvent, Reception};

use crate::framing::{frame_privmsg, validate_target, FrameParams, FramingError};
use crate::mapping::{channel_for, fold_nick, CaseMapping};
use crate::membership::Membership;
use crate::recent::RecentSet;

/// The largest `id` or `session` the ingress accepts, in bytes.
///
/// Both are remote-controlled and neither is bounded by
/// `mesh::verify_and_decode_dm`, which decodes and verifies a capability — it
/// is not a size policy. The gateway retains a fingerprint of both in a bounded
/// window and puts the id on the wire as an IRCv3 tag value, so the cap belongs
/// here, at this crate's own ingress, rather than in the shared decoder. 256
/// bytes is far above every id the mesh actually mints (a ULID is 26
/// characters, a session uuid 36) and far below a size worth holding.
pub const MAX_FIELD_LEN: usize = 256;

/// The largest destination subject the ingress accepts, in bytes.
///
/// NATS fixes no maximum subject length; what bounds one in practice is the
/// server's `max_control_line` (4096 bytes by default), which has to hold the
/// whole `MSG <subject> <sid> <bytes>` protocol line. 1024 bytes leaves ample
/// room for the rest of that line while sitting orders of magnitude above any
/// subject [`PeerId::dm_subject`] produces, so a destination past it did not
/// come from the mesh's own subject derivation.
pub const MAX_DESTINATION_LEN: usize = 1024;

/// Why the gateway's ingress refused a payload.
///
/// Body-free by construction, exactly like [`DmRejected`] itself: it names the
/// failure class and, for a size refusal, which field was oversized — never any
/// content, so a diagnostic built from it cannot leak a refused DM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngressRejected {
    /// The shared fail-closed `mesh::verify_and_decode_dm` refused it.
    Unverified(DmRejected),
    /// It verified, but a remote-controlled field was longer than this crate's
    /// documented cap ([`MAX_FIELD_LEN`], [`MAX_DESTINATION_LEN`]), so it is
    /// refused here and never reaches routing.
    Oversized(OversizedField),
}

/// Which remote-controlled field made an envelope an oversized ingress
/// rejection. Names the field, never its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OversizedField {
    /// The subject the payload arrived on.
    Destination,
    /// The envelope's mesh message id.
    Id,
    /// The envelope's target session.
    Session,
}

impl std::fmt::Display for OversizedField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            OversizedField::Destination => "destination",
            OversizedField::Id => "id",
            OversizedField::Session => "session",
        })
    }
}

impl std::fmt::Display for IngressRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately body-free — safe to log verbatim.
        match self {
            IngressRejected::Unverified(why) => write!(f, "{why}"),
            IngressRejected::Oversized(field) => {
                write!(f, "oversized dm {field} (refused before routing)")
            }
        }
    }
}

/// The gateway's current view routing reads. All of it is live and disposable —
/// the discovered-peer snapshot, the observed membership, and the per-human
/// remembered channel — so a decision reflects the mesh and IRC as they are now,
/// never a stored roster.
pub struct RouteEnv<'a> {
    /// Agents discovered on the mesh right now (for presence + collision).
    pub peers: &'a [PeerId],
    /// Observed channel membership — the sole authority on human presence.
    pub membership: &'a Membership,
    /// Folded human nick → the folded channel that human was last addressed in
    /// (routing memory owned by the IRC→mesh side; read-only here).
    pub remembered: &'a HashMap<String, String>,
    pub prefix: &'a str,
    pub lobby: &'a str,
    pub channellen: usize,
    pub cm: CaseMapping,
    /// Whether `message-tags` is negotiated — the `+mu.id` tag rides only then.
    pub message_tags: bool,
}

/// What routing decided for one inbound DM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteDecision {
    /// Send these framed PRIVMSG lines to `target` (a channel or a nick).
    Deliver { target: String, lines: Vec<String> },
    /// Send this single body-free notice line to `target` (the lobby): a DM
    /// arrived for a human who is not currently observed on IRC, so its body is
    /// withheld. `line` is a COMPLETE framed line ending in `\r\n`, built the
    /// same way [`RouteDecision::Deliver`] lines are — a notice is not a
    /// second, unchecked way onto the wire.
    Notice { target: String, line: String },
    /// Nothing is delivered, with why.
    Drop(DropReason),
}

/// Why a routing decision delivered nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// The endpoint/observer overlap already delivered this `(id, destination,
    /// session)`.
    Duplicate,
    /// A body-free notice for this absent human was already sent this
    /// withdrawal; it is not repeated until the human returns.
    SuppressedNotice,
    /// The destination subject did not resolve to a routable peer.
    Unroutable,
    /// The destination named the `human` role but no usable nick: none at all,
    /// or one that cannot appear on an IRC line (a control character, a target
    /// spelling that would change what the line means, or text long enough that
    /// a body-free notice would not fit one line). It addresses no one, and it
    /// is emphatically NOT an agent: routing it as one would put a private body
    /// in the public lobby. Dropped body-free.
    MalformedHumanDestination,
    /// The body could not be framed as a safe PRIVMSG even untagged (e.g. it
    /// carried a raw control character). No body is disclosed.
    Unframable,
}

/// The mesh→IRC router. Holds only the disposable per-connection state routing
/// owns itself: the verifier's issuer key, the overlap-dedup set, and the
/// per-human absent-notice suppression.
pub struct Router {
    issuer: PublicKey,
    /// `(id, destination, session)` already delivered — the exactly-once key,
    /// held in a bounded window rather than for the life of the connection. See
    /// [`crate::recent`]: the overlap this collapses is two receptions of one
    /// dispatch, so a window sized far above that gap is all the guarantee
    /// needs, and it is what keeps a healthy long-lived connection from growing
    /// this set with total traffic.
    ///
    /// What is stored is [`dedup_key`]'s 32-byte digest of that tuple, not the
    /// tuple: the window bounds the NUMBER of entries, and all three parts are
    /// remote text, so retaining them (twice, per [`RecentSet`]) made the byte
    /// cost of a full window the sender's choice rather than the gateway's.
    seen: RecentSet<[u8; 32]>,
    /// Folded human nicks a body-free notice has already been sent for this
    /// withdrawal, in the SAME bounded window as `seen` and for the same reason:
    /// the sender chooses which nicks to address, so a set that only ever grew
    /// until each of those humans happened to appear would grow with traffic
    /// from anyone holding the generic `agent_dm` capability. The window keeps
    /// the most recent [`crate::recent::DEFAULT_CAPACITY`] nicks and evicts the
    /// oldest; an evicted nick's next DM merely notifies once more, which is the
    /// safe direction to fail — the body is never disclosed either way.
    notified: RecentSet<String>,
}

impl Router {
    /// A router verifying against `issuer`.
    pub fn new(issuer: PublicKey) -> Self {
        Router {
            issuer,
            seen: RecentSet::default(),
            notified: RecentSet::default(),
        }
    }

    /// The fail-closed ingress: verify a raw payload that arrived on `destination`
    /// via `reception`, returning a verified event or a body-free rejection. This
    /// is the ONLY gate; a successful subscription is not authorization.
    ///
    /// Verification is unchanged — the SAME `mesh::verify_and_decode_dm` the
    /// daemon runs. What is added here is the size policy that decoder
    /// deliberately does not have: `destination`, `id` and `session` are all
    /// remote-controlled and unbounded on the wire, and routing goes on to
    /// fingerprint them into a bounded window and put the id on an IRC line. A
    /// field past this crate's documented cap is refused HERE, so an oversized
    /// envelope never reaches routing at all.
    ///
    /// The destination is checked before the payload is even decoded: it is the
    /// one field known without decoding, and one longer than
    /// [`MAX_DESTINATION_LEN`] cannot have come from [`PeerId::dm_subject`].
    pub fn accept(
        &self,
        destination: &str,
        payload: &[u8],
        reception: Reception,
    ) -> Result<MeshDmEvent, IngressRejected> {
        if destination.len() > MAX_DESTINATION_LEN {
            return Err(IngressRejected::Oversized(OversizedField::Destination));
        }
        let ev = verify_and_decode_dm(self.issuer, destination, payload, reception)
            .map_err(IngressRejected::Unverified)?;
        if ev.id.len() > MAX_FIELD_LEN {
            return Err(IngressRejected::Oversized(OversizedField::Id));
        }
        if ev
            .session
            .as_deref()
            .is_some_and(|session| session.len() > MAX_FIELD_LEN)
        {
            return Err(IngressRejected::Oversized(OversizedField::Session));
        }
        Ok(ev)
    }

    /// A human just became present again: clear any absent-notice suppression so
    /// the next withdrawal is diagnosed afresh (withdrawal-scoped).
    pub fn human_returned(&mut self, nick: &str, cm: CaseMapping) {
        // An explicit removal, not an aging-out: their return is the event that
        // ends this withdrawal, whatever the window would have done next.
        self.notified.remove(&fold_nick(nick, cm));
    }

    /// Reset the disposable per-connection state on reconnect. The issuer key
    /// stays; the overlap and notice state do not.
    pub fn reset(&mut self) {
        self.seen.clear();
        self.notified.clear();
    }

    /// Decide the IRC output for one verified inbound DM against the live view.
    ///
    /// A slot of the bounded exactly-once window is spent only by an event that
    /// reached a delivery decision. Spending one before the destination was even
    /// validated meant a stream of subjects that address nobody — free to mint
    /// for anyone the generic `agent_dm` capability authorizes — pushed the keys
    /// of real deliveries out of the window, re-opening for that traffic exactly
    /// the endpoint/observer overlap this set exists to collapse.
    pub fn route(&mut self, ev: &MeshDmEvent, env: &RouteEnv) -> RouteDecision {
        // Exactly-once overlap: key on (id, destination, session), never id
        // alone, so an endpoint+observer overlap collapses while distinct
        // destinations sharing a fan-out id each deliver. The key is a digest,
        // so what the window retains is 32 bytes per entry whatever the sender
        // put in those three fields.
        let key = dedup_key(ev);
        if self.seen.contains(&key) {
            return RouteDecision::Drop(DropReason::Duplicate);
        }
        // Deciding and recording are one step from any caller's view — `route`
        // takes `&mut self` — so nothing can slip between them and deliver the
        // same key twice.
        let decision = self.decide(ev, env);
        if !matches!(decision, RouteDecision::Drop(_)) {
            self.seen.record(key);
        }
        decision
    }

    /// The decision itself, with de-duplication already settled by
    /// [`Router::route`] — which is also what decides whether this outcome is
    /// worth a window slot, so this returns a decision and records nothing.
    fn decide(&mut self, ev: &MeshDmEvent, env: &RouteEnv) -> RouteDecision {
        let target = match peer_from_subject(&ev.destination) {
            Ok(target) => target,
            Err(reason) => return RouteDecision::Drop(reason),
        };
        // The ROLE decides which half of the policy applies, not whether a nick
        // happened to parse. A `human` destination carrying no nick names
        // nobody; falling through to agent routing put its body in the lobby,
        // which is the one place a human-addressed body must never go.
        if target.is_human() {
            let Some(nick) = target.human_nick() else {
                // `peer_from_subject` already refuses this shape; the branch
                // that depends on the invariant still states it.
                return RouteDecision::Drop(DropReason::MalformedHumanDestination);
            };
            let nick = nick.to_string();
            return self.route_to_human(&nick, ev, env);
        }
        self.route_to_agent(&target, ev, env)
    }

    /// A DM addressed to an agent is mirrored into that agent's channel when the
    /// agent is present, labelled when the channel collides, and dropped to the
    /// lobby (labelled with the intended target) when the agent is absent.
    fn route_to_agent(&self, peer: &PeerId, ev: &MeshDmEvent, env: &RouteEnv) -> RouteDecision {
        let Some(channel) = channel_for(peer, env.prefix, env.channellen) else {
            // `channel_for` declines only human peers, and those never reach
            // here — but if one ever did, it would be a destination with no
            // channel, which is a drop, not a lobby disclosure.
            return RouteDecision::Drop(DropReason::MalformedHumanDestination);
        };
        // PRESENCE is exact-peer-id membership in the discovered set. Channel
        // occupancy is a different question: two peers can fold to one channel,
        // so "somebody's channel folds to this name" says nothing about whether
        // THIS peer is on the mesh. Conflating them delivered a DM for an absent
        // `cc:abc` into `#cc-abc` unlabelled, silently attributing it to the
        // present `cc:ABC`.
        if !env.peers.iter().any(|p| p == peer) {
            return self.deliver_to_lobby(peer, ev, env);
        }
        // The target IS present. Collision counting now decides only one thing:
        // whether the channel is shared, so the operator needs a label to tell
        // which of the colliding peers this body is from.
        let folded_ch = fold_nick(&channel, env.cm);
        let colliding = env
            .peers
            .iter()
            .filter(|p| {
                channel_for(p, env.prefix, env.channellen)
                    .is_some_and(|c| fold_nick(&c, env.cm) == folded_ch)
            })
            .count();
        let body = if colliding > 1 {
            format!("[{peer}] {}", ev.body)
        } else {
            ev.body.clone()
        };
        self.frame_to(&channel, &body, ev, env)
    }

    /// Mirror an ABSENT agent's DM to the lobby, labelled with the peer it was
    /// for so the operator still sees who it was addressed to — including when
    /// a present peer's channel folds to the same name, which is exactly the
    /// case a bare channel delivery would misattribute.
    fn deliver_to_lobby(&self, peer: &PeerId, ev: &MeshDmEvent, env: &RouteEnv) -> RouteDecision {
        let body = format!("[\u{2192} {peer}] {}", ev.body);
        self.frame_to(env.lobby, &body, ev, env)
    }

    /// A DM addressed to a human. Delivery is exclusive and precedence-ordered
    /// against CURRENT observed membership: a remembered channel the human is
    /// still in, else a private message to their nick, else — when the human is
    /// not observed present at all — a body-free notice sent once per withdrawal.
    fn route_to_human(&mut self, nick: &str, ev: &MeshDmEvent, env: &RouteEnv) -> RouteDecision {
        // The nick is remote text: it arrives as a NATS subject token and
        // `PeerId::parse` is deliberately total, so nothing upstream has held it
        // to what an IRC line may carry. Both routes out of here put it on the
        // wire — as the PRIVMSG target when the human is present, inside the
        // notice line when they are not — so it faces the framing module's
        // target rules once, here, ahead of either.
        if validate_target(nick).is_err() {
            return RouteDecision::Drop(DropReason::MalformedHumanDestination);
        }
        let folded = fold_nick(nick, env.cm);
        if env.membership.is_present(nick) {
            // Present: their body may be disclosed. Clear stale suppression.
            self.notified.remove(&folded);
            // Remembered-channel precedence — only if they are STILL in it.
            if let Some(remembered) = env.remembered.get(&folded) {
                if env
                    .membership
                    .channels_of(nick)
                    .iter()
                    .any(|c| c == remembered)
                {
                    let target = env
                        .membership
                        .channel_display(remembered)
                        .unwrap_or(remembered)
                        .to_string();
                    return self.frame_to(&target, &ev.body, ev, env);
                }
            }
            // Private precedence.
            let target = env
                .membership
                .display_nick(nick)
                .unwrap_or_else(|| nick.to_string());
            return self.frame_to(&target, &ev.body, ev, env);
        }
        // Absent: never disclose the body. One body-free notice per withdrawal,
        // framed before the withdrawal is marked as notified — a nick that
        // cannot be framed must not consume the one notice this withdrawal gets.
        let Some(line) = notice_line(nick, env) else {
            return RouteDecision::Drop(DropReason::MalformedHumanDestination);
        };
        if !self.notified.record(folded) {
            return RouteDecision::Drop(DropReason::SuppressedNotice);
        }
        RouteDecision::Notice {
            target: env.lobby.to_string(),
            line,
        }
    }

    /// Frame a body to `target` with the negotiated tag policy. An id that cannot
    /// ride an IRCv3 tag falls back to an untagged delivery (the framing
    /// contract's documented fallback) rather than dropping the message; a body
    /// that cannot be framed at all is dropped body-free.
    fn frame_to(
        &self,
        target: &str,
        body: &str,
        ev: &MeshDmEvent,
        env: &RouteEnv,
    ) -> RouteDecision {
        let params = FrameParams {
            target,
            mesh_id: Some(&ev.id),
            message_tags: env.message_tags,
        };
        match frame_privmsg(&params, body) {
            Ok(lines) => RouteDecision::Deliver {
                target: target.to_string(),
                lines,
            },
            Err(FramingError::UnsafeMeshId) => {
                let untagged = FrameParams {
                    target,
                    mesh_id: None,
                    message_tags: false,
                };
                match frame_privmsg(&untagged, body) {
                    Ok(lines) => RouteDecision::Deliver {
                        target: target.to_string(),
                        lines,
                    },
                    Err(_) => RouteDecision::Drop(DropReason::Unframable),
                }
            }
            Err(_) => RouteDecision::Drop(DropReason::Unframable),
        }
    }
}

/// The single body-free line an absent-human notice sends, or `None` when it
/// cannot be one safe line.
///
/// It is built through [`frame_privmsg`] exactly like a delivery — same target
/// rules, same control-character rejection, same 512-byte budget — because the
/// nick in it is remote text and this is the one outbound line that carries no
/// body to hide behind. A notice says only that something arrived, so it is ONE
/// line by definition: a nick long enough to split it into continuations is
/// refused rather than smeared across the lobby. It carries no `+mu.id` tag; the
/// point of the notice is that nothing about the message is disclosed.
fn notice_line(nick: &str, env: &RouteEnv) -> Option<String> {
    let params = FrameParams {
        target: env.lobby,
        mesh_id: None,
        message_tags: false,
    };
    let body = format!("[mu] a direct message arrived for {nick} (not currently present)");
    let mut lines = frame_privmsg(&params, &body).ok()?;
    if lines.len() != 1 {
        return None;
    }
    lines.pop()
}

/// The domain-separation context the exactly-once fingerprint is computed
/// under, so a digest from this window is never the same value as some other
/// blake3 digest of the same bytes elsewhere in the gateway (the channel-tail
/// hash in [`crate::mapping`] is the other one).
const DEDUP_CONTEXT: &[u8] = b"mu-irc-gateway/routing/exactly-once/v1";

/// The fixed-size exactly-once key for one event: a 32-byte blake3 digest of
/// `(id, destination, session)`.
///
/// The window ([`crate::recent`]) bounds how MANY keys are held, not how big
/// one is, and it holds each key twice (order + set). All three parts are
/// remote text that `mesh::verify_and_decode_dm` does not bound, so retaining
/// the tuple itself made the byte cost of a full window the sender's choice.
/// A digest makes that cost fixed and independent of the ingress caps.
///
/// Each part is absorbed LENGTH-PREFIXED, not merely separated: a separator
/// byte only works if no part can contain it, and remote text can contain
/// anything, so `("a", "bc", …)` and `("ab", "c", …)` must differ by
/// construction rather than by luck. `session` absorbs a present/absent tag
/// ahead of its bytes, so `None` and `Some("")` are different keys.
fn dedup_key(ev: &MeshDmEvent) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    absorb(&mut hasher, DEDUP_CONTEXT);
    absorb(&mut hasher, ev.id.as_bytes());
    absorb(&mut hasher, ev.destination.as_bytes());
    match ev.session.as_deref() {
        Some(session) => {
            hasher.update(&[1]);
            absorb(&mut hasher, session.as_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
    *hasher.finalize().as_bytes()
}

/// Absorb one length-prefixed part into a fingerprint (see [`dedup_key`]).
fn absorb(hasher: &mut blake3::Hasher, part: &[u8]) {
    hasher.update(&(part.len() as u64).to_le_bytes());
    hasher.update(part);
}

/// Whether one subject component is a component the mesh's own subject
/// derivation could have produced AND that the `role:id[:sub]` re-spelling in
/// [`peer_from_subject`] hands back unchanged.
///
/// Two separate grammars meet here. [`PeerId::dm_subject`] joins the levels
/// with `.` and emits each one VERBATIM; [`PeerId::parse`] splits on `:`. A
/// component carrying a `:` therefore survives the subject unscathed and is
/// then re-split into two levels — `mu.agent.human.alice:bob.dm` resolved to
/// the human `alice`, delivering a body addressed to a destination that does
/// not exist to a real person who is merely observed. So a `:` in any component
/// is refused here, before the re-spelling, rather than silently reinterpreted.
///
/// Whitespace and control characters go with it: a NATS subject cannot contain
/// either (the protocol line is space-delimited), so `dm_subject` could never
/// have produced such a component and nothing downstream should have to cope
/// with one. `.` is NOT refused — the sub-level legitimately keeps the rest of
/// the subject, which is how `mu:daemon:a.b` round-trips.
fn addressable_component(component: &str) -> bool {
    !component.is_empty()
        && !component.contains(':')
        && !component.contains(|c: char| c.is_whitespace() || c.is_control())
}

/// Resolve a mesh DM subject (`mu.agent.<role>[.<id>[.<sub>]].dm`) back to the
/// peer it addresses — the inverse of [`PeerId::dm_subject`] — or the typed drop
/// reason for a subject that addresses no one.
///
/// EVERY component is checked, not just the core, and it is checked BEFORE the
/// levels are re-spelled as `role:id[:sub]` for [`PeerId::parse`]. `PeerId::parse`
/// is total by design (the peers table has to round-trip whatever ids clients
/// have ever presented), so nothing below this refuses `mu.agent.cc..dm`: it
/// would mint the peer `cc:` and, from that, the degenerate channel `#cc-`. A
/// component with nothing in it names nothing, so it is refused here instead.
///
/// Non-empty is not enough, because the re-spelling changes delimiters: the
/// subject joins levels with `.` while `PeerId::parse` splits on `:`, so a
/// component that itself contains a `:` came through the subject intact and was
/// then re-split into two different levels — a destination silently becoming a
/// DIFFERENT peer, which is a misdelivery, not a parse detail. Every component
/// is therefore held to [`addressable_component`] first, and only then spelled;
/// after that check the round trip is exact.
///
/// The `human` role is decided BEFORE those checks, so a human destination
/// carrying no nick — or one whose nick is not a component the mesh could have
/// addressed — keeps its own reason rather than being folded into the generic
/// one: it is not merely unroutable, it is the shape that must never be
/// re-routed as an agent.
fn peer_from_subject(subject: &str) -> Result<PeerId, DropReason> {
    let Some(core) = subject
        .strip_prefix(MESH_SUBJECT_PREFIX)
        .and_then(|core| core.strip_suffix(".dm"))
    else {
        return Err(DropReason::Unroutable);
    };
    let mut parts = core.splitn(3, '.');
    let role = parts.next().unwrap_or_default();
    let id = parts.next();
    let sub = parts.next();
    if role == ROLE_HUMAN && id.is_none_or(str::is_empty) {
        return Err(DropReason::MalformedHumanDestination);
    }
    if role.is_empty() || id.is_some_and(str::is_empty) || sub.is_some_and(str::is_empty) {
        return Err(DropReason::Unroutable);
    }
    // Delimiter fidelity, checked before the re-spelling below (see
    // `addressable_component`). A `human` destination keeps its own reason: a
    // nick the mesh could not have addressed still names no one, and this is
    // the shape that must never fall through to agent routing.
    let unaddressable = |component: &str| !addressable_component(component);
    if unaddressable(role) || id.is_some_and(unaddressable) || sub.is_some_and(unaddressable) {
        return Err(if role == ROLE_HUMAN {
            DropReason::MalformedHumanDestination
        } else {
            DropReason::Unroutable
        });
    }
    let spelled = match (id, sub) {
        (None, _) => role.to_string(),
        (Some(id), None) => format!("{role}:{id}"),
        (Some(id), Some(sub)) => format!("{role}:{id}:{sub}"),
    };
    Ok(PeerId::parse(&spelled))
}
