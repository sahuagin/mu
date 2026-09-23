//! Offline membership + channel-lifecycle tests. No socket:
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
fn a_casemapping_change_keeps_the_channels_the_gateway_is_still_in() {
    // The gateway does not leave a channel because the server changed how it
    // folds case. Forgetting the channel set here is unrecoverable: the bridge
    // can only re-open a NAMES generation for a channel the view still holds,
    // and a JOIN for one the client already occupies yields no echo and no
    // NAMES — so the roster could never be rebuilt and the humans in it would
    // stay un-fronted for the life of the connection.
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#a");
    m.names_reply("#a", g, names(&[("alice", None)]));
    m.names_end("#a", g);
    m.self_joined("#b");
    m.joined("#b", "bob", None);

    let e = m.set_casemapping(CaseMapping::Ascii);
    assert!(e.is_empty(), "nothing moved, so nothing to report: {e:?}");
    let mut joined = m.joined_channels();
    joined.sort();
    assert_eq!(joined, vec!["#a".to_string(), "#b".to_string()]);
    assert!(m.is_present("alice") && m.is_present("bob"));
    assert_eq!(m.casemapping(), CaseMapping::Ascii);
    // The same mapping is a no-op.
    assert!(m.set_casemapping(CaseMapping::Ascii).is_empty());
}

#[test]
fn a_casemapping_change_that_moves_a_human_is_a_rename() {
    // `alice[]` folds to `alice{}` under rfc1459 and to `alice[]` under ascii:
    // two different `human:` peer ids, so the endpoint the gateway fronts has to
    // move. That is the same thing a NICK does, and it is reported the same way
    // so the executor's release-then-front ordering applies unchanged.
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#a");
    m.names_reply("#a", g, names(&[("alice[]", None), ("bob", None)]));
    m.names_end("#a", g);
    assert!(m.is_present("alice{}"));

    let e = m.set_casemapping(CaseMapping::Ascii);
    assert_eq!(
        e,
        vec![HumanEffect::Rename {
            from: human("alice{}"),
            to: human("alice[]"),
        }],
        "bob folds the same way under both rules and must not move"
    );
    assert!(m.is_present("alice[]"));
    assert!(m.is_present("bob"));
    // The display spelling is still the wire's, so a private PRIVMSG addresses
    // the person the server knows.
    assert_eq!(m.display_nick("alice[]").as_deref(), Some("alice[]"));
}

#[test]
fn a_casemapping_change_that_collides_two_humans_withdraws_the_loser() {
    // Under ascii `a{` and `a[` are two people; under rfc1459 they fold to one
    // key. One identity survives, and the other is no longer addressable — so it
    // is withdrawn rather than left fronted with nobody behind it.
    let mut m = Membership::new("mu-gw", CaseMapping::Ascii);
    let g = m.self_joined("#a");
    m.names_reply("#a", g, names(&[("a{", None), ("a[", None)]));
    m.names_end("#a", g);
    assert!(m.is_present("a{") && m.is_present("a["));

    // `a{` is already its own fold under rfc1459, so it is the identity that
    // stays; `a[` folds onto it and is withdrawn rather than renamed onto
    // someone who never moved.
    let e = m.set_casemapping(RFC);
    assert_eq!(e, vec![HumanEffect::Withdraw(human("a["))], "{e:?}");
    assert!(
        m.is_present("a{"),
        "the surviving folded identity is present"
    );
    assert_eq!(m.channels_of("a{").len(), 1);
}

