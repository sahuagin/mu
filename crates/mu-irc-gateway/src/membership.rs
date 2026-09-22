//! Disposable membership and channel lifecycle.
//!
//! Two offline state machines, both pure functions of the events fed to them —
//! no socket, no timer, no stored roster that outlives the process:
//!
//! - [`Membership`] tracks who the gateway currently observes in each channel it
//!   is in, folded under the server's live `CASEMAPPING`, reconciled from
//!   `RPL_NAMREPLY`/`RPL_ENDOFNAMES` against intervening JOIN/PART/KICK/QUIT/NICK.
//!   A NAMES burst is a snapshot of an earlier instant, so an event the gateway
//!   watched happen wins over a snapshot line that contradicts it, in either
//!   order of arrival — a departure leaves a tombstone for the rest of that
//!   sync, and a later JOIN or NICK clears it.
//!   Every real nick other than the gateway's OWN nicks is a human operator —
//!   the gateway's nick plus every puppet nick it holds on behalf of an agent
//!   (`specs/plans/mu-irc-gateway-v1-puppets.md`): humans are present nicks
//!   minus that owned set. So membership is also the sole authority on which
//!   humans are *present* — the fact routing consults before ever disclosing a
//!   private body. It emits [`HumanEffect`]s (front / release / rename) whose
//!   executor uses `front_peer`/`release_peer`; it never touches the mesh
//!   itself. The owned set is handed in by the bridge, from the puppet pool,
//!   BEFORE any puppet connects, so no JOIN or NAMES entry for a puppet is ever
//!   read as a human arriving. (Puppets land in increments, this seam
//!   first: the bridge's ownership sync, its departure reports and the
//!   barrier they are ordered behind are the integration increment above
//!   this one, so this module's puppet surface has no caller here yet.)
//! - [`ChannelReconciler`] decides which channels the gateway *should* be in from
//!   the current discovered-agent snapshot (the lobby always; one channel per
//!   agent peer via [`crate::mapping::channel_for`]; never a human channel),
//!   diffs that against what it has joined/pending, and emits JOIN/PART effects.
//!   A refused JOIN is diagnosed once and then retried on an exponential backoff
//!   (30 s, doubling, capped at ten minutes).
//!
//! Everything is invalidated on the events the plan calls disposable: a gateway
//! PART/KICK drops one channel, and a disconnect drops all of it — after which
//! fresh discovery and NAMES rebuild it from nothing, with no traffic retained.
//! A live `CASEMAPPING` change is the one case that is re-derived rather than
//! dropped: the gateway is still in the same channels, so every folded key is
//! rebuilt from the wire spelling it came from and a fresh NAMES commits over
//! the kept roster. Dropping the channel set there would be unrecoverable — a
//! JOIN for a channel the client already occupies yields no echo and no NAMES.

use std::collections::{HashMap, HashSet};

use mu_peer::PeerId;

use crate::mapping::{channel_for, fold_nick, CaseMapping, SelfNick};

// ─────────────────────────────── Membership ─────────────────────────────────

/// One observed channel member. Identity is the folded nick alone; the account
/// is metadata the server reported (`account-tag`) and never changes which
/// `human:<nick>` this is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The nick as last seen on the wire (for display / framing).
    pub display: String,
    /// The services account the server attributed, if any. Informational.
    pub account: Option<String>,
}

/// An in-progress NAMES synchronization for one channel.
struct Sync {
    /// The generation this sync belongs to. A `Names`/`NamesEnd` for a different
    /// generation is stale and ignored, so a NAMES burst that races a rejoin
    /// cannot resurrect the wrong roster.
    gen: u64,
    /// Names gathered so far. Live JOIN/PART/etc. during the sync are applied
    /// here too, so committing this at `NamesEnd` preserves interleaved events.
    pending: HashMap<String, Member>,
    /// Folded nicks OBSERVED LEAVING while this sync was open — tombstones.
    ///
    /// A NAMES burst is a snapshot the server took at some earlier instant, and
    /// its lines arrive interleaved with live events. Removing a departed nick
    /// from `pending` is not enough: if the PART/KICK/QUIT arrives before the
    /// NAMES line that mentions that nick, there is nothing in `pending` to
    /// remove, and the later line reinserts them. A departure the gateway
    /// actually watched happen is newer than any line of the snapshot, so it
    /// wins — the tombstone makes the NAMES entry a no-op. A subsequent live
    /// JOIN or NICK clears it, because that is newer still.
    departed: HashSet<String>,
}

impl Sync {
    fn new(gen: u64) -> Self {
        Sync {
            gen,
            pending: HashMap::new(),
            departed: HashSet::new(),
        }
    }
}

/// A channel the gateway is in, with its observed members (folded nick → member).
struct Channel {
    /// The channel name as the gateway knows it (for effects / framing).
    display: String,
    members: HashMap<String, Member>,
    sync: Option<Sync>,
    /// The generation of the last snapshot opened for the channel: what an
    /// arrival held back about it is stamped with, so a committed roster
    /// can supersede what was held back before it was requested.
    gen: u64,
}

/// A human whose `human:<nick>` endpoint the executor should front / release /
/// rename on the mesh. Offline: returned as data, never executed here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HumanEffect {
    /// First time this human is observed present anywhere: front the endpoint.
    Register(PeerId),
    /// This human just left their last shared channel: release the endpoint.
    Withdraw(PeerId),
    /// A NICK change moved this human to a new identity: release the old
    /// endpoint, front the new one, and carry their routing memory across. A
    /// single effect so the transfer is atomic to the executor.
    Rename { from: PeerId, to: PeerId },
}

/// The gateway's live view of channel membership and human presence.
pub struct Membership {
    /// The gateway's own nick, kept in its wire spelling with the folded form
    /// derived — never tracked as a human. See [`SelfNick`]: re-folding an
    /// already-folded nick loses the gateway's identity across a `CASEMAPPING`
    /// change, so the original is what survives.
    self_nick: SelfNick,
    /// Puppet nicks the gateway holds: folded key → wire spelling (kept so a
    /// `CASEMAPPING` change re-derives, as with `self_nick`). Never humans,
    /// never present, never fronted.
    owned: HashMap<String, Held>,
    /// Puppet nicks the pool has RELEASED but whose departure this connection
    /// has not yet observed (their QUIT is queued or in flight, up to the
    /// grace). Treated exactly as owned until the server's QUIT for the nick
    /// arrives here — the release is driven off the observed departure, never
    /// off the queued QUIT, so a NAMES snapshot committing in the gap cannot
    /// front a still-connected puppet as a human.
    retiring: HashMap<String, Held>,
    /// Owned puppet nicks whose QUIT this connection has ALREADY observed on
    /// the wire while they were still owned (the puppet's socket died before
    /// the pool released it). A release of such a nick has nothing to wait
    /// for and must not retire it — a human could otherwise never take the
    /// freed name — and until that release whoever appears under the name is
    /// a human (`is_puppet`). Cleared by the release, by a fresh registration
    /// of the spelling, or by a live puppet renamed onto it.
    gone: HashSet<String>,
    /// The CONNECTIONS whose puppet was seen to QUIT while the pool still
    /// listed it, until the pool stops listing them: a listing of such a
    /// connection is stale — it holds nothing on the server — whatever
    /// spelling it names, which `gone` (a spelling) cannot say once another
    /// connection's puppet has moved onto the spelling.
    gone_connections: HashSet<u64>,
    /// What the wire said about a RETIRING nick, held back in order: a JOIN,
    /// a snapshot line, a PART or KICK, a NICK — under a nick the pool
    /// released but whose departure this connection has not yet observed.
    /// Which of two things it all was depends on how the entry resolves. If
    /// the puppet's own QUIT is observed on the wire it was in a shared
    /// channel, its QUIT was broadcast after everything of its own, and every
    /// arrival before that QUIT was the puppet itself: discarded. If instead
    /// the executor's CONFIRMED report resolves the entry (the puppet was in
    /// no shared channel, so nothing of its own could have come here — see
    /// [`puppet_departed`](Self::puppet_departed) for what the bridge
    /// guarantees), every arrival was the human who took the freed name in
    /// the window: replayed then, in order, so the name's new holder is
    /// fronted where they are and not where they were (panel findings,
    /// PRs #662, #671). Keyed by the folded nick.
    deferred: HashMap<String, Vec<Arrival>>,
    /// Spellings a puppet has MOVED ON from by a rename its own connection
    /// reported, while that rename waits at the bridge's barrier: still
    /// ours (for the ownership sync, and against a human's claim) but no
    /// longer the puppet's on the server. What the wire says under one is
    /// held back (`deferred`) as under a retiring nick, and settled at that
    /// rename's barrier — replayed as the name's new holder's (the puppet
    /// shared no channel) unless the wire's own NICK settled it first (it
    /// did, and what came before was the puppet: discarded). A chain
    /// reported before the first barrier vacates each spelling in turn,
    /// each settled by its own barrier. Folded spelling → the puppet's.
    vacating: HashMap<String, Held>,
    /// The spelling a puppet is on the server under by the LAST rename its
    /// connection reported, while the barrier has not yet applied it: a JOIN
    /// under it is the puppet's, never a human's. Folded → (connection,
    /// wire). One per connection.
    expected: HashMap<String, (u64, String)>,
    /// A vacating entry a story's holder moved to (a human who took a vacated
    /// spelling and renamed, `renamed`) → the vacated spelling it descends
    /// from, so the barrier that settles that spelling settles the holder's
    /// entry with it. Folded → folded.
    origin: HashMap<String, String>,
    cm: CaseMapping,
    channels: HashMap<String, Channel>,
    /// Folded human nick → the set of folded channels they are currently in.
    /// A human is present iff this set is non-empty; the count is what makes a
    /// human shared across two channels survive leaving one of them.
    present: HashMap<String, HashSet<String>>,
    gen: u64,
}

/// A puppet nick as membership holds it: the wire spelling, and the
/// CONNECTION it belongs to — an id the bridge supplies (the executor's
/// attempt id), so a departure report resolves the entry of the connection
/// that departed and never a later connection's under the same spelling.
/// `0` is "unidentified": a caller with no connection ids gets the old
/// nick-keyed behaviour (tests, mostly).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Held {
    wire: String,
    connection: u64,
    /// Owned only: the spellings this entry has been moved on from here —
    /// the wire's NICK, or a barrier — since the pool last listed one for
    /// the connection, oldest first; a listing under one of them is the
    /// pool lagging, under any other it is ahead. Empty for an entry the
    /// pool listed itself. WIRE spellings, folded where compared: a folded
    /// key would stop matching across a `CASEMAPPING` change (panel
    /// finding, PR #679 run 2).
    trail: Vec<String>,
}

/// One thing the wire said about a retiring nick, held back until the entry
/// resolves (see `Membership::deferred`). Spellings as the wire gave them.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Arrival {
    /// A JOIN, or a snapshot line: `(channel, nick, account, generation)` —
    /// the generation of the channel's last opened snapshot when it was
    /// held back.
    Joined(String, String, Option<String>, u64),
    /// A PART or KICK: `(channel, nick, generation)`.
    Left(String, String, u64),
    /// A NICK: `(from, to)`.
    Renamed(String, String),
}

impl Membership {
    /// A fresh, empty view for a gateway registered as `self_nick`, folding under
    /// `cm`.
    pub fn new(self_nick: &str, cm: CaseMapping) -> Self {
        Membership {
            self_nick: SelfNick::new(self_nick, cm),
            owned: HashMap::new(),
            retiring: HashMap::new(),
            gone: HashSet::new(),
            gone_connections: HashSet::new(),
            deferred: HashMap::new(),
            vacating: HashMap::new(),
            expected: HashMap::new(),
            origin: HashMap::new(),
            cm,
            channels: HashMap::new(),
            present: HashMap::new(),
            gen: 0,
        }
    }

    /// The current fold rule.
    pub fn casemapping(&self) -> CaseMapping {
        self.cm
    }

