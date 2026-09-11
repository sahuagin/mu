//! Offline IRC→mesh tests for increment 3: intent classification, destination
//! checks, fan-out, routing-memory rules, and both loop guards — including the
//! publish-then-observe race. No socket, no live mesh.

use mu_dialogue::mesh::{MeshDmEvent, Reception};
use mu_irc_gateway::mapping::{channel_for, CaseMapping};
use mu_irc_gateway::membership::Membership;
use mu_irc_gateway::outbound::{
    MemoryDestination, MemoryUpdate, OutDrop, OutEnv, Outbound, OutboundDecision, RefuseReason,
};
use mu_peer::PeerId;

const RFC: CaseMapping = CaseMapping::Rfc1459;

fn out() -> Outbound {
    Outbound::new("mu-gw", RFC, "#", "#mu", 50)
}

fn empty_mem() -> Membership {
    Membership::new("mu-gw", RFC)
}

/// A membership with `alice` present in `#a`.
fn mem_with_alice() -> Membership {
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#a");
    m.names_reply("#a", g, [("alice".to_string(), None)]);
    m.names_end("#a", g);
    m
}

fn event(id: &str, from: &str) -> MeshDmEvent {
    MeshDmEvent {
        id: id.into(),
        destination: "mu.agent.cc.abc.dm".into(),
        from: from.into(),
        body: "b".into(),
        subject: None,
        session: None,
        reception: Reception::Observer,
    }
}

/// Destructure a `Publish` decision.
fn published(d: &OutboundDecision) -> (&str, &[PeerId], &str, &Option<MemoryUpdate>) {
    match d {
        OutboundDecision::Publish {
            id,
            targets,
            body,
            memory,
            ..
        } => (id.as_str(), targets.as_slice(), body.as_str(), memory),
        other => panic!("expected Publish, got {other:?}"),
    }
}

// ─────────────────────────────── Loop guard 1 ───────────────────────────────

#[test]
fn own_nick_line_is_dropped_folded() {
    let mut o = out();
    let mem = empty_mem();
    let env = OutEnv {
        peers: &[],
        membership: &mem,
    };
    // Exact and case-folded spellings of the gateway's own nick both drop.
    assert_eq!(
        o.route_line("mu-gw", "#mu", "hi", "ID", &env),
        OutboundDecision::Drop(OutDrop::OwnNick)
    );
    assert_eq!(
        o.route_line("MU-GW", "#mu", "hi", "ID", &env),
        OutboundDecision::Drop(OutDrop::OwnNick)
    );
}

// ───────────────────────────── Disconnected ─────────────────────────────────

#[test]
fn a_line_while_disconnected_is_dropped() {
    let mut o = out();
    o.set_connected(false);
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:abc")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
    };
    assert_eq!(
        o.route_line("alice", "#mu", "hi", "ID", &env),
        OutboundDecision::Drop(OutDrop::Disconnected)
    );
}

// ───────────────────────────────── Fan-out ──────────────────────────────────

#[test]
fn lobby_fans_out_under_one_id_with_no_memory_change() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:a"), PeerId::parse("mu:d")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
    };
    let d = o.route_line("alice", "#mu", "hello all", "FAN", &env);
    let (id, targets, body, memory) = published(&d);
    assert_eq!(id, "FAN");
    assert_eq!(targets.len(), 2, "one id across every discovered agent");
    assert_eq!(body, "hello all");
    assert!(memory.is_none(), "a lobby line changes no routing memory");
}

#[test]
fn a_fanout_with_no_agents_is_refused() {
    let mut o = out();
    let mem = mem_with_alice();
    let env = OutEnv {
        peers: &[],
        membership: &mem,
    };
    assert_eq!(
        o.route_line("alice", "#mu", "anyone?", "ID", &env),
        OutboundDecision::Refuse(RefuseReason::NoDestinations)
    );
}

// ─────────────────────────── Directed channel line ──────────────────────────

