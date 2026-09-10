//! Offline mesh→IRC routing tests for increment 2b: the fail-closed ingress,
//! exactly-once overlap, and every decision branch (agent channel / collision /
//! lobby, and the exclusive human precedence). No socket, no live mesh.

use std::collections::HashMap;

use base64::Engine as _;
use biscuit_auth::macros::biscuit;
use biscuit_auth::KeyPair;

use mu_dialogue::mesh::{AgentCommand, DmEnvelope, DmRejected, MeshDmEvent, Reception};
use mu_irc_gateway::mapping::CaseMapping;
use mu_irc_gateway::membership::Membership;
use mu_irc_gateway::routing::{
    DropReason, IngressRejected, OversizedField, RouteDecision, RouteEnv, Router,
    MAX_DESTINATION_LEN, MAX_FIELD_LEN,
};
use mu_peer::PeerId;

const RFC: CaseMapping = CaseMapping::Rfc1459;

/// A verified-shaped event built directly (the decision tests do not need a real
/// signature — the ingress test below exercises verification for real).
fn event(id: &str, dest: &str, from: &str, body: &str, session: Option<&str>) -> MeshDmEvent {
    MeshDmEvent {
        id: id.into(),
        destination: dest.into(),
        from: from.into(),
        body: body.into(),
        subject: None,
        session: session.map(str::to_string),
        reception: Reception::Observer,
    }
}

/// A `RouteEnv` with the given discovered peers and membership; sensible offline
/// defaults for the rest.
fn env<'a>(
    peers: &'a [PeerId],
    membership: &'a Membership,
    remembered: &'a HashMap<String, String>,
) -> RouteEnv<'a> {
    RouteEnv {
        peers,
        membership,
        remembered,
        prefix: "#",
        lobby: "#mu",
        channellen: 50,
        cm: RFC,
        message_tags: true,
    }
}

fn empty_mem() -> Membership {
    Membership::new("mu-gw", RFC)
}

/// The target and single PRIVMSG line of a one-line Deliver decision.
fn delivered(d: &RouteDecision) -> (String, String) {
    match d {
        RouteDecision::Deliver { target, lines } => {
            assert_eq!(lines.len(), 1, "expected one line: {lines:?}");
            (target.clone(), lines[0].clone())
        }
        other => panic!("expected Deliver, got {other:?}"),
    }
}

// ──────────────────────────── Fail-closed ingress ───────────────────────────

/// Build a real capability-signed envelope payload, signed by `issuer`.
fn signed_payload(issuer: &KeyPair, id: &str, from: &str, body: &str) -> Vec<u8> {
    signed_payload_for(issuer, id, from, body, None)
}

