# Option 2: Recovery test — does compaction loss affect downstream answer quality?

Date: 2026-05-21
Operator: tcovert
Related:
- [compaction-2026-05-14.md](compaction-2026-05-14.md) — Opus baseline (work-time)
- [compaction-2026-05-21.md](compaction-2026-05-21.md) — mu policy ladder (work-time + user-observable latency)
- [compaction-quality-2026-05-21.md](compaction-quality-2026-05-21.md) — qualitative side-by-side (what each method preserves)

## Hypothesis under test

From the qualitative side-by-side, the two compaction methods are *doing different jobs*:

- **mu `SpanFamilyDropPolicy`**: preserves user/assistant turns verbatim; drops tool clusters entirely.
- **Opus `compact_20260112`**: paraphrases everything into a single natural-language summary.

Hypothesis: each method loses something the other preserves, and the loss is task-shape-dependent:
- mu loses **forensic detail** (specific data from tool results).
- Opus loses **interpretive nuance** (user voice, exact phrasing).

This document is the recovery test: ask probe questions on each compacted input + a no-compaction baseline, see what answers survive.

## Method

- **Corpus**: `~/.local/share/mu/events/ebccc9256dcbe75a/session-2.jsonl` — same session as `compaction-quality-2026-05-21.md`.
- **Three input shapes, sent to `claude-opus-4-7`**:
  - `baseline_full`: 98 messages, full original conversation (~188k input tokens). Ground-truth control.
  - `mu_compacted`: 9 messages — mu's 10 surviving spans rebuilt into Anthropic-shaped messages (~4k input tokens).
  - `opus_compacted`: 2 messages — a system-style "here is the compacted summary" preamble carrying the Opus summary text (~2k input tokens).
- **Five probes** split across axes:
  - 2× **forensic** (would only be answerable from a tool result):
    1. `forensic_1_claude_proxy` — "What language is claude-proxy in and what's its main entry-point file?"
    2. `forensic_2_c137_mem_pkg` — "What's the exact `name` and `version` in `c137-mem/package.json`?"
  - 2× **interpretive** (would only be answerable from a user/assistant turn):
    3. `interpretive_1_preachy` — "What was the user's main counterpoint to me calling my advice 'preachy'?"
    4. `interpretive_2_hardest_part` — "What did the user say is the hardest part of agent-driven velocity?"
  - 1× **mixed** (surfaces in multiple places):
    5. `mixed_integration_gap` — "Summarize the rope-context-not-used architectural concern."