#[test]
fn a_channel_line_to_one_agent_publishes_and_remembers() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:abc")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
    };
    let d = o.route_line("alice", "#cc-abc", "hi cc", "ID", &env);
    let (_, targets, body, memory) = published(&d);
    assert_eq!(targets, &[PeerId::parse("cc:abc")]);
    assert_eq!(body, "hi cc");
    assert_eq!(
        memory,
        &Some(MemoryUpdate {
            human: "alice".into(),
            destination: MemoryDestination::Channel("#cc-abc".into()),
        })
    );
}

#[test]
fn an_ambiguous_channel_is_refused_naming_the_peers() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:abc"), PeerId::parse("cc:ABC")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
    };
    match o.route_line("alice", "#cc-abc", "hi", "ID", &env) {
        OutboundDecision::Refuse(RefuseReason::AmbiguousChannel(peers)) => {
            assert!(peers.contains(&PeerId::parse("cc:abc")));
            assert!(peers.contains(&PeerId::parse("cc:ABC")));
        }
        other => panic!("expected AmbiguousChannel, got {other:?}"),
    }
}

#[test]
fn an_unknown_channel_is_refused() {
    let mut o = out();
    let mem = mem_with_alice();
    let env = OutEnv {
        peers: &[],
        membership: &mem,
    };
    assert_eq!(
        o.route_line("alice", "#nobody", "hi", "ID", &env),
        OutboundDecision::Refuse(RefuseReason::UnknownChannel("#nobody".into()))
    );
}

// ───────────────────────────── Explicit address ─────────────────────────────

#[test]
fn explicit_address_from_elsewhere_overrides_the_channel_and_remembers_private() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:abc")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
    };
    // Typed in the lobby, but explicitly addressed: goes to cc:abc alone.
    let d = o.route_line("alice", "#mu", "cc:abc: hey there", "ID", &env);
    let (_, targets, body, memory) = published(&d);
    assert_eq!(targets, &[PeerId::parse("cc:abc")]);
    assert_eq!(body, "hey there");
    // `#cc-abc` is the AGENT's channel, and alice is not in it — she is in the
    // lobby. Remembering it would aim the reply at a room she never joined, so
    // the memory says private and the answer comes back as a DM.
    assert_eq!(
        memory,
        &Some(MemoryUpdate {
            human: "alice".into(),
            destination: MemoryDestination::Private,
        })
    );
}

#[test]
fn explicit_address_inside_the_peers_own_channel_remembers_that_channel() {
    // The other half of the same rule: the address is typed IN `#cc-abc`, which
    // is where that conversation is happening, so the reply belongs there.
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:abc")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
    };
    let d = o.route_line("alice", "#cc-abc", "cc:abc: hey there", "ID", &env);
    let (_, targets, body, memory) = published(&d);
    assert_eq!(targets, &[PeerId::parse("cc:abc")]);
    assert_eq!(body, "hey there");
    assert_eq!(
        memory,
        &Some(MemoryUpdate {
            human: "alice".into(),
            destination: MemoryDestination::Channel("#cc-abc".into()),
        })
    );
}

#[test]
fn an_explicit_address_in_another_agents_channel_is_private_not_that_channel() {
    // Addressing `cc:abc` while sitting in `#cc-other` must not remember either
    // channel: not `#cc-abc` (alice is not there) and not `#cc-other` (that is
    // not the conversation she directed the line at).
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:abc"), PeerId::parse("cc:other")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
    };
    let d = o.route_line("alice", "#cc-other", "cc:abc: hey there", "ID", &env);
    let (_, targets, _, memory) = published(&d);
    assert_eq!(targets, &[PeerId::parse("cc:abc")]);
    assert_eq!(
        memory,
        &Some(MemoryUpdate {
            human: "alice".into(),
            destination: MemoryDestination::Private,
        })
    );
}

#[test]
fn a_private_explicit_address_to_the_gateway_remembers_private() {
    // `/msg mu-gw cc:abc: …` — there is no channel at all, so there is nothing
    // for a reply to be part of.
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:abc")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
    };
    let d = o.route_line("alice", "mu-gw", "cc:abc: hey there", "ID", &env);
    let (_, targets, _, memory) = published(&d);
    assert_eq!(targets, &[PeerId::parse("cc:abc")]);
    assert_eq!(
        memory,
        &Some(MemoryUpdate {
            human: "alice".into(),
            destination: MemoryDestination::Private,
        })
    );
}