#[test]
fn a_casemapping_change_leaves_the_roster_re_syncable() {
    // What the bridge does next: re-open a generation per kept channel and let a
    // fresh NAMES burst commit over the re-folded roster. The kept roster is a
    // starting point, not an authority — a member who left during the change is
    // withdrawn by the commit.
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#a");
    m.names_reply("#a", g, names(&[("alice", None), ("carol", None)]));
    m.names_end("#a", g);
    m.set_casemapping(CaseMapping::Ascii);

    let g2 = m.self_joined("#a");
    assert_ne!(g2, g, "a fresh generation, so a stale burst cannot commit");
    m.names_reply("#a", g2, names(&[("alice", None), ("dave", None)]));
    let e = m.names_end("#a", g2);
    assert!(e.contains(&HumanEffect::Register(human("dave"))));
    assert!(e.contains(&HumanEffect::Withdraw(human("carol"))));
    assert!(m.is_present("alice") && m.is_present("dave"));
    assert!(!m.is_present("carol"));
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

// ──────────────────────── Gateway-owned nick set (puppets) ──────────────────
//
// Design: specs/plans/mu-irc-gateway-v1-puppets.md — humans are present nicks
// minus the set of nicks the gateway itself holds (its own + every puppet).
// Increment 2a: the set is fed by the bridge from the pool; until 2b nothing
// feeds it, and an empty set is exactly v0.

// ─────────────────────── Account attribution (WHOX / ACCOUNT) ───────────────
//
// NAMES decides who is THERE; attribution decides who they ARE. The two are
// deliberately separate state — see `Membership::set_account`.

#[test]
fn set_account_attributes_a_committed_member() {
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#mu");
    // NAMES carries no account: the roster commits unattributed, which is what
    // the WHOX pass then fills in.
    m.names_reply("#mu", g, names(&[("alice", None)]));
    m.names_end("#mu", g);
    assert_eq!(m.account_of("alice"), None);

    m.set_account("alice", Some("alice-acct".to_string()));
    assert_eq!(m.account_of("alice"), Some("alice-acct"));
    // Folded, like every other identity lookup here.
    assert_eq!(m.account_of("ALICE"), Some("alice-acct"));
}

#[test]
fn set_account_reaches_a_member_still_inside_an_open_sync() {
    // A WHOX reply can beat ENDOFNAMES. The attribution must land on the
    // pending side too, or committing the roster would discard it.
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("alice", None)]));
    m.set_account("alice", Some("alice-acct".to_string()));
    m.names_end("#mu", g);
    assert_eq!(
        m.account_of("alice"),
        Some("alice-acct"),
        "an attribution made mid-sync survives the commit"
    );
}

#[test]
fn set_account_attributes_across_every_channel_the_nick_is_in() {
    let mut m = Membership::new("mu-gw", RFC);
    for ch in ["#a", "#b"] {
        let g = m.self_joined(ch);
        m.names_reply(ch, g, names(&[("alice", None)]));
        m.names_end(ch, g);
    }
    m.set_account("alice", Some("alice-acct".to_string()));
    // Whichever roster is consulted, the answer is the same one.
    assert_eq!(m.account_of("alice"), Some("alice-acct"));
    m.left("#a", "alice");
    assert_eq!(
        m.account_of("alice"),
        Some("alice-acct"),
        "leaving one channel does not de-attribute the other"
    );
}

#[test]
fn a_logout_clears_the_attribution_without_removing_the_member() {
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("alice", Some("alice-acct"))]));
    m.names_end("#mu", g);
    assert_eq!(m.account_of("alice"), Some("alice-acct"));
    // `ACCOUNT *` — logged out, still in the channel.
    m.set_account("alice", None);
    assert_eq!(m.account_of("alice"), None);
    assert!(m.is_present("alice"), "a logout is not a departure");
}

#[test]
fn attributing_an_unknown_nick_is_a_no_op() {
    // A WHOX reply that races a departure names someone who is no longer in
    // the roster. It must not resurrect them.
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("alice", None)]));
    m.names_end("#mu", g);
    m.set_account("ghost", Some("ghost-acct".to_string()));
    assert_eq!(m.account_of("ghost"), None);
    assert!(!m.is_present("ghost"));
}

// ─────────────── Attribution is per nick, not per roster entry ──────────────
//
// Regressions for the two defects the review panel found on the first pass:
// attribution stored on each channel's Member could be clobbered by a later
// NAMES line, and could differ between two channels — so the answer depended
// on which roster was consulted, and on HashMap iteration order.

#[test]
fn an_attribution_survives_a_later_names_line_for_the_same_nick() {
    // The order the first implementation got wrong: attribute FIRST, then let
    // the NAMES line for that nick arrive. NAMES carries no account, and its
    // silence must not be read as "this nick has no account".
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("alice", None)]));
    m.names_end("#mu", g);
    m.set_account("alice", Some("alice-acct".to_string()));

    // A resync: a fresh snapshot whose line for alice carries no account.
    let g2 = m.self_joined("#mu");
    m.names_reply("#mu", g2, names(&[("alice", None)]));
    m.names_end("#mu", g2);
    assert_eq!(
        m.account_of("alice"),
        Some("alice-acct"),
        "a NAMES line that says nothing about the account must not erase it"
    );
}

