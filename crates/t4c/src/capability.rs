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
    /// Explicit hierarchy weight (higher = preferred). Applied as a
    /// *deterministic tie-break* after the relevance score and before the path
    /// name, so an operator-declared ordering decides among comparably-relevant
    /// capabilities — e.g. `bash.sprint-start` over `bash.git.*` for an
    /// "isolated workspace" intent — without ever lifting an irrelevant
    /// capability above a relevant one. `0` is neutral (the default for probed,
    /// chained, and reconstructed-from-snapshot capabilities); curated catalog
    /// entries may set it. See [`crate::rank::sort_ranked`].
    pub priority: i32,
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

/// `skip_serializing_if` predicate for a false bool — keeps a minimal producer's
/// emitted `--help-ai` lean (no `"positional": false` noise on every arg).
fn is_false(b: &bool) -> bool {
    !*b
}

/// One CLI argument as described by `--help-ai --json` (the standard's `args`
/// entry). Only `name` is required; the rest is optional so a producer emits
/// just what it knows — enough for a consumer to build an invocation or a
/// JSON-Schema input.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HelpAiArg {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub positional: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub required: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub takes_value: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub multiple: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub possible_values: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default: Vec<String>,
}

/// Deserialization target for a tool's `--help-ai --json` output — the superset
/// standard (`crates/t4c/docs/help-ai-standard.md`). A single RECURSIVE node:
/// the root document and every subcommand share this shape. Required core is
/// `name` plus the discovery-facing `summary`; every rich field is optional, so
/// a minimal `{name, summary}` producer still deserializes and unknown fields
/// are ignored (forward-compatible).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HelpAiDoc {
    pub name: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    /// Recursive: each entry is itself a node with this same schema.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subcommands: Vec<HelpAiDoc>,
    // ── optional rich fields (the superset) ──
    /// Per-argument calling convention.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<HelpAiArg>,
    /// One-line usage / synopsis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<String>,
    /// JSON Schema of this command's stdout, when it has structured output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    /// Alternate names that invoke this node.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Explicit runnability. `None` ⇒ infer it: a node with no `subcommands` is
    /// a runnable leaf, a node with children is a group. See
    /// [`crate::source::HelpAiProbeSource::doc_to_caps`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invokable: Option<bool>,
    /// Producer-stated invocation path. PARSE-ONLY — t4c computes its own
    /// capability path from tree position; this never overrides addressing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl HelpAiDoc {
    /// Resolve this node's runnability per the standard: the explicit
    /// `invokable` flag when present, else true iff it has no subcommands.
    pub fn is_invokable(&self) -> bool {
        self.invokable.unwrap_or(self.subcommands.is_empty())
    }
}

/// Back-compat alias: the producer/consumer historically named the subcommand
/// node `HelpAiSub`. It is now the same recursive node as [`HelpAiDoc`].
pub type HelpAiSub = HelpAiDoc;

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

    #[test]
    fn superset_doc_round_trips_with_rich_and_nested() {
        let doc = HelpAiDoc {
            name: "tool".to_string(),
            summary: "does things".to_string(),
            keywords: vec!["kw".to_string()],
            subcommands: vec![HelpAiDoc {
                name: "run".to_string(),
                summary: "run it".to_string(),
                args: vec![HelpAiArg {
                    name: "target".to_string(),
                    positional: true,
                    required: true,
                    ..Default::default()
                }],
                usage: Some("tool run <target>".to_string()),
                output_schema: Some(serde_json::json!({"type": "object"})),
                ..Default::default()
            }],
            ..Default::default()
        };
        let back: HelpAiDoc = serde_json::from_str(&serde_json::to_string(&doc).unwrap()).unwrap();
        assert_eq!(back.subcommands.len(), 1);
        let run = &back.subcommands[0];
        assert_eq!(run.name, "run");
        assert_eq!(run.args.len(), 1);
        assert_eq!(run.args[0].name, "target");
        assert!(run.args[0].required);
        assert_eq!(run.usage.as_deref(), Some("tool run <target>"));
        assert!(run.output_schema.is_some());
    }

    #[test]
    fn minimal_doc_still_parses() {
        // back-compat: a producer emitting only name + summary deserializes,
        // every rich field defaulted.
        let doc: HelpAiDoc = serde_json::from_str(r#"{"name":"t","summary":"s"}"#).unwrap();
        assert_eq!(doc.name, "t");
        assert_eq!(doc.summary, "s");
        assert!(doc.args.is_empty());
        assert!(doc.usage.is_none());
        assert!(doc.subcommands.is_empty());
        assert!(doc.is_invokable(), "a childless node is a runnable leaf");
    }

    #[test]
    fn is_invokable_explicit_overrides_then_infers() {
        let mut n = HelpAiDoc {
            name: "x".to_string(),
            invokable: Some(false),
            ..Default::default()
        };
        assert!(!n.is_invokable(), "explicit invokable:false wins");
        n.invokable = Some(true);
        n.subcommands = vec![HelpAiDoc {
            name: "c".to_string(),
            ..Default::default()
        }];
        assert!(
            n.is_invokable(),
            "explicit invokable:true wins over children"
        );
        let group = HelpAiDoc {
            name: "g".to_string(),
            subcommands: vec![HelpAiDoc {
                name: "c".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(!group.is_invokable(), "children + no flag => group");
        let leaf = HelpAiDoc {
            name: "l".to_string(),
            ..Default::default()
        };
        assert!(leaf.is_invokable(), "no children + no flag => leaf");
    }
}
