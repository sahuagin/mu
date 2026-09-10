//! Gateway-local `[irc]` configuration and its validation.
//!
//! The mesh side is NOT re-implemented here: [`load`] delegates to
//! `mu_dialogue::mesh::load`, so there is one loader and one set of defaults for
//! the mesh connection. This module owns only the IRC-specific `[irc]` section.
//!
//! Secrets are handled carefully. A SASL password read from config or a file is
//! wrapped in [`Secret`], whose `Debug` redacts it, and every [`ConfigError`]
//! names a field or a path but never a secret value — including the
//! deserialization path, which reports the field name and expected type from
//! [`IRC_FIELDS`] rather than forwarding a serde message that would quote the
//! offending value. The mesh half carries two secrets: the borrowed
//! [`MeshConfig`] derives `Debug` over its plaintext Ed25519 `issuer_key`, and
//! its `nats_url` may hold userinfo credentials — so [`GatewayConfig`] does NOT
//! derive `Debug`; it prints the mesh section through a wrapper that redacts
//! both.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use mu_dialogue::mesh;
pub use mu_dialogue::mesh::MeshConfig;

/// A credential value that must never be logged. `Debug` redacts it; the plain
/// value is reachable only through [`Secret::expose`], which callers use when
/// they actually authenticate.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
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
    /// diagnostic. See [`toml_error_summary`].
    #[error("{0} is not valid TOML: {1}")]
    Toml(PathBuf, String),
    #[error("no [irc] section in {0}")]
    MissingSection(PathBuf),
    /// The `[irc]` section did not deserialize. The second field is built by
    /// [`section_fault`] from the field NAME and the expected TYPE only — the
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
    #[error("mesh config: {0}")]
    Mesh(String),
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
    nick: Option<String>,
    sasl_user: Option<String>,
    sasl_password: Option<String>,
    sasl_password_file: Option<String>,
    channel_prefix: Option<String>,
    lobby: Option<String>,
    observe_agent_dms: Option<bool>,
}

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
    ("nick", FieldType::Str),
    ("sasl_user", FieldType::Str),
    ("sasl_password", FieldType::Str),
    ("sasl_password_file", FieldType::Str),
    ("channel_prefix", FieldType::Str),
    ("lobby", FieldType::Str),
    ("observe_agent_dms", FieldType::Bool),
];

/// The TOML type an `[irc]` field accepts.
#[derive(Clone, Copy)]
enum FieldType {
    Str,
    Bool,
}

impl FieldType {
    fn name(self) -> &'static str {
        match self {
            Self::Str => "string",
            Self::Bool => "boolean",
        }
    }

    fn matches(self, value: &toml::Value) -> bool {
        match self {
            Self::Str => value.is_str(),
            Self::Bool => value.is_bool(),
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
    let Some(table) = section.as_table() else {
        return "[irc] is not a table".to_string();
    };
    for (key, value) in table {
        match IRC_FIELDS.iter().find(|(name, _)| name == key) {
            None => return format!("unknown field{}", named(key)),
            Some((name, ty)) if !ty.matches(value) => {
                return format!("field `{name}` expects a {}", ty.name())
            }
            Some(_) => {}
        }
    }
    // Every key is recognized and well-typed, so the failure is structural
    // (a duplicate or a nested table serde rejected). Say so without quoting.
    "section is not a valid [irc] table".to_string()
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

    // Non-empty-only: an empty string in TOML is treated as "unset" so it does
    // not, for instance, half-configure SASL with a blank user.
    let nonempty = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
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

    Ok(IrcConfig {
        server,
        tls: raw.tls.unwrap_or(true),
        nick,
        sasl,
        channel_prefix: nonempty(raw.channel_prefix).unwrap_or_else(|| "#".to_string()),
        lobby: nonempty(raw.lobby).unwrap_or_else(|| "#mu".to_string()),
        observe_agent_dms: raw.observe_agent_dms.unwrap_or(true),
    })
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
