//! IRC → mesh routing (increments 3 and 4).
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

//!
//! # Bot verbs (increment 4)
//!
//! Two textual commands are dispatched AHEAD of all of the above, because a
//! command is not a message: `mu peers` prints the live presence set and
//! `mu say <peer id or alias> <text>` sends one line to one peer. A line the
//! command parser claims is never mirrored to the lobby, never fanned out, and
//! never published as itself — an `mu <verb>` this gateway does not implement
//! costs a one-line [`USAGE`] reply and nothing else, which is the whole point
//! of dispatching first: a typo must not become a broadcast.
//!
//! `mu say` is `role:id: text` wearing different clothes. It resolves against
//! the same live presence set (the full peer id first, then the channel alias
//! `mu peers` printed), refuses the same way, and writes the same routing
//! memory through the same code path — so which spelling a human used cannot
//! change where the answer comes back.
//!
//! A verb's answer is addressed to whoever typed it, not to the channel they
//! typed it in ([`OutboundDecision::Reply`]): a roster is for the person who
//! asked. Those answers are gateway-authored lines, so loop guard 1 is what
//! keeps them out of the mirror if one ever comes back around.

use mu_peer::PeerId;

use mu_dialogue::mesh::MeshDmEvent;

use crate::mapping::{
    channel_for, fold_nick, peer_alias, resolve_channel, CaseMapping, Resolved, SelfNick,
};
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

/// Where the operator-facing answer to one outbound line belongs when that line
/// cannot be delivered.
///
/// The decision carries this rather than the executor deriving it, because only
/// the decision knows how the line was SPELLED: an explicit address and a bot
/// verb reach the mesh through the same publish, and the difference between them
/// is not visible by the time delivery fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// Back where the line was said — the channel it was typed in, or privately
    /// when it was typed privately. What ordinary routing has always done: a
    /// line said to a room is part of that room's conversation, and so is the
    /// news that it did not go anywhere.
    WhereItWasSaid,
    /// Privately to whoever typed the line, whichever target they typed it to.
    /// A bot verb answers its sender for EVERY outcome, and a mesh that is down
    /// is one of its outcomes: `mu say` typed in a channel must not turn a
    /// delivery failure into the one command answer the room gets to read.
    Sender,
}

/// What the outbound side decided for one IRC line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboundDecision {
    /// Publish `body` from `from` to every peer in `targets`, all sharing the one
    /// minted `id`. `memory` is applied only for a specifically-addressed line,
    /// and `answer` is where a failure to deliver it is reported.
    Publish {
        id: String,
        from: PeerId,
        targets: Vec<PeerId>,
        body: String,
        memory: Option<MemoryUpdate>,
        answer: Answer,
    },
    /// Answer a bot verb, publishing nothing. The answer goes PRIVATELY to
    /// whoever typed the line, whichever target they typed it to — a roster,
    /// a usage line and a `mu say` that named nobody are all for the person who
    /// asked, not for the room.
    Reply(CommandReply),
    /// Refuse and tell the operator why (the reason names peers, never a body).
    Refuse(RefuseReason),
    /// Drop silently — the gateway's own echo, or a disconnected transport.
    Drop(OutDrop),
}

/// What a bot verb answered with. Nothing here is published; every variant is
/// text on its way back to one human.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandReply {
    /// The live presence set, already rendered: one line per present agent,
    /// naming its full peer id and the channel it maps to.
    ///
    /// Rendered HERE rather than handed over as peers, because what a peer's
    /// channel is — and whether that channel is shared with another peer — is
    /// this crate's mapping knowledge, and the executor deriving it a second
    /// time is how two answers to the same question start to disagree.
    Peers(Vec<String>),
    /// A `mu say` that named no deliverable destination. The reasons are the
    /// ones ordinary routing already uses, so one vocabulary explains both
    /// spellings of the same mistake.
    Refused(RefuseReason),
    /// An `mu <verb>` this gateway does not implement, `mu say` with nothing to
    /// say included: one line of [`USAGE`], and no publication.
    Usage,
}