    /// The gateway's own nick as the server spells it. Stable across
    /// `CASEMAPPING` changes; changes only when the gateway itself renames.
    pub fn self_nick(&self) -> &str {
        self.self_nick.original()
    }

    /// Whether a folded nick is one of the gateway's own: its nick, a puppet
    /// it holds, or a puppet it released whose departure is not yet observed.
    /// Such a nick is never a human.
    fn is_own(&self, key: &str) -> bool {
        key == self.self_nick.folded() || self.is_puppet(key)
    }

    /// Whether a puppet of ours is on the server under this folded nick, or
    /// may still be: held by the pool and not yet seen to leave, or released
    /// and not yet seen to leave (retiring). The gateway's own nick excluded.
    /// A nick the pool still lists but whose puppet was already seen to QUIT
    /// (`gone`) is NOT a puppet's any more: whoever appears under it before
    /// the pool's release reaches membership is a new occupant, a human, and
    /// is fronted like one (panel finding, PR #665).
    fn is_puppet(&self, key: &str) -> bool {
        ((self.owned.contains_key(key) || self.expected.contains_key(key))
            && !self.gone.contains(key))
            || self.retiring.contains_key(key)
            || self.vacating.contains_key(key)
    }

    /// (A retiring entry may sit under a key the owned set still lists for
    /// a DEPARTED puppet — `gone` — after a NICK onto that name: the story
    /// goes on there, the departed puppet's listing notwithstanding.)
    /// The entry a folded nick's story is held under — the nick itself, when
    /// it is vacating, or retiring and not owned. Such an entry moves with
    /// the wire's NICK for it (`renamed`), so a vacated spelling is free and
    /// the story follows the name (a vacating entry's, away from the owned
    /// spelling the pool still lists, which stays owned for the sync).
    fn story_key(&self, key: &str) -> Option<String> {
        let held = self.vacating.contains_key(key)
            || (self.retiring.contains_key(key)
                && (!self.owned.contains_key(key) || self.gone.contains(key)));
        held.then(|| key.to_string())
    }

    /// A NICK on the wire ONTO a retiring name: the server accepted it, so
    /// the name is free — the puppet has left the server (one in a shared
    /// channel would have had its QUIT seen first, resolving the entry). The
    /// retiring entry resolves here as the observed departure it implies:
    /// nothing of a holder's can be held under a name nobody holds, the
    /// departed puppet's spelling is tombstoned for open syncs (the human's
    /// own rename clears it again wherever they are known), and the name is
    /// the human's from here — their PART or QUIT under it a human's, never
    /// swallowed as the puppet's; the late report finds nothing (panel
    /// finding, PR #671 run 16). A live puppet's listing is not touched (a
    /// NICK onto it cannot happen; a story moving onto a departed puppet's
    /// listed name goes on beside the listing, `story_key`) — but a retiring
    /// entry BESIDE such a listing resolves like any other: its holder had
    /// to be gone for the server to give the name away, and left stale it
    /// would swallow the newcomer's PART and QUIT (panel finding, PR #671
    /// run 20); the departed listing itself stays as it is — or onto a VACATED spelling, which
    /// `renamed` holds back.
    fn taken_by_nick(&mut self, key: &str) -> Vec<HumanEffect> {
        let live = self.owned.contains_key(key) && !self.gone.contains(key);
        if live || self.vacating.contains_key(key) {
            return Vec::new();
        }
        let Some(held) = self.retiring.remove(key) else {
            return Vec::new();
        };
        self.forget_story(key);
        for ch in self.channels.values_mut() {
            if let Some(sync) = ch.sync.as_mut() {
                sync.pending.remove(key);
                sync.departed.insert(key.to_string());
            }
        }
        // The connection is off the server, so its whole window resolves
        // here as it does on an observed QUIT: its remaining hops' stories
        // are their holders' and replay as theirs, and nothing of it stays
        // expected — a spelling it was renaming to would otherwise keep
        // reading as ours and swallow the human who takes it (panel
        // finding, PR #679 run 10).
        if held.connection == 0 {
            return Vec::new();
        }
        self.forget_expectation(held.connection);
        self.settle(held.connection, None, true)
    }

    /// The retiring entry held under `connection`, by whatever spelling it
    /// has come to (a report is about a connection, not a spelling); `0`
    /// — no connection — finds by `nick` alone.
    fn retiring_of(&self, nick: &str, connection: u64) -> Option<String> {
        if connection == 0 {
            let key = self.fold(nick);
            return self.retiring.contains_key(&key).then_some(key);
        }
        self.retiring
            .iter()
            .find(|(_, h)| h.connection == connection)
            .map(|(k, _)| k.clone())
    }

    /// Forget the held-back story of a resolved retiring entry.
    fn forget_story(&mut self, key: &str) {
        self.deferred.remove(key);
    }

    /// The connection is gone (its departure report is in): a rename it
    /// reported and the barrier has not applied expects nothing any more —
    /// the name is free, and whoever appears under it is a human. Nothing
    /// for the id-less form, whose expectations are nobody's in particular.
    fn forget_expectation(&mut self, connection: u64) {
        if connection != 0 {
            self.expected.retain(|_, (c, _)| *c != connection);
        }
    }

