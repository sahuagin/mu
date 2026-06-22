//! Preference chains — the config grammar for "which implementation of a
//! capability I already model."
//!
//! A **chain** is an ordered, best-first list of *interchangeable* implementations
//! of one capability slot (`rg`/`grep`, `eza`/`exa`/`ls`, `zstd`/`pixz`/`xz`/`gzip`).
//! `discover` (mu-d2iy.2) resolves a chain to its first *installed* impl and
//! tombstones the rest, so the live registry holds exactly one node per slot and
//! preference is baked in at resolve-time rather than carried as a runtime weight.
//!
//! A chain models true *synonyms*. A **neighborhood** of related-but-distinct
//! tools (`diff` vs `diff3` vs a diff-pager) is NOT a chain — those stay as
//! separate [`crate::capability::Capability`] entries and are disambiguated by the
//! semantic ranker, never folded together. The author's only per-group judgment
//! is "synonyms, or neighbors?"

use crate::capability::{Capability, HelpSpec};
use crate::path::CapPath;
use crate::source::RegistrySource;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// An ordered preference chain for one capability slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chain {
    /// The familiar path the resolved capability takes (e.g. `bash.ls`). You
    /// address it by the name you know; it runs the impl you prefer.
    pub slot: String,
    /// One-line, discovery-facing.
    #[serde(default)]
    pub summary: String,
    /// Extra match terms for ranking.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Ordered best-first: the interchangeable impls. First *installed* one wins
    /// the slot; the rest are tombstoned (mu-d2iy.2). Each impl carries its own
    /// mandatory safe-default flags (mu-kex4.6.7).
    pub impls: Vec<Impl>,
}

/// One interchangeable implementation of a chain slot: the command name plus the
/// mandatory safety/correctness flags that must always be applied when it runs.
///
/// Flags are **per-impl** because synonyms diverge: `fd` takes
/// `--one-file-system`, BSD `find` takes `-x`; `eza`/`exa` take `--color=never`
/// to keep ANSI escapes out of agent-parsed output — but BSD `ls` has no such
/// flag and would *error* on it (it defaults to no color), so it carries none.
/// Baking the *right* per-impl flag in is the whole point: a flag that's safe
/// for one synonym breaks another (tcovert + cold-agent session c9ecd980,
/// mu-kex4.6.7 — an `ls --color=always` alias once mangled a path capture).
///
/// Deserializes from either a bare command string (no flags) or a
/// `{ cmd, flags }` table, so existing `impls = ["rg", "grep"]` configs keep
/// parsing unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ImplRepr", into = "ImplRepr")]
pub struct Impl {
    /// The command name (also the installed-on-`$PATH` probe key).
    pub cmd: String,
    /// Mandatory flags prepended to the invocation — always applied so the
    /// agent never gets broken output (ANSI, cross-filesystem crawl, …).
    pub mandatory_flags: Vec<String>,
}

impl Impl {
    /// An impl with no mandatory flags.
    pub fn bare(cmd: impl Into<String>) -> Self {
        Self {
            cmd: cmd.into(),
            mandatory_flags: Vec::new(),
        }
    }

    /// An impl carrying mandatory safe-default flags.
    pub fn with_flags(cmd: impl Into<String>, flags: &[&str]) -> Self {
        Self {
            cmd: cmd.into(),
            mandatory_flags: flags.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// Serde wire form for [`Impl`]: a bare string (no flags) or a `{ cmd, flags }`
/// table. Keeps `impls = ["rg", "grep"]` valid while allowing
/// `impls = [{ cmd = "fd", flags = ["--one-file-system"] }]`.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum ImplRepr {
    Bare(String),
    Full {
        cmd: String,
        #[serde(default)]
        flags: Vec<String>,
    },
}

impl From<ImplRepr> for Impl {
    fn from(r: ImplRepr) -> Self {
        match r {
            ImplRepr::Bare(cmd) => Impl {
                cmd,
                mandatory_flags: Vec::new(),
            },
            ImplRepr::Full { cmd, flags } => Impl {
                cmd,
                mandatory_flags: flags,
            },
        }
    }
}

impl From<Impl> for ImplRepr {
    fn from(i: Impl) -> Self {
        // Serialize back to the compact bare form when there are no flags.
        if i.mandatory_flags.is_empty() {
            ImplRepr::Bare(i.cmd)
        } else {
            ImplRepr::Full {
                cmd: i.cmd,
                flags: i.mandatory_flags,
            }
        }
    }
}

/// Parse the `[[chain]]` array from a t4c config TOML. Unknown sections (e.g.
/// `[[capability]]`) are ignored, so chains and neighborhood entries can share
/// one config file.
pub fn parse_chains(text: &str) -> Result<Vec<Chain>> {
    #[derive(Deserialize)]
    struct ChainFile {
        #[serde(default)]
        chain: Vec<Chain>,
    }
    let file: ChainFile = toml::from_str(text).context("parsing [[chain]] config")?;
    Ok(file.chain)
}

/// State of a chain impl after resolution against the environment — the
/// 3-state tombstone model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImplState {
    /// Installed and preferred — this impl wins the slot.
    Active,
    /// Installed, but a more-preferred impl already won the slot.
    Superseded { behind: String },
    /// Not installed.
    Absent,
}

/// One chain impl after resolution: its slot, command, and state. Active impls
/// become [`Capability`] nodes; Superseded/Absent are tombstones that `list`
/// surfaces but `find` never ranks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub slot: String,
    pub impl_cmd: String,
    pub state: ImplState,
}

