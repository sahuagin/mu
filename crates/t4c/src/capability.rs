//! The unit of the registry: a [`Capability`] — one addressable, invokable node.

use crate::path::CapPath;
use serde::Deserialize;

/// One discoverable capability: where it lives ([`CapPath`]), what it is for
/// (summary + keywords), how to run it (`invoke` argv), how to learn it
/// (`help`), and what it requires (`requires` — the capability gate, so
/// discovery can track permission).
#[derive(Debug, Clone)]
pub struct Capability {
    pub path: CapPath,
    pub summary: String,
    pub keywords: Vec<String>,
    pub invoke: Vec<String>,
    pub help: Option<HelpSpec>,
    pub requires: Vec<String>,
}

/// How to fetch a capability's help / schema.
#[derive(Debug, Clone)]
pub struct HelpSpec {
    /// argv that yields help, e.g. `["jj", "status", "--help"]`.
    pub argv: Vec<String>,
    /// True if the tool speaks the `--help-ai [--json]` convention (mu-kex4.5
    /// ratifies that convention as the published standard).
    pub ai: bool,
}

/// Deserialization target for a tool's `--help-ai --json` output.
///
/// This is the de-facto schema the probe consumes. mu-kex4.5 turns it into the
/// published standard and ships emitters (a clap derive + a shell template) so
/// any tool can produce conforming output for free.
#[derive(Debug, Clone, Deserialize)]
pub struct HelpAiDoc {
    pub name: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub subcommands: Vec<HelpAiSub>,
}

/// One subcommand entry within a [`HelpAiDoc`].
#[derive(Debug, Clone, Deserialize)]
pub struct HelpAiSub {
    pub name: String,
    #[serde(default)]
    pub summary: String,
}
