//! Registry sources: where capabilities come from. mu plugs its live, in-process
//! manifest as just another source; the CLI uses a TOML config, a `--help-ai`
//! probe, and small in-code defaults.

use crate::capability::{Capability, Effects, HelpAiDoc, HelpSpec};
use crate::path::CapPath;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Command;

/// A source of capabilities. Implement this to teach t4c about a new universe of
/// tools — a config file, a `--help-ai` probe, mu's manifest, anything.
pub trait RegistrySource {
    /// Human-facing source name (for diagnostics / provenance).
    fn name(&self) -> &str;
    /// Produce this source's capabilities. Best-effort: a source that can't
    /// reach part of its world should skip it rather than fail the whole build,
    /// unless the failure is genuinely fatal (e.g. malformed config).
    fn capabilities(&self) -> Result<Vec<Capability>>;
}

/// In-code defaults — a tiny curated catalog, and a convenient second source in
/// tests to exercise multi-source merge / override.
pub struct StaticSource {
    name: String,
    caps: Vec<Capability>,
}

impl StaticSource {
    pub fn new(name: impl Into<String>, caps: Vec<Capability>) -> Self {
        Self {
            name: name.into(),
            caps,
        }
    }
}

impl RegistrySource for StaticSource {
    fn name(&self) -> &str {
        &self.name
    }
    fn capabilities(&self) -> Result<Vec<Capability>> {
        Ok(self.caps.clone())
    }
}

/// TOML config source. Example shape:
///
/// ```toml
/// [[capability]]
/// path = "bash.jj.status"
/// summary = "show the working-copy and parent status"
/// keywords = ["vcs", "diff", "working copy"]
/// invoke = ["jj", "status"]   # optional; defaults to the path minus its source
/// requires = []
///
/// [capability.help]
/// argv = ["jj", "status", "--help"]
/// ai = false
/// ```
pub struct TomlConfigSource {
    path: PathBuf,
}

impl TomlConfigSource {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Parse capabilities from TOML text. Split out from [`Self::capabilities`]
    /// so it is unit-testable without touching the filesystem.
    pub fn parse_str(text: &str) -> Result<Vec<Capability>> {
        let file: TomlFile = toml::from_str(text).context("parsing t4c TOML config")?;
        let mut caps = Vec::with_capacity(file.capability.len());
        for c in file.capability {
            let path = CapPath::parse(&c.path)
                .with_context(|| format!("bad capability path {:?}", c.path))?;
            let invoke = if c.invoke.is_empty() {
                path.invoke_argv()
            } else {
                c.invoke
            };
            caps.push(Capability {
                path,
                summary: c.summary,
                keywords: c.keywords,
                priority: c.priority,
                invoke,
                help: c.help.map(|h| HelpSpec {
                    argv: h.argv,
                    ai: h.ai,
                }),
                requires: c.requires,
                effects: c.effects,
            });
        }
        Ok(caps)
    }

    /// Serialize capabilities back to the config TOML (the inverse of
    /// [`Self::parse_str`]), so `discover` can persist a self-configured registry.
    pub fn to_toml(caps: &[Capability]) -> Result<String> {
        let file = TomlFile {
            capability: caps
                .iter()
                .map(|c| TomlCap {
                    path: c.path.to_string(),
                    summary: c.summary.clone(),
                    keywords: c.keywords.clone(),
                    priority: c.priority,
                    invoke: c.invoke.clone(),
                    requires: c.requires.clone(),
                    help: c.help.as_ref().map(|h| TomlHelp {
                        argv: h.argv.clone(),
                        ai: h.ai,
                    }),
                    effects: c.effects.clone(),
                })
                .collect(),
        };
        toml::to_string_pretty(&file).context("serializing t4c registry to TOML")
    }
}