/// The one-line answer to an unsupported verb. Names both verbs and their
/// arguments, because the human who typed the typo is the one who needs it.
pub const USAGE: &str =
    "usage: `mu peers` lists the agents on the mesh; `mu say <peer id or alias> <text>` \
     sends one line to one of them";

/// Why an outbound line was refused, phrased for the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefuseReason {
    /// The target is (or resolves to) a human; humans are not agent destinations.
    HumanDestination,
    /// An explicit address that names no currently-discovered peer.
    AbsentDestination(PeerId),
    /// A channel that case-folds onto more than one peer; neither is chosen.
    AmbiguousChannel(Vec<PeerId>),
    /// A `mu say` alias that more than one present peer answers to; neither is
    /// chosen, and the colliding peers are named so the human can retype one of
    /// them as a full id.
    AmbiguousPeer(Vec<PeerId>),
    /// A `mu say` destination that is neither a present peer nor any present
    /// peer's alias — including a token that is not a peer id at all. Carried
    /// verbatim so the reply can quote what was typed.
    UnknownPeer(String),
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

        // Bot verbs are dispatched AHEAD of every routing rule below, which is
        // what makes "a command is never mirrored" structural: a line the parser
        // claims cannot reach the lobby fan-out, the channel publish, or the
        // explicit-address path, whichever target it was typed to. The
        // authorization above still ran first, because `mu say` publishes under
        // the gateway's own capability exactly as an explicit address does — and
        // because the roster is not something to hand a nick this gateway shares
        // no channel with.
        if let Some(command) = parse_command(text) {
            return self.run_command(command, from, target, minted_id, env);
        }

        // An explicit `role:id: body` address overrides the channel/target.
        if let Some((addr, body)) = parse_explicit(text) {
            match classify_address(addr) {
                Address::Human => return OutboundDecision::Refuse(RefuseReason::HumanDestination),
                Address::Agent(peer) => {
                    return self.publish_explicit(
                        from,
                        peer,
                        target,
                        body,
                        minted_id,
                        env,
                        Answer::WhereItWasSaid,
                    );
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

    /// Run one parsed bot verb. `source` is the IRC target the human typed the
    /// command to, which is what `mu say` remembers from — the same rule an
    /// explicit address follows, so `mu say cc:abc hi` and `cc:abc: hi` typed in
    /// the same place leave the same routing memory behind.
    fn run_command(
        &mut self,
        command: Command<'_>,
        from: PeerId,
        source: &str,
        minted_id: &str,
        env: &OutEnv,
    ) -> OutboundDecision {
        match command {
            Command::Peers => OutboundDecision::Reply(CommandReply::Peers(self.peers_lines(env))),
            Command::Say { dest, body } => match self.resolve_say(dest, env) {
                // Resolution already proved presence; `publish_explicit` re-checks
                // it against the same snapshot and owns the routing-memory rule,
                // which is why `mu say` goes through it rather than beside it.
                SayTarget::Peer(peer) => {
                    // The publication carries the command's own destination, so
                    // the executor answers the SENDER if the mesh turns out to be
                    // down — the same place every other outcome of this verb is
                    // answered, rather than the channel it was typed in.
                    match self.publish_explicit(
                        from,
                        peer,
                        source,
                        body,
                        minted_id,
                        env,
                        Answer::Sender,
                    ) {
                        // Resolution proved presence against this same snapshot,
                        // so the re-check cannot refuse today. Folding it into the
                        // verb's private answer anyway means the destination rule
                        // holds by construction rather than by that argument.
                        OutboundDecision::Refuse(reason) => {
                            OutboundDecision::Reply(CommandReply::Refused(reason))
                        }
                        published => published,
                    }
                }
                SayTarget::Human => {
                    OutboundDecision::Reply(CommandReply::Refused(RefuseReason::HumanDestination))
                }
                SayTarget::Absent(peer) => OutboundDecision::Reply(CommandReply::Refused(
                    RefuseReason::AbsentDestination(peer),
                )),
                SayTarget::Ambiguous(peers) => OutboundDecision::Reply(CommandReply::Refused(
                    RefuseReason::AmbiguousPeer(peers),
                )),
                SayTarget::Unknown => OutboundDecision::Reply(CommandReply::Refused(
                    RefuseReason::UnknownPeer(dest.to_string()),
                )),
            },
            Command::Unsupported => OutboundDecision::Reply(CommandReply::Usage),
        }
    }

    /// Render the live presence set: a header, then one line per present agent
    /// naming its FULL peer id (the spelling `mu say` and an explicit address
    /// both accept) and the channel it maps to.
    ///
    /// Humans are left out. They are not mesh destinations, the gateway fronts
    /// them only while it can see them on IRC, and IRC already shows a person
    /// who is in the room — so a roster of them would be a second, staler answer
    /// to a question the client already answers. The contract names no place for
    /// them here, and inventing one would be inventing a disclosure.
    ///
    /// A channel more than one present peer folds onto is marked `(shared)`
    /// rather than printed as if it addressed the peer on that line: that is
    /// exactly what [`Resolved::Ambiguous`] means, and it is the same collision
    /// the mesh→IRC side labels bodies for.
    fn peers_lines(&self, env: &OutEnv) -> Vec<String> {
        let agents: Vec<&PeerId> = env.peers.iter().filter(|p| !p.is_human()).collect();
        if agents.is_empty() {
            return vec![NO_AGENTS.to_string()];
        }
        let mut lines = Vec::with_capacity(agents.len() + 1);
        lines.push(format!("{} on the mesh right now:", plural(agents.len())));
        for peer in agents {
            lines.push(format!("{peer} — {}", self.where_peer_is(peer, env)));
        }
        lines
    }

    /// Where one peer is, as the mapping module reports it: its channel, that
    /// channel marked `(shared)` when another present peer folds onto it, or
    /// `(no channel)` for a peer the mapping gives none.
    fn where_peer_is(&self, peer: &PeerId, env: &OutEnv) -> String {
        let Some(channel) = channel_for(peer, &self.prefix, self.channellen) else {
            return "(no channel)".to_string();
        };
        let shared = matches!(
            resolve_channel(&channel, env.peers, &self.prefix, self.channellen, self.cm),
            Resolved::Ambiguous(_)
        );
        if shared {
            format!("{channel} (shared)")
        } else {
            channel
        }
    }

    /// Resolve a `mu say` destination against the LIVE presence set.
    ///
    /// The full peer id is tried first and exactly, the same rule an explicit
    /// address uses — a destination is present only on exact membership in the
    /// discovery snapshot, never on a matching DM subject. Only then is the
    /// token read as the alias `mu peers` printed a channel from, folded under
    /// the server's rule because the human typed it into IRC. An alias several
    /// peers answer to resolves to none of them.
    fn resolve_say(&self, dest: &str, env: &OutEnv) -> SayTarget {
        let parsed = PeerId::parse(dest);
        if parsed.is_human() {
            return SayTarget::Human;
        }
        if discovered(env.peers, &parsed) {
            return SayTarget::Peer(parsed);
        }
        let want = fold_nick(dest, self.cm);
        let mut hits: Vec<PeerId> = env
            .peers
            .iter()
            .filter(|p| fold_nick(&peer_alias(p), self.cm) == want)
            .cloned()
            .collect();
        match hits.len() {
            // A named identity the mesh does not currently carry is ABSENT, and
            // saying so names it back; a token that is no peer id at all is
            // simply unknown, and quoting it is all that can be said.
            0 => match classify_address(dest) {
                Address::Agent(peer) => SayTarget::Absent(peer),
                Address::Human => SayTarget::Human,
                Address::Ordinary => SayTarget::Unknown,
            },
            1 => match hits.pop().expect("len checked") {
                peer if peer.is_human() => SayTarget::Human,
                peer => SayTarget::Peer(peer),
            },
            _ => SayTarget::Ambiguous(hits),
        }
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
    ///
    /// `answer` is passed through to the decision: the caller knows whether this
    /// is an explicit address or a `mu say`, and nothing downstream does.
    #[allow(clippy::too_many_arguments)]
    fn publish_explicit(
        &mut self,
        from: PeerId,
        peer: PeerId,
        source: &str,
        body: &str,
        minted_id: &str,
        env: &OutEnv,
        answer: Answer,
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
            answer,
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
                    answer: Answer::WhereItWasSaid,
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
            answer: Answer::WhereItWasSaid,
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

/// What an empty presence set is called, in the one place both the `mu peers`
/// listing and a refused fan-out can read it from. Two sentences for one fact
/// drift apart; this one does not.
pub const NO_AGENTS: &str = "no agents are on the mesh right now";

/// `"1 agent"` / `"N agents"` — the count the roster header opens with.
fn plural(n: usize) -> String {
    format!("{n} agent{}", if n == 1 { "" } else { "s" })
}

/// A bot verb, already parsed. Borrowed from the line, because a command's
/// arguments are used and dropped inside one decision.
enum Command<'a> {
    /// `mu peers` — print the live presence set.
    Peers,
    /// `mu say <dest> <body>` — one line to one peer.
    Say { dest: &'a str, body: &'a str },
    /// An `mu <verb>` this gateway does not implement, or a verb whose
    /// arguments are not the ones it takes. Answered with [`USAGE`], published
    /// nowhere.
    Unsupported,
}

/// The bot-verb prefix. A line whose FIRST whitespace-delimited token is
/// exactly this (ASCII case-insensitively) is a command.
///
/// `mu:d: hello` is not: its token is `mu:d:`, which is an explicit address and
/// stays one. That distinction is the token boundary and nothing cleverer,
/// which is what keeps a peer id from ever being read as a verb.
const VERB_PREFIX: &str = "mu";

/// Parse a bot-verb line, or `None` if this is ordinary text.
///
/// A bare `mu` with no verb is ordinary text on purpose: it is a word, people
/// type it, and claiming it would silently swallow a line the fan-out should
/// have carried. Everything after a real verb prefix IS claimed, including an
/// unknown verb — a command that is nearly right must not fall through and be
/// broadcast to every agent on the mesh.
fn parse_command(text: &str) -> Option<Command<'_>> {
    let (head, rest) = split_token(text.trim());
    if !head.eq_ignore_ascii_case(VERB_PREFIX) || rest.is_empty() {
        return None;
    }
    let (verb, args) = split_token(rest);
    if verb.eq_ignore_ascii_case("peers") {
        // `mu peers` takes no argument. Ignoring one would answer a question
        // that was not asked; the usage line says what the verbs take instead.
        return Some(if args.is_empty() {
            Command::Peers
        } else {
            Command::Unsupported
        });
    }
    if verb.eq_ignore_ascii_case("say") {
        let (dest, body) = split_token(args);
        // A destination with nothing to say is not a message; it is a
        // half-typed command, and publishing an empty body would be worse than
        // answering with the usage.
        return Some(if dest.is_empty() || body.is_empty() {
            Command::Unsupported
        } else {
            Command::Say { dest, body }
        });
    }
    Some(Command::Unsupported)
}

/// Split the leading whitespace-delimited token off `s`, returning it and the
/// remainder with its leading whitespace trimmed.
fn split_token(s: &str) -> (&str, &str) {
    match s.find(char::is_whitespace) {
        Some(end) => (&s[..end], s[end..].trim_start()),
        None => (s, ""),
    }
}

/// What a `mu say` destination resolved to against the live presence set.
enum SayTarget {
    /// One present, addressable agent.
    Peer(PeerId),
    /// A well-formed agent id that nothing on the mesh currently advertises.
    Absent(PeerId),
    /// An alias more than one present peer answers to.
    Ambiguous(Vec<PeerId>),
    /// A human — the gateway's own nick included. Never a mesh destination.
    Human,
    /// Neither a present peer, nor an alias, nor a peer id at all.
    Unknown,
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
