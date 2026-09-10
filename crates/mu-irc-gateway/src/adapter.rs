//! Offline IRC adapter: a single-connection registration/capability state
//! machine, normalized IRC message parsing, and live ISUPPORT tracking.
//!
//! This module is **offline**. It never opens a socket. The real TLS transport,
//! the maintained IRC client crate, and the read/write event loop are the
//! integration increment's job; here the adapter is a pure state machine that
//! *consumes* parsed inbound [`IrcMessage`]s and *produces* the exact outbound
//! protocol lines to send, so the whole registration handshake — CAP
//! negotiation, mandatory SASL PLAIN, optional message-tags/account
//! capabilities, and live `CASEMAPPING`/`CHANNELLEN` — is exercised without a
//! network.
//!
//! The single seam the executor implements is [`Transport`]: one connection,
//! one `send_line`. Time is injected through [`Clock`] so a diagnostic's
//! timestamp is deterministic under test.
//!
//! Secrets are handled as carefully as the rest of the crate. The SASL PLAIN
//! response is built, emitted as outbound lines, and dropped; it is never
//! stored on the [`Registration`], never in an [`AdapterError`], and never in a
//! [`Diagnostic`]. It does pass through the returned [`Step`], which is the one
//! place it has to — so `Step` hand-writes `Debug` and redacts the payload of
//! every `AUTHENTICATE` line it carries. Neither the raw password nor its
//! base64 encoding survives in any printable state, so a full `Debug` dump of
//! the state machine, or of a step, cannot leak a credential.
//!
//! SASL, when configured, is MANDATORY and fails closed. There is no path from
//! configured credentials to [`RegPhase::Ready`] that skips the exchange: a CAP
//! reply that omits `sasl`, a NAK, a terminal SASL numeric in either exchange
//! phase, and a welcome numeric arriving before `903` all end the machine in
//! [`RegPhase::Failed`]. The welcome case is decided in
//! [`Registration::on_message`] *before* the message reaches a phase handler,
//! so it holds in every phase that precedes the welcome rather than only in
//! the one phase that expects a welcome. With no credentials configured the
//! same early welcome is not a fault at all: it means the server ended
//! negotiation on its own, so the machine closes capability negotiation
//! through the usual choke point and registers.
//!
//! Capabilities are not frozen at registration. `CAP LS 302` implicitly
//! enables cap-notify, so the server may announce (`CAP NEW`) or withdraw
//! (`CAP DEL`) capabilities at any point, including hours after `001`. Those
//! are handled in every phase, ahead of the phase dispatch: a withdrawal
//! clears the matching [`Negotiated`] flag, so nothing downstream goes on
//! acting on a capability the connection no longer has. An announcement is
//! recorded as available and deliberately not requested — v0 negotiates once.
//! A withdrawal of `sasl` does not un-authenticate an already authenticated
//! connection.
//!
//! Registration rejection is terminal too. A server that refuses the
//! configured nick (`432`, `433`, `436`, `437`) will never send `001`, so
//! those numerics end the machine in [`RegPhase::Failed`] with
//! [`AdapterError::NickRejected`] instead of leaving it waiting for a welcome
//! that is not coming. v0 does not retry under a replacement nick: the gateway
//! is single-nick by design and the operator picks another one. An automatic
//! nick-retry policy is a v1 question.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;

use crate::config::{validate_nick, IrcConfig, NickFault, SaslCreds};
use crate::mapping::CaseMapping;

/// The one connection this gateway drives. The integration increment implements
/// it over a real TLS socket + IRC client; the offline registration returns the
/// lines a caller would hand to [`Transport::send_line`]. Kept deliberately
/// tiny — a single connection, write-only from the state machine's side (reads
/// arrive as [`IrcMessage`]s fed into [`Registration::on_message`]).
pub trait Transport {
    /// The transport's own failure type.
    type Error;
    /// Send one already-framed protocol line (no trailing CRLF; the transport
    /// adds it). Lines come from the registration/routing state machines, which
    /// budget and sanitize them first.
    fn send_line(&mut self, line: &str) -> Result<(), Self::Error>;
}

/// Injectable wall clock, so a [`Diagnostic`] timestamp is deterministic under
/// test. The integration increment passes a real clock.
pub trait Clock {
    fn now(&self) -> SystemTime;
}

/// A clock reading the real system time.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A fixed clock for tests: every `now()` returns the same instant.
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(pub SystemTime);

impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        self.0
    }
}

// ──────────────────────────── Message parsing ───────────────────────────────

/// A normalized inbound IRC protocol message. The line's `\r\n` is already
/// stripped; tags, prefix, command, and params are split out per RFC 1459 +
/// IRCv3 message-tags.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IrcMessage {
    /// IRCv3 message tags, in order, each `(key, value)`; a valueless tag has
    /// `None`. Empty when the line carried no `@`-prefixed tag section.
    pub tags: Vec<(String, Option<String>)>,
    /// The `:`-prefixed source, without the leading colon, if present.
    pub prefix: Option<String>,
    /// The command or three-digit numeric, upper-cased for commands so callers
    /// match `"CAP"` regardless of how the server spelled it.
    pub command: String,
    /// Parameters, with the trailing `:`-parameter as the final element and its
    /// leading colon removed.
    pub params: Vec<String>,
}

