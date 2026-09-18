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

#[test]
fn puppet_joins_and_names_entries_are_never_humans() {
    let mut m = Membership::new("mu-gw", RFC);
    assert!(m.set_owned_nicks(["cc-abc", "mu-d-session-1"]).is_empty());
    assert_eq!(m.owned_nicks(), vec!["cc-abc", "mu-d-session-1"]);
    let g = m.self_joined("#mu");
    // A NAMES burst listing puppets beside a human fronts the human only.
    m.names_reply(
        "#mu",
        g,
        names(&[
            ("alice", None),
            ("cc-abc", None),
            ("@mu-d-session-1", None),
            ("mu-gw", None),
        ]),
    );
    let effects = m.names_end("#mu", g);
    assert_eq!(effects, vec![HumanEffect::Register(human("alice"))]);
    assert!(!m.is_present("cc-abc"));
    assert!(
        !m.is_present("MU-D-SESSION-1"),
        "owned nicks fold like every other name"
    );
    assert_eq!(m.present_humans(), vec![human("alice")]);
    // A live puppet JOIN produces no Register and no presence.
    assert!(m.joined("#mu", "CC-ABC", None).is_empty());
    assert!(!m.is_present("cc-abc"));
    assert_eq!(m.present_humans(), vec![human("alice")]);
    // A puppet PART or QUIT is not a human departure and does not drop the channel.
    assert!(m.left("#mu", "cc-abc").is_empty());
    assert!(m.quit("mu-d-session-1").is_empty());
    assert_eq!(m.joined_channels(), vec!["#mu".to_string()]);
    assert!(
        m.is_present("alice"),
        "the human is untouched by puppet churn"
    );
    assert!(m.is_owned("cc-abc") && m.is_owned("mu-gw") && !m.is_owned("alice"));
}

#[test]
fn an_owned_set_arriving_after_a_live_join_evicts_the_puppet_from_humans() {
    // The window the bridge must not open (2b wires the set before the first
    // puppet connects) — but if it ever does, membership corrects rather than
    // leaves a puppet fronted as `human:cc-abc`.
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("alice", None), ("cc-abc", None)]));
    let effects = m.names_end("#mu", g);
    assert!(
        effects.contains(&HumanEffect::Register(human("cc-abc"))),
        "precondition: fronted"
    );
    assert!(m.is_present("cc-abc"));
    let effects = m.set_owned_nicks(["cc-abc"]);
    assert_eq!(effects, vec![HumanEffect::Withdraw(human("cc-abc"))]);
    assert!(!m.is_present("cc-abc"));
    assert_eq!(m.present_humans(), vec![human("alice")]);
    // And it stays out of a NAMES that still lists it.
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("alice", None), ("cc-abc", None)]));
    assert!(m.names_end("#mu", g).is_empty());
    assert!(!m.is_present("cc-abc"));
}

#[test]
fn a_released_nick_is_retiring_until_its_quit_is_observed_then_a_human_may_take_it() {
    // The pool releases a nick when it QUEUES the puppet's QUIT; the puppet is
    // still on the server until that QUIT lands (up to the grace). Release is
    // therefore driven off the OBSERVED departure: until this connection sees
    // the QUIT, the nick is retiring — never a human, even in a NAMES snapshot
    // that commits in the gap — and after it, the next JOIN is a human's.
    let mut m = Membership::new("mu-gw", RFC);
    m.set_owned_nicks(["cc-abc"]);
    m.self_joined("#mu");
    assert!(m.joined("#mu", "cc-abc", None).is_empty());
    // The pool gave the nick up (peer left the mesh; QUIT queued).
    assert!(m.set_owned_nicks(Vec::<&str>::new()).is_empty());
    assert_eq!(m.owned_nicks(), Vec::<&str>::new());
    assert_eq!(m.retiring_nicks(), vec!["cc-abc"]);
    assert!(m.is_owned("cc-abc"), "retiring reads as ours");
    // A NAMES resync commits in the gap, still listing the puppet: not a human.
    let g = m.self_joined("#mu");
    m.names_reply("#mu", g, names(&[("alice", None), ("cc-abc", None)]));
    assert_eq!(
        m.names_end("#mu", g),
        vec![HumanEffect::Register(human("alice"))]
    );
    assert!(!m.is_present("cc-abc"));
    // A PART of one channel does not end the gap (the connection is still up)…
    assert!(m.left("#mu", "cc-abc").is_empty());
    assert_eq!(m.retiring_nicks(), vec!["cc-abc"]);
    // …the QUIT does: the nick is nobody's, and a human may take it.
    assert!(m.quit("cc-abc").is_empty());
    assert!(m.retiring_nicks().is_empty());
    assert!(!m.is_owned("cc-abc"));
    assert_eq!(
        m.joined("#mu", "cc-abc", None),
        vec![HumanEffect::Register(human("cc-abc"))]
    );
    // Re-owning a retiring nick (the pool re-registered it) just owns it again.
    let mut m2 = Membership::new("mu-gw", RFC);
    m2.set_owned_nicks(["cc-def"]);
    m2.set_owned_nicks(Vec::<&str>::new());
    assert_eq!(m2.retiring_nicks(), vec!["cc-def"]);
    m2.set_owned_nicks(["cc-def"]);
    assert!(m2.retiring_nicks().is_empty());
    assert_eq!(m2.owned_nicks(), vec!["cc-def"]);
}

#[test]
fn owned_set_refolds_from_wire_spellings_on_casemapping_change() {
    let mut m = Membership::new("mu-gw", CaseMapping::Ascii);
    m.set_owned_nicks(["cc-a[b"]);
    let g = m.self_joined("#mu");
    // Under ascii `cc-a{b` is a different nick — a human, fronted.
    m.names_reply("#mu", g, names(&[("cc-a{b", None)]));
    assert_eq!(
        m.names_end("#mu", g),
        vec![HumanEffect::Register(human("cc-a{b"))]
    );
    // Under rfc1459 `[` and `{` fold together: the human now collides with the
    // gateway's puppet and is withdrawn, and the puppet stays owned.
    let effects = m.set_casemapping(RFC);
    assert_eq!(effects, vec![HumanEffect::Withdraw(human("cc-a{b"))]);
    assert!(m.is_owned("cc-a{b"));
    assert!(m.is_owned("cc-a[b"));
    assert!(m.present_humans().is_empty());
    // A puppet renamed by the server stays owned under its new spelling.
    assert!(m.renamed("cc-a[b", "cc-a[b2").is_empty());
    assert!(m.is_owned("cc-a[b2") && !m.is_owned("cc-a[b"));
    assert_eq!(m.owned_nicks(), vec!["cc-a[b2"]);
}

