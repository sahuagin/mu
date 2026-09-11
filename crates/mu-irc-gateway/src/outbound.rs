//! IRC → mesh routing (increment 3).
//!
//! An offline decision function for the other direction: a human types a line in
//! IRC, and this decides the mesh DM(s) to publish — to one explicitly-addressed
//! agent, to the agent a channel maps to, or a fan-out across every discovered
//! agent for the lobby / a private line to the gateway. It publishes nothing;
//! the executor runs the returned [`OutboundDecision`] against the mesh, using
//! the ONE caller-supplied minted id for a whole fan-out.
//!
//! Two loop guards live here, because a gateway that mirrors both directions can
//! echo its own traffic:
//!
//! - **Own nick.** A line whose sender folds to the gateway's own nick is the
//!   gateway's own output looping back through the channel; it is dropped.
//! - **Own publications.** Every id this side mints is recorded *before* the
//!   decision is returned — i.e. before the executor can publish and the observer
//!   can see it — so [`Outbound::is_own_echo`] recognises the gateway's own DM
//!   (by minted id, or by its `human:` sender) when it comes back around and the
//!   mesh→IRC side suppresses it. Recording before the return is what makes the
//!   publish-then-observe race safe.
//!
//! Sender authorization happens ONCE, before any destination logic. The gateway
//! is about to assert `human:<nick>` to the mesh under its own capability, so it
//! must be able to vouch for that identity — and that is true whatever
//! destination the line names. Checking it per-destination meant an explicit
//! `cc:abc:` prefix selected a path that never checked, so a prefix defeated the
//! gate that the same sender's unprefixed line ran into.
//!
//! Routing memory is updated only for *specifically addressed* lines (an explicit
//! address, or a message to a specific agent's channel) — never a lobby or
//! private fan-out — so the mesh→IRC side's remembered-channel precedence
//! reflects real, directed conversations.
//!
//! What a directed line remembers is **where the human typed it**, not where the
//! agent lives. A line in the peer's own channel is a conversation happening in
//! that channel, so the reply belongs there. The same address typed in the lobby,
//! in some other agent's channel, or privately to the gateway is not: the human
//! may not even be in the peer's channel, and remembering it would send the reply
//! to a room they never asked about — so those record
//! [`MemoryDestination::Private`] and the reply comes back as a DM.

use mu_peer::PeerId;

use mu_dialogue::mesh::MeshDmEvent;

use crate::mapping::{channel_for, fold_nick, resolve_channel, CaseMapping, Resolved, SelfNick};
use crate::membership::Membership;
use crate::recent::RecentSet;

/// The gateway's current view the outbound decision reads: discovered agents
/// (for fan-out and destination checks) and observed membership (to authorize a
/// private sender). Live and disposable.
pub struct OutEnv<'a> {
    pub peers: &'a [PeerId],
    pub membership: &'a Membership,
}

/// A routing-memory write the executor should apply: where this human's next
/// inbound DM belongs, as decided by the line they just typed. Folded keys,
/// ready to key the mesh→IRC `remembered` map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryUpdate {
    /// Folded human nick.
    pub human: String,
    /// Where a reply to this human belongs now.
    pub destination: MemoryDestination,
}

/// Where a directed line says a reply to that human belongs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryDestination {
    /// This folded channel: the human spoke to the agent *in* it, so the
    /// conversation is happening there and the answer is part of it.
    Channel(String),
    /// Privately. The human addressed the agent from somewhere that is not the
    /// agent's own channel, so there is no channel the reply is part of — and
    /// this is a WRITE, not the absence of one: an earlier in-channel line must
    /// not keep steering replies into a room this line did not name.
    Private,
}

/// What the outbound side decided for one IRC line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboundDecision {
    /// Publish `body` from `from` to every peer in `targets`, all sharing the one
    /// minted `id`. `memory` is applied only for a specifically-addressed line.
    Publish {
        id: String,
        from: PeerId,
        targets: Vec<PeerId>,
        body: String,
        memory: Option<MemoryUpdate>,
    },
    /// Refuse and tell the operator why (the reason names peers, never a body).
    Refuse(RefuseReason),
    /// Drop silently — the gateway's own echo, or a disconnected transport.
    Drop(OutDrop),
}

