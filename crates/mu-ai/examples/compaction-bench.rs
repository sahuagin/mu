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
//!                       `live`: Anthropic Haiku via $ANTHROPIC_API_KEY
//!                       (override model with $MU_BENCH_JUDGE_MODEL).
//!   -h | --help         Print this banner and exit.
//! ```
//!
//! Mock mode is the load-bearing default — the bead's "Out" list
//! explicitly defers live judge calls. The `--judge live` switch
//! activates the real Provider-backed adapter (mu-kgu.11):
//! Anthropic Haiku 4.5 by default, ~$0.05 per compaction event.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use mu_core::agent::Provider;
use mu_core::context::compaction::bench::{
    benchmark_session, csv_header, csv_row, load_session_rope, BenchRow, KeepHalfJudge,
    LabeledPolicy,
};
use mu_core::context::compaction::hash_summary::HashAndSummaryPolicy;
use mu_core::context::compaction::heuristic::SpanFamilyDropPolicy;
use mu_core::context::compaction::provider_judge::ProviderJudge;
use mu_core::context::compaction::NoCompactionPolicy;
// mu-kgu.11: --judge live uses Anthropic Haiku 4.5 by default
// (cheapest reliable judge per the bead). Operator can override via
// $MU_BENCH_JUDGE_MODEL.
// 2026-05-21: --judge-provider {anthropic|openai-codex|openrouter} lets
// the operator route the judge's LLM call through their preferred
// provider (e.g., subscription-priced OpenAI Codex OAuth instead of
// pay-per-token Anthropic API).
use mu_ai::{AnthropicProvider, OpenRouterProvider, OpenaiCodexProvider};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JudgeProvider {
    Anthropic,
    OpenaiCodex,
    Openrouter,
}

impl JudgeProvider {
    fn label(self) -> &'static str {
        match self {
            JudgeProvider::Anthropic => "anthropic",
            JudgeProvider::OpenaiCodex => "openai-codex",
            JudgeProvider::Openrouter => "openrouter",
        }
    }
    fn default_model(self) -> &'static str {
        match self {
            JudgeProvider::Anthropic => "claude-haiku-4-5-20251001",
            JudgeProvider::OpenaiCodex => "gpt-5.5",
            JudgeProvider::Openrouter => "anthropic/claude-haiku-4.5",
        }
    }
}

struct Args {
    corpus: PathBuf,
    format: Format,
    target_tokens: usize,
    max_sessions: Option<usize>,
    judge: JudgeKind,
    judge_provider: JudgeProvider,
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
         --corpus DIR                       root of per-daemon JSONL trees (default ~/.local/share/mu/events)\n  \
         --format csv|json                  output shape (default csv)\n  \
         --target-tokens N                  target_tokens for compact() (default 4000)\n  \
         --max-sessions N                   stop after N sessions (default unlimited)\n  \
         --judge mock|live                  HashAndSummaryPolicy judge (default mock)\n  \
         --judge-provider PROVIDER          live-judge backend: anthropic | openai-codex | openrouter\n  \
         \\\\                                  (default anthropic). OpenAI Codex uses OAuth; OpenRouter uses\n  \
         \\\\                                  OPENROUTER_API_KEY. Subscription-priced backends help bound cost\n  \
         \\\\                                  on heavy research runs. Set MU_BENCH_JUDGE_MODEL to override\n  \
         \\\\                                  the per-provider default model.\n  \
         -h, --help                         print this banner\n",
    );
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        corpus: default_corpus(),
        format: Format::Csv,
        target_tokens: 4_000,
        max_sessions: None,
        judge: JudgeKind::Mock,
        judge_provider: JudgeProvider::Anthropic,
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
            "--judge-provider" => {
                let v = it.next().ok_or_else(|| {
                    "--judge-provider requires anthropic|openai-codex|openrouter".to_string()
                })?;
                args.judge_provider = match v.as_str() {
                    "anthropic" => JudgeProvider::Anthropic,
                    "openai-codex" => JudgeProvider::OpenaiCodex,
                    "openrouter" => JudgeProvider::Openrouter,
                    other => {
                        return Err(format!(
                            "--judge-provider: expected anthropic|openai-codex|openrouter, got {other:?}"
                        ))
                    }
                };
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    Ok(args)
}

fn build_policies(
    judge: JudgeKind,
    judge_provider: JudgeProvider,
) -> Result<Vec<LabeledPolicy>, String> {
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
            // mu-kgu.11 + 2026-05-21: real Provider-backed judge.
            // judge_provider picks the backend; MU_BENCH_JUDGE_MODEL
            // overrides the per-provider default model. Anthropic
            // remains the default for backward compatibility.
            let model = std::env::var("MU_BENCH_JUDGE_MODEL")
                .unwrap_or_else(|_| judge_provider.default_model().to_string());
            let provider: Arc<dyn Provider> = match judge_provider {
                JudgeProvider::Anthropic => {
                    let p = AnthropicProvider::from_env(model.clone())
                        .map_err(|e| format!("--judge live (anthropic): {e}"))?;
                    Arc::new(p)
                }
                JudgeProvider::OpenaiCodex => {
                    let p = OpenaiCodexProvider::from_store(model.clone())
                        .map_err(|e| format!("--judge live (openai-codex): {e}"))?;
                    Arc::new(p)
                }
                JudgeProvider::Openrouter => {
                    let p = OpenRouterProvider::from_env(model.clone())
                        .map_err(|e| format!("--judge live (openrouter): {e}"))?;
                    Arc::new(p)
                }
            };
            let judge_impl = ProviderJudge::new(provider);
            policies.push(LabeledPolicy {
                label: format!(
                    "hash-and-summary-v1[live:{}:{model}]",
                    judge_provider.label()
                ),
                policy: Arc::new(HashAndSummaryPolicy::new(Arc::new(judge_impl))),
                model_calls: 1,
            });
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
    let policies = build_policies(args.judge, args.judge_provider)?;
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