#[test]
fn an_attribution_is_the_same_from_every_channel() {
    // Learned in one channel, asked from another. The first implementation
    // stored it on the joined channel's Member only, so the answer depended on
    // which roster `account_of` happened to reach first.
    let mut m = Membership::new("mu-gw", RFC);
    let ga = m.self_joined("#a");
    m.names_reply("#a", ga, names(&[("alice", None)]));
    m.names_end("#a", ga);
    let gb = m.self_joined("#b");
    m.names_end("#b", gb);

    // alice joins #b with an extended-join account; #a's roster predates it.
    m.joined("#b", "alice", Some("alice-acct".to_string()));
    assert_eq!(m.account_of("alice"), Some("alice-acct"));
    // Leaving the channel she was attributed in does not lose the answer.
    m.left("#b", "alice");
    assert_eq!(
        m.account_of("alice"),
        Some("alice-acct"),
        "the attribution is the nick's, not one roster entry's"
    );
}

#[test]
fn an_attribution_follows_a_rename_and_frees_the_old_nick() {
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("alice", None)]));
    m.names_end("#mu", g);
    m.set_account("alice", Some("alice-acct".to_string()));
    m.renamed("alice", "alice2");
    assert_eq!(m.account_of("alice2"), Some("alice-acct"));
    assert_eq!(
        m.account_of("alice"),
        None,
        "the vacated spelling carries no attribution"
    );
}

#[test]
fn a_departed_nick_does_not_leave_its_account_to_the_next_holder() {
    // Nicks are reusable. A surviving entry would answer the departed user's
    // account for whoever takes the name next.
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("alice", None)]));
    m.names_end("#mu", g);
    m.set_account("alice", Some("alice-acct".to_string()));
    m.quit("alice");
    assert_eq!(m.account_of("alice"), None);

    // A stranger takes the freed name, unauthenticated.
    m.joined("#mu", "alice", None);
    assert_eq!(
        m.account_of("alice"),
        None,
        "the new holder inherits nothing from the old one"
    );
}

#[test]
fn a_who_reply_that_races_a_departure_records_nothing() {
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("alice", None)]));
    m.names_end("#mu", g);
    m.set_account("ghost", Some("ghost-acct".to_string()));
    assert_eq!(m.account_of("ghost"), None);
    assert!(!m.is_present("ghost"));
}

#[test]
fn a_logout_clears_the_attribution_but_a_silent_join_does_not() {
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("alice", None)]));
    m.names_end("#mu", g);
    m.set_account("alice", Some("alice-acct".to_string()));
    // A JOIN elsewhere with no account parameter says nothing about it.
    let g2 = m.self_joined("#b");
    m.names_end("#b", g2);
    m.joined("#b", "alice", None);
    assert_eq!(m.account_of("alice"), Some("alice-acct"));
    // `ACCOUNT *` is an explicit answer, and does clear it.
    m.set_account("alice", None);
    assert_eq!(m.account_of("alice"), None);
    assert!(m.is_present("alice"), "a logout is not a departure");
}

// ───────────── Attribution lifetime on the mass-removal paths ───────────────
//
// Board run 2 found `prune_account` wired into `left` and `quit` only, while
// three other paths can remove a nick's last roster entry. After the gateway
// stops watching a channel it hears no QUIT for the nicks that were in it, so
// nothing later prunes them — the stale entry is then inherited by the next
// holder of the name.

#[test]
fn leaving_a_channel_does_not_strand_the_attribution() {
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("alice", None)]));
    m.names_end("#mu", g);
    m.set_account("alice", Some("alice-acct".to_string()));
    // The GATEWAY parts the only channel it shared with alice. No QUIT for her
    // will ever arrive, so this is the last chance to prune.
    m.left("#mu", "mu-gw");
    assert_eq!(m.account_of("alice"), None);
}