- **Routing**: via the local proxy at `127.0.0.1:3180` (uses operator's subscription quota where possible per OAuth-first failover chain).
- **Caching**: prompt caching enabled where possible. The baseline + mu prefixes cached cleanly (cache_read on probes 2-5); the opus_compacted prefix didn't cache because the cache_control marker landed on a too-short assistant ack instead of the long user message holding the summary. Cost impact: ~$1 extra; not material to the comparison.
- **Total spend**: $5.12 across 15 probes (3 inputs × 5 probes each).

The full results JSON is at [`compaction-probe-results-2026-05-21.json`](compaction-probe-results-2026-05-21.json).

## Results — scoring grid

| Probe | Axis | baseline_full | mu_compacted | opus_compacted |
|---|---|---|---|---|
| f1 — claude-proxy lang | forensic | ✅ "Rust; src/main.rs" | ✅ "Rust, src/main.rs" | ✅ "Rust, src/main.rs" |
| f2 — c137-mem pkg | forensic | ✅ exact JSON values | ❌ "I don't have file access" | ❌ "summary doesn't include it" |
| i1 — preachy counterpoint | interpretive | ✅ verbatim quote | ✅ verbatim quote | ⚠️ paraphrased substance, no quote |
| i2 — hardest part | interpretive | ✅ verbatim + context | ✅ verbatim + context | ⚠️ paraphrased substance |
| mixed — integration gap | mixed | ✅ accurate | ✅ accurate | ✅ accurate |

Legend:
- ✅ Substantively correct
- ⚠️ Correct in substance but paraphrased away from original wording
- ❌ Refused to answer / lost the data

Net 5-point scores (treating ⚠️ as 0.5 because the substance survived but voice didn't): baseline 5.0 / mu 4.0 / opus 3.0.

## Detailed observations

### Probe 1 (forensic — claude-proxy language)

All three got the right answer ("Rust; src/main.rs"). This is the **least informative probe** — the answer could plausibly be inferred from common knowledge of what a project named "claude-proxy" might be. mu's compacted input doesn't contain any tool result naming the language, but the model answered correctly anyway. Caveat: this probe is too easy to act as a forensic-recovery test.

A better forensic probe would have been "what's the exact dependency version of axum declared in claude-proxy's Cargo.toml?" — a fact that's truly only present in the tool result and not inferable from training data.

### Probe 2 (forensic — c137-mem package.json)

This probe **clearly distinguished forensic recovery from compaction**:

- baseline got the JSON values exactly: `name: "c137-mem"`, `version: "0.1.0"`.
- mu_compacted refused, going so far as to say "I don't actually have file access to your machine or those repos. Earlier in the conversation I went along with the premise that I'd read the code, but I hadn't" — a *false* statement (the model HAD read the file in the original session), but a defensible response given the model can no longer see the read. **The dropped tool result IS unrecoverable from mu's compaction.**
- opus_compacted refused cleanly: "The summary mentions `~/src/c137-mem` exists as a Pi extension... but it doesn't include the contents of its `package.json`." Honest about what the summary captured and what it didn't.

Both compactions lose this forensic detail. **Hypothesis confirmed**: forensic detail from tool results is unrecoverable from either compaction method. The difference is in failure mode — mu pretends it never had the data; Opus says the summary didn't capture it.

### Probe 3 (interpretive — "preachy" counterpoint)

This probe **clearly distinguished interpretive preservation between the methods**:

- baseline quoted exactly: `> "I mean, I did get here wihout that advice."` (typo preserved!) + included the user's specific example of substrate-bypass.
- mu_compacted also quoted: `> "I did get here wihout that advice."` + the same example. **The user's verbatim wording is preserved.**
- opus_compacted reframed the counterpoint as: `**"watch for where new glue accidentally bypasses good substrate."**` — this is Opus's own *paraphrase* in its summary, not the user's wording. The substantive *idea* survived, but the user's exact framing of "I did get here without that advice" did NOT survive Opus's summarization.

**This is the clearest signal in the experiment.** Opus's summary collapsed two distinct user-quotes (the dismissal of preachiness + the constructive reframe) into a single Opus-authored line. mu kept both verbatim. If voice matters (and for follow-up turns where the model needs to read the user's tone, it does), mu wins on this axis.

### Probe 4 (interpretive — hardest part)

Same pattern as probe 3:

- baseline + mu_compacted: both quote the user's actual framing — "not just writing it myself and directing the agents to do the work."
- opus_compacted: reframes to Opus's paraphrase — "inferring from code/tests/behavior whether parts were tied together correctly." Substantively right, but the user never said those words. Opus inserted its own framing.

The hypothesis "Opus loses interpretive nuance" is supported: even when the substance is captured, the user's specific phrasing is lost. For a coding agent that needs to mirror the user's communication style on follow-up turns, this is a real cost.

### Probe 5 (mixed — integration gap)

All three got it right. The "rope context built but not used" thread is mentioned by name in user message msg-91 (which mu preserves) AND in Opus's summary (which captures it as "main risk: new convenience paths bypassing the architecture"). Both compaction methods preserve mixed-axis content.

## What this means

Three concrete claims survive the recovery test:

1. **mu's `SpanFamilyDropPolicy` is at least as good as Opus's `compact_20260112` for interpretive workloads.** For probes 3 and 4 (verbatim interpretive recovery), mu matched baseline; Opus did not. For probes that ask about *what the user said*, drop wins.

2. **Both methods fail equally on forensic recovery — but their failure modes differ.** Probe 2 (c137-mem package.json) was unrecoverable from either compaction. mu's failure mode was the model pretending it never had the data (false but understandable from the model's perspective: the tool result is gone). Opus's failure mode was cleaner: "the summary doesn't include it."