    /// Replace the set of puppet nicks the gateway holds (wire spellings). The
    /// bridge calls this from the pool's owned set — before the first puppet
    /// connects, and again as puppets register or give up.
    ///
    /// A nick that is newly owned but currently tracked as a human (a puppet
    /// whose JOIN arrived before the pool told membership about it) is evicted
    /// from every roster and withdrawn, so the correction is made here rather
    /// than left to whoever noticed the ordering. A nick that LEAVES the set
    /// is not forgotten: the pool releases a nick when it queues the puppet's
    /// QUIT, and the puppet is still on the server until that QUIT lands, so
    /// the nick moves to the retiring set and stays "ours" until this
    /// connection observes its departure ([`quit`](Self::quit)). A nick in
    /// both is simply owned again.
    ///
    /// Nicks without connection ids; see [`set_owned`](Self::set_owned).
    pub fn set_owned_nicks<I, S>(&mut self, nicks: I) -> Vec<HumanEffect>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.set_owned(nicks.into_iter().map(|n| (n, 0)))
    }

    /// [`set_owned_nicks`](Self::set_owned_nicks) with each nick's CONNECTION
    /// id (the executor's attempt id). A nick that leaves the set keeps the
    /// id it was held under, and [`puppet_departed`](Self::puppet_departed)
    /// resolves it only with that id: a report about an earlier connection
    /// that lags a re-registration of the same spelling cannot free the
    /// newer connection's nick (panel finding, PR #671).
    pub fn set_owned<I, S>(&mut self, nicks: I) -> Vec<HumanEffect>
    where
        I: IntoIterator<Item = (S, u64)>,
        S: AsRef<str>,
    {
        let cm = self.cm;
        let listed: Vec<(String, u64)> = nicks
            .into_iter()
            .map(|(n, connection)| (n.as_ref().to_string(), connection))
            .collect();
        // A connection whose puppet was seen to QUIT holds nothing, whatever
        // the pool still lists it under (it learns at the puppet's Ended):
        // its listing is dropped here — the spelling may be another
        // connection's by now, moved onto after the QUIT — and the
        // connection is forgotten once the pool stops listing it.
        self.gone_connections
            .retain(|c| listed.iter().any(|(_, connection)| connection == c));
        let mut next: HashMap<String, Held> = listed
            .into_iter()
            .filter(|(_, connection)| !self.gone_connections.contains(connection))
            .map(|(wire, connection)| {
                (
                    fold_nick(&wire, cm),
                    Held {
                        wire,
                        connection,
                        trail: Vec::new(),
                    },
                )
            })
            .collect();
        // A connection listed under another spelling is the same nick
        // renamed; which side is current is read from the entry's `trail`
        // and from the window. A listing under a spelling the trail records
        // is the pool LAGGING a rename applied here (by one hop or
        // several): the wire's spelling stands, and the old one neither
        // claims the vacated name (a human may hold it) nor releases the
        // new. Any other spelling is the pool AHEAD — a rename it learned
        // of first, the intermediate ones never listed (panel finding, PR
        // #671 run 27): the entry moves to it as a rename does, the old
        // spelling free and tombstoned, never retired.
        //
        // A spelling can be BOTH in the trail and current: the puppet
        // renamed back to a name it had before. Then the connection has a
        // rename pending onto it — the pool's spelling moves only with a
        // rename this side applied (the wire's NICK) or one applied at a
        // barrier, so a spelling the pool lists that this side has not
        // moved to yet is one a report has named — and that expectation
        // settles it: the listing is current, not stale (panel finding, PR
        // #682 run 1). Only a real connection id tells nicks apart; the
        // id-less form keys by spelling alone.
        let mut renamed: Vec<(String, String, Held)> = Vec::new();
        let mut lagging: Vec<(String, String, Held)> = Vec::new();
        for (k, h) in next.iter().filter(|(_, h)| h.connection != 0) {
            if let Some((pk, ph)) = self
                .owned
                .iter()
                .find(|(pk, ph)| ph.connection == h.connection && *pk != k)
            {
                let expected_here = self
                    .expected
                    .get(k)
                    .is_some_and(|(c, _)| *c == h.connection);
                if !expected_here && ph.trail.iter().any(|w| fold_nick(w, cm) == *k) {
                    lagging.push((k.clone(), pk.clone(), ph.clone()));
                } else {
                    renamed.push((pk.clone(), k.clone(), h.clone()));
                }
            }
        }
        // Applied as ONE step — every lagging spelling out, then every
        // current one in — since a lagging spelling can be another lagging
        // connection's current one (b → c on the wire freed b; a → b took
        // it; the pool lists both as they were), and one at a time the
        // order would decide which survives (panel finding, PR #671 run 26).
        for (stale, _, _) in &lagging {
            next.remove(stale);
        }
        for (_, current, held) in lagging {
            // The spelling the connection moved to may be listed for ANOTHER
            // connection by now — a fresh registration under it (the first
            // connection's own departure, if that is how the spelling came
            // free, was dropped from the listing above). The listing is the
            // newer fact: the newcomer keeps the spelling, and the lagging
            // connection holds nothing here (panel findings, PR #671 runs
            // 24-25).
            next.entry(current).or_insert(held);
        }
        for (old, _, _) in &renamed {
            self.owned.remove(old);
            for ch in self.channels.values_mut() {
                if let Some(sync) = ch.sync.as_mut() {
                    sync.pending.remove(old);
                    sync.departed.insert(old.clone());
                }
            }
        }
        let previous = std::mem::replace(&mut self.owned, next);
        // Newly owned — a spelling not held before, or held before by ANOTHER
        // connection: a fresh registration is on the server again, whatever
        // departure of an earlier holder of the spelling was remembered, and
        // whatever arrivals under it were held back. A nick owned across the
        // call by the same connection keeps its record — its departure may
        // have been seen just before the release that is still to come.
        let fresh: Vec<String> = self
            .owned
            .iter()
            .filter(|(k, held)| {
                previous
                    .get(*k)
                    .is_none_or(|was| was.connection != held.connection)
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in &fresh {
            self.gone.remove(key);
            // …but a spelling the connection's OWN pending rename named —
            // an intermediate hop the pool has caught up to, or the name it
            // is renaming to — is not a fresh registration to clear: the
            // window over it is the reports', and a holder held back under
            // it is still waiting for its barrier (panel finding, PR #679
            // run 12).
            let connection = self.owned.get(key).map_or(0, |h| h.connection);
            let pending = connection != 0
                && (self
                    .vacating
                    .get(key)
                    .is_some_and(|h| h.connection == connection)
                    || self
                        .expected
                        .get(key)
                        .is_some_and(|(c, _)| *c == connection));
            if pending {
                continue;
            }
            self.forget_story(key);
            self.vacating.remove(key);
            self.origin.remove(key);
            self.expected.remove(key);
        }
        for (key, held) in previous {
            if !self.owned.contains_key(&key) {
                // The pool CAUGHT UP to a rename the connection reported:
                // it lists the connection under the new spelling, and the
                // old one is a vacated spelling of the same connection —
                // not a released nick. Its barrier settles it; retiring it
                // here would leave an entry nothing resolves, and the name
                // would read as ours for good (panel finding, PR #679 run
                // 13).
                let caught_up = self
                    .vacating
                    .get(&key)
                    .is_some_and(|h| h.connection == held.connection)
                    && self.owned.values().any(|h| h.connection == held.connection);
                if caught_up {
                    continue;
                }
                if self.gone.remove(&key) {
                    // Its departure was already observed while it was owned:
                    // there is no gap to cover, and the name is free right
                    // now. Open syncs were tombstoned when it was seen.
                    continue;
                }
                // Released: retiring until its departure is seen, under the
                // connection it was held by (a story held back while it was
                // vacating continues under it). Tombstoned for open syncs
                // too, so a snapshot line naming it after the departure
                // cannot front it. A rename the connection reported and the
                // barrier has not applied keeps its window — the vacated
                // spellings, the expected name: the puppet is on the server
                // under that name, QUIT still pending, and a JOIN, a NAMES
                // line or a QUIT under it is the puppet's until seen to
                // leave; what is held back under a vacated spelling is its
                // new holder's, settled at the barrier or by the departure
                // report (panel findings, PR #671 runs 13-15).
                self.retiring.insert(key.clone(), held);
                for ch in self.channels.values_mut() {
                    if let Some(sync) = ch.sync.as_mut() {
                        sync.pending.remove(&key);
                        sync.departed.insert(key.clone());
                    }
                }
            }
        }
        let keys: Vec<String> = self.owned.keys().cloned().collect();
        let mut effects = Vec::new();
        for key in keys {
            // A nick the pool holds again is not retiring — the name is a
            // live puppet's — unless the listing is a DEPARTED puppet's
            // (`gone`) and the retiring entry under it is another
            // connection's story that moved onto the name: that one goes
            // on beside the listing and resolves on its own terms.
            let owner = self.owned.get(&key).map_or(0, |h| h.connection);
            let other = self.gone.contains(&key)
                && self
                    .retiring
                    .get(&key)
                    .is_some_and(|h| h.connection != 0 && owner != 0 && h.connection != owner);
            if !other {
                self.retiring.remove(&key);
            }
            if self.gone.contains(&key) {
                // Our puppet under this nick was seen to leave and the pool
                // has not released it yet: whoever holds the name now is a
                // human, tracked as one until the release frees the name.
                continue;
            }
            effects.extend(self.evict(&key));
        }
        effects
    }

    /// The puppet's OWN connection closed (the executor saw its socket end,
    /// after a QUIT or otherwise) and the pool has RELEASED its nick: the nick
    /// is off the server whether or not this connection ever shared a channel
    /// with it, so the retiring entry for it is resolved here. This is the
    /// ordered resolution for a puppet released before it joined anything —
    /// `mu-gw` never sees that QUIT. Tombstoned for open syncs like an
    /// observed QUIT.
    ///
    /// Only a RETIRING nick is resolved, and only the entry of the
    /// CONNECTION that departed: `connection` is the id the nick was held
    /// under ([`set_owned`](Self::set_owned)). A nick the pool holds may by
    /// now be a newer puppet's (the same peer re-registered, or another peer
    /// took the spelling), and so may a retiring entry, if the spelling went
    /// through a whole registration and release while this report was on its
    /// way; neither is this connection's to free. So the bridge releases
    /// first (the ownership sync), then reports: the release retires the
    /// nick under its connection, the report with that connection resolves
    /// it. A report for an owned nick, a nobody's nick, or another
    /// connection's retiring entry is nothing to act on.
    ///
    /// What was held back under the nick while it was retiring is replayed
    /// here, in order, as the human's who took the freed name. That rests on
    /// a guarantee the BRIDGE gives, not this type: it applies a confirmed
    /// report only once every line the server wrote to this connection
    /// before it closed the puppet's has been consumed (the departure
    /// barrier, `bridge::session`). Under it, a puppet that was in a shared
    /// channel has had its QUIT observed — broadcast when the server
    /// processed it, before the close — and `quit` resolved the entry and
    /// discarded what was its own before this is reached; so a retiring
    /// entry this still finds belonged to a puppet in no shared channel,
    /// nothing of whose could have arrived here. A report the server did
    /// not confirm goes through
    /// [`puppet_departed_unconfirmed`](Self::puppet_departed_unconfirmed).
    pub fn puppet_departed(&mut self, nick: &str, connection: u64) -> Vec<HumanEffect> {
        let Some(key) = self.retiring_of(nick, connection) else {
            return Vec::new();
        };
        self.forget_expectation(connection);
        let wire = self
            .retiring
            .remove(&key)
            .map(|h| h.wire)
            .unwrap_or_default();
        let held_back = self.deferred.remove(&key).unwrap_or_default();
        self.forget_story(&key);
        // The connection's rename window closes with it: every spelling it
        // vacated, and every name a holder of one moved on to, is settled
        // now — each story the spelling's new holder's, replayed as theirs
        // (panel finding, PR #671 run 15).
        let mut effects = self.settle(connection, None, true);
        // The name is nobody's from here: a snapshot line naming it that
        // predates this cannot be told from the departed puppet, so it is
        // tombstoned like an observed QUIT; the arrivals replayed below are
        // the newer facts.
        for ch in self.channels.values_mut() {
            if let Some(sync) = ch.sync.as_mut() {
                sync.pending.remove(&key);
                sync.departed.insert(key.clone());
            }
        }
        effects.extend(self.replay(held_back, &wire));
        effects
    }

    /// Replay a held-back story as ONE human's, under `name` — the name the
    /// story's entry has come to, since the entry moved with every NICK the
    /// story holds. The arrivals are projected onto that name: the holder was
    /// never fronted under the earlier ones, so nothing is renamed, and
    /// nothing is done under a spelling that may by now be somebody else's.
    /// An arrival older than a snapshot still open for its channel is
    /// replayed as live presence but not as that snapshot's evidence: the
    /// roster the snapshot commits decides whether the holder is there —
    /// it lists them (their line, no longer held back, commits them) or it
    /// does not (the commit withdraws them) — and an older arrival must not
    /// stand in for the line the snapshot did not carry (panel finding, PR
    /// #671 run 22). A departure in the story is departure evidence
    /// whatever its age (run 25).
    fn replay(&mut self, story: Vec<Arrival>, name: &str) -> Vec<HumanEffect> {
        let mut effects = Vec::new();
        for arrival in story {
            effects.extend(match arrival {
                Arrival::Joined(channel, _, account, gen) => {
                    self.arrived(&channel, name, account, Some(gen))
                }
                Arrival::Left(channel, _, _) => self.left(&channel, name),
                Arrival::Renamed(..) => Vec::new(),
            });
        }
        effects
    }

    /// The connection a puppet nick is held under — owned and not seen to
    /// leave, retiring, expected, or vacating (a spelling the puppet moved on
    /// from, still its connection's until the barrier) — if it is a
    /// puppet's at all. The bridge asks before applying a report about a
    /// connection to a nick: a human who took the spelling since is not that
    /// connection's to move.
    pub fn held_by(&self, nick: &str) -> Option<u64> {
        let key = self.fold(nick);
        if let Some(h) = self.owned.get(&key) {
            if !self.gone.contains(&key) {
                return Some(h.connection);
            }
        }
        self.retiring
            .get(&key)
            .map(|h| h.connection)
            .or_else(|| self.expected.get(&key).map(|(c, _)| *c))
            .or_else(|| self.vacating.get(&key).map(|h| h.connection))
    }

    /// Whether a NICK `from` → `to` on the wire is `connection`'s puppet
    /// renaming ITSELF: `from` is that connection's, and either no rename is
    /// pending under it (any NICK under a spelling the puppet holds is the
    /// puppet's) or `to` is where a reported rename is taking it. A NICK
    /// under a spelling the puppet has moved on from, onto anything else,
    /// is the spelling's new holder's — a human's. The bridge asks before
    /// letting the pool follow a NICK.
    pub fn is_puppets_nick(&self, from: &str, to: &str, connection: u64) -> bool {
        if self.held_by(from) != Some(connection) {
            return false;
        }
        let old = self.fold(from);
        if !self.vacating.contains_key(&old) {
            return true;
        }
        self.hop_of(&old) == Some(connection) && self.target_of(&self.fold(to)) == Some(connection)
    }

    /// The connection a spelling is a HOP of a pending rename for — one the
    /// puppet itself left (`puppet_renaming`) — as opposed to a name a
    /// holder of such a spelling went on to (`origin`), which is a human's.
    fn hop_of(&self, key: &str) -> Option<u64> {
        if self.origin.contains_key(key) {
            return None;
        }
        self.vacating.get(key).map(|h| h.connection)
    }

    /// The connection a spelling is the destination of a pending rename for:
    /// expected (the last hop) or a hop vacated again (an intermediate one,
    /// moved on from). A name a holder went on to is nobody's destination.
    fn target_of(&self, key: &str) -> Option<u64> {
        self.expected
            .get(key)
            .map(|(c, _)| *c)
            .or_else(|| self.hop_of(key))
    }

    /// Move `connection`'s entry — owned, or retiring — to the spelling `to`:
    /// the puppet is there on the server. Whatever key it was under (the
    /// pool's spelling, or where the wire moved it last) is tombstoned for
    /// open syncs; a departure remembered under the new spelling is an
    /// earlier holder's and is forgotten. Nothing for a connection with no
    /// entry (its report is behind its Ended).
    fn move_entry(&mut self, connection: u64, to: &str) {
        let new = self.fold(to);
        let owned_key = self
            .owned
            .iter()
            .find(|(k, h)| h.connection == connection && !self.gone.contains(*k))
            .map(|(k, _)| k.clone());
        let old = if let Some(key) = owned_key {
            let was = self.owned.remove(&key).expect("found above");
            let mut trail = was.trail;
            trail.push(was.wire);
            self.owned.insert(
                new.clone(),
                Held {
                    wire: to.to_string(),
                    connection,
                    trail,
                },
            );
            self.gone.remove(&new);
            key
        } else if let Some(key) = self.retiring_of("", connection) {
            self.retiring.remove(&key);
            self.retiring.insert(
                new.clone(),
                Held {
                    wire: to.to_string(),
                    connection,
                    trail: Vec::new(),
                },
            );
            key
        } else {
            return;
        };
        if old != new {
            for ch in self.channels.values_mut() {
                if let Some(sync) = ch.sync.as_mut() {
                    sync.pending.remove(&old);
                    sync.departed.insert(old.clone());
                }
            }
        }
    }

    /// Settle the vacating entries of `connection` — all of them, or those
    /// of one hop (`root`: a vacated spelling, and the entries its holders
    /// moved on to). Each entry's story is the spelling's new holder's and
    /// is replayed as theirs, under the name they go by; `replay = false`
    /// discards it instead (an unconfirmed departure: what arrived may have
    /// been the puppet, and nothing can tell).
    fn settle(&mut self, connection: u64, root: Option<&str>, replay: bool) -> Vec<HumanEffect> {
        let mut keys: Vec<(String, String)> = self
            .vacating
            .iter()
            .filter(|(k, h)| {
                h.connection == connection
                    && root
                        .is_none_or(|r| self.origin.get(*k).map_or(k.as_str(), String::as_str) == r)
            })
            .map(|(k, h)| (k.clone(), h.wire.clone()))
            .collect();
        keys.sort();
        let mut effects = Vec::new();
        for (key, wire) in keys {
            self.vacating.remove(&key);
            self.origin.remove(&key);
            let story = self.deferred.remove(&key).unwrap_or_default();
            if replay {
                effects.extend(self.replay(story, &wire));
            }
        }
        effects
    }

    /// [`puppet_departed`](Self::puppet_departed) for a departure the server
    /// did NOT confirm: the puppet's connection was cut at the grace without
    /// the server closing it, so nothing orders the server's view of that
    /// puppet behind the report. The retiring entry is resolved all the same
    /// — the name must not stay ours for good on a server that stopped
    /// answering — and what arrived under it so far is DISCARDED, not
    /// replayed: it may have been the puppet, and nothing can tell. What
    /// arrives under the name after this is a human's, as for any free name;
    /// that is the accepted residual of this path: an echo of the puppet's
    /// that the unanswering server emits later fronts the gateway's own
    /// puppet as a human until the same server broadcasts that puppet's
    /// QUIT to the same channel and withdraws it. No marker could tell that
    /// echo from a human who took the name, and holding the name against
    /// both is the worse outcome. A human who took the name in the window
    /// is picked up by the next snapshot of the channel.
    pub fn puppet_departed_unconfirmed(&mut self, nick: &str, connection: u64) {
        let Some(key) = self.retiring_of(nick, connection) else {
            return;
        };
        self.forget_expectation(connection);
        self.settle(connection, None, false);
        self.retiring.remove(&key);
        self.forget_story(&key);
        for ch in self.channels.values_mut() {
            if let Some(sync) = ch.sync.as_mut() {
                sync.pending.remove(&key);
                sync.departed.insert(key.clone());
            }
        }
    }

    /// Puppet nicks released by the pool whose departure this connection has
    /// not yet observed (wire spelling, sorted).
    pub fn retiring_nicks(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.retiring.values().map(|h| h.wire.as_str()).collect();
        v.sort_unstable();
        v
    }

    /// The puppet nicks currently owned, in their wire spelling.
    pub fn owned_nicks(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.owned.values().map(|h| h.wire.as_str()).collect();
        v.sort_unstable();
        v
    }

    /// Whether `nick` is one of the gateway's own nicks (its own or a puppet's).
    pub fn is_owned(&self, nick: &str) -> bool {
        self.is_own(&self.fold(nick))
    }

    /// Remove `key` from every roster and open sync, and withdraw it if it was
    /// present — the correction for a nick learned to be the gateway's own
    /// after it was seen.
    fn evict(&mut self, key: &str) -> Vec<HumanEffect> {
        for ch in self.channels.values_mut() {
            ch.members.remove(key);
            if let Some(sync) = ch.sync.as_mut() {
                sync.pending.remove(key);
                // Tombstoned too: if the pool releases the nick before this
                // sync ends, a delayed snapshot line must still not front it.
                sync.departed.insert(key.to_string());
            }
        }
        match self.forget_presence(key) {
            Some(peer) => vec![HumanEffect::Withdraw(peer)],
            None => Vec::new(),
        }
    }

    fn fold(&self, name: &str) -> String {
        fold_nick(name, self.cm)
    }

    /// The gateway itself joined `channel`: record the (empty) channel and open a
    /// NAMES sync for it. Returns the generation to tag this channel's incoming
    /// `Names`/`NamesEnd` with, so a later sync cannot be closed by an older
    /// burst. Idempotent apart from the fresh generation.
    ///
    /// The snapshot this opens is the channel's roster as of now: when it
    /// COMMITS (`names_end`), whatever was held back about the channel from
    /// before it was requested — a snapshot line of an earlier generation, a
    /// JOIN or PART under a retiring or vacating name — is superseded by
    /// it: a holder still there is listed again (and held back again, under
    /// the same rule), one who left is not, and a story replayed later must
    /// not re-add a name a newer roster showed absent (panel finding, PR
    /// #671 run 14). Until it commits, nothing is dropped: a story resolved
    /// while the snapshot is still on its way replays what it has, and the
    /// snapshot's own line then finds a live member (panel finding, PR #671
    /// run 21). Arrivals held back from here on are the snapshot's
    /// generation or newer, and are kept.
    pub fn self_joined(&mut self, channel: &str) -> u64 {
        self.gen += 1;
        let gen = self.gen;
        let folded = self.fold(channel);
        let entry = self.channels.entry(folded).or_insert_with(|| Channel {
            display: channel.to_string(),
            members: HashMap::new(),
            sync: Some(Sync::new(gen)),
            gen,
        });
        entry.sync = Some(Sync::new(gen));
        entry.gen = gen;
        gen
    }

    /// The generation an arrival about `folded_ch` is stamped with now: the
    /// channel's last opened snapshot.
    fn channel_gen(&self, folded_ch: &str) -> u64 {
        self.channels.get(folded_ch).map_or(0, |c| c.gen)
    }

    /// Drop the held-back arrivals about `folded_ch` from every story —
    /// those older than snapshot `gen` (a roster of that generation has
    /// committed and supersedes them), or all of them (`None`: the channel
    /// is no longer watched).
    fn forget_channel_stories(&mut self, folded_ch: &str, gen: Option<u64>) {
        let cm = self.cm;
        for story in self.deferred.values_mut() {
            story.retain(|a| match a {
                Arrival::Joined(c, _, _, g) | Arrival::Left(c, _, g) => {
                    fold_nick(c, cm) != folded_ch || gen.is_some_and(|gen| *g >= gen)
                }
                Arrival::Renamed(..) => true,
            });
        }
    }

    /// One `RPL_NAMREPLY` line for `channel` at generation `gen`. Names for a
    /// channel not currently syncing that generation are ignored as stale.
    pub fn names_reply(
        &mut self,
        channel: &str,
        gen: u64,
        nicks: impl IntoIterator<Item = (String, Option<String>)>,
    ) {
        let folded = self.fold(channel);
        let cm = self.cm;
        let self_nick = self.self_nick.folded().to_string();
        let owned = &self.owned;
        let retiring = &self.retiring;
        let gone = &self.gone;
        let vacating = &self.vacating;
        let expected = &self.expected;
        let deferred = &mut self.deferred;
        let Some(ch) = self.channels.get_mut(&folded) else {
            return;
        };
        let Some(sync) = ch.sync.as_mut().filter(|s| s.gen == gen) else {
            return;
        };
        for (nick, account) in nicks {
            let key = fold_nick(strip_prefixes(&nick), cm);
            // The gateway's own nick or one of its puppets (held and not seen
            // to leave, or released but not yet seen to leave): never a
            // human. The same test as `is_puppet`, spelled out for the borrow.
            // Under a RETIRING nick the line is held back like a JOIN is —
            // the departed puppet, or the name's new holder; see `deferred`.
            // Exactly `story_key`, spelled out for the borrow: vacating, or
            // retiring and not a live puppet's listing (not owned, or owned
            // but gone — a story beside a departed puppet's listed name).
            let entry = (vacating.contains_key(&key)
                || (retiring.contains_key(&key)
                    && (!owned.contains_key(&key) || gone.contains(&key))))
            .then(|| key.clone());
            let puppet = ((owned.contains_key(&key) || expected.contains_key(&key))
                && !gone.contains(&key))
                || retiring.contains_key(&key)
                || vacating.contains_key(&key);
            if key == self_nick || puppet {
                // Held back only if this connection did not watch the nick
                // leave after the snapshot was taken (`departed`): a stale
                // line replayed as an arrival would re-add a holder the wire
                // showed gone.
                if let Some(entry) = entry {
                    if !sync.departed.contains(&key) {
                        deferred.entry(entry).or_default().push(Arrival::Joined(
                            channel.to_string(),
                            strip_prefixes(&nick).to_string(),
                            account,
                            gen,
                        ));
                    }
                }
                continue;
            }
            if sync.departed.contains(&key) {
                // The gateway watched this nick leave after the server took the
                // snapshot this line comes from. The observed departure is the
                // newer fact; the snapshot entry is discarded.
                continue;
            }
            sync.pending.insert(
                key,
                Member {
                    display: strip_prefixes(&nick).to_string(),
                    account,
                },
            );
        }
    }

    /// `RPL_ENDOFNAMES`: commit the sync as the channel's authoritative roster,
    /// then reconcile human presence. A stale generation is ignored.
    pub fn names_end(&mut self, channel: &str, gen: u64) -> Vec<HumanEffect> {
        let folded = self.fold(channel);
        let Some(ch) = self.channels.get_mut(&folded) else {
            return Vec::new();
        };
        let Some(sync) = ch.sync.take_if(|s| s.gen == gen) else {
            return Vec::new();
        };
        // The committed roster supersedes what was held back about the
        // channel from before this snapshot was requested (`self_joined`).
        self.forget_channel_stories(&folded, Some(gen));
        let Some(ch) = self.channels.get_mut(&folded) else {
            return Vec::new();
        };
        // Diff the committed roster against what the channel held before, so a
        // re-sync that DROPS a member withdraws them (if it was their last
        // channel) rather than leaving stale presence, and one that adds a member
        // registers them. Members in both are unchanged.
        let old: Vec<String> = ch.members.keys().cloned().collect();
        ch.members = sync.pending;
        let added: Vec<(String, String)> = ch
            .members
            .iter()
            .filter(|(k, _)| !old.contains(k))
            .map(|(k, m)| (k.clone(), m.display.clone()))
            .collect();
        let removed: Vec<String> = old
            .into_iter()
            .filter(|k| !ch.members.contains_key(k))
            .collect();
        // `ch`'s borrow of self.channels ends here; the mark_* calls need &mut
        // self.present.
        let mut effects = Vec::new();
        for key in removed {
            effects.extend(self.mark_absent(&key, &folded));
        }
        for (key, display) in added {
            effects.extend(self.mark_present(&key, &display, &folded));
        }
        effects
    }

    /// A nick joined `channel`. Applies to the live roster and, if a sync is
    /// open, to its pending set so the interleaved event is not lost at commit.
    pub fn joined(
        &mut self,
        channel: &str,
        nick: &str,
        account: Option<String>,
    ) -> Vec<HumanEffect> {
        self.arrived(channel, nick, account, None)
    }

    /// [`joined`](Self::joined), with the generation the arrival was held
    /// back at when it is a replay (`seen`): an arrival older than the
    /// snapshot open for the channel is live presence, not that snapshot's
    /// evidence (`replay`).
    fn arrived(
        &mut self,
        channel: &str,
        nick: &str,
        account: Option<String>,
        seen: Option<u64>,
    ) -> Vec<HumanEffect> {
        let folded_ch = self.fold(channel);
        let key = self.fold(nick);
        if self.is_own(&key) {
            // Our own JOIN echo (`self_joined` already recorded the channel),
            // or a puppet of ours arriving: neither is a human. (A puppet
            // whose QUIT was seen is not "ours" here — `is_puppet` — so a JOIN
            // under its old name is a human's, not a resurrection.) Under a
            // RETIRING nick it is held back: the puppet's own echo, or a
            // human who took the freed name — see `deferred`.
            if let Some(entry) = self.story_key(&key) {
                let gen = self.channel_gen(&folded_ch);
                self.deferred
                    .entry(entry)
                    .or_default()
                    .push(Arrival::Joined(
                        channel.to_string(),
                        nick.to_string(),
                        account,
                        gen,
                    ));
            }
            return Vec::new();
        }
        let member = Member {
            display: nick.to_string(),
            account,
        };
        let Some(ch) = self.channels.get_mut(&folded_ch) else {
            return Vec::new();
        };
        ch.members.insert(key.clone(), member.clone());
        if let Some(sync) = ch.sync.as_mut() {
            // A live JOIN is newer than any tombstone from earlier in this
            // sync — and a replayed one, older than the snapshot, still
            // says the holder is there unless a later departure in the
            // story says otherwise (`departed` restores the tombstone),
            // so a snapshot line naming them is theirs; only the
            // snapshot's own evidence is left to the snapshot.
            sync.departed.remove(&key);
            if seen.is_none_or(|g| g >= sync.gen) {
                sync.pending.insert(key.clone(), member);
            }
        }
        self.mark_present(&key, nick, &folded_ch)
    }

    /// A nick left `channel` (PART or KICK). If it was the gateway itself, the
    /// whole channel is dropped and every human who was only there is withdrawn.
    pub fn left(&mut self, channel: &str, nick: &str) -> Vec<HumanEffect> {
        let folded_ch = self.fold(channel);
        let key = self.fold(nick);
        if key == self.self_nick.folded() {
            return self.drop_channel(&folded_ch);
        }
        if self.is_puppet(&key) {
            // A puppet leaving is the pool's business; the gateway is still in
            // the channel and no human moved. The departure is still recorded
            // for an open sync: if the pool later releases the nick, a delayed
            // snapshot line naming it must not front a human. A retiring
            // puppet stays retiring: a PART leaves its server connection up,
            // and only its QUIT ends the gap the retiring set covers.
            if let Some(sync) = self
                .channels
                .get_mut(&folded_ch)
                .and_then(|c| c.sync.as_mut())
            {
                sync.pending.remove(&key);
                sync.departed.insert(key.clone());
            }
            // Under a RETIRING nick, held back with the JOINs, so a holder
            // who came and went in the window is replayed as gone, not as a
            // member of a channel the wire showed them leaving.
            if let Some(entry) = self.story_key(&key) {
                let gen = self.channel_gen(&folded_ch);
                self.deferred.entry(entry).or_default().push(Arrival::Left(
                    channel.to_string(),
                    nick.to_string(),
                    gen,
                ));
            }
            return Vec::new();
        }
        let Some(ch) = self.channels.get_mut(&folded_ch) else {
            return Vec::new();
        };
        ch.members.remove(&key);
        if let Some(sync) = ch.sync.as_mut() {
            // A departure, replayed or live, is departure evidence for the
            // open snapshot: a line naming the holder after it is stale.
            sync.pending.remove(&key);
            sync.departed.insert(key.clone());
        }
        self.mark_absent(&key, &folded_ch)
    }

    /// A QUIT removes the nick from every channel at once.
    pub fn quit(&mut self, nick: &str) -> Vec<HumanEffect> {
        let key = self.fold(nick);
        if key == self.self_nick.folded() {
            return Vec::new();
        }
        if self.is_puppet(&key) {
            // A puppet's QUIT: no human moved, but the departure is tombstoned
            // in every open sync for the same reason as in `left`. For a
            // retiring puppet this IS the observed departure the release was
            // waiting on: the nick is nobody's from here, and the next event
            // naming it (a human's JOIN) is a human's. A QUIT under the name
            // the retiring holder renamed to ends that entry — it moved with
            // the NICK. For a still-owned puppet it is remembered, so the
            // pool's later release has nothing to wait for.
            let mut effects = Vec::new();
            match self.story_key(&key) {
                Some(entry) => {
                    // Under a VACATED spelling the QUIT is its holder's —
                    // the puppet's own report has it elsewhere — and a
                    // released entry filed under it (the pool's spelling,
                    // which the report had already left) is the puppet's,
                    // kept for the barrier to move; under a retiring name
                    // it is the observed departure the release was waiting
                    // on (panel finding, PR #679 run 2).
                    if !self.vacating.contains_key(&entry) {
                        // The connection is off the server: the departure
                        // resolves its whole window, not just this entry.
                        // Its remaining hops' stories are its holders' and
                        // are replayed as theirs, as their barriers would
                        // have, and nothing of it stays expected — else the
                        // vacated spellings would keep reading as ours and
                        // the expected one would be withheld from the human
                        // who takes it (panel finding, PR #679 run 9).
                        let connection = self.retiring.remove(&entry).map_or(0, |h| h.connection);
                        if connection != 0 {
                            self.forget_expectation(connection);
                            effects = self.settle(connection, None, true);
                        }
                    }
                    // Whatever arrived under the name before this QUIT was
                    // the puppet itself (its QUIT is broadcast after its
                    // JOIN) — or a holder who is gone now; nothing to front.
                    self.forget_story(&entry);
                }
                None => {
                    // Under an EXPECTED name — the puppet, under the name its
                    // last reported rename took it to, quit before the
                    // barrier applied it. The wire has told the whole story
                    // now: the renames happened, then the puppet left. So
                    // the window is settled HERE, from the wire, as the
                    // barriers would have settled it — every vacated
                    // spelling's story is its new holder's, replayed as
                    // theirs — the pool's entry moves to this name (owned:
                    // so the pool's catch-up sync finds the same connection
                    // under it and claims nothing; a released entry stays
                    // retiring for its departure report), and the name is
                    // free from here: gone. A late barrier finds nothing
                    // of the connection's left to move (panel findings,
                    // PR #671 runs 12-13).
                    if let Some((connection, to)) = self.expected.remove(&key) {
                        let owned_here = self
                            .owned
                            .iter()
                            .any(|(k, h)| h.connection == connection && !self.gone.contains(k));
                        if owned_here {
                            self.move_entry(connection, &to);
                        }
                        effects = self.settle(connection, None, true);
                    }
                    self.gone.insert(key.clone());
                    if let Some(h) = self.owned.get(&key) {
                        if h.connection != 0 {
                            self.gone_connections.insert(h.connection);
                        }
                    }
                }
            }
            for ch in self.channels.values_mut() {
                if let Some(sync) = ch.sync.as_mut() {
                    sync.pending.remove(&key);
                    sync.departed.insert(key.clone());
                }
            }
            return effects;
        }
        let channels: Vec<String> = self.channels.keys().cloned().collect();
        for folded_ch in &channels {
            if let Some(ch) = self.channels.get_mut(folded_ch) {
                ch.members.remove(&key);
                if let Some(sync) = ch.sync.as_mut() {
                    sync.pending.remove(&key);
                    sync.departed.insert(key.clone());
                }
            }
        }
        // One withdraw at most: the human is gone from everywhere.
        if let Some(peer) = self.forget_presence(&key) {
            return vec![HumanEffect::Withdraw(peer)];
        }
        Vec::new()
    }

    /// A NICK change, applied across every channel. When the folded identity
    /// actually changes, emits a single [`HumanEffect::Rename`] carrying the
    /// routing-memory transfer; a case-only change under folding is the same
    /// identity and emits nothing.
    ///
    /// A NICK seen on the wire under a RETIRING nick: the entry and its
    /// held-back story move to the new spelling (the holder — the departed
    /// puppet, or the name's new holder, settled when the entry resolves —
    /// answers to it from here; a live event under it must not run ahead of
    /// the held-back ones), the rename joins the story, and the vacated
    /// spelling is free: a JOIN under it is a new occupant's, at once, and a
    /// snapshot line naming it is tombstoned. A report about the connection
    /// still finds the entry by the connection, whatever it is spelled.
    pub fn renamed(&mut self, from: &str, to: &str) -> Vec<HumanEffect> {
        let old = self.fold(from);
        let new = self.fold(to);
        let mut effects = Vec::new();
        if new != old {
            effects = self.taken_by_nick(&new);
        }
        // The puppet's OWN NICK, seen on the wire, for a rename its
        // connection reported and the barrier has not yet applied — `from`
        // is the connection's and `to` is where a reported rename takes it
        // (the last hop's expected name, or an intermediate one vacated
        // again since): the puppet shared a channel, so what was held back
        // under the old spelling was the puppet — discarded — and the
        // rename is applied now, from the wire, which is in order by
        // construction. The connection's entry — owned or retiring, under
        // the pool's spelling or wherever the wire moved it last — moves to
        // `to`; a hop the wire settles this way leaves its barrier nothing.
        if let Some(connection) = self.hop_of(&old) {
            if self.target_of(&new) == Some(connection) {
                // The name it moves to may be ANOTHER connection's vacated
                // hop — the server gave that name away, so that rename went
                // through and its hop settles here, before this entry takes
                // the name (panel finding, PR #679 run 12).
                if self.hop_of(&new).is_some_and(|c| c != connection) {
                    effects.extend(self.free_hop(&new));
                }
                self.vacating.remove(&old);
                self.origin.remove(&old);
                self.forget_story(&old);
                self.expected.remove(&new);
                self.move_entry(connection, to);
                return effects;
            }
        }
        // The puppet's own NICK from its EXPECTED name — the wire ahead of
        // the barrier for the hop that brought it there, and of the report
        // of this rename: an expected name is the puppet's on the server,
        // so this is nobody else's. The entry and the expectation move on
        // to where the NICK takes it (a human this connection still lists
        // there is stale), the vacated name is free at once, and the older
        // hop's barrier finds the entry already elsewhere and moves nothing
        // (panel finding, PR #679 run 7).
        if !self.vacating.contains_key(&old) {
            if new == old {
                if let Some(e) = self.expected.get_mut(&old) {
                    e.1 = to.to_string();
                    return effects;
                }
            } else if let Some((connection, _)) = self.expected.remove(&old) {
                // As on the reported-rename path: the name it moves to may
                // be ANOTHER connection's vacated hop, whose rename the
                // server has therefore accepted — that hop settles first,
                // or its entry would be overwritten here (panel finding,
                // PR #679 run 13).
                if self.hop_of(&new).is_some_and(|c| c != connection) {
                    effects.extend(self.free_hop(&new));
                }
                self.expected
                    .insert(new.clone(), (connection, to.to_string()));
                self.gone.remove(&new);
                effects.extend(self.evict(&new));
                self.move_entry(connection, to);
                // The spelling the wire showed the puppet leave is nobody's
                // until someone takes it, and no longer expected: a
                // snapshot line naming it predates the NICK, so it is
                // tombstoned like any vacated spelling's. The expectation's
                // own eviction covered only the syncs open when the rename
                // was reported; one opened since would front the puppet's
                // pre-rename echo as a human (panel finding, PR #679 run 8).
                for ch in self.channels.values_mut() {
                    if let Some(sync) = ch.sync.as_mut() {
                        sync.pending.remove(&old);
                        sync.departed.insert(old.clone());
                    }
                }
                return effects;
            }
        }
        // The gateway's own NICK, or a live puppet's (a server can force
        // one), ONTO a spelling ANOTHER connection vacated: the server gave
        // the name away, so that connection's rename went through — its
        // hop settles here on the wire's word, the entry moving on to the
        // name its report expects. What was held under the vacated spelling
        // itself was a holder's who is gone now: nothing to front. A name a
        // holder went ON to from it is a live fact — they renamed away and
        // may well be present — replayed as theirs, as the barrier would
        // have. Then the rename applies as any own NICK does, below. A
        // story's holder — a name held back under a hop — is not "own"
        // here: their NICK moves their story (panel findings, PR #679 runs
        // 3 and 5).
        let own_live = old == self.self_nick.folded()
            || (self.owned.contains_key(&old) && !self.gone.contains(&old));
        if new != old && own_live {
            if self.hop_of(&new).is_some() {
                effects.extend(self.free_hop(&new));
            } else if self.vacating.remove(&new).is_some() {
                self.origin.remove(&new);
                self.forget_story(&new);
            }
        }
        if new != old
            && !own_live
            && self.story_key(&old).is_none()
            && self.vacating.contains_key(&new)
        {
            // A human's NICK ONTO a spelling the puppet has moved on from,
            // before the barrier: the server took it, so the spelling is
            // theirs — but it is held back like a JOIN under it would be,
            // and settled the same way at the barrier, because the sync
            // still owns the spelling until then and would evict a human it
            // found present under it, and every event under it is held
            // back as the story's. So the human leaves under the old name
            // — a departure in every open sync, like a QUIT, and withdrawn
            // where they were present — and arrives under the new one in
            // the story, in each channel the wire had them in: a committed
            // member, or one a snapshot still on its way has listed
            // (`pending`), which must not commit under the old name. They
            // are fronted as the spelling's new holder when the hop
            // settles. Their routing memory does not carry across; the
            // window is the price (panel findings, PR #671 run 16 and PR
            // #679 run 1).
            let mut arrived: Vec<(String, String, Option<String>)> = Vec::new();
            for (folded_ch, ch) in self.channels.iter_mut() {
                let mut there = ch.members.remove(&old).map(|m| m.account);
                if let Some(sync) = ch.sync.as_mut() {
                    if let Some(m) = sync.pending.remove(&old) {
                        there = there.or(Some(m.account));
                    }
                    sync.departed.insert(old.clone());
                }
                if let Some(account) = there {
                    arrived.push((folded_ch.clone(), ch.display.clone(), account));
                }
            }
            for (folded_ch, display, account) in arrived {
                effects.extend(self.mark_absent(&old, &folded_ch));
                let gen = self.channel_gen(&folded_ch);
                self.deferred
                    .entry(new.clone())
                    .or_default()
                    .push(Arrival::Joined(display, to.to_string(), account, gen));
            }
            return effects;
        }
        if let Some(entry) = self.story_key(&old) {
            // A NICK onto a name the owned set still lists for a DEPARTED
            // puppet (`gone`) is a NICK like any other here: the entry and
            // its story move onto it, beside that listing — whether the
            // holder is a human who took the retiring name or the retiring
            // puppet itself, force-renamed before its QUIT, nothing here
            // can tell, and the story's resolution (the observed QUIT, or
            // the departure report) decides as it always does (panel
            // findings, PR #671 runs 17-18).
            // The story's holder answers to `to` from here: the entry and
            // its story move with it, the vacated spelling is free — a new
            // occupant under it is a human at once, a snapshot line naming
            // it is tombstoned — and a report about the connection still
            // finds the entry by the connection.
            self.deferred
                .entry(entry.clone())
                .or_default()
                .push(Arrival::Renamed(from.to_string(), to.to_string()));
            if new != old {
                // A spelling both VACATING and retiring — the pool released
                // the connection under a spelling its own report had
                // already left — is the hop's: whoever renames from it is
                // its new holder, on the hop's window, and the released
                // entry stays under the pool's spelling for the barrier to
                // move (panel finding, PR #679 run 2).
                let moved = if let Some(held) = self.vacating.remove(&old) {
                    Some((held, true))
                } else {
                    self.retiring.remove(&old).map(|held| (held, false))
                };
                // Onto a name already held under a hop — another
                // connection's vacated spelling, or a name in its lineage —
                // the holder JOINS that hop's story, settled by that hop's
                // barrier as the name's new holder; the hop's own entry
                // stands, whichever connection's it is (panel finding, PR
                // #679 run 6).
                let joins = self.vacating.contains_key(&new);
                if let Some((held, was_vacating)) = moved {
                    let held = Held {
                        wire: to.to_string(),
                        connection: held.connection,
                        trail: Vec::new(),
                    };
                    if was_vacating {
                        // The vacated spelling stays VACATING under the old
                        // key — whoever takes it next starts a story of their
                        // own under it, and the sync still sees the pool's
                        // spelling — and the holder's story moves to the
                        // name they go by, held as theirs, descending from
                        // the vacated spelling so its barrier settles both.
                        let root = self
                            .origin
                            .get(&old)
                            .cloned()
                            .unwrap_or_else(|| old.clone());
                        // Back onto the vacated spelling itself: that is the
                        // hop's own name again, not a name descending from it.
                        if new != root && !joins {
                            self.origin.insert(new.clone(), root);
                        }
                        self.vacating.insert(
                            old.clone(),
                            Held {
                                wire: from.to_string(),
                                connection: held.connection,
                                trail: Vec::new(),
                            },
                        );
                        if !joins {
                            self.vacating.insert(new.clone(), held);
                        }
                    } else {
                        self.retiring.insert(new.clone(), held);
                    }
                }
                if let Some(mut story) = self.deferred.remove(&old) {
                    // The NICK is a live fact about the holder, newer than any
                    // snapshot still on its way: where the story has them is
                    // where they are NOW, so it is re-stamped with each
                    // channel's current generation — a snapshot taken before
                    // the rename lists them under the old spelling, which is
                    // tombstoned, and must not commit them away with it
                    // (panel finding, PR #671 run 23).
                    for arrival in story.iter_mut() {
                        match arrival {
                            Arrival::Joined(c, _, _, g) | Arrival::Left(c, _, g) => {
                                *g = self.channel_gen(&fold_nick(c, self.cm));
                            }
                            Arrival::Renamed(..) => {}
                        }
                    }
                    self.deferred.entry(new).or_default().extend(story);
                }
                for ch in self.channels.values_mut() {
                    if let Some(sync) = ch.sync.as_mut() {
                        sync.pending.remove(&old);
                        sync.departed.insert(old.clone());
                    }
                }
            } else if let Some(held) = self
                .vacating
                .get_mut(&old)
                .or_else(|| self.retiring.get_mut(&old))
            {
                held.wire = to.to_string();
            }
            return effects;
        }
        effects.extend(self.rename(from, to));
        effects
    }

    /// The puppet's OWN connection reported a rename, and the bridge will
    /// apply it at its barrier: from now until then the old spelling is
    /// VACATING (still ours; what the wire says under it is held back) and
    /// the new one is EXPECTED (ours on the server already; a JOIN under it
    /// is the puppet's). A rename reported while one is pending — the
    /// puppet renamed again before the first barrier — vacates the
    /// intermediate spelling in turn: it was expected, and is held back
    /// from here like any vacated one. Nothing when membership no longer
    /// files `from` under `connection`. The effects: a human this connection
    /// still lists under the new spelling is stale — the server says the
    /// spelling is ours — and is withdrawn.
    pub fn puppet_renaming(&mut self, from: &str, to: &str, connection: u64) -> Vec<HumanEffect> {
        // `0` is the id-less form's connection ([`set_owned_nicks`]), which
        // the window's own bookkeeping cannot tell apart from another's; a
        // report never carries it.
        if connection == 0 || self.held_by(from) != Some(connection) {
            return Vec::new();
        }
        let old = self.fold(from);
        let new = self.fold(to);
        if new == old {
            // A case-only rename is the same name under the server's rule:
            // the entry's spelling follows, and no window opens — one key
            // in both `vacating` and `expected` would read as a hop's
            // holder's on the puppet's own QUIT (panel finding, PR #679
            // run 4).
            if let Some(h) = self
                .owned
                .get_mut(&old)
                .or_else(|| self.retiring.get_mut(&old))
            {
                h.wire = to.to_string();
            }
            return Vec::new();
        }
        if !self.vacating.contains_key(&old) {
            self.expected.remove(&old);
            self.vacating.insert(
                old,
                Held {
                    wire: from.to_string(),
                    connection,
                    trail: Vec::new(),
                },
            );
        }
        // Back onto a spelling this connection vacated (a → b → a before
        // the barrier): the server gave it back, so whoever held it
        // meanwhile is gone, and that hop's window closes here — the
        // spelling is the puppet's again, not a hop's holder's. A name
        // that descended from the hop still settles at its barrier (panel
        // finding, PR #679 run 4).
        if self.hop_of(&new) == Some(connection) {
            self.vacating.remove(&new);
            self.forget_story(&new);
        }
        self.gone.remove(&new);
        let effects = self.evict(&new);
        self.expected.insert(new, (connection, to.to_string()));
        effects
    }

    /// The rename a puppet's own connection reported, applied at the
    /// bridge's barrier: that hop's vacated spelling — and the names its
    /// holders moved on to — is settled: each story is the spelling's new
    /// holder's (the puppet was in no shared channel, or the wire's own
    /// NICK would have settled it first) and is replayed as theirs, under
    /// the name they go by; and the connection's entry moves to the new
    /// spelling when membership still files it under the old one (the
    /// wire's NICK may have moved it already). Only a hop still pending is
    /// applied: one the wire's NICK settled, or the puppet's QUIT under its
    /// expected name, or its departure report, has nothing left to move —
    /// and a released entry is never moved onto a name a human may hold by
    /// now (panel findings, PR #671 runs 13-15). The effects are the
    /// replays.
    pub fn puppet_renamed(&mut self, from: &str, to: &str, connection: u64) -> Vec<HumanEffect> {
        let old = self.fold(from);
        let new = self.fold(to);
        let pending = self.vacating.get(&old).map(|h| h.connection) == Some(connection)
            || self.target_of(&new) == Some(connection);
        if !pending {
            return Vec::new();
        }
        if self.expected.get(&new).map(|(c, _)| *c) == Some(connection) {
            self.expected.remove(&new);
        }
        // The entry first, so a replay under the vacated spelling finds it
        // free; then the hop's stories.
        let moves = self.held_by(from) == Some(connection)
            && (self.owned.contains_key(&old) || self.retiring.contains_key(&old));
        if moves {
            self.move_entry(connection, to);
        }
        self.settle(connection, Some(&old), true)
    }

    /// The hop `key` is a vacated spelling of: its name has been given away
    /// on the wire, so that connection's rename went through. Its entry
    /// moves on to the name its report expects, and the hop settles — its
    /// stories are their holders', replayed as theirs, as its barrier would
    /// have (the vacated spelling's own holder is gone: the name was free).
    fn free_hop(&mut self, key: &str) -> Vec<HumanEffect> {
        let Some(connection) = self.hop_of(key) else {
            return Vec::new();
        };
        let target = self
            .expected
            .iter()
            .find(|(_, (c, _))| *c == connection)
            .map(|(k, (_, wire))| (k.clone(), wire.clone()));
        if let Some((expected, wire)) = target {
            self.expected.remove(&expected);
            self.move_entry(connection, &wire);
        }
        self.vacating.remove(key);
        self.forget_story(key);
        self.settle(connection, Some(key), true)
    }

    fn rename(&mut self, from: &str, to: &str) -> Vec<HumanEffect> {
        let old = self.fold(from);
        let new = self.fold(to);
        // Only a puppet still on the server moves its ownership: a human who
        // took a departed puppet's name before the pool's release (owned but
        // `gone`) renames as a human, and the release frees the old spelling.
        let (was_owned, was_retiring) = if self.is_puppet(&old) {
            (self.owned.remove(&old), self.retiring.remove(&old))
        } else {
            (None, None)
        };
        if was_owned.is_some() || was_retiring.is_some() {
            // A puppet renamed (a server can force a NICK): it stays ours under
            // the new spelling and is still not a human. The vacated spelling
            // gets the same tombstone the gateway's own rename leaves: it is no
            // longer in the owned set, so a delayed snapshot line naming it
            // would otherwise land in `pending` and be fronted as a human.
            let held = |h: Option<Held>| Held {
                wire: to.to_string(),
                connection: h.map_or(0, |h| h.connection),
                trail: Vec::new(),
            };
            if let Some(h) = was_owned {
                let mut moved = held(Some(h.clone()));
                moved.trail = h.trail;
                moved.trail.push(h.wire);
                self.owned.insert(new.clone(), moved);
                // A live puppet now holds the new spelling, whatever
                // departure of an earlier holder was remembered under it.
                self.gone.remove(&new);
            } else {
                self.retiring.insert(new.clone(), held(was_retiring));
            }
            if old != new {
                for ch in self.channels.values_mut() {
                    if let Some(sync) = ch.sync.as_mut() {
                        sync.pending.remove(&old);
                        sync.departed.insert(old.clone());
                    }
                }
            }
            return Vec::new();
        }
        if old == self.self_nick.folded() {
            // The gateway renamed itself: track the new self by its WIRE
            // spelling, so a later CASEMAPPING change re-derives correctly.
            self.self_nick.rename(to, self.cm);
            if old != new {
                // `names_reply` recognizes the gateway by its CURRENT folded
                // nick alone, so a snapshot line still naming the PRE-rename
                // spelling is no longer filtered: it lands in `pending` and
                // `names_end` fronts the gateway's own vacated nick as a human.
                // The old spelling left the channel as far as anyone watching
                // can tell, so it is a departure like any other — tombstone it
                // for the rest of every open sync. A real human who takes the
                // freed nick arrives by JOIN or NICK, and that live event is
                // newer still and clears the tombstone.
                for ch in self.channels.values_mut() {
                    if let Some(sync) = ch.sync.as_mut() {
                        sync.pending.remove(&old);
                        sync.departed.insert(old.clone());
                    }
                }
            }
            return Vec::new();
        }
        // Move the member entry in every channel it appears in.
        //
        // A NICK is evidence about ONE user, so it may only rewrite the sync
        // state of the channels that user is actually in — a live member, or an
        // entry the open snapshot has already named. Clearing the destination
        // nick's tombstone everywhere let a rename in one channel resurrect the
        // PREVIOUS holder of that nick in another: observe `bob` QUIT during an
        // open `#a` sync, watch `alice` in `#b` take the freed nick, and a
        // delayed `#a` snapshot line naming the old bob commits as membership
        // for the new one, in a channel they never joined.
        let channels: Vec<String> = self.channels.keys().cloned().collect();
        let mut in_channels: HashSet<String> = HashSet::new();
        for folded_ch in &channels {
            if let Some(ch) = self.channels.get_mut(folded_ch) {
                let moved = ch.members.remove(&old).map(|mut m| {
                    m.display = to.to_string();
                    m
                });
                if let Some(m) = moved.clone() {
                    ch.members.insert(new.clone(), m);
                    in_channels.insert(folded_ch.clone());
                }
                if let Some(sync) = ch.sync.as_mut() {
                    if old != new {
                        // A NICK away from `old` retires that nick server-wide,
                        // whether or not this channel has seen the user yet: a
                        // user who renames during the INITIAL sync, before their
                        // own NAMES line arrives, is in neither `members` nor
                        // `pending`, and a delayed snapshot line naming the old
                        // nick would otherwise commit it as present. So the old
                        // identity is a departure in every open sync, like a
                        // QUIT; the NEW identity is handled below, and only
                        // where this user is actually known.
                        sync.departed.insert(old.clone());
                    }
                    // Carry the identity across in the snapshot too. The old
                    // identity is often known only from `members` — the user
                    // renamed before their NAMES line arrived — and the
                    // committed roster is evidence of membership just as good as
                    // a pending entry, so seed `pending` from the live member
                    // when the snapshot has not reached them yet. Without this
                    // the rename tombstones the old name, the late snapshot
                    // entry is discarded, and `names_end` withdraws a member who
                    // never left.
                    let carried = sync
                        .pending
                        .remove(&old)
                        .map(|mut m| {
                            m.display = to.to_string();
                            m
                        })
                        .or(moved);
                    if let Some(m) = carried {
                        sync.pending.insert(new.clone(), m);
                        if old != new {
                            // The new identity is live here as of now.
                            sync.departed.remove(&new);
                        }
                    }
                }
            }
        }
        if old == new || in_channels.is_empty() {
            // Same identity, or the renamer was not a tracked member.
            if old != new {
                self.present.remove(&old);
                if !in_channels.is_empty() {
                    self.present.insert(new.clone(), in_channels);
                }
            }
            return Vec::new();
        }
        self.present.remove(&old);
        self.present.insert(new.clone(), in_channels);
        vec![HumanEffect::Rename {
            from: PeerId::human(old),
            to: PeerId::human(new),
        }]
    }

    /// The connection dropped, or discovery must restart: forget everything and
    /// withdraw every currently-present human. Fresh NAMES rebuilds from empty.
    pub fn reset(&mut self) -> Vec<HumanEffect> {
        self.channels.clear();
        // The puppets die with the connection that owned them; the next
        // session's pool starts empty and hands over a fresh owned set.
        self.owned.clear();
        self.retiring.clear();
        self.gone.clear();
        self.gone_connections.clear();
        self.deferred.clear();
        self.vacating.clear();
        self.expected.clear();
        self.origin.clear();
        let effects = self
            .present
            .keys()
            .map(|k| HumanEffect::Withdraw(PeerId::human(k.clone())))
            .collect();
        self.present.clear();
        effects
    }

    /// The server changed `CASEMAPPING` to `cm`. Every folded key the view holds
    /// is now derived under the wrong rule, so each one is RE-DERIVED from the
    /// wire spelling it was built from — the channel from its display name, each
    /// member from theirs.
    ///
    /// The channel SET is kept, because a mapping change does not move the
    /// gateway out of anything: it is still in exactly the channels it was in,
    /// and a view that forgot them could never be repaired (a JOIN for a channel
    /// the client already occupies produces no self-JOIN echo and no NAMES, so
    /// nothing would ever re-open a sync). The rosters are kept only as a
    /// starting point — the caller re-opens a NAMES generation per channel via
    /// [`self_joined`](Self::self_joined) and the fresh burst commits over them —
    /// so any open sync is dropped here rather than committed under two rules.
    ///
    /// Folding is lossy, so a re-derivation can MOVE a human: `alice[]` is
    /// `alice{}` under rfc1459 and `alice[]` under ascii, which are two different
    /// `human:` peer ids. That is reported as a [`HumanEffect::Rename`], the same
    /// effect a NICK produces, because it is the same thing from the mesh's side.
    /// A fold that collides two humans onto one key withdraws the loser.
    pub fn set_casemapping(&mut self, cm: CaseMapping) -> Vec<HumanEffect> {
        if cm == self.cm {
            return Vec::new();
        }
        let old_cm = self.cm;
        self.cm = cm;
        // Re-DERIVED from the wire spelling, not re-folded from the previous
        // folded value: folding is lossy, so re-folding would silently change
        // who the gateway thinks it is.
        self.self_nick.set_casemapping(cm);
        let self_folded = self.self_nick.folded().to_string();
        // The owned and retiring sets re-derive from wire spellings for the
        // same reason. Two spellings that fold equal under the new rule
        // cannot both be kept under one key: the lexicographically earlier
        // spelling survives — the choice the pool's nick table makes, so
        // the two agree on which puppet stays — except that a LIVE owned
        // puppet is kept over one whose departure was seen. `gone` marks
        // owned keys, so it re-derives from the owned wire spellings it
        // marked: forgetting it instead would make a puppet already seen to
        // leave "ours" again, and a human who took its name would be
        // dropped from the rosters as a puppet and the pool's release would
        // retire the name for a QUIT that was already seen. It marks a
        // merged key only when EVERY spelling under it was seen to leave,
        // so a departed connection cannot mark a live one gone (panel
        // finding, PR #671).
        let was_gone = std::mem::take(&mut self.gone);
        let mut owned: Vec<(String, Held, bool)> = std::mem::take(&mut self.owned)
            .into_iter()
            .map(|(key, held)| (fold_nick(&held.wire, cm), held, was_gone.contains(&key)))
            .collect();
        // Live spellings first, then by spelling: the survivor is the
        // earliest live one, and its key is gone only if none was live.
        owned.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.1.wire.cmp(&b.1.wire)));
        for (key, held, gone) in owned {
            if self.owned.contains_key(&key) {
                continue;
            }
            if gone {
                self.gone.insert(key.clone());
            }
            self.owned.insert(key, held);
        }
        // Held-back arrivals re-key with the entry they were held under: the
        // retiring or vacating entry's wire spelling. Two such spellings that
        // now fold equal were two NAMES under the old rule — two holders'
        // stories — and cannot be one holder's under the new: the survivor's
        // story goes on, the loser's is discarded, as an unconfirmed
        // report's is. Only the loser connection's own report could have
        // said its story was a human's and not the puppet's own echo, and
        // after the merge that report resolves nothing; what the name's
        // holder is doing now, the snapshot the bridge asks for on every
        // mapping change says (panel finding, PR #671 run 29).
        let mut stories = std::mem::take(&mut self.deferred);
        let mut retiring: Vec<(String, Held)> =
            std::mem::take(&mut self.retiring).into_iter().collect();
        retiring.sort_by(|a, b| a.1.wire.cmp(&b.1.wire));
        for (old_key, held) in retiring {
            let key = fold_nick(&held.wire, cm);
            if self.retiring.contains_key(&key) {
                continue;
            }
            if let Some(story) = stories.remove(&old_key) {
                self.deferred.insert(key.clone(), story);
            }
            self.retiring.insert(key, held);
        }
        // The rename window's sets re-derive the same way, a losing hop's
        // story discarded with it. `origin` links folded keys: both ends
        // follow their vacating entries' spellings, and a link through a
        // losing hop goes with the hop — a name that descended from it is
        // a hop of its own from here, settled when its connection departs.
        let mut vacating: Vec<(String, Held)> =
            std::mem::take(&mut self.vacating).into_iter().collect();
        vacating.sort_by(|a, b| a.1.wire.cmp(&b.1.wire));
        let mut vacated: HashMap<String, String> = HashMap::new();
        for (old_key, held) in vacating {
            let key = fold_nick(&held.wire, cm);
            if self.vacating.contains_key(&key) {
                continue;
            }
            vacated.insert(old_key.clone(), key.clone());
            if let Some(story) = stories.remove(&old_key) {
                self.deferred.insert(key.clone(), story);
            }
            self.vacating.insert(key, held);
        }
        let mut expected: Vec<(u64, String)> =
            std::mem::take(&mut self.expected).into_values().collect();
        expected.sort_by(|a, b| a.1.cmp(&b.1));
        for (connection, wire) in expected {
            self.expected
                .entry(fold_nick(&wire, cm))
                .or_insert((connection, wire));
        }
        let mut origin: Vec<(String, String)> =
            std::mem::take(&mut self.origin).into_iter().collect();
        origin.sort();
        for (hop, root) in origin {
            if let (Some(hop), Some(root)) = (vacated.get(&hop), vacated.get(&root)) {
                self.origin
                    .entry(hop.clone())
                    .or_insert_with(|| root.clone());
            }
        }

        // Re-key the channels and their rosters from the spellings the wire gave.
        let mut rekeyed: HashMap<String, Channel> = HashMap::new();
        let mut moved: HashMap<String, String> = HashMap::new();
        for channel in std::mem::take(&mut self.channels).into_values() {
            let entry = rekeyed
                .entry(fold_nick(&channel.display, cm))
                .or_insert_with(|| Channel {
                    display: channel.display.clone(),
                    members: HashMap::new(),
                    sync: None,
                    gen: channel.gen,
                });
            for member in channel.members.into_values() {
                let new_key = fold_nick(&member.display, cm);
                // A nick that folds onto the gateway's own identity — or onto
                // one of its puppets — under the new rule is not a human to front.
                if new_key == self_folded || self.is_puppet(&new_key) {
                    continue;
                }
                moved.insert(fold_nick(&member.display, old_cm), new_key.clone());
                entry.members.insert(new_key, member);
            }
        }
        self.channels = rekeyed;

        // Presence is a projection of the rosters, so it is rebuilt from them
        // rather than re-keyed separately and allowed to disagree.
        let mut before: Vec<String> = self.present.keys().cloned().collect();
        before.sort();
        self.present = HashMap::new();
        for (folded_ch, channel) in &self.channels {
            for key in channel.members.keys() {
                self.present
                    .entry(key.clone())
                    .or_default()
                    .insert(folded_ch.clone());
            }
        }

        // Identities the new rule leaves exactly where they were are settled
        // FIRST, so a collision is always reported as the newcomer losing rather
        // than as a rename onto an identity that never moved.
        let stayed: HashSet<String> = before
            .iter()
            .filter(|k| moved.get(*k).is_some_and(|new| new == *k))
            .cloned()
            .collect();
        let mut claimed = stayed.clone();
        let mut effects = Vec::new();
        for old_key in &before {
            if stayed.contains(old_key) {
                continue;
            }
            match moved.get(old_key) {
                Some(new_key)
                    if self.present.contains_key(new_key) && !claimed.contains(new_key) =>
                {
                    claimed.insert(new_key.clone());
                    effects.push(HumanEffect::Rename {
                        from: PeerId::human(old_key.clone()),
                        to: PeerId::human(new_key.clone()),
                    });
                }
                // Folded onto someone else's identity, or onto the gateway's:
                // either way this human is no longer addressable.
                _ => effects.push(HumanEffect::Withdraw(PeerId::human(old_key.clone()))),
            }
        }
        let mut after: Vec<String> = self.present.keys().cloned().collect();
        after.sort();
        for new_key in after {
            if !claimed.contains(&new_key) && !before.contains(&new_key) {
                effects.push(HumanEffect::Register(PeerId::human(new_key)));
            }
        }
        effects
    }

    /// Whether `nick` is currently observed present in any channel the gateway is
    /// in — the authority routing checks before disclosing a private body.
    pub fn is_present(&self, nick: &str) -> bool {
        self.present.contains_key(&self.fold(nick))
    }

    /// The nick as last seen on the wire for a present human, folded key in,
    /// display out — what a private PRIVMSG is addressed to. `None` if the human
    /// is not currently observed in any channel.
    pub fn display_nick(&self, nick: &str) -> Option<String> {
        let key = self.fold(nick);
        let set = self.present.get(&key)?;
        // Any channel they are in carries their current display nick.
        set.iter()
            .find_map(|ch| self.channels.get(ch)?.members.get(&key))
            .map(|m| m.display.clone())
    }

    /// Every human currently observed present, as the `human:` peer id the mesh
    /// fronts for them.
    ///
    /// The executor's answer to "who should have a mesh endpoint right now" —
    /// which is a question with an answer even when a registration failed or a
    /// mesh connection dropped, because presence is what IRC says it is and this
    /// view never stopped saying it. Sorted, so a reconnect's effects are
    /// reproducible.
    pub fn present_humans(&self) -> Vec<PeerId> {
        let mut keys: Vec<&String> = self.present.keys().collect();
        keys.sort();
        keys.into_iter().map(|k| PeerId::human(k.clone())).collect()
    }

    /// The channels a human is currently observed in (folded names), or empty.
    pub fn channels_of(&self, nick: &str) -> Vec<String> {
        self.present
            .get(&self.fold(nick))
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// The folded names of channels the gateway is currently in.
    pub fn joined_channels(&self) -> Vec<String> {
        self.channels.keys().cloned().collect()
    }

    /// The display name of a joined channel, by folded key.
    pub fn channel_display(&self, folded: &str) -> Option<&str> {
        self.channels.get(folded).map(|c| c.display.as_str())
    }

    // — presence bookkeeping —

    /// Record `key` as present in `folded_ch`; emit `Register` on 0→present.
    fn mark_present(&mut self, key: &str, display: &str, folded_ch: &str) -> Vec<HumanEffect> {
        let set = self.present.entry(key.to_string()).or_default();
        let first = set.is_empty();
        set.insert(folded_ch.to_string());
        let _ = display; // display is carried on the Member; identity is the key
        if first {
            vec![HumanEffect::Register(PeerId::human(key.to_string()))]
        } else {
            Vec::new()
        }
    }

    /// Record `key` as no longer in `folded_ch`; emit `Withdraw` on →absent.
    fn mark_absent(&mut self, key: &str, folded_ch: &str) -> Vec<HumanEffect> {
        let Some(set) = self.present.get_mut(key) else {
            return Vec::new();
        };
        set.remove(folded_ch);
        if set.is_empty() {
            self.present.remove(key);
            return vec![HumanEffect::Withdraw(PeerId::human(key.to_string()))];
        }
        Vec::new()
    }

    /// Forget a human's presence entirely, returning their peer id iff they were
    /// present (so the caller can withdraw them exactly once).
    fn forget_presence(&mut self, key: &str) -> Option<PeerId> {
        self.present
            .remove(key)
            .map(|_| PeerId::human(key.to_string()))
    }

    /// Drop a channel the gateway left, withdrawing every human who was only
    /// there.
    fn drop_channel(&mut self, folded_ch: &str) -> Vec<HumanEffect> {
        let Some(ch) = self.channels.remove(folded_ch) else {
            return Vec::new();
        };
        // Whatever was held back about this channel is moot: the gateway no
        // longer watches it, and a later rejoin starts from a fresh snapshot.
        self.forget_channel_stories(folded_ch, None);
        let mut effects = Vec::new();
        for key in ch.members.keys() {
            effects.extend(self.mark_absent(key, folded_ch));
        }
        effects
    }
}