impl IrcMessage {
    /// Parse one protocol line (CRLF already stripped). Total: a malformed line
    /// still yields a message with whatever could be split out, because a real
    /// server sends odd lines and the adapter must not panic on them.
    pub fn parse(line: &str) -> Self {
        let mut rest = line;
        let mut tags = Vec::new();
        if let Some(after) = rest.strip_prefix('@') {
            let (tagpart, r) = split_once_space(after);
            tags = parse_tags(tagpart);
            rest = r;
        }
        // RFC 1459 lets a server put a *run* of spaces between components, and
        // real servers do. `split_once_space` consumes exactly one, so without
        // this the next component would start at a space: the prefix would not
        // be recognized, and the command would come out empty with the real
        // command shifted into the parameters (`:srv  001 mu-gw :Welcome` would
        // never register). Only the separators *between* components are eaten —
        // the trailing `:`-parameter is taken verbatim below, so double spaces
        // inside a message body survive untouched.
        rest = rest.trim_start_matches(' ');
        let mut prefix = None;
        if let Some(after) = rest.strip_prefix(':') {
            let (src, r) = split_once_space(after);
            prefix = Some(src.to_string());
            rest = r.trim_start_matches(' ');
        }
        // Command then params; a `:`-parameter is the rest of the line verbatim.
        let mut params = Vec::new();
        let (command, mut r) = split_once_space(rest);
        loop {
            r = r.trim_start_matches(' ');
            if r.is_empty() {
                break;
            }
            if let Some(trailing) = r.strip_prefix(':') {
                params.push(trailing.to_string());
                break;
            }
            let (p, next) = split_once_space(r);
            params.push(p.to_string());
            r = next;
        }
        IrcMessage {
            tags,
            prefix,
            command: command.to_ascii_uppercase(),
            params,
        }
    }

    /// The value of tag `key`, if the line carried it with a value.
    pub fn tag(&self, key: &str) -> Option<&str> {
        self.tags
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.as_deref())
    }
}

/// Split `s` at the first space into `(before, after)`; `after` is `""` when
/// there is no space. Never allocates.
fn split_once_space(s: &str) -> (&str, &str) {
    match s.find(' ') {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, ""),
    }
}

/// Parse a `key=value;key2;key3=v` tag section into ordered pairs, unescaping
/// tag values per the IRCv3 grammar.
fn parse_tags(section: &str) -> Vec<(String, Option<String>)> {
    section
        .split(';')
        .filter(|t| !t.is_empty())
        .map(|t| match t.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(unescape_tag_value(v))),
            None => (t.to_string(), None),
        })
        .collect()
}

/// Reverse IRCv3 tag-value escaping (`\:`→`;`, `\s`→space, `\\`→`\`, `\r`→CR,
/// `\n`→LF); a stray trailing backslash is dropped, matching the spec.
fn unescape_tag_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some(':') => out.push(';'),
            Some('s') => out.push(' '),
            Some('\\') => out.push('\\'),
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

// ─────────────────────────────── ISUPPORT ───────────────────────────────────

/// The subset of `RPL_ISUPPORT` (005) the gateway acts on. Everything else is
/// ignored. Defaults are the RFC 1459 fallbacks a server that advertises
/// nothing implies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsupportSettings {
    /// How the server folds nick/channel case. Governs every identity and
    /// channel comparison downstream.
    pub casemapping: CaseMapping,
    /// Maximum channel-name length. Channel derivation stays within it.
    pub channellen: usize,
}

/// The default `CHANNELLEN` when a server advertises none: the traditional
/// RFC 1459 limit of 200.
pub const DEFAULT_CHANNELLEN: usize = 200;

impl Default for IsupportSettings {
    fn default() -> Self {
        IsupportSettings {
            casemapping: CaseMapping::default(),
            channellen: DEFAULT_CHANNELLEN,
        }
    }
}

impl IsupportSettings {
    /// Apply one 005 line's tokens (`CASEMAPPING=ascii`, `CHANNELLEN=50`,
    /// `-CHANNELLEN` to reset, …). Unknown tokens and the trailing human-readable
    /// `:are supported by this server` parameter are ignored. Returns whether a
    /// value the gateway compares on actually changed — an *incompatible*
    /// CASEMAPPING change invalidates folded membership, so callers watch it.
    pub fn apply_tokens<'a>(&mut self, tokens: impl IntoIterator<Item = &'a str>) -> bool {
        let mut changed = false;
        for tok in tokens {
            // The trailing `:are supported …` param has spaces; skip it.
            if tok.contains(' ') {
                continue;
            }
            let (neg, key, value) = match tok.strip_prefix('-') {
                Some(rest) => (true, rest, None),
                None => match tok.split_once('=') {
                    Some((k, v)) => (false, k, Some(v)),
                    None => (false, tok, None),
                },
            };
            match key.to_ascii_uppercase().as_str() {
                "CASEMAPPING" => {
                    let cm = if neg {
                        CaseMapping::default()
                    } else {
                        parse_casemapping(value.unwrap_or(""))
                    };
                    if cm != self.casemapping {
                        self.casemapping = cm;
                        changed = true;
                    }
                }
                "CHANNELLEN" => {
                    let len = if neg {
                        DEFAULT_CHANNELLEN
                    } else {
                        value
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(DEFAULT_CHANNELLEN)
                    };
                    if len != self.channellen {
                        self.channellen = len;
                        changed = true;
                    }
                }
                _ => {}
            }
        }
        changed
    }
}