#[test]
fn a_resync_that_drops_a_member_does_not_strand_the_attribution() {
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("alice", None)]));
    m.names_end("#mu", g);
    m.set_account("alice", Some("alice-acct".to_string()));
    // A fresh snapshot no longer lists her.
    let g2 = m.self_joined("#mu");
    m.names_reply("#mu", g2, names(&[("bob", None)]));
    m.names_end("#mu", g2);
    assert_eq!(m.account_of("alice"), None);
    // …and the next holder of the freed name inherits nothing.
    m.joined("#mu", "alice", None);
    assert_eq!(m.account_of("alice"), None);
}

#[test]
fn an_attribution_survives_while_another_channel_still_holds_the_nick() {
    // The prune is conditional, not eager: leaving one shared channel must not
    // forget an account the gateway can still see in another.
    let mut m = Membership::new("mu-gw", RFC);
    for ch in ["#a", "#b"] {
        let g = m.self_joined(ch);
        m.names_reply(ch, g, names(&[("alice", None)]));
        m.names_end(ch, g);
    }
    m.set_account("alice", Some("alice-acct".to_string()));
    m.left("#a", "mu-gw");
    assert_eq!(
        m.account_of("alice"),
        Some("alice-acct"),
        "still in #b, so the attribution stands"
    );
    m.left("#b", "mu-gw");
    assert_eq!(m.account_of("alice"), None, "last channel gone: pruned");
}

#[test]
fn an_explicit_extended_join_logout_clears_the_attribution() {
    // `JOIN #c * :realname` is the server ANSWERING "not logged in". Without
    // `account-notify` it is the only logout signal on the wire, so it must
    // clear — unlike a JOIN that simply carries no account information.
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("alice", None)]));
    m.names_end("#mu", g);
    m.set_account("alice", Some("alice-acct".to_string()));
    // What session.rs does for `JoinAccount::LoggedOut`: the add-only join,
    // then the explicit clear.
    m.joined("#mu", "alice", None);
    m.set_account("alice", None);
    assert_eq!(m.account_of("alice"), None);
    assert!(m.is_present("alice"), "a logout is not a departure");
}

// ───────── Attribution lifetime is the roster entry's, by construction ──────
//
// Board run 3 found the previous mechanism — a per-nick map pruned by hand —
// still leaking: `prune_account` walked `ch.members`, but a nick attributed
// while it existed only in an open sync's `pending` was stranded when the
// channel was dropped mid-sync. These cover that case and its siblings. They
// pass because the attribution now lives ON the roster entry, so it dies with
// it; there is no prune to forget.

#[test]
fn an_attribution_made_before_endofnames_dies_with_a_dropped_sync() {
    // The exact run-3 case: attributed while known ONLY through pending, then
    // the gateway leaves before the roster ever commits.
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("alice", None)]));
    m.set_account("alice", Some("alice-acct".to_string()));
    assert_eq!(m.account_of("alice"), Some("alice-acct"));
    // Gateway PARTs before ENDOFNAMES. The channel and its open sync go.
    m.left("#mu", "mu-gw");
    assert_eq!(m.account_of("alice"), None);
    // A later holder of the reused nick inherits nothing.
    let g2 = m.self_joined("#other");
    m.names_reply("#other", g2, names(&[("alice", None)]));
    m.names_end("#other", g2);
    assert_eq!(m.account_of("alice"), None);
}

#[test]
fn a_member_joining_a_second_channel_inherits_the_known_account() {
    // The copies must not disagree: a fresh roster entry seeds from what is
    // already attributed, so the new one cannot shadow the answer.
    let mut m = Membership::new("mu-gw", RFC);
    let ga = m.self_joined("#a");
    m.names_reply("#a", ga, names(&[("alice", None)]));
    m.names_end("#a", ga);
    m.set_account("alice", Some("alice-acct".to_string()));

    let gb = m.self_joined("#b");
    m.names_end("#b", gb);
    m.joined("#b", "alice", None);
    assert_eq!(m.account_of("alice"), Some("alice-acct"));
    // …and leaving the channel it was first learned in keeps it.
    m.left("#a", "alice");
    assert_eq!(m.account_of("alice"), Some("alice-acct"));
}