impl RegistrySource for TomlConfigSource {
    fn name(&self) -> &str {
        "toml-config"
    }
    fn capabilities(&self) -> Result<Vec<Capability>> {
        // The override layer is BEST-EFFORT: a user's malformed override TOML
        // (a typo) must not brick the whole registry build and take down `find`.
        // We warn loudly to stderr (so the loss isn't silent — the user sees it
        // every invocation until fixed) and contribute no capabilities, letting
        // catalog + chains still resolve. This mirrors the fail-soft-but-visible
        // posture of `verify` (always returns a verdict) and the embed-model
        // fallback (ranks lexically rather than failing). A genuinely missing
        // file is not an error (the source just contributes nothing).
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                eprintln!(
                    "t4c: cannot read override {} ({e}) — ignoring overrides",
                    self.path.display()
                );
                return Ok(Vec::new());
            }
        };
        match Self::parse_str(&text) {
            Ok(caps) => Ok(caps),
            Err(e) => {
                eprintln!(
                    "t4c: malformed override {} ({e}) — ignoring overrides; \
                     fix the TOML to restore them",
                    self.path.display()
                );
                Ok(Vec::new())
            }
        }
    }
}

/// `skip_serializing_if` predicate for the neutral priority, so `to_toml`
/// doesn't write `priority = 0` onto every capability in the persisted registry.
fn is_zero(n: &i32) -> bool {
    *n == 0
}

#[derive(Deserialize, Serialize)]
struct TomlFile {
    #[serde(default)]
    capability: Vec<TomlCap>,
}

#[derive(Deserialize, Serialize)]
struct TomlCap {
    path: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    keywords: Vec<String>,
    /// Explicit hierarchy weight; omitted (and not re-serialized) when neutral.
    #[serde(default, skip_serializing_if = "is_zero")]
    priority: i32,
    #[serde(default)]
    invoke: Vec<String>,
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    help: Option<TomlHelp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    effects: Option<Effects>,
}

#[derive(Deserialize, Serialize)]
struct TomlHelp {
    argv: Vec<String>,
    #[serde(default)]
    ai: bool,
}

/// Probes commands for `--help-ai --json` and turns conforming output into
/// capabilities (a tool node plus one node per subcommand, addressed
/// `<class>.<tool>.<sub>`). A command that is absent or non-conforming is
/// skipped — discovery is best-effort across a heterogeneous environment.
pub struct HelpAiProbeSource {
    commands: Vec<String>,
    source_class: String,
}

impl HelpAiProbeSource {
    pub fn new(commands: Vec<String>) -> Self {
        Self {
            commands,
            source_class: "bash".to_string(),
        }
    }

    /// Turn a parsed `--help-ai` doc into capabilities. Split out for testing
    /// without spawning a subprocess.
    pub fn doc_to_caps(source_class: &str, cmd: &str, doc: HelpAiDoc) -> Result<Vec<Capability>> {
        let mut caps = Vec::new();
        let root_path = format!("{source_class}.{}", doc.name);
        let root_invoke = vec![cmd.to_string()];
        // The root tool node is the discovery anchor. Then recurse the subcommand
        // tree, registering one capability per node whose path is addressable
        // (within [`crate::path::max_depth`]). Registration is INDEPENDENT of
        // permission: the ingested `invokable` flag is metadata for the permission
        // layer (bead mu-3nzm), NOT a discovery gate — a group node stays
        // discoverable and its `t4c help` lists its subcommands. A node deeper than
        // the cap is not
        // separately addressable (its tail is arguments); it is skipped here and
        // surfaced in the parent's rich help. Paths + invoke argv come from tree
        // position — producer `path` is ignored.
        Self::push_node(&mut caps, &root_path, &root_invoke, &doc);
        for sub in &doc.subcommands {
            Self::walk(&mut caps, &root_path, &root_invoke, sub);
        }
        Ok(caps)
    }

    /// Register one subcommand node (when its path is addressable), then descend
    /// into its children regardless.
    fn walk(
        caps: &mut Vec<Capability>,
        parent_path: &str,
        parent_invoke: &[String],
        node: &HelpAiDoc,
    ) {
        let path = format!("{parent_path}.{}", node.name);
        let mut invoke = parent_invoke.to_vec();
        invoke.push(node.name.clone());
        Self::push_node(caps, &path, &invoke, node);
        for sub in &node.subcommands {
            Self::walk(caps, &path, &invoke, sub);
        }
    }

