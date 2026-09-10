//! Offline capability regression tests for increment 2a: configuration,
//! identity folding, mapping/channel policy, and framing. No network, no IRC
//! client — every check is a pure function of its inputs or a temp file.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use mu_irc_gateway::config::{load, load_irc, ConfigError, GatewayConfig, MeshConfig};
use mu_irc_gateway::framing::{frame_privmsg, FrameParams, FramingError, CONTINUATION_MARKER};
use mu_irc_gateway::mapping::{
    channel_for, fold_nick, human_identity, human_peer, peer_alias, resolve_channel, CaseMapping,
    Resolved,
};
use mu_peer::PeerId;

/// A distinctive password value: every credential-error diagnostic is asserted
/// NOT to contain it, so a misconfiguration can be logged without leaking.
const SENTINEL: &str = "hunter2-DO-NOT-LEAK-9Z";

/// A distinctive Ed25519 issuer key. `MeshConfig` belongs to `mu-dialogue` and
/// derives `Debug` over this field in plaintext, so anything in THIS crate that
/// prints a mesh config must redact it.
const ISSUER_SENTINEL: &str = "abadcafe-ISSUERKEY-DO-NOT-LEAK-7Q";

/// Write a uniquely named temp file and return its path. Uses Cargo's
/// per-crate integration-test temp dir (`CARGO_TARGET_TMPDIR`), which is inside
/// the writable target directory rather than a possibly-sandboxed `/tmp`.
fn tmp(name: &str, content: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let p = dir.join(format!(
        "mu-irc-gw-{}-{}-{name}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&p, content).unwrap();
    p
}

// ───────────────────────────── Configuration ────────────────────────────────

#[test]
fn minimal_config_applies_defaults() {
    let p = tmp(
        "min.toml",
        r#"
[irc]
server = "irc.example.org:6697"
nick = "mu-gw"
"#,
    );
    let cfg = load_irc(&p).unwrap();
    assert_eq!(cfg.server, "irc.example.org:6697");
    assert_eq!(cfg.nick, "mu-gw");
    assert!(cfg.tls, "tls defaults on");
    assert_eq!(cfg.channel_prefix, "#");
    assert_eq!(cfg.lobby, "#mu");
    assert!(cfg.observe_agent_dms, "observer defaults on");
    assert!(cfg.sasl.is_none(), "no SASL configured");
}

#[test]
fn required_fields_are_enforced() {
    let p = tmp("noserver.toml", "[irc]\nnick = \"x\"\n");
    assert!(matches!(
        load_irc(&p),
        Err(ConfigError::MissingField("server"))
    ));
    let p = tmp("nonick.toml", "[irc]\nserver = \"h:1\"\n");
    assert!(matches!(
        load_irc(&p),
        Err(ConfigError::MissingField("nick"))
    ));
    let p = tmp("nosection.toml", "[other]\nx = 1\n");
    assert!(matches!(load_irc(&p), Err(ConfigError::MissingSection(_))));
}

#[test]
fn full_sasl_pair_loads_and_debug_redacts() {
    let p = tmp(
        "sasl.toml",
        &format!(
            "[irc]\nserver=\"h:1\"\nnick=\"n\"\nsasl_user=\"acct\"\nsasl_password=\"{SENTINEL}\"\n"
        ),
    );
    let cfg = load_irc(&p).unwrap();
    let sasl = cfg.sasl.as_ref().expect("sasl present");
    assert_eq!(sasl.user, "acct");
    assert_eq!(
        sasl.password.expose(),
        SENTINEL,
        "value reachable to authenticate"
    );
    // But Debug must never surface it.
    let dbg = format!("{cfg:?}");
    assert!(!dbg.contains(SENTINEL), "Debug leaked the password: {dbg}");
    assert!(dbg.contains("redacted"));
}

#[test]
fn sasl_user_without_password_is_rejected() {
    let p = tmp(
        "nouserpw.toml",
        "[irc]\nserver=\"h:1\"\nnick=\"n\"\nsasl_user=\"acct\"\n",
    );
    let err = load_irc(&p).unwrap_err();
    assert!(matches!(err, ConfigError::SaslMissingPassword));
}

#[test]
fn password_without_user_is_rejected_without_leaking() {
    let p = tmp(
        "nopwuser.toml",
        &format!("[irc]\nserver=\"h:1\"\nnick=\"n\"\nsasl_password=\"{SENTINEL}\"\n"),
    );
    let err = load_irc(&p).unwrap_err();
    assert!(matches!(err, ConfigError::SaslMissingUser));
    assert_no_secret(&err);
}

#[test]
fn conflicting_password_sources_are_rejected_without_leaking() {
    let pwfile = tmp("pw.secret", SENTINEL);
    let p = tmp(
        "conflict.toml",
        &format!(
            "[irc]\nserver=\"h:1\"\nnick=\"n\"\nsasl_user=\"acct\"\n\
             sasl_password=\"{SENTINEL}\"\nsasl_password_file=\"{}\"\n",
            pwfile.display()
        ),
    );
    let err = load_irc(&p).unwrap_err();
    assert!(matches!(err, ConfigError::SaslPasswordConflict));
    assert_no_secret(&err);
}

#[test]
fn password_file_is_loaded_and_trimmed() {
    let pwfile = tmp("pw2.secret", &format!("{SENTINEL}\n"));
    let p = tmp(
        "pwfile.toml",
        &format!(
            "[irc]\nserver=\"h:1\"\nnick=\"n\"\nsasl_user=\"acct\"\nsasl_password_file=\"{}\"\n",
            pwfile.display()
        ),
    );
    let cfg = load_irc(&p).unwrap();
    // Trailing newline trimmed; the secret itself is intact.
    assert_eq!(cfg.sasl.unwrap().password.expose(), SENTINEL);
}

#[test]
fn unreadable_password_file_is_rejected_without_leaking() {
    let p = tmp(
        "badpwfile.toml",
        "[irc]\nserver=\"h:1\"\nnick=\"n\"\nsasl_user=\"acct\"\n\
         sasl_password_file=\"/no/such/mu-irc-gw/secret\"\n",
    );
    let err = load_irc(&p).unwrap_err();
    assert!(matches!(err, ConfigError::PasswordFile(_, _)));
    assert_no_secret(&err);
}

#[test]
fn malformed_config_is_rejected() {
    // Unknown key inside [irc].
    let p = tmp(
        "unknown.toml",
        "[irc]\nserver=\"h:1\"\nnick=\"n\"\nbogus=1\n",
    );
    assert!(matches!(load_irc(&p), Err(ConfigError::Malformed(_, _))));
    // Not valid TOML at all.
    let p = tmp("badtoml.toml", "[irc]\nserver = \n");
    assert!(matches!(load_irc(&p), Err(ConfigError::Toml(_, _))));
}

#[test]
fn toml_syntax_errors_never_quote_the_source_line() {
    // `toml::de::Error`'s Display renders an annotated snippet that reproduces
    // the offending source line verbatim. When the offending line IS the
    // password assignment, passing that Display through to the error put the
    // credential into a diagnostic this crate documents as safe to log.
    let cases = [
        // Unterminated string: the fault is on the password line itself.
        format!("[irc]\nsasl_password = \"{SENTINEL}\n"),
        // Fault one line later: the snippet still spans nearby source.
        format!("[irc]\nsasl_password = \"{SENTINEL}\"\nserver = \n"),
        // Junk after an otherwise valid password assignment.
        format!("[irc]\nsasl_password = \"{SENTINEL}\" oops\n"),
    ];
    for (i, text) in cases.iter().enumerate() {
        let p = tmp(&format!("leaky{i}.toml"), text);
        let err = load_irc(&p).unwrap_err();
        assert!(matches!(err, ConfigError::Toml(_, _)), "case {i}: {err:?}");
        assert_no_secret(&err);
        let d = format!("{err}");
        // Still a usable diagnostic: it says where parsing failed...
        assert!(d.contains("line"), "case {i}: no position: {d}");
        // ...on exactly one line, so no snippet can ride along in a log record.
        assert_eq!(d.lines().count(), 1, "case {i}: multi-line: {d}");
    }
}

#[test]
fn gateway_debug_redacts_the_mesh_signing_key() {
    let p = tmp(
        "gwdebug.toml",
        &format!(
            "[irc]\nserver=\"h:1\"\nnick=\"n\"\nsasl_user=\"acct\"\nsasl_password=\"{SENTINEL}\"\n"
        ),
    );
    let irc = load_irc(&p).unwrap();
    let mesh = MeshConfig {
        enabled: true,
        nats_url: "nats://127.0.0.1:4222".to_string(),
        issuer_key: ISSUER_SENTINEL.to_string(),
    };
    // Precondition: the borrowed mesh type really does print its private key.
    // That is exactly why GatewayConfig must not derive Debug.
    assert!(
        format!("{mesh:?}").contains(ISSUER_SENTINEL),
        "precondition: MeshConfig's own Debug shows the key"
    );

    let dbg = format!("{:?}", GatewayConfig { irc, mesh });
    assert!(
        !dbg.contains(ISSUER_SENTINEL),
        "Debug leaked the mesh signing key: {dbg}"
    );
    assert!(!dbg.contains(SENTINEL), "Debug leaked the password: {dbg}");
    // Redacted, not dropped: the non-secret mesh settings still diagnose.
    assert!(dbg.contains("redacted"), "no redaction marker: {dbg}");
    assert!(
        dbg.contains("nats://127.0.0.1:4222"),
        "mesh url lost: {dbg}"
    );
    assert!(dbg.contains("enabled: true"), "mesh enabled lost: {dbg}");
}

#[test]
fn mesh_loading_is_delegated() {
    // A valid [irc] but no [dialogue.mesh]: the bundled loader must surface the
    // shared mesh loader's error, proving delegation (not a re-implementation).
    let p = tmp("nomesh.toml", "[irc]\nserver=\"h:1\"\nnick=\"n\"\n");
    let err = load(&p, &p).unwrap_err();
    assert!(matches!(err, ConfigError::Mesh(_)));
}

#[test]
fn mesh_loader_diagnostics_never_carry_the_issuer_key() {
    // The shared mesh loader renders serde diagnostics verbatim: an unquoted
    // issuer_key arrives as `invalid type: integer `8675309``. That text must
    // not reach the ConfigError this crate documents as safe to log.
    let numeric = "8675309";
    let p = tmp(
        "meshnum.toml",
        &format!(
            "[irc]\nserver=\"h:1\"\nnick=\"n\"\n[dialogue.mesh]\nenabled=true\nnats_url=\"nats://h:4222\"\nissuer_key={numeric}\n"
        ),
    );
    let err = load(&p, &p).unwrap_err();
    assert!(matches!(err, ConfigError::Mesh(_)), "{err:?}");
    let d = format!("{err}");
    let dbg = format!("{err:?}");
    assert!(!d.contains(numeric), "Display leaked the value: {d}");
    assert!(!dbg.contains(numeric), "Debug leaked the value: {dbg}");
    assert!(d.contains("is malformed"), "kind lost: {d}");
    assert_eq!(d.lines().count(), 1, "multi-line diagnostic: {d}");

    // A syntax error on the key's own line: the whole file is parsed by the
    // [irc] loader first, so it surfaces as the (source-free) Toml variant;
    // whichever loader reports it, the key must not be in it.
    let p = tmp(
        "meshsyntax.toml",
        &format!(
            "[irc]\nserver=\"h:1\"\nnick=\"n\"\n[dialogue.mesh]\nenabled=true\nissuer_key=\"{ISSUER_SENTINEL}\" oops\n"
        ),
    );
    let err = load(&p, &p).unwrap_err();
    assert!(
        matches!(err, ConfigError::Toml(..) | ConfigError::Mesh(_)),
        "{err:?}"
    );
    let d = format!("{err}");
    let dbg = format!("{err:?}");
    assert!(!d.contains(ISSUER_SENTINEL), "Display leaked the key: {d}");
    assert!(
        !dbg.contains(ISSUER_SENTINEL),
        "Debug leaked the key: {dbg}"
    );
    assert_eq!(d.lines().count(), 1, "multi-line diagnostic: {d}");
}

#[test]
fn wrong_typed_fields_never_echo_the_offending_value() {
    // An operator who forgets the quotes writes valid TOML whose value is an
    // integer. serde's own complaint for that is `invalid type: integer
    // `8675309`, expected a string` — which carries the intended password into
    // an error this crate documents as safe to log. The diagnostic must be
    // rebuilt from the field name and the expected type instead.
    let numeric = "8675309";
    let p = tmp(
        "numpw.toml",
        &format!("[irc]\nserver=\"h:1\"\nnick=\"n\"\nsasl_user=\"a\"\nsasl_password={numeric}\n"),
    );
    let err = load_irc(&p).unwrap_err();
    assert!(matches!(err, ConfigError::Malformed(_, _)), "{err:?}");
    let d = format!("{err}");
    let dbg = format!("{err:?}");
    assert!(!d.contains(numeric), "Display leaked the value: {d}");
    assert!(!dbg.contains(numeric), "Debug leaked the value: {dbg}");
    // Still diagnostic: the field and the type it wanted.
    assert!(d.contains("sasl_password"), "no field named: {d}");
    assert!(d.contains("string"), "no expected type: {d}");

    // The same holds for a wrong-typed value that IS a string: a boolean field
    // given the password by mistake must not echo it either.
    let p = tmp(
        "strbool.toml",
        &format!("[irc]\nserver=\"h:1\"\nnick=\"n\"\ntls=\"{SENTINEL}\"\n"),
    );
    let err = load_irc(&p).unwrap_err();
    assert!(matches!(err, ConfigError::Malformed(_, _)), "{err:?}");
    assert_no_secret(&err);
    assert!(format!("{err}").contains("boolean"), "{err}");
}

#[test]
fn unknown_field_diagnostics_name_the_key_but_carry_no_value() {
    let p = tmp(
        "unknownkey.toml",
        &format!("[irc]\nserver=\"h:1\"\nnick=\"n\"\nsasl_pasword=\"{SENTINEL}\"\n"),
    );
    let err = load_irc(&p).unwrap_err();
    assert!(matches!(err, ConfigError::Malformed(_, _)), "{err:?}");
    assert_no_secret(&err);
    assert!(
        format!("{err}").contains("sasl_pasword"),
        "the typo should be named: {err}"
    );
}

#[test]
fn mesh_url_credentials_are_redacted_in_debug() {
    // A NATS URL can carry authentication material in its userinfo, so the
    // "safe to log" Debug must not print it verbatim.
    let irc = load_irc(&tmp(
        "urlredact.toml",
        "[irc]\nserver=\"h:1\"\nnick=\"n\"\n",
    ))
    .unwrap();
    let cases = [
        (
            "nats://user:hunter2-URLPASS-DO-NOT-LEAK@nats.example.org:4222",
            "hunter2-URLPASS-DO-NOT-LEAK",
            "nats://<redacted>@nats.example.org:4222",
        ),
        (
            "nats://T0KEN-DO-NOT-LEAK-4Q@nats.example.org:4222",
            "T0KEN-DO-NOT-LEAK-4Q",
            "nats://<redacted>@nats.example.org:4222",
        ),
        // A comma is legal inside userinfo, so the list must not be split on
        // commas before the userinfo is found: this used to leak "SECRET".
        (
            "nats://user:SECRET-DO-NOT-LEAK,tail@nats.example.org:4222",
            "SECRET-DO-NOT-LEAK",
            "nats://<redacted>@nats.example.org:4222",
        ),
        // An '@' inside the password: no fragment of it survives either.
        (
            "nats://user:p@ss-DO-NOT-LEAK@nats.example.org:4222",
            "ss-DO-NOT-LEAK",
            "@nats.example.org:4222",
        ),
        // A two-URL list with credentials on both; each host stays readable.
        (
            "nats://a:ONE-DO-NOT-LEAK@h1:4222,nats://b:TWO-DO-NOT-LEAK@h2:4222",
            "ONE-DO-NOT-LEAK",
            "nats://<redacted>@h1:4222,nats://<redacted>@h2:4222",
        ),
        // Scheme-less form is accepted by the client, so it is redacted too.
        (
            "user:BARE-DO-NOT-LEAK@nats.example.org:4222",
            "BARE-DO-NOT-LEAK",
            "<redacted>@nats.example.org:4222",
        ),
    ];
    for (url, secret, expected) in cases {
        let cfg = GatewayConfig {
            irc: irc.clone(),
            mesh: MeshConfig {
                enabled: true,
                nats_url: url.to_string(),
                issuer_key: ISSUER_SENTINEL.to_string(),
            },
        };
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains(secret), "Debug leaked url credential: {dbg}");
        // Redacted, not dropped: host and port still diagnose the connection.
        assert!(dbg.contains(expected), "url over-redacted: {dbg}");
    }
    // A URL with no userinfo is untouched, so the ordinary case still reads.
    let cfg = GatewayConfig {
        irc,
        mesh: MeshConfig {
            enabled: true,
            nats_url: "nats://127.0.0.1:4222".to_string(),
            issuer_key: ISSUER_SENTINEL.to_string(),
        },
    };
    assert!(format!("{cfg:?}").contains("nats://127.0.0.1:4222"));
}

/// Assert an error's Display and Debug never contain the sentinel secret.
fn assert_no_secret(err: &ConfigError) {
    let d = format!("{err}");
    let dbg = format!("{err:?}");
    assert!(!d.contains(SENTINEL), "Display leaked secret: {d}");
    assert!(!dbg.contains(SENTINEL), "Debug leaked secret: {dbg}");
}

// ───────────────────────────── Identity folding ─────────────────────────────

#[test]
fn ascii_folding_is_case_only() {
    assert_eq!(fold_nick("Alice", CaseMapping::Ascii), "alice");
    // The rfc1459 "extra" characters are untouched under ascii.
    assert_eq!(fold_nick("A[]\\~", CaseMapping::Ascii), "a[]\\~");
}

#[test]
fn rfc1459_folds_bracket_group() {
    assert_eq!(fold_nick("Nick[]\\~", CaseMapping::Rfc1459), "nick{}|^");
    // Equivalent identities: differ only by IRC-equivalent case.
    assert_eq!(
        fold_nick("Foo[]", CaseMapping::Rfc1459),
        fold_nick("foo{}", CaseMapping::Rfc1459)
    );
    // Genuinely distinct nicks stay distinct.
    assert_ne!(
        fold_nick("foo", CaseMapping::Rfc1459),
        fold_nick("bar", CaseMapping::Rfc1459)
    );
}

#[test]
fn strict_rfc1459_keeps_tilde_and_caret_distinct() {
    // `[ ] \` still fold; `~` does NOT map to `^`.
    assert_eq!(fold_nick("[]\\", CaseMapping::StrictRfc1459), "{}|");
    assert_eq!(fold_nick("~", CaseMapping::StrictRfc1459), "~");
    assert_eq!(fold_nick("^", CaseMapping::StrictRfc1459), "^");
    // `~` and `^` are one identity under rfc1459 but two under strict.
    assert_eq!(
        fold_nick("~", CaseMapping::Rfc1459),
        fold_nick("^", CaseMapping::Rfc1459)
    );
    assert_ne!(
        fold_nick("~", CaseMapping::StrictRfc1459),
        fold_nick("^", CaseMapping::StrictRfc1459)
    );
}

#[test]
fn human_identity_folds_nick_but_not_account() {
    let peer = human_peer("Alice", CaseMapping::Rfc1459);
    assert_eq!(peer.human_nick(), Some("alice"));
    assert_eq!(peer.dm_subject(), "mu.agent.human.alice.dm");

    // Account metadata is kept beside identity, unfolded and unmerged.
    let id = human_identity("Alice", Some("AcctName"), CaseMapping::Rfc1459);
    assert_eq!(id.peer, PeerId::human("alice"));
    assert_eq!(id.account.as_deref(), Some("AcctName"));
    // No account is fine.
    let id = human_identity("Bob", None, CaseMapping::Ascii);
    assert_eq!(id.peer.human_nick(), Some("bob"));
    assert!(id.account.is_none());
}

// ───────────────────────────── Aliases & channels ───────────────────────────

#[test]
fn aliases_cover_every_role_shape() {
    assert_eq!(peer_alias(&PeerId::parse("cc:abc123")), "cc-abc123");
    assert_eq!(peer_alias(&PeerId::parse("mu:daemon7")), "mu-daemon7");
    assert_eq!(
        peer_alias(&PeerId::parse("mu:daemon7:sess2")),
        "mu-daemon7-sess2"
    );
    assert_eq!(peer_alias(&PeerId::human("alice")), "alice");
}

#[test]
fn channels_fit_and_are_deterministic() {
    let peer = PeerId::parse("cc:abc");
    let c = channel_for(&peer, "#", 50).unwrap();
    assert_eq!(c, "#cc-abc");
    // Same peer always maps to the same channel.
    assert_eq!(channel_for(&peer, "#", 50), channel_for(&peer, "#", 50));
}

#[test]
fn long_names_use_a_stable_hash_tail_within_budget() {
    let long = PeerId::mu_session(
        "daemon-with-a-very-long-id",
        "and-an-even-longer-session-name",
    );
    let len = 20;
    let c = channel_for(&long, "#", len).unwrap();
    assert!(c.len() <= len, "channel {c} exceeds CHANNELLEN {len}");
    // Deterministic: same peer, same channel, across calls.
    assert_eq!(channel_for(&long, "#", len), Some(c.clone()));
    // A different session yields a different channel (the hash covers the full
    // id), so distinct long peers do not silently share a channel.
    let other = PeerId::mu_session(
        "daemon-with-a-very-long-id",
        "a-different-long-session-name",
    );
    assert_ne!(channel_for(&other, "#", len), Some(c));
}

#[test]
fn tiny_channel_budget_never_panics() {
    let peer = PeerId::parse("cc:some-longish-id");
    for len in 0..=6 {
        let c = channel_for(&peer, "#", len).unwrap();
        assert!(c.len() <= len, "len {len}: {c:?} too long");
    }
}

#[test]
fn humans_never_get_a_channel() {
    assert_eq!(channel_for(&PeerId::human("alice"), "#", 50), None);
    // And a human is never returned by reverse resolution.
    let peers = vec![PeerId::human("alice"), PeerId::parse("cc:abc")];
    // The human's would-be subject-derived name is not a channel; resolving the
    // cc channel returns only the cc peer.
    let cc_channel = channel_for(&peers[1], "#", 50).unwrap();
    assert_eq!(
        resolve_channel(&cc_channel, &peers, "#", 50, CaseMapping::Rfc1459),
        Resolved::Peer(PeerId::parse("cc:abc"))
    );
}

#[test]
fn reverse_resolution_reports_unknown_and_ambiguous() {
    let p1 = PeerId::parse("cc:abc");
    // Unknown against an empty / non-matching snapshot.
    assert_eq!(
        resolve_channel("#cc-abc", &[], "#", 50, CaseMapping::Rfc1459),
        Resolved::Unknown
    );
    assert_eq!(
        resolve_channel(
            "#cc-abc",
            &[PeerId::parse("mu:d")],
            "#",
            50,
            CaseMapping::Rfc1459
        ),
        Resolved::Unknown
    );
    // Exactly one match.
    assert_eq!(
        resolve_channel(
            "#cc-abc",
            std::slice::from_ref(&p1),
            "#",
            50,
            CaseMapping::Rfc1459
        ),
        Resolved::Peer(p1)
    );
}

#[test]
fn alias_collisions_resolve_as_ambiguous_never_selecting() {
    // Two DISTINCT peers whose aliases collide: `mu:d:s` (role mu, id d, sub s)
    // and `mu:d-s` (role mu, id "d-s") both spell "mu-d-s".
    let a = PeerId::parse("mu:d:s");
    let b = PeerId::parse("mu:d-s");
    assert_eq!(
        peer_alias(&a),
        peer_alias(&b),
        "precondition: aliases collide"
    );
    let channel = channel_for(&a, "#", 50).unwrap();
    assert_eq!(channel, "#mu-d-s");

    let peers = vec![a.clone(), b.clone()];
    match resolve_channel(&channel, &peers, "#", 50, CaseMapping::Rfc1459) {
        Resolved::Ambiguous(hits) => {
            assert!(hits.contains(&a) && hits.contains(&b));
        }
        other => panic!("collision must be ambiguous, got {other:?}"),
    }
}

#[test]
fn resolution_tracks_the_live_snapshot() {
    let p = PeerId::parse("cc:xyz");
    let channel = channel_for(&p, "#", 50).unwrap();
    // Present in the snapshot -> resolves.
    assert_eq!(
        resolve_channel(
            &channel,
            std::slice::from_ref(&p),
            "#",
            50,
            CaseMapping::Rfc1459
        ),
        Resolved::Peer(p.clone())
    );
    // Gone from a later snapshot -> unknown, with no stored roster remembering it.
    assert_eq!(
        resolve_channel(&channel, &[], "#", 50, CaseMapping::Rfc1459),
        Resolved::Unknown
    );
}

#[test]
fn channel_resolution_folds_case_like_the_server() {
    let p = PeerId::parse("cc:Abc");
    assert_eq!(channel_for(&p, "#", 50).as_deref(), Some("#cc-Abc"));
    // A server echoes back whatever case the user typed and treats all of these
    // as one channel, so all of them must reach the same peer.
    for typed in ["#cc-Abc", "#cc-abc", "#CC-ABC", "#Cc-aBc"] {
        assert_eq!(
            resolve_channel(
                typed,
                std::slice::from_ref(&p),
                "#",
                50,
                CaseMapping::Rfc1459
            ),
            Resolved::Peer(p.clone()),
            "{typed} must resolve to the peer"
        );
    }
    // A genuinely different name still does not resolve.
    assert_eq!(
        resolve_channel(
            "#cc-abd",
            std::slice::from_ref(&p),
            "#",
            50,
            CaseMapping::Rfc1459
        ),
        Resolved::Unknown
    );
}

#[test]
fn channel_resolution_honours_rfc1459_bracket_equivalence() {
    let p = PeerId::parse("cc:a[b]");
    assert_eq!(channel_for(&p, "#", 50).as_deref(), Some("#cc-a[b]"));
    // Under rfc1459 `[ ] \` are the "upper case" forms of `{ } |`, for channel
    // names as much as for nicks.
    assert_eq!(
        resolve_channel(
            "#cc-a{b}",
            std::slice::from_ref(&p),
            "#",
            50,
            CaseMapping::Rfc1459
        ),
        Resolved::Peer(p.clone())
    );
    // A server advertising `ascii` folds only letters, so there it is a
    // different channel — proving the mapping is threaded, not assumed.
    assert_eq!(
        resolve_channel(
            "#cc-a{b}",
            std::slice::from_ref(&p),
            "#",
            50,
            CaseMapping::Ascii
        ),
        Resolved::Unknown
    );
}

#[test]
fn channels_colliding_only_under_folding_are_ambiguous() {
    // Two DISTINCT peers whose channels differ only by IRC-equivalent case. The
    // server cannot tell the two channels apart, so neither peer may be picked.
    let a = PeerId::parse("cc:abc");
    let b = PeerId::parse("cc:ABC");
    assert_ne!(
        channel_for(&a, "#", 50),
        channel_for(&b, "#", 50),
        "precondition: the channels are byte-distinct"
    );
    let peers = vec![a.clone(), b.clone()];
    match resolve_channel("#cc-abc", &peers, "#", 50, CaseMapping::Rfc1459) {
        Resolved::Ambiguous(hits) => assert!(hits.contains(&a) && hits.contains(&b)),
        other => panic!("folded collision must be ambiguous, got {other:?}"),
    }

    // The same pair under the bracket rule: ambiguous under rfc1459, and still
    // two separate channels under ascii.
    let l = PeerId::parse("cc:x[y");
    let r = PeerId::parse("cc:x{y");
    let peers = vec![l.clone(), r.clone()];
    assert!(matches!(
        resolve_channel("#cc-x{y", &peers, "#", 50, CaseMapping::Rfc1459),
        Resolved::Ambiguous(_)
    ));
    assert_eq!(
        resolve_channel("#cc-x{y", &peers, "#", 50, CaseMapping::Ascii),
        Resolved::Peer(r)
    );
}

// ───────────────────────────────── Framing ──────────────────────────────────

/// Reassemble a framed message back to its original body, given the fixed
/// prefix each line carries. Every non-final line ends with the continuation
/// marker, which is stripped before concatenation.
fn reassemble(lines: &[String], prefix: &str) -> String {
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        let body = line
            .strip_prefix(prefix)
            .and_then(|l| l.strip_suffix("\r\n"))
            .expect("line has the expected prefix and CRLF");
        if i + 1 < lines.len() {
            out.push_str(
                body.strip_suffix(CONTINUATION_MARKER)
                    .expect("marked continuation"),
            );
        } else {
            out.push_str(body);
        }
    }
    out
}