/// Map an ISUPPORT `CASEMAPPING` token to the folding rule. Unknown values fall
/// back to the RFC 1459 default rather than guessing a stricter rule.
fn parse_casemapping(value: &str) -> CaseMapping {
    match value.to_ascii_lowercase().as_str() {
        "ascii" => CaseMapping::Ascii,
        "strict-rfc1459" => CaseMapping::StrictRfc1459,
        _ => CaseMapping::Rfc1459,
    }
}

// ─────────────────────────── Negotiated capabilities ────────────────────────

/// Which optional IRCv3 capabilities the server actually granted. SASL is not
/// here: it is *mandatory when configured* and its success gates readiness, so a
/// missing SASL is an error, not an optional-off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Negotiated {
    /// `message-tags`: the `+mu.id` client tag is emitted only when this is on.
    pub message_tags: bool,
    /// `account-tag`: messages may carry an `account` tag naming the sender's
    /// services account.
    pub account_tag: bool,
    /// `account-notify`: the server sends `ACCOUNT` messages on login/logout.
    pub account_notify: bool,
}

/// The optional capabilities the gateway *requests* (SASL is added on top only
/// when configured). Order is stable so a `CAP REQ` line is deterministic.
const OPTIONAL_CAPS: [&str; 3] = ["message-tags", "account-tag", "account-notify"];

// ─────────────────────────────── Diagnostics ────────────────────────────────

/// A connection-scoped, timestamped, **body-free** diagnostic. It names a
/// connection event or failure class; it never carries a credential, a SASL
/// payload, or a message body, so it is safe to log verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Seconds since the Unix epoch, from the injected [`Clock`].
    pub at_unix_secs: u64,
    /// The event, already sanitized.
    pub message: String,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[t={}] {}", self.at_unix_secs, self.message)
    }
}

// ─────────────────────────────── Errors ─────────────────────────────────────

/// Why registration could not complete. Every variant is body-free and
/// secret-free: it names a protocol/capability fault, never a credential.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdapterError {
    /// SASL PLAIN is configured but the connection is not TLS. Sending PLAIN
    /// credentials over cleartext would expose them, so registration refuses.
    #[error("SASL PLAIN is configured but the connection is not TLS; refusing to send credentials in cleartext")]
    SaslWithoutTls,
    /// SASL is configured but the server did not offer the `sasl` capability.
    #[error("SASL is configured but the server does not advertise the `sasl` capability")]
    SaslUnsupported,
    /// The server NAKed the `sasl` capability the gateway requested.
    #[error("the server refused the `sasl` capability")]
    SaslRejected,
    /// The server rejected the SASL exchange (a terminal SASL numeric — see
    /// [`terminal_sasl_failure`]). No detail is carried: the numeric class is
    /// all a body-free diagnostic may say.
    #[error("SASL authentication failed")]
    SaslFailed,
    /// SASL is configured, and configured means mandatory — but the handshake
    /// arrived at a point where it would complete WITHOUT a successful
    /// exchange: a `CAP ACK`/`NAK` that never acknowledged `sasl`, or a welcome
    /// numeric before `903`. Registration fails rather than proceeding
    /// unauthenticated.
    #[error("SASL is configured but the handshake would complete without authenticating")]
    SaslNotCompleted,
    /// The configured nick is not a usable IRC nickname. [`IrcConfig`] is a
    /// public struct, so a caller can hand the adapter a nick the config loader
    /// never vetted; the check is repeated here before a single line is framed.
    #[error("the configured nick cannot be registered: {0}")]
    InvalidNick(NickFault),
    /// The server refused the nick offered at registration: `432`
    /// ERR_ERRONEUSNICKNAME, `433` ERR_NICKNAMEINUSE, `436` ERR_NICKCOLLISION
    /// or `437` ERR_UNAVAILRESOURCE. The welcome numeric will never arrive, so
    /// continuing to wait for it is a hang; the machine fails instead.
    ///
    /// The nick is not a secret — it is already on the wire in `NICK`/`USER` —
    /// so this message names it along with the numeric, and that text is the
    /// body-free diagnostic for the failure. v0 does not choose a replacement
    /// nick: the gateway is single-nick and the operator configures another
    /// one. An automatic retry policy is a v1 question.
    #[error("the server rejected the nick `{nick}` at registration (numeric {numeric}); registration cannot continue until a different nick is configured")]
    NickRejected {
        /// The rejection numeric, as received.
        numeric: String,
        /// The nick the gateway offered.
        nick: String,
    },
    /// A registration message arrived out of the order the handshake allows.
    #[error("unexpected {0} during registration")]
    Unexpected(String),
}

// ─────────────────────────── Registration machine ───────────────────────────

/// Where the handshake is. Readiness is [`RegPhase::Ready`]; anything past a
/// terminal error is [`RegPhase::Failed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegPhase {
    /// `CAP LS` sent, awaiting the capability list.
    CapList,
    /// `CAP REQ` sent, awaiting ACK/NAK.
    CapAck,
    /// `AUTHENTICATE PLAIN` sent, awaiting the `+` challenge.
    SaslChallenge,
    /// SASL response sent, awaiting the success/failure numeric.
    SaslResult,
    /// `CAP END` sent, awaiting `001`.
    Welcome,
    /// Registered. ISUPPORT still tracked live.
    Ready,
    /// Terminal failure.
    Failed,
}