#[test]
fn a_renamed_puppet_leaves_a_tombstone_so_a_stale_names_line_cannot_front_it() {
    // Own cc-old, open a sync, rename it, then a delayed 353 still naming the
    // old spelling arrives: it must not become `human:cc-old`.
    let mut m = Membership::new("mu-gw", RFC);
    m.set_owned_nicks(["cc-old"]);
    let g = m.self_joined("#mu");
    assert!(m.renamed("cc-old", "cc-new").is_empty());
    assert!(m.is_owned("cc-new") && !m.is_owned("cc-old"));
    m.names_reply(
        "#mu",
        g,
        names(&[("alice", None), ("cc-old", None), ("cc-new", None)]),
    );
    let effects = m.names_end("#mu", g);
    assert_eq!(effects, vec![HumanEffect::Register(human("alice"))]);
    assert!(
        !m.is_present("cc-old"),
        "the vacated spelling was tombstoned"
    );
    assert!(!m.is_present("cc-new"), "the new spelling is owned");
    // A real human who later takes the freed nick arrives by JOIN, which is
    // newer than the tombstone.
    assert_eq!(
        m.joined("#mu", "cc-old", None),
        vec![HumanEffect::Register(human("cc-old"))]
    );
}

#[test]
fn an_owned_puppet_quit_or_part_is_tombstoned_for_an_open_sync_even_after_release() {
    // Own cc-abc, open a sync, the puppet QUITs, the pool releases the nick,
    // then a delayed 353 still lists it: the observed departure wins.
    let mut m = Membership::new("mu-gw", RFC);
    m.set_owned_nicks(["cc-abc", "cc-def"]);
    let g = m.self_joined("#mu");
    assert!(m.quit("cc-abc").is_empty());
    assert!(m.left("#mu", "cc-def").is_empty());
    assert!(m.set_owned_nicks(Vec::<&str>::new()).is_empty());
    m.names_reply(
        "#mu",
        g,
        names(&[("alice", None), ("cc-abc", None), ("cc-def", None)]),
    );
    assert_eq!(
        m.names_end("#mu", g),
        vec![HumanEffect::Register(human("alice"))]
    );
    assert!(!m.is_present("cc-abc") && !m.is_present("cc-def"));
    // cc-abc's QUIT was observed (while owned): a live JOIN under the freed
    // name is a human's. cc-def only PARTed: it is retiring until its QUIT.
    assert_eq!(
        m.joined("#mu", "cc-abc", None),
        vec![HumanEffect::Register(human("cc-abc"))]
    );
    assert!(m.joined("#mu", "cc-def", None).is_empty());
    assert!(m.quit("cc-def").is_empty());
    assert_eq!(
        m.joined("#mu", "cc-def", None),
        vec![HumanEffect::Register(human("cc-def"))]
    );
}

#[test]
fn a_puppets_own_departure_resolves_retiring_without_a_shared_channel() {
    // The puppet was released before it ever joined the lobby (JOIN refused,
    // or the peer left in the registered→JOIN gap), so mu-gw never sees its
    // QUIT. The executor sees the puppet's own socket close and reports it;
    // that report, not a channel event, frees the name.
    let mut m = Membership::new("mu-gw", RFC);
    m.set_owned_nicks(["cc-abc"]);
    m.set_owned_nicks(Vec::<&str>::new());
    assert_eq!(m.retiring_nicks(), vec!["cc-abc"]);
    let g = m.self_joined("#mu");
    m.puppet_departed("cc-abc", 0);
    assert!(m.retiring_nicks().is_empty());
    assert!(!m.is_owned("cc-abc"));
    // Tombstoned for the open sync, like an observed QUIT…
    m.names_reply("#mu", g, names(&[("cc-abc", None)]));
    assert!(m.names_end("#mu", g).is_empty());
    // …and a human may take the name by a live JOIN.
    assert_eq!(
        m.joined("#mu", "cc-abc", None),
        vec![HumanEffect::Register(human("cc-abc"))]
    );
    // A report for a nick the pool still holds is not acted on: it names a
    // nick, not a connection, and the holder may be a newer puppet by now.
    // The bridge releases first, then reports.
    let mut m2 = Membership::new("mu-gw", RFC);
    let g2 = m2.self_joined("#mu");
    m2.set_owned_nicks(["cc-def"]);
    m2.puppet_departed("cc-def", 0);
    assert!(m2.is_owned("cc-def"), "a report cannot un-own a held nick");
    assert!(
        m2.joined("#mu", "cc-def", None).is_empty(),
        "still our puppet"
    );
    m2.set_owned_nicks(Vec::<&str>::new());
    assert_eq!(
        m2.retiring_nicks(),
        vec!["cc-def"],
        "the release retires it"
    );
    m2.puppet_departed("cc-def", 0);
    assert!(
        m2.retiring_nicks().is_empty(),
        "the report after the release resolves it"
    );
    m2.names_reply("#mu", g2, names(&[("cc-def", None)]));
    assert!(
        m2.names_end("#mu", g2).is_empty(),
        "tombstoned for the open sync"
    );
    assert_eq!(
        m2.joined("#mu", "cc-def", None),
        vec![HumanEffect::Register(human("cc-def"))]
    );
}

#[test]
fn a_stale_departure_report_does_not_touch_a_newer_holder_of_the_name() {
    // Old connection's QUIT seen on the wire, released, and the spelling
    // registered again by a new connection; then the executor's delayed
    // report about the OLD connection arrives. The new puppet stays ours.
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#mu");
    m.set_owned_nicks(["cc-abc"]);
    assert!(m.quit("cc-abc").is_empty());
    m.set_owned_nicks(Vec::<&str>::new());
    m.set_owned_nicks(["cc-abc"]);
    m.puppet_departed("cc-abc", 0);
    assert!(
        m.is_owned("cc-abc"),
        "the stale report marked the new puppet gone"
    );
    assert!(
        m.joined("#mu", "cc-abc", None).is_empty(),
        "our puppet, not a human"
    );
    m.set_owned_nicks(Vec::<&str>::new());
    assert_eq!(
        m.retiring_nicks(),
        vec!["cc-abc"],
        "the new puppet's release retires it"
    );
}