#[test]
fn short_message_is_one_line_with_crlf() {
    let params = FrameParams {
        target: "#chan",
        mesh_id: None,
        message_tags: false,
    };
    let lines = frame_privmsg(&params, "hello world").unwrap();
    assert_eq!(lines, vec!["PRIVMSG #chan :hello world\r\n".to_string()]);
}

#[test]
fn tag_is_emitted_only_when_negotiated() {
    let id = "01HXQ";
    // Negotiated + id present -> tag on the wire.
    let p = FrameParams {
        target: "#c",
        mesh_id: Some(id),
        message_tags: true,
    };
    let lines = frame_privmsg(&p, "hi").unwrap();
    assert_eq!(lines, vec!["@+mu.id=01HXQ PRIVMSG #c :hi\r\n".to_string()]);
    // Not negotiated -> no tag, even with an id.
    let p = FrameParams {
        target: "#c",
        mesh_id: Some(id),
        message_tags: false,
    };
    assert_eq!(
        frame_privmsg(&p, "hi").unwrap(),
        vec!["PRIVMSG #c :hi\r\n".to_string()]
    );
    // Negotiated but no id -> no tag.
    let p = FrameParams {
        target: "#c",
        mesh_id: None,
        message_tags: true,
    };
    assert_eq!(
        frame_privmsg(&p, "hi").unwrap(),
        vec!["PRIVMSG #c :hi\r\n".to_string()]
    );
}