    /// Build + push one capability for `node`. A path that exceeds the addressable
    /// depth (or is otherwise unparseable) is SKIPPED — discovery is best-effort,
    /// and a too-deep subcommand stays reachable via its parent's help + invoke
    /// arguments rather than failing the whole probe.
    fn push_node(caps: &mut Vec<Capability>, path: &str, invoke: &[String], node: &HelpAiDoc) {
        let Ok(cap_path) = CapPath::parse(path) else {
            return;
        };
        // This node's OWN keywords, plus its aliases folded in (so an alias
        // matches in `find`). Ancestor keywords are NOT inherited — the ranker's
        // haystack already carries ancestor names via the path segments.
        let mut keywords = node.keywords.clone();
        keywords.extend(node.aliases.iter().cloned());
        let mut help_argv = invoke.to_vec();
        help_argv.push("--help-ai".to_string());
        caps.push(Capability {
            path: cap_path,
            summary: node.summary.clone(),
            keywords,
            priority: 0,
            invoke: invoke.to_vec(),
            help: Some(HelpSpec {
                argv: help_argv,
                ai: true,
            }),
            requires: vec![],
            effects: None,
        });
    }

    /// Probe a single command (optionally scoped to a subcommand path) for its
    /// `--help-ai --json` document, returning the parsed [`HelpAiDoc`] for that
    /// node. This is the subprocess/parse primitive [`Self::probe_one`] builds
    /// on; it is also exposed so an embedding host (e.g. mu's MCP `mu/aiHelp`
    /// surface) can fetch the rich doc for one scoped node — `<command>
    /// <scope...> --help-ai --json` — without going through the flattened
    /// capability projection. `scope` is the subcommand argv between the command
    /// and `--help-ai` (empty for the root document).
    pub fn probe_help_ai(command: &str, scope: &[String]) -> Result<HelpAiDoc> {
        let out = Command::new(command)
            .args(scope)
            .arg("--help-ai")
            .arg("--json")
            .output()
            .with_context(|| format!("spawning {command} {scope:?} --help-ai --json"))?;
        if !out.status.success() {
            anyhow::bail!("{command} {scope:?} --help-ai --json exited {}", out.status);
        }
        serde_json::from_slice(&out.stdout)
            .with_context(|| format!("parsing {command} {scope:?} --help-ai --json output"))
    }

    fn probe_one(&self, cmd: &str) -> Result<Vec<Capability>> {
        let doc = Self::probe_help_ai(cmd, &[])?;
        Self::doc_to_caps(&self.source_class, cmd, doc)
    }
}

