//! Gateway-local `[irc]` configuration and its validation.
//!
//! The mesh side is NOT re-implemented here: [`load`] delegates to
//! `mu_dialogue::mesh::load`, so there is one loader and one set of defaults for
//! the mesh connection. This module owns only the IRC-specific `[irc]` section.
//!
//! Validation is eager where the alternative is a runtime surprise. `tls_ca_file`
//! is READ and PARSED here, not at connect time, so an operator running IRC
//! behind a private CA learns that their bundle is missing, unparsable or empty
//! from `--check-config` rather than from a reconnect loop that never succeeds.
//!
//! Secrets are handled carefully. A SASL password read from config or a file is
//! wrapped in [`Secret`], whose `Debug` redacts it, and every [`ConfigError`]
//! names a field or a path but never a secret value — including the
//! deserialization path, which reports the field name and expected type from
//! `IRC_FIELDS` rather than forwarding a serde message that would quote the
//! offending value. The mesh half carries two secrets: the borrowed
//! [`MeshConfig`] derives `Debug` over its plaintext Ed25519 `issuer_key`, and
//! its `nats_url` may hold userinfo credentials — so [`GatewayConfig`] does NOT
//! derive `Debug`; it prints the mesh section through a wrapper that redacts
//! both.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use mu_dialogue::mesh;
pub use mu_dialogue::mesh::MeshConfig;

use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls;
use tokio_rustls::rustls::crypto::ring::sign::any_supported_type;
use tokio_rustls::rustls::sign::CertifiedKey;

use crate::transport::{CaFault, TlsTrust};

/// A credential value that must never be logged. `Debug` redacts it; the plain
/// value is reachable only through [`Secret::expose`], which callers use when
/// they actually authenticate.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wrap a credential value. Used by config loading and by callers that hold
    /// a password from another source (e.g. constructing an [`IrcConfig`]
    /// directly). The value is redacted in `Debug` from this point on.
    pub fn new(value: impl Into<String>) -> Self {
        Secret(value.into())
    }

    /// The secret bytes, for the one place that authenticates. Kept off `Debug`
    /// and `Display` on purpose.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// A resolved SASL PLAIN credential pair. Present only when the operator
/// configured SASL; absent means no SASL.
#[derive(Clone, Debug)]
pub struct SaslCreds {
    pub user: String,
    /// Redacting wrapper — never printed by `Debug`.
    pub password: Secret,
}

/// The validated `[irc]` configuration. `Debug` is safe to log: the only secret
/// it holds is inside [`Secret`], which redacts.
#[derive(Clone, Debug)]
pub struct IrcConfig {
    /// `host:port` of the IRC server.
    pub server: String,
    /// Whether to connect over TLS. Defaults to `true`.
    pub tls: bool,
    /// What a server certificate is verified against: the system trust store by
    /// default, plus (or instead of) the PEM bundle `tls_ca_file` names. Built
    /// here, at load time, so a bundle that cannot be used is a configuration
    /// error rather than a reconnect that never succeeds.
    pub tls_trust: TlsTrust,
    /// The single nick the gateway registers as.
    pub nick: String,
    /// SASL PLAIN credentials, or `None` when SASL is not configured.
    pub sasl: Option<SaslCreds>,
    /// Channel-name prefix (e.g. `#`). Defaults to `#`.
    pub channel_prefix: String,
    /// The lobby channel for fan-out and fallbacks. Defaults to `#mu`.
    pub lobby: String,
    /// Whether to subscribe the agent-DM observer wildcard. Defaults to `true`.
    pub observe_agent_dms: bool,
    /// `[irc.puppets]` — one IRC nick per live agent. Defaults apply when the
    /// table is absent.
    pub puppets: PuppetsConfig,
}

/// The validated `[irc.puppets]` table: one IRC nick per live agent, operated
/// by the gateway over its own connection (design:
/// `specs/plans/mu-irc-gateway-v1-puppets.md`). Every field has a default, so
/// an absent table is the design's defaults; `enabled = false` is the one
/// switch that turns the whole thing off. A puppet's credential is a client
/// CERTIFICATE — it leases a pre-registered slot account and proves it with
/// that account's certificate (SASL EXTERNAL / certfp) — so a `sasl_*`
/// password key in this table is refused rather than ignored: it describes a
/// credential no puppet will ever present.
///
/// NOTHING HERE IS READ AT RUNTIME YET. The pool is not wired to the bridge
/// (design increment 2b), and [`crate::transport`] still builds the client
/// side `with_no_client_auth()`, so no certificate is presented to anything
/// today. What this table buys now is that the contract is loaded, the
/// credentials are parsed, and both are shown by `--check-config` before the
/// increment that connects with them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PuppetsConfig {
    /// Run puppets at all. Defaults to `true`.
    pub enabled: bool,
    /// Roles whose SESSION-shaped peers get a puppet (`cc:<id>`,
    /// `mu:<daemon>:<session>`) — ruling A. Defaults to `["cc", "mu"]`. `human`
    /// is never accepted: humans keep their own names.
    pub roles: Vec<String>,
    /// Also give bare daemons (`mu:<daemon>`, no session) a puppet. Defaults to
    /// `false` (ruling A: a daemon puppet is a nick nobody can usefully address).
    pub daemons: bool,
    /// Upper bound on concurrently connected puppets. Defaults to 16 — Ergo's
    /// per-IP `max-concurrent-connections`, which the operator raises for the
    /// gateway host before increment 2b goes live.
    pub max: usize,
    /// A peer must have been discovered this long before it gets a puppet, so
    /// a review-panel seat that lives for one ask never costs a connection.
    /// Defaults to 60.
    pub min_age_secs: u64,
    /// Puppet connections started concurrently. Defaults to 2, well under
    /// Ergo's throttle of 32 connections per 10 minutes.
    pub connect_parallelism: usize,
    /// How long a puppet told to QUIT may take to get the QUIT written before
    /// its socket is cut. Defaults to 3, the grace the gateway's own QUIT
    /// gets. A session's puppet teardown is bounded by this plus a second, so
    /// it is 1 to [`QUIT_GRACE_MAX_SECS`]: no grace is no QUIT (every quit
    /// forced), and a shutdown cannot be made to wait indefinitely.
    pub quit_grace_secs: u64,
    /// Depth of one puppet's command queue (JOINs and lines the session hands
    /// it). A puppet with this many commands unread is stalled and loses its
    /// voice until it catches up; nothing else waits on it. Defaults to 32.
    pub command_queue: usize,
    /// Depth of the queue every puppet reports on to the session (lifecycle
    /// events wait for room; protocol lines are dropped and counted when it
    /// is full). Defaults to 256.
    pub event_queue: usize,
    /// How long a JOIN the puppet's outbound queue refused waits before it is
    /// offered again (it is also re-offered whenever the connection is
    /// otherwise active). Defaults to 250.
    pub join_retry_ms: u64,
    /// Account-name prefix for the leased slot pool: slot `n` is
    /// `<slot_prefix>-<n>`, for `n` in `1..=max`. Defaults to `cc`.
    ///
    /// ONE pool, not one per role: `max` is Ergo's per-IP
    /// `max-concurrent-connections`, so two pools of that size would breach
    /// the limit the size came from. A slot is leased by whichever peer
    /// qualifies, and the role stays visible in the puppet's LABEL (its nick
    /// and realname). Recorded in the design under *Provisioning*.
    pub slot_prefix: String,
    /// Directory holding one client certificate per slot account:
    /// `<slot_certs_dir>/<account>.crt` and `.key`. `None` (the default)
    /// means the pool is not provisioned and no puppet authenticates.
    ///
    /// Every pair is loaded and checked when the config is read — parsed as
    /// X.509, the key loaded as a signing key, and the two matched by public
    /// key — so a pool that could not authenticate is refused at load rather
    /// than one failed registration at a time. Requires `[irc] tls = true`:
    /// the credential is presented in the TLS handshake, and a cleartext
    /// connection has none.
    ///
    /// Certificates rather than passwords because Ergo's certfp matches a
    /// FINGERPRINT, not a chain — self-signed per-slot certs are enough, no CA
    /// is involved, and no secret has to live in or beside this file. One cert
    /// per account is not a choice: Ergo refuses a second account on a
    /// fingerprint it already knows, and `authzid` must equal `authcid`, so a
    /// shared certificate cannot assume a different slot. Verified against
    /// Ergo 2.19 and recorded in the design under *Provisioning*.
    pub slot_certs_dir: Option<PathBuf>,
}

