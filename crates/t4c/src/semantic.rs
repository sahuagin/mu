//! Semantic ranking: cosine of the intent embedding against cached catalog
//! vectors. The catalog is tiny, so the split is compile-once / query-cheap —
//! [`VectorCache`] is built + persisted at `discover`, and `find` loads it and
//! embeds only the intent (one call), then brute-force cosines. No vector DB.
//!
//! [`SemanticRanker`] degrades to [`LexicalRanker`] whenever the live intent
//! embed fails or nothing is cached — so an offline / endpoint-down run still
//! ranks, just lexically. The real semantic signal is validated at the mu-d2iy.6
//! gate.

use crate::capability::Capability;
use crate::embedder::{cosine, Embedder};
use crate::rank::{LexicalRanker, Ranked, Ranker};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// The text embedded for a capability: path + summary + keywords.
pub fn cap_text(c: &Capability) -> String {
    format!("{} {} {}", c.path, c.summary, c.keywords.join(" "))
}

/// A persisted catalog-vector cache (path -> embedding). Written at `discover`,
/// loaded at `find`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorCache {
    pub model: String,
    pub by_path: HashMap<String, Vec<f32>>,
}

impl VectorCache {
    /// Embed every capability's text once and key the vectors by path.
    pub fn build<E: Embedder>(embedder: &E, model: &str, caps: &[&Capability]) -> Result<Self> {
        let texts: Vec<String> = caps.iter().map(|c| cap_text(c)).collect();
        let vecs = embedder.embed(&texts)?;
        let by_path = caps.iter().map(|c| c.path.to_string()).zip(vecs).collect();
        Ok(Self {
            model: model.to_string(),
            by_path,
        })
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(path, serde_json::to_string(self)?)
            .with_context(|| format!("writing vector cache {}", path.display()))
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading vector cache {}", path.display()))?;
        serde_json::from_str(&text).context("parsing vector cache")
    }
}

/// Ranks by cosine of the intent embedding against cached catalog vectors.
/// Degrades to lexical when the intent embed fails or nothing is cached.
pub struct SemanticRanker<E: Embedder> {
    embedder: E,
    cache: HashMap<String, Vec<f32>>,
    fallback: LexicalRanker,
}

impl<E: Embedder> SemanticRanker<E> {
    pub fn new(embedder: E, cache: HashMap<String, Vec<f32>>) -> Self {
        Self {
            embedder,
            cache,
            fallback: LexicalRanker,
        }
    }
}

impl<E: Embedder> Ranker for SemanticRanker<E> {
    fn rank<'a>(&self, intent: &str, caps: &[&'a Capability]) -> Vec<Ranked<'a>> {
        let qvec = match self.embedder.embed(&[intent.to_string()]) {
            Ok(mut v) if !v.is_empty() => v.remove(0),
            _ => return self.fallback.rank(intent, caps), // embed failed -> lexical
        };
        // A partial cache is stale (catalog changed since `t4c discover`). Do
        // NOT assign missing capabilities score 0.0: that silently hides new
        // entries such as bash.gh.pr behind old cached neighbors. Fall back to
        // lexical until discover rebuilds the vector cache.
        if !caps
            .iter()
            .all(|c| self.cache.contains_key(&c.path.to_string()))
        {
            return self.fallback.rank(intent, caps);
        }
        let mut out: Vec<Ranked> = caps
            .iter()
            .map(|cap| {
                let score = self
                    .cache
                    .get(&cap.path.to_string())
                    .map(|cv| cosine(&qvec, cv) as f64)
                    .unwrap_or(0.0);
                Ranked { cap, score }
            })
            .collect();
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.cap.path.to_string().cmp(&b.cap.path.to_string()))
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::FakeEmbedder;
    use crate::path::CapPath;

    fn cap(path: &str, summary: &str, kw: &[&str]) -> Capability {
        Capability {
            path: CapPath::parse(path).unwrap(),
            summary: summary.to_string(),
            keywords: kw.iter().map(|s| s.to_string()).collect(),
            invoke: vec![],
            help: None,
            requires: vec![],
            effects: None,
        }
    }

    #[test]
    fn cache_round_trips() {
        let ci = cap("mcp.code-index.recall", "semantic code search", &["symbol"]);
        let jq = cap("bash.jq", "query json", &["filter"]);
        let caps = vec![&ci, &jq];
        let cache = VectorCache::build(&FakeEmbedder::new(), "fake", &caps).unwrap();
        let path = std::env::temp_dir().join("t4c_vectors_test.json");
        cache.save(&path).unwrap();
        let back = VectorCache::load(&path).unwrap();
        assert_eq!(back.by_path.len(), 2);
        assert!(back.by_path.contains_key("mcp.code-index.recall"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ranks_matching_cap_first() {
        let ci = cap(
            "mcp.code-index.recall",
            "semantic code search symbols",
            &["code"],
        );
        let jq = cap("bash.jq", "query and transform json data", &["json"]);
        let caps = vec![&ci, &jq];
        let emb = FakeEmbedder::new();
        let cache = VectorCache::build(&emb, "fake", &caps).unwrap();
        let ranker = SemanticRanker::new(emb, cache.by_path);
        let ranked = ranker.rank("search code for a symbol", &caps);
        assert_eq!(ranked[0].cap.path.to_string(), "mcp.code-index.recall");
        assert!(ranked[0].score > ranked[1].score);
    }

    #[test]
    fn empty_cache_falls_back_to_lexical() {
        let rg = cap(
            "bash.search",
            "search files for a regex",
            &["grep", "regex"],
        );
        let caps = vec![&rg];
        // empty cache -> missing cap vector -> lexical fallback still ranks
        let ranker = SemanticRanker::new(FakeEmbedder::new(), HashMap::new());
        let ranked = ranker.rank("regex search", &caps);
        assert_eq!(ranked.len(), 1);
        assert!(ranked[0].score > 0.0); // lexical found the overlap
    }

    #[test]
    fn partial_cache_falls_back_to_lexical_so_new_caps_are_not_hidden() {
        let gh = cap(
            "bash.gh.pr",
            "GitHub CLI for PRs/issues. In jj sibling workspaces use -R owner/repo",
            &["github", "gh", "pr", "pull-request", "jj", "workspace"],
        );
        let jj = cap("bash.jj.status", "jujutsu status", &["jj"]);
        let caps = vec![&gh, &jj];
        // Stale cache has only the older jj entry. Semantic scoring would give gh
        // 0.0 and hide it; correct behavior is lexical fallback over all caps.
        let emb = FakeEmbedder::new();
        let cache = VectorCache::build(&emb, "fake", &[&jj]).unwrap();
        let ranker = SemanticRanker::new(emb, cache.by_path);
        let ranked = ranker.rank("create github pr in jj workspace", &caps);
        assert_eq!(ranked[0].cap.path.to_string(), "bash.gh.pr");
        assert!(ranked[0].score > ranked[1].score);
    }
}