/// What one fed-in message produced: the lines to send next, any diagnostic,
/// and whether readiness was just reached.
///
/// `Debug` is hand-written, not derived: `out` is the one structure in this
/// module that legitimately carries the base64 SASL response, and a caller
/// logging a step (or the `Result` wrapping it) would otherwise print a
/// reversibly-encoded password. See [`redact_outbound`].
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Step {
    /// Protocol lines to send, in order (no CRLF; the transport adds it).
    pub out: Vec<String>,
    /// A body-free diagnostic worth logging, if this step produced one.
    pub diagnostic: Option<Diagnostic>,
    /// True on the single step that transitions the connection to ready.
    pub became_ready: bool,
}

impl fmt::Debug for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let out: Vec<String> = self.out.iter().map(|l| redact_outbound(l)).collect();
        f.debug_struct("Step")
            .field("out", &out)
            .field("diagnostic", &self.diagnostic)
            .field("became_ready", &self.became_ready)
            .finish()
    }
}

/// Render one outbound line for `Debug`, replacing the payload of an
/// `AUTHENTICATE` line with `<redacted>`.
///
/// Only three `AUTHENTICATE` payloads are protocol constants rather than
/// credential material — the mechanism name the gateway selects, the `+` that
/// terminates a chunked response, and the `*` abort — so those pass through and
/// everything else is redacted. The allowlist is deliberately the small side of
/// the decision: an unrecognized payload is treated as a secret.
fn redact_outbound(line: &str) -> String {
    match line.split_once(' ') {
        Some(("AUTHENTICATE", payload)) if !matches!(payload, "PLAIN" | "+" | "*") => {
            "AUTHENTICATE <redacted>".to_string()
        }
        _ => line.to_string(),
    }
}

/// The connection the gateway asks the executor to open. `tls` mirrors config;
/// the integration increment refuses a cleartext socket the same way the SASL
/// gate does, but the request is carried explicitly so the policy is visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectRequest {
    pub server: String,
    pub tls: bool,
}

/// The offline registration/capability state machine for one connection.
///
/// Drive it: [`Registration::start`] returns the machine and the first lines to
/// send; feed each inbound line's [`IrcMessage`] to [`Registration::on_message`]
/// until [`Registration::is_ready`]. It holds no secret in printable state — the
/// SASL response is built inside [`Registration::on_message`] and dropped.
pub struct Registration<C: Clock> {
    nick: String,
    tls: bool,
    server: String,
    /// The SASL credentials, if configured. `SaslCreds` redacts its password in
    /// `Debug`; this struct's own `Debug` is hand-written to also omit the user.
    sasl: Option<SaslCreds>,
    phase: RegPhase,
    /// Capabilities the server advertised in `CAP LS` (accumulated across a
    /// multi-line `*` list).
    advertised: Vec<String>,
    negotiated: Negotiated,
    /// `sasl` was ACKed and the exchange must complete before readiness.
    sasl_acked: bool,
    /// The exchange actually completed (`903`). Readiness is gated on this
    /// whenever credentials are configured, so no reordering of server messages
    /// can reach [`RegPhase::Ready`] unauthenticated.
    sasl_succeeded: bool,
    isupport: IsupportSettings,
    clock: C,
}

impl<C: Clock> fmt::Debug for Registration<C> {
    /// Never prints the SASL user or password — not even the presence of a
    /// specific credential value. A full `{:?}` of the machine is safe to log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registration")
            .field("nick", &self.nick)
            .field("server", &self.server)
            .field("tls", &self.tls)
            .field("sasl_configured", &self.sasl.is_some())
            .field("sasl_succeeded", &self.sasl_succeeded)
            .field("phase", &self.phase)
            .field("negotiated", &self.negotiated)
            .field("isupport", &self.isupport)
            .finish_non_exhaustive()
    }
}

impl<C: Clock> Registration<C> {
    /// Begin registration for `config`, returning the machine and the lines to
    /// send on connect (`CAP LS`, `NICK`, `USER`). Fails immediately if SASL is
    /// configured without TLS — the one policy that must hold before a single
    /// credential byte is ever framed — or if the configured nick could not be
    /// interpolated safely.
    pub fn start(config: &IrcConfig, clock: C) -> Result<(Self, Vec<String>), AdapterError> {
        if config.sasl.is_some() && !config.tls {
            return Err(AdapterError::SaslWithoutTls);
        }
        // The nick is interpolated into NICK and USER below. `IrcConfig` is
        // public, so the loader's check cannot be assumed: re-run the shared
        // rule here, before a single line is framed.
        validate_nick(&config.nick).map_err(AdapterError::InvalidNick)?;
        let reg = Registration {
            nick: config.nick.clone(),
            tls: config.tls,
            server: config.server.clone(),
            sasl: config.sasl.clone(),
            phase: RegPhase::CapList,
            advertised: Vec::new(),
            negotiated: Negotiated::default(),
            sasl_acked: false,
            sasl_succeeded: false,
            isupport: IsupportSettings::default(),
            clock,
        };
        let lines = vec![
            "CAP LS 302".to_string(),
            format!("NICK {}", reg.nick),
            format!("USER {} 0 * :{}", reg.nick, reg.nick),
        ];
        Ok((reg, lines))
    }

    /// The connection the executor should open for this registration.
    pub fn connect_request(&self) -> ConnectRequest {
        ConnectRequest {
            server: self.server.clone(),
            tls: self.tls,
        }
    }

    /// The capabilities granted so far.
    pub fn negotiated(&self) -> Negotiated {
        self.negotiated
    }

    /// The current live ISUPPORT settings.
    pub fn isupport(&self) -> IsupportSettings {
        self.isupport
    }

    /// Whether registration has completed.
    pub fn is_ready(&self) -> bool {
        self.phase == RegPhase::Ready
    }