/// Why an outbound line was refused, phrased for the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefuseReason {
    /// The target is (or resolves to) a human; humans are not agent destinations.
    HumanDestination,
    /// An explicit address that names no currently-discovered peer.
    AbsentDestination(PeerId),
    /// A channel that case-folds onto more than one peer; neither is chosen.
    AmbiguousChannel(Vec<PeerId>),
    /// A channel target that maps to no current peer and is not the lobby.
    UnknownChannel(String),
    /// A line from a nick the gateway does not observe in any shared channel.
    /// Its identity cannot be authorized, so no `human:<nick>` is asserted on
    /// the mesh on its behalf — whatever destination the line named.
    UnauthorizedSender(String),
    /// A fan-out with no agents discovered to send to.
    NoDestinations,
}

/// Why an outbound line was dropped silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutDrop {
    /// The sender is the gateway's own nick (loop guard 1).
    OwnNick,
    /// The transport is not currently connected.
    Disconnected,
}

/// The IRC→mesh decision maker and the home of both loop guards.
pub struct Outbound {
    /// The gateway's own nick, kept in its wire spelling with the folded form
    /// derived. Re-folding an already-folded nick is lossy across a
    /// `CASEMAPPING` change (see [`SelfNick`]), and getting it wrong here breaks
    /// loop guard 1 — the gateway stops recognizing its own echoed output.
    self_nick: SelfNick,
    cm: CaseMapping,
    prefix: String,
    lobby_folded: String,
    channellen: usize,
    /// Whether the transport is connected; a line while disconnected is dropped.
    connected: bool,
    /// Ids this side has minted — loop guard 2. Recorded before a `Publish` is
    /// returned, so an observation can never precede the record. Held in a
    /// bounded window ([`crate::recent`], the same policy the mesh→IRC overlap
    /// dedup uses): the echo it guards against comes back within a network round
    /// trip, so a window covers it without growing with lifetime traffic.
    /// Disposable: cleared on reconnect.
    minted: RecentSet<String>,
}

impl Outbound {
    /// A decision maker for a gateway registered as `self_nick`, folding under
    /// `cm`, with the configured channel `prefix`, `lobby`, and `channellen`.
    pub fn new(
        self_nick: &str,
        cm: CaseMapping,
        prefix: &str,
        lobby: &str,
        channellen: usize,
    ) -> Self {
        Outbound {
            self_nick: SelfNick::new(self_nick, cm),
            cm,
            prefix: prefix.to_string(),
            lobby_folded: fold_nick(lobby, cm),
            channellen,
            connected: true,
            minted: RecentSet::default(),
        }
    }

    /// Update the fold rule after a live CASEMAPPING change. The self nick is
    /// re-DERIVED from its wire spelling and the lobby from the fresh argument;
    /// neither is re-folded from a value that was already folded under the old
    /// rule, because folding is lossy and would silently change who the gateway
    /// thinks it is.
    pub fn set_casemapping(&mut self, cm: CaseMapping, lobby: &str) {
        self.cm = cm;
        self.self_nick.set_casemapping(cm);
        self.lobby_folded = fold_nick(lobby, cm);
    }

    /// A live CHANNELLEN change (an `005` after registration, including
    /// `-CHANNELLEN`): the adapter tracks it and mesh→IRC routing reads it
    /// fresh per call, so this side must follow too, or channel resolution
    /// and the routing-memory channel names drift from what the server now
    /// derives for long peer ids.
    pub fn set_channellen(&mut self, channellen: usize) {
        self.channellen = channellen;
    }

    /// The gateway renamed itself: track the new WIRE nick so loop guard 1 keeps
    /// matching, and so a later casemapping change re-derives from it.
    pub fn set_self_nick(&mut self, nick: &str) {
        self.self_nick.rename(nick, self.cm);
    }

    /// The gateway's own nick as the server spells it.
    pub fn self_nick(&self) -> &str {
        self.self_nick.original()
    }

    /// Mark the transport connected or not. A line received while disconnected is
    /// dropped rather than published — the gateway is a live mirror, never a
    /// queue.
    pub fn set_connected(&mut self, connected: bool) {
        self.connected = connected;
    }

    /// Forget the disposable minted-id set on reconnect. The self nick and config
    /// stay.
    pub fn reset(&mut self) {
        self.minted.clear();
    }

    /// Loop guard 2, for the mesh→IRC side: whether `ev` is the gateway's own
    /// publication coming back around — either an id this side minted, or a
    /// `human:` sender (a human never originates a mesh DM except through this
    /// gateway). Such an event must not be mirrored back to IRC.
    pub fn is_own_echo(&self, ev: &MeshDmEvent) -> bool {
        self.minted.contains(&ev.id) || PeerId::parse(&ev.from).is_human()
    }

