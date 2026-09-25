//! The leased slot pool: which pre-registered account a peer speaks as.
//!
//! Offline, like [`crate::membership`] and [`crate::puppets`] — no socket, no
//! timer, no stored state that outlives the process. Time arrives as a
//! millisecond stamp the caller supplies, so every decision here is a pure
//! function of the events fed to it and two gateways given the same events
//! make the same decisions in the same order.
//!
//! The pool is a fixed set of accounts (`cc-1`…`cc-<max>`, from
//! [`PuppetsConfig::slot_accounts`]) that were registered on the server once,
//! by hand or by a one-off script. A qualifying peer LEASES one, speaks as it,
//! and returns it when it goes. Nothing is created per session, so nothing
//! accumulates on the server and nothing has to be garbage-collected — the
//! property that ruled out an account per agent instance
//! (`specs/plans/mu-irc-gateway-v1-puppets.md`).
//!
//! # Leases go by ACTIVITY, not presence
//!
//! A `mu ask` peer stays *discoverable* for about an hour after its last
//! heartbeat, so handing slots out on discovery alone would give them to
//! sessions that finished long ago and keep them there. A peer holds its slot
//! while it is ACTIVE — [`Slots::touch`] on each sign of life — and a lease
//! idle longer than the configured window becomes evictable. Nothing is
//! evicted by the clock alone: idleness only makes a lease *available* to a
//! peer that wants one.
//!
//! # Exhaustion is loud, and never takes a conversation
//!
//! When every slot is held and someone new qualifies, the LEAST RECENTLY
//! ACTIVE evictable lease is taken — reported, never silent, because a cleanup
//! path that stalls quietly is what broke Libera's bridge. Two things are
//! exempt: a lease still inside its idle window, and any peer the caller marks
//! PROTECTED (the routing memory knows which agents have traded a line with a
//! human recently; taking a nick mid-conversation is the one eviction a human
//! would notice).
//!
//! When nothing may be taken, the answer is [`Grant::Spillover`] — NOT a
//! shared nick. Those agents stay reachable the v0 way, through `mu-gw` and
//! their own channel. Sharing one nick between agents would destroy exactly
//! the identity this whole increment exists to give them.
//!
//! # Nothing calls this yet
//!
//! No puppet leases a slot today: outside tests, nothing constructs [`Slots`].
//! The pool's decisions are landed on their own, as [`crate::membership`] and
//! [`crate::puppets`] were, so that what they are can be read and argued with
//! before a socket depends on them. The increment that wires it up leases a
//! slot per qualifying peer, connects as that account with SASL EXTERNAL, and
//! hands [`Slots::leased_accounts`] to membership as the set that is ours.
//!
//! Read the present tense here as describing the RULES, not a running pool.

use std::collections::HashMap;

use mu_peer::PeerId;

/// One held slot.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Lease {
    peer: PeerId,
    /// When this peer was last seen to be active, in the caller's clock.
    last_active_ms: u64,
}

/// What asking for a slot produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grant {
    /// The peer now holds this account. Carries the previous holder when the
    /// grant came from an eviction, so the caller can tear that puppet down
    /// and say so — an eviction is never silent.
    Leased {
        account: String,
        evicted: Option<PeerId>,
    },
    /// The peer already held a slot; the lease is refreshed, not moved.
    Held(String),
    /// Every slot is held and none may be taken: too recently active, or
    /// protected. The peer gets no puppet and stays reachable through the
    /// gateway's own nick.
    Spillover,
}

/// The pool and who holds what.
#[derive(Debug, Clone)]
pub struct Slots {
    /// The accounts, in pool order. A free slot is granted lowest-first so a
    /// given sequence of events always produces the same assignment.
    accounts: Vec<String>,
    /// account → lease. Absent means free.
    held: HashMap<String, Lease>,
    /// peer → the account it holds, so a repeat request is O(1) and cannot
    /// hand one peer two slots.
    by_peer: HashMap<PeerId, String>,
    /// How long a lease must be idle before it may be evicted.
    idle_window_ms: u64,
    /// Evictions since the process started. Reported, never reset here: a
    /// rising count is the signal that the pool is too small for the fleet.
    evictions: u64,
}