/// Strip the IRC channel-status prefixes (`@`, `+`, `%`, `~`, `&`) a NAMES reply
/// puts before a nick. Only leading status sigils are removed; the nick itself
/// is untouched.
fn strip_prefixes(nick: &str) -> &str {
    nick.trim_start_matches(['@', '+', '%', '~', '&'])
}

// ──────────────────────────── Channel reconciler ────────────────────────────

/// A channel-join effect the reconciler asks the executor to perform. Offline:
/// returned as data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelEffect {
    /// Join this channel (a newly-desired agent channel, or the lobby).
    Join(String),
    /// Part this channel (an agent left, so its channel is no longer desired).
    Part(String),
}

/// The exponential-backoff schedule for a channel whose JOIN the server refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Backoff {
    /// The earliest time (ms, monotonic) the channel may be retried.
    next_attempt_ms: u64,
    /// The delay applied on the last refusal; the next is `min(2×, cap)`.
    delay_ms: u64,
}

/// First backoff after a refused JOIN: 30 seconds.
const BACKOFF_START_MS: u64 = 30_000;
/// The backoff cap: ten minutes.
const BACKOFF_CAP_MS: u64 = 600_000;

/// Decides which channels the gateway should be in and reconciles that against
/// what it has joined or is trying to join, honouring per-channel backoff after
/// a refused JOIN. Time is injected as a monotonic millisecond counter so the
/// backoff is deterministic under test.
pub struct ChannelReconciler {
    prefix: String,
    /// The lobby, folded: always desired.
    lobby_folded: String,
    lobby_display: String,
    cm: CaseMapping,
    channellen: usize,
    /// Folded channels the gateway has confirmed it is in.
    joined: HashSet<String>,
    /// Folded channels a JOIN has been emitted for, awaiting confirmation.
    pending: HashSet<String>,
    /// Folded channel → backoff after a refused JOIN.
    backoff: HashMap<String, Backoff>,
    /// Folded channels whose refusal has already been diagnosed once.
    diagnosed: HashSet<String>,
}