#[test]
fn exact_512_byte_boundary() {
    let params = FrameParams {
        target: "#x",
        mesh_id: None,
        message_tags: false,
    };
    let prefix = "PRIVMSG #x :";
    let overhead = prefix.len() + 2; // + CRLF
    let fit = 512 - overhead;
    // A body that exactly fills the budget is one line of exactly 512 bytes.
    let body: String = "a".repeat(fit);
    let lines = frame_privmsg(&params, &body).unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].len(), 512);
    // One byte more spills to a second line; nothing is lost.
    let body: String = "a".repeat(fit + 1);
    let lines = frame_privmsg(&params, &body).unwrap();
    assert!(lines.len() >= 2);
    for l in &lines {
        assert!(l.len() <= 512, "line over budget: {}", l.len());
    }
    assert_eq!(reassemble(&lines, prefix), body);
}

#[test]
fn long_multibyte_message_splits_on_char_boundaries_without_loss() {
    let params = FrameParams {
        target: "#u",
        mesh_id: Some("01ID"),
        message_tags: true,
    };
    let prefix = "@+mu.id=01ID PRIVMSG #u :";
    // 4-byte scalars so a naive byte split would land mid-character; 400 of them
    // is ~1600 bytes, several lines.
    let body: String = "😀".repeat(400);
    let lines = frame_privmsg(&params, &body).unwrap();
    assert!(lines.len() >= 3);
    for l in &lines {
        assert!(l.len() <= 512, "line over budget: {}", l.len());
        // Each line is valid UTF-8 by construction (it is a String); the strong
        // check is lossless reassembly.
    }
    assert_eq!(
        reassemble(&lines, prefix),
        body,
        "content lost across split"
    );
}