#[test]
fn a_departure_report_resolves_only_the_connection_that_departed() {
    // Connection 1 held cc-abc, was released (retiring under 1), then the
    // spelling was registered again by connection 2 and released too
    // (retiring under 2). Connection 1's report, lagging all of that, must
    // not free connection 2's nick — connection 2 is still on the server
    // with its QUIT queued. Connection 2's own report does.
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#mu");
    m.set_owned([("cc-abc", 1)]);
    m.set_owned(Vec::<(&str, u64)>::new());
    m.set_owned([("cc-abc", 2)]);
    m.set_owned(Vec::<(&str, u64)>::new());
    assert_eq!(m.retiring_nicks(), vec!["cc-abc"]);
    m.puppet_departed("cc-abc", 1);
    assert_eq!(
        m.retiring_nicks(),
        vec!["cc-abc"],
        "connection 1's report freed connection 2's nick"
    );
    assert!(m.is_owned("cc-abc"));
    // A snapshot from a sync open since before the releases: the nick was
    // tombstoned in it at release, so this line predates the departure and
    // is neither fronted nor held back.
    m.names_reply("#mu", g, names(&[("cc-abc", None)]));
    assert!(
        m.names_end("#mu", g).is_empty(),
        "still our puppet in a snapshot"
    );
    // A snapshot from a sync opened after the release lists the name's new
    // holder: held back, and fronted by connection 2's report.
    let g2 = m.self_joined("#mu");
    m.names_reply("#mu", g2, names(&[("cc-abc", None)]));
    assert!(m.names_end("#mu", g2).is_empty(), "held back");
    assert_eq!(
        m.puppet_departed("cc-abc", 2),
        vec![HumanEffect::Register(human("cc-abc"))]
    );
    assert!(m.retiring_nicks().is_empty());
    assert!(m.is_present("cc-abc"));
    // The puppet's own rename report carries the connection with it — and
    // only its own connection's report moves it, and only for a rename that
    // connection reported (the barrier applies what `puppet_renaming` set
    // up; a report with no expectation standing moves nothing).
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#mu");
    m.set_owned([("cc-abc", 7)]);
    assert!(m.puppet_renaming("cc-abc", "cc-abc2", 7).is_empty());
    m.set_owned(Vec::<(&str, u64)>::new());
    assert!(m.puppet_renamed("cc-abc", "cc-abc2", 6).is_empty());
    assert_eq!(m.retiring_nicks(), vec!["cc-abc"], "not this connection's");
    assert!(m.puppet_renamed("cc-abc", "cc-abc2", 7).is_empty());
    assert_eq!(m.retiring_nicks(), vec!["cc-abc2"]);
    assert!(
        m.puppet_renamed("cc-abc2", "cc-abc3", 7).is_empty()
            && m.retiring_nicks() == vec!["cc-abc2"],
        "no expectation standing: nothing moves"
    );
    assert!(m.puppet_departed("cc-abc2", 6).is_empty());
    assert_eq!(m.retiring_nicks(), vec!["cc-abc2"], "wrong connection");
    m.puppet_departed("cc-abc2", 7);
    assert!(m.retiring_nicks().is_empty());
}

#[test]
fn what_the_wire_says_about_a_retiring_name_is_replayed_in_order_or_discarded() {
    // In the window, the wire shows the name JOIN #mu, get KICKed from it,
    // JOIN #ops, and rename to `carol`. A confirmed report (no shared
    // channel: nothing of the puppet's could have come) replays it all in
    // order: the holder ends up as carol in #ops only. An observed QUIT
    // instead discards it all.
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#mu");
    m.self_joined("#ops");
    m.set_owned([("cc-abc", 1)]);
    m.set_owned(Vec::<(&str, u64)>::new());
    assert!(m.joined("#mu", "cc-abc", None).is_empty());
    assert!(m.left("#mu", "cc-abc").is_empty());
    assert!(m.joined("#ops", "cc-abc", None).is_empty());
    assert!(m.renamed("cc-abc", "carol").is_empty(), "held back");
    assert_eq!(
        m.retiring_nicks(),
        vec!["carol"],
        "the entry moved with the NICK"
    );
    assert!(!m.is_present("cc-abc") && !m.is_present("carol"));
    // The vacated spelling is free: a new occupant under it is a human.
    assert_eq!(
        m.joined("#mu", "cc-abc", None),
        vec![HumanEffect::Register(human("cc-abc"))]
    );
    assert_eq!(
        m.quit("cc-abc"),
        vec![HumanEffect::Withdraw(human("cc-abc"))]
    );
    // The report is about the connection: it finds the entry by it, under
    // whatever spelling it has come to.
    // Replayed PROJECTED onto the name the holder goes by now: nothing is
    // done under cc-abc, which may be somebody else's by now (it is: see
    // the QUIT above), and no rename is emitted for a holder never fronted
    // under the old name.
    let effects = m.puppet_departed("cc-abc", 1);
    assert_eq!(
        effects,
        vec![
            HumanEffect::Register(human("carol")),
            HumanEffect::Withdraw(human("carol")),
            HumanEffect::Register(human("carol")),
        ]
    );
    assert!(m.is_present("carol") && !m.is_present("cc-abc"));
    assert!(m.retiring_nicks().is_empty());
    // The same story ended by the puppet's QUIT instead: nothing of it was
    // a human's.
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#mu");
    m.set_owned([("cc-abc", 1)]);
    m.set_owned(Vec::<(&str, u64)>::new());
    assert!(m.joined("#mu", "cc-abc", None).is_empty());
    assert!(m.renamed("cc-abc", "cc-abc2").is_empty());
    assert!(
        m.quit("cc-abc2").is_empty(),
        "the QUIT comes under the new name"
    );
    assert!(!m.is_present("cc-abc") && !m.is_present("cc-abc2"));
    assert!(m.retiring_nicks().is_empty());
    assert!(m.puppet_departed("cc-abc", 1).is_empty());
}