#[test]
fn a_names_line_for_an_attributed_nick_does_not_blank_it() {
    // A NAMES entry carries no account. The entry it creates inherits, rather
    // than starting blank and losing the answer at commit.
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("alice", None)]));
    m.names_end("#mu", g);
    m.set_account("alice", Some("alice-acct".to_string()));
    let g2 = m.self_joined("#mu");
    m.names_reply("#mu", g2, names(&[("alice", None)]));
    m.names_end("#mu", g2);
    assert_eq!(m.account_of("alice"), Some("alice-acct"));
}

#[test]
fn a_rename_carries_the_attribution_with_no_re_keying() {
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("alice", None)]));
    m.names_end("#mu", g);
    m.set_account("alice", Some("alice-acct".to_string()));
    m.renamed("alice", "alice2");
    assert_eq!(m.account_of("alice2"), Some("alice-acct"));
    assert_eq!(m.account_of("alice"), None);
}

#[test]
fn a_first_join_carrying_an_account_is_attributed() {
    // The primary extended-join path: a nick the view has never seen, joining
    // with an account. Board run 4 found the account was silently discarded
    // here, because the fan-out write only reaches copies that already exist
    // and the new member was inserted after it.
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#mu");
    m.names_end("#mu", g);
    m.joined("#mu", "alice", Some("alice-acct".to_string()));
    assert_eq!(
        m.account_of("alice"),
        Some("alice-acct"),
        "an extended-join account on a first JOIN must not be dropped"
    );
}

#[test]
fn a_join_reporting_a_changed_account_updates_every_copy() {
    // The same ordering could leave the new channel's copy holding the stale
    // value while the others took the new one.
    let mut m = Membership::new("mu-gw", RFC);
    let ga = m.self_joined("#a");
    m.names_reply("#a", ga, names(&[("alice", None)]));
    m.names_end("#a", ga);
    m.set_account("alice", Some("old-acct".to_string()));

    let gb = m.self_joined("#b");
    m.names_end("#b", gb);
    m.joined("#b", "alice", Some("new-acct".to_string()));
    assert_eq!(m.account_of("alice"), Some("new-acct"));
    // …and the copy in #a is the new value too, not the old one.
    m.left("#b", "alice");
    assert_eq!(
        m.account_of("alice"),
        Some("new-acct"),
        "every copy took the new account, so none can answer the old one"
    );
}
// ───────────────────── Ours-ness is the account, not the spelling ───────────
//
// The rename window modelled who might hold each spelling across the gap
// between a rename and learning of it. The server answers that directly, and
// unforgeably, with the services account. These are the cases the window's
// `retiring`/`gone`/`deferred` sets existed for; each is now one rule.

#[test]
fn a_member_on_one_of_our_accounts_is_never_fronted() {
    let mut m = Membership::new("mu-gw", RFC);
    m.set_owned_accounts(["cc-1"]);
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("claude-pr777", None), ("alice", None)]));
    let effects = m.names_end("#mu", g);
    // Unattributed and not in the fallback nick set: a human, for now.
    assert!(effects.contains(&HumanEffect::Register(human("claude-pr777"))));

    // The WHOX pass answers, and the correction is made here.
    let effects = m.set_account("claude-pr777", Some("cc-1".to_string()));
    assert_eq!(
        effects,
        vec![HumanEffect::Withdraw(human("claude-pr777"))],
        "R1: a puppet fronted as a human is withdrawn the moment the server says so"
    );
    assert!(!m.is_present("claude-pr777"));
    assert!(m.is_present("alice"), "the real human is untouched");
}

#[test]
fn the_nick_set_is_only_a_fallback_and_the_account_overrides_it() {
    // The hazard the whole window existed for: a human holding a name the
    // pool still lists. The account settles it in one round trip.
    let mut m = Membership::new("mu-gw", RFC);
    m.set_owned_accounts(["cc-1"]);
    m.set_owned_nicks(["cc-7"]);
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("cc-7", None)]));
    let effects = m.names_end("#mu", g);
    assert!(
        effects.is_empty(),
        "unattributed, and the pool lists the name: treated as ours, conservatively"
    );
    assert!(!m.is_present("cc-7"));

    // WHOX: that nick is a human who took the name.
    let effects = m.set_account("cc-7", Some("mallory".to_string()));
    assert_eq!(
        effects,
        vec![HumanEffect::Register(human("cc-7"))],
        "the server's answer overrides the pool's stale nick set"
    );
    assert!(m.is_present("cc-7"));
}

