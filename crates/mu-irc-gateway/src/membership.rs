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

/// One observed channel member. Identity is the folded nick alone; which
/// `human:<nick>` this is never depends on the services account.
///
/// The account lives HERE, on the roster entry, because the roster entry
/// already has exactly the lifetime an attribution needs: it exists while the
/// view knows the nick's current holder, and disappears when it stops. A
/// separate map keyed by nick has to be pruned by hand on every path that can
/// remove the last entry, and each one missed is a stale account waiting to be
/// inherited by the next holder of a reused nick.
///
/// The cost is that a nick in several channels has several copies, which must
/// not disagree. That is handled in the one write path
/// ([`Membership::attribute_all`]), which fans out to every roster and every
/// open sync, and by seeding a newly-inserted member from what is already
/// attributed. One write path to get right, instead of five removal paths to
/// remember.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The nick as last seen on the wire (for display / framing).
    pub display: String,
    /// The services account the server attributed, if it has answered.
    /// `None` means UNATTRIBUTED, never "has no account": a NAMES line carries
    /// no account field, so its silence is not an answer.
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
    /// The services accounts the gateway's puppet connections are logged in
    /// as, folded. The AUTHORITATIVE answer to "is this one of ours?".
    ///
    /// An account is a server fact that survives a rename, so it needs no
    /// per-spelling bookkeeping and has no gap to cover: the window's
    /// `retiring`/`gone`/`deferred` sets existed only because a NICK could
    /// move a puppet out from under the spelling we knew it by. The bridge
    /// hands this in from the pool's leases, before any puppet connects, and
    /// an account stays for as long as its CONNECTION exists — not for as
    /// long as it holds some particular nick.
    owned_accounts: HashMap<String, String>,
    /// Puppet nicks the pool currently holds: folded key → wire spelling
    /// (kept so a `CASEMAPPING` change re-derives, as with `self_nick`).
    ///
    /// A FALLBACK, consulted only for a member the server has not attributed
    /// — before the WHOX pass answers for members already present when the
    /// gateway joined, or on a server with no WHOX at all. Where an account is
    /// known it wins: a nick in this set whose account says otherwise is a
    /// human who took the name, and is fronted as one.
    owned_nicks: HashMap<String, String>,
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
            owned_accounts: HashMap::new(),
            owned_nicks: HashMap::new(),
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

    /// Whether the folded nick `key` is one of the gateway's puppets.
    ///
    /// Account first, spelling second. Where the server has attributed the
    /// nick, the account decides and is final — it survives renames, cannot be
    /// forged by taking a name, and leaves no gap between a rename and our
    /// learning of it. Where it has not, the pool's current nick set is the
    /// conservative fallback, which is what the gateway had before it could
    /// ask. A nick the pool lists whose account says it is somebody else's is
    /// a HUMAN holding that name, and is treated as one.
    /// CONTRACT FOR THE SLOT INCREMENT: the `None` arm below conflates two
    /// things the server can mean — "nobody has told us yet" and "this holder
    /// is explicitly not logged in" — and falls back to the nick set for both.
    /// That is correct TODAY, because puppets connect unauthenticated
    /// (`[irc.puppets]` refuses `sasl_*`), so one of ours legitimately has no
    /// account and must stay suppressed.
    ///
    /// It stops being correct the moment puppets authenticate as slot
    /// accounts. Then a puppet ALWAYS holds an account, so an explicit "not
    /// logged in" implies NOT one of ours, and a human holding a name the pool
    /// still lists should be fronted on that answer instead of staying
    /// suppressed by the fallback. Whoever lands the slot accounts must carry
    /// the adapter's existing three-valued distinction
    /// (`JoinAccount::Unknown` vs `LoggedOut`) through to here and split this
    /// arm. Raised by the review panel on this increment (board run 7,
    /// gpt-6-astra) and deferred deliberately, not overlooked: it is tracked
    /// as `mu-irc-remote-session-zgbdz.8` (slot accounts), which is the
    /// increment that makes the flip correct.
    fn is_puppet(&self, key: &str) -> bool {
        match self.attributed(key) {
            Some(account) => self
                .owned_accounts
                .contains_key(&fold_nick(&account, self.cm)),
            None => self.owned_nicks.contains_key(key),
        }
    }

    /// Replace the set of services accounts the gateway's puppets are logged
    /// in as (the pool's current leases).
    ///
    /// An account belongs here for as long as its CONNECTION exists, not for
    /// as long as it holds a given nick — which is why this needs no retiring
    /// set. A connection's account leaves only once its socket is gone, and by
    /// then the server has dropped it too, so there is no interval in which a
    /// puppet is on the server while the gateway believes the name is free.
    ///
    /// THAT LAST SENTENCE IS A CONTRACT ON THE CALLER, and it is the property
    /// the window-deletion rests on. This reconciles IMMEDIATELY, so a lease
    /// returned before this connection has observed the puppet's departure
    /// fronts the gateway's own puppet as a human (R1). The bridge must return
    /// a lease only after the departure is observed; tracked as
    /// `mu-irc-remote-session-zgbdz.4.1`, which is where it has to be built.
    pub fn set_owned_accounts<I, S>(&mut self, accounts: I) -> Vec<HumanEffect>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let cm = self.cm;
        // Keyed by the folded form for lookup, but the ORIGINAL spelling is
        // kept beside it: folding is lossy, so a `CASEMAPPING` change must
        // re-derive from what the server said, never re-fold an already-folded
        // value. Same reason `self_nick` and the fallback nick set keep their
        // wire spellings.
        self.owned_accounts = accounts
            .into_iter()
            .map(|a| (fold_nick(a.as_ref(), cm), a.as_ref().to_string()))
            .collect();
        self.reconcile_ours()
    }

    /// Replace the puppet NICK set (wire spellings) — the fallback consulted
    /// for members the server has not attributed. See
    /// [`set_owned_accounts`](Self::set_owned_accounts), the authoritative half.
    pub fn set_owned_nicks<I, S>(&mut self, nicks: I) -> Vec<HumanEffect>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let cm = self.cm;
        // Two spellings that fold equal cannot both be kept under one key, and
        // the lexicographically earlier one survives — the same choice
        // `set_casemapping` makes when a fold change merges two, and the same
        // one the pool's nick table makes.
        let mut spellings: Vec<String> =
            nicks.into_iter().map(|n| n.as_ref().to_string()).collect();
        spellings.sort_unstable();
        self.owned_nicks.clear();
        for wire in spellings {
            self.owned_nicks.entry(fold_nick(&wire, cm)).or_insert(wire);
        }
        self.reconcile_ours()
    }

    /// Re-decide, for everyone the view holds, whether they are a human to
    /// front — and emit the difference.
    ///
    /// The single self-healing path that replaces the window's held-back
    /// stories. Membership tracks who the SERVER says is in each channel,
    /// puppets included; who is a human is a predicate over that. So when the
    /// predicate's inputs change — a lease taken or returned, an account
    /// attributed — the correction is a re-evaluation, not a replay: anyone
    /// newly ours is withdrawn, anyone no longer ours is fronted where they
    /// actually are, and because they were in the roster all along there is
    /// nothing to replay to find out where that is. Bounded by one round trip,
    /// which is R3.
    fn reconcile_ours(&mut self) -> Vec<HumanEffect> {
        let mut effects = Vec::new();
        // Every nick the rosters hold, with the channels it is in. Presence is
        // a PROJECTION of this, so it is recomputed rather than patched: a
        // member already fronted may have gained or lost channels, and leaving
        // its set stale is how a later PART withdraws someone who never left.
        let mut in_channels: HashMap<String, HashSet<String>> = HashMap::new();
        for (folded_ch, ch) in &self.channels {
            for key in ch.members.keys() {
                in_channels
                    .entry(key.clone())
                    .or_default()
                    .insert(folded_ch.clone());
            }
        }
        let mut keys: Vec<String> = in_channels.keys().cloned().collect();
        keys.sort_unstable();
        for key in keys {
            if key == self.self_nick.folded() {
                continue;
            }
            let ours = self.is_puppet(&key);
            let fronted = self.present.contains_key(&key);
            if ours {
                if fronted {
                    if let Some(peer) = self.forget_presence(&key) {
                        effects.push(HumanEffect::Withdraw(peer));
                    }
                }
                continue;
            }
            let channels = in_channels.remove(&key).unwrap_or_default();
            if channels.is_empty() {
                continue;
            }
            self.present.insert(key.clone(), channels);
            if !fronted {
                effects.push(HumanEffect::Register(PeerId::human(key)));
            }
        }
        effects
    }

    /// The puppet nicks currently held, in their wire spelling.
    pub fn owned_nicks(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.owned_nicks.values().map(String::as_str).collect();
        v.sort_unstable();
        v
    }

    /// Whether `nick` is one of the gateway's own nicks (its own or a puppet's).
    pub fn is_owned(&self, nick: &str) -> bool {
        self.is_own(&self.fold(nick))
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
    /// JOIN or PART under a retiring name — is superseded by it: a holder
    /// still there is listed again (and held back again, under the same
    /// rule), one who left is not, and a story replayed later must not
    /// re-add a name a newer roster showed absent (panel finding, PR #671
    /// run 14). Until it commits, nothing is dropped: a story resolved
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

    /// One `RPL_NAMREPLY` line for `channel` at generation `gen`. Names for a
    /// channel not currently syncing that generation are ignored as stale.
    pub fn names_reply(
        &mut self,
        channel: &str,
        gen: u64,
        nicks: impl IntoIterator<Item = (String, Option<String>)>,
    ) {
        let nicks: Vec<(String, Option<String>)> = nicks.into_iter().collect();
        let folded = self.fold(channel);
        let cm = self.cm;
        let self_nick = self.self_nick.folded().to_string();
        // What is already attributed for each nick this burst names, read
        // before the channel borrow so a new roster entry can inherit it: a
        // NAMES line carries no account, and a blank copy must not shadow an
        // answer the server already gave.
        let seeded: HashMap<String, Option<String>> = nicks
            .iter()
            .map(|(n, _)| {
                let k = fold_nick(strip_prefixes(n), cm);
                let a = self.attributed(&k);
                (k, a)
            })
            .collect();
        let Some(ch) = self.channels.get_mut(&folded) else {
            return;
        };
        let Some(sync) = ch.sync.as_mut().filter(|s| s.gen == gen) else {
            return;
        };
        for (nick, account) in nicks {
            let key = fold_nick(strip_prefixes(&nick), cm);
            // The gateway's own nick is the one thing never a member of its own
            // view. Puppets ARE recorded, like anyone else the server lists:
            // the roster is who is THERE, and whether a member is a human to
            // front is decided separately, at presence time. That is what lets
            // a late attribution correct itself — a nick later found to be a
            // human is already known to be in this channel, so there is
            // nothing to replay.
            if key == self_nick {
                continue;
            }
            if sync.departed.contains(&key) {
                // The gateway watched this nick leave after the server took the
                // snapshot this line comes from. The observed departure is the
                // newer fact; the snapshot entry is discarded.
                continue;
            }
            let inherited = account
                .clone()
                .or_else(|| seeded.get(&key).cloned().flatten());
            sync.pending.insert(
                key,
                Member {
                    display: strip_prefixes(&nick).to_string(),
                    account: inherited,
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
        let Some(ch) = self.channels.get_mut(&folded) else {
            return Vec::new();
        };
        // Diff the committed roster against what the channel held before, so a
        // re-sync that DROPS a member withdraws them (if it was their last
        // channel) rather than leaving stale presence, and one that adds a member
        // registers them. Members in both are unchanged.
        let old: Vec<String> = ch.members.keys().cloned().collect();
        ch.members = sync.pending;
        let added: Vec<String> = ch
            .members
            .keys()
            .filter(|k| !old.contains(k))
            .cloned()
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
        for key in added {
            effects.extend(self.mark_present(&key, &folded));
        }
        effects
    }

    /// A nick joined `channel`. Applies to the live roster and, if a sync is
    /// open, to its pending set so the interleaved event is not lost at commit.
    ///
    /// The roster records every arrival the server reports, puppets included;
    /// [`mark_present`](Self::mark_present) is the single place that decides
    /// which of them is a human to front.
    pub fn joined(
        &mut self,
        channel: &str,
        nick: &str,
        account: Option<String>,
    ) -> Vec<HumanEffect> {
        let folded_ch = self.fold(channel);
        let key = self.fold(nick);
        if key == self.self_nick.folded() {
            // Our own JOIN echo; `self_joined` already recorded the channel.
            return Vec::new();
        }
        if !self.channels.contains_key(&folded_ch) {
            return Vec::new();
        }
        // What this entry starts with: the account the JOIN carried if it
        // carried one, otherwise whatever is already attributed — so a first
        // JOIN is not left blank by a fan-out with no copies to reach yet, and
        // a member joining a second channel does not create a blank copy
        // beside an attributed one. A JOIN without an account says nothing
        // about it and must not clear a known answer, which is why the
        // fallback is `attributed` rather than `None`.
        let member = Member {
            display: nick.to_string(),
            account: account.clone().or_else(|| self.attributed(&key)),
        };
        // …and if it DID carry one, every other copy takes it too, so a JOIN
        // reporting a changed account cannot leave older copies behind.
        let attributed = account.is_some();
        self.attribute(&key, account);
        let Some(ch) = self.channels.get_mut(&folded_ch) else {
            return Vec::new();
        };
        ch.members.insert(key.clone(), member.clone());
        if let Some(sync) = ch.sync.as_mut() {
            // A live JOIN is newer than any tombstone from earlier in this
            // sync, so it clears one and stands as the snapshot's evidence.
            sync.departed.remove(&key);
            sync.pending.insert(key.clone(), member);
        }
        let mut effects = self.mark_present(&key, &folded_ch);
        // A JOIN that carried an account changed the verdict for this nick
        // EVERYWHERE, not just here: a member already fronted in another
        // channel must be withdrawn if the account makes it ours, and one that
        // stops being ours must be fronted for every channel it is in, not
        // only this one — otherwise its presence set holds a single channel
        // and leaving that channel withdraws a member who never left.
        if attributed {
            effects.extend(self.reconcile_ours());
        }
        effects
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
            // A departure is departure evidence for the open snapshot: a line
            // naming the holder after it is stale.
            sync.pending.remove(&key);
            sync.departed.insert(key.clone());
        }
        // Emits nothing for a puppet, which was never fronted. The attribution
        // goes with the roster entry, so there is nothing else to clean up.
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
        // One withdraw at most, and none at all for a puppet: the departing
        // nick is gone from everywhere either way.
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
    /// A rename is the case the window was built for, and under account
    /// identity it is ordinary: the member moves to the new spelling carrying
    /// its attribution with it, so whatever it was before the NICK it still is
    /// after. Nothing has to be remembered about the vacated spelling, because
    /// nothing about identity was ever stored there.
    pub fn renamed(&mut self, from: &str, to: &str) -> Vec<HumanEffect> {
        self.rename(from, to)
    }

    fn rename(&mut self, from: &str, to: &str) -> Vec<HumanEffect> {
        let old = self.fold(from);
        let new = self.fold(to);
        // Nothing to re-key for the attribution: it rides on the member
        // entries and the rename moves those. The pool's fallback nick set
        // does follow a puppet the server renamed (it can force a NICK), so
        // the fallback keeps answering for the connection it belongs to.
        if old != new {
            if let Some(_wire) = self.owned_nicks.remove(&old) {
                self.owned_nicks.insert(new.clone(), to.to_string());
            }
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
                if !in_channels.is_empty() && !self.is_puppet(&new) {
                    self.present.insert(new.clone(), in_channels);
                }
            }
            return Vec::new();
        }
        // Presence is decided AFTER the move, because the move can change the
        // verdict: an unattributed nick renaming onto one the pool lists
        // becomes ours, and one renaming off it stops being ours. Where the
        // server has attributed the member the account travelled with it and
        // nothing changes — which is the point of keying on the account.
        let was_fronted = self.present.remove(&old).is_some();
        if self.is_puppet(&new) {
            return if was_fronted {
                vec![HumanEffect::Withdraw(PeerId::human(old))]
            } else {
                Vec::new()
            };
        }
        self.present.insert(new.clone(), in_channels);
        if was_fronted {
            vec![HumanEffect::Rename {
                from: PeerId::human(old),
                to: PeerId::human(new),
            }]
        } else {
            vec![HumanEffect::Register(PeerId::human(new))]
        }
    }

    /// The connection dropped, or discovery must restart: forget everything and
    /// withdraw every currently-present human. Fresh NAMES rebuilds from empty.
    pub fn reset(&mut self) -> Vec<HumanEffect> {
        self.channels.clear();
        // The puppets die with the connection that owned them; the next
        // session's pool starts empty and hands over a fresh owned set.
        self.owned_accounts.clear();
        self.owned_nicks.clear();
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
        // The fallback nick set re-derives from wire spellings for the same
        // reason: re-folding an already-folded value is lossy. Two spellings
        // that fold equal under the new rule cannot both be kept under one
        // key, and the lexicographically earlier one survives.
        //
        // The ACCOUNT set needs nothing of the kind: it is re-folded from
        // account names, not from nicks, and an account is not a spelling a
        // puppet moves between. The attributions themselves ride on the member
        // entries, which are re-keyed below with their rosters. That is the
        // whole of what the window's `gone`/`retiring`/`deferred` merge logic
        // used to do here.
        let mut owned: Vec<(String, String)> = std::mem::take(&mut self.owned_nicks)
            .into_values()
            .map(|wire| (fold_nick(&wire, cm), wire))
            .collect();
        owned.sort_by(|a, b| a.1.cmp(&b.1));
        for (key, wire) in owned {
            self.owned_nicks.entry(key).or_insert(wire);
        }
        let mut accounts: Vec<String> = std::mem::take(&mut self.owned_accounts)
            .into_values()
            .collect();
        accounts.sort_unstable();
        self.owned_accounts.clear();
        for original in accounts {
            self.owned_accounts
                .entry(fold_nick(&original, cm))
                .or_insert(original);
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
                // Only the gateway itself is never a member of its own view.
                // Puppets STAY in the rebuilt roster, as they do everywhere
                // else: the roster is who is there, and who is a human is
                // decided afterwards, against the rebuilt attributions. Asking
                // `is_puppet` here would be asking it mid-rebuild, with
                // `self.channels` already taken — it would see no attributions
                // at all and answer from the fallback nick set alone, so an
                // account-owned puppet would survive the filter and be fronted
                // as a human by the presence rebuild below (R1).
                if new_key == self_folded {
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
        // Puppets are in the rebuilt rosters like everyone else, so they must
        // be filtered HERE rather than at re-key time — by now `self.channels`
        // is restored, so `is_puppet` can see the attributions again and the
        // account answers properly. Computed up front because the rebuild
        // below borrows `self.present` mutably.
        let puppets: HashSet<String> = self
            .channels
            .values()
            .flat_map(|ch| ch.members.keys().cloned())
            .filter(|k| self.is_puppet(k))
            .collect();
        for (folded_ch, channel) in &self.channels {
            for key in channel.members.keys() {
                if puppets.contains(key) {
                    continue;
                }
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

    /// Record what the server said a nick's services account is, or that it
    /// has none.
    ///
    /// Three things on the wire say this, and all three land here:
    /// `extended-join` on arrival, a `WHOX` reply for the members already
    /// present when the gateway joins, and `ACCOUNT` (from `account-notify`)
    /// when someone logs in or out mid-session.
    ///
    /// `Some` sets, `None` clears — a logout. Only an explicit answer clears;
    /// see [`attribute`](Self::attribute) for the roster paths, which may only
    /// add, because a NAMES line carries no account field and saying nothing
    /// is not the same as saying "none".
    ///
    /// Attribution stays apart from MEMBERSHIP: this changes who a nick is,
    /// never whether it is there, so a WHO reply that races a departure
    /// cannot resurrect anyone — a nick this does not find is not updated.
    /// Returns the correction, if the new attribution changes whether this
    /// nick is one of ours: a member the gateway was treating as its own
    /// because the pool's nick set said so, but whose account says otherwise,
    /// is a human holding that name and is fronted here — one round trip after
    /// the server answered, with no story to replay, because they were in the
    /// roster all along. The reverse is withdrawn, which is R1.
    pub fn set_account(&mut self, nick: &str, account: Option<String>) -> Vec<HumanEffect> {
        let key = self.fold(nick);
        self.attribute_all(&key, account);
        self.reconcile_ours()
    }

    /// Record an account learned from a roster path (`extended-join`, a NAMES
    /// line that carried one). Add-only: `None` means the line said nothing
    /// about this nick, not that the nick has no account, so it must not clear
    /// an answer the server gave elsewhere.
    fn attribute(&mut self, key: &str, account: Option<String>) {
        if account.is_some() {
            self.attribute_all(key, account);
        }
    }

    /// The ONE write path. Applies `account` to every copy of `key` the view
    /// holds — each channel's committed roster and each open sync's pending
    /// side — so the copies cannot drift apart and the answer cannot depend on
    /// which roster is consulted.
    ///
    /// Nothing prunes, anywhere. Each copy dies with the roster entry holding
    /// it, which is exactly when the view stops knowing that nick's holder: a
    /// PART, a QUIT, a resync that drops the member, the gateway leaving the
    /// channel, or the channel's whole sync being discarded. When the last one
    /// goes the attribution goes with it, and a later holder of the same
    /// spelling starts from a fresh, unattributed member. That is the entire
    /// lifetime rule, and the structure enforces it instead of five separate
    /// removal paths each having to remember to call something.
    fn attribute_all(&mut self, key: &str, account: Option<String>) {
        for ch in self.channels.values_mut() {
            if let Some(member) = ch.members.get_mut(key) {
                member.account.clone_from(&account);
            }
            if let Some(member) = ch.sync.as_mut().and_then(|s| s.pending.get_mut(key)) {
                member.account.clone_from(&account);
            }
        }
    }

    /// What is already attributed to this folded nick, from any copy the view
    /// holds. Seeds a newly-inserted member, so a roster entry created after
    /// the server answered does not sit there blank and shadow the answer.
    fn attributed(&self, key: &str) -> Option<String> {
        self.channels.values().find_map(|ch| {
            ch.members
                .get(key)
                .and_then(|m| m.account.clone())
                .or_else(|| {
                    ch.sync
                        .as_ref()?
                        .pending
                        .get(key)
                        .and_then(|m| m.account.clone())
                })
        })
    }

    /// The services account currently attributed to `nick`, if the server has
    /// answered for it.
    ///
    /// Scans for an ANSWER rather than stopping at the first roster holding
    /// the nick: a copy that is `None` must not shadow one that has it. Since
    /// every write fans out, two copies cannot hold DIFFERENT accounts, so the
    /// result is order-independent despite iterating a `HashMap`.
    pub fn account_of(&self, nick: &str) -> Option<&str> {
        let key = self.fold(nick);
        self.channels.values().find_map(|ch| {
            ch.members
                .get(&key)
                .and_then(|m| m.account.as_deref())
                .or_else(|| {
                    ch.sync
                        .as_ref()?
                        .pending
                        .get(&key)
                        .and_then(|m| m.account.as_deref())
                })
        })
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
    fn mark_present(&mut self, key: &str, folded_ch: &str) -> Vec<HumanEffect> {
        // The one gate that decides who is fronted. Membership records every
        // member the server names, the gateway's own nick aside; this is where
        // "and which of them is a human" is applied. Keeping it in one place is
        // what makes a late attribution a re-evaluation rather than a replay.
        if self.is_puppet(key) {
            return Vec::new();
        }
        let set = self.present.entry(key.to_string()).or_default();
        let first = set.is_empty();
        set.insert(folded_ch.to_string());
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
        let mut effects = Vec::new();
        let members: Vec<String> = ch.members.keys().cloned().collect();
        for key in members {
            effects.extend(self.mark_absent(&key, folded_ch));
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