#[test]
fn a_spelling_re_held_by_another_connection_without_a_release_is_fresh() {
    // Connection 1's QUIT was seen while the pool still listed the nick;
    // before that release reached membership the pool re-registered the
    // spelling under connection 2. The remembered departure was 1's: 2 is
    // our puppet, not a human, and 2's release retires it.
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#mu");
    m.set_owned([("cc-abc", 1)]);
    assert!(m.quit("cc-abc").is_empty());
    assert!(
        !m.is_owned("cc-abc"),
        "1 is gone: the name is a human's for now"
    );
    m.set_owned([("cc-abc", 2)]);
    assert!(m.is_owned("cc-abc"), "connection 2 holds the name");
    assert!(
        m.joined("#mu", "cc-abc", None).is_empty(),
        "our puppet, not a human"
    );
    m.set_owned(Vec::<(&str, u64)>::new());
    assert_eq!(m.retiring_nicks(), vec!["cc-abc"], "2's release retires it");
}

#[test]
fn a_human_who_takes_a_retiring_name_before_the_report_is_fronted_by_the_report() {
    // The puppet was released; it was in no channel the gateway shares, so
    // no QUIT for it will ever be observed here. A human takes the freed
    // name and JOINs before the executor's report arrives: the JOIN is held
    // back under the retiring nick, and the report — which resolves the
    // entry — replays it as the human's.
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#mu");
    m.set_owned([("cc-abc", 1)]);
    m.set_owned(Vec::<(&str, u64)>::new());
    assert!(m.joined("#mu", "cc-abc", None).is_empty(), "held back");
    assert!(!m.is_present("cc-abc"));
    // A snapshot line in the window is held back the same way.
    m.names_reply("#mu", g, names(&[("cc-abc", None)]));
    assert!(m.names_end("#mu", g).is_empty());
    assert_eq!(
        m.puppet_departed("cc-abc", 1),
        vec![HumanEffect::Register(human("cc-abc"))],
        "the report fronts the name's new holder"
    );
    assert!(m.is_present("cc-abc"));
    assert!(m.retiring_nicks().is_empty());
    // The same report twice is nothing.
    assert!(m.puppet_departed("cc-abc", 1).is_empty());
}

#[test]
fn a_retiring_puppets_own_join_echo_is_discarded_by_its_observed_quit() {
    // The puppet WAS in a shared channel: its JOIN echo reaches this
    // connection (held back under the retiring nick), then its QUIT, which
    // resolves the entry — and what was held back was the puppet itself.
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#mu");
    m.set_owned([("cc-abc", 1)]);
    m.set_owned(Vec::<(&str, u64)>::new());
    assert!(m.joined("#mu", "cc-abc", None).is_empty());
    assert!(m.quit("cc-abc").is_empty());
    assert!(
        !m.is_present("cc-abc"),
        "the puppet's echo was replayed as a human"
    );
    assert!(m.retiring_nicks().is_empty());
    // The executor's report after that is nothing…
    assert!(m.puppet_departed("cc-abc", 1).is_empty());
    assert!(!m.is_present("cc-abc"));
    // …and a human JOIN after the QUIT is a human's, at once.
    assert_eq!(
        m.joined("#mu", "cc-abc", None),
        vec![HumanEffect::Register(human("cc-abc"))]
    );
}

#[test]
fn an_unconfirmed_departure_frees_the_name_but_replays_nothing() {
    // The puppet's connection was cut at the grace, the server never closed
    // it: nothing orders the server's view behind the report. The entry is
    // resolved, but the arrival held back under the name — maybe the
    // puppet's own echo — is not fronted as a human.
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#mu");
    m.set_owned([("cc-abc", 1)]);
    m.set_owned(Vec::<(&str, u64)>::new());
    assert!(m.joined("#mu", "cc-abc", None).is_empty(), "held back");
    assert_eq!(m.held_by("cc-abc"), Some(1));
    m.puppet_departed_unconfirmed("cc-abc", 2);
    assert_eq!(
        m.retiring_nicks(),
        vec!["cc-abc"],
        "another connection's report"
    );
    m.puppet_departed_unconfirmed("cc-abc", 1);
    assert!(m.retiring_nicks().is_empty());
    assert!(!m.is_present("cc-abc"), "nothing replayed");
    assert_eq!(m.held_by("cc-abc"), None);
    // The name is free: the next JOIN under it is a human's.
    assert_eq!(
        m.joined("#mu", "cc-abc", None),
        vec![HumanEffect::Register(human("cc-abc"))]
    );
    // held_by: owned, gone, retiring.
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#mu");
    m.set_owned([("cc-abc", 7)]);
    assert_eq!(m.held_by("cc-abc"), Some(7));
    assert!(m.quit("cc-abc").is_empty());
    assert_eq!(
        m.held_by("cc-abc"),
        None,
        "seen to leave: a human's for now"
    );
    assert_eq!(m.held_by("nobody"), None);
}

