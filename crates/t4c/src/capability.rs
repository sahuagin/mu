//! The unit of the registry: a [`Capability`] — one addressable, invokable node.

use crate::path::CapPath;
use serde::{Deserialize, Serialize};

/// One discoverable capability: where it lives ([`CapPath`]), what it is for
/// (summary + keywords), how to run it (`invoke` argv), how to learn it
/// (`help`), what it requires (`requires` — the capability gate, so discovery
/// can track permission), and what it does to the world (`effects` — the
/// safety surface a model reasons over before invoking).
#[derive(Debug, Clone)]
pub struct Capability {
    pub path: CapPath,
    pub summary: String,
    pub keywords: Vec<String>,
    pub invoke: Vec<String>,
    pub help: Option<HelpSpec>,
    pub requires: Vec<String>,
    /// What running this does to the world, when known. `None` means the
    /// effects are *unannotated* — deliberately distinct from a known-benign
    /// `Some(Effects::default())`. A source that can't speak to effects leaves
    /// this `None` rather than fabricating a falsely-benign claim.
    pub effects: Option<Effects>,
}

/// Filesystem reach of a capability.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsEffect {
    /// Touches no files.
    #[default]
    None,
    /// Reads files but does not modify them.
    Read,
    /// Creates, modifies, or deletes files.
    Write,
}

/// What invoking a capability does to the world — the safety surface a model
/// consumer reasons over before it commits to a call (cold-agent session
/// c9ecd980 named this "the biggest practical gap"). The questions it answers:
/// read-only or writes? touches version control? reaches the network? spends
/// metered resources? spawns a daemon?
///
/// An `Effects` value is a *positive claim* that exactly these effects apply.
/// Where the effects are unknown, [`Capability::effects`] stays `None`; this
/// type's `Default` (no reach, no flags) is the conservative claim and is only
/// ever attached where it is actually true.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Effects {
    /// Filesystem reach: none / read / write.
    #[serde(default)]
    pub filesystem: FsEffect,
    /// Mutates version control (commit, rebase, push, branch ops).
    #[serde(default)]
    pub vcs: bool,
    /// Makes network calls.
    #[serde(default)]
    pub network: bool,
    /// Spends metered or paid resources (API dollars, token budget).
    #[serde(default)]
    pub spend: bool,
    /// Spawns a long-running process / daemon.
    #[serde(default)]
    pub process: bool,
}

/// A session's declared restrictions on what may run — the *appropriateness*
/// axis, orthogonal to availability (installed) and permission (in the
/// biscuit). A capability can be installed and permitted yet still be the wrong
/// thing right now because the operator said "read-only" or "no network". The
/// discovery surface uses [`Effects::disallowed_by`] to mark such a capability
/// `allowed_by_session: false` with a reason rather than hiding it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionConstraints {
    /// No filesystem writes (create / edit / delete).
    pub no_writes: bool,
    /// No version-control mutation.
    pub no_vcs: bool,
    /// No network calls.
    pub no_network: bool,
    /// No spending of metered / paid resources.
    pub no_spend: bool,
    /// No spawning of long-running processes / daemons.
    pub no_process: bool,
}

impl Effects {
    /// If these effects violate `constraints`, return a human-readable reason
    /// the capability — though installed and permitted — is *inappropriate*
    /// this session. `None` means it clears every active constraint. The
    /// `Capability::effects == None` (unannotated) case is the caller's to
    /// decide; an unknown-effects capability has no claim to check here.
    pub fn disallowed_by(&self, constraints: &SessionConstraints) -> Option<String> {
        if constraints.no_writes && self.filesystem == FsEffect::Write {
            return Some("session is read-only (no filesystem writes)".to_string());
        }
        if constraints.no_vcs && self.vcs {
            return Some("session disallows version-control changes".to_string());
        }
        if constraints.no_network && self.network {
            return Some("session disallows network access".to_string());
        }
        if constraints.no_spend && self.spend {
            return Some("session disallows spending metered resources".to_string());
        }
        if constraints.no_process && self.process {
            return Some("session disallows spawning processes".to_string());
        }
        None
    }
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelpAiSub {
    pub name: String,
    #[serde(default)]
    pub summary: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A write-touching, VCS-mutating effect — e.g. `jj commit`.
    fn writes_and_commits() -> Effects {
        Effects {
            filesystem: FsEffect::Write,
            vcs: true,
            ..Effects::default()
        }
    }

    #[test]
    fn read_only_session_flags_writes_not_reads() {
        let ro = SessionConstraints {
            no_writes: true,
            ..SessionConstraints::default()
        };
        // a writer is flagged, with a reason
        assert!(writes_and_commits().disallowed_by(&ro).is_some());
        // a pure reader clears the constraint
        let reader = Effects {
            filesystem: FsEffect::Read,
            ..Effects::default()
        };
        assert!(reader.disallowed_by(&ro).is_none());
    }

    #[test]
    fn each_constraint_matches_its_own_axis() {
        let net = Effects {
            network: true,
            ..Effects::default()
        };
        let no_net = SessionConstraints {
            no_network: true,
            ..SessionConstraints::default()
        };
        assert!(net.disallowed_by(&no_net).is_some());
        // a network effect is untouched by an unrelated (no-vcs) constraint
        let no_vcs = SessionConstraints {
            no_vcs: true,
            ..SessionConstraints::default()
        };
        assert!(net.disallowed_by(&no_vcs).is_none());
    }

    #[test]
    fn no_constraints_allows_everything() {
        let unconstrained = SessionConstraints::default();
        assert!(writes_and_commits().disallowed_by(&unconstrained).is_none());
    }

    #[test]
    fn default_effects_is_the_conservative_claim() {
        // Default = no reach, no flags — clears every possible constraint.
        let all = SessionConstraints {
            no_writes: true,
            no_vcs: true,
            no_network: true,
            no_spend: true,
            no_process: true,
        };
        assert!(Effects::default().disallowed_by(&all).is_none());
        assert_eq!(Effects::default().filesystem, FsEffect::None);
    }

    #[test]
    fn effects_json_roundtrips() {
        let e = writes_and_commits();
        let json = serde_json::to_string(&e).expect("serialize");
        let back: Effects = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(e, back);
        // FsEffect serializes lowercase (the wire convention)
        assert!(json.contains("\"write\""));
    }

    #[test]
    fn effects_deserializes_from_partial_json() {
        // omitted fields fall back to Default — a source can annotate just the
        // axis it knows (e.g. only `network`).
        let e: Effects = serde_json::from_str(r#"{"network": true}"#).expect("partial");
        assert!(e.network);
        assert_eq!(e.filesystem, FsEffect::None);
        assert!(!e.vcs);
    }
}