    /// Decide the mesh output for one IRC line: `sender` wrote `text` to `target`
    /// (a channel, or the gateway's own nick for a private line). `minted_id` is
    /// the single id the caller has minted for this line's fan-out; it is
    /// recorded for loop-guarding before any `Publish` is returned.
    pub fn route_line(
        &mut self,
        sender: &str,
        target: &str,
        text: &str,
        minted_id: &str,
        env: &OutEnv,
    ) -> OutboundDecision {
        if !self.connected {
            return OutboundDecision::Drop(OutDrop::Disconnected);
        }
        // Loop guard 1: our own line echoed back through a channel.
        if fold_nick(sender, self.cm) == self.self_nick.folded() {
            return OutboundDecision::Drop(OutDrop::OwnNick);
        }
        // Sender authorization, once, BEFORE any destination logic. The mesh
        // publish that follows asserts `human:<nick>` under the gateway's own
        // capability, so the gateway must observe that identity first —
        // regardless of which destination the line names. Checking this only on
        // the private path let a `cc:abc:` prefix route around it.
        if !env.membership.is_present(sender) {
            return OutboundDecision::Refuse(RefuseReason::UnauthorizedSender(sender.to_string()));
        }
        let from = PeerId::human(fold_nick(sender, self.cm));

        // An explicit `role:id: body` address overrides the channel/target.
        if let Some((addr, body)) = parse_explicit(text) {
            match classify_address(addr) {
                Address::Human => return OutboundDecision::Refuse(RefuseReason::HumanDestination),
                Address::Agent(peer) => {
                    return self.publish_explicit(from, peer, target, body, minted_id, env);
                }
                // Not an address at all (ordinary text that happens to hold a
                // colon): fall through to channel/private handling.
                Address::Ordinary => {}
            }
        }

        if fold_nick(target, self.cm) == self.self_nick.folded() {
            // A private line to the gateway is a fan-out to every discovered
            // agent, with no routing-memory change (it is not specifically
            // addressed). The sender was authorized above.
            return self.fan_out(from, text, minted_id, env);
        }
        self.publish_channel(target, from, text, minted_id, env)
    }

    /// Publish an explicitly-addressed line to one currently-discovered agent,
    /// remembering where the reply belongs — which is decided by `source`, the
    /// IRC target the human typed the line to, not by where the agent lives.
    ///
    /// Addressing `cc:abc` *in* `#cc-abc` is a conversation in that channel and
    /// the reply is part of it. Addressing it from the lobby, from another
    /// agent's channel, or in a private line to the gateway is not: the human
    /// is very likely not in `#cc-abc` at all, so remembering the agent's
    /// channel would aim the reply at a room they never joined and it would
    /// fall through to a DM anyway — with a stale memory left behind to
    /// mis-route the next one. Those record [`MemoryDestination::Private`].
    fn publish_explicit(
        &mut self,
        from: PeerId,
        peer: PeerId,
        source: &str,
        body: &str,
        minted_id: &str,
        env: &OutEnv,
    ) -> OutboundDecision {
        if !discovered(env.peers, &peer) {
            return OutboundDecision::Refuse(RefuseReason::AbsentDestination(peer));
        }
        let source_folded = fold_nick(source, self.cm);
        let in_peers_own_channel = channel_for(&peer, &self.prefix, self.channellen)
            .is_some_and(|ch| fold_nick(&ch, self.cm) == source_folded);
        let destination = if in_peers_own_channel {
            MemoryDestination::Channel(source_folded)
        } else {
            MemoryDestination::Private
        };
        let memory = Some(MemoryUpdate {
            human: human_key(&from),
            destination,
        });
        self.mint(minted_id);
        OutboundDecision::Publish {
            id: minted_id.to_string(),
            from,
            targets: vec![peer],
            body: body.to_string(),
            memory,
        }
    }

