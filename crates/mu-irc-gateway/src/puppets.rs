//! Puppet pool DECISIONS: which peers get an IRC nick of their own, under what
//! nick, in what order, and what to do when the server says no. Pure — no
//! socket, no timer, no task. The bridge's session loop executes the actions
//! this module returns, exactly as it executes routing and outbound decisions
//! today (design: `specs/plans/mu-irc-gateway-v1-puppets.md`, "Connections
//! and lifecycle"; this is increment 2a, delivered before any bridge wiring).
//!
//! Three things live here:
//!
//! - [`Pool`]: the per-peer state machine (waiting → connecting → registered,
//!   with backoff and give-up), driven by discovery snapshots, a clock, and the
//!   registration outcomes the bridge reports back. It owns the
//!   [`NickTable`], so reverse resolution of a puppet nick and the
//!   gateway-owned nick set both come from one place.
//! - [`classify`]: the single-consumer rule at the fan-in. Every puppet and
//!   `mu-gw` sit in the same channels, so Ergo delivers one channel line to
//!   N+1 gateway-held connections; only one copy may reach routing, decided
//!   by CLASS before routing sees anything.
//! - Ruling A as a predicate ([`qualifies`]): session-shaped peers only.
//!
//! Nothing here is invalidated by a reconnect in a way that needs care: the
//! pool is owned by the gateway `Session`, which is rebuilt empty on every
//! registration of the main connection, so a fresh pool starts from an empty
//! table and re-derives everything from the next snapshot (the "process state
//! is disposable" rule).

use std::collections::BTreeMap;

use mu_peer::PeerId;

use crate::adapter::IrcMessage;
use crate::config::PuppetsConfig;
use crate::mapping::{fold_nick, nick_for, nick_for_tailed, CaseMapping, NickCollision, NickTable};

// ───────────────────────────── Ruling A ─────────────────────────────────────

/// Does `peer` get a puppet under `cfg`? Ruling A (operator, 2026-09-16):
/// session-shaped peers only — `cc:<id>` and `mu:<daemon>:<session>` — with a
/// bare daemon `mu:<daemon>` channel-only unless `daemons = true`. Humans
/// never qualify; a role outside `roles` never qualifies; a peer with no id is
/// a bare role and never qualifies.
pub fn qualifies(peer: &PeerId, cfg: &PuppetsConfig) -> bool {
    if peer.is_human() || peer.id().is_empty() {
        return false;
    }
    if !cfg.roles.iter().any(|r| r == peer.role()) {
        return false;
    }
    match peer.as_mu() {
        Some(mu) if mu.session.is_none() => cfg.daemons,
        _ => true,
    }
}

// ───────────────────────────── Pool state ───────────────────────────────────

/// First retry delay after a failed puppet connection; doubles to
/// [`BACKOFF_MAX`]. The same schedule the main connection uses.
pub const BACKOFF_MIN_MS: u64 = 2_000;
/// Longest retry delay.
pub const BACKOFF_MAX_MS: u64 = 300_000;

/// Why a peer is channel-only for the rest of this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelOnly {
    /// Both the plain nick and its hash-tail form were already held on the
    /// server (`433`, or a human who joined first). Humans keep their names.
    NickTaken,
    /// The server refused the nick as erroneous (`432`). The tailed form uses
    /// the same alphabet, so there is nothing else to offer.
    NickErroneous,
    /// More qualifying peers than `max`; this one is beyond the cap.
    OverCap,
}

/// Where one puppet is in its life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PuppetState {
    /// Qualifies; waiting for `min_age`, a pacing slot, or room under the cap.
    Waiting,
    /// A connection is in flight offering `nick`.
    Connecting {
        nick: String,
        tailed: bool,
        attempt: u32,
    },
    /// Registered on the server as `nick`.
    Registered { nick: String },
    /// The last connection failed; retry at `until_ms`.
    BackingOff { until_ms: u64, attempt: u32 },
    /// Channel-only for this session; the reason was reported once.
    ChannelOnly(ChannelOnly),
}

/// One tracked peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Puppet {
    /// The discovery tick that first listed the peer.
    pub first_seen_ms: u64,
    pub state: PuppetState,
}

/// What the bridge must do next. Actions are complete instructions: the
/// executor needs no pool state to carry one out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolAction {
    /// Open a connection for `peer` and register as `nick` (no SASL).
    Connect { peer: PeerId, nick: String },
    /// `peer`'s registered puppet leaves: send `QUIT` on its connection.
    Quit { peer: PeerId, nick: String },
    /// Abort `peer`'s in-flight connection or pending retry.
    Cancel { peer: PeerId },
    /// `peer` is channel-only from now on; say so once (gateway notice /
    /// `mu peers`).
    ChannelOnly { peer: PeerId, reason: ChannelOnly },
}