#[test]
fn cr_lf_and_nul_injection_is_refused() {
    let params = FrameParams {
        target: "#c",
        mesh_id: None,
        message_tags: false,
    };
    for bad in ["a\r\nQUIT", "line1\nline2", "nul\0here", "\r"] {
        assert_eq!(
            frame_privmsg(&params, bad),
            Err(FramingError::ControlChar),
            "body {bad:?} must be refused"
        );
    }
    // A control character in the target is refused too.
    let params = FrameParams {
        target: "#c\r\nJOIN #evil",
        mesh_id: None,
        message_tags: false,
    };
    assert_eq!(frame_privmsg(&params, "hi"), Err(FramingError::ControlChar));
}

#[test]
fn empty_target_and_impossible_budget_are_errors() {
    let params = FrameParams {
        target: "",
        mesh_id: None,
        message_tags: false,
    };
    assert_eq!(frame_privmsg(&params, "hi"), Err(FramingError::EmptyTarget));
    // A target so long the overhead alone blows the 512-byte budget.
    let huge = "#".to_string() + &"z".repeat(600);
    let params = FrameParams {
        target: &huge,
        mesh_id: None,
        message_tags: false,
    };
    assert_eq!(
        frame_privmsg(&params, "hi"),
        Err(FramingError::BudgetTooSmall)
    );
}

