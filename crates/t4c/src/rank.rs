//! Ranking for the `find` front door.
//!
//! [`Ranker`] is a trait so mu can swap in semantic ranking (it owns recall
//! infrastructure). The default, [`LexicalRanker`], is keyword overlap with a
//! stopword filter — a deliberate floor, not the destination. Semantic ranking
//! is mu-side and explicitly NOT a blocker for this crate.

use crate::capability::Capability;

/// Intent-framing words dropped before scoring, so "where is the jj status"
/// ranks on "jj"/"status", not on "where"/"is"/"the". (Ported from the t4c
/// prototype, which found that framing words otherwise dominate the score.)
const STOPWORDS: &[&str] = &[
    "find", "where", "search", "look", "locate", "show", "get", "a", "an", "the", "is", "are",
    "was", "in", "on", "of", "for", "to", "this", "that", "how", "do", "i", "me", "my", "want",
    "need", "with", "it", "and",
];

/// A capability with its relevance score for some intent.
#[derive(Debug, Clone)]
pub struct Ranked<'a> {
    pub cap: &'a Capability,
    pub score: f64,
}

/// Rank capabilities against a free-text intent, best-first.
pub trait Ranker {
    fn rank<'a>(&self, intent: &str, caps: &[&'a Capability]) -> Vec<Ranked<'a>>;
}

/// Keyword-overlap ranker — the lexical floor.
#[derive(Debug, Default, Clone)]
pub struct LexicalRanker;

impl LexicalRanker {
    /// Lowercased content words of the intent, framing-words removed.
    fn terms(intent: &str) -> Vec<String> {
        intent
            .split_whitespace()
            .map(normalize)
            .filter(|t| !t.is_empty() && !STOPWORDS.contains(&t.as_str()))
            .collect()
    }

    /// Lowercased match terms for a capability: its path segments, summary
    /// words, and explicit keywords.
    fn haystack(cap: &Capability) -> Vec<String> {
        let mut words: Vec<String> = cap
            .path
            .segments()
            .iter()
            .map(|s| s.to_lowercase())
            .collect();
        words.extend(cap.summary.split_whitespace().map(normalize));
        words.extend(cap.keywords.iter().map(|k| k.to_lowercase()));
        words.retain(|w| !w.is_empty());
        words
    }
}

impl Ranker for LexicalRanker {
    fn rank<'a>(&self, intent: &str, caps: &[&'a Capability]) -> Vec<Ranked<'a>> {
        let terms = Self::terms(intent);
        let mut out: Vec<Ranked<'a>> = caps
            .iter()
            .map(|cap| {
                let hay = Self::haystack(cap);
                let score = terms.iter().filter(|t| hay.contains(*t)).count() as f64;
                Ranked { cap, score }
            })
            .collect();
        // Best score first; ties broken by path for stable output.
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.cap.path.to_string().cmp(&b.cap.path.to_string()))
        });
        out
    }
}

/// Lowercase and strip surrounding non-alphanumerics from a token.
fn normalize(tok: &str) -> String {
    tok.trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::CapPath;

    fn cap(path: &str, summary: &str, keywords: &[&str]) -> Capability {
        Capability {
            path: CapPath::parse(path).unwrap(),
            summary: summary.to_string(),
            keywords: keywords.iter().map(|k| k.to_string()).collect(),
            invoke: vec![],
            help: None,
            requires: vec![],
        }
    }

    #[test]
    fn ranks_obvious_match_first_and_ignores_framing_words() {
        let recall = cap("mcp.code-index.recall", "semantic code search", &["symbol"]);
        let rg = cap("bash.rg", "ripgrep literal regex search", &["grep"]);
        let caps = vec![&recall, &rg];
        let ranked = LexicalRanker.rank("where is the code symbol defined", &caps);
        // "where"/"is"/"the" are stopwords; "code" (summary) + "symbol" (keyword)
        // hit code-index, nothing hits rg.
        assert_eq!(ranked[0].cap.path.to_string(), "mcp.code-index.recall");
        assert!(ranked[0].score >= 2.0);
        assert_eq!(ranked[1].score, 0.0);
    }

    #[test]
    fn all_stopwords_yields_zero_score() {
        let rg = cap("bash.rg", "ripgrep", &[]);
        let caps = vec![&rg];
        let ranked = LexicalRanker.rank("the is a how do i", &caps);
        assert_eq!(ranked[0].score, 0.0);
    }
}