/// One row of `mu peers`: the peer, its nick if any, and its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PuppetStatus {
    pub peer: PeerId,
    pub nick: Option<String>,
    pub state: PuppetState,
}

/// The pool: every qualifying peer the last snapshot listed, its state, and
/// the nicks held. Deterministic — peers are kept in `PeerId` order, so two
/// gateways fed the same events make the same decisions in the same order.
#[derive(Debug, Clone)]
pub struct Pool {
    cfg: PuppetsConfig,
    nicklen: usize,
    table: NickTable,
    puppets: BTreeMap<PeerId, Puppet>,
}

impl Pool {
    /// An empty pool for a server advertising `nicklen`, folding under `cm`.
    pub fn new(cfg: PuppetsConfig, nicklen: usize, cm: CaseMapping) -> Self {
        Pool {
            cfg,
            nicklen,
            table: NickTable::new(cm),
            puppets: BTreeMap::new(),
        }
    }

    /// The configuration the pool decides under.
    pub fn config(&self) -> &PuppetsConfig {
        &self.cfg
    }

    /// The nick table: reverse resolution and the owned set.
    pub fn table(&self) -> &NickTable {
        &self.table
    }

    /// Feed a discovery snapshot at `now_ms`. New qualifying peers start
    /// `Waiting` with `first_seen_ms = now_ms`; peers no longer listed are
    /// dropped, with a `Quit` for a registered puppet or a `Cancel` for one in
    /// flight or backing off. A peer that stops qualifying (config unchanged,
    /// so only by ceasing to be listed) is handled the same way. Nothing
    /// connects from here — see [`Pool::tick`].
    pub fn observe(&mut self, peers: &[PeerId], now_ms: u64) -> Vec<PoolAction> {
        let mut actions = Vec::new();
        let listed: std::collections::BTreeSet<&PeerId> =
            peers.iter().filter(|p| qualifies(p, &self.cfg)).collect();
        let gone: Vec<PeerId> = self
            .puppets
            .keys()
            .filter(|p| !listed.contains(p))
            .cloned()
            .collect();
        for peer in gone {
            actions.extend(self.forget(&peer));
        }
        for peer in listed {
            self.puppets.entry(peer.clone()).or_insert(Puppet {
                first_seen_ms: now_ms,
                state: PuppetState::Waiting,
            });
        }
        actions
    }

    /// Drop `peer` entirely, releasing its nick; the action that undoes
    /// whatever was in progress.
    fn forget(&mut self, peer: &PeerId) -> Vec<PoolAction> {
        let Some(p) = self.puppets.remove(peer) else {
            return Vec::new();
        };
        self.table.remove_peer(peer);
        match p.state {
            PuppetState::Registered { nick } => vec![PoolAction::Quit {
                peer: peer.clone(),
                nick,
            }],
            PuppetState::Connecting { .. } | PuppetState::BackingOff { .. } => {
                vec![PoolAction::Cancel { peer: peer.clone() }]
            }
            PuppetState::Waiting | PuppetState::ChannelOnly(_) => Vec::new(),
        }
    }

    /// Decide what to start at `now_ms`: connections for peers that are old
    /// enough, whose backoff has elapsed, while at most `connect_parallelism`
    /// registrations are in flight and at most `max` puppets are live
    /// (registered or in flight). Peers beyond the cap are marked channel-only
    /// once, in `PeerId` order, so the same roster always yields the same
    /// choice of who is capped. Disabled config: no actions, ever.
    pub fn tick(&mut self, now_ms: u64) -> Vec<PoolAction> {
        if !self.cfg.enabled {
            return Vec::new();
        }
        let mut actions = Vec::new();
        let min_age_ms = self.cfg.min_age_secs.saturating_mul(1000);
        let mut in_flight = self
            .puppets
            .values()
            .filter(|p| matches!(p.state, PuppetState::Connecting { .. }))
            .count();
        let mut live = in_flight
            + self
                .puppets
                .values()
                .filter(|p| matches!(p.state, PuppetState::Registered { .. }))
                .count();
        let peers: Vec<PeerId> = self.puppets.keys().cloned().collect();
        for peer in peers {
            let p = self.puppets.get(&peer).expect("listed above");
            let due = match &p.state {
                PuppetState::Waiting => now_ms.saturating_sub(p.first_seen_ms) >= min_age_ms,
                PuppetState::BackingOff { until_ms, .. } => now_ms >= *until_ms,
                _ => false,
            };
            if !due {
                continue;
            }
            if live >= self.cfg.max {
                // Only a waiting peer is newly capped; one already backing off
                // keeps its slot claim and simply waits for a free one.
                if matches!(p.state, PuppetState::Waiting) {
                    self.puppets.get_mut(&peer).expect("listed above").state =
                        PuppetState::ChannelOnly(ChannelOnly::OverCap);
                    actions.push(PoolAction::ChannelOnly {
                        peer: peer.clone(),
                        reason: ChannelOnly::OverCap,
                    });
                }
                continue;
            }
            if in_flight >= self.cfg.connect_parallelism {
                continue;
            }
            let attempt = match &p.state {
                PuppetState::BackingOff { attempt, .. } => *attempt,
                _ => 0,
            };
            let Some(nick) = self.plain_nick_for(&peer) else {
                continue;
            };
            self.puppets.get_mut(&peer).expect("listed above").state = PuppetState::Connecting {
                nick: nick.clone(),
                tailed: false,
                attempt,
            };
            in_flight += 1;
            live += 1;
            actions.push(PoolAction::Connect { peer, nick });
        }
        actions
    }