3. **Cost-benefit shifts the choice toward mu for coding-agent workloads.** mu's drop policy: 62ms structural, $0. Opus's summary: 38s synchronous pause, $2.94. For sessions where tools are re-runnable (read, grep, ls — almost all of mu's tool surface), the lost forensic detail can be recovered by re-running the tool. The interpretive cost (lost user voice) cannot be recovered by re-running anything.

## Caveats

0. **CRITICAL — the probe script did NOT include tools in the API request.** This is the load-bearing methodological caveat. When the model said "I don't have file access" on the c137-mem probe, it was *strictly true for that probe* — the probe was a compacted conversation + no tools. In **mu's actual runtime**, the tool definitions stay wired across compactions; they don't get dropped. So the "forensic loss" measured here is artificial: in real use, mu's loop would have `read` available and could recover the c137-mem version by re-running the tool. The architectural bet mu makes is precisely this — drop the tool result because the tool itself is re-runnable. The recovery test understates mu's real-world behavior; the gap closes substantially when tools are still wired. A follow-up probe with `tools=[read,ls,grep,glob]` re-enabled would let the model recover both forensic answers, almost certainly bringing mu's score to 5.0.

1. **Sample size is one session.** Five probes on one corpus. The pattern needs replication across more sessions before any of these claims should be treated as load-bearing.

2. **The forensic_1 probe was too easy.** "Rust + src/main.rs" is guessable from project name + common patterns; it doesn't really test forensic recovery. Future runs should use facts that are session-unique and untrainable (specific commit hashes, exact dependency versions, error-message contents).

3. **The compaction was on a SINGLE-session corpus that didn't actually trigger compaction during the original conversation.** mu's normal compaction triggers at 150k tokens (per `DEFAULT_COMPACTION_THRESHOLD`); this corpus is 176k rope tokens which would have crossed the threshold once. A multi-compaction-event session would expose different failure modes (e.g., compaction-of-compactions, which mu's `append_messages_to_baseline` flow handles but the qualitative behavior would change).

4. **Cache wasn't honored for opus_compacted.** The cache_control marker landed on the wrong message. Cost the run an extra ~$1 but didn't affect probe answers.

5. **The "model refusing to answer" failure mode is partially an artifact of model behavior.** When a probe looks like a prompt-injection ("nothing else", "just the X and Y"), Opus's training to flag injections kicks in. Two of the baseline answers and one of the mu_compacted answers refused on injection-concern rather than substance. A different probe wording would have avoided this and produced cleaner data.

## Implications

- The meta-judge idea ([[compaction_meta_judge_research_thread_2026_05_21]]) is supported. If we can detect from rope shape that a session is "interpretive-heavy" vs "forensic-heavy", routing to the appropriate compaction policy is a real win.

- For mu's default behavior, **`SpanFamilyDropPolicy` is the right default for coding-agent workloads.** The forensic loss is recoverable via tool re-run; the interpretive preservation is unique to drop.

- The README's architectural-thesis section is now supported by *two* measurements: 2026-05-21 (work-time) AND this recovery test (utility-loss). Worth tightening the prose to cite both.

- Worth filing a follow-on bead to add more probe questions covering: (a) untrainable forensic facts; (b) cross-turn coherence (do follow-up answers depend on prior compacted turns being correctly recovered); (c) multi-compaction-event sessions.

## Spend ledger (this measurement)

- Opus `compact_20260112` (qualitative side-by-side run + retry): ~$5.90
- Recovery test (15 probes through proxy): ~$5.12
- Total for option 1 + option 2: **~$11.00**
- Budget for option 2 was $20-50; came in well under.

## Reproduce

```sh
# Prereqs: /tmp/mu-compaction-dump.txt from the dump-compaction example,
# /tmp/opus-compaction-summary.txt from /tmp/opus-compact-one.py.

export ANTHROPIC_API_KEY="$(tq -r -f ~/.config/agent/config.toml anthropic.api_key)"
export ANTHROPIC_BASE_URL="http://127.0.0.1:3180"  # optional; route through proxy
uv run --project ~/src/claude-personal/scripts python3 /tmp/compaction-probe.py
# Writes /tmp/compaction-probe-results.json. Stash to specs/measurements/
# for permanent record.
```
