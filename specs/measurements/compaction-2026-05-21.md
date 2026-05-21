# Path B: mu compaction ladder measured against real-workload corpus (post-mu-yqeq.8)

Date: 2026-05-21
Operator: tcovert
Tracking bead: (none — measurement follow-on to the 2026-05-14 spec)

> Supersedes the README's previous "live judge estimated ~50ms / ~$0.05" numbers, which were not directly measured. The 2026-05-14 spec measured only the Anthropic Opus side; mu's policy ladder was estimated. This document is the missing measured-mu half.

## Motivation

Operator request: "Re-run with both legs through the API." The 2026-05-14 spec produced a defensible Opus baseline ($2.03 / 38.18s per compaction at 124k tokens). The mu side was an estimate, not a measurement. We now have:

- A working `compaction-bench --judge live` harness wired to a real Anthropic provider (mu-kgu.11).
- A real-workload corpus of large sessions (multiple sessions >100k tokens, several >1M tokens).
- Post-mu-yqeq.8 the Projected path is live — making this measurement representative of current code, not a stale snapshot.

This document captures the live-judge measurement and corrects the README's headline claims.

## Method

- **Corpus**: top-5 largest sessions in `~/.local/share/mu/events/` by rope-token count (selected by `jq '.payload.usage.input_tokens' | sort -rn` walk; symlinked into `/tmp/mu-bench-large-corpus/<daemon>/<session>.jsonl` to scope the bench).
  - `ebccc9256dcbe75a/session-2.jsonl` — 91,199 rope tokens
  - `f3845422ad5bb009/session-4.jsonl` — 234,631 rope tokens
  - `0e87ec2a7f37729c/session-2.jsonl` — 122,478 rope tokens (closest to 2026-05-14 baseline size of 124,091)
  - `2fc69e9a05512041/session-1.jsonl` — 176,454 rope tokens
  - `8c78230c467e1de7/session-1.jsonl` — 102,751 rope tokens

  Total rope tokens across the 5: 727,513. (Note: each session's `usage.input_tokens` peak — what Anthropic billed during live use — is larger than the rope token count because it includes the cache prefix, tool schemas, and system prompts that the bench's rope extraction skips. The bench measures what compaction would see, not what the provider was billed for.)

- **Harness**: `cargo run --release --example compaction-bench -p mu-ai -- --corpus /tmp/mu-bench-large-corpus --judge live --format json`
- **Policies measured**:
  - `no-compaction` (`NoCompactionPolicy`) — baseline, no transform
  - `span-family-drop` (`SpanFamilyDropPolicy`) — structural rules, no LLM call
  - `hash-and-summary-v1[live:claude-haiku-4-5-20251001]` (`HashAndSummaryPolicy` with `ProviderJudge`) — Haiku 4.5 generates a `keep + summary` JSON, one LLM call per session
- **Model for live judge**: `claude-haiku-4-5-20251001` via direct `api.anthropic.com` (no proxy). `ANTHROPIC_API_KEY` sourced from `~/.config/agent/config.toml`.
- **Runs**: one per (policy, session) pair = 15 rows total. Single run; no warm-up; no averaging across re-runs.
- **Measurement points**: bench's in-process `wall_clock_us` timer (microsecond resolution); token counts before/after via the `Tokenizer` integration added in 2026-05 (real tokenization, not char/4 estimate).

## Results

Aggregate by policy (5 sessions each):

| Policy | Sessions | Median wall | Total wall | Reduction (total) | LLM calls |
|---|---:|---:|---:|---:|---:|
| `no-compaction` | 5 | 71 ms | 398 ms | 727,513 → 727,513 (0%) | 0 |
| `span-family-drop` | 5 | **62 ms** | 347 ms | 727,513 → 16,193 (**97.8%**) | 0 |
| `hash-and-summary-v1[live:Haiku]` | 5 | **6.0 s** | 31.5 s | 727,513 → 235,039 (**67.7%**) | 5 |

Per-session detail:

| Policy | Session | Tokens before | Tokens after | Spans b/a | Wall (ms) |
|---|---|---:|---:|---:|---:|
| `no-compaction` | ebccc9256/sess-2 | 91,199 | 91,199 | 66/66 | 99 |
| `span-family-drop` | ebccc9256/sess-2 | 91,199 | 4,057 | 66/4 | 46 |
| `hash-and-summary[Haiku]` | ebccc9256/sess-2 | 91,199 | 7,230 | 66/6 | 3,198 |
| `no-compaction` | 0e87ec2/sess-2 | 122,478 | 122,478 | 76/76 | 63 |
| `span-family-drop` | 0e87ec2/sess-2 | 122,478 | 1,416 | 76/11 | 62 |
| `hash-and-summary[Haiku]` | 0e87ec2/sess-2 | 122,478 | 10,997 | 76/11 | 5,124 |
| `no-compaction` | 8c78230/sess-1 | 102,751 | 102,751 | 43/43 | 54 |
| `span-family-drop` | 8c78230/sess-1 | 102,751 | 1,707 | 43/24 | 54 |
| `hash-and-summary[Haiku]` | 8c78230/sess-1 | 102,751 | **102,751** | 43/**43** | 6,445 |
| `no-compaction` | 2fc69e9/sess-1 | 176,454 | 176,454 | 99/99 | 71 |
| `span-family-drop` | 2fc69e9/sess-1 | 176,454 | 3,268 | 99/10 | 73 |
| `hash-and-summary[Haiku]` | 2fc69e9/sess-1 | 176,454 | 84,517 | 99/26 | 6,004 |
| `no-compaction` | f3845422/sess-4 | 234,631 | 234,631 | 124/124 | 112 |
| `span-family-drop` | f3845422/sess-4 | 234,631 | 5,745 | 124/34 | 111 |
| `hash-and-summary[Haiku]` | f3845422/sess-4 | 234,631 | 29,544 | 124/71 | 10,704 |

## Cost (measured / calculated)

Haiku 4.5 retail rates (as of 2026-05): $1/M input, $5/M output. The live judge sends approximately the rope's content as input (templated into a "rate these spans" prompt) and receives a small JSON `keep + summary` response.

- Total input across 5 calls: ~727k tokens × $1/M = **$0.73**
- Total output across 5 calls: ~5k tokens × $5/M = **$0.025**
- **Total measurement spend: ~$0.76** (manually verified against the operator's Anthropic console)
- **Per-event average: ~$0.16**
- **Per-event range: $0.09 (91k corpus) — $0.23 (234k corpus)** — scales linearly with input size

## User-observable latency (work-time vs perceived-time)

The numbers above are *work-time* — wall-clock from `policy.compact()` start to finish. But mu's agent loop runs async-capable policies in the background (`mod.rs:784`: `if policy.is_async() && bg_compaction.can_start() { bg_compaction.start(...); }`). The trigger turn proceeds with the un-compacted rope immediately; the completed result is picked up at the start of the next turn via `bg_compaction.try_take().await`.

Which policies are async:

- `HashAndSummaryPolicy`: `is_async() = true` (per `hash_summary.rs:345`). Background-eligible.
- `SpanFamilyDropPolicy`: default `is_async() = false` — but microseconds-fast, so sync is fine.
- `NoCompactionPolicy`: default `is_async() = false` — no-op anyway.

So the *user-observable* compaction latency picture is:

| Method | Work time (median) | User-observable latency |
|---|---:|---:|
| Anthropic Opus `compact_20260112` | 38.18 s | **38.18 s** — synchronous pause mid-response |
| mu `HashAndSummaryPolicy[live:Haiku]` | 6.0 s | **~0 s** — background; the trigger turn doesn't wait |
| mu `SpanFamilyDropPolicy` | 62 ms | **~0 s** — sync but fast enough to be invisible |

The 6-second Haiku wall is only "observed" if the next turn happens within 6 seconds of the compaction trigger. In a typical conversation (model-thinking + user-reading + user-typing time between turns), 6s is comfortably amortized. The Opus pathway has no such amortization — it blocks the in-flight response, and the user sees the full 38 seconds.

So the "Haiku is 7.5× faster than Opus" framing below understates the win on the latency axis. The work-time ratio is 7.5×; the **user-observable-latency ratio is effectively infinite** (mu has zero observable latency in normal use; Opus has ~38s).

The work-time numbers below remain canonical for cost calculations and capacity planning. The observable-latency numbers above are what determines what the user *feels*.

## Apples-to-apples vs the 2026-05-14 Opus baseline

The 2026-05-14 measurement was a single session of 124,091 tokens. The closest match in this corpus is `0e87ec2a7f37729c/session-2` at 122,478 rope tokens.

| Policy on 122,478-token corpus | Wall | Cost per event |
|---|---:|---:|
| Opus auto-compaction (2026-05-14 median, 124k corpus) | 38.18 s | $2.03 |
| mu `HashAndSummaryPolicy[live:Haiku]` (this run) | 5.12 s | ~$0.12 |
| mu `SpanFamilyDropPolicy` (this run) | 62 ms | $0.00 |

Ratios:

- **Live Haiku judge vs Opus**: ~7.5× faster, ~17× cheaper.
- **Structural drop vs Opus**: ~616× faster, infinite cost ratio (or just "$0/event").
- **Structural drop's token reduction**: 122,478 → 1,416 = **98.8%** — *exceeding* Opus's 98% on this session.

## Corrections to prior numbers

The README previously cited:

| Claim | Source | Status |
|---|---|---|
| Live Haiku judge ~50ms | README, estimated | **Wrong**. Measured at ~6 seconds median. Network round-trip dominates over Haiku's compute. |
| Live Haiku judge ~$0.05 / event | README, estimated | **Slightly off**. Measured at ~$0.16 / event on 145k-avg corpus. Estimate was reasonable for a smaller corpus but scales linearly with input size. |
| 700× speed advantage over Opus | README, derived from estimate | **Wrong**. Real ratio is ~7.5× at matched corpus size. |
| 40× cost advantage over Opus | README, derived from estimate | **Wrong**. Real ratio is ~17× at matched corpus size. |
| Structural drop 19µs / $0 | README | **Approximately right for small sessions** (~2k-token rope). Scales to ~62 ms on 100k-token sessions. Still $0 cost. |
| 2M× ratio (structural vs Opus) | README, with caveats | **Approximately right at small scale** (38s ÷ 19µs ≈ 2M). At large-corpus scale (38s ÷ 62ms ≈ 616×), the ratio is smaller but still extreme. The caveat that "structural doesn't include real tokenization" was correct — this measurement DOES include real tokenization. |

The README has been updated to reflect the measured numbers from this run.

## Anomaly: session where Haiku declined to compact

`8c78230c467e1de7/session-1` (102,751 tokens, 43 spans): the Haiku judge call returned `keep_all` — `tokens_after == tokens_before`, `spans_after == 43` unchanged, `decisions_count == 43`. Possible causes:

- The judge's prompt instructed it to be conservative and it interpreted the rope's structure as "everything is important."
- The judge call errored / timed out and the policy's fallback returned the unchanged rope. (No error was surfaced in the bench output; would need to wire `--judge live` to log judge response text to distinguish.)
- The rope content genuinely had no obvious "absorb me" structure — all spans were user/assistant turns with no tool noise.

Open question; not blocking. Worth investigating in a follow-on bead if compaction-quality variance becomes a load-bearing claim.

## Caveats

1. **Single-run measurement.** No averaging across re-runs; the 6-second Haiku median is from 5 distinct sessions, not 5 retries of one session. Re-running the same session could vary by ±1-2 seconds based on Anthropic's load and network RTT.

2. **Haiku-only judge.** The live judge was Haiku 4.5. A Sonnet or Opus judge would be slower and more expensive but might produce higher-quality summaries / better keep decisions. Not measured here.

3. **Corpus size matters.** This corpus is heavy on large sessions (91k–235k tokens). The README's previously-cited ~50ms estimate may have been for a smaller corpus where Haiku's per-call overhead dominates differently. Both regimes are valid; the README now cites the measured numbers.

4. **Token counts are rope-tokens, not API-billed tokens.** The bench's `tokens_before` is the rope content. Real Anthropic billing also includes system prompts, tool schemas, and cache prefixes. Per-session billed `input_tokens` is typically 1.5×–10× the rope count.

5. **Reduction ratio compares token counts, not semantic quality.** Both `span-family-drop` (97.8% reduction) and Opus compaction (98% reduction) achieve similar quantitative reduction — but the latter produces a coherent natural-language summary while the former drops typed span families. They are not equivalent compactions; they are equivalent *reductions*. Quality comparison is a separate research question (filed informally; not in this measurement).

## Reproduce

```sh
# 1. Symlink the top-5 sessions into a one-off corpus.
CORPUS=/tmp/mu-bench-large-corpus
mkdir -p "$CORPUS"
for entry in \
  "ebccc9256dcbe75a session-2.jsonl" \
  "f3845422ad5bb009 session-4.jsonl" \
  "0e87ec2a7f37729c session-2.jsonl" \
  "2fc69e9a05512041 session-1.jsonl" \
  "8c78230c467e1de7 session-1.jsonl"
do
  daemon=$(echo "$entry" | awk '{print $1}')
  session=$(echo "$entry" | awk '{print $2}')
  mkdir -p "$CORPUS/$daemon"
  ln -sf "$HOME/.local/share/mu/events/$daemon/$session" "$CORPUS/$daemon/$session"
done

# 2. Run the bench against the curated corpus with the live judge.
export ANTHROPIC_API_KEY="$(tq -r -f ~/.config/agent/config.toml anthropic.api_key)"
cargo run --release --example compaction-bench -p mu-ai -- \
  --corpus "$CORPUS" --judge live --format json > /tmp/compaction-bench-large.json

# 3. Aggregate by policy.
jq -r '
  group_by(.policy_label) | map({
    policy: .[0].policy_label,
    sessions: length,
    wall_ms_median: ((map(.wall_clock_us) | sort | .[length/2|floor]) / 1000),
    tokens_before_total: (map(.tokens_before) | add),
    tokens_after_total: (map(.tokens_after) | add),
    reduction_pct: ((1 - ((map(.tokens_after) | add) / (map(.tokens_before) | add))) * 100 | floor)
  })
' /tmp/compaction-bench-large.json
```

## Provenance

- Measurement runner: `cargo run --release --example compaction-bench -p mu-ai`
- Bench source: `crates/mu-ai/examples/compaction-bench.rs` (CLI shim) + `crates/mu-core/src/context/compaction/bench.rs` (substantive logic).
- Policy implementations: `crates/mu-core/src/context/compaction/{no_op,span_family_drop,hash_and_summary}.rs`.
- Live judge implementation: `crates/mu-core/src/context/compaction/provider_judge.rs` (`ProviderJudge`).
- Tokenizer: `crates/mu-core/src/context/tokenizer.rs` (mu-y34, added 2026-05 to make cost estimates honest).
- This document supersedes the live-judge estimates in `specs/measurements/compaction-2026-05-14.md` §"Apples-to-apples summary"; the Opus side of that spec remains canonical (the live-judge half was always an estimate awaiting this measurement).