    /// A line to a channel: the lobby fans out; a channel that resolves to one
    /// present agent is a directed publish (and updates memory); ambiguity and
    /// unknown channels are refused.
    fn publish_channel(
        &mut self,
        target: &str,
        from: PeerId,
        text: &str,
        minted_id: &str,
        env: &OutEnv,
    ) -> OutboundDecision {
        if fold_nick(target, self.cm) == self.lobby_folded {
            return self.fan_out(from, text, minted_id, env);
        }
        match resolve_channel(target, env.peers, &self.prefix, self.channellen, self.cm) {
            Resolved::Unknown => {
                OutboundDecision::Refuse(RefuseReason::UnknownChannel(target.to_string()))
            }
            Resolved::Ambiguous(peers) => {
                OutboundDecision::Refuse(RefuseReason::AmbiguousChannel(peers))
            }
            Resolved::Peer(peer) => {
                if peer.is_human() {
                    return OutboundDecision::Refuse(RefuseReason::HumanDestination);
                }
                let memory = Some(MemoryUpdate {
                    human: human_key(&from),
                    destination: MemoryDestination::Channel(fold_nick(target, self.cm)),
                });
                self.mint(minted_id);
                OutboundDecision::Publish {
                    id: minted_id.to_string(),
                    from,
                    targets: vec![peer],
                    body: text.to_string(),
                    memory,
                }
            }
        }
    }

    /// Fan out to every discovered agent under the one minted id, no memory
    /// change. Refused when no agents are present.
    fn fan_out(
        &mut self,
        from: PeerId,
        text: &str,
        minted_id: &str,
        env: &OutEnv,
    ) -> OutboundDecision {
        let targets: Vec<PeerId> = env
            .peers
            .iter()
            .filter(|p| !p.is_human())
            .cloned()
            .collect();
        if targets.is_empty() {
            return OutboundDecision::Refuse(RefuseReason::NoDestinations);
        }
        self.mint(minted_id);
        OutboundDecision::Publish {
            id: minted_id.to_string(),
            from,
            targets,
            body: text.to_string(),
            memory: None,
        }
    }

    /// Record a minted id for loop-guarding. Called before a `Publish` is
    /// returned, so the record always precedes any observation of that id.
    fn mint(&mut self, id: &str) {
        self.minted.record(id.to_string());
    }
}

/// The folded human key of a `human:<nick>` peer (its nick is already folded).
fn human_key(peer: &PeerId) -> String {
    peer.human_nick().unwrap_or_default().to_string()
}

/// Whether `peer` is among the discovered peers.
///
/// Presence is **exact peer-id membership** in the discovery snapshot, which
/// carries full peer ids — the same rule mesh→IRC routing already uses for
/// channel presence. It deliberately does not fall back to comparing
/// `dm_subject()`, because that mapping is not injective: `mu:d:s` (daemon `d`,
/// session `s`) and `mu:d.s` (daemon `d.s`, no session) are different peers
/// that both derive `mu.agent.mu.d.s.dm`. Treating equal subjects as one
/// identity let an explicit `mu:d.s: hello` through while only `mu:d:s` was
/// discovered: the publish then named the absent identity rather than the
/// present one, and the routing memory remembered that identity's channel
/// (`#mu-d.s`) instead of the discovered peer's (`#mu-d-s`). A destination
/// nobody has advertised is refused, whatever subject it happens to share.
fn discovered(peers: &[PeerId], peer: &PeerId) -> bool {
    peers.contains(peer)
}

/// An explicit address's classification.
enum Address {
    /// A `human:<nick>` address — refused as a destination.
    Human,
    /// An addressable agent peer.
    Agent(PeerId),
    /// The leading token was not an agent/human address; treat the line as
    /// ordinary text.
    Ordinary,
}

/// The agent roles this gateway accepts as explicit destinations.
const AGENT_ROLES: [&str; 3] = ["cc", "mu", "warden"];

/// Classify an explicit-address token (`cc:abc`, `mu:d:s`, `human:x`).
fn classify_address(addr: &str) -> Address {
    let peer = PeerId::parse(addr);
    if peer.is_human() {
        return Address::Human;
    }
    if !peer.id().is_empty() && AGENT_ROLES.contains(&peer.role()) {
        return Address::Agent(peer);
    }
    Address::Ordinary
}

/// Parse a leading `<token>: <body>` explicit address. The token is the leading
/// non-space run and must end in a colon (so a peer id's internal colons stay
/// with it); returns the token without its trailing colon, and the trimmed body.
/// `None` when the line has no such prefix.
fn parse_explicit(text: &str) -> Option<(&str, &str)> {
    let trimmed = text.trim_start();
    let end = trimmed.find(char::is_whitespace)?;
    let token = &trimmed[..end];
    let addr = token.strip_suffix(':')?;
    if addr.is_empty() {
        return None;
    }
    Some((addr, trimmed[end..].trim_start()))
}