    /// The sender account named on `msg`, but only when `account-tag` was
    /// negotiated — an untrusted tag on an un-negotiated connection is ignored.
    pub fn message_account<'a>(&self, msg: &'a IrcMessage) -> Option<&'a str> {
        if !self.negotiated.account_tag {
            return None;
        }
        msg.tag("account").filter(|a| !a.is_empty() && *a != "*")
    }

    /// Advance the handshake with one inbound message. Before readiness this
    /// walks CAP → SASL → welcome; after readiness it tracks the two things a
    /// server may still change under a live connection — ISUPPORT, and the
    /// capability set via `CAP NEW`/`CAP DEL`. A protocol/credential fault
    /// returns a body-free [`AdapterError`] and moves the machine to a terminal
    /// failed phase.
    pub fn on_message(&mut self, msg: &IrcMessage) -> Result<Step, AdapterError> {
        // ISUPPORT is tracked in every phase it can appear, including after
        // readiness — the server may change CASEMAPPING/CHANNELLEN live.
        if msg.command == "005" {
            return Ok(self.on_isupport(msg));
        }
        // Two faults can arrive in ANY registration phase, and a per-phase
        // handler that only knows its own step drops both silently: a nick the
        // server refuses, and a welcome that outruns the handshake. Deciding
        // them here, ahead of the dispatch, is what makes them hold in every
        // phase instead of only in the phase that happens to look for them.
        if self.is_registering() {
            if nick_rejected(&msg.command) {
                return self.on_nick_rejected(msg);
            }
            // `on_welcome` owns the in-phase case; this is the out-of-order one.
            if msg.command == "001" && self.phase != RegPhase::Welcome {
                return self.on_early_welcome(msg);
            }
        }
        // A capability *change* is not a handshake step. `start` sends
        // `CAP LS 302`, which implicitly enables cap-notify, so the gateway has
        // asked the server to announce additions and withdrawals — and those
        // arrive in any phase, most often long after readiness, where the phase
        // dispatch below has no handler at all. Deciding them here is what
        // makes a withdrawal hold for the life of the connection rather than
        // only during negotiation, and it keeps `on_cap_list`/`on_cap_ack` free
        // to read their own subcommands as the replies they expect.
        if msg.command == "CAP" {
            match cap_subcommand(msg) {
                "DEL" => return Ok(self.on_cap_del(msg)),
                "NEW" => return Ok(self.on_cap_new(msg)),
                _ => {}
            }
        }
        match self.phase {
            RegPhase::CapList => self.on_cap_list(msg),
            RegPhase::CapAck => self.on_cap_ack(msg),
            RegPhase::SaslChallenge => self.on_sasl_challenge(msg),
            RegPhase::SaslResult => self.on_sasl_result(msg),
            RegPhase::Welcome => self.on_welcome(msg),
            RegPhase::Ready | RegPhase::Failed => Ok(Step::default()),
        }
    }

    /// Whether the handshake is still running: every phase before readiness and
    /// before a terminal failure.
    fn is_registering(&self) -> bool {
        matches!(
            self.phase,
            RegPhase::CapList
                | RegPhase::CapAck
                | RegPhase::SaslChallenge
                | RegPhase::SaslResult
                | RegPhase::Welcome
        )
    }

    /// A `001` that arrived before the handshake reached [`RegPhase::Welcome`].
    ///
    /// With SASL configured this is precisely the ordering violation the
    /// mandatory-SASL invariant exists for: the server is registering the
    /// connection with the exchange unfinished, so the machine fails closed
    /// rather than waiting for a `903` the server has already skipped.
    ///
    /// Without SASL there is nothing outstanding — the server simply ended
    /// negotiation early — so the machine ends capability negotiation through
    /// `finish_cap`, the one choke point for that, and lets `on_welcome` do the
    /// registering. The `CAP END` still goes out: a server that jumped ahead is
    /// no reason to leave it waiting for a line the protocol says we owe it.
    fn on_early_welcome(&mut self, msg: &IrcMessage) -> Result<Step, AdapterError> {
        if self.sasl.is_some() && !self.sasl_succeeded {
            self.phase = RegPhase::Failed;
            return Err(AdapterError::SaslNotCompleted);
        }
        let mut step = self.finish_cap()?;
        let welcome = self.on_welcome(msg)?;
        step.out.extend(welcome.out);
        step.diagnostic = welcome.diagnostic;
        step.became_ready = welcome.became_ready;
        Ok(step)
    }

    /// The server refused the configured nick. Registration is over: the
    /// welcome numeric is not coming, and v0 has no replacement-nick policy to
    /// fall back on, so the executor gets a terminal, body-free error naming
    /// the numeric and the nick instead of a connection that hangs.
    fn on_nick_rejected(&mut self, msg: &IrcMessage) -> Result<Step, AdapterError> {
        self.phase = RegPhase::Failed;
        Err(AdapterError::NickRejected {
            numeric: msg.command.clone(),
            nick: self.nick.clone(),
        })
    }

    /// Track a live ISUPPORT line, emitting a diagnostic only when a value the
    /// gateway compares on actually changed (so ordinary re-advertisement is
    /// quiet).
    fn on_isupport(&mut self, msg: &IrcMessage) -> Step {
        // 005 params: `<nick> TOKEN=v TOKEN2=v :are supported…`. Skip the first
        // (our nick) and let `apply_tokens` skip the trailing human param.
        let changed = self
            .isupport
            .apply_tokens(msg.params.iter().skip(1).map(String::as_str));
        let mut step = Step::default();
        if changed {
            step.diagnostic = Some(self.diag(format!(
                "ISUPPORT updated: casemapping={:?} channellen={}",
                self.isupport.casemapping, self.isupport.channellen
            )));
        }
        step
    }

    /// A capability the server has withdrawn (`CAP <nick> DEL :cap …`).
    ///
    /// Honouring this is not optional politeness: `CAP LS 302` implicitly
    /// enables cap-notify, so the gateway asked for these notifications and
    /// must not go on reporting a capability the server has taken away. Each
    /// withdrawn optional capability is cleared from [`Negotiated`], which is
    /// what stops `negotiated()` from lying, stops [`Registration::message_account`]
    /// from trusting an `account` tag on a connection that no longer carries
    /// one, and closes the `+mu.id` gate in the framer.
    ///
    /// The names also leave `advertised`, so the server's offer and what the
    /// gateway believes it negotiated cannot drift apart.
    ///
    /// `sasl` is deliberately not undone. Authentication is an event that
    /// already happened; withdrawing the capability afterwards says the server
    /// will accept no *new* exchange, not that this connection stopped being
    /// authenticated. v0 never re-authenticates on a live connection, so
    /// `sasl_acked`/`sasl_succeeded` stand. (A withdrawal that lands mid
    /// negotiation removes `sasl` from `advertised` and so fails the mandatory
    /// SASL check at the end of the list — fail-closed, as everywhere else.)
    fn on_cap_del(&mut self, msg: &IrcMessage) -> Step {
        let withdrawn = cap_list(msg);
        if withdrawn.is_empty() {
            return Step::default();
        }
        for c in &withdrawn {
            match c.as_str() {
                "message-tags" => self.negotiated.message_tags = false,
                "account-tag" => self.negotiated.account_tag = false,
                "account-notify" => self.negotiated.account_notify = false,
                _ => {}
            }
        }
        self.advertised.retain(|a| !withdrawn.contains(a));
        Step {
            diagnostic: Some(self.diag(format!(
                "server withdrew capabilities: {}",
                withdrawn.join(" ")
            ))),
            ..Step::default()
        }
    }

    /// A capability the server has newly announced (`CAP <nick> NEW :cap …`).
    ///
    /// v0 chooses its capability set exactly once, during registration, and
    /// never issues a mid-connection `CAP REQ`: a capability switching on at an
    /// arbitrary later point is a change nothing downstream is built to absorb,
    /// and `sasl` in particular would mean re-authenticating a connection that
    /// is already registered. So the announcement is recorded as available —
    /// `advertised` stays an honest picture of the server's offer — and
    /// explicitly not requested. Requesting a late-announced capability is a v1
    /// question.
    fn on_cap_new(&mut self, msg: &IrcMessage) -> Step {
        let announced = cap_list(msg);
        if announced.is_empty() {
            return Step::default();
        }
        for c in &announced {
            if !self.advertised.iter().any(|a| a == c) {
                self.advertised.push(c.clone());
            }
        }
        Step {
            diagnostic: Some(self.diag(format!(
                "server announced capabilities, not requested in v0: {}",
                announced.join(" ")
            ))),
            ..Step::default()
        }
    }

    fn on_cap_list(&mut self, msg: &IrcMessage) -> Result<Step, AdapterError> {
        if msg.command != "CAP" {
            // A server may send NOTICE/PING before the CAP list; ignore quietly.
            return Ok(Step::default());
        }
        // Only an `LS` reply is a capability list. Reading the subcommand is
        // what keeps some other CAP message that happens to arrive in this
        // phase from being folded into `advertised` as though the server had
        // offered those capabilities.
        if cap_subcommand(msg) != "LS" {
            return Ok(Step::default());
        }
        // `CAP * LS [*] :cap1 cap2 …`. The `*` third param means more lines
        // follow; the capability list is always the trailing param.
        let more = msg.params.get(2).map(String::as_str) == Some("*");
        if let Some(list) = msg.params.last() {
            self.advertised
                .extend(list.split_whitespace().map(|c| cap_name(c).to_string()));
        }
        if more {
            return Ok(Step::default());
        }
        // Full list in hand. SASL is mandatory when configured.
        if self.sasl.is_some() && !self.advertised.iter().any(|c| c == "sasl") {
            self.phase = RegPhase::Failed;
            return Err(AdapterError::SaslUnsupported);
        }
        let mut req: Vec<&str> = OPTIONAL_CAPS
            .iter()
            .copied()
            .filter(|c| self.advertised.iter().any(|a| a == c))
            .collect();
        if self.sasl.is_some() {
            req.push("sasl");
        }
        self.phase = RegPhase::CapAck;
        if req.is_empty() {
            // Nothing to request: end negotiation straight away.
            return self.finish_cap();
        }
        Ok(Step {
            out: vec![format!("CAP REQ :{}", req.join(" "))],
            ..Step::default()
        })
    }

    fn on_cap_ack(&mut self, msg: &IrcMessage) -> Result<Step, AdapterError> {
        if msg.command != "CAP" {
            return Ok(Step::default());
        }
        let sub = cap_subcommand(msg);
        let list = cap_list(msg);
        match sub {
            "ACK" => {
                let mut unsolicited_sasl = false;
                for c in &list {
                    match c.as_str() {
                        "message-tags" => self.negotiated.message_tags = true,
                        "account-tag" => self.negotiated.account_tag = true,
                        "account-notify" => self.negotiated.account_notify = true,
                        // An ACK for `sasl` counts ONLY when the gateway asked
                        // for it. A server that acknowledges a capability the
                        // client never requested is misbehaving; honouring it
                        // would drive the machine into a credential-dependent
                        // phase with no credentials to use.
                        "sasl" if self.sasl.is_some() => self.sasl_acked = true,
                        "sasl" => unsolicited_sasl = true,
                        _ => {}
                    }
                }
                if self.sasl_acked {
                    // Begin the SASL exchange; readiness waits on its success.
                    self.phase = RegPhase::SaslChallenge;
                    return Ok(Step {
                        out: vec!["AUTHENTICATE PLAIN".to_string()],
                        ..Step::default()
                    });
                }
                let mut step = self.finish_cap()?;
                if unsolicited_sasl {
                    step.diagnostic =
                        Some(self.diag("ignored an unsolicited `sasl` capability ACK".into()));
                }
                Ok(step)
            }
            "NAK" => {
                // A NAKed `sasl` is fatal (it was mandatory). Optional caps that
                // are refused simply stay off. A NAK list that does not mention
                // `sasl` at all still cannot end negotiation with mandatory SASL
                // outstanding — `finish_cap` is the choke point for that.
                if self.sasl.is_some() && list.iter().any(|c| c == "sasl") {
                    self.phase = RegPhase::Failed;
                    return Err(AdapterError::SaslRejected);
                }
                self.finish_cap()
            }
            _ => Ok(Step::default()),
        }
    }

    /// End capability negotiation: send `CAP END` and wait for the welcome
    /// numeric.
    ///
    /// This is the single choke point through which every "negotiation is over,
    /// no SASL exchange is running" path passes — an ACK whose list omitted
    /// `sasl`, a NAK that did not name it, and the no-capabilities-at-all case.
    /// It therefore carries the mandatory-SASL invariant: configured
    /// credentials that never got an ACK fail the registration instead of
    /// quietly downgrading it to an unauthenticated one.
    fn finish_cap(&mut self) -> Result<Step, AdapterError> {
        if self.sasl.is_some() && !self.sasl_acked {
            self.phase = RegPhase::Failed;
            return Err(AdapterError::SaslNotCompleted);
        }
        self.phase = RegPhase::Welcome;
        Ok(Step {
            out: vec!["CAP END".to_string()],
            ..Step::default()
        })
    }

    fn on_sasl_challenge(&mut self, msg: &IrcMessage) -> Result<Step, AdapterError> {
        // A server may reject the mechanism outright, before ever sending the
        // `+` challenge. Those numerics are terminal in THIS phase too; without
        // this the machine would sit in SaslChallenge forever, since the
        // numeric handling used to live only in the next phase.
        if terminal_sasl_failure(msg) {
            self.phase = RegPhase::Failed;
            return Err(AdapterError::SaslFailed);
        }
        if msg.command != "AUTHENTICATE" || msg.params.first().map(String::as_str) != Some("+") {
            return Ok(Step::default());
        }
        // Credentials are checked, never assumed: `on_cap_ack` only enters this
        // phase with configured SASL, but a checked error is what belongs on a
        // path a remote server drives — an `expect` here was a reachable panic.
        let Some(creds) = self.sasl.as_ref() else {
            self.phase = RegPhase::Failed;
            return Err(AdapterError::Unexpected("AUTHENTICATE".into()));
        };
        // Build the PLAIN response, emit it, and let it drop at the end of this
        // scope: nothing here is stored on `self`, so neither the password nor
        // its base64 encoding survives past the returned lines.
        let out = authenticate_lines(&sasl_plain(&creds.user, creds.password.expose()));
        self.phase = RegPhase::SaslResult;
        Ok(Step {
            out,
            ..Step::default()
        })
    }

    fn on_sasl_result(&mut self, msg: &IrcMessage) -> Result<Step, AdapterError> {
        // Checked first, and on the whole message: whether a numeric is
        // terminal can depend on its parameters (see `terminal_sasl_failure`).
        if terminal_sasl_failure(msg) {
            self.phase = RegPhase::Failed;
            return Err(AdapterError::SaslFailed);
        }
        match msg.command.as_str() {
            // RPL_LOGGEDIN: informational, keep waiting for 903.
            "900" => Ok(Step::default()),
            // RPL_SASLSUCCESS.
            "903" => {
                self.sasl_succeeded = true;
                self.phase = RegPhase::Welcome;
                Ok(Step {
                    out: vec!["CAP END".to_string()],
                    diagnostic: Some(self.diag("SASL authentication succeeded".into())),
                    ..Step::default()
                })
            }
            _ => Ok(Step::default()),
        }
    }

    fn on_welcome(&mut self, msg: &IrcMessage) -> Result<Step, AdapterError> {
        if msg.command == "001" {
            // The last gate on the mandatory-SASL invariant, independent of how
            // the machine got here: configured credentials that never produced
            // a 903 do not become a ready, unauthenticated connection. Every
            // route into this phase already refuses that case first
            // (`finish_cap` and `on_early_welcome` both fail closed), so this
            // is the belt to their braces — kept precisely because the
            // invariant must not depend on having enumerated the routes.
            if self.sasl.is_some() && !self.sasl_succeeded {
                self.phase = RegPhase::Failed;
                return Err(AdapterError::SaslNotCompleted);
            }
            self.phase = RegPhase::Ready;
            return Ok(Step {
                diagnostic: Some(self.diag(format!("registered as {}", self.nick))),
                became_ready: true,
                ..Step::default()
            });
        }
        Ok(Step::default())
    }

    /// Build a timestamped diagnostic from the injected clock. The message is
    /// caller-sanitized; this only stamps it.
    fn diag(&self, message: String) -> Diagnostic {
        let at = self
            .clock
            .now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();
        Diagnostic {
            at_unix_secs: at,
            message,
        }
    }
}