#[test]
fn hostile_mesh_ids_cannot_inject_a_second_command_or_tag() {
    // The mesh id is attacker-influenced: `DmEnvelope.id` is an unrestricted
    // String that reaches framing verbatim, and it used to be interpolated raw
    // into the `@+mu.id=` tag. Each of these is a real injection shape.
    let cases = [
        "01H\r\nQUIT :owned",   // a whole second command
        "01H\nPRIVMSG evil :x", // a bare LF second command
        "01H ",                 // a space ends the tag list
        "01H QUIT",             // ...and hands the rest to the parser
        "01H;evil",             // a second tag
        "01H\\",                // an escape that would eat the separator
        "01H\\r\\nQUIT",        // an already-escaped-looking id
    ];
    for id in cases {
        let p = FrameParams {
            target: "#c",
            mesh_id: Some(id),
            message_tags: true,
        };
        let lines = frame_privmsg(&p, "hi").unwrap_or_else(|e| panic!("id {id:?}: {e}"));
        assert_eq!(lines.len(), 1, "id {id:?} produced {} lines", lines.len());
        let line = &lines[0];

        // Exactly one command: one CRLF, and it terminates the line.
        assert!(line.ends_with("\r\n"), "id {id:?}: {line:?}");
        assert_eq!(
            line.matches("\r\n").count(),
            1,
            "id {id:?} produced a second command: {line:?}"
        );
        assert!(
            !line[..line.len() - 2].contains(['\r', '\n', '\0']),
            "id {id:?} left a raw separator: {line:?}"
        );
        // Exactly one tag, and the single command after it is untouched: the
        // escaped id stays wholly inside the tag, before the first real space,
        // however much it looks like a command.
        let (tag, rest) = line.split_once(' ').expect("tag then command");
        assert!(tag.starts_with("@+mu.id="), "id {id:?}: {tag:?}");
        assert!(
            !tag.contains(';'),
            "id {id:?} injected a second tag: {tag:?}"
        );
        assert_eq!(rest, "PRIVMSG #c :hi\r\n", "id {id:?}: {rest:?}");
    }
}