#[test]
fn a_private_address_replaces_a_channel_this_human_had_remembered() {
    // The private record is a WRITE, not an omission: alice talks in `#cc-abc`
    // (remembered), then addresses the same agent from the lobby. The second
    // line must not leave the first line's channel standing, or the reply lands
    // in a room she did not name this time.
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:abc")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
    };
    let first = o.route_line("alice", "#cc-abc", "hi cc", "ID1", &env);
    let (_, _, _, memory) = published(&first);
    assert_eq!(
        memory,
        &Some(MemoryUpdate {
            human: "alice".into(),
            destination: MemoryDestination::Channel("#cc-abc".into()),
        })
    );
    let second = o.route_line("alice", "#mu", "cc:abc: and again", "ID2", &env);
    let (_, _, _, memory) = published(&second);
    assert_eq!(
        memory,
        &Some(MemoryUpdate {
            human: "alice".into(),
            destination: MemoryDestination::Private,
        })
    );
}

#[test]
fn explicit_address_to_an_absent_peer_is_refused() {
    let mut o = out();
    let mem = mem_with_alice();
    let env = OutEnv {
        peers: &[],
        membership: &mem,
    };
    assert_eq!(
        o.route_line("alice", "#mu", "cc:ghost: hi", "ID", &env),
        OutboundDecision::Refuse(RefuseReason::AbsentDestination(PeerId::parse("cc:ghost")))
    );
}

#[test]
fn a_peer_sharing_a_dm_subject_is_not_a_discovered_peer() {
    // `mu:d:s` (daemon `d`, session `s`) and `mu:d.s` (daemon `d.s`) are
    // different identities that derive the same DM subject,
    // `mu.agent.mu.d.s.dm`. Only the first is present, so the second is absent
    // however its subject spells out.
    assert_eq!(
        PeerId::parse("mu:d:s").dm_subject(),
        PeerId::parse("mu:d.s").dm_subject()
    );

    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("mu:d:s")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
    };
    let d = o.route_line("alice", "#mu", "mu:d.s: hello", "ID", &env);
    // Refused as absent — and a refusal carries no MemoryUpdate at all, so
    // nothing remembers that `alice` addressed `#mu-d.s`.
    assert_eq!(
        d,
        OutboundDecision::Refuse(RefuseReason::AbsentDestination(PeerId::parse("mu:d.s")))
    );

    // The discovered spelling still routes, so this is a tightening of identity,
    // not a refusal of the present peer.
    let d = o.route_line("alice", "#mu", "mu:d:s: hello", "ID2", &env);
    let (_, targets, body, _) = published(&d);
    assert_eq!(targets, &[PeerId::parse("mu:d:s")]);
    assert_eq!(body, "hello");
}

#[test]
fn explicit_human_address_is_refused() {
    let mut o = out();
    let mem = mem_with_alice();
    let env = OutEnv {
        peers: &[],
        membership: &mem,
    };
    assert_eq!(
        o.route_line("alice", "#mu", "human:bob: hi", "ID", &env),
        OutboundDecision::Refuse(RefuseReason::HumanDestination)
    );
}

#[test]
fn ordinary_text_with_a_colon_is_not_an_explicit_address() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:a")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
    };
    // "note: something" — `note` is no agent role, so this fans out as ordinary
    // lobby text, body intact.
    let d = o.route_line("alice", "#mu", "note: buy milk", "ID", &env);
    let (_, _, body, _) = published(&d);
    assert_eq!(body, "note: buy milk");
}

// ───────────────────────────── Private senders ──────────────────────────────

#[test]
fn a_private_line_from_a_nonmember_is_refused() {
    let mut o = out();
    let mem = empty_mem(); // alice is present nowhere
    let peers = vec![PeerId::parse("cc:a")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
    };
    assert_eq!(
        o.route_line("alice", "mu-gw", "hi", "ID", &env),
        OutboundDecision::Refuse(RefuseReason::UnauthorizedSender("alice".into()))
    );
}

#[test]
fn a_private_line_from_a_member_fans_out() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:a")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
    };
    let d = o.route_line("alice", "mu-gw", "hi", "ID", &env);
    let (_, targets, _, memory) = published(&d);
    assert_eq!(targets.len(), 1);
    assert!(
        memory.is_none(),
        "a private line is not specifically addressed"
    );
}

