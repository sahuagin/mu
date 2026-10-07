//! Offline IRC→mesh tests for increments 3 and 4: intent classification,
//! destination checks, fan-out, routing-memory rules, both loop guards —
//! including the publish-then-observe race — and the bot verbs dispatched ahead
//! of all of it. No socket, no live mesh.

use mu_dialogue::mesh::{MeshDmEvent, Reception};
use mu_irc_gateway::config::PuppetsConfig;
use mu_irc_gateway::mapping::{channel_for, CaseMapping};
use mu_irc_gateway::membership::Membership;
use mu_irc_gateway::outbound::{
    CommandReply, MemoryDestination, MemoryUpdate, OutDrop, OutEnv, Outbound, OutboundDecision,
    RefuseReason, NO_AGENTS, USAGE,
};
use mu_irc_gateway::puppets::Pool;
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
        puppets: None,
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
        puppets: None,
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
        puppets: None,
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
        puppets: None,
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
        puppets: None,
    };
    let d = o.route_line("alice", "#cc-abc", "hi cc", "ID", &env);
    let (_, targets, body, memory) = published(&d);
    assert_eq!(targets, &[PeerId::parse("cc:abc")]);
    assert_eq!(body, "hi cc");
    assert_eq!(
        memory,
        &Some(MemoryUpdate {
            human: "alice".into(),
            agent: PeerId::parse("cc:abc"),
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
        puppets: None,
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
        puppets: None,
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
        puppets: None,
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
            agent: PeerId::parse("cc:abc"),
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
        puppets: None,
    };
    let d = o.route_line("alice", "#cc-abc", "cc:abc: hey there", "ID", &env);
    let (_, targets, body, memory) = published(&d);
    assert_eq!(targets, &[PeerId::parse("cc:abc")]);
    assert_eq!(body, "hey there");
    assert_eq!(
        memory,
        &Some(MemoryUpdate {
            human: "alice".into(),
            agent: PeerId::parse("cc:abc"),
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
        puppets: None,
    };
    let d = o.route_line("alice", "#cc-other", "cc:abc: hey there", "ID", &env);
    let (_, targets, _, memory) = published(&d);
    assert_eq!(targets, &[PeerId::parse("cc:abc")]);
    assert_eq!(
        memory,
        &Some(MemoryUpdate {
            human: "alice".into(),
            agent: PeerId::parse("cc:abc"),
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
        puppets: None,
    };
    let d = o.route_line("alice", "mu-gw", "cc:abc: hey there", "ID", &env);
    let (_, targets, _, memory) = published(&d);
    assert_eq!(targets, &[PeerId::parse("cc:abc")]);
    assert_eq!(
        memory,
        &Some(MemoryUpdate {
            human: "alice".into(),
            agent: PeerId::parse("cc:abc"),
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
        puppets: None,
    };
    let first = o.route_line("alice", "#cc-abc", "hi cc", "ID1", &env);
    let (_, _, _, memory) = published(&first);
    assert_eq!(
        memory,
        &Some(MemoryUpdate {
            human: "alice".into(),
            agent: PeerId::parse("cc:abc"),
            destination: MemoryDestination::Channel("#cc-abc".into()),
        })
    );
    let second = o.route_line("alice", "#mu", "cc:abc: and again", "ID2", &env);
    let (_, _, _, memory) = published(&second);
    assert_eq!(
        memory,
        &Some(MemoryUpdate {
            human: "alice".into(),
            agent: PeerId::parse("cc:abc"),
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
        puppets: None,
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
        puppets: None,
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
        puppets: None,
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
        puppets: None,
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
        puppets: None,
    };
    assert_eq!(
        o.route_line("alice", "mu-gw", "hi", "ID", &env),
        OutboundDecision::Refuse(RefuseReason::UnauthorizedSender("alice".into()))
    );
}

#[test]
fn a_private_line_to_the_gateway_names_no_agent() {
    // Ruling C: the lobby is the fan-out. A private line to the gateway's own
    // nick that reached every agent was the first misfire the operator hit, so
    // it is refused with the addresses that do work.
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:a")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    assert_eq!(
        o.route_line("alice", "mu-gw", "hi", "ID", &env),
        OutboundDecision::Refuse(RefuseReason::PrivateToGateway)
    );
    // The lobby still fans out.
    let d = o.route_line("alice", "#mu", "hi", "ID2", &env);
    let (_, targets, _, memory) = published(&d);
    assert_eq!(targets.len(), 1);
    assert!(memory.is_none(), "a fan-out is not specifically addressed");
}

// ────────────────────────── A puppet nick is an address ─────────────────────

/// A pool with puppets on and no age gate, so a test can drive one peer
/// through to whichever state it wants to assert about.
fn live_pool() -> Pool {
    Pool::new(
        PuppetsConfig {
            enabled: true,
            min_age_secs: 0,
            ..PuppetsConfig::default()
        },
        32,
        RFC,
    )
}

/// Listed and in conversation: what a peer has to be before the pool dials it.
fn offered(pool: &mut Pool, peer: &PeerId) {
    pool.observe(std::slice::from_ref(peer), 0);
    pool.touch(peer, 0);
    pool.tick(0);
}

/// A pool in which `peer` holds `nick`, as the live one would be after the
/// server accepted its registration.
fn holding(nick: &str, peer: &str) -> Pool {
    let mut pool = live_pool();
    let p = PeerId::parse(peer);
    offered(&mut pool, &p);
    pool.registered(&p, nick, 0);
    assert_eq!(pool.nick_of(&p), Some(nick), "the fixture registered");
    pool
}

#[test]
fn a_puppet_nick_addresses_the_agent_holding_it() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:abc")];
    let pool = holding("cc-1", "cc:abc");
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: Some(&pool),
    };
    // In the lobby: the nick reaches the same peer its id would, and the reply
    // belongs privately (the lobby is not that agent's channel).
    let d = o.route_line("alice", "#mu", "cc-1: by your nick", "ID", &env);
    let (_, targets, body, memory) = published(&d);
    assert_eq!(targets, &[PeerId::parse("cc:abc")]);
    assert_eq!(body, "by your nick");
    assert_eq!(
        memory,
        &Some(MemoryUpdate {
            human: "alice".into(),
            agent: PeerId::parse("cc:abc"),
            destination: MemoryDestination::Private,
        })
    );
    // Folding follows the server's rule, like every other nick comparison.
    let d = o.route_line("alice", "#mu", "CC-1: upper", "ID2", &env);
    assert_eq!(published(&d).1, &[PeerId::parse("cc:abc")]);
    // A nick nobody holds is REFUSED rather than fanned out to the room.
    // Operator's rule: IRC itself answers "No such nick" for a nick that is
    // not connected, so a session-shaped name resolving to nobody should not
    // be addressable. Before this it fell through to the lobby and went to
    // every agent — not where the human aimed it (mu-t8im0).
    let d = o.route_line("alice", "#mu", "cc-9: nobody", "ID3", &env);
    match &d {
        OutboundDecision::Refuse(RefuseReason::UnknownPeer(named)) => {
            assert_eq!(named, "cc-9", "the refusal quotes what was typed");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn without_puppets_a_nick_names_nobody_and_is_refused() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:abc")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    // With puppets off nothing holds `cc-1`, and no live peer's id or alias
    // begins with it, so it names nobody. It is session-shaped, so it is
    // refused rather than said to the room (operator's rule, mu-t8im0).
    let d = o.route_line("alice", "#mu", "cc-1: hi", "ID", &env);
    assert!(
        matches!(d, OutboundDecision::Refuse(RefuseReason::UnknownPeer(_))),
        "got {d:?}"
    );
}

/// The line the operator actually broadcasts with: no address token at all.
/// Refusing unmatched addresses must not touch this path, because being the
/// only human logged in, a line to the room IS how he reaches everyone.
#[test]
fn a_bare_lobby_line_still_broadcasts_to_every_agent() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![
        PeerId::parse("cc:abc"),
        PeerId::parse("mu:d5:session-1"),
        PeerId::parse("cc:def"),
    ];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    let d = o.route_line("alice", "#mu", "heads up everyone", "ID", &env);
    let (_, targets, body, _) = published(&d);
    assert_eq!(targets.len(), 3, "every agent: {targets:?}");
    assert_eq!(body, "heads up everyone");
}

/// And ordinary prose that merely contains a colon is not an address, so it is
/// said in the room rather than refused.
#[test]
fn prose_with_a_colon_is_not_an_address_and_is_still_said() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:abc")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    let d = o.route_line("alice", "#mu", "Note: deploy is green", "ID", &env);
    assert_eq!(published(&d).2, "Note: deploy is green");
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
        puppets: None,
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
        puppets: None,
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
        puppets: None,
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
        puppets: None,
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
        puppets: None,
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
        puppets: None,
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
        puppets: None,
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

// ────────────────────────── Bot verbs (increment 4) ─────────────────────────

/// Destructure a bot-verb answer.
fn replied(d: &OutboundDecision) -> &CommandReply {
    match d {
        OutboundDecision::Reply(reply) => reply,
        other => panic!("expected a command Reply, got {other:?}"),
    }
}

/// The rendered roster of a `mu peers` answer.
fn roster(d: &OutboundDecision) -> Vec<String> {
    match replied(d) {
        CommandReply::Peers(lines) => lines.clone(),
        other => panic!("expected a peers listing, got {other:?}"),
    }
}

/// The reason a `mu say` was refused.
fn command_refusal(d: &OutboundDecision) -> RefuseReason {
    match replied(d) {
        CommandReply::Refused(reason) => reason.clone(),
        other => panic!("expected a refused command, got {other:?}"),
    }
}

/// `mu say <nick>` and `<nick>:` are one question asked two ways and must
/// reach the same agent. They did not: the `mu say` path never consulted
/// puppet nicks, so it fell past the exact forms into prefix matching and
/// could land on a different live peer whose alias merely began with the
/// nick — one an agent already owns (run 4).
#[test]
fn mu_say_and_an_address_resolve_a_puppet_nick_to_the_same_peer() {
    let mut o = out();
    let mem = mem_with_alice();
    let owner = PeerId::parse("cc:abc");
    // Its alias is `cc-12345678`, which begins with `cc-1` — the nick below.
    let decoy = PeerId::parse("cc:12345678");
    let peers = vec![owner.clone(), decoy];
    let pool = holding("cc-1", "cc:abc");
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: Some(&pool),
    };
    let viasay = o.route_line("alice", "#mu", "mu say cc-1 hello", "ID1", &env);
    assert_eq!(
        published(&viasay).1,
        std::slice::from_ref(&owner),
        "mu say reaches the nick's owner"
    );
    let viaaddr = o.route_line("alice", "#mu", "cc-1: hello", "ID2", &env);
    assert_eq!(
        published(&viaaddr).1,
        std::slice::from_ref(&owner),
        "and so does the address form"
    );
}

/// A prefix abbreviates one component; it never climbs the hierarchy. mu ids
/// are `mu:<daemon>[:<session>]`, so an absent daemon must be reported absent
/// rather than resolving to its live child — a different peer, picked by who
/// happens to be online (run 4).
#[test]
fn an_absent_daemon_does_not_resolve_to_its_live_session() {
    let mut o = out();
    let mem = mem_with_alice();
    let daemon = PeerId::parse("mu:d5");
    let child = PeerId::parse("mu:d5:session-1");
    // Only the child is live.
    let peers = vec![child];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    let d = o.route_line("alice", "#mu", "mu:d5: for the daemon", "ID", &env);
    match &d {
        OutboundDecision::Refuse(RefuseReason::AbsentDestination(p)) => {
            assert_eq!(*p, daemon, "the daemon is named back, not silently swapped");
        }
        other => panic!("expected absent, got {other:?}"),
    }
}

/// Without the glob a token is matched EXACTLY, so a name that is merely the
/// beginning of a live peer's id resolves to nobody rather than to that peer.
/// This is what makes the two questions non-overlapping, and it is why an
/// absent daemon can no longer be answered by its live child session.
#[test]
fn without_the_glob_a_partial_name_is_not_expanded() {
    let mut o = out();
    let mem = mem_with_alice();
    let long = PeerId::parse("cc:c689911a-1111-2222");
    let peers = vec![long];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    let d = o.route_line("alice", "#mu", "cc:c689911a: no glob", "ID", &env);
    match &d {
        OutboundDecision::Refuse(RefuseReason::AbsentDestination(p)) => {
            assert_eq!(p.to_string(), "cc:c689911a", "named back, not expanded");
        }
        other => panic!("expected absent, got {other:?}"),
    }
}

/// A bare `*` declared a name and gave nothing to match. It must be refused,
/// not re-read as prose: falling through puts the line in front of every agent
/// in the lobby, which is the opposite of an unsupported destination failing.
#[test]
fn a_bare_glob_with_nothing_before_it_is_refused() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:abc"), PeerId::parse("mu:d5:session-1")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    let d = o.route_line("alice", "#mu", "*: do the task", "ID", &env);
    assert!(
        matches!(d, OutboundDecision::Refuse(RefuseReason::UnknownPeer(_))),
        "a bare glob must not fan out: got {d:?}"
    );
}

/// A glob that answers to nobody is refused rather than re-read as ordinary
/// text: the caller said it was a name.
#[test]
fn a_glob_that_matches_nothing_is_refused() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:abc")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    let d = o.route_line("alice", "#mu", "cc:zzzz*: nobody", "ID", &env);
    assert!(
        matches!(d, OutboundDecision::Refuse(RefuseReason::UnknownPeer(_))),
        "got {d:?}"
    );
}

/// And a glob IS allowed to cross the mu id hierarchy, because that is what
/// was asked for. `mu:d5*` means "everything beginning with mu:d5", so the
/// live child session is a legitimate answer — where bare `mu:d5` is exact
/// and reports the daemon absent.
#[test]
fn a_glob_may_reach_a_child_session_because_it_was_asked_for() {
    let mut o = out();
    let mem = mem_with_alice();
    let child = PeerId::parse("mu:d5:session-1");
    let peers = vec![child.clone()];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    let d = o.route_line("alice", "#mu", "mu:d5*: for whoever", "ID", &env);
    assert_eq!(published(&d).1, std::slice::from_ref(&child));
}

// ──────────────── A puppet query resolves names the same way ────────────────

/// A line in one agent's query buffer that NAMES another agent must reach the
/// one named. Before run 3's finding this path understood only puppet nicks,
/// so a glob was treated as ordinary text and the whole line — including what
/// looked like an address — was delivered to the agent being queried.
#[test]
fn a_puppet_query_delivers_a_prefixed_address_to_the_named_peer() {
    let mut o = out();
    let mem = mem_with_alice();
    let queried = PeerId::parse("cc:abc");
    let other = PeerId::parse("cc:c689911a-1111-2222");
    let peers = vec![queried.clone(), other.clone()];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    let d = o.route_puppet_private(
        "alice",
        queried,
        "cc:c689911a*: for the other one",
        "ID",
        &env,
    );
    let (_, targets, body, _) = published(&d);
    assert_eq!(
        targets,
        std::slice::from_ref(&other),
        "the named peer, not the queried one: {targets:?}"
    );
    assert_eq!(body, "for the other one");
}

/// And a name answering to nobody is refused there too, rather than delivered
/// to whichever agent happened to be queried.
#[test]
fn a_puppet_query_refuses_a_name_that_answers_to_nobody() {
    let mut o = out();
    let mem = mem_with_alice();
    let queried = PeerId::parse("cc:abc");
    let peers = vec![queried.clone()];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    let d = o.route_puppet_private("alice", queried, "cc-zzzz9999: private task", "ID", &env);
    assert!(
        matches!(d, OutboundDecision::Refuse(RefuseReason::UnknownPeer(_))),
        "got {d:?}"
    );
}

/// An UNADDRESSED query carries a destination the gateway already resolved —
/// the puppet's own peer — and that must be validated exactly. Expanding it
/// like a typed token would send a query for a peer that has left discovery
/// to a DIFFERENT live peer whose id merely extends it (run 3, high).
#[test]
fn an_unaddressed_query_for_a_departed_peer_is_absent_not_redirected() {
    let mut o = out();
    let mem = mem_with_alice();
    let departed = PeerId::parse("cc:abc");
    let extends_it = PeerId::parse("cc:abcdef");
    // Only the longer peer is live; the queried one has left discovery.
    let peers = vec![extends_it];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    let d = o.route_puppet_private("alice", departed.clone(), "still there?", "ID", &env);
    match &d {
        OutboundDecision::Refuse(RefuseReason::AbsentDestination(p)) => {
            assert_eq!(*p, departed, "named back as the peer that is gone");
        }
        other => panic!("a resolved identity must not be expanded: got {other:?}"),
    }
}

// ───────────────── Addressing a session by a unique prefix ─────────────────

/// The case the operator hit: an 8-character prefix of a peer id, which used
/// to answer "not on the mesh right now" because only the FULL id matched.
/// The trailing `*` is the caller saying "this is a prefix" — without it the
/// token is matched exactly, so the two questions never overlap. Both the id
/// form and the channel-alias form resolve.
#[test]
fn a_unique_prefix_of_a_peer_id_or_alias_reaches_that_peer() {
    let mut o = out();
    let mem = mem_with_alice();
    let peer = PeerId::parse("cc:c689911a-1111-2222-3333-444455556666");
    let peers = vec![peer.clone(), PeerId::parse("mu:d5:session-1")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    // The id form, explicitly addressed.
    let d = o.route_line("alice", "#mu", "cc:c689911a*: by prefix", "ID1", &env);
    let (_, targets, body, _) = published(&d);
    assert_eq!(
        targets,
        std::slice::from_ref(&peer),
        "the prefix named exactly one peer"
    );
    assert_eq!(body, "by prefix");
    // The channel-alias form, which reaches us as ordinary text rather than a
    // peer id and so takes a different branch.
    let d = o.route_line("alice", "#mu", "cc-c689911a*: by alias prefix", "ID2", &env);
    assert_eq!(published(&d).1, std::slice::from_ref(&peer));
    // And `mu say`, which is the other way the operator types a destination.
    let d = o.route_line("alice", "#mu", "mu say cc:c689911a* hello", "ID3", &env);
    assert_eq!(published(&d).1, &[peer]);
}

/// A glob two peers answer to is a question only the human can settle, so the
/// gateway lists them and delivers nothing — the rule ambiguous aliases
/// already follow.
#[test]
fn a_prefix_matching_two_peers_lists_them_and_publishes_nothing() {
    let mut o = out();
    let mem = mem_with_alice();
    let a = PeerId::parse("cc:c689911a-aaaa");
    let b = PeerId::parse("cc:c689911b-bbbb");
    let peers = vec![a.clone(), b.clone()];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    let d = o.route_line("alice", "#mu", "cc:c6899*: which one", "ID", &env);
    match &d {
        OutboundDecision::Refuse(RefuseReason::AmbiguousPeer(named)) => {
            assert_eq!(named.len(), 2, "both candidates are named back: {named:?}");
        }
        other => panic!("expected an ambiguity refusal, got {other:?}"),
    }
    assert!(
        matches!(d, OutboundDecision::Refuse(_)),
        "an ambiguous prefix delivers to nobody"
    );
}

/// An EXACT id wins outright, even when it is also a prefix of another live
/// peer. Otherwise adding a longer-named session would silently break
/// addressing for the shorter one.
#[test]
fn an_exact_id_beats_a_prefix_of_a_longer_one() {
    let mut o = out();
    let mem = mem_with_alice();
    let short = PeerId::parse("cc:abc");
    let long = PeerId::parse("cc:abcdef");
    let peers = vec![short.clone(), long.clone()];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    let d = o.route_line("alice", "#mu", "cc:abc: exact wins", "ID", &env);
    assert_eq!(
        published(&d).1,
        &[short],
        "cc:abc is a whole peer id and must not read as ambiguous"
    );
}

/// The alias form needs the same exact-wins rule the id form has. `cc-abc` is
/// one peer's WHOLE alias and also a prefix of `cc-abcdef`'s; the whole alias
/// must win, or adding a longer-named session breaks addressing the shorter.
#[test]
fn an_exact_alias_beats_a_prefix_of_a_longer_one() {
    let mut o = out();
    let mem = mem_with_alice();
    let short = PeerId::parse("cc:abc");
    let long = PeerId::parse("cc:abcdef");
    let peers = vec![short.clone(), long];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    let d = o.route_line("alice", "#mu", "cc-abc: exact alias wins", "ID", &env);
    assert_eq!(
        published(&d).1,
        std::slice::from_ref(&short),
        "cc-abc is a whole alias and must not read as ambiguous"
    );
}

/// An alias is case-FOLDED, so two distinct peers can answer to one. Picking
/// either would send a line aimed at one session to whichever came first in
/// the discovery snapshot — silently. `mu say` refuses that and `mu peers`
/// marks the channel `(shared)`; the explicit-address path has to agree.
#[test]
fn two_peers_folding_to_one_alias_are_refused_not_silently_picked() {
    let mut o = out();
    let mem = mem_with_alice();
    // Under RFC1459 folding these are two peers with one alias, `cc-abc`.
    let lower = PeerId::parse("cc:abc");
    let upper = PeerId::parse("cc:ABC");
    let peers = vec![lower, upper];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    let d = o.route_line("alice", "#mu", "cc-abc: private task", "ID", &env);
    match &d {
        OutboundDecision::Refuse(RefuseReason::AmbiguousPeer(named)) => {
            assert_eq!(named.len(), 2, "both are named back: {named:?}");
        }
        other => panic!("expected an ambiguity refusal, got {other:?}"),
    }
    assert!(
        !matches!(d, OutboundDecision::Publish { .. }),
        "a line aimed at one session must not land on a coin flip"
    );
}

/// The guard that keeps this out of ordinary prose: a token needs a role, a
/// separator AND id characters before a prefix is read from it. A line opening
/// `mu: ` is someone talking, not someone addressing every mu session.
#[test]
fn a_bare_role_is_not_a_prefix_and_stays_ordinary_text() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![
        PeerId::parse("mu:d5:session-1"),
        PeerId::parse("mu:d6:session-1"),
    ];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    // Two mu peers are live, so a prefix reading of "mu" would be ambiguous.
    // It must fan out to the lobby as the ordinary line it is.
    let d = o.route_line("alice", "#mu", "mu: look at this", "ID", &env);
    let (_, targets, body, _) = published(&d);
    assert_eq!(
        targets.len(),
        2,
        "said in the room, not refused: {targets:?}"
    );
    assert_eq!(body, "mu: look at this", "the text is unchanged");
}

/// A prefix nothing answers to behaves exactly as before: named back as absent
/// when it is peer-shaped, so the human learns the session is not here rather
/// than that their typing was wrong.
#[test]
fn a_prefix_matching_nothing_is_still_reported_absent() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:abc")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    let d = o.route_line("alice", "#mu", "cc:zzzz9999: nobody home", "ID", &env);
    match &d {
        OutboundDecision::Refuse(RefuseReason::AbsentDestination(p)) => {
            assert_eq!(p.to_string(), "cc:zzzz9999");
        }
        other => panic!("expected an absent refusal, got {other:?}"),
    }
}

#[test]
fn mu_peers_lists_every_present_agent_with_its_full_id_and_channel() {
    let mut o = out();
    let mem = mem_with_alice();
    // A session-level peer, a SESSIONLESS daemon, and a cc session: the daemon
    // is a peer like any other here and must not be filtered out for having no
    // session.
    let peers = vec![
        PeerId::parse("cc:abc"),
        PeerId::parse("mu:d"),
        PeerId::parse("mu:d:s"),
    ];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    // Typed in the lobby — where an ordinary line would fan out to all three.
    let lines = roster(&o.route_line("alice", "#mu", "mu peers", "ID", &env));
    assert!(lines[0].contains("3 agents"), "{lines:?}");
    assert_eq!(lines.len(), 4, "a header and one line per peer: {lines:?}");
    for (peer, channel) in [
        ("cc:abc", "#cc-abc"),
        ("mu:d", "#mu-d"),
        ("mu:d:s", "#mu-d-s"),
    ] {
        assert!(
            lines
                .iter()
                .any(|l| l.contains(peer) && l.contains(channel)),
            "{peer} and {channel} must be on one line: {lines:?}"
        );
    }
}

#[test]
fn mu_peers_marks_a_shared_channel_and_leaves_humans_out() {
    let mut o = out();
    let mem = mem_with_alice();
    // Two peers whose channels differ only by case fold onto ONE channel, which
    // is what `(shared)` says; the human is not a mesh destination and is not a
    // roster entry either.
    let peers = vec![
        PeerId::parse("cc:abc"),
        PeerId::parse("cc:ABC"),
        PeerId::human("alice"),
    ];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    let lines = roster(&o.route_line("alice", "mu-gw", "mu peers", "ID", &env));
    assert!(lines[0].contains("2 agents"), "{lines:?}");
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(
        lines[1..].iter().all(|l| l.contains("(shared)")),
        "both peers fold onto one channel: {lines:?}"
    );
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("human:") || l.contains("alice")),
        "humans are not roster entries: {lines:?}"
    );
}

#[test]
fn mu_peers_names_the_nick_a_puppet_holds() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:abc")];
    let pool = holding("cc-1", "cc:abc");
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: Some(&pool),
    };
    let lines = roster(&o.route_line("alice", "#mu", "mu peers", "ID", &env));
    assert_eq!(lines.len(), 2, "{lines:?}");
    // The nick is a second address for the same agent — the one a client can
    // /query — so the roster prints it beside the id and the channel.
    assert!(lines[1].contains("cc:abc"), "{lines:?}");
    assert!(lines[1].contains("#cc-abc"), "{lines:?}");
    assert!(lines[1].contains("nick cc-1"), "{lines:?}");
}