impl ChannelReconciler {
    /// A reconciler for `prefix`/`lobby`, folding channels under `cm` within
    /// `channellen`.
    pub fn new(prefix: &str, lobby: &str, cm: CaseMapping, channellen: usize) -> Self {
        ChannelReconciler {
            prefix: prefix.to_string(),
            lobby_folded: fold_nick(lobby, cm),
            lobby_display: lobby.to_string(),
            cm,
            channellen,
            joined: HashSet::new(),
            pending: HashSet::new(),
            backoff: HashMap::new(),
            diagnosed: HashSet::new(),
        }
    }

    /// The set of channels currently desired, folded → display, from the lobby
    /// plus one channel per non-human discovered peer. A folded collision (two
    /// peers mapping to the same channel) is one desired channel, not two.
    fn desired(&self, peers: &[PeerId]) -> HashMap<String, String> {
        let mut want = HashMap::new();
        want.insert(self.lobby_folded.clone(), self.lobby_display.clone());
        for p in peers {
            if let Some(ch) = channel_for(p, &self.prefix, self.channellen) {
                want.entry(fold_nick(&ch, self.cm)).or_insert(ch);
            }
        }
        want
    }

    /// Reconcile against the current discovered peers at time `now_ms`. Emits a
    /// JOIN for every desired channel not joined/pending and not inside its
    /// backoff window, and a PART for every joined/pending channel no longer
    /// desired (never the lobby). Channels in backoff whose window has not
    /// elapsed are left alone; this same call is the periodic *refresh*.
    pub fn reconcile(&mut self, peers: &[PeerId], now_ms: u64) -> Vec<ChannelEffect> {
        let want = self.desired(peers);
        let mut effects = Vec::new();

        // JOIN newly-desired channels not already in-flight or backing off.
        for (folded, display) in &want {
            if self.joined.contains(folded) || self.pending.contains(folded) {
                continue;
            }
            if let Some(b) = self.backoff.get(folded) {
                if now_ms < b.next_attempt_ms {
                    continue; // still inside the backoff window
                }
            }
            self.pending.insert(folded.clone());
            effects.push(ChannelEffect::Join(display.clone()));
        }

        // PART channels no longer desired (the lobby is always desired).
        let stale: Vec<String> = self
            .joined
            .iter()
            .chain(self.pending.iter())
            .filter(|f| !want.contains_key(*f))
            .cloned()
            .collect();
        for folded in stale {
            let display = self.forget(&folded);
            effects.push(ChannelEffect::Part(display));
        }

        // …and drop the per-channel state of channels that are no longer desired
        // but were never joined or pending, so nothing to PART. `join_refused`
        // takes a never-confirmed channel out of `pending` and leaves it only in
        // `backoff`/`diagnosed`; when its peer then disappears from discovery
        // the loop above cannot see it, and `dropped` never fires for a channel
        // the gateway was never in. The stale window then blocks the JOIN that
        // should follow rediscovery for up to ten minutes and swallows the
        // diagnostic for the fresh refusal.
        let lingering: Vec<String> = self
            .backoff
            .keys()
            .chain(self.diagnosed.iter())
            .filter(|f| !want.contains_key(*f))
            .cloned()
            .collect();
        for folded in lingering {
            self.forget(&folded);
        }
        effects
    }

