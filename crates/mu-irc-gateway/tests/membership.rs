//! Offline membership + channel-lifecycle tests for increment 2b. No socket:
//! every case drives the two state machines with normalized events and asserts
//! on the effects and observed state.

use mu_irc_gateway::mapping::CaseMapping;
use mu_irc_gateway::membership::{ChannelEffect, ChannelReconciler, HumanEffect, Membership};
use mu_peer::PeerId;

const RFC: CaseMapping = CaseMapping::Rfc1459;

fn human(nick: &str) -> PeerId {
    PeerId::human(nick)
}

/// Names for a NAMES reply: `(nick, account)` pairs.
fn names(list: &[(&str, Option<&str>)]) -> Vec<(String, Option<String>)> {
    list.iter()
        .map(|(n, a)| (n.to_string(), a.map(str::to_string)))
        .collect()
}

// ────────────────────────────── Membership ──────────────────────────────────

#[test]
fn names_sync_registers_humans_and_excludes_self() {
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#mu");
    m.names_reply(
        "#mu",
        g,
        names(&[("alice", None), ("@bob", Some("bobacct")), ("mu-gw", None)]),
    );
    let effects = m.names_end("#mu", g);
    assert!(effects.contains(&HumanEffect::Register(human("alice"))));
    assert!(effects.contains(&HumanEffect::Register(human("bob"))));
    assert_eq!(effects.len(), 2, "the gateway's own nick is not a human");
    // Presence folds under CASEMAPPING; the gateway is never present as a human.
    assert!(m.is_present("Alice"));
    assert!(m.is_present("bob"));
    assert!(!m.is_present("mu-gw"));
}

#[test]
fn interleaved_events_survive_the_names_commit() {
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#a");
    m.names_reply("#a", g, names(&[("alice", None)]));
    // A JOIN and a PART arrive mid-sync, before ENDOFNAMES.
    let e = m.joined("#a", "carol", None);
    assert_eq!(e, vec![HumanEffect::Register(human("carol"))]);
    m.left("#a", "alice"); // alice leaves before the roster ever committed
    let committed = m.names_end("#a", g);
    // carol was already present; committing does not re-register her.
    assert!(committed.is_empty());
    assert!(
        m.is_present("carol"),
        "interleaved JOIN survived the commit"
    );
    assert!(!m.is_present("alice"), "interleaved PART was not clobbered");
}

#[test]
fn stale_names_generation_is_ignored() {
    let mut m = Membership::new("mu-gw", RFC);
    let _g1 = m.self_joined("#a");
    let g2 = m.self_joined("#a"); // a second sync supersedes the first
                                  // A reply/end tagged with the OLD generation is dropped.
    m.names_reply("#a", _g1, names(&[("ghost", None)]));
    assert!(m.names_end("#a", _g1).is_empty());
    // The current generation is honoured.
    m.names_reply("#a", g2, names(&[("real", None)]));
    let e = m.names_end("#a", g2);
    assert_eq!(e, vec![HumanEffect::Register(human("real"))]);
    assert!(!m.is_present("ghost"));
    assert!(m.is_present("real"));
}

#[test]
fn a_resync_that_drops_a_member_withdraws_them() {
    let mut m = Membership::new("mu-gw", RFC);
    let g1 = m.self_joined("#a");
    m.names_reply("#a", g1, names(&[("alice", None), ("carol", None)]));
    let e = m.names_end("#a", g1);
    assert_eq!(e.len(), 2, "both registered on first sync");
    // A fresh sync of the same channel no longer lists alice: she is withdrawn
    // (it was her only channel), carol is unchanged.
    let g2 = m.self_joined("#a");
    m.names_reply("#a", g2, names(&[("carol", None)]));
    let e = m.names_end("#a", g2);
    assert_eq!(e, vec![HumanEffect::Withdraw(human("alice"))]);
    assert!(!m.is_present("alice"));
    assert!(m.is_present("carol"));
}

#[test]
fn shared_channel_ownership_holds_until_the_last_departure() {
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#a");
    m.self_joined("#b");
    assert_eq!(
        m.joined("#a", "alice", None),
        vec![HumanEffect::Register(human("alice"))]
    );
    // Present in a second channel: no new register.
    assert!(m.joined("#b", "alice", None).is_empty());
    // Leaving one channel does not withdraw her — she is still in the other.
    assert!(m.left("#a", "alice").is_empty());
    assert!(m.is_present("alice"));
    // Leaving the last one does.
    assert_eq!(
        m.left("#b", "alice"),
        vec![HumanEffect::Withdraw(human("alice"))]
    );
    assert!(!m.is_present("alice"));
}