#[test]
fn mu_peers_says_why_a_channel_only_peer_holds_no_nick() {
    let mut o = out();
    let mem = mem_with_alice();
    let peer = PeerId::parse("cc:abc");
    let peers = vec![peer.clone()];
    let mut pool = live_pool();
    offered(&mut pool, &peer);
    // 432: the server refused the nick as erroneous. The tailed form uses the
    // same alphabet, so the pool latches the peer channel-only for good.
    pool.nick_rejected(&peer, "432", 0);
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: Some(&pool),
    };
    let lines = roster(&o.route_line("alice", "#mu", "mu peers", "ID", &env));
    assert!(lines[1].contains("no nick:"), "{lines:?}");
    assert!(
        lines[1].contains("the server refused its nick"),
        "the row says WHY, so the human does not diagnose it by waiting: {lines:?}"
    );
    // Still reachable the v0 way, so the channel is still on the row.
    assert!(lines[1].contains("#cc-abc"), "{lines:?}");
}

#[test]
fn mu_peers_says_not_yet_for_a_peer_still_working_toward_a_nick() {
    let mut o = out();
    let mem = mem_with_alice();
    let peer = PeerId::parse("cc:abc");
    let peers = vec![peer.clone()];
    // Observed and dialled, but the server has not accepted the nick yet.
    let mut pool = live_pool();
    offered(&mut pool, &peer);
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: Some(&pool),
    };
    let lines = roster(&o.route_line("alice", "#mu", "mu peers", "ID", &env));
    assert!(lines[1].contains("no nick yet"), "{lines:?}");
}