#[test]
fn a_retiring_names_holder_is_followed_under_the_name_they_rename_to() {
    // In the window the holder JOINs #mu as cc-abc, renames to carol, then
    // JOINs #ops as carol and PARTs #mu as carol. The entry and its story
    // move to carol — nothing under carol runs ahead of what is held back,
    // and the vacated cc-abc is free — and a confirmed report, which finds
    // the entry by the connection, replays it in order: carol in #ops only.
    // A QUIT under carol ends the entry.
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#mu");
    m.self_joined("#ops");
    m.set_owned([("cc-abc", 1)]);
    m.set_owned(Vec::<(&str, u64)>::new());
    assert!(m.joined("#mu", "cc-abc", None).is_empty());
    assert!(m.renamed("cc-abc", "carol").is_empty());
    assert!(
        m.is_owned("carol"),
        "carol is the retiring holder's name now"
    );
    assert!(
        m.joined("#ops", "carol", None).is_empty(),
        "held back under the entry"
    );
    assert!(m.left("#mu", "carol").is_empty());
    assert!(!m.is_present("carol") && !m.is_present("cc-abc"));
    assert!(!m.is_owned("cc-abc"), "the vacated spelling is free");
    assert_eq!(m.retiring_nicks(), vec!["carol"]);
    let effects = m.puppet_departed("cc-abc", 1);
    assert_eq!(effects, vec![HumanEffect::Register(human("carol"))]);
    assert!(m.is_present("carol"));
    assert!(!m.is_owned("carol"), "the story is over: carol is a human");
    assert!(m.retiring_nicks().is_empty());
    assert_eq!(
        m.left("#ops", "carol"),
        vec![HumanEffect::Withdraw(human("carol"))]
    );
    // A case-only respelling keeps the entry under the same folded key
    // with the new wire spelling: CAROL is still the holder's name.
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#mu");
    m.set_owned([("cc-abc", 1)]);
    m.set_owned(Vec::<(&str, u64)>::new());
    assert!(m.renamed("cc-abc", "carol").is_empty());
    assert!(m.renamed("carol", "CAROL").is_empty());
    assert!(
        m.is_owned("CAROL"),
        "the case-only respelling lost the entry"
    );
    assert_eq!(m.retiring_nicks(), vec!["CAROL"]);
    assert!(m.joined("#mu", "CAROL", None).is_empty(), "held back");
    assert!(m.quit("CAROL").is_empty());
    assert!(
        m.retiring_nicks().is_empty(),
        "a QUIT under the respelt name ends the entry"
    );
    // A new occupant of the vacated spelling, in the window, is a human at
    // once — and the holder's QUIT under the new name does not touch them.
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#mu");
    m.set_owned([("cc-abc", 1)]);
    m.set_owned(Vec::<(&str, u64)>::new());
    assert!(m.renamed("cc-abc", "carol").is_empty());
    assert_eq!(
        m.joined("#mu", "cc-abc", None),
        vec![HumanEffect::Register(human("cc-abc"))]
    );
    // A snapshot line still naming the vacated spelling, from a sync open
    // before the NICK, is tombstoned — the newer fact is the live JOIN.
    m.names_reply("#mu", g, names(&[("cc-abc", None)]));
    assert!(m.names_end("#mu", g).is_empty());
    assert!(m.is_present("cc-abc"));
    assert!(m.quit("carol").is_empty());
    assert!(m.retiring_nicks().is_empty());
    assert!(
        m.is_present("cc-abc"),
        "the new occupant was discarded with the story"
    );
    // The same, ended by a QUIT under the new name.
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#mu");
    m.set_owned([("cc-abc", 1)]);
    m.set_owned(Vec::<(&str, u64)>::new());
    assert!(m.joined("#mu", "cc-abc", None).is_empty());
    assert!(m.renamed("cc-abc", "carol").is_empty());
    assert!(m.quit("carol").is_empty());
    assert!(
        m.retiring_nicks().is_empty(),
        "the QUIT under the new name ended the entry"
    );
    assert!(!m.is_owned("carol") && !m.is_owned("cc-abc"));
    assert!(m.puppet_departed("cc-abc", 1).is_empty());
    assert_eq!(
        m.joined("#mu", "carol", None),
        vec![HumanEffect::Register(human("carol"))]
    );
}

#[test]
fn a_reported_rename_holds_the_old_spelling_and_claims_the_new_until_the_barrier() {
    // The puppet's own connection reported cc-abc → cc-b; the bridge will
    // apply it at its barrier. Until then: cc-b is ours (a JOIN under it is
    // the puppet's), cc-abc is still ours for the sync but vacating (what the
    // wire says under it is held back), and the sync in between changes
    // nothing. Then, the two ways it settles.
    //
    // The wire's own NICK first (the puppet shared a channel): what was held
    // under cc-abc was the puppet's echo — discarded — and the rename is
    // applied from the wire; the barrier then finds nothing to do.
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#mu");
    m.set_owned([("cc-abc", 1)]);
    assert!(m.puppet_renaming("cc-abc", "cc-b", 1).is_empty());
    assert!(m.is_owned("cc-b"), "expected: ours on the server already");
    assert!(
        m.joined("#mu", "cc-b", None).is_empty(),
        "the puppet's JOIN under the new name"
    );
    assert!(
        m.joined("#mu", "cc-abc", None).is_empty(),
        "held back under the vacating name"
    );
    assert!(m.set_owned([("cc-abc", 1)]).is_empty(), "a sync in between");
    assert_eq!(m.owned_nicks(), vec!["cc-abc"]);
    assert!(m.renamed("cc-abc", "cc-b").is_empty());
    assert_eq!(m.owned_nicks(), vec!["cc-b"]);
    assert!(
        !m.is_present("cc-abc") && !m.is_present("cc-b"),
        "the echo was the puppet"
    );
    assert!(
        m.puppet_renamed("cc-abc", "cc-b", 1).is_empty(),
        "the barrier: history"
    );
    assert_eq!(m.owned_nicks(), vec!["cc-b"]);
    assert!(m.retiring_nicks().is_empty());
    // A stranger under cc-abc after the rename is a human, at once.
    assert_eq!(
        m.joined("#mu", "cc-abc", None),
        vec![HumanEffect::Register(human("cc-abc"))]
    );

    // The barrier first (the puppet was in no shared channel — the rename
    // happened in the registered→JOIN gap): a human who took cc-abc in the
    // window and JOINed is held back, and fronted by the barrier; the
    // puppet's JOIN under cc-b never was a human.
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#mu");
    m.set_owned([("cc-abc", 1)]);
    assert!(m.puppet_renaming("cc-abc", "cc-b", 1).is_empty());
    assert!(m.joined("#mu", "cc-b", None).is_empty());
    assert!(m.joined("#mu", "cc-abc", None).is_empty(), "held back");
    assert_eq!(
        m.puppet_renamed("cc-abc", "cc-b", 1),
        vec![HumanEffect::Register(human("cc-abc"))],
        "the name's new holder, fronted by the barrier"
    );
    assert_eq!(m.owned_nicks(), vec!["cc-b"]);
    assert!(m.is_present("cc-abc"));
    assert!(m.retiring_nicks().is_empty());

    // The new holder renamed within the window: the story follows them,
    // and is replayed under the name they go by. The vacated spelling is
    // still the pool's, so a SECOND taker of it in the window starts a
    // story of their own there, and the barrier fronts both.
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#mu");
    m.set_owned([("cc-abc", 1)]);
    assert!(m.puppet_renaming("cc-abc", "cc-b", 1).is_empty());
    assert!(m.joined("#mu", "cc-abc", None).is_empty());
    assert!(
        m.renamed("cc-abc", "dave").is_empty(),
        "held back: the holder's, not ours to move"
    );
    assert_eq!(
        m.owned_nicks(),
        vec!["cc-abc"],
        "the pool's spelling stays owned for the sync"
    );
    assert!(
        m.is_owned("dave"),
        "and dave is held as the story's, not a human's yet"
    );
    assert!(
        m.joined("#mu", "cc-abc", None).is_empty(),
        "a second taker: held back too"
    );
    // A CASEMAPPING change in the window keeps every story.
    assert!(m.set_casemapping(CaseMapping::Ascii).is_empty());
    let mut effects = m.puppet_renamed("cc-abc", "cc-b", 1);
    effects.sort_by_key(|e| format!("{e:?}"));
    assert_eq!(
        effects,
        vec![
            HumanEffect::Register(human("cc-abc")),
            HumanEffect::Register(human("dave")),
        ]
    );
    assert_eq!(m.owned_nicks(), vec!["cc-b"]);
    assert!(m.is_present("dave") && m.is_present("cc-abc"));
}