    /// The nick to offer first: the plain form, unless another held nick
    /// already folds equal to it, in which case the tailed form straight away.
    fn plain_nick_for(&self, peer: &PeerId) -> Option<String> {
        let plain = nick_for(peer, self.nicklen)?;
        match self.table.resolve(&plain) {
            Some(holder) if holder != peer => nick_for_tailed(peer, self.nicklen),
            _ => Some(plain),
        }
    }

    /// The bridge reports that `peer`'s connection registered as `nick` (the
    /// spelling the server confirmed). The nick enters the table; a collision
    /// there means the server accepted a nick that folds equal to one the
    /// gateway already holds, which a server that enforces unique nicks never
    /// does — treated as the server's word being final: the earlier holder is
    /// unaffected and this peer is channel-only.
    pub fn registered(&mut self, peer: &PeerId, nick: &str) -> Vec<PoolAction> {
        let Some(p) = self.puppets.get_mut(peer) else {
            // Registered after the peer left the mesh: it has no place here.
            return vec![PoolAction::Quit {
                peer: peer.clone(),
                nick: nick.to_string(),
            }];
        };
        match self.table.insert(nick, peer.clone()) {
            Ok(()) => {
                p.state = PuppetState::Registered {
                    nick: nick.to_string(),
                };
                Vec::new()
            }
            Err(NickCollision::HeldBy(_)) | Err(NickCollision::PeerHasNick(_)) => {
                p.state = PuppetState::ChannelOnly(ChannelOnly::NickTaken);
                vec![
                    PoolAction::Quit {
                        peer: peer.clone(),
                        nick: nick.to_string(),
                    },
                    PoolAction::ChannelOnly {
                        peer: peer.clone(),
                        reason: ChannelOnly::NickTaken,
                    },
                ]
            }
        }
    }

    /// The server rejected the offered nick at registration with `numeric`
    /// (`433` nick in use, `432` erroneous). `433` on the plain form → offer
    /// the tailed form now, no backoff; `433` on the tailed form, or `432` on
    /// either → channel-only for this session. Any other numeric is treated as
    /// a connection failure (backoff).
    pub fn nick_rejected(&mut self, peer: &PeerId, numeric: &str, now_ms: u64) -> Vec<PoolAction> {
        let Some(p) = self.puppets.get(peer) else {
            return Vec::new();
        };
        let (tailed, attempt) = match &p.state {
            PuppetState::Connecting {
                tailed, attempt, ..
            } => (*tailed, *attempt),
            _ => return Vec::new(),
        };
        let give_up = |reason: ChannelOnly| {
            vec![PoolAction::ChannelOnly {
                peer: peer.clone(),
                reason,
            }]
        };
        match numeric {
            "433" if !tailed => match nick_for_tailed(peer, self.nicklen) {
                Some(nick) => {
                    self.puppets.get_mut(peer).expect("checked").state = PuppetState::Connecting {
                        nick: nick.clone(),
                        tailed: true,
                        attempt,
                    };
                    vec![PoolAction::Connect {
                        peer: peer.clone(),
                        nick,
                    }]
                }
                None => Vec::new(),
            },
            "433" => {
                self.puppets.get_mut(peer).expect("checked").state =
                    PuppetState::ChannelOnly(ChannelOnly::NickTaken);
                give_up(ChannelOnly::NickTaken)
            }
            "432" => {
                self.puppets.get_mut(peer).expect("checked").state =
                    PuppetState::ChannelOnly(ChannelOnly::NickErroneous);
                give_up(ChannelOnly::NickErroneous)
            }
            _ => self.disconnected(peer, now_ms),
        }
    }

    /// `peer`'s connection failed or dropped at `now_ms`. A registered puppet
    /// releases its nick. Either way the peer backs off on the 2 s → 5 min
    /// schedule and [`Pool::tick`] retries it when due.
    pub fn disconnected(&mut self, peer: &PeerId, now_ms: u64) -> Vec<PoolAction> {
        let Some(p) = self.puppets.get_mut(peer) else {
            return Vec::new();
        };
        let attempt = match &p.state {
            PuppetState::Connecting { attempt, .. } => *attempt + 1,
            PuppetState::BackingOff { attempt, .. } => *attempt,
            PuppetState::Registered { .. } => 1,
            PuppetState::Waiting | PuppetState::ChannelOnly(_) => return Vec::new(),
        };
        self.table.remove_peer(peer);
        p.state = PuppetState::BackingOff {
            until_ms: now_ms + backoff_ms(attempt),
            attempt,
        };
        Vec::new()
    }

