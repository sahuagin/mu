# Qualitative side-by-side: Opus auto-compaction vs mu `SpanFamilyDropPolicy`

Date: 2026-05-21
Operator: tcovert
Related: [compaction-2026-05-14.md](compaction-2026-05-14.md), [compaction-2026-05-21.md](compaction-2026-05-21.md)

## What this is

The benchmark numbers measure *speed and cost* of each compaction policy. They do NOT measure *what survives the compaction* — they only count tokens-before and tokens-after. This document is the first attempt at the qualitative comparison: **what does each method actually preserve from the same input?**

Reduction-percentage parity is misleading because the two methods are not doing the same thing:

- **Opus `compact_20260112`**: a *lossy compression* — produces a natural-language summary that paraphrases the entire conversation. Recall-preserving; fidelity-losing.
- **mu `SpanFamilyDropPolicy`**: a *lossy selection* — keeps a subset of spans verbatim and drops the rest with no trace. Fidelity-preserving on kept spans; zero information on dropped spans.

The token-reduction numbers tell you "how much space did we save." This document asks "and what did we save it from?"

## Method

- **Corpus**: `~/.local/share/mu/events/ebccc9256dcbe75a/session-2.jsonl` — a multi-step code-review conversation about the mu / c137 / claude-proxy ecosystem, ending in architectural discussion about event-log observability. 99 spans in the projected rope, 176,454 rope tokens (the bench previously reported 91k for this session against a different size estimator; this re-run with the dump-compaction example reports 176,454 — same data, slightly different token-counter parametrization).
- **Opus side**: ran `compact_20260112` beta endpoint with `pause_after_compaction: true`, model `claude-opus-4-7`, trigger 50k tokens. Single run. ~38.7s wall, ~$2.94 spend, 1,609 output tokens of summary.
- **mu side**: ran `cargo run --release --example dump-compaction -p mu-ai -- <session.jsonl>` — applies `SpanFamilyDropPolicy` with `target_tokens = 4000`. ~123 ms wall, $0.00 spend.

## Numbers (same input, different methods)

| | Tokens before | Tokens after | Reduction | Wall | Cost |
|---|---:|---:|---:|---:|---:|
| Opus `compact_20260112` | ~188k input | 1,609 output (summary) | ~99% | 38.7 s | $2.94 |
| mu `SpanFamilyDropPolicy` | 176,454 | 3,268 | 98.1% | 123 ms | $0.00 |

Both achieve ~98% reduction. The reduction percentages are nearly identical. The *content* of what each kept is wildly different.

## What Opus produced — a 5,190-character semantic summary

The Opus summary is structured under headings: `Repositories reviewed`, `Key architectural findings`, `Concerns raised and discussed`, `User's current state and direction`, `User pushback / corrections`, `Productive thread on agent-directed development`, `Final user comment`, `Tone notes`, `If continuing`. It captures:

- Cross-cutting findings ("`mu` is a serious local-first agent runtime with event-sourced design, ...")
- Specifics from tool results that no individual span retains coherently ("History showed spec-first development: protocol → transport → loop → faux provider → real provider → tools → ...")
- The user's emotional register and corrections ("User found my advice 'a little preachy' — they got here without that advice")
- Forward-looking suggestions for what should happen next ("If continuing... avoid lecturing... prefer specific suggestions tied to their actual seams")

Full text at `/tmp/opus-compaction-summary.txt`. Excerpt of "Key architectural findings" section:

> - `mu` is a serious local-first agent runtime with event-sourced design, capability-based tool model, queue-driven agent loop, provider abstraction (Anthropic, OpenRouter, OpenAI Codex, Faux), tool dispatch with approvals/retry, autonomy mode, observability primitives.
> - History (`/tmp/jj.log`) showed spec-first development: protocol → transport → loop → faux provider → real provider → tools → policy → event log → observability → autonomy → delegation → TUI → compaction → auth → analytics. ~150+ commits over ~10 days.
> - User has already addressed concerns I raised (e.g., `loop_.rs` was already split via mu-nk3 refactor).

Notice: the last bullet contains a specific factual claim ("`loop_.rs` was already split via mu-nk3 refactor") that came from a tool result the model saw during the original conversation. Opus integrated that fact into its summary. **mu's drop policy threw that tool result away entirely** — but the substance survives in the kept assistant message that originally surfaced it.

## What mu's `SpanFamilyDropPolicy` produced — 10 verbatim spans

Dropped 89 of 99 spans (all 86 ToolCall/ToolResult cluster members plus 3 old tool-call-bearing assistant turns). Survivors are the 7 user messages + 3 substantive assistant messages, verbatim:

| Span | Kind | Chars | Content (preview) |
|---|---|---:|---|
| msg-0-user | User | 788 | The initial code-review prompt: "please read the code in the mu workspace, ~/src/public_github/c137_orchestration ~/src/public_github/claude_proxy ..." |
| msg-60-user | User | 167 | "can you take a look at the repository history, in particular the first commit and subsequent work. Tell me if you have any further feedback ..." |
| msg-87-user | User | 21 | "try this. /tmp/jj.log" |
| msg-91-user | User | 1170 | "Yeah, I think I have most of what I want in there, now they all have to be tied together a bit better (like the rope context that is created but not actually used yet ...)" |
| msg-93-user | User | 356 | "a little preachy. But thanks? I mean, I did get here without that advice. ..." |
| msg-94-assistant | Assistant | 2795 | "Fair. You're right — that came off like 'discover architecture and discipline,' when you're obviously already doing that. Sorry. ..." |
| msg-95-user | User | 768 | "thanks. there was other good stuff you offered above that I need to incorporate as well. ..." |
| msg-96-assistant | Assistant | 5785 | "Yeah, that makes sense. The hard part isn't 'can agents write code?' ..." |
| msg-97-user | User | 343 | "One nice thing is that there have been a few occassions where we've gone, 'why did that happen?', or 'what just happened?', and by reviewing the event logs ..." |
| msg-98-assistant | Assistant | 2876 | "That's exactly the payoff. That's the moment where 'event-sourced agent runtime' stops being architecture taste and starts being a debugging superpower. ..." |