/// The most `[irc.puppets] quit_grace_secs` may be: an hour. A shutdown waits
/// up to the grace (plus a second) for the pool's QUITs, and the deadline
/// arithmetic on it must not overflow.
pub const QUIT_GRACE_MAX_SECS: u64 = 3600;

impl Default for PuppetsConfig {
    fn default() -> Self {
        PuppetsConfig {
            enabled: true,
            roles: vec!["cc".to_string(), "mu".to_string()],
            daemons: false,
            max: 16,
            min_age_secs: 60,
            connect_parallelism: 2,
            quit_grace_secs: 3,
            command_queue: 32,
            event_queue: 256,
            join_retry_ms: 250,
            slot_prefix: "cc".to_string(),
            slot_certs_dir: None,
        }
    }
}

impl PuppetsConfig {
    /// The account name of slot `n` (1-based), e.g. `cc-3`.
    pub fn slot_account(&self, n: usize) -> String {
        format!("{}-{n}", self.slot_prefix)
    }

    /// Every slot account in the pool, in order.
    pub fn slot_accounts(&self) -> Vec<String> {
        (1..=self.max).map(|n| self.slot_account(n)).collect()
    }

    /// The certificate and key paths for a slot account, when the pool is
    /// provisioned.
    pub fn slot_cert(&self, account: &str) -> Option<(PathBuf, PathBuf)> {
        let dir = self.slot_certs_dir.as_ref()?;
        Some((
            dir.join(format!("{account}.crt")),
            dir.join(format!("{account}.key")),
        ))
    }
}

/// Both halves of the gateway's configuration: the IRC side (this crate) and
/// the mesh side (loaded verbatim through the shared mesh loader).
///
/// `Debug` is hand-written, not derived, and is safe to log. [`MeshConfig`]
/// belongs to `mu-dialogue` and derives `Debug` over `issuer_key`, a plaintext
/// Ed25519 private key; deriving here would print that key in full alongside a
/// SASL password that [`Secret`] carefully redacts.
#[derive(Clone)]
pub struct GatewayConfig {
    pub irc: IrcConfig,
    pub mesh: MeshConfig,
}

impl fmt::Debug for GatewayConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GatewayConfig")
            .field("irc", &self.irc)
            .field("mesh", &RedactedMesh(&self.mesh))
            .finish()
    }
}

/// Prints a [`MeshConfig`] with every credential it can carry redacted.
///
/// The upstream type is out of this crate's scope to change, so the redaction
/// lives at the point of printing. `issuer_key` is a plaintext private key and
/// is replaced wholesale. `nats_url` is NOT known-non-secret either: a NATS URL
/// may carry authentication material in its userinfo (`nats://user:pass@host`
/// or `nats://token@host`), so it is printed through [`redact_url_userinfo`],
/// which keeps the diagnostic value (scheme, host, port) and drops the
/// credential. The output is marked non-exhaustive: if `mu-dialogue` adds a
/// field, it stays out of the log until someone decides it is safe to show.
struct RedactedMesh<'a>(&'a MeshConfig);

impl fmt::Debug for RedactedMesh<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MeshConfig")
            .field("enabled", &self.0.enabled)
            .field("nats_url", &redact_url_userinfo(&self.0.nats_url))
            .field("issuer_key", &Redacted)
            .finish_non_exhaustive()
    }
}