#[test]
fn an_expected_name_is_freed_by_its_puppets_quit_and_by_a_release() {
    // The puppet, under the name it was renamed to, quits before the barrier
    // applied the rename: the name is free from here — a human under it is
    // a human. And a release (the pool let the peer go) drops the
    // expectation with it.
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#mu");
    m.set_owned([("cc-abc", 1)]);
    assert!(m.puppet_renaming("cc-abc", "cc-b", 1).is_empty());
    assert!(
        m.joined("#mu", "cc-b", None).is_empty(),
        "the puppet's JOIN"
    );
    assert!(
        m.quit("cc-b").is_empty(),
        "the puppet's QUIT, under its new name"
    );
    assert!(!m.is_owned("cc-b"), "the expected name is free");
    assert_eq!(
        m.joined("#mu", "cc-b", None),
        vec![HumanEffect::Register(human("cc-b"))]
    );
    // The barrier finds nothing of the connection's left to move and leaves
    // the human; so does the pool's rename catching up (the same connection,
    // under the name the wire already moved it to), and the release after.
    assert!(m.puppet_renamed("cc-abc", "cc-b", 1).is_empty());
    assert!(m.is_present("cc-b") && !m.is_owned("cc-b"));
    assert!(m.set_owned([("cc-b", 1)]).is_empty());
    assert!(m.is_present("cc-b") && !m.is_owned("cc-b"));
    assert!(m.set_owned(Vec::<(&str, u64)>::new()).is_empty());
    assert!(m.retiring_nicks().is_empty(), "its departure was seen");
    assert!(m.puppet_departed("cc-b", 1).is_empty());
    assert!(m.is_present("cc-b"));
    // A release while a rename is pending KEEPS the expectation: the puppet
    // is on the server under the new name, its QUIT still pending, so what
    // the wire says under it — a JOIN, a NAMES line — is the puppet's, until
    // its QUIT frees the name. The departure report resolves the retiring
    // entry by connection, whatever it is filed under; the barrier, coming
    // after the QUIT, moves nothing.
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#mu");
    m.set_owned([("cc-abc", 1)]);
    assert!(m.puppet_renaming("cc-abc", "cc-b", 1).is_empty());
    m.set_owned(Vec::<(&str, u64)>::new());
    assert!(m.is_owned("cc-b"), "the expectation survives the release");
    assert_eq!(m.retiring_nicks(), vec!["cc-abc"]);
    assert!(
        m.joined("#mu", "cc-b", None).is_empty(),
        "the puppet's JOIN"
    );
    m.names_reply("#mu", g, names(&[("cc-b", None)]));
    assert!(m.names_end("#mu", g).is_empty(), "the puppet in a snapshot");
    assert!(m.quit("cc-b").is_empty(), "the puppet's QUIT");
    assert!(!m.is_owned("cc-b") && !m.is_present("cc-b"));
    assert_eq!(m.retiring_nicks(), vec!["cc-abc"], "resolved by its report");
    assert_eq!(
        m.joined("#mu", "cc-b", None),
        vec![HumanEffect::Register(human("cc-b"))]
    );
    assert!(m.puppet_renamed("cc-abc", "cc-b", 1).is_empty());
    assert!(m.is_present("cc-b") && !m.is_owned("cc-b"));
    assert!(m.puppet_departed("cc-b", 1).is_empty());
    assert!(m.retiring_nicks().is_empty());
    assert_eq!(
        m.left("#mu", "cc-b"),
        vec![HumanEffect::Withdraw(human("cc-b"))]
    );
    // The same, with the wire's own NICK settling it: the retiring entry
    // moves to the new name, the old spelling is free at once.
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#mu");
    m.set_owned([("cc-abc", 1)]);
    assert!(m.joined("#mu", "cc-abc", None).is_empty());
    assert!(m.puppet_renaming("cc-abc", "cc-b", 1).is_empty());
    m.set_owned(Vec::<(&str, u64)>::new());
    assert!(m.renamed("cc-abc", "cc-b").is_empty());
    assert_eq!(m.retiring_nicks(), vec!["cc-b"]);
    assert_eq!(
        m.joined("#mu", "cc-abc", None),
        vec![HumanEffect::Register(human("cc-abc"))]
    );
    assert!(m.quit("cc-b").is_empty());
    assert!(m.retiring_nicks().is_empty());
    assert_eq!(
        m.joined("#mu", "cc-b", None),
        vec![HumanEffect::Register(human("cc-b"))]
    );
    assert!(m.puppet_renamed("cc-abc", "cc-b", 1).is_empty());
    assert!(m.is_present("cc-b") && m.is_present("cc-abc"));
    assert!(m.puppet_departed("cc-b", 1).is_empty());
    // Renaming onto a spelling this connection still lists a human under
    // (stale: the server says it is ours) withdraws that human.
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#mu");
    m.set_owned([("cc-abc", 1)]);
    assert_eq!(
        m.joined("#mu", "cc-b", None),
        vec![HumanEffect::Register(human("cc-b"))]
    );
    assert_eq!(
        m.puppet_renaming("cc-abc", "cc-b", 1),
        vec![HumanEffect::Withdraw(human("cc-b"))]
    );
    assert!(m.is_owned("cc-b") && !m.is_present("cc-b"));
}

