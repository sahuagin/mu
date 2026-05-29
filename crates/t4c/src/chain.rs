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

#[cfg(test)]
mod tests {
    use super::*;

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
