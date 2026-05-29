//! Find-quality benchmark — the instrument behind the mu-d2iy.6 gate.
//!
//! A fixed fixture catalog (the diff / compression / search neighborhoods, with
//! their distinct members) plus intent-sets whose right answer is known in
//! advance. [`run`] ranks each intent and checks the top hit. With
//! [`FakeEmbedder`](crate::embedder::FakeEmbedder) the score is deterministic
//! (CI baseline); the gate reruns the SAME cases with the real embedder to ask
//! the actual question — does semantic ranking route the intents a token ranker
//! conflates (the mu-d33g confident-wrong failure).

use crate::capability::Capability;
use crate::embedder::Embedder;
use crate::path::CapPath;
use crate::rank::Ranker;
use crate::semantic::{SemanticRanker, VectorCache};
use anyhow::Result;
use serde::Serialize;

/// One benchmark case: an intent and the capability path that should rank first.
pub struct Case {
    pub intent: &'static str,
    pub expect: &'static str,
}

fn cap(path: &str, summary: &str, kw: &[&str]) -> Capability {
    Capability {
        path: CapPath::parse(path).expect("fixture path is valid"),
        summary: summary.to_string(),
        keywords: kw.iter().map(|s| s.to_string()).collect(),
        invoke: vec![],
        help: None,
        requires: vec![],
    }
}

/// The fixture catalog the benchmark ranks over — distinct members of overlapping
/// neighborhoods (diff vs diff3 vs pretty-diff; search vs code-index), plus
/// distractors. Independent of the host so the benchmark is reproducible.
pub fn fixture_catalog() -> Vec<Capability> {
    vec![
        cap(
            "bash.diff",
            "compare two text files line by line and show the differences",
            &["compare", "files", "difference", "diff", "changed"],
        ),
        cap(
            "bash.diff3",
            "three-way merge reconciling three versions of a file",
            &["three", "merge", "reconcile", "versions", "conflict"],
        ),
        cap(
            "bash.diff-pretty",
            "show a syntax-highlighted side-by-side colorized diff",
            &["pretty", "highlight", "colorized", "side", "delta"],
        ),
        cap(
            "bash.compress",
            "compress data into a smaller archive",
            &["compress", "archive", "shrink", "zip"],
        ),
        cap(
            "bash.search",
            "search file contents for a text pattern or regex",
            &["search", "grep", "pattern", "regex", "contents"],
        ),
        cap(
            "mcp.code-index.recall",
            "find where a concept or symbol is implemented in source code",
            &["concept", "symbol", "implemented", "where", "source", "function"],
        ),
        cap(
            "bash.jq",
            "query and transform json data",
            &["json", "query", "transform"],
        ),
        cap(
            "bash.ls",
            "list files in a directory",
            &["list", "directory", "files"],
        ),
    ]
}

/// Intent-sets with known-right answers.
pub fn cases() -> Vec<Case> {
    vec![
        Case { intent: "compare two files and see the differences", expect: "bash.diff" },
        Case { intent: "three-way merge reconciling versions", expect: "bash.diff3" },
        Case { intent: "show a colorized side-by-side highlighted diff", expect: "bash.diff-pretty" },
        Case { intent: "compress a folder into an archive", expect: "bash.compress" },
        Case { intent: "search file contents for a regex pattern", expect: "bash.search" },
        Case { intent: "where is this function implemented in source", expect: "mcp.code-index.recall" },
        Case { intent: "query a json document", expect: "bash.jq" },
        // ADVERSARIAL (the discriminator): the intent shares no tokens with the
        // target's description/keywords, so a lexical / hashed-BoW ranker CANNOT
        // route it (fake misses this). The gate asks whether real embeddings can —
        // i.e. whether semantic ranking earns `find` its front door (mu-d33g).
        Case { intent: "locate the bug in this module", expect: "mcp.code-index.recall" },
    ]
}

/// One case's outcome.
#[derive(Debug, Clone, Serialize)]
pub struct CaseResult {
    pub intent: String,
    pub expect: String,
    pub got: String,
    pub ok: bool,
}

/// The benchmark report.
#[derive(Debug, Clone, Serialize)]
pub struct BenchReport {
    pub passed: usize,
    pub total: usize,
    pub results: Vec<CaseResult>,
}

/// Run the benchmark against `embedder`: embed the fixture once, then rank each
/// case's intent and check the top hit.
pub fn run<E: Embedder>(embedder: E) -> Result<BenchReport> {
    let fixture = fixture_catalog();
    let refs: Vec<&Capability> = fixture.iter().collect();
    let cache = VectorCache::build(&embedder, "bench", &refs)?;
    let ranker = SemanticRanker::new(embedder, cache.by_path);

    let mut results = Vec::new();
    for case in cases() {
        let ranked = ranker.rank(case.intent, &refs);
        let got = ranked
            .first()
            .map(|r| r.cap.path.to_string())
            .unwrap_or_default();
        let ok = got == case.expect;
        results.push(CaseResult {
            intent: case.intent.to_string(),
            expect: case.expect.to_string(),
            got,
            ok,
        });
    }
    let passed = results.iter().filter(|r| r.ok).count();
    Ok(BenchReport {
        total: results.len(),
        passed,
        results,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::FakeEmbedder;

    #[test]
    fn fake_embedder_run_is_deterministic_and_meets_baseline() {
        let a = run(FakeEmbedder::new()).unwrap();
        let b = run(FakeEmbedder::new()).unwrap();
        // deterministic
        assert_eq!(a.passed, b.passed);
        assert_eq!(a.total, cases().len());
        // Pinned fake baseline: the 7 token-aligned cases pass, the 1 adversarial
        // case (intent vocab disjoint from the target) MUST miss lexically. The
        // gate's real-embedder run is meaningful exactly because it can beat this:
        // 8/8 would mean semantic ranking routed the case lexical can't.
        assert_eq!(
            a.passed,
            7,
            "fake baseline moved: {}/{} ({:?})",
            a.passed,
            a.total,
            a.results.iter().filter(|r| !r.ok).map(|r| (&r.intent, &r.got)).collect::<Vec<_>>()
        );
        // the adversarial case is the one that misses lexically
        assert!(a.results.last().is_some_and(|r| !r.ok));
    }
}