#[test]
fn mu_peers_says_the_pool_is_full_for_a_peer_denied_a_slot() {
    let mut o = out();
    let mem = mem_with_alice();
    let peer = PeerId::parse("cc:abc");
    let peers = vec![peer.clone()];
    let mut pool = live_pool();
    offered(&mut pool, &peer);
    // The live spillover path: the bridge asked for a slot on Grant::Spillover
    // and the lease was refused. The peer is deliberately NOT latched off — it
    // backs off and asks again — so its STATE says "retrying" and nothing but
    // the recorded refusal can say why.
    pool.no_slot(&peer, 0);
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: Some(&pool),
    };
    let lines = roster(&o.route_line("alice", "#mu", "mu peers", "ID", &env));
    assert!(
        lines[1].contains("the slot pool was full"),
        "a full pool is the one fact an operator can act on; it must not \
         read as an ordinary wait: {}",
        lines[1]
    );
    // The remedy has to be the condition the pool actually tests. A refused
    // peer is re-dialled only once `spoke_since_refusal` holds — activity
    // later than the refusal — so a freed slot alone never revives a silent
    // peer, and saying otherwise would send the operator looking at capacity
    // for a peer that is only waiting to be spoken to.
    assert!(
        lines[1].contains("after its next line"),
        "the row must name the real retry trigger: {}",
        lines[1]
    );
    assert!(
        !lines[1].contains("frees"),
        "\"when a slot frees\" is a remedy that never arrives: {}",
        lines[1]
    );
}