/// Redact every userinfo in a NATS URL list. `nats_url` may be a
/// comma-separated list, and a URL's userinfo may itself contain `,`, `@`, or
/// `:` (all legal there), so the list is NOT split first: the string is walked
/// left to right and everything between a scheme separator (`://`, or the
/// start of the current fragment when there is none) and the next `@` is
/// replaced. Anything ambiguous is over-redacted — a scheme-less host that
/// precedes a credentialed URL in the same list disappears with it — which is
/// the safe failure for a value whose only job here is to be logged.
fn redact_url_userinfo(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    let mut rest = url;
    while let Some(at) = rest.find('@') {
        let (head, tail) = rest.split_at(at);
        let start = head.rfind("://").map(|i| i + 3).unwrap_or(0);
        out.push_str(&head[..start]);
        out.push_str("<redacted>@");
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}

/// Stands in for a secret value in `Debug` output, matching [`Secret`]'s
/// wording so a redaction reads the same wherever it appears.
struct Redacted;

impl fmt::Debug for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Everything that can go wrong resolving `[irc]`. No variant carries a secret
/// value; each names a field or a path so a diagnostic is safe to log.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read IRC config {0}: {1}")]
    Read(PathBuf, std::io::Error),
    /// The file is not valid TOML. The second field is deliberately NOT the
    /// `toml` crate's own `Display`: that quotes the offending source line, so a
    /// malformed `sasl_password = "…"` would put the credential in the
    /// diagnostic. See `toml_error_summary`.
    #[error("{0} is not valid TOML: {1}")]
    Toml(PathBuf, String),
    #[error("no [irc] section in {0}")]
    MissingSection(PathBuf),
    /// The `[irc]` section did not deserialize. The second field is built by
    /// `section_fault` from the field NAME and the expected TYPE only — the
    /// serde error's own text is discarded, because `invalid type: integer
    /// `8675309`, expected a string` embeds the offending value, and that value
    /// is the operator's unquoted password.
    #[error("[irc] in {0} is malformed: {1}")]
    Malformed(PathBuf, String),
    #[error("[irc] is missing required field `{0}`")]
    MissingField(&'static str),
    #[error("[irc] sets a SASL password but no `sasl_user`")]
    SaslMissingUser,
    #[error("[irc] sets `sasl_user` but no password (`sasl_password` or `sasl_password_file`)")]
    SaslMissingPassword,
    #[error("[irc] sets both `sasl_password` and `sasl_password_file`; use exactly one")]
    SaslPasswordConflict,
    #[error("cannot read `sasl_password_file` {0}: {1}")]
    PasswordFile(PathBuf, std::io::Error),
    /// `tls_ca_file` names something that cannot serve as a trust anchor. The
    /// fault is a fixed phrase or an io error from [`CaFault`]; no part of the
    /// file's contents reaches the diagnostic, for the same reason a malformed
    /// `sasl_password` is reported by field name.
    #[error("[irc] `tls_ca_file` {0} cannot be used: {1}")]
    TlsCaFile(PathBuf, CaFault),
    #[error(
        "[irc] sets `tls_system_roots = false` without a `tls_ca_file`, which leaves nothing \
         to verify any server certificate against"
    )]
    NoTrustAnchors,
    #[error(
        "[irc] sets `tls = false` with `tls_ca_file`/`tls_system_roots`: a cleartext \
         connection presents no certificate, so nothing would be verified against them"
    )]
    TrustWithoutTls,
    /// `nick` is not usable as an IRC nickname. The reason is a fixed phrase,
    /// never the offending nick, so the diagnostic stays value-free like the
    /// rest of this enum.
    #[error("[irc] `nick` is not a valid IRC nickname: {0}")]
    InvalidNick(NickFault),
    /// `[irc.puppets]` carries a SASL PASSWORD key. A puppet's credential is a
    /// client CERTIFICATE (SASL EXTERNAL / certfp), not a password, so a
    /// password here names a mechanism the design does not use. Still refused,
    /// for the reason it always was: so the operator is not left believing a
    /// credential is in use when it is not. Verified on Ergo 2.19 — certfp
    /// matches a fingerprint rather than a chain, and the mapping is one
    /// account per certificate (`authcid` and `authzid` must agree), which is
    /// why slots are provisioned one cert each under `slot_certs_dir`.
    ///
    /// Present tense is about the CONFIG CONTRACT, not about a live
    /// connection: see [`PuppetsConfig`] — nothing presents a certificate
    /// until the wiring increment.
    #[error(
        "[irc.puppets] cannot carry `{0}`: a puppet's credential is a client certificate \
         (SASL EXTERNAL), not a password, so this key would never be presented. Remove \
         the `sasl_*` keys and point `slot_certs_dir` at the per-slot certificates instead"
    )]
    PuppetsSasl(&'static str),
    /// `[irc.puppets] slot_certs_dir` is set but a slot's certificate or key is
    /// missing or unusable. Refused at load: a pool that cannot authenticate
    /// would otherwise fail one registration at a time, at connect, with the
    /// cause a long way from the symptom.
    #[error(
        "[irc.puppets] slot credential {0} {1}. Every slot needs a client certificate \
         and its key, both PEM: generate one per account (self-signed is fine — Ergo \
         matches a fingerprint, not a chain), register it with `NS CERT ADD` while \
         connected as that account, or unset `slot_certs_dir` to leave the pool \
         unprovisioned"
    )]
    PuppetsSlotCert(String, SlotCertFault),
    /// `[irc.puppets] slot_certs_dir` is set while `[irc] tls = false`. The
    /// same class of contradiction as [`ConfigError::TrustWithoutTls`], and
    /// refused for the same reason: the config would otherwise describe a pool
    /// that authenticates, on a connection that cannot.
    #[error(
        "[irc.puppets] `slot_certs_dir` needs `[irc] tls = true`: a slot's credential is \
         a TLS client certificate, and a cleartext connection has no handshake to present \
         it in. Set `tls = true`, or unset `slot_certs_dir` to leave the pool unprovisioned"
    )]
    PuppetsCertsWithoutTls,
    /// Two slots are provisioned with the SAME certificate. Caught at load
    /// because it is a pool that looks complete and is not: one fingerprint
    /// maps to one account, so all but one of those slots fails at connect.
    #[error(
        "[irc.puppets] slots `{0}` and `{1}` are provisioned with the same certificate. \
         A server maps one account per certificate fingerprint, so only one of them \
         could ever log in — give every slot its own certificate (self-signed is fine) \
         and register each with `NS CERT ADD` while connected as that account"
    )]
    PuppetsDuplicateSlotCert(String, String),
    /// `[irc.puppets] quit_grace_secs` is past [`QUIT_GRACE_MAX_SECS`].
    /// Rendered from the constant for the same reason as
    /// [`ConfigError::PuppetsTooManySlots`].
    #[error(
        "[irc.puppets] `quit_grace_secs` is too large (at most {}): a shutdown waits \
         up to this long for the pool's QUITs before it stops waiting",
        QUIT_GRACE_MAX_SECS
    )]
    PuppetsGraceTooLong,
    /// A slot account name that `[irc.puppets] slot_prefix` generates is not a
    /// legal nickname. The account name is also what that puppet registers as,
    /// so the fault is the nick's; the highest slot makes the longest name and
    /// is usually the one that trips it.
    #[error(
        "[irc.puppets] slot_prefix generates `{0}`, which is not a valid nickname: {1}. \
         Every slot from `<slot_prefix>-1` through `<slot_prefix>-<max>` has to be a \
         legal nick — shorten `slot_prefix`, or lower `max`"
    )]
    PuppetsSlotNick(String, NickFault),
    /// `[irc.puppets]` is malformed; the message names fields and types only.
    #[error("[irc.puppets] is malformed: {0}")]
    PuppetsMalformed(String),
    /// A `[irc.puppets]` value is out of range or contradictory.
    #[error("[irc.puppets] `{0}` {1}")]
    PuppetsInvalid(&'static str, &'static str),
    #[error("mesh config: {0}")]
    Mesh(String),
}

