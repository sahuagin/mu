//! t4c — tools4claude.
//!
//! A discovery / capability surface built for an *agent* consumer: find tools by
//! intent, learn how to call them, and invoke them. The design premise is that a
//! fresh agent boots into the dark — it knows its intent, not the toolset — so
//! discovery must be the front door, not a wall of context dumped at startup.
//!
//! # The one data model: a shallow path-addressed tree
//!
//! Capabilities form a shallow tree addressed as `source.tool.subcommand`
//! (~3 levels; everything past that is free-form arguments, not tree depth —
//! deep hierarchies just recreate the "guess the structure" problem for a model).
//! The tree is the canonical model; the surfaces over it are *projections*:
//!
//! - a **CLI** dotted-path surface (`t4c <path> [args]`, `t4c find <intent>`), and
//! - a future **mu-native** in-process binding (rpc into the same tree).
//!
//! Both read the same [`registry`]-as-trait, so adding a transport never forks
//! the model. (Surface + projections land in mu-kex4.2 / mu-kex4.3.)
//!
//! # Leaf crate
//!
//! This crate depends only on published crates, never on its workspace siblings,
//! so it builds and publishes standalone while mu links it in-process. See the
//! invariant comment in `Cargo.toml`.

pub mod bench;
pub mod capability;
pub mod catalog;
pub mod chain;
pub mod cli;
pub mod embedder;
pub mod helpai;
pub mod path;
pub mod rank;
pub mod registry;
pub mod semantic;
pub mod source;

pub use capability::{Capability, HelpAiDoc, HelpSpec};
pub use catalog::EnvCatalogSource;
pub use chain::Chain;
pub use embedder::{cosine, ConfigEmbedder, Embedder, FakeEmbedder};
pub use path::CapPath;
pub use rank::{LexicalRanker, Ranked, Ranker};
pub use registry::{Registry, Tree};
pub use semantic::{SemanticRanker, VectorCache};
pub use source::{HelpAiProbeSource, RegistrySource, StaticSource, TomlConfigSource};

/// Crate version, surfaced by the CLI (`t4c --version`) and any embedding host.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_nonempty() {
        // Sanity check that the scaffold links and the package metadata is wired.
        assert!(!version().is_empty());
    }
}