    /// The server changed `CASEMAPPING`: the table re-derives from wire
    /// spellings; any two held nicks that now fold equal cannot both be kept,
    /// and the dropped one's peer goes back to `Waiting` so the next tick
    /// re-registers it (its plain form will now collide, so it gets the tail).
    pub fn set_casemapping(&mut self, cm: CaseMapping) -> Vec<PoolAction> {
        let mut actions = Vec::new();
        for (nick, peer) in self.table.set_casemapping(cm) {
            if let Some(p) = self.puppets.get_mut(&peer) {
                p.state = PuppetState::Waiting;
                actions.push(PoolAction::Quit {
                    peer: peer.clone(),
                    nick,
                });
            }
        }
        actions
    }

    /// Tear the pool down: every registered puppet quits, every connection in
    /// flight or retry pending is cancelled, and the table is emptied. The
    /// caller applies one bounded grace to the whole batch (design: "one
    /// bounded step for the whole pool").
    pub fn teardown(&mut self) -> Vec<PoolAction> {
        let peers: Vec<PeerId> = self.puppets.keys().cloned().collect();
        let mut actions = Vec::new();
        for peer in peers {
            actions.extend(self.forget(&peer));
        }
        actions
    }

    /// The gateway-owned nick set (folded) — what membership subtracts from
    /// human presence. Registered puppets only: a nick merely offered is not
    /// on the server yet.
    pub fn owned_folded(&self) -> impl Iterator<Item = &str> {
        self.table.owned_folded()
    }

    /// The peer behind a nick the gateway holds, if any.
    pub fn resolve(&self, nick: &str) -> Option<&PeerId> {
        self.table.resolve(nick)
    }

    /// The registered nick of `peer`, if it has one.
    pub fn nick_of(&self, peer: &PeerId) -> Option<&str> {
        self.table.nick_of(peer)
    }

    /// Whether `nick` (folded) is one of the gateway's — the own-nick-SET guard.
    pub fn is_owned(&self, nick: &str) -> bool {
        self.table.is_owned(nick)
    }

    /// `mu peers` rows, in `PeerId` order.
    pub fn status(&self) -> Vec<PuppetStatus> {
        self.puppets
            .iter()
            .map(|(peer, p)| PuppetStatus {
                peer: peer.clone(),
                nick: self.table.nick_of(peer).map(str::to_string),
                state: p.state.clone(),
            })
            .collect()
    }

    /// The state of one tracked peer.
    pub fn state_of(&self, peer: &PeerId) -> Option<&PuppetState> {
        self.puppets.get(peer).map(|p| &p.state)
    }

    /// How many puppets are registered right now.
    pub fn registered_count(&self) -> usize {
        self.table.len()
    }
}

/// Retry delay for the `attempt`-th failure (1-based): 2 s, 4 s, 8 s, … capped
/// at 5 min.
pub fn backoff_ms(attempt: u32) -> u64 {
    let shift = attempt.saturating_sub(1).min(16);
    (BACKOFF_MIN_MS << shift).min(BACKOFF_MAX_MS)
}

// ───────────────────────────── Fan-in filter ────────────────────────────────

/// Which gateway-held connection a line arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source<'a> {
    /// The main connection, `mu-gw`.
    Gateway,
    /// A puppet's connection, owned on behalf of this peer.
    Puppet(&'a PeerId),
}

/// The class of an inbound line, decided from the message alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineClass {
    /// A `PRIVMSG`/`NOTICE` to a channel, or any membership event
    /// (`JOIN`/`PART`/`KICK`/`QUIT`/`NICK`/`353`/`366`): the server sends one
    /// copy to every connection in the room.
    Channel,
    /// A `PRIVMSG`/`NOTICE` addressed to the receiving connection's own nick.
    Private,
    /// Everything else: registration numerics, `PING`, `ISUPPORT`, `ERROR`,
    /// `432`/`433`, `MODE`, … — per-connection protocol traffic.
    Control,
}

/// Why a line was dropped at the fan-in rather than routed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanInDrop {
    /// A channel-class line on a puppet: the gateway connection is the one
    /// consumer of channel input, so this copy is a duplicate.
    ChannelClassOnPuppet,
    /// A private-class line on a puppet whose target is NOT that puppet's
    /// own nick (a server oddity, or a NOTICE to a mask): nobody's to route.
    PrivateNotForThisNick,
    /// Control traffic on a puppet: the puppet's adapter handles it; routing
    /// never sees it.
    ControlOnPuppet,
}

