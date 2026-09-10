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
//!   Every real nick other than the gateway's own is a human operator (mesh
//!   agents are channels, never IRC members), so membership is also the sole
//!   authority on which humans are *present* — the fact routing consults before
//!   ever disclosing a private body. It emits [`HumanEffect`]s (front / release /
//!   rename) whose executor uses `front_peer`/`release_peer`; it never touches
//!   the mesh itself.
//! - [`ChannelReconciler`] decides which channels the gateway *should* be in from
//!   the current discovered-agent snapshot (the lobby always; one channel per
//!   agent peer via [`crate::mapping::channel_for`]; never a human channel),
//!   diffs that against what it has joined/pending, and emits JOIN/PART effects.
//!   A refused JOIN is diagnosed once and then retried on an exponential backoff
//!   (30 s, doubling, capped at ten minutes).
//!
//! Everything is invalidated on the events the plan calls disposable: a gateway
//! PART/KICK drops one channel, and a disconnect or an incompatible `CASEMAPPING`
//! change drops all of it — after which fresh discovery and NAMES rebuild it from
//! nothing, with no traffic retained.

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
    cm: CaseMapping,
    channels: HashMap<String, Channel>,
    /// Folded human nick → the set of folded channels they are currently in.
    /// A human is present iff this set is non-empty; the count is what makes a
    /// human shared across two channels survive leaving one of them.
    present: HashMap<String, HashSet<String>>,
    gen: u64,
}

impl Membership {
    /// A fresh, empty view for a gateway registered as `self_nick`, folding under
    /// `cm`.
    pub fn new(self_nick: &str, cm: CaseMapping) -> Self {
        Membership {
            self_nick: SelfNick::new(self_nick, cm),
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

    fn fold(&self, name: &str) -> String {
        fold_nick(name, self.cm)
    }

    /// The gateway itself joined `channel`: record the (empty) channel and open a
    /// NAMES sync for it. Returns the generation to tag this channel's incoming
    /// `Names`/`NamesEnd` with, so a later sync cannot be closed by an older
    /// burst. Idempotent apart from the fresh generation.
    pub fn self_joined(&mut self, channel: &str) -> u64 {
        self.gen += 1;
        let gen = self.gen;
        let folded = self.fold(channel);
        let entry = self.channels.entry(folded).or_insert_with(|| Channel {
            display: channel.to_string(),
            members: HashMap::new(),
            sync: Some(Sync::new(gen)),
        });
        entry.sync = Some(Sync::new(gen));
        gen
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
        let Some(ch) = self.channels.get_mut(&folded) else {
            return;
        };
        let Some(sync) = ch.sync.as_mut().filter(|s| s.gen == gen) else {
            return;
        };
        for (nick, account) in nicks {
            let key = fold_nick(strip_prefixes(&nick), cm);
            if key == self_nick {
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
        let folded_ch = self.fold(channel);
        let key = self.fold(nick);
        if key == self.self_nick.folded() {
            // Our own JOIN echo: `self_joined` already recorded the channel.
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
            // A live JOIN is newer than any tombstone from earlier in this sync.
            sync.departed.remove(&key);
            sync.pending.insert(key.clone(), member);
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
        let Some(ch) = self.channels.get_mut(&folded_ch) else {
            return Vec::new();
        };
        ch.members.remove(&key);
        if let Some(sync) = ch.sync.as_mut() {
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
    pub fn renamed(&mut self, from: &str, to: &str) -> Vec<HumanEffect> {
        let old = self.fold(from);
        let new = self.fold(to);
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
        let effects = self
            .present
            .keys()
            .map(|k| HumanEffect::Withdraw(PeerId::human(k.clone())))
            .collect();
        self.present.clear();
        effects
    }

    /// The server changed `CASEMAPPING` to `cm`. If it differs, every folded key
    /// the view holds is now suspect, so the view is invalidated exactly like a
    /// disconnect and rebuilt under the new rule from fresh NAMES.
    pub fn set_casemapping(&mut self, cm: CaseMapping) -> Vec<HumanEffect> {
        if cm == self.cm {
            return Vec::new();
        }
        let effects = self.reset();
        self.cm = cm;
        // Re-DERIVED from the wire spelling, not re-folded from the previous
        // folded value: folding is lossy, so re-folding would silently change
        // who the gateway thinks it is.
        self.self_nick.set_casemapping(cm);
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