#[test]
fn gateway_part_drops_the_channel_and_withdraws_its_only_humans() {
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#a");
    m.joined("#a", "alice", None);
    // The gateway itself parts (its own nick): the channel and its humans go.
    let e = m.left("#a", "mu-gw");
    assert_eq!(e, vec![HumanEffect::Withdraw(human("alice"))]);
    assert!(!m.is_present("alice"));
    assert!(m.joined_channels().is_empty());
}

#[test]
fn quit_withdraws_from_every_channel_once() {
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#a");
    m.self_joined("#b");
    m.joined("#a", "alice", None);
    m.joined("#b", "alice", None);
    // A single QUIT removes her everywhere and withdraws exactly once.
    assert_eq!(m.quit("alice"), vec![HumanEffect::Withdraw(human("alice"))]);
    assert!(!m.is_present("alice"));
}

#[test]
fn nick_rename_transfers_identity_but_case_only_change_does_not() {
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#a");
    m.joined("#a", "alice", None);
    // A real rename: one atomic transfer effect, old identity gone.
    assert_eq!(
        m.renamed("alice", "alice2"),
        vec![HumanEffect::Rename {
            from: human("alice"),
            to: human("alice2"),
        }]
    );
    assert!(!m.is_present("alice"));
    assert!(m.is_present("alice2"));

    // A case-only change folds to the same identity: no transfer.
    m.joined("#a", "bob", None);
    assert!(m.renamed("bob", "BOB").is_empty());
    assert!(m.is_present("bob"));
}

#[test]
fn reset_withdraws_all_humans_and_forgets_channels() {
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#a");
    m.joined("#a", "alice", None);
    m.joined("#a", "carol", None);
    let e = m.reset();
    assert!(e.contains(&HumanEffect::Withdraw(human("alice"))));
    assert!(e.contains(&HumanEffect::Withdraw(human("carol"))));
    assert_eq!(e.len(), 2);
    assert!(!m.is_present("alice"));
    assert!(m.joined_channels().is_empty());
}

#[test]
fn incompatible_casemapping_invalidates_like_a_disconnect() {
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#a");
    m.joined("#a", "alice", None);
    // Switching to ascii could re-partition folded identities, so all is dropped.
    let e = m.set_casemapping(CaseMapping::Ascii);
    assert_eq!(e, vec![HumanEffect::Withdraw(human("alice"))]);
    assert!(m.joined_channels().is_empty());
    // The same mapping is a no-op.
    assert!(m.set_casemapping(CaseMapping::Ascii).is_empty());
}

#[test]
fn a_departure_beats_a_names_entry_from_the_same_generation() {
    // The exact ordering the roster commit used to lose: the gateway WATCHES
    // alice leave, and only afterwards does the NAMES snapshot — taken before
    // she left — name her. Removing her from `pending` was a no-op (she was
    // never in it), so the later line resurrected her and names_end committed
    // her as present. Presence is what gates disclosing a private body, so a
    // resurrected member is a body delivered to someone who already left.
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#a");
    let e = m.left("#a", "alice");
    assert!(e.is_empty(), "she was never registered: {e:?}");
    m.names_reply("#a", g, names(&[("alice", None)]));
    let committed = m.names_end("#a", g);
    assert!(
        committed.is_empty(),
        "the stale NAMES entry re-registered a departed human: {committed:?}"
    );
    assert!(!m.is_present("alice"), "alice was resurrected by NAMES");

    // The tombstone is scoped to the sync it belongs to: a LATER sync (a fresh
    // generation, e.g. after a rejoin) legitimately re-registers her.
    let g2 = m.self_joined("#a");
    m.names_reply("#a", g2, names(&[("alice", None)]));
    let e = m.names_end("#a", g2);
    assert_eq!(e, vec![HumanEffect::Register(human("alice"))]);
    assert!(m.is_present("alice"));
}