#[test]
fn unrepresentable_mesh_ids_are_refused_not_mangled() {
    // A NUL has no IRCv3 tag escape at all, and a character outside the safe id
    // grammar has none either — both are refused rather than guessed at.
    for id in [
        "01H\0QUIT",             // NUL: no escape exists
        "\0",                    // ...even alone
        "01H;account=root",      // `=` is outside the safe id grammar
        "01H\nPRIVMSG #evil :x", // so is `#`, even where the LF would escape
        "01H\u{1}",              // a stray control character
        "01H\u{a0}",             // and anything non-ASCII
    ] {
        let p = FrameParams {
            target: "#c",
            mesh_id: Some(id),
            message_tags: true,
        };
        assert_eq!(
            frame_privmsg(&p, "hi"),
            Err(FramingError::UnsafeMeshId),
            "id {id:?} must be refused"
        );
    }
    // Refusing the TAG never refuses the message: the same body frames fine
    // when the caller drops the tag, which is the documented fallback.
    let p = FrameParams {
        target: "#c",
        mesh_id: Some("01H\0QUIT"),
        message_tags: false,
    };
    assert_eq!(
        frame_privmsg(&p, "hi").unwrap(),
        vec!["PRIVMSG #c :hi\r\n".to_string()]
    );
    // An ordinary ULID-shaped id is untouched by any of this.
    let p = FrameParams {
        target: "#c",
        mesh_id: Some("01J9ZC4M8QK7XW2V5N3B6TYRHD"),
        message_tags: true,
    };
    assert_eq!(
        frame_privmsg(&p, "hi").unwrap(),
        vec!["@+mu.id=01J9ZC4M8QK7XW2V5N3B6TYRHD PRIVMSG #c :hi\r\n".to_string()]
    );
}