#[test]
fn a_refusal_stops_being_reported_once_the_pool_dials_again() {
    let mut o = out();
    let mem = mem_with_alice();
    let peer = PeerId::parse("cc:abc");
    let peers = vec![peer.clone()];
    let mut pool = live_pool();
    offered(&mut pool, &peer);
    pool.no_slot(&peer, 0);
    // A later line makes it due again and the pool emits a fresh Connect. The
    // peer is now in flight with a slot granted, so reporting the old refusal
    // would describe a state the pool has already left — and if THIS attempt
    // is refused too, no_slot records it again.
    pool.touch(&peer, 10_000);
    pool.tick(10_000);
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: Some(&pool),
    };
    let lines = roster(&o.route_line("alice", "#mu", "mu peers", "ID", &env));
    assert!(
        !lines[1].contains("slot pool"),
        "the pool is dialling it; the refusal is answered: {}",
        lines[1]
    );
    assert!(lines[1].contains("no nick yet"), "{}", lines[1]);
}

#[test]
fn a_registered_puppet_stops_reporting_the_refusal_that_preceded_it() {
    let mut o = out();
    let mem = mem_with_alice();
    let peer = PeerId::parse("cc:abc");
    let peers = vec![peer.clone()];
    let mut pool = live_pool();
    offered(&mut pool, &peer);
    pool.no_slot(&peer, 0);
    // A later line makes it due again, a slot has freed, and it registers.
    pool.touch(&peer, 10_000);
    pool.tick(10_000);
    pool.registered(&peer, "cc-1", 10_000);
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: Some(&pool),
    };
    let lines = roster(&o.route_line("alice", "#mu", "mu peers", "ID", &env));
    assert!(
        lines[1].contains("nick cc-1"),
        "a held nick answers the question the reason stood in for: {}",
        lines[1]
    );
    assert!(
        !lines[1].contains("slot pool"),
        "a stale refusal must not outlive the nick that settled it: {}",
        lines[1]
    );
}

