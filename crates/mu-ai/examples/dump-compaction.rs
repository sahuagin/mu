//! Dump SpanFamilyDropPolicy decisions + surviving spans for a single
//! session.jsonl. One-off tool for qualitative side-by-side
//! comparison against other compaction methods.
//!
//! Usage:
//!   cargo run --release --example dump-compaction -p mu-ai -- <session.jsonl> [target_tokens]

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use mu_core::context::compaction::bench::load_session_rope;
use mu_core::context::compaction::heuristic::SpanFamilyDropPolicy;
use mu_core::context::compaction::{CompactionDecision, CompactionPolicy};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: dump-compaction <session.jsonl> [target_tokens]");
        return ExitCode::FAILURE;
    }
    let path = PathBuf::from(&args[1]);
    let target_tokens: usize = args
        .get(2)
        .map(|s| s.parse().unwrap_or(4000))
        .unwrap_or(4000);

    let (session_id, rope, malformed) = match load_session_rope(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error loading session: {e}");
            return ExitCode::FAILURE;
        }
    };

    println!("# session: {} ({})", session_id, path.display());
    println!(
        "# spans: {} (malformed lines skipped: {})",
        rope.len(),
        malformed
    );
    println!();
    println!("# rope (one line per span):");
    for (i, span) in rope.iter().enumerate() {
        let snippet: String = span.content().chars().take(160).collect();
        println!(
            "  [{i:>3}] kind={:?} id={} retention={:?} cacheable={} chars={} preview={:?}",
            span.kind(),
            span.id(),
            span.retention(),
            span.cacheable(),
            span.content().chars().count(),
            snippet.replace('\n', " "),
        );
    }

    let policy: Arc<dyn CompactionPolicy> = Arc::new(SpanFamilyDropPolicy::new());
    let result = policy.compact(&rope, target_tokens);
    println!();
    println!("# === SpanFamilyDropPolicy compact (target_tokens = {target_tokens}) ===");
    println!(
        "# tokens: {} -> {} (reduction: {:.1}%)",
        result.tokens_before,
        result.tokens_after,
        (1.0 - result.tokens_after as f64 / result.tokens_before.max(1) as f64) * 100.0
    );
    println!(
        "# spans:  {} -> {} (drop decisions: {})",
        rope.len(),
        result.rope.len(),
        result.decisions.len()
    );
    println!("# wall: {} µs", result.wall_clock_us);
    println!();
    println!("# survivors (verbatim):");
    for (i, span) in result.rope.iter().enumerate() {
        let snippet: String = span.content().chars().take(300).collect();
        println!(
            "  [{i}] kind={:?} id={} chars={} preview={:?}",
            span.kind(),
            span.id(),
            span.content().chars().count(),
            snippet.replace('\n', " "),
        );
    }
    println!();
    println!("# drop decisions (id => reason):");
    for d in &result.decisions {
        match d {
            CompactionDecision::Dropped { span_id, reason } => {
                println!("  - {}: {}", span_id, reason);
            }
            other => {
                println!("  - (other variant): {:?}", other);
            }
        }
    }
    ExitCode::SUCCESS
}