#[test]
fn a_rename_during_the_initial_sync_still_tombstones_the_old_nick() {
    // The user renames BEFORE their own NAMES line arrives on the very first
    // sync: they are in neither `members` nor `pending`, so nothing "carries"
    // across — but a NICK away from `alice` retires that nick regardless, and
    // the delayed snapshot line naming it is stale. It used to commit alice as
    // present (and then front her) next to the live alice2.
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#a");
    let e = m.renamed("alice", "alice2");
    assert!(e.is_empty(), "nobody was registered yet: {e:?}");
    m.names_reply("#a", g, names(&[("alice", None)]));
    let committed = m.names_end("#a", g);
    assert!(
        !committed.contains(&HumanEffect::Register(human("alice"))),
        "the stale NAMES entry registered the pre-rename nick: {committed:?}"
    );
    assert!(!m.is_present("alice"), "alice was resurrected by NAMES");

    // A live JOIN under the freed nick is newer still and clears the tombstone.
    let g2 = m.self_joined("#a");
    m.joined("#a", "alice", None);
    m.names_reply("#a", g2, names(&[("alice", None)]));
    m.names_end("#a", g2);
    assert!(m.is_present("alice"));
}

#[test]
fn a_quit_or_rename_mid_sync_also_beats_a_later_names_entry() {
    // Same rule, reached through the other two departure paths.
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#a");
    m.quit("bob");
    m.names_reply("#a", g, names(&[("bob", None)]));
    assert!(m.names_end("#a", g).is_empty());
    assert!(!m.is_present("bob"), "QUIT was clobbered by NAMES");

    // A NICK away from an identity tombstones the OLD one but not the new.
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#a");
    m.joined("#a", "carol", None);
    m.renamed("carol", "carol2");
    m.names_reply("#a", g, names(&[("carol", None), ("carol2", None)]));
    m.names_end("#a", g);
    assert!(!m.is_present("carol"), "the pre-rename nick came back");
    assert!(m.is_present("carol2"), "the post-rename nick was lost");
}

#[test]
fn a_rejoin_after_a_mid_sync_part_is_not_tombstoned() {
    // The tombstone is "the last thing I saw was a departure", not a ban: a
    // live JOIN afterwards is newer still and must win.
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#a");
    m.left("#a", "alice");
    let e = m.joined("#a", "alice", None);
    assert_eq!(e, vec![HumanEffect::Register(human("alice"))]);
    m.names_reply("#a", g, names(&[("alice", None)]));
    m.names_end("#a", g);
    assert!(m.is_present("alice"), "a live rejoin was tombstoned");
}

#[test]
fn the_self_nick_survives_a_casemapping_change() {
    // `[` folds to `{` under rfc1459 and not at all under ascii. Re-folding the
    // already-folded self nick left the gateway believing it was called `gw{`
    // under ascii — so it no longer recognized its own NAMES entry (fronting
    // ITSELF as a human) and its own PART no longer dropped the channel.
    let mut m = Membership::new("gw[", CaseMapping::Rfc1459);
    assert_eq!(m.self_nick(), "gw[");
    m.set_casemapping(CaseMapping::Ascii);
    assert_eq!(m.self_nick(), "gw[", "the wire spelling must not change");

    let g = m.self_joined("#a");
    m.names_reply("#a", g, names(&[("gw[", None), ("alice", None)]));
    let e = m.names_end("#a", g);
    assert_eq!(
        e,
        vec![HumanEffect::Register(human("alice"))],
        "the gateway fronted itself as a human: {e:?}"
    );
    assert!(!m.is_present("gw["), "the gateway is not a human");

    // ...and its own PART is still a self-departure, dropping the channel and
    // withdrawing the humans who were only there.
    let e = m.left("#a", "gw[");
    assert_eq!(e, vec![HumanEffect::Withdraw(human("alice"))]);
    assert!(m.joined_channels().is_empty());
}

#[test]
fn a_self_rename_keeps_the_wire_spelling_for_later_refolding() {
    // The same rule after the gateway renames itself: what is stored is the new
    // WIRE nick, so a later mapping change re-derives from it rather than from
    // a folded form.
    let mut m = Membership::new("gw", CaseMapping::Rfc1459);
    m.renamed("gw", "gw]");
    assert_eq!(m.self_nick(), "gw]");
    m.set_casemapping(CaseMapping::Ascii);
    assert_eq!(m.self_nick(), "gw]");
    let g = m.self_joined("#a");
    m.names_reply("#a", g, names(&[("gw]", None)]));
    assert!(
        m.names_end("#a", g).is_empty(),
        "the renamed gateway fronted itself as a human"
    );
}

