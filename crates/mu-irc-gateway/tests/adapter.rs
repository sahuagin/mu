//! Offline adapter tests for increment 2b: the registration/capability state
//! machine, ISUPPORT tracking, and — above all — that no credential, raw or
//! base64-encoded, survives in any printable state. No socket, no IRC client:
//! every check drives the state machine with parsed lines.

use std::time::{Duration, UNIX_EPOCH};

use mu_irc_gateway::adapter::{
    AdapterError, FixedClock, IrcMessage, IsupportSettings, Registration, Step,
};
use mu_irc_gateway::config::{
    load_irc, validate_nick, ConfigError, IrcConfig, NickFault, SaslCreds, Secret, NICK_MAX_LEN,
};
use mu_irc_gateway::framing::{frame_privmsg, FrameParams};
use mu_irc_gateway::mapping::CaseMapping;
use mu_irc_gateway::transport::TlsTrust;

/// The one password value that must never surface off the wire. The `AUTHENTICATE`
/// line legitimately carries its base64 form; nothing else may.
const SECRET: &str = "hunter2-DO-NOT-LEAK-9Z";

fn cfg(sasl: bool, tls: bool) -> IrcConfig {
    IrcConfig {
        server: "irc.example.org:6697".into(),
        tls,
        // The adapter never touches the trust store: it decides lines, and the
        // transport is what verifies a certificate.
        tls_trust: TlsTrust::default(),
        nick: "mu-gw".into(),
        sasl: sasl.then(|| SaslCreds {
            user: "acct".into(),
            password: Secret::new(SECRET),
        }),
        channel_prefix: "#".into(),
        lobby: "#mu".into(),
        observe_agent_dms: true,
    }
}