/// Why a slot's credential cannot be used. Fixed phrases plus one io error,
/// for the same reason [`CaFault`] uses them: the PEM reader's own wording
/// names its internals ("section is missing its END marker"), which tells an
/// operator nothing about which file to go look at or what to put in it.
#[derive(Debug, thiserror::Error)]
pub enum SlotCertFault {
    #[error("is missing")]
    Missing,
    #[error("is not a file")]
    NotAFile,
    #[error("cannot be read: {0}")]
    Read(std::io::Error),
    #[error("is not usable PEM")]
    NotPem,
    #[error("holds no PEM certificate")]
    NoCert,
    #[error("holds no PEM private key")]
    NoKey,
    #[error("is PEM-wrapped but is not a certificate")]
    NotACertificate,
    #[error("is not a private key this build can sign with")]
    UnusableKey,
    #[error("are not a pair: their public keys differ")]
    Mismatch,
}

/// The longest nickname this gateway will register. RFC 1459 caps a nick at 9
/// characters and modern servers advertise `NICKLEN` well above that, so the
/// limit here is only a sanity bound: it keeps a pathological value from being
/// interpolated into `NICK`/`USER` and blowing the 512-byte line budget before
/// the server ever sees it.
pub const NICK_MAX_LEN: usize = 32;

/// Why a nickname cannot be used. Each variant is a fixed phrase and carries no
/// part of the offending value, so it composes into a value-free diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NickFault {
    #[error("it is empty")]
    Empty,
    #[error("it is longer than {} characters", NICK_MAX_LEN)]
    TooLong,
    #[error("it contains a non-ASCII character, and an RFC 2812 nickname is ASCII")]
    NotAscii,
    #[error("it does not begin with a letter or one of `[ ] \\ ` _ ^ {{ | }}`")]
    BadStart,
    #[error(
        "it contains a character that is not a letter, a digit, `-`, \
         or one of `[ ] \\ ` _ ^ {{ | }}`"
    )]
    BadCharacter,
}

/// The RFC 2812 `special` set — `%x5B-60` and `%x7B-7D`, i.e. the nine
/// characters `[`, `]`, `\`, `` ` ``, `_`, `^`, `{`, `|`, `}`. On IRC these are
/// ordinary nickname characters rather than punctuation: `{}|^` are the
/// case-folded twins of `[]\~` (see [`crate::mapping`]), which is exactly why
/// the same nick may be spelled with either.
fn is_nick_special(b: u8) -> bool {
    matches!(
        b,
        b'[' | b']' | b'\\' | b'`' | b'_' | b'^' | b'{' | b'|' | b'}'
    )
}

/// Check that `nick` is a usable IRC nickname, per the RFC 2812 grammar:
///
/// ```text
/// nickname = ( letter / special ) *( letter / digit / special / "-" )
/// special  = %x5B-60 / %x7B-7D   ; "[", "]", "\", "`", "_", "^", "{", "|", "}"
/// ```
///
/// with the RFC's 9-character bound relaxed to [`NICK_MAX_LEN`], since modern
/// servers advertise `NICKLEN` well above nine.
///
/// This is the ONE rule, shared by config loading and by
/// [`crate::adapter::Registration::start`]: `IrcConfig` is a public struct that
/// a caller can build without going through [`load_irc`], so the adapter cannot
/// assume the loader vetted the nick before it is interpolated into `NICK` and
/// `USER`. Taking the grammar whole rather than blacklisting the dangerous
/// bytes is what makes that safe by construction: CR and LF (which would append
/// whole extra protocol lines), NUL, a space or a `,` (which would silently
/// change which parameter the server reads) and a leading `:` (a trailing-
/// parameter marker) are all outside the grammar, and so is everything nobody
/// has thought of yet.
pub fn validate_nick(nick: &str) -> Result<(), NickFault> {
    if nick.is_empty() {
        return Err(NickFault::Empty);
    }
    if nick.len() > NICK_MAX_LEN {
        return Err(NickFault::TooLong);
    }
    if !nick.is_ascii() {
        return Err(NickFault::NotAscii);
    }
    // ASCII from here, so bytes and characters are the same thing.
    let bytes = nick.as_bytes();
    if !(bytes[0].is_ascii_alphabetic() || is_nick_special(bytes[0])) {
        return Err(NickFault::BadStart);
    }
    if !bytes[1..]
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || is_nick_special(b) || b == b'-')
    {
        return Err(NickFault::BadCharacter);
    }
    Ok(())
}

/// The raw `[irc]` section as it appears in TOML, before validation. Every
/// field is optional so that a missing *required* field produces a precise
/// [`ConfigError::MissingField`] rather than a generic deserialization error,
/// and unknown keys are rejected so a typo does not silently disable a setting.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IrcRaw {
    server: Option<String>,
    tls: Option<bool>,
    tls_ca_file: Option<String>,
    tls_system_roots: Option<bool>,
    nick: Option<String>,
    sasl_user: Option<String>,
    sasl_password: Option<String>,
    sasl_password_file: Option<String>,
    channel_prefix: Option<String>,
    lobby: Option<String>,
    observe_agent_dms: Option<bool>,
    /// The nested `[irc.puppets]` table, kept raw here and parsed by
    /// [`parse_puppets`] so its faults are described in its own terms.
    puppets: Option<toml::Value>,
}

/// The raw `[irc.puppets]` table before validation. Every field optional so a
/// default applies per field, unknown keys rejected so a typo cannot silently
/// leave a puppet setting at its default.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PuppetsRaw {
    enabled: Option<bool>,
    roles: Option<Vec<String>>,
    daemons: Option<bool>,
    max: Option<u64>,
    min_age_secs: Option<u64>,
    connect_parallelism: Option<u64>,
    quit_grace_secs: Option<u64>,
    command_queue: Option<u64>,
    event_queue: Option<u64>,
    join_retry_ms: Option<u64>,
    slot_prefix: Option<String>,
    slot_certs_dir: Option<String>,
}