#[test]
fn a_puppet_that_renames_is_still_ours_under_the_new_name() {
    // The case that took PR #679 to twenty board runs. The account travels
    // with the member, so a NICK settles nothing and needs to settle nothing.
    let mut m = Membership::new("mu-gw", RFC);
    m.set_owned_accounts(["cc-1"]);
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("claude-pr777", Some("cc-1"))]));
    let effects = m.names_end("#mu", g);
    assert!(effects.is_empty(), "attributed on arrival: never fronted");

    let effects = m.renamed("claude-pr777", "claude-pr888");
    assert!(effects.is_empty(), "a puppet's rename moves no human");
    assert!(!m.is_present("claude-pr888"));
    assert_eq!(m.account_of("claude-pr888"), Some("cc-1"));
    // …and the vacated spelling is nobody's: a human taking it is a human.
    let effects = m.joined("#mu", "claude-pr777", Some("mallory".to_string()));
    assert_eq!(effects, vec![HumanEffect::Register(human("claude-pr777"))]);
}

#[test]
fn returning_a_lease_makes_the_nick_a_humans_again() {
    // An account leaves the set when its CONNECTION is gone, at which point
    // the server has dropped it too — so there is no interval to model.
    let mut m = Membership::new("mu-gw", RFC);
    m.set_owned_accounts(["cc-1"]);
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("claude-pr777", Some("cc-1"))]));
    assert!(m.names_end("#mu", g).is_empty());

    // The lease returns while someone is still under that name: whoever it is
    // now, it is not ours.
    let effects = m.set_owned_accounts::<[&str; 0], &str>([]);
    assert_eq!(effects, vec![HumanEffect::Register(human("claude-pr777"))]);
    assert!(m.is_present("claude-pr777"));
}

#[test]
fn a_logged_out_puppet_nick_falls_back_to_the_pools_nick_set() {
    // `ACCOUNT *`: the attribution is gone, so the fallback decides again.
    let mut m = Membership::new("mu-gw", RFC);
    m.set_owned_accounts(["cc-1"]);
    m.set_owned_nicks(["claude-pr777"]);
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("claude-pr777", Some("cc-1"))]));
    assert!(m.names_end("#mu", g).is_empty());
    let effects = m.set_account("claude-pr777", None);
    assert!(
        effects.is_empty(),
        "still ours by the fallback: no flap when the attribution clears"
    );
    assert!(!m.is_present("claude-pr777"));
}

#[test]
fn both_owned_sets_re_derive_across_a_casemapping_change() {
    let mut m = Membership::new("mu-gw", RFC);
    // `cc[` and `cc{` fold together under rfc1459 but not under ascii.
    m.set_owned_accounts(["CC-1"]);
    m.set_owned_nicks(["cc[", "cc{"]);
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("claude-x", Some("cc-1"))]));
    assert!(
        m.names_end("#mu", g).is_empty(),
        "attributed, so not fronted"
    );

    m.set_casemapping(CaseMapping::Ascii);
    // The account set re-folds from account NAMES, which no rename moves, so
    // it still answers for the member.
    assert!(
        m.is_owned("claude-x"),
        "the account still answers for the member after the fold changed"
    );
    // The fallback set re-derives from the WIRE spellings it was given. The
    // two collided under rfc1459 and were already one entry then, so the fold
    // change cannot un-merge them — the survivor is the earlier spelling, the
    // same one the pool's nick table keeps.
    assert_eq!(m.owned_nicks(), vec!["cc["]);
}

#[test]
fn reset_forgets_both_owned_sets() {
    let mut m = Membership::new("mu-gw", RFC);
    m.set_owned_accounts(["cc-1"]);
    m.set_owned_nicks(["cc-7"]);
    m.reset();
    assert!(m.owned_nicks().is_empty());
    assert!(!m.is_owned("cc-7"));
}

