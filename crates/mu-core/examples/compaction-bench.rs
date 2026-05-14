//! Benchmark harness — load mu-upb's per-session JSONL corpus, run
//! each [`CompactionPolicy`] against every session's projected rope,
//! emit CSV (default) or JSON to stdout.
//!
//! This is the mu-kgu.5 deliverable. The substantive logic lives in
//! [`mu_core::context::compaction::bench`]; this file is just a CLI
//! shim.
//!
//! ## Usage
//!
//! ```text
//! cargo run --example compaction-bench -- [FLAGS]
//!
//! FLAGS
//!   --corpus DIR        Root directory of per-daemon JSONL trees.
//!                       Default: $HOME/.local/share/mu/events
//!   --format csv|json   Output format. Default: csv.
//!   --target-tokens N   target_tokens passed to compact(). Default: 4000.
//!   --max-sessions N    Stop after N sessions (debug). Default: unlimited.
//!   --judge mock|live   HashAndSummaryPolicy judge wiring.
//!                       Default: mock (KeepHalfJudge — no network).
//!                       `live` errors out: a Provider-backed Judge
//!                       adapter is not yet wired (mu-kgu.4 follow-up).
//!   -h | --help         Print this banner and exit.
//! ```
//!
//! Mock mode is the load-bearing default — the bead's "Out" list
//! explicitly defers live judge calls. The `--judge live` switch
//! exists so the CLI surface is forward-compatible: once an
//! adapter lands, only the wiring inside `build_policies` changes.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use mu_core::context::compaction::bench::{
    benchmark_session, csv_header, csv_row, load_session_rope, BenchRow, KeepHalfJudge,
    LabeledPolicy,
};
use mu_core::context::compaction::hash_summary::HashAndSummaryPolicy;
use mu_core::context::compaction::heuristic::SpanFamilyDropPolicy;
use mu_core::context::compaction::NoCompactionPolicy;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Csv,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JudgeKind {
    Mock,
    Live,
}

struct Args {
    corpus: PathBuf,
    format: Format,
    target_tokens: usize,
    max_sessions: Option<usize>,
    judge: JudgeKind,
}

fn default_corpus() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".local/share/mu/events")
}

fn print_help() {
    print!(
        "compaction-bench — mu-kgu.5\n\
         Loads ~/.local/share/mu/events/<daemon>/<session>.jsonl session\n\
         logs and benchmarks each registered CompactionPolicy.\n\n\
         Flags:\n  \
         --corpus DIR        root of per-daemon JSONL trees (default ~/.local/share/mu/events)\n  \
         --format csv|json   output shape (default csv)\n  \
         --target-tokens N   target_tokens for compact() (default 4000)\n  \
         --max-sessions N    stop after N sessions (default unlimited)\n  \
         --judge mock|live   HashAndSummaryPolicy judge (default mock; live not wired)\n  \
         -h, --help          print this banner\n",
    );
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        corpus: default_corpus(),
        format: Format::Csv,
        target_tokens: 4_000,
        max_sessions: None,
        judge: JudgeKind::Mock,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "--corpus" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--corpus requires a directory argument".to_string())?;
                args.corpus = PathBuf::from(v);
            }
            "--format" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--format requires csv|json".to_string())?;
                args.format = match v.as_str() {
                    "csv" => Format::Csv,
                    "json" => Format::Json,
                    other => return Err(format!("--format: expected csv|json, got {other:?}")),
                };
            }
            "--target-tokens" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--target-tokens requires a number".to_string())?;
                args.target_tokens = v
                    .parse::<usize>()
                    .map_err(|e| format!("--target-tokens parse: {e}"))?;
            }
            "--max-sessions" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--max-sessions requires a number".to_string())?;
                args.max_sessions = Some(
                    v.parse::<usize>()
                        .map_err(|e| format!("--max-sessions parse: {e}"))?,
                );
            }
            "--judge" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--judge requires mock|live".to_string())?;
                args.judge = match v.as_str() {
                    "mock" => JudgeKind::Mock,
                    "live" => JudgeKind::Live,
                    other => return Err(format!("--judge: expected mock|live, got {other:?}")),
                };
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    Ok(args)
}

fn build_policies(judge: JudgeKind) -> Result<Vec<LabeledPolicy>, String> {
    let mut policies: Vec<LabeledPolicy> = Vec::new();
    policies.push(LabeledPolicy {
        label: "no-compaction".into(),
        policy: Arc::new(NoCompactionPolicy::new()),
        model_calls: 0,
    });
    policies.push(LabeledPolicy {
        label: "span-family-drop".into(),
        policy: Arc::new(SpanFamilyDropPolicy::new()),
        model_calls: 0,
    });
    match judge {
        JudgeKind::Mock => {
            policies.push(LabeledPolicy {
                label: "hash-and-summary-v1[mock]".into(),
                policy: Arc::new(HashAndSummaryPolicy::new(Arc::new(KeepHalfJudge::new()))),
                model_calls: 1,
            });
        }
        JudgeKind::Live => {
            return Err(
                "--judge live: Provider-backed Judge adapter not yet wired (mu-kgu.4 \
                 follow-up). Re-run with --judge mock for now."
                    .to_string(),
            );
        }
    }
    Ok(policies)
}

/// Walk `root` two levels deep: each subdir is a daemon, each `.jsonl`
/// inside is a session. Returns the discovered session paths in
/// deterministic (sorted) order.
fn discover_sessions(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut sessions: Vec<PathBuf> = Vec::new();
    if !root.exists() {
        return Ok(sessions);
    }
    let mut daemons: Vec<PathBuf> = std::fs::read_dir(root)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.path())
        .collect();
    daemons.sort();
    for daemon in daemons {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&daemon)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
            .collect();
        entries.sort();
        sessions.extend(entries);
    }
    Ok(sessions)
}

fn emit_csv(rows: &[BenchRow]) {
    println!("{}", csv_header());
    for r in rows {
        println!("{}", csv_row(r));
    }
}

fn emit_json(rows: &[BenchRow]) {
    // Pretty-printed array of rows — small enough that streaming
    // line-by-line isn't worth the JSON-spec gymnastics.
    match serde_json::to_string_pretty(rows) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("json serialization failed: {e}"),
    }
}

fn run(args: Args) -> Result<(), String> {
    let policies = build_policies(args.judge)?;
    let sessions = discover_sessions(&args.corpus)
        .map_err(|e| format!("read corpus {:?}: {}", args.corpus, e))?;
    if sessions.is_empty() {
        eprintln!(
            "no .jsonl sessions found under {:?} — corpus directory empty or missing",
            args.corpus,
        );
    }
    let take = args.max_sessions.unwrap_or(usize::MAX);
    let mut all_rows: Vec<BenchRow> = Vec::new();
    let mut total_malformed: usize = 0;
    for path in sessions.iter().take(take) {
        match load_session_rope(path) {
            Ok((sid, rope, malformed)) => {
                total_malformed = total_malformed.saturating_add(malformed);
                let rows = benchmark_session(&sid, &rope, &policies, args.target_tokens);
                all_rows.extend(rows);
            }
            Err(e) => {
                eprintln!("skip {path:?}: load failed: {e}");
            }
        }
    }
    match args.format {
        Format::Csv => emit_csv(&all_rows),
        Format::Json => emit_json(&all_rows),
    }
    if total_malformed > 0 {
        eprintln!("warning: skipped {total_malformed} malformed JSONL line(s) across the corpus",);
    }
    Ok(())
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("argument error: {e}");
            eprintln!("run with --help for usage");
            return ExitCode::from(2);
        }
    };
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