/// The `[irc.puppets]` fields and the TOML type each expects, for the same
/// value-free fault description [`IRC_FIELDS`] gives the parent section.
const PUPPETS_FIELDS: &[(&str, FieldType)] = &[
    ("enabled", FieldType::Bool),
    ("roles", FieldType::StrList),
    ("daemons", FieldType::Bool),
    ("max", FieldType::Int),
    ("min_age_secs", FieldType::Int),
    ("connect_parallelism", FieldType::Int),
    ("quit_grace_secs", FieldType::Int),
    ("command_queue", FieldType::Int),
    ("event_queue", FieldType::Int),
    ("join_retry_ms", FieldType::Int),
    ("slot_prefix", FieldType::Str),
    ("slot_certs_dir", FieldType::Str),
];

/// Resolve the config path: `$MU_CONFIG` if set, else `~/.config/mu/config.toml`
/// (the same convention `mu-dialogue` uses).
pub fn default_config_path() -> PathBuf {
    if let Ok(p) = std::env::var("MU_CONFIG") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".config/mu/config.toml")
}

/// Load and validate the `[irc]` section from `path`.
pub fn load_irc(path: &Path) -> Result<IrcConfig, ConfigError> {
    let text =
        std::fs::read_to_string(path).map_err(|e| ConfigError::Read(path.to_path_buf(), e))?;
    let root: toml::Value = text.parse().map_err(|e: toml::de::Error| {
        ConfigError::Toml(path.to_path_buf(), toml_error_summary(&text, &e))
    })?;
    let section = root
        .get("irc")
        .ok_or_else(|| ConfigError::MissingSection(path.to_path_buf()))?;
    // The deserializer's error text is deliberately DROPPED, not forwarded: a
    // recognized field with a wrong-typed value renders as `invalid type:
    // integer `8675309`, expected a string`, which puts an unquoted password
    // into a diagnostic this module documents as safe to log. The fault is
    // re-derived from the section itself, naming only the field and the type.
    let raw: IrcRaw = section.clone().try_into().map_err(|_: toml::de::Error| {
        ConfigError::Malformed(path.to_path_buf(), section_fault(section))
    })?;
    validate(raw)
}

/// The `[irc]` fields and the TOML type each one expects. This table is what
/// [`section_fault`] reports against, so a malformed section is described
/// without the deserializer ever being asked to phrase the complaint.
const IRC_FIELDS: &[(&str, FieldType)] = &[
    ("server", FieldType::Str),
    ("tls", FieldType::Bool),
    ("tls_ca_file", FieldType::Str),
    ("tls_system_roots", FieldType::Bool),
    ("nick", FieldType::Str),
    ("sasl_user", FieldType::Str),
    ("sasl_password", FieldType::Str),
    ("sasl_password_file", FieldType::Str),
    ("channel_prefix", FieldType::Str),
    ("lobby", FieldType::Str),
    ("observe_agent_dms", FieldType::Bool),
    ("puppets", FieldType::Table),
];

/// The TOML type an `[irc]` field accepts.
#[derive(Clone, Copy)]
enum FieldType {
    Str,
    Bool,
    Int,
    StrList,
    Table,
}

impl FieldType {
    fn name(self) -> &'static str {
        match self {
            Self::Str => "string",
            Self::Bool => "boolean",
            Self::Int => "non-negative integer",
            Self::StrList => "list of strings",
            Self::Table => "table",
        }
    }

    fn matches(self, value: &toml::Value) -> bool {
        match self {
            Self::Str => value.is_str(),
            Self::Bool => value.is_bool(),
            Self::Int => value.as_integer().is_some_and(|i| i >= 0),
            Self::StrList => value
                .as_array()
                .is_some_and(|items| items.iter().all(toml::Value::is_str)),
            Self::Table => value.is_table(),
        }
    }
}

/// Describe why an `[irc]` section is malformed using NOTHING from the section
/// but field names and expected types.
///
/// Values never appear: a wrong-typed `sasl_password` is reported as "field
/// `sasl_password` expects a string", not as the number the operator forgot to
/// quote. An unknown key is named only when the key itself looks like an
/// identifier — a long or exotic key is reported anonymously, since a key that
/// is not a plausible field name is more likely to be misplaced data.
fn section_fault(section: &toml::Value) -> String {
    table_fault(section, "[irc]", IRC_FIELDS)
}

/// [`section_fault`] for any table with a known field list.
fn table_fault(section: &toml::Value, label: &str, fields: &[(&str, FieldType)]) -> String {
    let Some(table) = section.as_table() else {
        return format!("{label} is not a table");
    };
    for (key, value) in table {
        match fields.iter().find(|(name, _)| name == key) {
            None => return format!("unknown field{}", named(key)),
            Some((name, ty)) if !ty.matches(value) => {
                return format!("field `{name}` expects a {}", ty.name())
            }
            Some(_) => {}
        }
    }
    // Every key is recognized and well-typed, so the failure is structural
    // (a duplicate or a nested table serde rejected). Say so without quoting.
    format!("section is not a valid {label} table")
}

/// Render ` \`key\`` when `key` is identifier-shaped, and the empty string
/// otherwise, so an unknown-field diagnostic cannot carry arbitrary text.
fn named(key: &str) -> String {
    let plausible = key.len() <= 40
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if plausible {
        format!(" `{key}`")
    } else {
        String::new()
    }
}

/// Summarize a TOML parse failure WITHOUT reproducing any source text.
///
/// `toml::de::Error`'s `Display` renders an annotated snippet that includes the
/// offending line verbatim, so a syntax error anywhere near `sasl_password =
/// "…"` would carry the credential into a diagnostic this module documents as
/// safe to log. Only two source-free facts are kept: the parser's own message
/// (its first line, since the message is the description of the fault, not the
/// input) and the 1-based line/column derived from the error span.
/// `mu_dialogue::mesh::load` renders its TOML and serde diagnostics verbatim,
/// and those can quote the source line or the offending value (an unquoted
/// `issuer_key = 8675309` arrives as `invalid type: integer \`8675309\``). Keep
/// the part of its message that names the file and the failure kind and
/// withhold the parser's own text; the other messages carry only a path.
fn mesh_error_summary(message: &str) -> String {
    for marker in ["is not valid TOML", "is malformed"] {
        if let Some(idx) = message.find(marker) {
            let head = &message[..idx + marker.len()];
            return format!(
                "{head} (parser diagnostic withheld: it may quote the source or a value)"
            );
        }
    }
    message
        .lines()
        .next()
        .filter(|l| !l.is_empty())
        .unwrap_or("mesh config failed to load")
        .to_string()
}