impl Chain {
    /// Resolve against an availability predicate. The first installed impl is
    /// [`ImplState::Active`] and becomes the slot's [`Capability`] (path = the
    /// familiar slot, invoke = the preferred impl); later installed impls are
    /// [`ImplState::Superseded`]; uninstalled impls are [`ImplState::Absent`].
    pub fn resolve<F: Fn(&str) -> bool>(
        &self,
        installed: F,
    ) -> Result<(Option<Capability>, Vec<Resolved>)> {
        let mut states = Vec::with_capacity(self.impls.len());
        let mut winner: Option<String> = None;
        let mut winner_flags: Vec<String> = Vec::new();
        for imp in &self.impls {
            let state = if !installed(&imp.cmd) {
                ImplState::Absent
            } else if let Some(w) = &winner {
                ImplState::Superseded { behind: w.clone() }
            } else {
                winner = Some(imp.cmd.clone());
                winner_flags = imp.mandatory_flags.clone();
                ImplState::Active
            };
            states.push(Resolved {
                slot: self.slot.clone(),
                impl_cmd: imp.cmd.clone(),
                state,
            });
        }
        let cap = match &winner {
            Some(cmd) => {
                // invoke = the winning impl + its mandatory safe-default flags,
                // so `t4c run <slot>` is always safe. help/probe use the bare cmd.
                let mut invoke = vec![cmd.clone()];
                invoke.extend(winner_flags.iter().cloned());
                Some(Capability {
                    path: CapPath::parse(&self.slot).with_context(|| {
                        format!("chain slot {:?} is not a valid path", self.slot)
                    })?,
                    summary: self.summary.clone(),
                    keywords: self.keywords.clone(),
                    priority: 0,
                    invoke,
                    help: Some(HelpSpec {
                        argv: vec![cmd.clone(), "--help".to_string()],
                        ai: false,
                    }),
                    requires: vec![],
                    effects: None,
                })
            }
            None => None,
        };
        Ok((cap, states))
    }
}

/// Resolve a set of chains: returns the active winner capabilities and the full
/// per-impl resolution (for `list`'s 3-state view).
pub fn resolve_chains<F: Fn(&str) -> bool + Copy>(
    chains: &[Chain],
    installed: F,
) -> Result<(Vec<Capability>, Vec<Resolved>)> {
    let mut caps = Vec::new();
    let mut all = Vec::new();
    for chain in chains {
        let (cap, states) = chain.resolve(installed)?;
        if let Some(c) = cap {
            caps.push(c);
        }
        all.extend(states);
    }
    Ok((caps, all))
}

/// A [`RegistrySource`] backed by preference chains: yields the active winner of
/// each chain (resolved against `$PATH`). Superseded/absent impls are NOT emitted
/// into the tree — they are tombstones, surfaced only by `list`.
pub struct ChainSource {
    chains: Vec<Chain>,
}

impl ChainSource {
    pub fn new(chains: Vec<Chain>) -> Self {
        Self { chains }
    }
}