#[test]
fn a_quit_under_the_expected_name_settles_the_pending_rename_from_the_wire() {
    // The puppet renamed cc-abc → cc-b in no shared channel, a human took
    // cc-abc in the window (held back, as the vacating spelling's), then the
    // puppet joined #mu as cc-b and quit. The QUIT is the wire's word that
    // the rename happened: the human under cc-abc is fronted by it, as the
    // barrier would have, and cc-abc is theirs from here.
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#mu");
    m.set_owned([("cc-abc", 1)]);
    assert!(m.puppet_renaming("cc-abc", "cc-b", 1).is_empty());
    assert!(m.joined("#mu", "cc-abc", None).is_empty(), "held back");
    assert!(
        m.joined("#mu", "cc-b", None).is_empty(),
        "the puppet's JOIN"
    );
    assert_eq!(m.quit("cc-b"), vec![HumanEffect::Register(human("cc-abc"))]);
    assert!(m.is_present("cc-abc") && !m.is_owned("cc-abc"));
    assert!(!m.is_owned("cc-b") && !m.is_present("cc-b"));
    assert!(m.puppet_renamed("cc-abc", "cc-b", 1).is_empty());
    assert_eq!(
        m.left("#mu", "cc-abc"),
        vec![HumanEffect::Withdraw(human("cc-abc"))]
    );
}

#[test]
fn a_sync_listing_a_connection_under_its_old_spelling_is_the_same_nick() {
    // The wire moved connection 1 from cc-abc to cc-b (its own NICK, seen on
    // the main connection); the pool learns of it only at the barrier. A
    // sync in between — another puppet registering — still lists connection
    // 1 as cc-abc: that is cc-b, not a second nick. A human who took cc-abc
    // since is left alone, and cc-b is not released.
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#mu");
    m.set_owned([("cc-abc", 1)]);
    assert!(m.joined("#mu", "cc-abc", None).is_empty());
    assert!(m.puppet_renaming("cc-abc", "cc-b", 1).is_empty());
    assert!(m.renamed("cc-abc", "cc-b").is_empty());
    assert!(m.is_owned("cc-b") && !m.is_owned("cc-abc"));
    assert_eq!(
        m.joined("#mu", "cc-abc", None),
        vec![HumanEffect::Register(human("cc-abc"))]
    );
    assert!(m.set_owned([("cc-abc", 1), ("cc-x", 2)]).is_empty());
    assert!(m.is_present("cc-abc") && !m.is_owned("cc-abc"));
    assert!(m.is_owned("cc-b") && m.is_owned("cc-x"));
    assert!(m.retiring_nicks().is_empty());
    // The barrier, then the pool's catch-up: nothing changes hands.
    assert!(m.puppet_renamed("cc-abc", "cc-b", 1).is_empty());
    assert!(m.set_owned([("cc-b", 1), ("cc-x", 2)]).is_empty());
    assert!(m.is_present("cc-abc") && m.is_owned("cc-b"));
    // Without a connection id there is nothing to tell them apart by:
    // spelling is the key, as before.
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#mu");
    m.set_owned_nicks(["cc-abc"]);
    assert!(m.renamed("cc-abc", "cc-b").is_empty());
    m.set_owned_nicks(["cc-abc"]);
    assert!(m.is_owned("cc-abc"));
    assert_eq!(m.retiring_nicks(), vec!["cc-b"]);
}

#[test]
fn a_replayed_story_never_touches_a_stranger_under_the_vacated_spelling() {
    // cc-abc is retiring; its new holder JOINs #a and renames to carol; a
    // SECOND human then takes cc-abc and JOINs #b. The report resolves the
    // entry (now carol): the story is replayed as carol's — #a — and the
    // second human, who is cc-abc in #b, is left exactly where they are.
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#a");
    m.self_joined("#b");
    m.set_owned([("cc-abc", 1)]);
    m.set_owned(Vec::<(&str, u64)>::new());
    assert!(m.joined("#a", "cc-abc", None).is_empty());
    assert!(m.renamed("cc-abc", "carol").is_empty());
    assert_eq!(
        m.joined("#b", "cc-abc", None),
        vec![HumanEffect::Register(human("cc-abc"))],
        "the vacated spelling's new occupant"
    );
    assert_eq!(
        m.puppet_departed("cc-abc", 1),
        vec![HumanEffect::Register(human("carol"))]
    );
    assert!(m.is_present("carol") && m.is_present("cc-abc"));
    assert_eq!(
        m.left("#b", "cc-abc"),
        vec![HumanEffect::Withdraw(human("cc-abc"))],
        "the stranger is still cc-abc in #b, and only there"
    );
    assert_eq!(
        m.left("#a", "carol"),
        vec![HumanEffect::Withdraw(human("carol"))]
    );
}

#[test]
fn a_story_forgets_a_channel_the_gateway_left() {
    // A JOIN under a retiring name is held back for #a; the gateway leaves
    // #a and rejoins it before the report. The held-back JOIN is moot — the
    // rejoin's snapshot says who is there — and is not replayed into the new
    // roster.
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#a");
    m.set_owned([("cc-abc", 1)]);
    m.set_owned(Vec::<(&str, u64)>::new());
    assert!(m.joined("#a", "cc-abc", None).is_empty());
    m.left("#a", "mu-gw");
    let g = m.self_joined("#a");
    m.names_reply("#a", g, names(&[("bob", None)]));
    assert_eq!(
        m.names_end("#a", g),
        vec![HumanEffect::Register(human("bob"))]
    );
    assert!(
        m.puppet_departed("cc-abc", 1).is_empty(),
        "nothing to replay"
    );
    assert!(!m.is_present("cc-abc"));
}