#[test]
fn mu_peers_says_never_not_yet_for_a_peer_ruling_a_excludes() {
    let mut o = out();
    let mem = mem_with_alice();
    // A bare daemon with `daemons` off is channel-only by SHAPE: the pool
    // never tracks it, so it has no state and no nick, and it never will.
    let daemon = PeerId::parse("mu:d5");
    let session = PeerId::parse("mu:d5:s1");
    let peers = vec![daemon.clone(), session.clone()];
    let mut pool = live_pool();
    offered(&mut pool, &session);
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: Some(&pool),
    };
    let lines = roster(&o.route_line("alice", "#mu", "mu peers", "ID", &env));
    let daemon_row = lines
        .iter()
        .find(|l| l.starts_with("mu:d5 "))
        .expect("the daemon is a roster entry like any other peer");
    assert!(
        daemon_row.contains("a daemon with no session"),
        "a peer that will NEVER hold a nick must not read as one still \
         working toward one: {daemon_row}"
    );
    assert!(
        !daemon_row.contains("yet"),
        "\"yet\" would have the human wait for something that is not \
         coming: {daemon_row}"
    );
    // The session beside it is the honest "not yet" case, so the two reasons
    // are visibly different in one roster.
    let session_row = lines
        .iter()
        .find(|l| l.starts_with("mu:d5:s1 "))
        .expect("the session is on the roster");
    assert!(session_row.contains("no nick yet"), "{session_row}");
}