fn toml_error_summary(text: &str, e: &toml::de::Error) -> String {
    let message = e.message().lines().next().unwrap_or_default().trim();
    match e.span() {
        Some(span) => {
            let (line, col) = line_col(text, span.start);
            format!("{message} (at line {line}, column {col})")
        }
        None => message.to_string(),
    }
}

/// 1-based line and column of byte `offset` in `text`. Used to point at a
/// parse error without quoting what is there.
fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let mut cut = offset.min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let before = &text[..cut];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
    (line, col)
}

/// Load both the IRC and mesh configuration. Mesh loading is delegated to the
/// shared `mu_dialogue::mesh::load` unchanged: `path` holds `[irc]` and
/// `[dialogue.mesh]`, `fleet_path` holds the top-level `[mesh]` that unset mesh
/// fields inherit from (usually the same file, which the mesh loader handles).
pub fn load(path: &Path, fleet_path: &Path) -> Result<GatewayConfig, ConfigError> {
    let irc = load_irc(path)?;
    let mesh =
        mesh::load(path, fleet_path).map_err(|e| ConfigError::Mesh(mesh_error_summary(&e)))?;
    Ok(GatewayConfig { irc, mesh })
}

/// Turn a parsed `[irc]` section into a validated config, applying defaults and
/// enforcing the credential rules.
fn validate(raw: IrcRaw) -> Result<IrcConfig, ConfigError> {
    let require = |v: Option<String>, field: &'static str| -> Result<String, ConfigError> {
        v.filter(|s| !s.trim().is_empty())
            .ok_or(ConfigError::MissingField(field))
    };
    let server = require(raw.server, "server")?;
    let nick = require(raw.nick, "nick")?;
    validate_nick(&nick).map_err(ConfigError::InvalidNick)?;

    // Non-empty-only: an empty string in TOML is treated as "unset" so it does
    // not, for instance, half-configure SASL with a blank user.
    let nonempty = |v: Option<String>| v.filter(|s| !s.trim().is_empty());

    let tls = raw.tls.unwrap_or(true);
    let tls_trust = tls_trust(tls, nonempty(raw.tls_ca_file), raw.tls_system_roots)?;
    let user = nonempty(raw.sasl_user);
    let inline = nonempty(raw.sasl_password);
    let file = nonempty(raw.sasl_password_file);

    if inline.is_some() && file.is_some() {
        return Err(ConfigError::SaslPasswordConflict);
    }
    let sasl = match (user, inline, file) {
        // No SASL configured at all: fine.
        (None, None, None) => None,
        (Some(user), Some(pw), None) => Some(SaslCreds {
            user,
            password: Secret(pw),
        }),
        (Some(user), None, Some(file)) => Some(SaslCreds {
            user,
            password: read_password_file(&file)?,
        }),
        // A user with no password source, or a password source with no user.
        (Some(_), None, None) => return Err(ConfigError::SaslMissingPassword),
        (None, Some(_), None) | (None, None, Some(_)) => return Err(ConfigError::SaslMissingUser),
        // Both password sources set is rejected as a conflict above.
        (_, Some(_), Some(_)) => unreachable!("password conflict checked above"),
    };

    let puppets = match raw.puppets {
        None => PuppetsConfig::default(),
        Some(table) => parse_puppets(&table, tls)?,
    };

    Ok(IrcConfig {
        server,
        tls,
        tls_trust,
        nick,
        sasl,
        channel_prefix: nonempty(raw.channel_prefix).unwrap_or_else(|| "#".to_string()),
        lobby: nonempty(raw.lobby).unwrap_or_else(|| "#mu".to_string()),
        observe_agent_dms: raw.observe_agent_dms.unwrap_or(true),
        puppets,
    })
}