#[test]
fn attribution_arriving_mid_sync_decides_the_commit() {
    // A WHOX reply that beats ENDOFNAMES must be what the commit acts on,
    // not the unattributed snapshot line.
    let mut m = Membership::new("mu-gw", RFC);
    m.set_owned_accounts(["cc-1"]);
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("claude-pr777", None)]));
    m.set_account("claude-pr777", Some("cc-1".to_string()));
    let effects = m.names_end("#mu", g);
    assert!(
        effects.is_empty(),
        "the roster commits with the attribution already applied: never fronted at all"
    );
    assert!(!m.is_present("claude-pr777"));
}

// ─────────── Ours-ness changes are global, and survive a re-fold ────────────
//
// Board run 6. Attribution fans out to every channel, so the VERDICT it
// implies is global too — and a `CASEMAPPING` change must not lose it.

#[test]
fn a_join_that_makes_a_nick_ours_withdraws_it_from_every_channel() {
    let mut m = Membership::new("mu-gw", RFC);
    m.set_owned_accounts(["cc-1"]);
    let ga = m.self_joined("#a");
    m.names_reply("#a", ga, names(&[("claude-x", None)]));
    assert_eq!(
        m.names_end("#a", ga),
        vec![HumanEffect::Register(human("claude-x"))],
        "unattributed and not pool-listed: a human, for now"
    );
    let gb = m.self_joined("#b");
    m.names_end("#b", gb);
    // It joins #b and the server names the account: it is ours, everywhere.
    let effects = m.joined("#b", "claude-x", Some("cc-1".to_string()));
    assert!(
        effects.contains(&HumanEffect::Withdraw(human("claude-x"))),
        "the JOIN's account makes it ours in #a too, so it must be withdrawn"
    );
    assert!(!m.is_present("claude-x"));
}

#[test]
fn a_join_that_makes_a_nick_human_fronts_it_for_every_channel_it_is_in() {
    // The reverse: registering with only the joined channel in its presence
    // set would let leaving that one channel withdraw a member still in another.
    let mut m = Membership::new("mu-gw", RFC);
    m.set_owned_nicks(["cc-abc"]);
    let ga = m.self_joined("#a");
    m.names_reply("#a", ga, names(&[("cc-abc", None)]));
    assert!(m.names_end("#a", ga).is_empty(), "pool-listed: not fronted");
    let gb = m.self_joined("#b");
    m.names_end("#b", gb);
    // WHOX/extended-join says it is a human's account after all.
    m.joined("#b", "cc-abc", Some("mallory".to_string()));
    assert!(m.is_present("cc-abc"));
    // Leaving #b must not withdraw them — they are still in #a.
    let effects = m.left("#b", "cc-abc");
    assert!(
        effects.is_empty(),
        "still in #a, so leaving #b withdraws nobody"
    );
    assert!(m.is_present("cc-abc"));
}

#[test]
fn an_account_owned_puppet_is_not_fronted_by_a_casemapping_rebuild() {
    // The rebuild takes `self.channels` before re-keying, so asking
    // `is_puppet` mid-rebuild would see no attributions and answer from the
    // fallback nick set alone — fronting an account-owned puppet as a human.
    let mut m = Membership::new("mu-gw", RFC);
    m.set_owned_accounts(["cc-1"]);
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("claude-x", Some("cc-1"))]));
    assert!(
        m.names_end("#mu", g).is_empty(),
        "attributed: never fronted"
    );

    let effects = m.set_casemapping(CaseMapping::Ascii);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, HumanEffect::Register(p) if p == &human("claude-x"))),
        "R1: a re-fold must not front one of our own puppets"
    );
    assert!(!m.is_present("claude-x"));
    assert!(m.is_owned("claude-x"));
}

#[test]
fn an_owned_account_survives_a_casemapping_change() {
    // `owned_accounts` keeps the original spelling: re-folding an
    // already-folded value is lossy, and would stop matching the roster.
    let mut m = Membership::new("mu-gw", RFC);
    m.set_owned_accounts(["cc["]);
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("claude-x", Some("cc["))]));
    assert!(m.names_end("#mu", g).is_empty());
    assert!(m.is_owned("claude-x"));

    m.set_casemapping(CaseMapping::Ascii);
    assert!(
        m.is_owned("claude-x"),
        "the account still matches after the fold rule changed"
    );
    assert!(!m.is_present("claude-x"));
}