#[test]
fn mu_peers_with_puppets_off_reads_exactly_as_it_did_before_puppets() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:abc")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    let lines = roster(&o.route_line("alice", "#mu", "mu peers", "ID", &env));
    assert_eq!(
        lines[1], "cc:abc — #cc-abc",
        "with no pool there are no nicks to report, and a row saying so would \
         describe a feature that is not running"
    );
}

#[test]
fn mu_peers_on_an_empty_mesh_says_so_and_publishes_nothing() {
    let mut o = out();
    let mem = mem_with_alice();
    let env = OutEnv {
        peers: &[PeerId::human("alice")],
        membership: &mem,
        puppets: None,
    };
    assert_eq!(
        roster(&o.route_line("alice", "#mu", "mu peers", "ID", &env)),
        vec![NO_AGENTS.to_string()],
        "a mesh with only humans on it has no agents to list"
    );
}

#[test]
fn mu_say_to_a_present_peer_publishes_and_remembers_like_an_explicit_address() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:abc")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    // Typed in the lobby: directed, so it remembers — and privately, because
    // `#cc-abc` is not where alice said it.
    let d = o.route_line("alice", "#mu", "mu say cc:abc hello there", "SAY", &env);
    let (id, targets, body, memory) = published(&d);
    assert_eq!(id, "SAY", "one minted id, the caller's");
    assert_eq!(targets, &[PeerId::parse("cc:abc")]);
    assert_eq!(
        body, "hello there",
        "the verb and destination are not the body"
    );
    assert_eq!(
        memory,
        &Some(MemoryUpdate {
            human: "alice".into(),
            agent: PeerId::parse("cc:abc"),
            destination: MemoryDestination::Private,
        })
    );
    assert!(
        o.is_own_echo(&event("SAY", "human:alice")),
        "a verb's publication is loop-guarded like any other"
    );
}