/// The capability name without its IRCv3 `CAP LS 302` value suffix
/// (`sasl=PLAIN` advertises as `sasl`). The value is not needed here — the
/// gateway only asks whether a capability exists.
fn cap_name(token: &str) -> &str {
    token.split('=').next().unwrap_or(token)
}

/// The subcommand of a `CAP` message (`LS`, `ACK`, `NAK`, `NEW`, `DEL`, …).
/// Every server-sent `CAP` line is `CAP <target> <subcommand> …`, so the
/// subcommand is the second parameter; `""` when the line is too short to carry
/// one, which matches nothing.
fn cap_subcommand(msg: &IrcMessage) -> &str {
    msg.params.get(1).map(String::as_str).unwrap_or("")
}

/// The capability names carried by a `CAP` message, each stripped of its
/// `CAP LS 302` value suffix. The list is always the trailing parameter, and a
/// line short enough that the trailing parameter *is* the subcommand carries no
/// list at all.
fn cap_list(msg: &IrcMessage) -> Vec<String> {
    if msg.params.len() < 3 {
        return Vec::new();
    }
    msg.params
        .last()
        .map(|l| {
            l.split_whitespace()
                .map(cap_name)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Whether this numeric ends the SASL exchange in failure, whichever phase it
/// arrives in.
///
/// `902` ERR_NICKLOCKED, `904` ERR_SASLFAIL, `905` ERR_SASLTOOLONG and `906`
/// ERR_SASLABORTED are terminal outright. `908` RPL_SASLMECHS is not an error
/// numeric at all: it *lists* the mechanisms the server supports. It is
/// terminal only when that list leaves out `PLAIN`, the one mechanism this
/// gateway speaks — a 908 that does advertise PLAIN is informational, and
/// aborting on it would kill an exchange that can still succeed.
fn terminal_sasl_failure(msg: &IrcMessage) -> bool {
    match msg.command.as_str() {
        "902" | "904" | "905" | "906" => true,
        "908" => !advertises_plain(msg),
        _ => false,
    }
}

/// Whether a `908` RPL_SASLMECHS advertises `PLAIN`.
///
/// The documented shape is `<nick> <mechanisms> :are available SASL
/// mechanisms`, which puts the list in the second parameter, but servers also
/// send the shorter `<nick> :<mechanisms>`, which puts it in the trailing one.
/// Both candidates are checked rather than guessing which one a server used;
/// the human-readable trailing text cannot false-positive, since it is not a
/// comma-separated token equal to `PLAIN`. Mechanism names are compared
/// case-insensitively, as the SASL registry is.
fn advertises_plain(msg: &IrcMessage) -> bool {
    [msg.params.get(1), msg.params.last()]
        .into_iter()
        .flatten()
        .any(|list| {
            list.split(',')
                .any(|mech| mech.trim().eq_ignore_ascii_case("PLAIN"))
        })
}

/// The numerics with which a server refuses the nick offered at registration:
/// `432` ERR_ERRONEUSNICKNAME, `433` ERR_NICKNAMEINUSE, `436` ERR_NICKCOLLISION
/// and `437` ERR_UNAVAILRESOURCE. Checked only while the handshake is running,
/// where all four mean the same thing — this nick will not be registered, so
/// the welcome numeric is never coming.
fn nick_rejected(command: &str) -> bool {
    matches!(command, "432" | "433" | "436" | "437")
}

/// The maximum number of base64 characters IRCv3 allows in one `AUTHENTICATE`
/// parameter. A chunk of exactly this length means "more follows".
const SASL_CHUNK: usize = 400;

/// Split an encoded SASL response into IRCv3 `AUTHENTICATE` lines.
///
/// The protocol reads a chunk shorter than [`SASL_CHUNK`] as the last one, so a
/// response whose encoded length is an exact multiple of 400 — including an
/// empty response — must be followed by a lone `AUTHENTICATE +` or the server
/// waits forever for a continuation that never comes. base64 output is ASCII,
/// so splitting on byte indices can never land mid-character.
fn authenticate_lines(encoded: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = encoded;
    while rest.len() >= SASL_CHUNK {
        let (head, tail) = rest.split_at(SASL_CHUNK);
        out.push(format!("AUTHENTICATE {head}"));
        rest = tail;
    }
    if rest.is_empty() {
        out.push("AUTHENTICATE +".to_string());
    } else {
        out.push(format!("AUTHENTICATE {rest}"));
    }
    out
}

/// The SASL PLAIN response: base64 of `authzid \0 authcid \0 password`, with an
/// empty authzid (authenticate as the same identity we authorize as). The raw
/// bytes are assembled, encoded, and returned; the caller emits the encoded
/// string as one line and drops it. Nothing retains the plaintext.
fn sasl_plain(authcid: &str, password: &str) -> String {
    let mut raw = Vec::with_capacity(authcid.len() + password.len() + 2);
    raw.push(0); // empty authzid
    raw.extend_from_slice(authcid.as_bytes());
    raw.push(0);
    raw.extend_from_slice(password.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(raw)
}
