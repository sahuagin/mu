//! Offline tests for the leased slot pool. No socket, no timer: every case
//! drives the state machine with events and a supplied clock.

use mu_irc_gateway::slots::{Grant, Slots};
use mu_peer::PeerId;

const MIN: u64 = 60_000;

fn pool(n: usize, idle_ms: u64) -> Slots {
    Slots::new((1..=n).map(|i| format!("cc-{i}")).collect(), idle_ms)
}

fn cc(id: &str) -> PeerId {
    PeerId::parse(&format!("cc:{id}"))
}

/// Nobody is protected.
fn open(_: &PeerId) -> bool {
    false
}

#[test]
fn a_free_slot_is_granted_lowest_first_and_deterministically() {
    let mut s = pool(3, 30 * MIN);
    assert_eq!(
        s.lease(&cc("a"), 0, &open),
        Grant::Leased {
            account: "cc-1".into(),
            evicted: None
        }
    );
    assert_eq!(
        s.lease(&cc("b"), 0, &open),
        Grant::Leased {
            account: "cc-2".into(),
            evicted: None
        }
    );
    assert_eq!(s.free(), 1);
    assert_eq!(s.leased_accounts(), vec!["cc-1", "cc-2"]);
}

#[test]
fn asking_twice_refreshes_rather_than_taking_a_second_slot() {
    let mut s = pool(3, 30 * MIN);
    s.lease(&cc("a"), 0, &open);
    assert_eq!(
        s.lease(&cc("a"), 5 * MIN, &open),
        Grant::Held("cc-1".into())
    );
    assert_eq!(s.free(), 2, "one peer never holds two slots");
    // …and the refresh moves its activity forward, which is the point: a peer
    // that keeps asking keeps its slot. Without the refresh at 60, `a` would
    // be an hour stale and evictable at 61.
    let mut full = pool(1, 10 * MIN);
    full.lease(&cc("a"), 0, &open);
    full.lease(&cc("a"), 60 * MIN, &open);
    assert_eq!(
        full.lease(&cc("b"), 61 * MIN, &open),
        Grant::Spillover,
        "one minute after the refresh the lease is fresh, so it is not taken"
    );
    // Ten minutes after the refresh — the window — it becomes available.
    assert_eq!(
        full.lease(&cc("b"), 70 * MIN, &open),
        Grant::Leased {
            account: "cc-1".into(),
            evicted: Some(cc("a"))
        }
    );
}

#[test]
fn a_lease_inside_its_idle_window_is_not_evictable() {
    let mut s = pool(1, 30 * MIN);
    s.lease(&cc("busy"), 0, &open);
    // 29 minutes later it is still within the window: the newcomer spills.
    assert_eq!(s.lease(&cc("new"), 29 * MIN, &open), Grant::Spillover);
    assert_eq!(s.evictions(), 0);
    // At the window it becomes available.
    assert_eq!(
        s.lease(&cc("new"), 30 * MIN, &open),
        Grant::Leased {
            account: "cc-1".into(),
            evicted: Some(cc("busy"))
        }
    );
    assert_eq!(s.evictions(), 1, "an eviction is counted, never silent");
}

#[test]
fn the_least_recently_active_lease_is_the_one_taken() {
    let mut s = pool(3, 10 * MIN);
    s.lease(&cc("a"), 0, &open);
    s.lease(&cc("b"), 0, &open);
    s.lease(&cc("c"), 0, &open);
    // b and c stay alive; a goes quiet.
    s.touch(&cc("b"), 20 * MIN);
    s.touch(&cc("c"), 25 * MIN);
    assert_eq!(
        s.lease(&cc("d"), 30 * MIN, &open),
        Grant::Leased {
            account: "cc-1".into(),
            evicted: Some(cc("a"))
        },
        "the stalest, not merely the first"
    );
}

#[test]
fn a_peer_mid_conversation_with_a_human_is_never_evicted() {
    // The one eviction a human would notice. The routing memory knows which
    // agents have traded a line with a person recently; this asks it.
    let mut s = pool(2, 10 * MIN);
    s.lease(&cc("talking"), 0, &open);
    s.lease(&cc("idle"), 0, &open);
    let protected = |p: &PeerId| *p == cc("talking");
    // `talking` is staler, but exempt — so `idle` goes instead.
    s.touch(&cc("idle"), 5 * MIN);
    assert_eq!(
        s.lease(&cc("new"), 40 * MIN, &protected),
        Grant::Leased {
            account: "cc-2".into(),
            evicted: Some(cc("idle"))
        },
        "the protected lease is skipped even though it is the stalest"
    );
    assert_eq!(s.account_of(&cc("talking")), Some("cc-1"));
}

#[test]
fn a_full_pool_of_protected_leases_spills_rather_than_sharing_a_nick() {
    // Spillover is a fallback, not a shared nick: those agents stay reachable
    // through mu-gw. Sharing one would destroy the identity this exists for.
    let mut s = pool(2, 10 * MIN);
    s.lease(&cc("a"), 0, &open);
    s.lease(&cc("b"), 0, &open);
    let all = |_: &PeerId| true;
    assert_eq!(s.lease(&cc("c"), 99 * MIN, &all), Grant::Spillover);
    assert_eq!(s.evictions(), 0);
    assert_eq!(s.account_of(&cc("c")), None, "no slot, and no shared one");
    assert_eq!(s.leased_accounts(), vec!["cc-1", "cc-2"]);
}

#[test]
fn releasing_returns_the_slot_to_the_free_list() {
    let mut s = pool(2, 10 * MIN);
    s.lease(&cc("a"), 0, &open);
    assert_eq!(s.release(&cc("a")).as_deref(), Some("cc-1"));
    assert_eq!(s.free(), 2);
    assert_eq!(s.account_of(&cc("a")), None);
    assert_eq!(s.release(&cc("a")), None, "releasing twice is harmless");
    // The freed slot is reused lowest-first, so it comes back.
    assert_eq!(
        s.lease(&cc("b"), MIN, &open),
        Grant::Leased {
            account: "cc-1".into(),
            evicted: None
        }
    );
}

#[test]
fn touching_a_peer_with_no_lease_does_not_grant_one() {
    // Activity is not a claim: only `lease` hands out a slot.
    let mut s = pool(1, 10 * MIN);
    s.touch(&cc("nobody"), 5 * MIN);
    assert_eq!(s.free(), 1);
    assert_eq!(s.account_of(&cc("nobody")), None);
}

#[test]
fn an_empty_pool_always_spills() {
    // `slot_certs_dir` unset, or a pool sized to nothing: every peer is
    // channel-only, which is exactly v0's behaviour.
    let mut s = pool(0, 10 * MIN);
    assert_eq!(s.lease(&cc("a"), 0, &open), Grant::Spillover);
    assert_eq!(s.free(), 0);
}
