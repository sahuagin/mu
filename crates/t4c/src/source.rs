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
        let mut caps = Vec::with_capacity(doc.subcommands.len() + 1);
        let tool_path = CapPath::parse(&format!("{source_class}.{}", doc.name))?;
        caps.push(Capability {
            path: tool_path,
            summary: doc.summary,
            keywords: doc.keywords,
            invoke: vec![cmd.to_string()],
            help: Some(HelpSpec {
                argv: vec![cmd.to_string(), "--help-ai".to_string()],
                ai: true,
            }),
            requires: vec![],
            effects: None,
        });
        for sub in doc.subcommands {
            let path = CapPath::parse(&format!("{source_class}.{}.{}", doc.name, sub.name))?;
            caps.push(Capability {
                path,
                summary: sub.summary,
                keywords: vec![],
                invoke: vec![cmd.to_string(), sub.name.clone()],
                help: Some(HelpSpec {
                    argv: vec![cmd.to_string(), sub.name, "--help-ai".to_string()],
                    ai: true,
                }),
                requires: vec![],
                effects: None,
            });
        }
        Ok(caps)
    }

    fn probe_one(&self, cmd: &str) -> Result<Vec<Capability>> {
        let out = Command::new(cmd)
            .arg("--help-ai")
            .arg("--json")
            .output()
            .with_context(|| format!("spawning {cmd} --help-ai --json"))?;
        if !out.status.success() {
            anyhow::bail!("{cmd} --help-ai --json exited {}", out.status);
        }
        let doc: HelpAiDoc = serde_json::from_slice(&out.stdout)
            .with_context(|| format!("parsing {cmd} --help-ai --json output"))?;
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
                },
                HelpAiSub {
                    name: "status".to_string(),
                    summary: "health".to_string(),
                },
            ],
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
}