fn clock() -> FixedClock {
    FixedClock(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
}

/// Feed a raw line and unwrap the resulting step.
fn feed(reg: &mut Registration<FixedClock>, line: &str) -> Step {
    reg.on_message(&IrcMessage::parse(line)).unwrap()
}

// ─────────────────────────────── Parsing ────────────────────────────────────

#[test]
fn parses_tags_prefix_command_and_trailing() {
    let m = IrcMessage::parse("@account=bob;+mu.id=01H :nick!u@h PRIVMSG #c :hello there");
    assert_eq!(m.tag("account"), Some("bob"));
    assert_eq!(m.tag("+mu.id"), Some("01H"));
    assert_eq!(m.prefix.as_deref(), Some("nick!u@h"));
    assert_eq!(m.command, "PRIVMSG");
    assert_eq!(m.params, vec!["#c".to_string(), "hello there".to_string()]);
}

#[test]
fn unescapes_tag_values() {
    let m = IrcMessage::parse(r"@k=a\sb\:c\\d PING x");
    assert_eq!(m.tag("k"), Some("a b;c\\d"));
}

#[test]
fn a_run_of_separator_spaces_does_not_shift_the_command() {
    // RFC 1459 permits more than one space between components. Consuming only
    // one left the command empty and the real command in the parameters, so a
    // welcome sent this way never registered the connection.
    let m = IrcMessage::parse(":srv  001 mu-gw :Welcome");
    assert_eq!(m.prefix.as_deref(), Some("srv"));
    assert_eq!(m.command, "001");
    assert_eq!(m.params, vec!["mu-gw".to_string(), "Welcome".to_string()]);

    // The same run between the tag section and the prefix.
    let m = IrcMessage::parse("@tag=v  :src PRIVMSG #c :hi");
    assert_eq!(m.tag("tag"), Some("v"));
    assert_eq!(m.prefix.as_deref(), Some("src"));
    assert_eq!(m.command, "PRIVMSG");
    assert_eq!(m.params, vec!["#c".to_string(), "hi".to_string()]);

    // …and a registration driven by such a line does become ready.
    let (mut reg, _) = Registration::start(&cfg(false, true), clock()).unwrap();
    feed(&mut reg, "CAP * LS :");
    let s = feed(&mut reg, ":srv   001 mu-gw :Welcome");
    assert!(s.became_ready);
    assert!(reg.is_ready());
}

#[test]
fn only_separators_are_collapsed_never_the_trailing_parameter() {
    // The trailing parameter is the message body: its own double spaces are
    // content and must survive verbatim.
    let m = IrcMessage::parse(":src  PRIVMSG  #c  :two  spaces  inside ");
    assert_eq!(m.prefix.as_deref(), Some("src"));
    assert_eq!(m.command, "PRIVMSG");
    assert_eq!(
        m.params,
        vec!["#c".to_string(), "two  spaces  inside ".to_string()]
    );
}

// ─────────────────────────── Registration flow ──────────────────────────────

#[test]
fn start_emits_cap_nick_user() {
    let (reg, lines) = Registration::start(&cfg(false, true), clock()).unwrap();
    assert_eq!(
        lines,
        vec![
            "CAP LS 302".to_string(),
            "NICK mu-gw".to_string(),
            "USER mu-gw 0 * :mu-gw".to_string(),
        ]
    );
    assert!(!reg.is_ready());
    assert!(reg.connect_request().tls);
}

#[test]
fn no_sasl_negotiates_optional_caps_and_becomes_ready() {
    let (mut reg, _) = Registration::start(&cfg(false, true), clock()).unwrap();
    let s = feed(&mut reg, "CAP * LS :message-tags account-tag foo");
    assert_eq!(s.out, vec!["CAP REQ :message-tags account-tag".to_string()]);
    let s = feed(&mut reg, "CAP * ACK :message-tags account-tag");
    assert_eq!(s.out, vec!["CAP END".to_string()]);
    assert!(reg.negotiated().message_tags);
    assert!(reg.negotiated().account_tag);
    assert!(!reg.is_ready());
    let s = feed(&mut reg, ":srv 001 mu-gw :Welcome");
    assert!(s.became_ready);
    assert!(reg.is_ready());
}

#[test]
fn no_advertised_caps_ends_negotiation_immediately() {
    let (mut reg, _) = Registration::start(&cfg(false, true), clock()).unwrap();
    // Server offers nothing the gateway wants: straight to CAP END.
    let s = feed(&mut reg, "CAP * LS :");
    assert_eq!(s.out, vec!["CAP END".to_string()]);
    assert!(!reg.negotiated().message_tags);
    feed(&mut reg, ":srv 001 mu-gw :hi");
    assert!(reg.is_ready());
}

#[test]
fn multiline_cap_ls_accumulates_before_requesting() {
    let (mut reg, _) = Registration::start(&cfg(true, true), clock()).unwrap();
    // First line is continued (`*`), so no request yet.
    let s = feed(&mut reg, "CAP * LS * :message-tags");
    assert!(s.out.is_empty(), "continued LS must not request yet");
    let s = feed(&mut reg, "CAP * LS :sasl");
    assert_eq!(s.out, vec!["CAP REQ :message-tags sasl".to_string()]);
}

// ───────────────────────────────── SASL ─────────────────────────────────────

#[test]
fn sasl_without_tls_is_refused_before_any_line() {
    let err = Registration::start(&cfg(true, false), clock()).unwrap_err();
    assert_eq!(err, AdapterError::SaslWithoutTls);
}

#[test]
fn configured_sasl_must_be_offered_by_the_server() {
    let (mut reg, _) = Registration::start(&cfg(true, true), clock()).unwrap();
    let err = reg
        .on_message(&IrcMessage::parse("CAP * LS :message-tags"))
        .unwrap_err();
    assert_eq!(err, AdapterError::SaslUnsupported);
}

#[test]
fn sasl_nak_is_fatal() {
    let (mut reg, _) = Registration::start(&cfg(true, true), clock()).unwrap();
    feed(&mut reg, "CAP * LS :sasl");
    let err = reg
        .on_message(&IrcMessage::parse("CAP * NAK :sasl"))
        .unwrap_err();
    assert_eq!(err, AdapterError::SaslRejected);
}

#[test]
fn full_sasl_plain_handshake_succeeds() {
    let (mut reg, _) = Registration::start(&cfg(true, true), clock()).unwrap();
    feed(&mut reg, "CAP * LS :sasl");
    let s = feed(&mut reg, "CAP * ACK :sasl");
    assert_eq!(s.out, vec!["AUTHENTICATE PLAIN".to_string()]);
    let s = feed(&mut reg, "AUTHENTICATE +");
    assert_eq!(s.out.len(), 1);
    let auth = &s.out[0];
    // The PLAIN frame is base64 of `\0acct\0<secret>`.
    let expected = {
        use base64::Engine as _;
        let mut raw = vec![0u8];
        raw.extend_from_slice(b"acct");
        raw.push(0);
        raw.extend_from_slice(SECRET.as_bytes());
        format!(
            "AUTHENTICATE {}",
            base64::engine::general_purpose::STANDARD.encode(raw)
        )
    };
    assert_eq!(auth, &expected);
    // 900 is informational; 903 completes and ends CAP negotiation.
    assert!(feed(&mut reg, ":srv 900 mu-gw acct :logged in")
        .out
        .is_empty());
    let s = feed(&mut reg, ":srv 903 mu-gw :SASL authentication successful");
    assert_eq!(s.out, vec!["CAP END".to_string()]);
    feed(&mut reg, ":srv 001 mu-gw :Welcome");
    assert!(reg.is_ready());
}

#[test]
fn sasl_failure_numeric_is_fatal() {
    let (mut reg, _) = Registration::start(&cfg(true, true), clock()).unwrap();
    feed(&mut reg, "CAP * LS :sasl");
    feed(&mut reg, "CAP * ACK :sasl");
    feed(&mut reg, "AUTHENTICATE +");
    let err = reg
        .on_message(&IrcMessage::parse(":srv 904 mu-gw :SASL failed"))
        .unwrap_err();
    assert_eq!(err, AdapterError::SaslFailed);
}

// ───────────────────────── No credential ever leaks ─────────────────────────

#[test]
fn no_secret_appears_in_any_diagnostic_debug_or_error_path() {
    let (mut reg, start_lines) = Registration::start(&cfg(true, true), clock()).unwrap();
    // The startup lines and the running Debug are secret-free from the outset.
    for l in &start_lines {
        assert!(!l.contains(SECRET));
    }
    assert_no_secret(&format!("{reg:?}"));

    feed(&mut reg, "CAP * LS :sasl message-tags");
    feed(&mut reg, "CAP * ACK :sasl message-tags");
    let s = feed(&mut reg, "AUTHENTICATE +");
    // The one place the credential legitimately appears is the AUTHENTICATE
    // wire line — capture its base64 so we can prove it shows up NOWHERE else.
    let b64 = s.out[0].strip_prefix("AUTHENTICATE ").unwrap().to_string();
    assert!(!b64.is_empty());

    let s = feed(&mut reg, ":srv 903 mu-gw :ok");
    // Diagnostics carry neither the plaintext nor the base64.
    let diag = s.diagnostic.expect("903 emits a success diagnostic");
    let diag_s = format!("{diag} {diag:?}");
    assert_no_secret(&diag_s);
    assert!(!diag_s.contains(&b64), "diagnostic leaked the SASL payload");

    feed(&mut reg, ":srv 001 mu-gw :hi");
    // A full Debug dump of the ready machine holds no credential in any form.
    let dbg = format!("{reg:?}");
    assert_no_secret(&dbg);
    assert!(!dbg.contains(&b64), "Debug leaked the SASL payload: {dbg}");
    // ...yet the machine still knows SASL was configured.
    assert!(dbg.contains("sasl_configured: true"));
}

/// A SASL failure error, and the whole failed machine, are also secret-free.
#[test]
fn failure_paths_are_secret_free() {
    let (mut reg, _) = Registration::start(&cfg(true, true), clock()).unwrap();
    feed(&mut reg, "CAP * LS :sasl");
    feed(&mut reg, "CAP * ACK :sasl");
    feed(&mut reg, "AUTHENTICATE +");
    let err = reg
        .on_message(&IrcMessage::parse(":srv 905 mu-gw :too long"))
        .unwrap_err();
    assert_no_secret(&format!("{err} {err:?}"));
    assert_no_secret(&format!("{reg:?}"));
}

/// A long password, so the base64 PLAIN frame is worth chunking.
fn cfg_with_password(pw: &str) -> IrcConfig {
    let mut c = cfg(true, true);
    c.sasl = Some(SaslCreds {
        user: "acct".into(),
        password: Secret::new(pw),
    });
    c
}

/// Drives a fresh machine into one pre-welcome phase, so a test can check the
/// same ordering rule in every phase that precedes the welcome.
type DriveToPhase = fn(&mut Registration<FixedClock>);

/// Drive a configured-SASL machine to the point where `AUTHENTICATE +` arrives.
fn to_challenge(reg: &mut Registration<FixedClock>) {
    feed(reg, "CAP * LS :sasl");
    feed(reg, "CAP * ACK :sasl");
}

#[test]
fn step_debug_never_prints_the_authenticate_payload() {
    // `out` is the ONE structure that legitimately carries the base64 SASL
    // frame. A caller logging the step (or the Result around it) must not get a
    // reversibly-encoded password out of it.
    let (mut reg, _) = Registration::start(&cfg(true, true), clock()).unwrap();
    to_challenge(&mut reg);
    let step = feed(&mut reg, "AUTHENTICATE +");
    let b64 = step.out[0]
        .strip_prefix("AUTHENTICATE ")
        .unwrap()
        .to_string();
    assert!(!b64.is_empty(), "precondition: a payload was produced");

    let dbg = format!("{step:?}");
    assert_no_secret(&dbg);
    assert!(
        !dbg.contains(&b64),
        "Step Debug leaked the SASL payload: {dbg}"
    );
    assert!(dbg.contains("AUTHENTICATE <redacted>"), "{dbg}");
    // The same holds through the Result a caller is most likely to log.
    let res: Result<Step, AdapterError> = Ok(step);
    let dbg = format!("{res:?}");
    assert!(
        !dbg.contains(&b64),
        "Result Debug leaked the SASL payload: {dbg}"
    );

    // Protocol constants are not secrets and still read normally, so the
    // redaction does not blind the log.
    let (mut reg, _) = Registration::start(&cfg(true, true), clock()).unwrap();
    feed(&mut reg, "CAP * LS :sasl");
    let step = feed(&mut reg, "CAP * ACK :sasl");
    assert!(
        format!("{step:?}").contains("AUTHENTICATE PLAIN"),
        "{step:?}"
    );
}

#[test]
fn mandatory_sasl_fails_closed_when_the_ack_omits_it() {
    // The server offers sasl, the gateway requests it, and the ACK comes back
    // naming only the optional caps. Treating that as "no SASL to do" would
    // register an unauthenticated connection with credentials configured.
    let (mut reg, _) = Registration::start(&cfg(true, true), clock()).unwrap();
    feed(&mut reg, "CAP * LS :sasl message-tags");
    let err = reg
        .on_message(&IrcMessage::parse("CAP * ACK :message-tags"))
        .unwrap_err();
    assert_eq!(err, AdapterError::SaslNotCompleted);
    // Terminal: a following 001 cannot resurrect the connection.
    assert!(feed(&mut reg, ":srv 001 mu-gw :Welcome").out.is_empty());
    assert!(!reg.is_ready(), "registered without authenticating");
}

#[test]
fn mandatory_sasl_fails_closed_when_the_nak_omits_it() {
    let (mut reg, _) = Registration::start(&cfg(true, true), clock()).unwrap();
    feed(&mut reg, "CAP * LS :sasl message-tags");
    let err = reg
        .on_message(&IrcMessage::parse("CAP * NAK :message-tags"))
        .unwrap_err();
    assert_eq!(err, AdapterError::SaslNotCompleted);
    assert!(!reg.is_ready());
}

#[test]
fn a_welcome_numeric_cannot_outrun_the_sasl_exchange() {
    // Once credentials are configured, a 001 arriving before 903 ends the
    // handshake in failure — in EVERY phase that precedes the welcome, not just
    // the one phase that expects a welcome. Ignoring it left the machine alive
    // and recoverable by a later 903, which is the ordering the mandatory-SASL
    // invariant exists to forbid.
    let phases: [(&str, DriveToPhase); 4] = [
        ("CapList", |_reg| {}),
        ("CapAck", |reg| {
            feed(reg, "CAP * LS :sasl message-tags");
        }),
        ("SaslChallenge", to_challenge),
        ("SaslResult", |reg| {
            to_challenge(reg);
            feed(reg, "AUTHENTICATE +");
        }),
    ];
    for (phase, drive) in phases {
        let (mut reg, _) = Registration::start(&cfg(true, true), clock()).unwrap();
        drive(&mut reg);
        let err = reg
            .on_message(&IrcMessage::parse(":srv 001 mu-gw :Welcome"))
            .unwrap_err();
        assert_eq!(err, AdapterError::SaslNotCompleted, "phase {phase}");
        assert!(!reg.is_ready(), "phase {phase}: registered unauthenticated");
        // Terminal: the exchange cannot resume and 001 cannot be replayed.
        assert!(
            feed(&mut reg, ":srv 903 mu-gw :ok").out.is_empty(),
            "phase {phase}: a failed handshake accepted a late 903"
        );
        assert!(feed(&mut reg, ":srv 001 mu-gw :Welcome").out.is_empty());
        assert!(!reg.is_ready(), "phase {phase}");
    }
}

#[test]
fn an_early_welcome_without_sasl_ends_negotiation_and_registers() {
    // With no credentials nothing is outstanding, so a server that jumps to the
    // welcome numeric mid-negotiation has simply ended negotiation for us. The
    // machine still closes CAP through the same choke point rather than leaving
    // the server waiting on a `CAP END` the protocol says it is owed.
    let (mut reg, _) = Registration::start(&cfg(false, true), clock()).unwrap();
    feed(&mut reg, "CAP * LS :message-tags");
    let s = feed(&mut reg, ":srv 001 mu-gw :Welcome");
    assert_eq!(s.out, vec!["CAP END".to_string()], "{s:?}");
    assert!(s.became_ready, "{s:?}");
    assert!(reg.is_ready());
}

#[test]
fn an_unsolicited_sasl_ack_is_ignored_and_never_panics() {
    // SASL is NOT configured. A server that ACKs `sasl` anyway used to set
    // sasl_acked, enter the challenge phase, and panic on the next
    // `AUTHENTICATE +` at an expect() that claimed the phase implied creds.
    let (mut reg, _) = Registration::start(&cfg(false, true), clock()).unwrap();
    feed(&mut reg, "CAP * LS :message-tags");
    let s = feed(&mut reg, "CAP * ACK :sasl");
    // Negotiation ends normally; the bogus ACK is noted, not obeyed.
    assert_eq!(s.out, vec!["CAP END".to_string()]);
    let diag = s.diagnostic.expect("the ignored ACK is worth a diagnostic");
    assert!(diag.message.contains("unsolicited"), "{diag}");
    // The challenge that used to panic is now just an unremarkable message.
    let s = feed(&mut reg, "AUTHENTICATE +");
    assert!(s.out.is_empty(), "{s:?}");
    feed(&mut reg, ":srv 001 mu-gw :Welcome");
    assert!(reg.is_ready(), "an unconfigured connection still registers");
}

#[test]
fn terminal_sasl_numerics_are_fatal_during_the_challenge_phase() {
    // A server can refuse `AUTHENTICATE PLAIN` before ever sending the `+`
    // challenge. Ignoring those numerics left the machine hung in the challenge
    // phase, since the handling lived only in the phase after it.
    // The 908 line here advertises no mechanisms at all, so it is a refusal;
    // `a_908_that_lists_plain_is_informational` covers the other reading.
    for numeric in ["902", "904", "905", "906", "908"] {
        let (mut reg, _) = Registration::start(&cfg(true, true), clock()).unwrap();
        to_challenge(&mut reg);
        let err = reg
            .on_message(&IrcMessage::parse(&format!(":srv {numeric} mu-gw :no")))
            .unwrap_err();
        assert_eq!(err, AdapterError::SaslFailed, "numeric {numeric}");
        assert!(!reg.is_ready(), "numeric {numeric}");
        // Terminal: nothing further advances the machine.
        assert!(feed(&mut reg, "AUTHENTICATE +").out.is_empty());
    }
}

#[test]
fn a_908_that_lists_plain_is_informational() {
    // RPL_SASLMECHS advertises what the server supports. When PLAIN is in that
    // list, the one mechanism this gateway speaks is available: the numeric is
    // informational, and aborting on it would kill a viable exchange.
    let (mut reg, _) = Registration::start(&cfg(true, true), clock()).unwrap();
    to_challenge(&mut reg);
    // The documented shape: the list is the second parameter, and mechanism
    // names compare case-insensitively.
    let s = feed(
        &mut reg,
        ":srv 908 mu-gw EXTERNAL,plain :are available SASL mechanisms",
    );
    assert!(s.out.is_empty(), "{s:?}");
    // The exchange proceeds exactly as if the 908 had never arrived.
    assert_eq!(feed(&mut reg, "AUTHENTICATE +").out.len(), 1);
    // The shorter shape, in the result phase: the list is the trailing param.
    let s = feed(&mut reg, ":srv 908 * :PLAIN");
    assert!(s.out.is_empty(), "{s:?}");
    assert_eq!(
        feed(&mut reg, ":srv 903 mu-gw :ok").out,
        vec!["CAP END".to_string()]
    );
    feed(&mut reg, ":srv 001 mu-gw :Welcome");
    assert!(reg.is_ready(), "an informational 908 aborted the exchange");
}

#[test]
fn a_908_that_omits_plain_is_terminal_in_both_sasl_phases() {
    // The same numeric, listing only mechanisms the gateway cannot speak, is a
    // refusal: PLAIN is off the table and the exchange cannot succeed.
    for stage in ["challenge", "result"] {
        let (mut reg, _) = Registration::start(&cfg(true, true), clock()).unwrap();
        to_challenge(&mut reg);
        if stage == "result" {
            feed(&mut reg, "AUTHENTICATE +");
        }
        let err = reg
            .on_message(&IrcMessage::parse(
                ":srv 908 mu-gw EXTERNAL,SCRAM-SHA-256 :are available SASL mechanisms",
            ))
            .unwrap_err();
        assert_eq!(err, AdapterError::SaslFailed, "stage {stage}");
        assert!(!reg.is_ready(), "stage {stage}");
    }
}

// ──────────────────── The server refuses the offered nick ───────────────────

#[test]
fn nickname_in_use_while_awaiting_the_welcome_ends_registration() {
    // 433 is the routine one: the nick is taken. The welcome numeric will never
    // arrive, so continuing to wait for it hangs the connection forever.
    let (mut reg, _) = Registration::start(&cfg(false, true), clock()).unwrap();
    feed(&mut reg, "CAP * LS :message-tags");
    feed(&mut reg, "CAP * ACK :message-tags");
    let err = reg
        .on_message(&IrcMessage::parse(
            ":srv 433 * mu-gw :Nickname is already in use",
        ))
        .unwrap_err();
    assert_eq!(
        err,
        AdapterError::NickRejected {
            numeric: "433".into(),
            nick: "mu-gw".into(),
        }
    );
    // The failure names both, so an operator can act on the log line alone.
    let text = err.to_string();
    assert!(text.contains("433") && text.contains("mu-gw"), "{text}");
    // v0 sends no replacement NICK, and the machine is terminal.
    assert!(feed(&mut reg, ":srv 001 mu-gw :Welcome").out.is_empty());
    assert!(!reg.is_ready());
}

#[test]
fn an_erroneous_nickname_during_cap_negotiation_ends_registration() {
    // 432 can arrive before the CAP list does: the rejection is not tied to the
    // phase the handshake happens to be in.
    let (mut reg, _) = Registration::start(&cfg(false, true), clock()).unwrap();
    let err = reg
        .on_message(&IrcMessage::parse(":srv 432 * mu-gw :Erroneous Nickname"))
        .unwrap_err();
    assert_eq!(
        err,
        AdapterError::NickRejected {
            numeric: "432".into(),
            nick: "mu-gw".into(),
        }
    );
    assert!(feed(&mut reg, "CAP * LS :message-tags").out.is_empty());
    assert!(!reg.is_ready());
}

#[test]
fn an_unavailable_nick_during_the_sasl_exchange_ends_registration() {
    // 437 mid-exchange: authenticating is moot once the nick is refused, so the
    // rejection wins over the SASL phase rather than being dropped by it.
    let (mut reg, _) = Registration::start(&cfg(true, true), clock()).unwrap();
    to_challenge(&mut reg);
    feed(&mut reg, "AUTHENTICATE +");
    let err = reg
        .on_message(&IrcMessage::parse(
            ":srv 437 * mu-gw :Nick/channel is temporarily unavailable",
        ))
        .unwrap_err();
    assert_eq!(
        err,
        AdapterError::NickRejected {
            numeric: "437".into(),
            nick: "mu-gw".into(),
        }
    );
    assert_no_secret(&format!("{err} {err:?}"));
    assert!(feed(&mut reg, ":srv 903 mu-gw :ok").out.is_empty());
    assert!(!reg.is_ready());
}

#[test]
fn the_sasl_response_is_chunked_at_400_characters() {
    // IRCv3 caps one AUTHENTICATE parameter at 400 base64 characters, and reads
    // a short chunk as the end of the response — so a response that is an exact
    // multiple of 400 needs a lone `AUTHENTICATE +` to terminate it.
    //
    // The PLAIN frame is `\0acct\0<password>`, i.e. 6 + password bytes, and
    // base64 encodes ceil(n/3) groups into 4 characters each. Choosing password
    // lengths that make the encoded length land exactly on 399/400/401/800.
    let encoded_len = |pw_len: usize| 4 * ((6 + pw_len).div_ceil(3));
    let pw_for = |target: usize| {
        (1..4096)
            .find(|n| encoded_len(*n) == target)
            .unwrap_or_else(|| panic!("no password length encodes to {target}"))
    };

    for (target, expect) in [
        // Under the chunk size: a single line, no terminator.
        (396_usize, vec![396_usize]),
        // Exactly one full chunk: the terminator is REQUIRED, or the server
        // waits for a continuation that never arrives.
        (400, vec![400, 0]),
        // One over: a full chunk plus a short one, which itself terminates.
        (404, vec![400, 4]),
        // Two full chunks, again needing the terminator.
        (800, vec![400, 400, 0]),
    ] {
        let pw = "p".repeat(pw_for(target));
        let (mut reg, _) = Registration::start(&cfg_with_password(&pw), clock()).unwrap();
        to_challenge(&mut reg);
        let step = feed(&mut reg, "AUTHENTICATE +");
        let payloads: Vec<&str> = step
            .out
            .iter()
            .map(|l| {
                l.strip_prefix("AUTHENTICATE ")
                    .expect("an AUTHENTICATE line")
            })
            .collect();
        let shapes: Vec<usize> = payloads
            .iter()
            .map(|p| if *p == "+" { 0 } else { p.len() })
            .collect();
        assert_eq!(shapes, expect, "encoded length {target}: {shapes:?}");
        for p in &payloads {
            assert!(p.len() <= 400, "chunk over 400: {}", p.len());
        }
        // Nothing was lost or duplicated: the chunks rejoin to the whole frame.
        let rejoined: String = payloads.iter().filter(|p| **p != "+").copied().collect();
        assert_eq!(rejoined.len(), target);
        let expected = {
            use base64::Engine as _;
            let mut raw = vec![0u8];
            raw.extend_from_slice(b"acct");
            raw.push(0);
            raw.extend_from_slice(pw.as_bytes());
            base64::engine::general_purpose::STANDARD.encode(raw)
        };
        assert_eq!(rejoined, expected, "encoded length {target}");
        // The exchange still completes normally afterwards.
        let s = feed(&mut reg, ":srv 903 mu-gw :ok");
        assert_eq!(s.out, vec!["CAP END".to_string()]);
    }
}

#[test]
fn an_unusable_nick_is_refused_by_the_loader_and_by_the_adapter() {
    // The config loader is the first gate. Nothing here is in the RFC 2812
    // nickname grammar, so nothing here survives it — including every byte that
    // could inject a second protocol line or move a parameter.
    for (i, (bad, fault)) in [
        ("mu gw", NickFault::BadCharacter),
        ("mu\r\nJOIN #evil", NickFault::BadCharacter),
        ("mu\0gw", NickFault::BadCharacter),
        ("mu,gw", NickFault::BadCharacter),
        ("mu.gw", NickFault::BadCharacter),
        (":mugw", NickFault::BadStart),
        // A nick may not start with a digit, though it may contain one.
        ("9mugw", NickFault::BadStart),
        // ...and it is ASCII, whatever the terminal can render.
        ("mü-gw", NickFault::NotAscii),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(validate_nick(bad), Err(fault), "nick {bad:?}");
        let p = std::env::temp_dir().join(format!("mu-irc-nick-{i}.toml"));
        std::fs::write(
            &p,
            format!("[irc]\nserver=\"h:1\"\nnick={}\n", toml_string(bad)),
        )
        .unwrap();
        match load_irc(&p) {
            Err(ConfigError::InvalidNick(f)) => assert_eq!(f, fault, "nick {bad:?}"),
            other => panic!("nick {bad:?} loaded: {other:?}"),
        }
    }
    assert_eq!(
        validate_nick(&"n".repeat(NICK_MAX_LEN + 1)),
        Err(NickFault::TooLong)
    );
    // The cap is a number in the diagnostic, not something the operator has to
    // go and look up.
    assert!(
        NickFault::TooLong
            .to_string()
            .contains(&NICK_MAX_LEN.to_string()),
        "the length fault must name the cap: {}",
        NickFault::TooLong
    );

    // The whole grammar is accepted, not just the alphanumeric part of it: the
    // RFC 2812 specials are ordinary nickname characters, in first position too.
    let rfc = "[mu]-gw_^{|}\\`9";
    assert_eq!(validate_nick(rfc), Ok(()), "nick {rfc:?}");
    let good = std::env::temp_dir().join("mu-irc-nick-rfc.toml");
    std::fs::write(
        &good,
        format!("[irc]\nserver=\"h:1\"\nnick={}\n", toml_string(rfc)),
    )
    .unwrap();
    assert_eq!(load_irc(&good).unwrap().nick, rfc);

    // ...but IrcConfig is public, so the adapter cannot assume it ran: a nick
    // that never went through the loader is refused before any line is framed.
    let mut c = cfg(false, true);
    c.nick = "mu\r\nJOIN #evil".into();
    assert_eq!(
        Registration::start(&c, clock()).unwrap_err(),
        AdapterError::InvalidNick(NickFault::BadCharacter)
    );
    // An ordinary nick is unaffected.
    assert!(Registration::start(&cfg(false, true), clock()).is_ok());
}

/// Quote a value as a TOML basic string, escaping what TOML requires.
fn toml_string(v: &str) -> String {
    let mut out = String::from("\"");
    for c in v.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            '\0' => out.push_str("\\u0000"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn assert_no_secret(s: &str) {
    assert!(!s.contains(SECRET), "leaked the plaintext secret: {s}");
}

// ─────────────────────────── Account metadata ───────────────────────────────

#[test]
fn account_tag_is_read_only_when_negotiated() {
    let msg = IrcMessage::parse("@account=servicesname :n!u@h PRIVMSG #c :hi");

    // Not negotiated: the tag is ignored, however present.
    let (reg, _) = Registration::start(&cfg(false, true), clock()).unwrap();
    assert_eq!(reg.message_account(&msg), None);

    // Negotiated: the account surfaces, verbatim (unfolded).
    let (mut reg, _) = Registration::start(&cfg(false, true), clock()).unwrap();
    feed(&mut reg, "CAP * LS :account-tag");
    feed(&mut reg, "CAP * ACK :account-tag");
    assert!(reg.negotiated().account_tag);
    assert_eq!(reg.message_account(&msg), Some("servicesname"));
    // The logged-out placeholder `*` is not an account.
    let out = IrcMessage::parse("@account=* :n PRIVMSG #c :hi");
    assert_eq!(reg.message_account(&out), None);
}

// ──────────────────────── Capability withdrawal ─────────────────────────────

/// Drive a no-SASL registration to readiness with all three optional
/// capabilities negotiated.
fn ready_with_optional_caps() -> Registration<FixedClock> {
    let (mut reg, _) = Registration::start(&cfg(false, true), clock()).unwrap();
    feed(
        &mut reg,
        "CAP * LS :message-tags account-tag account-notify",
    );
    feed(
        &mut reg,
        "CAP * ACK :message-tags account-tag account-notify",
    );
    feed(&mut reg, ":srv 001 mu-gw :Welcome");
    assert!(reg.is_ready());
    reg
}

#[test]
fn cap_del_after_readiness_clears_the_withdrawn_capabilities() {
    let mut reg = ready_with_optional_caps();
    let tagged = IrcMessage::parse("@account=servicesname :n!u@h PRIVMSG #c :hi");
    assert_eq!(reg.message_account(&tagged), Some("servicesname"));

    // `CAP LS 302` implies cap-notify, so this arrives long after 001 and must
    // still be honoured.
    let s = feed(&mut reg, ":srv CAP mu-gw DEL :message-tags account-tag");
    assert!(s.out.is_empty(), "a withdrawal is not answered on the wire");
    assert!(s.diagnostic.is_some(), "a withdrawal is diagnosed");
    assert!(reg.is_ready(), "a withdrawal does not end the connection");

    let neg = reg.negotiated();
    assert!(!neg.message_tags);
    assert!(!neg.account_tag);
    assert!(neg.account_notify, "only the named caps are cleared");

    // The account tag is no longer trusted…
    assert_eq!(reg.message_account(&tagged), None);
    // …and the framer's `+mu.id` gate, which reads the same flag, is shut.
    let p = FrameParams {
        target: "#c",
        mesh_id: Some("01HXQ"),
        message_tags: reg.negotiated().message_tags,
    };
    assert_eq!(
        frame_privmsg(&p, "hi").unwrap(),
        vec!["PRIVMSG #c :hi\r\n".to_string()]
    );
}

#[test]
fn cap_del_during_negotiation_is_not_read_as_a_capability_list() {
    let (mut reg, _) = Registration::start(&cfg(false, true), clock()).unwrap();
    // A DEL arriving while awaiting the LS reply is a withdrawal, not the list:
    // it must neither be folded into the advertised set nor end the list and
    // trigger the CAP REQ.
    let s = feed(&mut reg, ":srv CAP * DEL :message-tags");
    assert!(s.out.is_empty(), "a DEL is not answered with CAP REQ");

    // The real LS still arrives and drives the request, so the machine was
    // genuinely still waiting for it.
    let s = feed(&mut reg, "CAP * LS :message-tags account-tag");
    assert_eq!(s.out, vec!["CAP REQ :message-tags account-tag".to_string()]);
}

#[test]
fn cap_new_is_recorded_but_never_requested() {
    let mut reg = ready_with_optional_caps();
    let before = reg.negotiated();
    // v0 negotiates once: a late announcement is logged, not requested.
    let s = feed(&mut reg, ":srv CAP mu-gw NEW :draft/example");
    assert!(s.out.is_empty(), "v0 sends no mid-connection CAP REQ");
    assert!(s.diagnostic.is_some());
    assert_eq!(reg.negotiated(), before);
    assert!(reg.is_ready());
}

#[test]
fn withdrawing_sasl_does_not_undo_an_authenticated_connection() {
    let (mut reg, _) = Registration::start(&cfg(true, true), clock()).unwrap();
    feed(&mut reg, "CAP * LS :sasl");
    feed(&mut reg, "CAP * ACK :sasl");
    feed(&mut reg, "AUTHENTICATE +");
    feed(&mut reg, ":srv 903 mu-gw :SASL authentication successful");
    feed(&mut reg, ":srv 001 mu-gw :Welcome");
    assert!(reg.is_ready());

    // Authentication already happened; withdrawing the capability says the
    // server will accept no new exchange, not that this one was undone.
    feed(&mut reg, ":srv CAP mu-gw DEL :sasl");
    assert!(reg.is_ready());
    assert!(format!("{reg:?}").contains("sasl_succeeded: true"));
}

// ───────────────────────────── Live ISUPPORT ────────────────────────────────

#[test]
fn isupport_tracks_casemapping_and_channellen_live() {
    let (mut reg, _) = Registration::start(&cfg(false, true), clock()).unwrap();
    assert_eq!(reg.isupport(), IsupportSettings::default());

    // A 005 before readiness updates settings and emits a change diagnostic.
    let s = feed(
        &mut reg,
        ":srv 005 mu-gw CASEMAPPING=ascii CHANNELLEN=32 :are supported",
    );
    assert!(s.diagnostic.is_some());
    assert_eq!(reg.isupport().casemapping, CaseMapping::Ascii);
    assert_eq!(reg.isupport().channellen, 32);

    // Reach readiness, then a LIVE 005 change after 001 still applies.
    feed(&mut reg, "CAP * LS :");
    feed(&mut reg, ":srv 001 mu-gw :hi");
    assert!(reg.is_ready());
    let s = feed(
        &mut reg,
        ":srv 005 mu-gw CASEMAPPING=rfc1459 :are supported",
    );
    assert!(s.diagnostic.is_some(), "a live change is diagnosed");
    assert_eq!(reg.isupport().casemapping, CaseMapping::Rfc1459);

    // Re-advertising the same value is quiet (no spurious diagnostic).
    let s = feed(
        &mut reg,
        ":srv 005 mu-gw CASEMAPPING=rfc1459 :are supported",
    );
    assert!(s.diagnostic.is_none());
}