impl Slots {
    /// A pool of `accounts`, none held, evicting only leases idle at least
    /// `idle_window_ms`.
    pub fn new(accounts: Vec<String>, idle_window_ms: u64) -> Self {
        Slots {
            accounts,
            held: HashMap::new(),
            by_peer: HashMap::new(),
            idle_window_ms,
            evictions: 0,
        }
    }

    /// Mark `peer` active. Cheap and idempotent; a peer with no lease is
    /// ignored rather than granted one, because activity is not a claim.
    pub fn touch(&mut self, peer: &PeerId, now_ms: u64) {
        if let Some(account) = self.by_peer.get(peer) {
            if let Some(lease) = self.held.get_mut(account) {
                lease.last_active_ms = now_ms;
            }
        }
    }

    /// Give `peer` a slot, or say why not.
    ///
    /// `protected` answers "has this peer traded a line with a human recently"
    /// — it is asked only about eviction candidates, never about the peer
    /// doing the asking.
    pub fn lease(
        &mut self,
        peer: &PeerId,
        now_ms: u64,
        protected: &dyn Fn(&PeerId) -> bool,
    ) -> Grant {
        if let Some(account) = self.by_peer.get(peer).cloned() {
            self.touch(peer, now_ms);
            return Grant::Held(account);
        }
        // A free slot first, lowest-numbered, before anyone is disturbed.
        if let Some(account) = self.accounts.iter().find(|a| !self.held.contains_key(*a)) {
            let account = account.clone();
            self.grant(&account, peer, now_ms);
            return Grant::Leased {
                account,
                evicted: None,
            };
        }
        // Otherwise the least recently active EVICTABLE lease. Ties break on
        // the account's pool order, so the choice is deterministic.
        let victim = self
            .accounts
            .iter()
            .filter_map(|a| self.held.get(a).map(|l| (a, l)))
            .filter(|(_, l)| now_ms.saturating_sub(l.last_active_ms) >= self.idle_window_ms)
            .filter(|(_, l)| !protected(&l.peer))
            .min_by_key(|(_, l)| l.last_active_ms)
            .map(|(a, l)| (a.clone(), l.peer.clone()));
        match victim {
            Some((account, previous)) => {
                self.by_peer.remove(&previous);
                self.grant(&account, peer, now_ms);
                self.evictions += 1;
                Grant::Leased {
                    account,
                    evicted: Some(previous),
                }
            }
            None => Grant::Spillover,
        }
    }

    /// Return `peer`'s slot to the free list, if it held one.
    ///
    /// The CALLER owes an ordering guarantee here: a slot must not be returned
    /// before the gateway has observed the puppet's departure. Membership
    /// treats an account as its own for as long as the connection exists, so
    /// returning early would let a still-present puppet be read as a human
    /// (`mu-irc-remote-session-zgbdz.4.1`).
    pub fn release(&mut self, peer: &PeerId) -> Option<String> {
        let account = self.by_peer.remove(peer)?;
        self.held.remove(&account);
        Some(account)
    }

    /// The accounts currently held, in pool order — what membership is told is
    /// ours.
    pub fn leased_accounts(&self) -> Vec<&str> {
        self.accounts
            .iter()
            .filter(|a| self.held.contains_key(*a))
            .map(String::as_str)
            .collect()
    }

    /// The account `peer` holds, if any.
    pub fn account_of(&self, peer: &PeerId) -> Option<&str> {
        self.by_peer.get(peer).map(String::as_str)
    }

    /// How many leases have been taken from one peer and given to another.
    pub fn evictions(&self) -> u64 {
        self.evictions
    }

    /// Slots not currently held.
    pub fn free(&self) -> usize {
        self.accounts.len() - self.held.len()
    }

    fn grant(&mut self, account: &str, peer: &PeerId, now_ms: u64) {
        self.held.insert(
            account.to_string(),
            Lease {
                peer: peer.clone(),
                last_active_ms: now_ms,
            },
        );
        self.by_peer.insert(peer.clone(), account.to_string());
    }
}