#[test]
fn mu_say_inside_the_peers_own_channel_remembers_that_channel() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:abc")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    let d = o.route_line("alice", "#cc-abc", "mu say cc:abc hello", "ID", &env);
    let (_, targets, _, memory) = published(&d);
    assert_eq!(targets, &[PeerId::parse("cc:abc")]);
    assert_eq!(
        memory,
        &Some(MemoryUpdate {
            human: "alice".into(),
            agent: PeerId::parse("cc:abc"),
            destination: MemoryDestination::Channel("#cc-abc".into()),
        }),
        "the same rule an explicit address follows, from the same code"
    );
}

#[test]
fn mu_say_falls_back_to_the_alias_the_roster_printed() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:abc")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    // `cc-abc` is the alias `#cc-abc` is built from, not a peer id.
    let d = o.route_line("alice", "#mu", "mu say cc-abc hi", "ID", &env);
    let (_, targets, body, _) = published(&d);
    assert_eq!(targets, &[PeerId::parse("cc:abc")]);
    assert_eq!(body, "hi");
    // …and folded, because the human typed it into IRC.
    let d = o.route_line("alice", "#mu", "MU SAY CC-ABC hi", "ID2", &env);
    let (_, targets, _, _) = published(&d);
    assert_eq!(targets, &[PeerId::parse("cc:abc")]);
}