#[test]
fn a_self_rename_tombstones_the_old_nick_in_every_open_sync() {
    // `names_reply` recognizes the gateway by its CURRENT folded nick alone. A
    // rename mid-sync therefore un-filters the pre-rename spelling: the snapshot
    // line naming `gw` was accepted, committed, and fronted as `human:gw` — the
    // gateway holding its own vacated nick as a present human, which is exactly
    // the fact routing checks before disclosing a private body.
    let mut m = Membership::new("gw", RFC);
    let g = m.self_joined("#a");
    assert!(
        m.renamed("gw", "gw2").is_empty(),
        "the gateway is not a human"
    );
    assert_eq!(m.self_nick(), "gw2");
    // The snapshot was taken while the gateway was still `gw`.
    m.names_reply("#a", g, names(&[("gw", None), ("alice", None)]));
    let e = m.names_end("#a", g);
    assert_eq!(
        e,
        vec![HumanEffect::Register(human("alice"))],
        "the gateway fronted its own pre-rename nick as a human: {e:?}"
    );
    assert!(
        !m.is_present("gw"),
        "a phantom human under the vacated nick"
    );
    assert!(!m.is_present("gw2"), "the gateway is never a human");

    // The tombstone is not a ban: a real human who takes the freed nick arrives
    // by a live JOIN, and that is newer than any line of the snapshot.
    let mut m = Membership::new("gw", RFC);
    let g = m.self_joined("#a");
    m.renamed("gw", "gw2");
    let e = m.joined("#a", "gw", None);
    assert_eq!(e, vec![HumanEffect::Register(human("gw"))]);
    m.names_reply("#a", g, names(&[("gw", None)]));
    m.names_end("#a", g);
    assert!(m.is_present("gw"), "a live JOIN was tombstoned");
}

#[test]
fn a_rename_leaves_tombstones_alone_in_channels_the_renamer_is_not_in() {
    // A NICK is evidence about one user. Clearing the destination nick's
    // tombstone in EVERY syncing channel let a rename in `#b` resurrect the
    // previous holder of that nick in `#a`, fabricating membership in a channel
    // the renaming user never joined.
    let mut m = Membership::new("gw", RFC);
    let ga = m.self_joined("#a");
    m.names_reply("#a", ga, names(&[("bob", None)]));
    assert_eq!(
        m.names_end("#a", ga),
        vec![HumanEffect::Register(human("bob"))]
    );
    let gb = m.self_joined("#b");
    m.names_reply("#b", gb, names(&[("alice", None)]));
    assert_eq!(
        m.names_end("#b", gb),
        vec![HumanEffect::Register(human("alice"))]
    );

    // A fresh `#a` sync opens and bob QUITs while it is open: tombstoned in #a.
    let ga2 = m.self_joined("#a");
    assert_eq!(m.quit("bob"), vec![HumanEffect::Withdraw(human("bob"))]);
    // alice — who is only in #b — takes the nick bob just freed.
    assert_eq!(
        m.renamed("alice", "bob"),
        vec![HumanEffect::Rename {
            from: human("alice"),
            to: human("bob"),
        }]
    );
    // The delayed `#a` snapshot still names the OLD bob.
    m.names_reply("#a", ga2, names(&[("bob", None)]));
    m.names_end("#a", ga2);
    assert!(m.is_present("bob"), "the new bob is present through #b");
    assert_eq!(
        m.channels_of("bob"),
        vec!["#b".to_string()],
        "a stale #a snapshot fabricated membership for the new bob"
    );
}

#[test]
fn a_rename_carries_a_committed_member_into_the_open_snapshot() {
    // The rename moved a LIVE member, so the new identity belongs in the pending
    // snapshot even though the old one was never pending: seeding `pending` only
    // from `pending` tombstoned the old name, discarded the late snapshot entry,
    // and let the commit withdraw a member who never left.
    let mut m = Membership::new("gw", RFC);
    let g1 = m.self_joined("#a");
    m.names_reply("#a", g1, names(&[("alice", None)]));
    assert_eq!(
        m.names_end("#a", g1),
        vec![HumanEffect::Register(human("alice"))]
    );
    // A fresh sync; alice renames before the new snapshot reaches her.
    let g2 = m.self_joined("#a");
    assert_eq!(
        m.renamed("alice", "alice2"),
        vec![HumanEffect::Rename {
            from: human("alice"),
            to: human("alice2"),
        }]
    );
    m.names_reply("#a", g2, names(&[("alice", None)]));
    let e = m.names_end("#a", g2);
    assert!(e.is_empty(), "a member who never left was withdrawn: {e:?}");
    assert!(!m.is_present("alice"), "the pre-rename nick came back");
    assert!(
        m.is_present("alice2"),
        "the renamed member was dropped at the commit"
    );
    assert_eq!(m.display_nick("alice2").as_deref(), Some("alice2"));
}