/// The same, with control over the envelope's target session — the third
/// remote-controlled, unbounded field the ingress caps.
fn signed_payload_for(
    issuer: &KeyPair,
    id: &str,
    from: &str,
    body: &str,
    session: Option<&str>,
) -> Vec<u8> {
    let token = biscuit!(r#"right("agent_dm");"#)
        .build(issuer)
        .unwrap()
        .to_vec()
        .unwrap();
    let env = DmEnvelope {
        id: id.into(),
        capability: base64::engine::general_purpose::STANDARD.encode(token),
        command: AgentCommand::Dm {
            from: from.into(),
            body: body.into(),
            session: session.map(str::to_string),
            subject: None,
            from_session: None,
        },
    };
    serde_json::to_vec(&env).unwrap()
}

#[test]
fn ingress_accepts_a_valid_envelope_on_both_paths() {
    let issuer = KeyPair::new();
    let router = Router::new(issuer.public());
    let payload = signed_payload(&issuer, "01H", "cc:sender", "hello");
    for reception in [Reception::Endpoint, Reception::Observer] {
        let ev = router
            .accept("mu.agent.human.alice.dm", &payload, reception)
            .expect("valid envelope accepted");
        assert_eq!(ev.id, "01H");
        assert_eq!(ev.body, "hello");
        assert_eq!(ev.from, "cc:sender");
        assert_eq!(ev.reception, reception);
    }
}

#[test]
fn ingress_rejects_garbage_and_wrong_issuer_body_free() {
    let issuer = KeyPair::new();
    let router = Router::new(issuer.public());
    // Not an envelope at all.
    assert_eq!(
        router.accept("mu.agent.human.alice.dm", b"{not json", Reception::Endpoint),
        Err(IngressRejected::Unverified(DmRejected::NotAnEnvelope))
    );
    // A well-formed envelope signed by a DIFFERENT issuer: unauthorized, and the
    // rejection names only the class — never the body.
    let other = KeyPair::new();
    let payload = signed_payload(&other, "01H", "cc:sender", "TOPSECRET-BODY");
    let err = router
        .accept("mu.agent.human.alice.dm", &payload, Reception::Observer)
        .unwrap_err();
    assert_eq!(err, IngressRejected::Unverified(DmRejected::Unauthorized));
    assert!(!format!("{err} {err:?}").contains("TOPSECRET-BODY"));
}

#[test]
fn an_oversized_ingress_field_is_refused_body_free() {
    // `verify_and_decode_dm` decodes and verifies a capability; it bounds no
    // field length. The gateway goes on to fingerprint `id`, `destination` and
    // `session` into a bounded window and to put the id on an IRC line, so an
    // envelope whose remote-controlled fields are megabytes long must not reach
    // routing at all — the entry COUNT being bounded is not a memory bound if
    // the sender picks the size of an entry.
    let issuer = KeyPair::new();
    let router = Router::new(issuer.public());
    let secret = "oversized-body-DO-NOT-LEAK";
    let huge = "z".repeat(1024 * 1024);

    // A 1 MiB id: refused, and the rejection names the field, never the body.
    let payload = signed_payload(&issuer, &huge, "cc:sender", secret);
    let err = router
        .accept("mu.agent.human.alice.dm", &payload, Reception::Endpoint)
        .unwrap_err();
    assert_eq!(err, IngressRejected::Oversized(OversizedField::Id));
    let rendered = format!("{err} {err:?}");
    assert!(!rendered.contains(secret), "the rejection carried the body");
    assert!(
        !rendered.contains("zzzz"),
        "the rejection carried the field"
    );

    // A 1 MiB session, the field routing never frames and nothing else checks.
    let payload = signed_payload_for(&issuer, "01H", "cc:sender", secret, Some(&huge));
    assert_eq!(
        router.accept("mu.agent.human.alice.dm", &payload, Reception::Observer),
        Err(IngressRejected::Oversized(OversizedField::Session))
    );

    // An oversized destination is refused BEFORE the payload is decoded: this
    // payload is not an envelope at all, and the destination is still what the
    // rejection names.
    let over = format!("mu.agent.cc.{}.dm", "z".repeat(MAX_DESTINATION_LEN));
    assert_eq!(
        router.accept(&over, b"{not json", Reception::Observer),
        Err(IngressRejected::Oversized(OversizedField::Destination))
    );

    // Exactly at the caps is still accepted: the caps bound, they do not shrink
    // what the mesh can legitimately address.
    let at_cap = "z".repeat(MAX_FIELD_LEN);
    let payload = signed_payload_for(&issuer, &at_cap, "cc:sender", "hi", Some(&at_cap));
    let dest = format!("mu.agent.cc.{}.dm", "z".repeat(MAX_DESTINATION_LEN - 15));
    assert_eq!(dest.len(), MAX_DESTINATION_LEN);
    let ev = router
        .accept(&dest, &payload, Reception::Endpoint)
        .expect("a field exactly at the cap is accepted");
    assert_eq!(ev.id, at_cap);
    assert_eq!(ev.session.as_deref(), Some(at_cap.as_str()));
}

// ─────────────────────────── Exactly-once overlap ───────────────────────────

#[test]
fn endpoint_observer_overlap_delivers_exactly_once() {
    let mem = empty_mem();
    let remembered = HashMap::new();
    let mut r = Router::new(KeyPair::new().public());
    let peers = vec![PeerId::parse("cc:abc")];
    let e = env(&peers, &mem, &remembered);
    let ev = event("F1", "mu.agent.cc.abc.dm", "cc:x", "hi", None);
    assert!(matches!(r.route(&ev, &e), RouteDecision::Deliver { .. }));
    // The same (id, destination, session) again — the observer echo — is dropped.
    assert_eq!(r.route(&ev, &e), RouteDecision::Drop(DropReason::Duplicate));
}

#[test]
fn one_fanout_id_still_delivers_to_distinct_destinations_and_sessions() {
    let mem = empty_mem();
    let remembered = HashMap::new();
    let mut r = Router::new(KeyPair::new().public());
    let peers = vec![PeerId::parse("cc:abc"), PeerId::parse("cc:def")];
    let e = env(&peers, &mem, &remembered);
    // Same fan-out id `F`, two DIFFERENT destinations: both deliver.
    let a = event("F", "mu.agent.cc.abc.dm", "cc:x", "hi", None);
    let b = event("F", "mu.agent.cc.def.dm", "cc:x", "hi", None);
    assert!(matches!(r.route(&a, &e), RouteDecision::Deliver { .. }));
    assert!(matches!(r.route(&b, &e), RouteDecision::Deliver { .. }));
    // Same id AND destination but a different session: still distinct.
    let s1 = event("F", "mu.agent.cc.abc.dm", "cc:x", "hi", Some("s1"));
    let s2 = event("F", "mu.agent.cc.abc.dm", "cc:x", "hi", Some("s2"));
    assert!(matches!(r.route(&s1, &e), RouteDecision::Deliver { .. }));
    assert!(matches!(r.route(&s2, &e), RouteDecision::Deliver { .. }));
}

// ───────────────────────────── Agent delivery ───────────────────────────────

#[test]
fn present_agent_goes_to_its_channel_untagged_label() {
    let mem = empty_mem();
    let remembered = HashMap::new();
    let mut r = Router::new(KeyPair::new().public());
    let peers = vec![PeerId::parse("cc:abc")];
    let e = env(&peers, &mem, &remembered);
    let ev = event("01H", "mu.agent.cc.abc.dm", "cc:x", "hi", None);
    let (target, line) = {
        let d = r.route(&ev, &e);
        let (t, l) = delivered(&d);
        (t.to_string(), l.to_string())
    };
    assert_eq!(target, "#cc-abc");
    assert_eq!(line, "@+mu.id=01H PRIVMSG #cc-abc :hi\r\n");
}

#[test]
fn absent_agent_falls_to_the_lobby_with_a_target_label() {
    let mem = empty_mem();
    let remembered = HashMap::new();
    let mut r = Router::new(KeyPair::new().public());
    // No discovered peers: the agent is absent.
    let peers: Vec<PeerId> = vec![];
    let e = env(&peers, &mem, &remembered);
    let ev = event("01H", "mu.agent.cc.abc.dm", "cc:x", "hi", None);
    let d = r.route(&ev, &e);
    let (target, line) = delivered(&d);
    assert_eq!(target, "#mu");
    assert!(
        line.contains("cc:abc"),
        "lobby line must name the target: {line}"
    );
    assert!(line.ends_with(":[\u{2192} cc:abc] hi\r\n"), "{line}");
}

#[test]
fn colliding_channel_carries_a_disambiguating_label() {
    let mem = empty_mem();
    let remembered = HashMap::new();
    let mut r = Router::new(KeyPair::new().public());
    // cc:abc and cc:ABC fold to one channel; the label names which peer.
    let peers = vec![PeerId::parse("cc:abc"), PeerId::parse("cc:ABC")];
    let e = env(&peers, &mem, &remembered);
    let ev = event("01H", "mu.agent.cc.abc.dm", "cc:x", "hi", None);
    let d = r.route(&ev, &e);
    let (target, line) = delivered(&d);
    assert_eq!(target, "#cc-abc");
    assert!(
        line.contains("[cc:abc] hi"),
        "collision label missing: {line}"
    );
}

// ───────────────────────── Human precedence (exclusive) ─────────────────────

/// A membership with the gateway in `#a` and `alice` present there.
fn mem_with_alice_in_a() -> Membership {
    let mut m = Membership::new("mu-gw", RFC);
    let g = m.self_joined("#a");
    m.names_reply("#a", g, [("alice".to_string(), None)]);
    m.names_end("#a", g);
    m
}

#[test]
fn present_human_with_no_memory_gets_a_private_message() {
    let mem = mem_with_alice_in_a();
    let remembered = HashMap::new();
    let mut r = Router::new(KeyPair::new().public());
    let peers: Vec<PeerId> = vec![];
    let e = env(&peers, &mem, &remembered);
    let ev = event("01H", "mu.agent.human.alice.dm", "cc:x", "psst", None);
    let (target, line) = delivered(&r.route(&ev, &e));
    assert_eq!(target, "alice");
    assert_eq!(line, "@+mu.id=01H PRIVMSG alice :psst\r\n");
}

#[test]
fn remembered_channel_takes_precedence_when_the_human_is_still_in_it() {
    let mem = mem_with_alice_in_a();
    let mut remembered = HashMap::new();
    remembered.insert("alice".to_string(), "#a".to_string());
    let mut r = Router::new(KeyPair::new().public());
    let peers: Vec<PeerId> = vec![];
    let e = env(&peers, &mem, &remembered);
    let ev = event("01H", "mu.agent.human.alice.dm", "cc:x", "psst", None);
    let (target, _) = delivered(&r.route(&ev, &e));
    assert_eq!(
        target, "#a",
        "remembered channel wins over a private message"
    );
}

#[test]
fn remembered_channel_is_ignored_when_the_human_has_left_it() {
    // The gateway is in #a but alice is NOT (she left); a stale memory of #a
    // must not disclose her DM to a channel she is no longer in.
    let mut mem = Membership::new("mu-gw", RFC);
    let g = mem.self_joined("#a");
    mem.names_reply("#a", g, [("bob".to_string(), None)]);
    mem.names_end("#a", g);
    // alice present only in #b.
    let g2 = mem.self_joined("#b");
    mem.names_reply("#b", g2, [("alice".to_string(), None)]);
    mem.names_end("#b", g2);

    let mut remembered = HashMap::new();
    remembered.insert("alice".to_string(), "#a".to_string()); // stale
    let mut r = Router::new(KeyPair::new().public());
    let peers: Vec<PeerId> = vec![];
    let e = env(&peers, &mem, &remembered);
    let ev = event("01H", "mu.agent.human.alice.dm", "cc:x", "psst", None);
    let (target, _) = delivered(&r.route(&ev, &e));
    assert_eq!(
        target, "alice",
        "falls back to private, not the stale channel"
    );
}

#[test]
fn absent_human_gets_one_bodiless_notice_suppressed_then_reset_on_return() {
    let mem = empty_mem(); // alice is present nowhere
    let remembered = HashMap::new();
    let mut r = Router::new(KeyPair::new().public());
    let peers: Vec<PeerId> = vec![];
    let e = env(&peers, &mem, &remembered);

    let ev1 = event(
        "01H",
        "mu.agent.human.alice.dm",
        "cc:x",
        "SECRET-BODY",
        None,
    );
    match r.route(&ev1, &e) {
        RouteDecision::Notice { target, line } => {
            assert_eq!(target, "#mu");
            assert!(line.contains("alice"));
            assert!(
                !line.contains("SECRET-BODY"),
                "notice leaked the body: {line}"
            );
        }
        other => panic!("expected a bodiless Notice, got {other:?}"),
    }
    // A second DM (fresh id, so not a dedup drop) is suppressed within the
    // withdrawal.
    let ev2 = event("02H", "mu.agent.human.alice.dm", "cc:x", "more", None);
    assert_eq!(
        r.route(&ev2, &e),
        RouteDecision::Drop(DropReason::SuppressedNotice)
    );
    // The human returns: suppression resets, so a later DM notifies again.
    r.human_returned("alice", RFC);
    let ev3 = event("03H", "mu.agent.human.alice.dm", "cc:x", "again", None);
    assert!(matches!(r.route(&ev3, &e), RouteDecision::Notice { .. }));
}

#[test]
fn an_unroutable_destination_is_dropped() {
    let mem = empty_mem();
    let remembered = HashMap::new();
    let mut r = Router::new(KeyPair::new().public());
    let peers: Vec<PeerId> = vec![];
    let e = env(&peers, &mem, &remembered);
    // Not a DM subject at all.
    let ev = event("01H", "some.other.subject", "cc:x", "hi", None);
    assert_eq!(
        r.route(&ev, &e),
        RouteDecision::Drop(DropReason::Unroutable)
    );
}

#[test]
fn untagged_when_message_tags_not_negotiated() {
    let mem = empty_mem();
    let remembered = HashMap::new();
    let mut r = Router::new(KeyPair::new().public());
    let peers = vec![PeerId::parse("cc:abc")];
    let mut e = env(&peers, &mem, &remembered);
    e.message_tags = false;
    let ev = event("01H", "mu.agent.cc.abc.dm", "cc:x", "hi", None);
    let (_, line) = delivered(&r.route(&ev, &e));
    assert_eq!(
        line, "PRIVMSG #cc-abc :hi\r\n",
        "no tag without negotiation"
    );
}

// ───────────────── Malformed human destination / presence / bounds ──────────

#[test]
fn a_human_destination_with_no_nick_is_dropped_not_lobbied() {
    // `mu.agent.human.dm` parses to the human ROLE with no nick, so
    // `human_nick()` is None. Deciding on that None sent it down the agent path,
    // where `channel_for` declines every human and the fallback framed the BODY
    // into the public lobby — the one place a human-addressed body must never
    // go, reachable by any sender the generic `agent_dm` capability authorizes.
    let mem = empty_mem();
    let remembered = HashMap::new();
    let mut r = Router::new(KeyPair::new().public());
    let peers: Vec<PeerId> = vec![];
    let e = env(&peers, &mem, &remembered);
    let secret = "private-body-DO-NOT-LEAK";
    for dest in ["mu.agent.human.dm", "mu.agent.human..dm"] {
        let ev = event("01H", dest, "cc:x", secret, None);
        let d = r.route(&ev, &e);
        assert_eq!(
            d,
            RouteDecision::Drop(DropReason::MalformedHumanDestination),
            "destination {dest:?}"
        );
        assert!(
            !format!("{d:?}").contains(secret),
            "the drop carried the body: {d:?}"
        );
    }
    // A human destination that DOES name a nick still routes as a human: the
    // fix narrows the malformed case, it does not disable human routing.
    let mem = mem_with_alice_in_a();
    let e = env(&peers, &mem, &remembered);
    let ev = event("01H2", "mu.agent.human.alice.dm", "cc:x", "hi", None);
    let (target, _) = delivered(&r.route(&ev, &e));
    assert_eq!(target, "alice");
}

#[test]
fn agent_presence_is_exact_peer_identity_not_channel_occupancy() {
    // `cc:ABC` and `cc:abc` fold to one channel. With only `cc:ABC` discovered,
    // a DM for the ABSENT `cc:abc` used to count one "colliding" channel, read
    // that as presence, and deliver into `#cc-abc` with no label — silently
    // attributing another agent's message to the one that is actually there.
    let mem = empty_mem();
    let remembered = HashMap::new();
    let mut r = Router::new(KeyPair::new().public());
    let peers = vec![PeerId::parse("cc:ABC")];
    let e = env(&peers, &mem, &remembered);
    let ev = event("01H", "mu.agent.cc.abc.dm", "cc:x", "hi", None);
    let (target, line) = delivered(&r.route(&ev, &e));
    assert_eq!(
        target, "#mu",
        "an absent target must fall back to the lobby"
    );
    assert!(
        line.contains("[\u{2192} cc:abc]"),
        "the lobby line must name the intended target: {line}"
    );

    // The present half of the same collision is unaffected: `cc:ABC` IS
    // discovered, so its own DM goes to the shared channel — labelled, because
    // the channel is shared and the operator cannot otherwise tell them apart.
    let ev = event("01H2", "mu.agent.cc.ABC.dm", "cc:x", "hi", None);
    let (target, line) = delivered(&r.route(&ev, &e));
    assert_eq!(target, "#cc-ABC");
    assert!(line.contains("hi"), "{line}");

    // And when the target is present with NO collision, no label is added.
    let peers = vec![PeerId::parse("cc:abc")];
    let e = env(&peers, &mem, &remembered);
    let ev = event("01H3", "mu.agent.cc.abc.dm", "cc:x", "plain", None);
    let (target, line) = delivered(&r.route(&ev, &e));
    assert_eq!(target, "#cc-abc");
    assert!(line.ends_with(":plain\r\n"), "unexpected label: {line}");
}

#[test]
fn the_overlap_dedup_window_is_bounded_and_still_catches_recent_duplicates() {
    // The exactly-once key used to be retained for the life of the connection,
    // so a healthy long-lived connection grew it with total traffic. It is now
    // a fixed-capacity window: the oldest key is evicted to make room, while
    // everything inside the window still de-duplicates.
    let mem = empty_mem();
    let remembered = HashMap::new();
    let mut r = Router::new(KeyPair::new().public());
    let peers: Vec<PeerId> = vec![];
    let e = env(&peers, &mem, &remembered);

    // The first id, then a full window's worth of distinct ids after it.
    let first = event("id-0", "mu.agent.cc.a.dm", "cc:x", "hi", None);
    assert!(matches!(r.route(&first, &e), RouteDecision::Deliver { .. }));
    assert_eq!(
        r.route(&first, &e),
        RouteDecision::Drop(DropReason::Duplicate),
        "an immediate duplicate must collapse"
    );
    let cap = mu_irc_gateway::recent::DEFAULT_CAPACITY;
    for i in 1..=cap {
        let ev = event(&format!("id-{i}"), "mu.agent.cc.a.dm", "cc:x", "hi", None);
        assert!(matches!(r.route(&ev, &e), RouteDecision::Deliver { .. }));
    }
    // The oldest key has been evicted, so the window did not grow without bound.
    assert!(
        matches!(r.route(&first, &e), RouteDecision::Deliver { .. }),
        "the oldest key was never evicted, so the set is unbounded"
    );
    // Recent ids are still de-duplicated, which is the whole point of the set.
    let recent = event(&format!("id-{cap}"), "mu.agent.cc.a.dm", "cc:x", "hi", None);
    assert_eq!(
        r.route(&recent, &e),
        RouteDecision::Drop(DropReason::Duplicate),
        "a recent duplicate escaped the window"
    );
}

#[test]
fn the_recent_window_evicts_in_insertion_order() {
    // The window's own contract, independent of routing: bounded, oldest-first
    // eviction, and a re-record does NOT refresh a key's position.
    use mu_irc_gateway::recent::RecentSet;
    let mut w: RecentSet<u32> = RecentSet::with_capacity(3);
    assert!(w.is_empty());
    for i in 0..3 {
        assert!(w.record(i), "{i} is new");
    }
    assert_eq!(w.len(), 3);
    assert!(!w.record(0), "still inside the window");
    // Re-recording 0 must not move it to the back: 0 is still the oldest.
    assert!(w.record(3), "3 is new");
    assert_eq!(w.len(), 3, "capacity held");
    assert!(!w.contains(&0), "the oldest key was not evicted");
    assert!(w.contains(&1) && w.contains(&2) && w.contains(&3));
    w.clear();
    assert!(w.is_empty());
    assert_eq!(w.capacity(), 3);
    // A zero capacity degrades to one rather than misbehaving.
    let mut w: RecentSet<u32> = RecentSet::with_capacity(0);
    assert!(w.record(1));
    assert!(!w.record(1));
    assert!(w.record(2));
    assert!(!w.contains(&1));
}

// ────────────────── Outbound safety of the body-free notice ─────────────────

#[test]
fn the_absent_human_notice_is_framed_like_every_other_outbound_line() {
    // The notice used to be raw `format!` interpolation of a destination-derived
    // nick — the one line this module produced that never met the framing
    // module's target rules, control-character rejection or line budget, even
    // though every `Deliver` did.
    let mem = empty_mem(); // alice is present nowhere
    let remembered = HashMap::new();
    let mut r = Router::new(KeyPair::new().public());
    let peers: Vec<PeerId> = vec![];
    let e = env(&peers, &mem, &remembered);
    let ev = event(
        "01H",
        "mu.agent.human.alice.dm",
        "cc:x",
        "SECRET-BODY",
        None,
    );
    match r.route(&ev, &e) {
        RouteDecision::Notice { target, line } => {
            assert_eq!(target, "#mu");
            assert_eq!(
                line,
                "PRIVMSG #mu :[mu] a direct message arrived for alice (not currently present)\r\n",
                "the notice must be a complete framed line"
            );
            assert!(!line.contains("SECRET-BODY"), "notice leaked the body");
        }
        other => panic!("expected a bodiless Notice, got {other:?}"),
    }
}

#[test]
fn a_destination_nick_that_cannot_be_framed_is_dropped_not_noticed() {
    // A nick arrives as a NATS subject token and `PeerId::parse` is total, so
    // nothing upstream holds it to what an IRC line can carry. Each of these
    // would previously have been interpolated into the notice unchecked.
    let mem = empty_mem();
    let remembered = HashMap::new();
    let mut r = Router::new(KeyPair::new().public());
    let peers: Vec<PeerId> = vec![];
    let e = env(&peers, &mem, &remembered);
    let secret = "notice-body-DO-NOT-LEAK";
    let oversized = "z".repeat(600);
    let cases = [
        "mu.agent.human.ali\r\nQUIT.dm".to_string(), // a whole second command
        "mu.agent.human.ali\nJOIN.dm".to_string(),   // a bare LF second command
        "mu.agent.human.alice,bob.dm".to_string(),   // a target list
        format!("mu.agent.human.{oversized}.dm"),    // past the 512-byte budget
    ];
    for (i, dest) in cases.iter().enumerate() {
        let ev = event(&format!("0{i}H"), dest, "cc:x", secret, None);
        let d = r.route(&ev, &e);
        assert_eq!(
            d,
            RouteDecision::Drop(DropReason::MalformedHumanDestination),
            "destination {dest:?} must be a body-free drop, not a notice"
        );
        assert!(
            !format!("{d:?}").contains(secret),
            "the drop carried the body: {d:?}"
        );
    }
    // A nick that DOES frame still gets its notice: the check narrows the
    // malformed case, it does not disable the notice.
    let ev = event("01H9", "mu.agent.human.alice.dm", "cc:x", secret, None);
    assert!(matches!(r.route(&ev, &e), RouteDecision::Notice { .. }));
}

// ─────────────────── Bounded absent-notice suppression ──────────────────────

#[test]
fn absent_notice_suppression_is_bounded_and_a_return_still_clears_it() {
    // The suppression set used to be an unbounded `HashSet`: an entry per
    // distinct absent nick, removed only when THAT human turned up. Who is
    // addressed is the sender's choice, so it grew with traffic. It is now the
    // same bounded window as the exactly-once key.
    let mem = empty_mem(); // nobody is present
    let remembered = HashMap::new();
    let mut r = Router::new(KeyPair::new().public());
    let peers: Vec<PeerId> = vec![];
    let e = env(&peers, &mem, &remembered);
    let cap = mu_irc_gateway::recent::DEFAULT_CAPACITY;

    // The oldest nick: notified once, then suppressed within the withdrawal.
    let first = event("n-0", "mu.agent.human.u0.dm", "cc:x", "s", None);
    assert!(matches!(r.route(&first, &e), RouteDecision::Notice { .. }));
    let repeat = event("n-0b", "mu.agent.human.u0.dm", "cc:x", "s", None);
    assert_eq!(
        r.route(&repeat, &e),
        RouteDecision::Drop(DropReason::SuppressedNotice)
    );

    // A full window of OTHER nicks who never appear.
    for i in 1..=cap {
        let ev = event(
            &format!("n-{i}"),
            &format!("mu.agent.human.u{i}.dm"),
            "cc:x",
            "s",
            None,
        );
        assert!(
            matches!(r.route(&ev, &e), RouteDecision::Notice { .. }),
            "nick u{i} must be notified once"
        );
    }
    // The oldest entry was evicted, so the set did not grow with traffic. The
    // cost is one more BODY-FREE notice; no body is disclosed either way.
    let aged_out = event("n-0c", "mu.agent.human.u0.dm", "cc:x", "s", None);
    assert!(
        matches!(r.route(&aged_out, &e), RouteDecision::Notice { .. }),
        "the oldest suppression entry was never evicted, so the set is unbounded"
    );

    // A human's RETURN still clears their entry outright rather than waiting for
    // the window to age it out.
    let a = event("n-r1", "mu.agent.human.zoe.dm", "cc:x", "s", None);
    assert!(matches!(r.route(&a, &e), RouteDecision::Notice { .. }));
    let b = event("n-r2", "mu.agent.human.zoe.dm", "cc:x", "s", None);
    assert_eq!(
        r.route(&b, &e),
        RouteDecision::Drop(DropReason::SuppressedNotice)
    );
    r.human_returned("zoe", RFC);
    let c = event("n-r3", "mu.agent.human.zoe.dm", "cc:x", "s", None);
    assert!(
        matches!(r.route(&c, &e), RouteDecision::Notice { .. }),
        "a returned human's suppression must be cleared"
    );
}

#[test]
fn the_recent_window_removes_a_key_without_reordering_the_rest() {
    // Removal has to take a key OUT of the window, not shuffle the ones around
    // it: eviction still runs oldest-first afterwards.
    use mu_irc_gateway::recent::RecentSet;
    let mut w: RecentSet<u32> = RecentSet::with_capacity(3);
    for i in 0..3 {
        assert!(w.record(i));
    }
    assert!(w.remove(&1), "1 was in the window");
    assert!(
        !w.remove(&1),
        "removing it again reports it was already gone"
    );
    assert!(!w.contains(&1));
    assert_eq!(w.len(), 2);
    // 0 is still the oldest, so it is what the next eviction takes.
    assert!(w.record(3));
    assert!(w.record(4));
    assert!(!w.contains(&0), "removal reordered the window");
    assert!(w.contains(&2) && w.contains(&3) && w.contains(&4));
}

// ───────────────────────── Destination validation ───────────────────────────

#[test]
fn unroutable_destinations_do_not_evict_the_exactly_once_window() {
    // The key used to be recorded BEFORE the destination was validated, so
    // subjects that address nobody — free to mint for anyone the generic
    // `agent_dm` capability authorizes — spent slots of the bounded window and
    // pushed real keys out of it, re-opening the endpoint/observer overlap that
    // window exists to collapse.
    let mem = empty_mem();
    let remembered = HashMap::new();
    let mut r = Router::new(KeyPair::new().public());
    let peers = vec![PeerId::parse("cc:abc")];
    let e = env(&peers, &mem, &remembered);

    let real = event("real-1", "mu.agent.cc.abc.dm", "cc:x", "hi", None);
    assert!(matches!(r.route(&real, &e), RouteDecision::Deliver { .. }));

    // Twice a full window of unroutable destinations, every one of them dropped.
    let cap = mu_irc_gateway::recent::DEFAULT_CAPACITY;
    for i in 0..(cap * 2) {
        let ev = event(
            &format!("junk-{i}"),
            &format!("mu.agent.cc.{i}..dm"),
            "cc:x",
            "hi",
            None,
        );
        assert_eq!(
            r.route(&ev, &e),
            RouteDecision::Drop(DropReason::Unroutable),
            "junk {i}"
        );
    }
    // The legitimate key is still in the window, so its observer echo collapses.
    assert_eq!(
        r.route(&real, &e),
        RouteDecision::Drop(DropReason::Duplicate),
        "a flood of unroutable destinations evicted a legitimate key"
    );
}

#[test]
fn a_destination_with_an_empty_component_is_dropped() {
    // `PeerId::parse` is total by design, so `mu.agent.cc..dm` used to mint the
    // peer `cc:` and, from it, the degenerate channel `#cc-`. A component with
    // nothing in it names nothing.
    let mem = empty_mem();
    let remembered = HashMap::new();
    let mut r = Router::new(KeyPair::new().public());
    let peers: Vec<PeerId> = vec![];
    let e = env(&peers, &mem, &remembered);
    let secret = "empty-component-body-DO-NOT-LEAK";
    for dest in [
        "mu.agent.cc..dm",   // an empty id
        "mu.agent..x.dm",    // an empty role
        "mu.agent.mu.d..dm", // an empty sub-level
        "mu.agent..dm",      // nothing at all
    ] {
        let ev = event("01H", dest, "cc:x", secret, None);
        let d = r.route(&ev, &e);
        assert_eq!(
            d,
            RouteDecision::Drop(DropReason::Unroutable),
            "destination {dest:?}"
        );
        assert!(
            !format!("{d:?}").contains(secret),
            "the drop carried the body: {d:?}"
        );
    }
    // The `human` role keeps its own reason for the same shape: it is not merely
    // unroutable, it is the shape that must never be re-routed as an agent.
    let ev = event("01H", "mu.agent.human..dm", "cc:x", secret, None);
    assert_eq!(
        r.route(&ev, &e),
        RouteDecision::Drop(DropReason::MalformedHumanDestination)
    );
    // Well-formed destinations are unaffected.
    let peers = vec![PeerId::parse("cc:abc")];
    let e = env(&peers, &mem, &remembered);
    let ev = event("01H2", "mu.agent.cc.abc.dm", "cc:x", "hi", None);
    let (target, _) = delivered(&r.route(&ev, &e));
    assert_eq!(target, "#cc-abc");
}

// ─────────────────────── Exactly-once key construction ──────────────────────

#[test]
fn the_exactly_once_key_separates_its_parts() {
    // The retained key is a digest, so its three parts have to be absorbed
    // unambiguously. These two events are DIFFERENT — different ids, different
    // sessions — but their parts concatenate to the same bytes, so a key built
    // by joining them without a length prefix would collapse the second into a
    // spurious `Duplicate` and silently drop a real delivery.
    let mem = empty_mem();
    let remembered = HashMap::new();
    let mut r = Router::new(KeyPair::new().public());
    let peers = vec![PeerId::parse("cc:abc")];
    let e = env(&peers, &mem, &remembered);

    let dest = "mu.agent.cc.abc.dm";
    let (id_a, session_a) = ("a", "u.agent.cc.abc.dm");
    let (id_b, session_b) = ("amu.agent.cc.abc.d", "");
    // The premise, asserted rather than assumed.
    assert_eq!(
        format!("{id_a}{dest}{session_a}"),
        format!("{id_b}{dest}{session_b}"),
        "the two events must collide under naive concatenation"
    );

    let a = event(id_a, dest, "cc:x", "first", Some(session_a));
    let b = event(id_b, dest, "cc:x", "second", Some(session_b));
    assert!(matches!(r.route(&a, &e), RouteDecision::Deliver { .. }));
    assert!(
        matches!(r.route(&b, &e), RouteDecision::Deliver { .. }),
        "the parts were concatenated without a boundary, so a distinct event \
         was mistaken for a duplicate"
    );
}

#[test]
fn hashed_exactly_once_keys_still_collapse_a_genuine_overlap() {
    // Hashing the key must not weaken the guarantee it exists for: the same
    // (id, destination, session) arriving twice — the endpoint/observer overlap
    // of ONE dispatch — is still exactly one delivery, and an absent session is
    // still a different key from a present-but-empty one.
    let mem = empty_mem();
    let remembered = HashMap::new();
    let mut r = Router::new(KeyPair::new().public());
    let peers = vec![PeerId::parse("cc:abc")];
    let e = env(&peers, &mem, &remembered);

    let mut endpoint = event("F1", "mu.agent.cc.abc.dm", "cc:x", "hi", Some("s"));
    endpoint.reception = Reception::Endpoint;
    let mut observer = endpoint.clone();
    observer.reception = Reception::Observer;
    assert!(matches!(
        r.route(&endpoint, &e),
        RouteDecision::Deliver { .. }
    ));
    assert_eq!(
        r.route(&observer, &e),
        RouteDecision::Drop(DropReason::Duplicate),
        "the overlap must still collapse after hashing"
    );

    // `None` and `Some("")` are distinct destinations of one fan-out id, so the
    // present/absent tag has to be part of the key.
    let none = event("F2", "mu.agent.cc.abc.dm", "cc:x", "hi", None);
    let empty = event("F2", "mu.agent.cc.abc.dm", "cc:x", "hi", Some(""));
    assert!(matches!(r.route(&none, &e), RouteDecision::Deliver { .. }));
    assert!(matches!(r.route(&empty, &e), RouteDecision::Deliver { .. }));
    assert_eq!(
        r.route(&none, &e),
        RouteDecision::Drop(DropReason::Duplicate)
    );
}

// ──────────────── Subject components the mesh could not emit ────────────────

#[test]
fn a_colon_in_a_subject_component_is_dropped_not_reinterpreted() {
    // The subject joins peer-id levels with `.`; `PeerId::parse` splits them on
    // `:`. A subject token carrying a `:` therefore came through the subject
    // intact and was then re-split into two levels: `mu.agent.human.alice:bob.dm`
    // resolved to the human `alice`, so a body addressed to a destination that
    // does not exist was delivered to a real, observed person.
    let mem = mem_with_alice_in_a();
    let remembered = HashMap::new();
    let mut r = Router::new(KeyPair::new().public());
    let peers = vec![PeerId::parse("cc:abc")];
    let e = env(&peers, &mem, &remembered);
    let secret = "colon-body-DO-NOT-LEAK";

    // alice IS reachable: the control case delivers to her.
    let ok = event("01H", "mu.agent.human.alice.dm", "cc:x", "hi", None);
    assert_eq!(delivered(&r.route(&ok, &e)).0, "alice");

    // The colon-bearing spelling of the same subject must NOT reach her.
    let ev = event("01H1", "mu.agent.human.alice:bob.dm", "cc:x", secret, None);
    let d = r.route(&ev, &e);
    assert_eq!(
        d,
        RouteDecision::Drop(DropReason::MalformedHumanDestination),
        "a colon in the nick token was re-split into a different peer"
    );
    assert!(
        !format!("{d:?}").contains(secret),
        "the drop carried the body"
    );
    assert!(
        !format!("{d:?}").contains("alice"),
        "the drop resolved to alice: {d:?}"
    );

    // An agent-role destination is held to the same rule, in every component.
    for dest in [
        "mu.agent.cc.a:b.dm",        // a colon in the id
        "mu.agent.cc:x.abc.dm",      // a colon in the role
        "mu.agent.mu.d.s:1.dm",      // a colon in the sub-level
        "mu.agent.human:x.alice.dm", // a colon that also hides the human role
    ] {
        let ev = event("01H2", dest, "cc:x", secret, None);
        let d = r.route(&ev, &e);
        assert_eq!(
            d,
            RouteDecision::Drop(DropReason::Unroutable),
            "destination {dest:?}"
        );
        assert!(
            !format!("{d:?}").contains(secret),
            "the drop carried the body: {d:?}"
        );
    }

    // A sub-level may still carry a `.` — that is how `mu:d:a.b` round-trips.
    let peers = vec![PeerId::parse("mu:d:a.b")];
    let e = env(&peers, &mem, &remembered);
    let ev = event("01H3", "mu.agent.mu.d.a.b.dm", "cc:x", "hi", None);
    assert!(matches!(r.route(&ev, &e), RouteDecision::Deliver { .. }));
}

#[test]
fn a_subject_component_the_mesh_could_not_have_emitted_is_dropped() {
    // A NATS subject is carried on a space-delimited protocol line, so
    // `PeerId::dm_subject` could never have produced a component holding
    // whitespace or a control character. Such a component names no peer.
    let mem = empty_mem();
    let remembered = HashMap::new();
    let mut r = Router::new(KeyPair::new().public());
    let peers: Vec<PeerId> = vec![];
    let e = env(&peers, &mem, &remembered);
    let secret = "unemittable-body-DO-NOT-LEAK";
    for dest in [
        "mu.agent.cc.a b.dm",
        "mu.agent.cc.a\tb.dm",
        "mu.agent.cc.a\u{0}b.dm",
        "mu.agent.cc x.abc.dm",
    ] {
        let ev = event("01H", dest, "cc:x", secret, None);
        let d = r.route(&ev, &e);
        assert_eq!(
            d,
            RouteDecision::Drop(DropReason::Unroutable),
            "destination {dest:?}"
        );
        assert!(
            !format!("{d:?}").contains(secret),
            "the drop carried the body: {d:?}"
        );
    }
    // The human role keeps its own reason for the same shape.
    let ev = event("01H1", "mu.agent.human.ali ce.dm", "cc:x", secret, None);
    assert_eq!(
        r.route(&ev, &e),
        RouteDecision::Drop(DropReason::MalformedHumanDestination)
    );
}