// ─────────────────────────────── Loop guard 2 ───────────────────────────────

#[test]
fn is_own_echo_recognises_minted_ids_and_human_senders() {
    let o = out();
    // A human sender is always the gateway's own publication.
    assert!(o.is_own_echo(&event("x", "human:alice")));
    // An agent-origin id we never minted is not.
    assert!(!o.is_own_echo(&event("never-minted", "cc:y")));
}

#[test]
fn a_minted_id_is_recorded_before_the_publish_can_be_observed() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:a")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
    };
    // The decision records the id as part of producing the Publish. By the time
    // the caller holds the decision — before it can publish — the observer path
    // would already recognise the echo, so a fast round-trip cannot loop.
    let d = o.route_line("alice", "#mu", "hello", "RACE", &env);
    assert!(matches!(d, OutboundDecision::Publish { .. }));
    assert!(
        o.is_own_echo(&event("RACE", "cc:z")),
        "the minted id must be guarded the instant the decision exists"
    );
    // A refused/dropped line mints nothing.
    let d = o.route_line("alice", "#nobody", "hi", "NOPE", &env);
    assert!(matches!(d, OutboundDecision::Refuse(_)));
    assert!(!o.is_own_echo(&event("NOPE", "cc:z")));
}

#[test]
fn reset_forgets_minted_ids() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:a")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
    };
    o.route_line("alice", "#mu", "hi", "GONE", &env);
    assert!(o.is_own_echo(&event("GONE", "cc:z")));
    o.reset();
    assert!(!o.is_own_echo(&event("GONE", "cc:z")));
}

// ────────────── Authorization, self-nick folding, bounded guard ─────────────

#[test]
fn an_explicit_address_cannot_route_around_sender_authorization() {
    // The gateway publishes as `human:<nick>` under its own mesh capability, so
    // it must observe that identity before asserting it. The check used to live
    // only on the private path, which the explicit-address branch returned
    // before ever reaching — so `cc:abc: hi` from a nick the gateway sees
    // nowhere was published while the same nick's plain `hi` was refused. A
    // prefix must not select a weaker gate.
    let mut o = out();
    let mem = empty_mem(); // outsider is present nowhere
    let peers = vec![PeerId::parse("cc:abc")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
    };
    let plain = o.route_line("outsider", "mu-gw", "hi", "ID1", &env);
    let addressed = o.route_line("outsider", "mu-gw", "cc:abc: hi", "ID2", &env);
    assert_eq!(
        plain,
        OutboundDecision::Refuse(RefuseReason::UnauthorizedSender("outsider".into()))
    );
    assert_eq!(
        addressed, plain,
        "an explicit address was treated more permissively than plain text"
    );
    // The same holds for a channel line and for the lobby fan-out: one rule.
    for (target, text) in [
        ("#cc-abc", "hi"),
        ("#cc-abc", "cc:abc: hi"),
        ("#mu", "hello all"),
        ("#mu", "cc:abc: hello"),
    ] {
        assert_eq!(
            o.route_line("outsider", target, text, "ID3", &env),
            OutboundDecision::Refuse(RefuseReason::UnauthorizedSender("outsider".into())),
            "target {target:?} text {text:?}"
        );
    }
    // Nothing was minted for any refused line, so no loop guard was primed by a
    // sender the gateway could not authorize.
    for id in ["ID1", "ID2", "ID3"] {
        assert!(!o.is_own_echo(&event(id, "cc:abc")), "id {id} was minted");
    }
    // An observed sender is unaffected: the same explicit address publishes.
    let mem = mem_with_alice();
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
    };
    let d = o.route_line("alice", "mu-gw", "cc:abc: hi", "OK", &env);
    let (_, targets, body, _) = published(&d);
    assert_eq!(targets, [PeerId::parse("cc:abc")]);
    assert_eq!(body, "hi");
}

