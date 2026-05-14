# mu: goal session 2026-05-14 — mu-kgu.5 (compaction benchmark harness)

Phase 3 of mu-kgu (parallel-worker experiment 7g-5). This worker
landed the benchmark harness; sibling worker mu-kgu.6 (cache
composition tests) ran in a separate workspace against integration
tests in mu-ai. No file conflicts expected (this worker touched
`crates/mu-core/{src/context/compaction,examples}/`; sibling touched
`crates/mu-ai/anthropic_tests.rs`).

## What landed

| Commit | Bead | One-line |
|---|---|---|
| (this branch) | mu-kgu.5 | benchmark harness: load mu-upb JSONL corpus, run NoCompactionPolicy / SpanFamilyDropPolicy / HashAndSummaryPolicy[mock] against each session, emit CSV or JSON |

Files added:
- `crates/mu-core/src/context/compaction/bench.rs` — loader (`load_session_rope` → `SessionEventLog::from_jsonl` (`event_log.rs:263`) → `assemble_rope` (`assembly.rs:49`)), `BenchRow`, `benchmark_session`, `KeepHalfJudge` (no-network mock), CSV helpers. 8 unit tests.
- `crates/mu-core/examples/compaction-bench.rs` — CLI shim: `--corpus`, `--format csv|json`, `--target-tokens`, `--max-sessions`, `--judge mock|live` (live errors out cleanly until a Provider-backed Judge adapter lands).
- `crates/mu-core/src/context/compaction.rs` — added `pub mod bench;`.

## Test state

| Gate | Result |
|---|---|
| `cargo test --workspace` | green — 489 tests, 0 failed |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | green |
| `cargo fmt --all --check` | clean |
| End-to-end smoke (`cargo run --example compaction-bench -- --max-sessions 3`) | emits 9 CSV rows against the live `~/.local/share/mu/events` corpus |

## Goal status

- mu-kgu.5: **complete** (pending merge — DO NOT MERGE per goal spec).
- PR: see branch `agent/kgu5-benchmark-harness-2026-05-14`.

## Stop criteria that fired

None. Single-pass implementation; two formatting/clippy nits fixed inside the loop (extra blank line in `bench.rs`; `print!("{}", "...")` → `print!("...")` to satisfy `clippy::print_literal`).

## Capability invariant audit

| Invariant | Held? | Notes |
|---|---|---|
| INV-1 (`AutonomyCapability::Disallowed` default; intersect is most-restrictive) | Y | Bench harness is read-only — never instantiates a Capability or touches the dispatch path |

## Spec drift check

- No trait, wire-protocol, or `EventPayload` changes. The bench-harness module is read-only against existing event-log and rope APIs.
- No spec amendment needed.

## Things noticed but not addressed

1. **Live judge adapter is the next bead.** mu-kgu.5's bead lists "Live judge API calls" under Out, and the harness ships with `--judge live` returning a clean "not yet wired (mu-kgu.4 follow-up)" error. Wiring `ProviderJudge` (sync `Judge` over async `Provider::stream`) is the natural follow-up. File a new bead before any benchmark vs live-Anthropic comparison runs.
2. **Token estimate is byte-based**, not tokenizer-accurate. Both `SpanFamilyDropPolicy::span_size` (`heuristic.rs:73`) and `HashAndSummaryPolicy::estimate_tokens` (`hash_summary.rs:485`) use char/byte counts. Hard-coded constants (target-tokens=4000, KEEP_RECENT_ASSISTANT=2) make numerical comparisons across policies meaningful only relative to one another, not as absolute token costs. A future bead can wire `tiktoken-rs` or a provider-side tokenizer.
3. **Corpus contains only `session-1.jsonl` per daemon today** (most sessions are empty or two-message ping/pongs from `faux` provider). The harness's output rows are correct but not yet interesting — they will become so once a non-trivial mu coding session is recorded.

## Suggested next session

- mu-kgu.4 follow-up: file a bead to land a `ProviderJudge` adapter (sync `Judge` wrapping `Provider::stream` via tokio `block_on`). This is the load-bearing dependency for converting `--judge live` from "errors out" to "calls Haiku 4.5 and measures wall-clock vs Anthropic's 5-min compaction baseline."
- mu-pex follow-up: route `BenchRow` into mu-pex's metrics pipeline so the comparisons land on a dashboard rather than stdout.

## Cost / turns / wall-clock

Tracked at session-end (operator runs `claude-code --print --include-usage` against the session log).

## How to run

```bash
cargo run --example compaction-bench                            # default: csv, target=4000, ~/.local/share/mu/events
cargo run --example compaction-bench -- --format json --max-sessions 5
cargo run --example compaction-bench -- --target-tokens 1000 --judge mock
cargo run --example compaction-bench -- --help
```

CSV columns: `session_id, policy_label, tokens_before, tokens_after, decisions_count, model_calls, wall_clock_ms, spans_before, spans_after`.