impl RegistrySource for HelpAiProbeSource {
    fn name(&self) -> &str {
        "help-ai-probe"
    }
    fn capabilities(&self) -> Result<Vec<Capability>> {
        let mut caps = Vec::new();
        for cmd in &self.commands {
            // Skip non-conforming / absent commands; don't fail the whole build.
            if let Ok(mut c) = self.probe_one(cmd) {
                caps.append(&mut c);
            }
        }
        Ok(caps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{HelpAiDoc, HelpAiSub};

    #[test]
    fn toml_parses_capabilities() {
        let text = r#"
            [[capability]]
            path = "bash.jj.status"
            summary = "working-copy status"
            keywords = ["vcs", "diff"]

            [[capability]]
            path = "bash.rg"
            summary = "ripgrep"
            invoke = ["rg"]
        "#;
        let caps = TomlConfigSource::parse_str(text).unwrap();
        assert_eq!(caps.len(), 2);
        let jj = &caps[0];
        assert_eq!(jj.path.to_string(), "bash.jj.status");
        // invoke defaults to path-minus-source when omitted
        assert_eq!(jj.invoke, vec!["jj".to_string(), "status".to_string()]);
        assert_eq!(caps[1].invoke, vec!["rg".to_string()]);
    }

    #[test]
    fn probe_doc_becomes_tool_plus_subcommands() {
        let doc = HelpAiDoc {
            name: "code-index".to_string(),
            summary: "semantic code search".to_string(),
            keywords: vec!["code".to_string()],
            subcommands: vec![
                HelpAiSub {
                    name: "recall".to_string(),
                    summary: "search".to_string(),
                    ..Default::default()
                },
                HelpAiSub {
                    name: "status".to_string(),
                    summary: "health".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let caps = HelpAiProbeSource::doc_to_caps("mcp", "code-index", doc).unwrap();
        assert_eq!(caps.len(), 3); // tool + 2 subs
        assert_eq!(caps[0].path.to_string(), "mcp.code-index");
        assert!(caps[0].help.as_ref().unwrap().ai);
        assert_eq!(caps[1].path.to_string(), "mcp.code-index.recall");
        assert_eq!(
            caps[1].invoke,
            vec!["code-index".to_string(), "recall".to_string()]
        );
    }

    #[test]
    fn recursion_registers_addressable_nodes_and_skips_beyond_max_depth() {
        // Default path cap is 5 (class.tool.sub.sub.sub). A `agent memory add`
        // chain (depth 4) registers; a node beyond the cap (depth 6) is skipped
        // — shown in the parent's help, not as a capability — without crashing
        // the probe. Registration is not gated on `invokable` (it's metadata for
        // the permission layer), so the `memory` group registers too.
        let doc = HelpAiDoc {
            name: "agent".to_string(),
            summary: "agent tool".to_string(),
            subcommands: vec![HelpAiSub {
                name: "memory".to_string(), // depth 3
                summary: "memory ops".to_string(),
                subcommands: vec![HelpAiSub {
                    name: "add".to_string(), // depth 4
                    summary: "add".to_string(),
                    subcommands: vec![HelpAiSub {
                        name: "fast".to_string(), // depth 5 (at cap)
                        summary: "fast".to_string(),
                        subcommands: vec![HelpAiSub {
                            name: "x".to_string(), // depth 6 (beyond cap)
                            summary: "x".to_string(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let caps = HelpAiProbeSource::doc_to_caps("bash", "agent", doc).unwrap();
        let paths: Vec<String> = caps.iter().map(|c| c.path.to_string()).collect();
        assert!(paths.contains(&"bash.agent".to_string()), "root (depth 2)");
        assert!(
            paths.contains(&"bash.agent.memory".to_string()),
            "group registers too (depth 3)"
        );
        assert!(
            paths.contains(&"bash.agent.memory.add".to_string()),
            "depth 4 is addressable under the default cap of 5"
        );
        assert!(
            paths.contains(&"bash.agent.memory.add.fast".to_string()),
            "depth 5 sits at the cap"
        );
        assert!(
            !paths.contains(&"bash.agent.memory.add.fast.x".to_string()),
            "depth 6 exceeds the cap — skipped, not registered (and the probe didn't crash)"
        );
        let add = caps
            .iter()
            .find(|c| c.path.to_string() == "bash.agent.memory.add")
            .unwrap();
        assert_eq!(
            add.invoke,
            vec!["agent".to_string(), "memory".to_string(), "add".to_string()]
        );
    }

    #[test]
    fn aliases_fold_into_keywords() {
        let doc = HelpAiDoc {
            name: "tool".to_string(),
            summary: "s".to_string(),
            subcommands: vec![HelpAiSub {
                name: "ls".to_string(),
                summary: "list".to_string(),
                keywords: vec!["enumerate".to_string()],
                aliases: vec!["dir".to_string()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let caps = HelpAiProbeSource::doc_to_caps("bash", "tool", doc).unwrap();
        let ls = caps
            .iter()
            .find(|c| c.path.to_string() == "bash.tool.ls")
            .unwrap();
        assert!(
            ls.keywords.contains(&"enumerate".to_string()),
            "own keyword kept"
        );
        assert!(
            ls.keywords.contains(&"dir".to_string()),
            "alias folded into keywords"
        );
    }
}