impl RegistrySource for ChainSource {
    fn name(&self) -> &str {
        "chains"
    }
    fn capabilities(&self) -> Result<Vec<Capability>> {
        let (caps, _tombstones) = resolve_chains(&self.chains, crate::catalog::which)?;
        Ok(caps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(slot: &str, impls: &[&str]) -> Chain {
        Chain {
            slot: slot.to_string(),
            summary: String::new(),
            keywords: vec![],
            impls: impls.iter().map(|s| Impl::bare(*s)).collect(),
        }
    }

    #[test]
    fn resolve_first_installed_wins_rest_superseded() {
        let (cap, states) = chain("bash.search", &["rg", "grep"])
            .resolve(|_| true)
            .unwrap();
        let cap = cap.unwrap();
        assert_eq!(cap.path.to_string(), "bash.search");
        assert_eq!(cap.invoke, vec!["rg".to_string()]); // preferred impl, addressed by familiar slot
        assert_eq!(states[0].state, ImplState::Active);
        assert_eq!(
            states[1].state,
            ImplState::Superseded {
                behind: "rg".to_string()
            }
        );
    }

    #[test]
    fn resolve_skips_absent_to_next_installed() {
        let (cap, states) = chain("bash.ls", &["eza", "exa", "ls"])
            .resolve(|cmd| cmd == "ls")
            .unwrap();
        assert_eq!(cap.unwrap().invoke, vec!["ls".to_string()]);
        assert_eq!(states[0].state, ImplState::Absent);
        assert_eq!(states[1].state, ImplState::Absent);
        assert_eq!(states[2].state, ImplState::Active);
    }

    #[test]
    fn resolve_none_installed_no_winner() {
        let (cap, states) = chain("bash.compress", &["zstd", "xz"])
            .resolve(|_| false)
            .unwrap();
        assert!(cap.is_none());
        assert!(states.iter().all(|r| r.state == ImplState::Absent));
    }

    #[test]
    fn resolve_chains_collects_winners_and_tombstones() {
        let chains = vec![
            chain("bash.search", &["rg", "grep"]),
            chain("bash.ls", &["eza", "ls"]),
        ];
        let (caps, all) = resolve_chains(&chains, |cmd| cmd != "eza").unwrap();
        assert_eq!(caps.len(), 2);
        assert_eq!(all.len(), 4);
        let superseded = all
            .iter()
            .filter(|r| matches!(r.state, ImplState::Superseded { .. }))
            .count();
        let absent = all.iter().filter(|r| r.state == ImplState::Absent).count();
        assert_eq!(superseded, 1); // grep behind rg
        assert_eq!(absent, 1); // eza absent
    }

    #[test]
    fn parses_chains_with_defaults() {
        let text = r#"
            [[chain]]
            slot = "bash.search"
            summary = "search file contents for a pattern"
            keywords = ["search", "grep", "regex"]
            impls = ["rg", "grep"]

            [[chain]]
            slot = "bash.ls"
            impls = ["eza", "exa", "ls"]
        "#;
        let chains = parse_chains(text).unwrap();
        assert_eq!(chains.len(), 2);
        assert_eq!(chains[0].slot, "bash.search");
        // bare-string impls parse as flagless Impls (backward compatibility)
        assert_eq!(chains[0].impls, vec![Impl::bare("rg"), Impl::bare("grep")]);
        // summary/keywords default when omitted
        assert_eq!(chains[1].slot, "bash.ls");
        assert!(chains[1].summary.is_empty());
        assert_eq!(
            chains[1].impls,
            vec![Impl::bare("eza"), Impl::bare("exa"), Impl::bare("ls")]
        );
    }

    #[test]
    fn parses_impls_with_mandatory_flags_and_bare_mixed() {
        // the `{ cmd, flags }` table form coexists with the bare-string form
        let text = r#"
            [[chain]]
            slot = "bash.find-files"
            impls = [{ cmd = "fd", flags = ["--one-file-system"] }, "find"]
        "#;
        let chains = parse_chains(text).unwrap();
        assert_eq!(chains.len(), 1);
        assert_eq!(
            chains[0].impls,
            vec![
                Impl::with_flags("fd", &["--one-file-system"]),
                Impl::bare("find")
            ]
        );
    }

    #[test]
    fn resolve_bakes_winning_impls_mandatory_flags_into_invoke() {
        // fd wins and its --one-file-system flag is baked into invoke so
        // `t4c run` applies it automatically; help still uses the bare cmd.
        let chain = Chain {
            slot: "bash.find-files".to_string(),
            summary: String::new(),
            keywords: vec![],
            impls: vec![
                Impl::with_flags("fd", &["--one-file-system"]),
                Impl::with_flags("find", &["-x"]),
            ],
        };
        let (cap, _) = chain.resolve(|cmd| cmd == "fd").unwrap();
        let cap = cap.unwrap();
        assert_eq!(
            cap.invoke,
            vec!["fd".to_string(), "--one-file-system".to_string()]
        );
        // help/probe key is the bare command, not the flagged invoke
        assert_eq!(cap.help.unwrap().argv[0], "fd");
    }

    #[test]
    fn impl_round_trips_through_toml_compactly() {
        // a flagless impl serializes back to a bare string; a flagged one to a table
        let chains = vec![Chain {
            slot: "bash.ls".to_string(),
            summary: String::new(),
            keywords: vec![],
            impls: vec![
                Impl::with_flags("eza", &["--color=never"]),
                Impl::bare("ls"),
            ],
        }];
        #[derive(serde::Serialize)]
        struct Wrap {
            chain: Vec<Chain>,
        }
        let text = toml::to_string(&Wrap { chain: chains }).unwrap();
        let back = parse_chains(&text).unwrap();
        assert_eq!(
            back[0].impls,
            vec![
                Impl::with_flags("eza", &["--color=never"]),
                Impl::bare("ls")
            ]
        );
    }

    #[test]
    fn ignores_non_chain_sections_and_empty() {
        assert!(parse_chains("").unwrap().is_empty());
        // a config with only neighborhood [[capability]] entries yields no chains
        let mixed = "[[capability]]\npath = \"bash.diff3\"\nsummary = \"three-way merge\"\n";
        assert!(parse_chains(mixed).unwrap().is_empty());
    }

    #[test]
    fn compression_chain_order_preserved() {
        let text = r#"
            [[chain]]
            slot = "bash.compress"
            keywords = ["compress", "archive"]
            impls = ["zstd", "pixz", "xz", "gzip"]
        "#;
        let c = parse_chains(text).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].impls.first().unwrap().cmd, "zstd");
        assert_eq!(c[0].impls.len(), 4);
    }
}