/// The fan-in verdict for one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FanIn {
    /// Route it, as arriving on `Gateway`, or as a private line to the named
    /// puppet's peer.
    Route {
        via: Option<PeerId>,
        class: LineClass,
    },
    /// Drop it, counted under `FanInDrop`.
    Drop(FanInDrop),
}

/// Classify one inbound message by what it is, independent of where it came
/// from. A target is a channel when it begins with `#`, `&`, `!`, or `+`
/// (RFC 2812 channel prefixes); `own_nick` is the receiving connection's nick,
/// compared folded under `cm`.
pub fn line_class(msg: &IrcMessage, own_nick: &str, cm: CaseMapping) -> LineClass {
    match msg.command.as_str() {
        "PRIVMSG" | "NOTICE" => match msg.params.first() {
            Some(target) if target.starts_with(['#', '&', '!', '+']) => LineClass::Channel,
            Some(target) if fold_nick(target, cm) == fold_nick(own_nick, cm) => LineClass::Private,
            _ => LineClass::Control,
        },
        "JOIN" | "PART" | "KICK" | "QUIT" | "NICK" | "353" | "366" => LineClass::Channel,
        _ => LineClass::Control,
    }
}

/// The single-consumer rule. Channel-class input is consumed only from the
/// gateway connection; private-class input only from the connection it was
/// addressed to (tagged with that puppet's peer); control traffic on a puppet
/// belongs to its adapter and never reaches routing. On the gateway
/// connection everything is routed as in v0 — the session loop already
/// handles its own control traffic — so v0 behaviour is unchanged when no
/// puppet exists.
pub fn classify(source: Source<'_>, msg: &IrcMessage, own_nick: &str, cm: CaseMapping) -> FanIn {
    let class = line_class(msg, own_nick, cm);
    match (source, class) {
        (Source::Gateway, class) => FanIn::Route { via: None, class },
        (Source::Puppet(_), LineClass::Channel) => FanIn::Drop(FanInDrop::ChannelClassOnPuppet),
        (Source::Puppet(peer), LineClass::Private) => FanIn::Route {
            via: Some(peer.clone()),
            class: LineClass::Private,
        },
        (Source::Puppet(_), LineClass::Control) => {
            if matches!(msg.command.as_str(), "PRIVMSG" | "NOTICE") {
                FanIn::Drop(FanInDrop::PrivateNotForThisNick)
            } else {
                FanIn::Drop(FanInDrop::ControlOnPuppet)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> PuppetsConfig {
        PuppetsConfig::default()
    }

    fn cc(id: &str) -> PeerId {
        PeerId::parse(&format!("cc:{id}"))
    }

    #[test]
    fn ruling_a_session_shaped_peers_only() {
        let c = cfg();
        assert!(qualifies(&cc("abc"), &c));
        assert!(qualifies(&PeerId::mu_session("d", "session-1"), &c));
        assert!(
            !qualifies(&PeerId::mu_daemon("d"), &c),
            "bare daemon is channel-only"
        );
        assert!(!qualifies(&PeerId::human("alice"), &c));
        assert!(
            !qualifies(&PeerId::parse("warden:w1"), &c),
            "role not in roles"
        );
        assert!(!qualifies(&PeerId::parse("cc"), &c), "no id");
        let daemons = PuppetsConfig {
            daemons: true,
            ..cfg()
        };
        assert!(qualifies(&PeerId::mu_daemon("d"), &daemons));
    }

    #[test]
    fn backoff_schedule_doubles_from_two_seconds_to_five_minutes() {
        assert_eq!(backoff_ms(1), 2_000);
        assert_eq!(backoff_ms(2), 4_000);
        assert_eq!(backoff_ms(3), 8_000);
        assert_eq!(backoff_ms(8), 256_000);
        assert_eq!(backoff_ms(9), 300_000);
        assert_eq!(backoff_ms(40), 300_000);
        assert_eq!(backoff_ms(0), 2_000);
    }

    #[test]
    fn a_new_peer_waits_min_age_then_connects_under_the_plain_nick() {
        let mut pool = Pool::new(cfg(), 32, CaseMapping::Ascii);
        let a = cc("abc");
        assert!(pool.observe(std::slice::from_ref(&a), 1_000).is_empty());
        assert!(pool.tick(1_000).is_empty(), "too young");
        assert!(pool.tick(60_999).is_empty(), "still too young");
        assert_eq!(
            pool.tick(61_000),
            vec![PoolAction::Connect {
                peer: a.clone(),
                nick: "cc-abc".into()
            }]
        );
        assert!(pool.tick(61_000).is_empty(), "already in flight");
        assert!(pool.registered(&a, "cc-abc").is_empty());
        assert_eq!(pool.nick_of(&a), Some("cc-abc"));
        assert_eq!(pool.resolve("CC-ABC"), Some(&a));
        assert!(pool.is_owned("cc-abc"));
        assert_eq!(pool.registered_count(), 1);
    }

    #[test]
    fn disabled_config_never_acts() {
        let mut pool = Pool::new(
            PuppetsConfig {
                enabled: false,
                ..cfg()
            },
            32,
            CaseMapping::Ascii,
        );
        pool.observe(&[cc("abc")], 0);
        assert!(pool.tick(1_000_000).is_empty());
        assert_eq!(pool.registered_count(), 0);
    }

    #[test]
    fn pacing_limits_registrations_in_flight_and_cap_marks_the_rest_channel_only() {
        let mut pool = Pool::new(
            PuppetsConfig {
                max: 3,
                connect_parallelism: 2,
                min_age_secs: 0,
                ..cfg()
            },
            32,
            CaseMapping::Ascii,
        );
        let peers: Vec<PeerId> = (0..5).map(|i| cc(&format!("p{i}"))).collect();
        pool.observe(&peers, 0);
        let first = pool.tick(0);
        // Two in flight (parallelism), nobody capped yet: the cap counts
        // live puppets, and only two are.
        assert_eq!(
            first,
            vec![
                PoolAction::Connect {
                    peer: peers[0].clone(),
                    nick: "cc-p0".into()
                },
                PoolAction::Connect {
                    peer: peers[1].clone(),
                    nick: "cc-p1".into()
                },
            ]
        );
        assert!(
            pool.tick(0).is_empty(),
            "parallelism holds while both are in flight"
        );
        pool.registered(&peers[0], "cc-p0");
        // One slot freed: p2 connects; live is now 3 = max, so p3 and p4 are
        // capped, in PeerId order, each reported once.
        let second = pool.tick(0);
        assert_eq!(
            second,
            vec![
                PoolAction::Connect {
                    peer: peers[2].clone(),
                    nick: "cc-p2".into()
                },
                PoolAction::ChannelOnly {
                    peer: peers[3].clone(),
                    reason: ChannelOnly::OverCap
                },
                PoolAction::ChannelOnly {
                    peer: peers[4].clone(),
                    reason: ChannelOnly::OverCap
                },
            ]
        );
        assert!(
            pool.tick(0).is_empty(),
            "capped peers are not reported twice"
        );
        assert_eq!(
            pool.state_of(&peers[4]),
            Some(&PuppetState::ChannelOnly(ChannelOnly::OverCap))
        );
    }

    #[test]
    fn nick_in_use_offers_the_tailed_form_then_gives_up() {
        let mut pool = Pool::new(
            PuppetsConfig {
                min_age_secs: 0,
                ..cfg()
            },
            32,
            CaseMapping::Ascii,
        );
        let a = cc("abc");
        pool.observe(std::slice::from_ref(&a), 0);
        pool.tick(0);
        // 433 on the plain form: the tailed form is offered immediately.
        let tailed = nick_for_tailed(&a, 32).unwrap();
        assert_eq!(
            pool.nick_rejected(&a, "433", 0),
            vec![PoolAction::Connect {
                peer: a.clone(),
                nick: tailed.clone()
            }]
        );
        assert!(matches!(
            pool.state_of(&a),
            Some(PuppetState::Connecting { tailed: true, .. })
        ));
        // 433 on the tailed form too: channel-only, reported once.
        assert_eq!(
            pool.nick_rejected(&a, "433", 0),
            vec![PoolAction::ChannelOnly {
                peer: a.clone(),
                reason: ChannelOnly::NickTaken
            }]
        );
        assert!(
            pool.tick(1_000_000).is_empty(),
            "a given-up peer is never retried"
        );
        assert_eq!(pool.registered_count(), 0);
    }

    #[test]
    fn erroneous_nick_is_channel_only_at_once() {
        let mut pool = Pool::new(
            PuppetsConfig {
                min_age_secs: 0,
                ..cfg()
            },
            32,
            CaseMapping::Ascii,
        );
        let a = cc("abc");
        pool.observe(std::slice::from_ref(&a), 0);
        pool.tick(0);
        assert_eq!(
            pool.nick_rejected(&a, "432", 0),
            vec![PoolAction::ChannelOnly {
                peer: a.clone(),
                reason: ChannelOnly::NickErroneous
            }]
        );
    }

    #[test]
    fn a_failed_connection_backs_off_on_the_schedule_and_retries_when_due() {
        let mut pool = Pool::new(
            PuppetsConfig {
                min_age_secs: 0,
                ..cfg()
            },
            32,
            CaseMapping::Ascii,
        );
        let a = cc("abc");
        pool.observe(std::slice::from_ref(&a), 0);
        pool.tick(0);
        assert!(pool.disconnected(&a, 10_000).is_empty());
        assert_eq!(
            pool.state_of(&a),
            Some(&PuppetState::BackingOff {
                until_ms: 12_000,
                attempt: 1
            })
        );
        assert!(pool.tick(11_999).is_empty());
        assert_eq!(pool.tick(12_000).len(), 1, "retried when due");
        // Second failure doubles the wait; a registered puppet that drops
        // starts the schedule over.
        pool.disconnected(&a, 20_000);
        assert_eq!(
            pool.state_of(&a),
            Some(&PuppetState::BackingOff {
                until_ms: 24_000,
                attempt: 2
            })
        );
        pool.tick(24_000);
        pool.registered(&a, "cc-abc");
        assert!(pool.is_owned("cc-abc"));
        pool.disconnected(&a, 30_000);
        assert!(
            !pool.is_owned("cc-abc"),
            "a dropped puppet releases its nick"
        );
        assert_eq!(
            pool.state_of(&a),
            Some(&PuppetState::BackingOff {
                until_ms: 32_000,
                attempt: 1
            })
        );
    }

    #[test]
    fn a_peer_leaving_the_mesh_quits_or_cancels_its_puppet() {
        let mut pool = Pool::new(
            PuppetsConfig {
                min_age_secs: 0,
                ..cfg()
            },
            32,
            CaseMapping::Ascii,
        );
        let a = cc("abc");
        let b = cc("bcd");
        pool.observe(&[a.clone(), b.clone()], 0);
        pool.tick(0);
        pool.registered(&a, "cc-abc");
        // a registered, b in flight; both vanish from the next snapshot.
        let actions = pool.observe(&[], 1);
        assert_eq!(
            actions,
            vec![
                PoolAction::Quit {
                    peer: a.clone(),
                    nick: "cc-abc".into()
                },
                PoolAction::Cancel { peer: b.clone() },
            ]
        );
        assert!(!pool.is_owned("cc-abc"));
        assert!(pool.status().is_empty());
        // A registration that lands after the peer left is quit, not kept.
        assert_eq!(
            pool.registered(&b, "cc-bcd"),
            vec![PoolAction::Quit {
                peer: b.clone(),
                nick: "cc-bcd".into()
            }]
        );
    }

    #[test]
    fn two_peers_whose_plain_nicks_fold_equal_get_the_tail_for_the_second() {
        let mut pool = Pool::new(
            PuppetsConfig {
                min_age_secs: 0,
                connect_parallelism: 1,
                ..cfg()
            },
            32,
            CaseMapping::Ascii,
        );
        let a = cc("abc");
        let b = cc("ABC");
        pool.observe(&[a.clone(), b.clone()], 0);
        // BTreeMap order puts `cc:ABC` before `cc:abc`; whichever registers
        // first holds the plain form, the other is offered the tail.
        let first = pool.tick(0);
        let PoolAction::Connect { peer: p1, nick: n1 } = &first[0] else {
            panic!("{first:?}");
        };
        pool.registered(p1, n1);
        let second = pool.tick(0);
        let PoolAction::Connect { peer: p2, nick: n2 } = &second[0] else {
            panic!("{second:?}");
        };
        assert_ne!(p1, p2);
        assert_eq!(n2, &nick_for_tailed(p2, 32).unwrap());
        assert_ne!(
            fold_nick(n1, CaseMapping::Ascii),
            fold_nick(n2, CaseMapping::Ascii)
        );
    }

    #[test]
    fn teardown_quits_registered_cancels_the_rest_and_empties_the_table() {
        let mut pool = Pool::new(
            PuppetsConfig {
                min_age_secs: 0,
                connect_parallelism: 3,
                ..cfg()
            },
            32,
            CaseMapping::Ascii,
        );
        let a = cc("abc");
        let b = cc("bcd");
        let c = cc("cde");
        pool.observe(&[a.clone(), b.clone(), c.clone()], 0);
        pool.tick(0);
        pool.registered(&a, "cc-abc");
        // a registered, b in flight, c backing off: three states, one step.
        pool.disconnected(&c, 0);
        let actions = pool.teardown();
        assert_eq!(
            actions,
            vec![
                PoolAction::Quit {
                    peer: a.clone(),
                    nick: "cc-abc".into()
                },
                PoolAction::Cancel { peer: b.clone() },
                PoolAction::Cancel { peer: c.clone() },
            ]
        );
        assert_eq!(pool.registered_count(), 0);
        assert!(pool.status().is_empty());
    }

    #[test]
    fn casemapping_change_requeues_the_peer_whose_nick_now_collides() {
        let mut pool = Pool::new(
            PuppetsConfig {
                min_age_secs: 0,
                ..cfg()
            },
            32,
            CaseMapping::Ascii,
        );
        let a = cc("a[b");
        let b = cc("a{b");
        pool.observe(&[a.clone(), b.clone()], 0);
        pool.tick(0);
        pool.registered(&a, "cc-a[b");
        pool.registered(&b, "cc-a{b");
        assert_eq!(pool.registered_count(), 2, "distinct under ascii");
        let actions = pool.set_casemapping(CaseMapping::Rfc1459);
        assert_eq!(
            actions,
            vec![PoolAction::Quit {
                peer: b.clone(),
                nick: "cc-a{b".into()
            }]
        );
        assert_eq!(pool.state_of(&b), Some(&PuppetState::Waiting));
        assert_eq!(pool.registered_count(), 1);
        // The next tick re-offers b: its plain form now folds equal to a's, so
        // it is offered the tailed form straight away.
        let again = pool.tick(0);
        assert_eq!(
            again,
            vec![PoolAction::Connect {
                peer: b.clone(),
                nick: nick_for_tailed(&b, 32).unwrap()
            }]
        );
    }

    fn msg(line: &str) -> IrcMessage {
        IrcMessage::parse(line)
    }

    #[test]
    fn line_classes_follow_the_target_not_the_connection() {
        let cm = CaseMapping::Ascii;
        assert_eq!(
            line_class(&msg(":alice!u@h PRIVMSG #mu :hi"), "cc-abc", cm),
            LineClass::Channel
        );
        assert_eq!(
            line_class(&msg(":alice!u@h PRIVMSG cc-abc :hi"), "cc-abc", cm),
            LineClass::Private
        );
        assert_eq!(
            line_class(&msg(":alice!u@h NOTICE CC-ABC :hi"), "cc-abc", cm),
            LineClass::Private,
            "own nick compared folded"
        );
        assert_eq!(
            line_class(&msg(":alice!u@h PRIVMSG mu-gw :hi"), "cc-abc", cm),
            LineClass::Control,
            "a private line to some other nick is not this connection's"
        );
        for line in [
            ":alice!u@h JOIN #mu",
            ":alice!u@h PART #mu :bye",
            ":op!u@h KICK #mu alice :out",
            ":alice!u@h QUIT :gone",
            ":alice!u@h NICK bob",
            ":srv 353 cc-abc = #mu :alice bob",
            ":srv 366 cc-abc #mu :End of /NAMES list.",
        ] {
            assert_eq!(
                line_class(&msg(line), "cc-abc", cm),
                LineClass::Channel,
                "{line}"
            );
        }
        for line in [
            "PING :srv",
            ":srv 433 * cc-abc :Nickname is already in use",
            ":srv 005 cc-abc NICKLEN=32 :are supported",
            "ERROR :Closing link",
        ] {
            assert_eq!(
                line_class(&msg(line), "cc-abc", cm),
                LineClass::Control,
                "{line}"
            );
        }
    }

    #[test]
    fn single_consumer_rule_routes_each_line_exactly_once() {
        let cm = CaseMapping::Ascii;
        let a = cc("abc");
        // A human line in #mu reaches every connection; only the gateway's
        // copy is routed.
        let chan = msg(":alice!u@h PRIVMSG #mu :hello all");
        assert_eq!(
            classify(Source::Gateway, &chan, "mu-gw", cm),
            FanIn::Route {
                via: None,
                class: LineClass::Channel
            }
        );
        assert_eq!(
            classify(Source::Puppet(&a), &chan, "cc-abc", cm),
            FanIn::Drop(FanInDrop::ChannelClassOnPuppet)
        );
        // A /query line to the puppet arrives only on the puppet, tagged.
        let private = msg(":alice!u@h PRIVMSG cc-abc :just you");
        assert_eq!(
            classify(Source::Puppet(&a), &private, "cc-abc", cm),
            FanIn::Route {
                via: Some(a.clone()),
                class: LineClass::Private
            }
        );
        // The same private line seen by the gateway connection (it is not the
        // gateway's nick) is control-class there and routed as v0 does — the
        // session loop decides; nothing here invents a second consumer.
        assert_eq!(
            classify(Source::Gateway, &private, "mu-gw", cm),
            FanIn::Route {
                via: None,
                class: LineClass::Control
            }
        );
        // A private line to mu-gw that a puppet somehow receives is nobody's.
        let to_gw = msg(":alice!u@h PRIVMSG mu-gw :hi");
        assert_eq!(
            classify(Source::Puppet(&a), &to_gw, "cc-abc", cm),
            FanIn::Drop(FanInDrop::PrivateNotForThisNick)
        );
        // Membership events and control traffic on a puppet never reach routing.
        assert_eq!(
            classify(Source::Puppet(&a), &msg(":bob!u@h JOIN #mu"), "cc-abc", cm),
            FanIn::Drop(FanInDrop::ChannelClassOnPuppet)
        );
        assert_eq!(
            classify(Source::Puppet(&a), &msg("PING :srv"), "cc-abc", cm),
            FanIn::Drop(FanInDrop::ControlOnPuppet)
        );
        // With no puppets, the gateway routes everything as before.
        assert_eq!(
            classify(Source::Gateway, &msg("PING :srv"), "mu-gw", cm),
            FanIn::Route {
                via: None,
                class: LineClass::Control
            }
        );
    }
}