    /// The executor confirmed a JOIN (the gateway is now in `channel`). Clears
    /// any pending/backoff state for it.
    pub fn join_confirmed(&mut self, channel: &str) {
        let folded = fold_nick(channel, self.cm);
        self.pending.remove(&folded);
        self.backoff.remove(&folded);
        self.diagnosed.remove(&folded);
        self.joined.insert(folded);
    }

    /// The server refused a JOIN. Schedules the next retry (30 s, doubling,
    /// capped at ten minutes) and reports whether this is the FIRST refusal for
    /// the channel, so the caller diagnoses it exactly once.
    pub fn join_refused(&mut self, channel: &str, now_ms: u64) -> bool {
        let folded = fold_nick(channel, self.cm);
        self.pending.remove(&folded);
        let delay = match self.backoff.get(&folded) {
            Some(b) => (b.delay_ms * 2).min(BACKOFF_CAP_MS),
            None => BACKOFF_START_MS,
        };
        self.backoff.insert(
            folded.clone(),
            Backoff {
                next_attempt_ms: now_ms + delay,
                delay_ms: delay,
            },
        );
        self.diagnosed.insert(folded)
    }

    /// The gateway left `channel` (a PART/KICK observed elsewhere): drop it from
    /// the joined/pending sets so a later refresh may re-join it if still desired.
    pub fn dropped(&mut self, channel: &str) {
        self.forget(&fold_nick(channel, self.cm));
    }

    /// Forget all reconciler state (disconnect / incompatible ISUPPORT): the
    /// next reconcile rebuilds from fresh discovery.
    pub fn reset(&mut self) {
        self.joined.clear();
        self.pending.clear();
        self.backoff.clear();
        self.diagnosed.clear();
    }

    /// Whether the gateway currently considers itself in `channel`.
    pub fn is_joined(&self, channel: &str) -> bool {
        self.joined.contains(&fold_nick(channel, self.cm))
    }

    /// Remove a folded channel from every piece of per-channel state, returning
    /// a display name for the PART (the folded key is a legal channel name to
    /// part with).
    ///
    /// Backoff and the diagnosed-once marker go too. They describe an
    /// in-progress attempt to join THIS channel; once the channel is no longer
    /// desired, or the gateway has left it, a leftover backoff window would
    /// block the next legitimate JOIN for up to ten minutes and swallow the
    /// diagnostic that should accompany a fresh refusal.
    fn forget(&mut self, folded: &str) -> String {
        self.joined.remove(folded);
        self.pending.remove(folded);
        self.backoff.remove(folded);
        self.diagnosed.remove(folded);
        folded.to_string()
    }
}