/// Validate the `[irc.puppets]` table. Faults are described in the table's own
/// terms (field names and types, never values), the way `[irc]` faults are.
fn parse_puppets(table: &toml::Value, tls: bool) -> Result<PuppetsConfig, ConfigError> {
    // Named before the generic unknown-field path so the diagnostic says WHY
    // the key has no place here, not merely that it is unknown.
    if let Some(t) = table.as_table() {
        for key in ["sasl_user", "sasl_password", "sasl_password_file"] {
            if t.contains_key(key) {
                return Err(ConfigError::PuppetsSasl(key));
            }
        }
    }
    let raw: PuppetsRaw = table.clone().try_into().map_err(|_: toml::de::Error| {
        ConfigError::PuppetsMalformed(table_fault(table, "[irc.puppets]", PUPPETS_FIELDS))
    })?;
    let defaults = PuppetsConfig::default();
    let roles = match raw.roles {
        None => defaults.roles,
        Some(roles) => {
            for role in &roles {
                if role.is_empty() || !role.chars().all(|c| c.is_ascii_alphanumeric()) {
                    return Err(ConfigError::PuppetsInvalid(
                        "roles",
                        "entries must be non-empty and alphanumeric (a peer role such as `cc`)",
                    ));
                }
                if role == "human" {
                    return Err(ConfigError::PuppetsInvalid(
                        "roles",
                        "cannot include `human`: humans keep their own nicks and never get a puppet",
                    ));
                }
            }
            roles
        }
    };
    let bounded =
        |v: Option<u64>, field: &'static str, dflt: usize| -> Result<usize, ConfigError> {
            match v {
                None => Ok(dflt),
                Some(0) => Err(ConfigError::PuppetsInvalid(
                    field,
                    "must be at least 1 (set `enabled = false` to turn puppets off)",
                )),
                Some(n) => usize::try_from(n)
                    .map_err(|_| ConfigError::PuppetsInvalid(field, "is too large")),
            }
        };
    // `max` is both the highest slot number and the size of the credential
    // set, so it is resolved once, ahead of the two checks that need it.
    //
    // There is deliberately NO upper bound on it. The design says pool size is
    // config — "raising it is a config change, not a source change" — and the
    // gateway host is exempted from the server's per-IP connection limit, so a
    // compiled ceiling would be exactly the source change the design says is
    // not needed. Nor is one load-bearing: an absurd `max` costs nothing here
    // when the pool is unprovisioned, and when it IS provisioned the credential
    // loop below returns at the FIRST slot whose files are not there, so
    // reaching slot n means the operator really does have n credentials on
    // disk.
    let max = bounded(raw.max, "max", defaults.max)?;
    // Every account the prefix generates must be a LEGAL nick, because the
    // account is also what that puppet registers as. Slot 1 is not the test:
    // `<prefix>-1` can sit inside the nick limit while `<prefix>-16` is two
    // characters over it, and the pool would then come up with its last slots
    // unable to register. The highest slot is the longest name, so it is the
    // one that decides; slot 1 is checked too, since it is the one whose
    // FIRST character has to be legal and a one-slot pool never reaches the
    // other arm.
    let slot_prefix = raw
        .slot_prefix
        .unwrap_or_else(|| defaults.slot_prefix.clone());
    // The DEFAULT prefix is checked too. It is short and always legal today,
    // so this never fires for it — but making the check conditional on the key
    // being present is the kind of reachability argument that quietly stops
    // being true, and the check is two `validate_nick` calls.
    for n in [1, max] {
        let account = format!("{slot_prefix}-{n}");
        validate_nick(&account).map_err(|fault| ConfigError::PuppetsSlotNick(account, fault))?;
    }
    // A provisioned pool that cannot present its credentials is a
    // misconfiguration, so it is refused at load rather than discovered one
    // failed registration at a time (AGENTS.md invariant 7: fail fast, and say
    // what to do). The first bad path is named — the operator needs the path,
    // not a count.
    //
    // The credential is BUILT, not stat'd and not merely unwrapped. `is_file`
    // admits an empty file and a DER blob; decoding the PEM armour on top of
    // that still admits base64 garbage under a `CERTIFICATE` header, a key of
    // a kind this build cannot sign with, and — the one hand-provisioning
    // sixteen slots actually produces — a certificate and key crossed between
    // two slots, where both files are individually perfect. All of those fail
    // at connect, which is exactly the distance between cause and symptom this
    // check exists to close, so what is built here is the real
    // `CertifiedKey`: X.509 parsed, signing key loaded, public halves compared.
    //
    // The one thing still not knowable here is whether the SERVER has this
    // fingerprint filed under this account. That is `NS CERT ADD`'s business
    // and surfaces at connect as a refused login.
    // An empty string is not a path. Every other path-valued field in this file
    // goes through the same filter, and without it `slot_certs_dir = ""` would
    // be reported as a credential that is missing rather than as a pool that
    // was never provisioned.
    let slot_certs_dir = raw
        .slot_certs_dir
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from);
    let slot_certs_dir = match slot_certs_dir {
        None => None,
        Some(dir) => {
            // A slot's credential is presented IN the TLS handshake, so a
            // cleartext connection has nowhere to put it. Refused before any
            // credential is read, the way `tls_trust` refuses `TrustWithoutTls`
            // ahead of opening the CA bundle: the cheap contradiction first.
            if !tls {
                return Err(ConfigError::PuppetsCertsWithoutTls);
            }
            // Same rule as the credentials themselves: an unreadable
            // directory is not an absent one, and saying so is the difference
            // between fixing a permission and re-provisioning a pool.
            match std::fs::metadata(&dir) {
                Ok(m) if m.is_dir() => {}
                Ok(_) => {
                    return Err(ConfigError::PuppetsInvalid(
                        "slot_certs_dir",
                        "is not a directory; it must hold one `<account>.crt` and \
                         `<account>.key` per slot",
                    ))
                }
                Err(e) => {
                    return Err(ConfigError::PuppetsSlotCert(
                        dir.display().to_string(),
                        if e.kind() == std::io::ErrorKind::NotFound {
                            SlotCertFault::Missing
                        } else {
                            SlotCertFault::Read(e)
                        },
                    ))
                }
            }
            // Leaf DER → the account it was found under, so a certificate
            // filed twice is caught. One fingerprint maps to ONE account (Ergo
            // refuses to register a second, and SASL EXTERNAL requires
            // `authzid` == `authcid`), so a pool sharing a certificate has
            // exactly one slot that can log in and the rest fail at connect —
            // a provisioning mistake that is entirely visible from here.
            let mut seen: HashMap<Vec<u8>, String> = HashMap::new();
            for n in 1..=max {
                let account = format!("{slot_prefix}-{n}");
                let leaf = check_slot_credential(
                    &dir.join(format!("{account}.crt")),
                    &dir.join(format!("{account}.key")),
                )?;
                if let Some(first) = seen.insert(leaf, account.clone()) {
                    return Err(ConfigError::PuppetsDuplicateSlotCert(first, account));
                }
            }
            Some(dir)
        }
    };
    Ok(PuppetsConfig {
        enabled: raw.enabled.unwrap_or(defaults.enabled),
        roles,
        daemons: raw.daemons.unwrap_or(defaults.daemons),
        max,
        min_age_secs: raw.min_age_secs.unwrap_or(defaults.min_age_secs),
        connect_parallelism: bounded(
            raw.connect_parallelism,
            "connect_parallelism",
            defaults.connect_parallelism,
        )?,
        quit_grace_secs: match raw.quit_grace_secs {
            None => defaults.quit_grace_secs,
            Some(0) => {
                return Err(ConfigError::PuppetsInvalid(
                    "quit_grace_secs",
                    "must be at least 1 (no grace would force every QUIT)",
                ))
            }
            Some(n) if n > QUIT_GRACE_MAX_SECS => return Err(ConfigError::PuppetsGraceTooLong),
            Some(n) => n,
        },
        command_queue: bounded(raw.command_queue, "command_queue", defaults.command_queue)?,
        event_queue: bounded(raw.event_queue, "event_queue", defaults.event_queue)?,
        join_retry_ms: match raw.join_retry_ms {
            None => defaults.join_retry_ms,
            Some(0) => {
                return Err(ConfigError::PuppetsInvalid(
                    "join_retry_ms",
                    "must be at least 1",
                ))
            }
            Some(n) => n,
        },
        slot_prefix,
        slot_certs_dir,
    })
}