#[test]
fn mu_say_to_an_absent_peer_names_it_and_publishes_nothing() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:abc")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    assert_eq!(
        command_refusal(&o.route_line("alice", "#mu", "mu say cc:gone hi", "ID", &env)),
        RefuseReason::AbsentDestination(PeerId::parse("cc:gone"))
    );
}

#[test]
fn mu_say_to_something_that_is_no_peer_at_all_quotes_what_was_typed() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:abc")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    assert_eq!(
        command_refusal(&o.route_line("alice", "#mu", "mu say wibble hi", "ID", &env)),
        RefuseReason::UnknownPeer("wibble".into())
    );
}

#[test]
fn mu_say_to_an_ambiguous_alias_names_the_colliding_peers() {
    let mut o = out();
    let mem = mem_with_alice();
    // `cc:a:b` and `cc:a-b` are different peers that share the alias `cc-a-b`.
    let peers = vec![PeerId::parse("cc:a:b"), PeerId::parse("cc:a-b")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    match command_refusal(&o.route_line("alice", "#mu", "mu say cc-a-b hi", "ID", &env)) {
        RefuseReason::AmbiguousPeer(peers) => {
            assert!(peers.contains(&PeerId::parse("cc:a:b")), "{peers:?}");
            assert!(peers.contains(&PeerId::parse("cc:a-b")), "{peers:?}");
        }
        other => panic!("expected AmbiguousPeer, got {other:?}"),
    }
    // The full id of either one still resolves: ambiguity is the alias's, not
    // the peers'.
    let d = o.route_line("alice", "#mu", "mu say cc:a:b hi", "ID2", &env);
    let (_, targets, _, _) = published(&d);
    assert_eq!(targets, &[PeerId::parse("cc:a:b")]);
}

#[test]
fn mu_say_to_a_human_is_refused_by_id_and_by_nick() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:abc"), PeerId::human("bob")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    assert_eq!(
        command_refusal(&o.route_line("alice", "#mu", "mu say human:bob hi", "ID", &env)),
        RefuseReason::HumanDestination
    );
    assert_eq!(
        command_refusal(&o.route_line("alice", "#mu", "mu say bob hi", "ID2", &env)),
        RefuseReason::HumanDestination,
        "the alias of a fronted human is still a human"
    );
}

#[test]
fn an_unsupported_verb_replies_with_usage_and_publishes_nothing() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:abc")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    for line in [
        "mu wat",
        "mu peers now",  // the verb takes no argument
        "mu say cc:abc", // a destination with nothing to say
        "mu say",
    ] {
        assert_eq!(
            replied(&o.route_line("alice", "#mu", line, "ID", &env)),
            &CommandReply::Usage,
            "`{line}` must cost a usage line and no publication"
        );
    }
    assert!(!USAGE.is_empty(), "the usage line names the verbs");
}

#[test]
fn ordinary_text_that_merely_starts_with_mu_is_still_a_message() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("mu:d")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    // A bare `mu` is a word, not a verb.
    let d = o.route_line("alice", "#mu", "mu", "ID", &env);
    let (_, targets, body, _) = published(&d);
    assert_eq!(targets, &[PeerId::parse("mu:d")]);
    assert_eq!(body, "mu");
    // A peer id is a token ending in `:`, so it is an address and not a verb.
    let d = o.route_line("alice", "#mu", "mu:d: hello", "ID2", &env);
    let (_, targets, body, memory) = published(&d);
    assert_eq!(targets, &[PeerId::parse("mu:d")]);
    assert_eq!(body, "hello");
    assert!(memory.is_some(), "an explicit address still remembers");
}

#[test]
fn a_command_is_dispatched_ahead_of_routing_but_behind_both_gates() {
    let mut o = out();
    let mem = mem_with_alice();
    let peers = vec![PeerId::parse("cc:abc")];
    let env = OutEnv {
        peers: &peers,
        membership: &mem,
        puppets: None,
    };
    // Loop guard 1 still comes first: a verb the gateway's own line carries is
    // its own output coming back, never a command to run.
    assert_eq!(
        o.route_line("mu-gw", "#mu", "mu peers", "ID", &env),
        OutboundDecision::Drop(OutDrop::OwnNick)
    );
    // So does sender authorization: a nick in no shared channel cannot be
    // vouched for, and does not get the roster either.
    assert_eq!(
        o.route_line("mallory", "#mu", "mu peers", "ID", &env),
        OutboundDecision::Refuse(RefuseReason::UnauthorizedSender("mallory".into()))
    );
    // A line while disconnected is still dropped rather than answered.
    o.set_connected(false);
    assert_eq!(
        o.route_line("alice", "#mu", "mu peers", "ID", &env),
        OutboundDecision::Drop(OutDrop::Disconnected)
    );
}