#[test]
fn the_own_nick_guard_survives_a_casemapping_change() {
    // `[` folds to `{` under rfc1459 and not at all under ascii. Re-folding the
    // already-folded self nick left the gateway believing it was `gw{` under
    // ascii, so its own echoed line no longer matched loop guard 1 and could be
    // republished to the mesh — exactly the echo the guard exists to stop.
    let mut o = Outbound::new("gw[", CaseMapping::Rfc1459, "#", "#mu", 50);
    assert_eq!(o.self_nick(), "gw[");
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:a")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
    };
    assert_eq!(
        o.route_line("gw[", "#mu", "echo", "ID", &env),
        OutboundDecision::Drop(OutDrop::OwnNick)
    );
    o.set_casemapping(CaseMapping::Ascii, "#mu");
    assert_eq!(o.self_nick(), "gw[", "the wire spelling must not change");
    assert_eq!(
        o.route_line("gw[", "#mu", "echo", "ID", &env),
        OutboundDecision::Drop(OutDrop::OwnNick),
        "the gateway stopped recognizing its own nick after the mapping change"
    );
    // A self-rename tracks the new wire nick, and survives the next change too.
    o.set_self_nick("gw]");
    o.set_casemapping(CaseMapping::Rfc1459, "#mu");
    assert_eq!(o.self_nick(), "gw]");
    assert_eq!(
        o.route_line("gw]", "#mu", "echo", "ID", &env),
        OutboundDecision::Drop(OutDrop::OwnNick)
    );
}

#[test]
fn the_minted_id_guard_is_bounded_and_still_catches_recent_echoes() {
    // Every published id used to be retained for the life of the connection, so
    // a busy gateway grew the set with total traffic. It is now the same
    // fixed-capacity window the mesh→IRC overlap dedup uses: oldest-first
    // eviction, with everything inside the window still recognized.
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:a")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
    };
    o.route_line("alice", "#mu", "first", "id-0", &env);
    assert!(
        o.is_own_echo(&event("id-0", "cc:a")),
        "recording must precede any observation"
    );
    let cap = mu_irc_gateway::recent::DEFAULT_CAPACITY;
    for i in 1..=cap {
        o.route_line("alice", "#mu", "more", &format!("id-{i}"), &env);
    }
    assert!(
        !o.is_own_echo(&event("id-0", "cc:a")),
        "the oldest minted id was never evicted, so the set is unbounded"
    );
    assert!(
        o.is_own_echo(&event(&format!("id-{cap}"), "cc:a")),
        "a recent minted id escaped the window"
    );
    // The `human:` half of loop guard 2 is independent of the window.
    assert!(o.is_own_echo(&event("never-minted", "human:alice")));
}

#[test]
fn a_live_channellen_change_is_followed_by_channel_resolution() {
    // CHANNELLEN used to be frozen at construction while the adapter tracked
    // it live and mesh→IRC routing read it fresh per call: after an `005`
    // raised the limit, the server-derived channel for a long peer id was
    // refused here as unknown, and routing memory kept the stale hashed name.
    let long_id = format!("cc:{}", "f".repeat(60));
    let peer = PeerId::parse(&long_id);
    let short_ch = channel_for(&peer, "#", 50).expect("agent peers get a channel");
    let long_ch = channel_for(&peer, "#", 500).expect("agent peers get a channel");
    assert_ne!(short_ch, long_ch, "the limit must actually change the name");

    let mut o = Outbound::new("mu-gw", RFC, "#", "#mu", 50);
    let mem = mem_with_alice();
    let peers = vec![peer.clone()];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
    };
    assert!(
        matches!(
            o.route_line("alice", &long_ch, "hi", "ID2", &env),
            OutboundDecision::Refuse(RefuseReason::UnknownChannel(_))
        ),
        "under the old limit the long name is not a known channel"
    );

    o.set_channellen(500);
    let after = o.route_line("alice", &long_ch, "hi", "ID3", &env);
    assert!(
        !matches!(
            after,
            OutboundDecision::Refuse(RefuseReason::UnknownChannel(_))
        ),
        "the new limit's channel must resolve: {after:?}"
    );
    assert!(
        matches!(
            o.route_line("alice", &short_ch, "hi", "ID4", &env),
            OutboundDecision::Refuse(RefuseReason::UnknownChannel(_))
        ),
        "the old hashed name is no longer a channel the server derives"
    );
}
