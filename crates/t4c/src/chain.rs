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
    /// Ordered best-first: the command names of the interchangeable impls.
    /// First *installed* one wins the slot; the rest are tombstoned (mu-d2iy.2).
    pub impls: Vec<String>,
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
        for cmd in &self.impls {
            let state = if !installed(cmd) {
                ImplState::Absent
            } else if let Some(w) = &winner {
                ImplState::Superseded { behind: w.clone() }
            } else {
                winner = Some(cmd.clone());
                ImplState::Active
            };
            states.push(Resolved {
                slot: self.slot.clone(),
                impl_cmd: cmd.clone(),
                state,
            });
        }
        let cap = match &winner {
            Some(cmd) => Some(Capability {
                path: CapPath::parse(&self.slot)
                    .with_context(|| format!("chain slot {:?} is not a valid path", self.slot))?,
                summary: self.summary.clone(),
                keywords: self.keywords.clone(),
                invoke: vec![cmd.clone()],
                help: Some(HelpSpec {
                    argv: vec![cmd.clone(), "--help".to_string()],
                    ai: false,
                }),
                requires: vec![],
            }),
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
        let (caps, _tombstones) = resolve_chains(&self.chains, |cmd| crate::catalog::which(cmd))?;
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
            impls: impls.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn resolve_first_installed_wins_rest_superseded() {
        let (cap, states) = chain("bash.search", &["rg", "grep"]).resolve(|_| true).unwrap();
        let cap = cap.unwrap();
        assert_eq!(cap.path.to_string(), "bash.search");
        assert_eq!(cap.invoke, vec!["rg".to_string()]); // preferred impl, addressed by familiar slot
        assert_eq!(states[0].state, ImplState::Active);
        assert_eq!(states[1].state, ImplState::Superseded { behind: "rg".to_string() });
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
        let (cap, states) = chain("bash.compress", &["zstd", "xz"]).resolve(|_| false).unwrap();
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
        assert_eq!(chains[0].impls, vec!["rg".to_string(), "grep".to_string()]);
        // summary/keywords default when omitted
        assert_eq!(chains[1].slot, "bash.ls");
        assert!(chains[1].summary.is_empty());
        assert_eq!(
            chains[1].impls,
            vec!["eza".to_string(), "exa".to_string(), "ls".to_string()]
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
        assert_eq!(c[0].impls.first().unwrap(), "zstd");
        assert_eq!(c[0].impls.len(), 4);
    }
}
