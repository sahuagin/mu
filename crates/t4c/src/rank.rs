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

/// The shared final ordering every [`Ranker`] applies: relevance `score` first
/// (best first), then [`Capability::priority`] (higher first) as a deterministic
/// tie-break, then path for stable output. Priority is a tie-break *over* the
/// relevance signal, not a weight blended *into* it — the rankers' score scales
/// differ (lexical = integer term overlap, semantic = cosine), so a blended
/// weight would be scale-dependent and could surface an irrelevant high-priority
/// capability. As a tie-break it only decides among comparably-scored matches,
/// which (because lexical scores are small integers) is the common case.
pub(crate) fn sort_ranked(out: &mut [Ranked]) {
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.cap.priority.cmp(&a.cap.priority))
            .then_with(|| a.cap.path.to_string().cmp(&b.cap.path.to_string()))
    });
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
        sort_ranked(&mut out);
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
            priority: 0,
            invoke: vec![],
            help: None,
            requires: vec![],
            effects: None,
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

    #[test]
    fn priority_breaks_score_ties_over_path() {
        // Both summaries contain the one query term "workspace" → equal score.
        // Path order alone (bash.git.* sorts before bash.sprint-start) would
        // float git first; priority overrides so the ordained tool wins among
        // comparable matches.
        let mut sprint = cap(
            "bash.sprint-start",
            "claim a bead and enter a jj workspace",
            &[],
        );
        sprint.priority = 10;
        let git = cap(
            "bash.git.worktree",
            "git worktree rooted in a workspace",
            &[],
        );
        let caps = vec![&git, &sprint];
        let ranked = LexicalRanker.rank("workspace", &caps);
        assert_eq!(
            ranked[0].score, ranked[1].score,
            "scores must tie for this test"
        );
        assert_eq!(ranked[0].cap.path.to_string(), "bash.sprint-start");
    }

    #[test]
    fn priority_never_overrides_a_better_score() {
        // A high priority must NOT lift an irrelevant capability above a
        // genuinely-relevant one — priority is a tie-break, not a weight.
        let mut noise = cap("bash.noise", "totally unrelated", &[]);
        noise.priority = 100;
        let hit = cap("bash.rg", "ripgrep regex search", &["grep"]);
        let caps = vec![&noise, &hit];
        let ranked = LexicalRanker.rank("regex search", &caps);
        assert_eq!(ranked[0].cap.path.to_string(), "bash.rg");
    }
}