/// Build the credential a slot will actually present, check it holds together,
/// and return its leaf certificate in DER. Not a proxy for the real thing — it
/// IS the real thing: the same [`CertifiedKey`] the client side hands to
/// rustls. The leaf goes back to the caller so the pool can also refuse one
/// certificate filed under two accounts.
///
/// Four distinct faults, each otherwise a connect-time surprise:
///
///  - the file is missing, unreadable, or not PEM;
///  - the PEM armour is right but the DER inside is not an X.509 certificate
///    (`keys_match` parses the leaf, so base64 garbage under a `CERTIFICATE`
///    header does not get through — envelope-checking alone let it through);
///  - the key is not one this build can sign with (wrong algorithm, or
///    structurally intact but unusable);
///  - the certificate and the key are each fine but are not a PAIR, compared
///    by SubjectPublicKeyInfo.
///
/// The last one matters most: a cert and key crossed between two slots is the
/// mistake provisioning sixteen of these actually produces, both files parse,
/// and the only symptom is a refused login much later.
///
/// What is still not knowable here: whether the SERVER has this certificate's
/// fingerprint filed under this account. That is `NS CERT ADD`'s business and
/// shows up at connect as a failed login.
fn check_slot_credential(crt: &Path, key: &Path) -> Result<Vec<u8>, ConfigError> {
    let bad = |p: &Path, f: SlotCertFault| ConfigError::PuppetsSlotCert(p.display().to_string(), f);
    let pair = |f: SlotCertFault| {
        ConfigError::PuppetsSlotCert(format!("{} and {}", crt.display(), key.display()), f)
    };
    // `is_file()` answers false for "absent", "is a directory" and "the
    // process may not search the directory it is in" alike, so using it here
    // would tell an operator whose credential is merely unreadable to go
    // generate a replacement for a file that is sitting right there. Only
    // NotFound is missing; every other io error is reported as itself, the way
    // the CA loader keeps its [`CaFault::Read`].
    for p in [crt, key] {
        match std::fs::metadata(p) {
            Ok(m) if m.is_file() => {}
            Ok(_) => return Err(bad(p, SlotCertFault::NotAFile)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(bad(p, SlotCertFault::Missing))
            }
            Err(e) => return Err(bad(p, SlotCertFault::Read(e))),
        }
    }
    let certs = CertificateDer::pem_file_iter(crt)
        .map_err(|e| bad(crt, slot_pem_fault(e, false)))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| bad(crt, slot_pem_fault(e, false)))?;
    if certs.is_empty() {
        return Err(bad(crt, SlotCertFault::NoCert));
    }
    let leaf = certs[0].as_ref().to_vec();
    let key_der =
        PrivateKeyDer::from_pem_file(key).map_err(|e| bad(key, slot_pem_fault(e, true)))?;
    let signing = any_supported_type(&key_der).map_err(|_| bad(key, SlotCertFault::UnusableKey))?;
    // `CertifiedKey::keys_match` (rustls 0.23) recovers the signing key's
    // SubjectPublicKeyInfo, parses the leaf with webpki, and compares the two.
    // Its error surface is small, and each variant names a DIFFERENT file to
    // go fix, so it is mapped exhaustively rather than through a catch-all
    // that would point the operator at the wrong one:
    //
    //   InvalidCertificate(_)          the leaf is not X.509       -> the cert
    //   InconsistentKeys(Unknown)      no public half to compare   -> the key
    //   InconsistentKeys(KeyMismatch)  both fine, not a pair       -> both
    //   NoCertificatesPresented        unreachable, checked above  -> the cert
    CertifiedKey::new(certs, signing)
        .keys_match()
        .map_err(|e| match e {
            rustls::Error::InvalidCertificate(_) | rustls::Error::NoCertificatesPresented => {
                bad(crt, SlotCertFault::NotACertificate)
            }
            rustls::Error::InconsistentKeys(rustls::InconsistentKeys::Unknown) => {
                bad(key, SlotCertFault::UnusableKey)
            }
            // Includes InconsistentKeys(KeyMismatch). Anything else rustls
            // grows here is still a fault of the PAIR, so naming both files
            // stays correct even for a variant that does not exist yet.
            _ => pair(SlotCertFault::Mismatch),
        })
        .map(|()| leaf)
}

/// Map the PEM reader's error onto a fixed phrase. `key` picks which "nothing
/// of the kind I wanted" phrase applies, because the reader reports an empty
/// file and a file holding only the OTHER half as the same
/// [`rustls_pki_types::pem::Error::NoItemsFound`].
fn slot_pem_fault(e: rustls_pki_types::pem::Error, key: bool) -> SlotCertFault {
    match e {
        rustls_pki_types::pem::Error::Io(e) => SlotCertFault::Read(e),
        rustls_pki_types::pem::Error::NoItemsFound if key => SlotCertFault::NoKey,
        rustls_pki_types::pem::Error::NoItemsFound => SlotCertFault::NoCert,
        _ => SlotCertFault::NotPem,
    }
}

/// Resolve what a server certificate is verified against.
///
/// Three rules, each one a contradiction the operator would otherwise only find
/// out about at connect time, or never:
///
/// - **Trust settings without TLS.** A cleartext connection presents no
///   certificate at all, so `tls_ca_file` or `tls_system_roots` alongside `tls =
///   false` describes verification that cannot happen. Refused rather than
///   ignored: the operator who wrote them believes the connection is verified.
/// - **`tls_system_roots = false` with no bundle.** That is an empty trust
///   store, and an empty store cannot validate anything. Refused here, where the
///   fix is one line away, rather than as a connect failure every 2 seconds.
/// - **A bundle that cannot be used.** The file is read and parsed NOW — the
///   same moment `--check-config` runs — so a missing path, a file that is not
///   PEM, and a bundle with no certificate in it are startup errors. The fault
///   comes from [`CaFault`], which carries no file content.
///
/// Note what is NOT a rule: a bundle never REPLACES the system anchors unless
/// asked to. `tls_system_roots` defaults to `true`, so adding a private CA for a
/// LAN server leaves every public network still verifiable.
fn tls_trust(
    tls: bool,
    ca_file: Option<String>,
    system_roots: Option<bool>,
) -> Result<TlsTrust, ConfigError> {
    if !tls {
        if ca_file.is_some() || system_roots.is_some() {
            return Err(ConfigError::TrustWithoutTls);
        }
        // Nothing is verified on a cleartext connection; the value is inert.
        return Ok(TlsTrust::system());
    }
    let system_roots = system_roots.unwrap_or(true);
    let Some(ca_file) = ca_file else {
        if !system_roots {
            return Err(ConfigError::NoTrustAnchors);
        }
        return Ok(TlsTrust::system());
    };
    let path = PathBuf::from(ca_file);
    TlsTrust::with_ca_file(&path, system_roots).map_err(|e| ConfigError::TlsCaFile(path, e))
}

/// Read a SASL password from a file, trimming a trailing newline. An empty file
/// counts as no password (so a stray empty file cannot silently authenticate).
/// The error names the path and the io error, never file contents.
fn read_password_file(file: &str) -> Result<Secret, ConfigError> {
    let path = PathBuf::from(file);
    let raw =
        std::fs::read_to_string(&path).map_err(|e| ConfigError::PasswordFile(path.clone(), e))?;
    let pw = raw.trim_end_matches(['\n', '\r']).to_string();
    if pw.is_empty() {
        return Err(ConfigError::SaslMissingPassword);
    }
    Ok(Secret(pw))
}