#[test]
fn the_escaped_mesh_id_is_what_the_line_budget_counts() {
    // Every character of this id doubles under escaping. Budgeting the RAW id
    // would under-count the prefix by 100 bytes and push lines past 512.
    let id = " ".repeat(100);
    let params = FrameParams {
        target: "#c",
        mesh_id: Some(&id),
        message_tags: true,
    };
    let body = "b".repeat(600);
    let lines = frame_privmsg(&params, &body).unwrap();
    assert!(lines.len() >= 2, "expected a split");
    for l in &lines {
        assert!(l.len() <= 512, "line over budget: {}", l.len());
    }
    // The prefix really is the escaped form, and nothing was lost splitting it.
    let prefix = format!("@+mu.id={} PRIVMSG #c :", r"\s".repeat(100));
    assert_eq!(reassemble(&lines, &prefix), body);
}

#[test]
fn empty_body_is_a_single_empty_privmsg() {
    let params = FrameParams {
        target: "#c",
        mesh_id: None,
        message_tags: false,
    };
    assert_eq!(
        frame_privmsg(&params, "").unwrap(),
        vec!["PRIVMSG #c :\r\n".to_string()]
    );
}

#[test]
fn multi_target_and_parameter_delimiters_in_the_target_are_refused() {
    // Neither of these injects a second command, so the CR/LF check passes
    // them — but each changes what the message means. `#a,#b` is an IRC target
    // LIST, delivering a body meant for one channel to two; `#chan :x` ends the
    // target parameter early, so `x` becomes the trailing payload and the real
    // body is appended after it.
    for bad in [
        "#a,#b",
        "#chan :x",
        "#chan extra",
        ":#chan",
        "alice,bob",
        " #chan",
    ] {
        let params = FrameParams {
            target: bad,
            mesh_id: None,
            message_tags: false,
        };
        assert_eq!(
            frame_privmsg(&params, "secret"),
            Err(FramingError::InvalidTarget),
            "target {bad:?} must be refused"
        );
    }
    // Ordinary channel and nick targets are unaffected.
    for good in ["#chan", "alice", "#mu-agents-cc", "guest[1]"] {
        let params = FrameParams {
            target: good,
            mesh_id: None,
            message_tags: false,
        };
        assert_eq!(
            frame_privmsg(&params, "hi").unwrap(),
            vec![format!("PRIVMSG {good} :hi\r\n")],
            "target {good:?} must frame"
        );
    }
}
