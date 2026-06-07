//! Emit JSON [`RopeFixture`]s for one session — the compacted rope
//! CONTENT (not just metrics) that the Layer-2 probe-question eval
//! consumes (mu-0fla). For each policy, dumps the post-compaction rope
//! the downstream model would see plus the spans that were dropped /
//! summarized (so probes can target lost content).
//!
//! Companion to `compaction-bench` (metrics) and `dump-compaction`
//! (human-readable single-policy dump). This one is machine-readable and
//! multi-policy: a JSON array, one element per policy.
//!
//! Usage:
//!   cargo run --release --example compaction-fixtures -p mu-ai -- <session.jsonl> [target_tokens]
//!
//! Policies emitted: no-compaction (the full-context baseline /
//! fidelity ceiling), span-family-drop (the production heuristic), and
//! hash-and-summary[mock] (deterministic mock judge — live-judge
//! fixtures are out of scope for this tool; the probe harness uses the
//! local model as the DOWNSTREAM answerer, not as the judge).

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use mu_core::context::compaction::bench::{load_session_rope, KeepHalfJudge};
use mu_core::context::compaction::fixture::{rope_fixture, RopeFixture};
use mu_core::context::compaction::hash_summary::HashAndSummaryPolicy;
use mu_core::context::compaction::heuristic::SpanFamilyDropPolicy;
use mu_core::context::compaction::{CompactionPolicy, NoCompactionPolicy};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 || args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!(
            "usage: compaction-fixtures <session.jsonl> [target_tokens]\n\
             emits a JSON array of RopeFixture (one per policy) to stdout"
        );
        return if args.len() < 2 {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        };
    }
    let path = PathBuf::from(&args[1]);
    let target_tokens: usize = args
        .get(2)
        .map(|s| s.parse().unwrap_or(4000))
        .unwrap_or(4000);

    let (session_id, rope, malformed) = match load_session_rope(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error loading session {path:?}: {e}");
            return ExitCode::FAILURE;
        }
    };
    if malformed > 0 {
        eprintln!("warning: skipped {malformed} malformed JSONL line(s)");
    }

    // (label, policy) in a fixed order: baseline first, then the two
    // real contenders.
    let policies: Vec<(&str, Arc<dyn CompactionPolicy>)> = vec![
        ("no-compaction", Arc::new(NoCompactionPolicy::new())),
        ("span-family-drop", Arc::new(SpanFamilyDropPolicy::new())),
        (
            "hash-and-summary-v1[mock]",
            Arc::new(HashAndSummaryPolicy::new(Arc::new(KeepHalfJudge::new()))),
        ),
    ];

    let fixtures: Vec<RopeFixture> = policies
        .iter()
        .map(|(label, policy)| {
            let result = policy.compact(&rope, target_tokens);
            rope_fixture(&session_id, label, &rope, &result, target_tokens)
        })
        .collect();

    match serde_json::to_string_pretty(&fixtures) {
        Ok(s) => {
            println!("{s}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("json serialization failed: {e}");
            ExitCode::FAILURE
        }
    }
}