// ─────────────────────────── Channel reconciler ─────────────────────────────

/// Collect a reconcile's effects into a set-friendly Vec of the joined channels.
fn joins(effects: &[ChannelEffect]) -> Vec<String> {
    effects
        .iter()
        .filter_map(|e| match e {
            ChannelEffect::Join(c) => Some(c.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn desired_channels_are_the_lobby_plus_one_per_agent() {
    let mut r = ChannelReconciler::new("#", "#mu", RFC, 50);
    let effects = r.reconcile(&[PeerId::parse("cc:abc")], 0);
    let mut got = joins(&effects);
    got.sort();
    assert_eq!(got, vec!["#cc-abc".to_string(), "#mu".to_string()]);
    // Already pending: a second reconcile emits nothing.
    assert!(r.reconcile(&[PeerId::parse("cc:abc")], 0).is_empty());
}

#[test]
fn departed_agent_channel_is_parted_but_never_the_lobby() {
    let mut r = ChannelReconciler::new("#", "#mu", RFC, 50);
    r.reconcile(&[PeerId::parse("cc:abc")], 0);
    r.join_confirmed("#mu");
    r.join_confirmed("#cc-abc");
    // The agent is gone from discovery: its channel is parted, the lobby stays.
    let effects = r.reconcile(&[], 1000);
    assert_eq!(effects, vec![ChannelEffect::Part("#cc-abc".to_string())]);
    assert!(r.is_joined("#mu"));
    assert!(!r.is_joined("#cc-abc"));
}

#[test]
fn folded_channel_collisions_are_joined_once() {
    let mut r = ChannelReconciler::new("#", "#mu", RFC, 50);
    // cc:abc → #cc-abc and cc:ABC → #cc-ABC fold to one channel; join it once.
    let effects = r.reconcile(&[PeerId::parse("cc:abc"), PeerId::parse("cc:ABC")], 0);
    assert_eq!(effects.len(), 2, "lobby + one collided channel, not three");
    let joined = joins(&effects);
    assert!(joined.contains(&"#mu".to_string()));
}

#[test]
fn refused_join_backs_off_and_diagnoses_once() {
    let mut r = ChannelReconciler::new("#", "#mu", RFC, 50);
    r.reconcile(&[PeerId::parse("cc:x")], 0);
    // First refusal: diagnosed once, retry scheduled 30 s out.
    assert!(r.join_refused("#cc-x", 0), "first refusal is diagnosed");
    // Inside the window: no re-join (the lobby is still pending, so no effect).
    let effects = r.reconcile(&[PeerId::parse("cc:x")], 100);
    assert!(
        !joins(&effects).contains(&"#cc-x".to_string()),
        "must not retry inside the 30 s window"
    );
    // At the window boundary: retried.
    let effects = r.reconcile(&[PeerId::parse("cc:x")], 30_000);
    assert!(joins(&effects).contains(&"#cc-x".to_string()));

    // Second refusal: NOT re-diagnosed, and the window doubles to 60 s.
    assert!(!r.join_refused("#cc-x", 30_000), "not diagnosed twice");
    assert!(
        !joins(&r.reconcile(&[PeerId::parse("cc:x")], 89_999)).contains(&"#cc-x".to_string()),
        "still inside the doubled window"
    );
    assert!(
        joins(&r.reconcile(&[PeerId::parse("cc:x")], 90_000)).contains(&"#cc-x".to_string()),
        "retried after the doubled window"
    );
}

#[test]
fn backoff_delay_is_capped_at_ten_minutes() {
    let mut r = ChannelReconciler::new("#", "#mu", RFC, 50);
    r.reconcile(&[PeerId::parse("cc:x")], 0);
    // Refuse repeatedly at the same instant; the schedule doubles 30s,60s,…
    // but never exceeds the 10-minute cap. After enough doublings the next
    // retry is exactly cap-ms out.
    let mut t = 0u64;
    for _ in 0..10 {
        r.join_refused("#cc-x", t);
        t += 1; // advance a hair so each refusal is a fresh instant
    }
    // A reconcile well before cap does not retry; one at cap does.
    let base = t;
    assert!(
        !joins(&r.reconcile(&[PeerId::parse("cc:x")], base + 599_000))
            .contains(&"#cc-x".to_string())
    );
    assert!(
        joins(&r.reconcile(&[PeerId::parse("cc:x")], base + 600_000))
            .contains(&"#cc-x".to_string())
    );
}

#[test]
fn reset_forgets_all_reconciler_state() {
    let mut r = ChannelReconciler::new("#", "#mu", RFC, 50);
    r.reconcile(&[PeerId::parse("cc:x")], 0);
    r.join_confirmed("#mu");
    r.join_confirmed("#cc-x");
    r.reset();
    assert!(!r.is_joined("#mu"));
    // After a reset the next reconcile rebuilds from scratch.
    let got = joins(&r.reconcile(&[PeerId::parse("cc:x")], 0));
    assert_eq!(got.len(), 2);
}

#[test]
fn forgetting_a_channel_clears_its_backoff_and_diagnostic_marker() {
    // A refused JOIN leaves a backoff window and a "already diagnosed" marker.
    // If the channel is then dropped — the agent went away, or the gateway was
    // parted/kicked — those are state about an attempt that no longer exists.
    // Leaving them behind delayed the next legitimate JOIN by up to ten minutes
    // and swallowed the diagnostic for a fresh refusal.
    let mut r = ChannelReconciler::new("#", "#mu", RFC, 50);
    let peers = [PeerId::parse("cc:x")];
    r.reconcile(&peers, 0);
    assert!(r.join_refused("#cc-x", 0), "first refusal is diagnosed");
    // The gateway leaves / loses the channel.
    r.dropped("#cc-x");
    // The very next reconcile re-joins it: no leftover 30 s window.
    assert!(
        joins(&r.reconcile(&peers, 1)).contains(&"#cc-x".to_string()),
        "a stale backoff blocked the rejoin"
    );
    // And a refusal after the drop is a FIRST refusal again, so it is reported.
    assert!(
        r.join_refused("#cc-x", 1),
        "a stale diagnosed marker swallowed the new diagnostic"
    );

    // The same holds when the channel simply stops being desired and is parted.
    let mut r = ChannelReconciler::new("#", "#mu", RFC, 50);
    r.reconcile(&peers, 0);
    r.join_confirmed("#cc-x");
    r.join_refused("#cc-x", 0);
    let parted = r.reconcile(&[], 1);
    assert!(
        parted.contains(&ChannelEffect::Part("#cc-x".to_string())),
        "{parted:?}"
    );
    assert!(joins(&r.reconcile(&peers, 2)).contains(&"#cc-x".to_string()));

    // A backoff that is still live is NOT cleared by an unrelated reconcile:
    // the window it schedules must still be honoured.
    let mut r = ChannelReconciler::new("#", "#mu", RFC, 50);
    r.reconcile(&peers, 0);
    r.join_refused("#cc-x", 0);
    assert!(
        !joins(&r.reconcile(&peers, 100)).contains(&"#cc-x".to_string()),
        "the backoff window was lost"
    );
}

#[test]
fn a_refused_channel_that_leaves_discovery_is_forgotten() {
    // `join_refused` takes a never-confirmed channel out of `pending` and keeps
    // it only in `backoff`/`diagnosed`, where the joined∪pending stale scan could
    // not see it. When its peer then disappeared from discovery nothing forgot
    // it — `dropped` never fires for a channel the gateway was never in — so a
    // rediscovery inside the window was suppressed for up to ten minutes and the
    // fresh refusal inherited the old "already diagnosed" marker.
    let mut r = ChannelReconciler::new("#", "#mu", RFC, 50);
    let peers = [PeerId::parse("cc:x")];
    assert!(joins(&r.reconcile(&peers, 0)).contains(&"#cc-x".to_string()));
    assert!(r.join_refused("#cc-x", 0), "first refusal is diagnosed");
    // The peer disappears. There is nothing to PART (the channel was never
    // joined), but its attempt state is now about a channel nobody wants.
    let effects = r.reconcile(&[], 1);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, ChannelEffect::Part(c) if c.as_str() == "#cc-x")),
        "parted a channel that was never joined: {effects:?}"
    );
    // Rediscovery inside the old 30 s window joins immediately…
    assert!(
        joins(&r.reconcile(&peers, 2)).contains(&"#cc-x".to_string()),
        "a leaked backoff suppressed the JOIN after rediscovery"
    );
    // …and a refusal of that fresh attempt is diagnosed again.
    assert!(
        r.join_refused("#cc-x", 2),
        "a leaked diagnosed marker swallowed the new diagnostic"
    );
}
