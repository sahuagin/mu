//! The aggregate: merge sources into one path-addressed tree, then look up by
//! exact path or walk a subtree. The tree is the canonical data model; the CLI
//! and the future mu-native binding are projections over it (mu-kex4.3 / .6).

use crate::capability::Capability;
use crate::path::CapPath;
use crate::source::RegistrySource;
use anyhow::Result;
use std::collections::BTreeMap;

/// Aggregates capability sources. Add sources, then [`build`](Self::build) a
/// [`Tree`].
#[derive(Default)]
pub struct Registry {
    sources: Vec<Box<dyn RegistrySource>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a source. Later sources win on path collision, so a higher-authority
    /// source (e.g. mu's live manifest) can shadow earlier config/probe entries.
    pub fn add_source(&mut self, source: Box<dyn RegistrySource>) -> &mut Self {
        self.sources.push(source);
        self
    }

    /// Collect every source's capabilities into one tree, recording each path's
    /// provenance (the [`RegistrySource::name`] that produced the winning entry).
    /// Because later sources win on collision, the recorded provenance is always
    /// the *authoritative* one — "live MCP says loaded" overwrites "curated
    /// catalog" overwrites a stale probe (mu-kex4.6.8).
    pub fn build(&self) -> Result<Tree> {
        let mut nodes: BTreeMap<String, Capability> = BTreeMap::new();
        let mut provenance: BTreeMap<String, String> = BTreeMap::new();
        for source in &self.sources {
            for cap in source.capabilities()? {
                let path = cap.path.to_string();
                provenance.insert(path.clone(), source.name().to_string());
                nodes.insert(path, cap);
            }
        }
        Ok(Tree { nodes, provenance })
    }
}

/// A built, queryable capability tree. Keyed by dotted path; the `BTreeMap`
/// keeps iteration ordered so walks and listings are stable.
pub struct Tree {
    nodes: BTreeMap<String, Capability>,
    /// path -> the source that produced the winning entry (provenance).
    provenance: BTreeMap<String, String>,
}

impl Tree {
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Exact-path lookup.
    pub fn get(&self, path: &CapPath) -> Option<&Capability> {
        self.nodes.get(&path.to_string())
    }

    /// Provenance of a path: the [`RegistrySource::name`] that produced the
    /// winning entry. `None` if the path isn't in the tree. Lets a consumer
    /// tell "live MCP says loaded" from "curated catalog says installed"
    /// (mu-kex4.6.8).
    pub fn source_of(&self, path: &CapPath) -> Option<&str> {
        self.provenance.get(&path.to_string()).map(String::as_str)
    }

    /// Every capability, in stable path order.
    pub fn all(&self) -> impl Iterator<Item = &Capability> {
        self.nodes.values()
    }

    /// Walk a subtree: every capability at or under `prefix` (the GETNEXT-style
    /// structural-discovery primitive behind a bare-prefix `t4c <prefix>`).
    pub fn walk(&self, prefix: &CapPath) -> Vec<&Capability> {
        self.nodes
            .values()
            .filter(|c| c.path.starts_with(prefix))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::StaticSource;

    fn cap(path: &str, summary: &str) -> Capability {
        let p = CapPath::parse(path).unwrap();
        Capability {
            invoke: p.invoke_argv(),
            path: p,
            summary: summary.to_string(),
            keywords: vec![],
            priority: 0,
            help: None,
            requires: vec![],
            effects: None,
        }
    }

    fn tree() -> Tree {
        let mut reg = Registry::new();
        reg.add_source(Box::new(StaticSource::new(
            "a",
            vec![
                cap("bash.jj.status", "status"),
                cap("bash.jj.diff", "diff"),
                cap("mcp.code-index.recall", "recall"),
            ],
        )));
        reg.build().unwrap()
    }

    #[test]
    fn exact_lookup_and_size() {
        let t = tree();
        assert_eq!(t.len(), 3);
        let p = CapPath::parse("bash.jj.status").unwrap();
        assert_eq!(t.get(&p).unwrap().summary, "status");
        assert!(t.get(&CapPath::parse("bash.nope").unwrap()).is_none());
    }

    #[test]
    fn prefix_walk_returns_subtree() {
        let t = tree();
        let jj = t.walk(&CapPath::parse("bash.jj").unwrap());
        assert_eq!(jj.len(), 2);
        let bash = t.walk(&CapPath::parse("bash").unwrap());
        assert_eq!(bash.len(), 2); // bash.jj.status + bash.jj.diff
        let mcp = t.walk(&CapPath::parse("mcp").unwrap());
        assert_eq!(mcp.len(), 1);
    }

    #[test]
    fn later_source_overrides_on_collision() {
        let mut reg = Registry::new();
        reg.add_source(Box::new(StaticSource::new(
            "a",
            vec![cap("bash.x", "from a")],
        )));
        reg.add_source(Box::new(StaticSource::new(
            "b",
            vec![cap("bash.x", "from b")],
        )));
        let t = reg.build().unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(
            t.get(&CapPath::parse("bash.x").unwrap()).unwrap().summary,
            "from b"
        );
    }

    #[test]
    fn provenance_records_the_winning_source() {
        let mut reg = Registry::new();
        reg.add_source(Box::new(StaticSource::new(
            "curated",
            vec![cap("bash.x", "from curated"), cap("bash.y", "only curated")],
        )));
        reg.add_source(Box::new(StaticSource::new(
            "mcp-live",
            vec![cap("bash.x", "from live")],
        )));
        let t = reg.build().unwrap();
        // bash.x collided: the later (authoritative) source wins both the entry
        // AND the recorded provenance.
        assert_eq!(
            t.source_of(&CapPath::parse("bash.x").unwrap()),
            Some("mcp-live")
        );
        // bash.y only came from curated.
        assert_eq!(
            t.source_of(&CapPath::parse("bash.y").unwrap()),
            Some("curated")
        );
        // absent path has no provenance.
        assert_eq!(t.source_of(&CapPath::parse("bash.nope").unwrap()), None);
    }
}