What's preserved: every word the user typed, plus the assistant's substantive replies to the user (not the assistant's tool-call invocations). Every drop reason was "old tool call/result cluster" — Tier 2 of the policy.

What's gone (zero information remaining):
- 43 `read` / `glob` tool calls (the assistant's tool-invocation messages)
- 43 tool results (file contents, glob output, .git history, etc.)
- 3 old assistant messages that were pure tool-call orchestration without substantive text

## The crucial observation

**Opus's summary mentions "mu-nk3 refactor"; mu's compaction does NOT mention this specific fact in the survivors.** That fact came from a tool result + the assistant's processing of it, and the assistant turn that processed it was dropped because it was a tool-call-bearing assistant (Tier 2). The user prompt that asked about it doesn't mention "mu-nk3" by name.

But look at what mu DID keep:

- Span `msg-91-user`: "...the rope context that is created but not actually used yet what llm sends server request" — the user's articulation of the integration gap.
- Span `msg-96-assistant`: a 5,785-character substantive reply about the *integration claims* framing, dataflow diffs, contract tests named after architectural contracts, specialized reviewer agents, the `seams.md` idea.
- Span `msg-98-assistant`: the "event-sourced runtime as debugging superpower" insight.

These are the **load-bearing turns** of the conversation. The actual *content* the user cared about — the architectural insights and the back-and-forth that produced them — is preserved verbatim.

**What's been lost: forensic detail.** If the conversation continues and a follow-up turn needs to recall what was in `loop_.rs` or what `agent-spawn` does or what `Cargo.toml` declared — that information is gone from mu's compaction but survives (paraphrased) in Opus's summary.

**What's been preserved better by mu**: the conversation's *interpretive content* — the user's prompts (full text, including tone), the assistant's substantive replies. Opus's summary paraphrases all of this. mu's drop keeps it verbatim.

## What does this mean for the architectural thesis?

The thesis was "the assistant's next turn is already a summary; the tool result is redundant once summarized." This experiment partially validates that thesis:

- ✅ For **interpretive content** (architecture discussions, opinions, takeaways), mu's drop preserves *more* than Opus's summary — verbatim quotes survive instead of paraphrases.
- ❌ For **forensic detail** (specific file contents, exact commit messages, specific tool outputs), mu's drop loses the data; Opus's summary preserves a paraphrase. If the conversation needs to recover that detail later, mu would need to re-run the tool calls; Opus would have a (lossy) summary to consult.
- ⚠️ The thesis assumes the assistant's next turn DID summarize the tool result. **When it didn't** — when the assistant just chained another tool call without commentary — mu's drop loses everything about that tool's output, and the chain of tool calls (which was three quarters of this session) is gone.

The two methods are doing **different jobs**. They are not interchangeable, and the higher reduction percentage is not "better."

## Implications for the next experiment

The cheap qualitative comparison above tells us *what* the difference is. It does NOT tell us *whether the difference matters for downstream task completion*. The recovery test (option 2 from prior discussion) would:

1. Take a session where a known-good outcome exists.
2. Mid-session, apply each compaction method.
3. Continue the session through the chosen compaction.
4. Compare: does each method's compaction let the agent reach the same final state? Where do they diverge?

Tasks likely to expose the divergence:

- "What was in `loop_.rs` line 200?" — mu drops, Opus paraphrases. mu would have to re-run `read`; Opus would consult its paraphrase (which may be wrong).
- "What did we decide about agent-directed development?" — both preserve this. Maybe mu preserves it *better* because the assistant message survives verbatim instead of being paraphrased.
- "What was the exact commit hash where mu-nk3 landed?" — both lose this. mu had to re-call git; Opus would invent or hedge.

**Net hypothesis worth testing:** mu's `SpanFamilyDropPolicy` is *better than Opus* at preserving interpretive context; *worse than Opus* at preserving forensic detail. The choice between them is a task-shape choice, not a "which is more compressed" choice. For coding-agent workloads where the agent re-runs tools as needed, mu's drop probably wins on cost AND on quality of the interpretive substrate. For research / analysis workloads where forensic detail can't be regenerated, Opus's summary probably wins.

## Reproduce

```sh
# 1. Opus auto-compaction (one run):
export ANTHROPIC_API_KEY="$(tq -r -f ~/.config/agent/config.toml anthropic.api_key)"
unset ANTHROPIC_BASE_URL
uv run --project ~/src/claude-personal/scripts python3 /tmp/opus-compact-one.py
# (writes /tmp/opus-compaction-summary.txt and /tmp/opus-compaction-meta.json)

# 2. mu SpanFamilyDropPolicy dump:
cargo run --release --example dump-compaction -p mu-ai -- \
  ~/.local/share/mu/events/ebccc9256dcbe75a/session-2.jsonl > /tmp/mu-compaction-dump.txt
```

## Provenance

- This document and its measurements: 2026-05-21
- Opus runner: `/tmp/opus-compact-one.py` (adapted from `~/src/claude-personal/scripts/compaction_calibration.py`)
- mu runner: `crates/mu-ai/examples/dump-compaction.rs` (new — added for this measurement)
- Session: `~/.local/share/mu/events/ebccc9256dcbe75a/session-2.jsonl`
- Total Anthropic spend across both Opus calls: ~$5.90 (one wasted on a missed-delta extraction, one with the working content-block fallback).