#[test]
fn a_live_puppet_renamed_onto_a_departed_puppets_name_owns_it_again() {
    // B's QUIT was seen while the pool still lists B (release pending); the
    // server then renames live puppet A onto the freed spelling B. A is our
    // puppet under B now: not a human, and B's pending release (the pool
    // moved A's ownership, so B stays listed) must not read B as departed.
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#mu");
    m.set_owned_nicks(["cc-aaa", "cc-bbb"]);
    assert!(m.quit("cc-bbb").is_empty());
    assert!(
        m.renamed("cc-aaa", "cc-bbb").is_empty(),
        "a puppet's rename"
    );
    assert!(
        m.is_owned("cc-bbb"),
        "a live puppet holds the spelling again"
    );
    assert!(
        m.joined("#mu", "cc-bbb", None).is_empty(),
        "our puppet, not a human"
    );
    // The pool's view after the rename: only B is held.
    assert!(m.set_owned_nicks(["cc-bbb"]).is_empty());
    assert!(m.is_owned("cc-bbb"));
    m.set_owned_nicks(Vec::<&str>::new());
    assert_eq!(
        m.retiring_nicks(),
        vec!["cc-bbb"],
        "released: retiring until its own departure"
    );
}

#[test]
fn a_human_taking_a_departed_puppets_name_before_the_release_is_fronted() {
    // The puppet's QUIT was observed on the main connection while the pool
    // still lists the nick (its release has not reached membership). A human
    // JOINs under the freed name in that window: a human, fronted at once,
    // still there after the release, and withdrawn by their own QUIT. Neither
    // the release nor a NAMES snapshot mistakes them for our puppet.
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#mu");
    m.set_owned_nicks(["cc-abc"]);
    assert!(m.quit("cc-abc").is_empty(), "our puppet's QUIT");
    assert_eq!(
        m.joined("#mu", "cc-abc", None),
        vec![HumanEffect::Register(human("cc-abc"))],
        "the JOIN under a departed puppet's name is a human's"
    );
    // A sync while the pool still lists the nick does not evict the human.
    assert!(m.set_owned_nicks(["cc-abc"]).is_empty());
    assert!(m.is_present("cc-abc"));
    // A snapshot in that window lists the human too.
    m.names_reply("#mu", g, names(&[("cc-abc", None)]));
    assert!(
        m.names_end("#mu", g).is_empty(),
        "already present: nothing new"
    );
    assert!(m.is_present("cc-abc"));
    // The human renames as a human, not as our puppet.
    assert_eq!(
        m.renamed("cc-abc", "carol"),
        vec![HumanEffect::Rename {
            from: human("cc-abc"),
            to: human("carol"),
        }]
    );
    // The pool's release frees the old spelling: nothing retires.
    assert!(m.set_owned_nicks(Vec::<&str>::new()).is_empty());
    assert!(m.retiring_nicks().is_empty());
    assert!(!m.is_owned("cc-abc"));
    assert!(m.is_present("carol"));
    assert_eq!(m.quit("carol"), vec![HumanEffect::Withdraw(human("carol"))]);
}

#[test]
fn a_departure_already_seen_on_the_main_connection_is_not_applied_twice() {
    // The main connection saw the puppet's QUIT (still owned: `gone`); a
    // human joined under the freed name into an open sync; then the
    // executor's own report of the same departure arrives. It must not
    // tombstone the human's snapshot entry — the departure is already
    // accounted for — and the sync commits them.
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#mu");
    m.set_owned_nicks(["cc-abc"]);
    assert!(m.quit("cc-abc").is_empty());
    // A live JOIN is unambiguous (a snapshot line could predate the QUIT):
    // the human is fronted at once and belongs in the sync being built.
    assert_eq!(
        m.joined("#mu", "cc-abc", None),
        vec![HumanEffect::Register(human("cc-abc"))]
    );
    m.puppet_departed("cc-abc", 0);
    assert!(
        m.names_end("#mu", g).is_empty(),
        "the executor's late report erased the human from the snapshot"
    );
    assert!(m.is_present("cc-abc"));
    assert!(m.set_owned_nicks(Vec::<&str>::new()).is_empty());
    assert!(m.retiring_nicks().is_empty());
    assert!(m.is_present("cc-abc"));
}

#[test]
fn a_departed_puppets_name_stays_a_humans_across_a_casemapping_change() {
    // Same window (QUIT seen, name taken by a human, release pending), and
    // the server changes CASEMAPPING. The departure must survive the re-key:
    // otherwise the human is dropped from the rosters as "our puppet" and the
    // release retires the name for a QUIT that was already seen.
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#mu");
    m.set_owned_nicks(["cc-abc"]);
    assert!(m.quit("cc-abc").is_empty());
    assert_eq!(
        m.joined("#mu", "cc-abc", None),
        vec![HumanEffect::Register(human("cc-abc"))]
    );
    assert!(
        m.set_casemapping(CaseMapping::Ascii).is_empty(),
        "the human was withdrawn by the re-key"
    );
    assert!(m.is_present("cc-abc"));
    assert!(
        !m.is_owned("cc-abc"),
        "a departed puppet's name is not ours"
    );
    assert!(m.set_owned_nicks(Vec::<&str>::new()).is_empty());
    assert!(m.retiring_nicks().is_empty());
    assert_eq!(
        m.quit("cc-abc"),
        vec![HumanEffect::Withdraw(human("cc-abc"))]
    );
}

#[test]
fn a_fresh_registration_forgets_an_earlier_holders_departure() {
    // cc-abc's first connection's QUIT was seen on the wire while owned, it
    // was released, then the pool re-registered the same spelling. The
    // remembered departure belonged to the first connection; the next release
    // must retire the second one until ITS departure is seen, not treat it as
    // already gone.
    let mut m = Membership::new("mu-gw", RFC);
    m.self_joined("#mu");
    m.set_owned_nicks(["cc-abc"]);
    assert!(m.quit("cc-abc").is_empty());
    m.set_owned_nicks(Vec::<&str>::new());
    assert!(m.retiring_nicks().is_empty());
    m.set_owned_nicks(["cc-abc"]);
    m.set_owned_nicks(Vec::<&str>::new());
    assert_eq!(
        m.retiring_nicks(),
        vec!["cc-abc"],
        "the second connection's release did not retire the nick"
    );
    assert!(m.joined("#mu", "cc-abc", None).is_empty(), "held back");
    assert_eq!(
        m.puppet_departed("cc-abc", 0),
        vec![HumanEffect::Register(human("cc-abc"))],
        "the held-back JOIN is the name's new holder"
    );
}
