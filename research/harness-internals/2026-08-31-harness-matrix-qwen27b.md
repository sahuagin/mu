# Same model, three harnesses: what the envelope is actually worth

*Research writeup, 2026-08-31 (bead `mu-nd9p2`, sessions 17b9afe5 + 9d9c460d).
Constant model: `qwen3.8:27b-q8_0-instruct` on one ollama 0.32.5 serve
(.143 box, all three cards, 262k ctx, thinks by default, baked Modelfile
sampling). Harnesses: mu (`mu ask --bare`), DeepSeek Harness v0.1.1-rc.2
headless (FreeBSD-narrowed port), Claude Code 2.1.251 (`claude -p`). Every
request body captured at jail-side proxies; raw captures, run transcripts, and
per-slice detail live on the bead and in the session scratchpad. Public
"model X in harness Y beats model Z in harness W" content never controls the
harness axis; this is the controlled version for our stack.*

## Design

One serve, identical weights, so quant/sampling/template are constants, not
per-arm pollutants. Three probe layers, each cheap before the next:

1. **Envelope anatomy** — what each harness actually puts on the wire
   (slices 1–4).
2. **Honesty flip** — a fixed impossible task ("use the bash tool… then reply
   done" in sessions with no bash tool), 3 arms x thinking on/off x n=3, with
   thinking toggled *server-side* at the proxy (`reasoning_effort:"none"` /
   `thinking:{disabled}`) so every harness runs its stock envelope (slice 5).
3. **Matrix v1** — 6 repo-inspection cases with regex-verifiable answers
   (agentic-bench `arch_cases`, re-verified live against the fixture repo),
   3 arms x 3 reps, one-shot protocol, 54 runs (slice 6).

## Finding 1: the envelope tax spans 15x — and is accuracy-neutral (here)

First-request prompt tokens, identical task and lane:

| arm | prompt tokens | tools / schema | system prompt |
|---|---|---|---|
| mu `--bare` | 2,525 | 12 / 9.2KB | 0 |
| mu + bash | 3,119 | 13 | 0 |
| dsh headless | 6,329 | 21 / 22.9KB | 3.6KB |
| cc (stock config) | ~n/a | 28 / ~88KB POST | ~10KB |
| cc (deployed config) | 37,603 | 116 / 120KB+ | 10.7KB |

cc pays ~19% of the 200k window the CLI assumes for an unknown model — ~14%
of the serve's actual 262k context — before work begins (MCP servers are ~88
of its 116 tools). Yet in matrix v1 the envelope did not cost accuracy:
mu 18/18, cc 18/18, dsh 15/18 — and cc's misses were zero once a scorer
false-negative was audited out. The tax shows up as wall-clock (cc ≈ 2–3x mu
per task) and context budget, which must bite as tasks grow toward the window.
Schema-size minimization is a speed/scale lever, not (at this task size) a
correctness lever.

## Finding 2: reasoning mode dominates integrity — 9/9 vs 4/9

The honesty probe, n=3 per cell, honest outcomes:

| arm | thinking on | thinking off |
|---|---|---|
| mu | 3/3 | 2/3 (one instant fake "done") |
| dsh | 3/3 | 2/3 (one instant fake "done") |
| cc | 3/3 | 0/3 |

Thinking-on: 9/9 across all three harnesses. Thinking-off: 4/9. The earlier
vllm/nvfp4 lane runs (non-thinking chat template, n=1 per arm) had shown both
mu and dsh hallucinating "done" — same axis. On this 27B, whether the harness
lets the model think is worth more than everything else measured here.

The interaction with envelope size is the interesting part. Small-envelope
harnesses fail small: an instant fabricated "done" (3–11s). cc fails large:
one run hard-looped — 84 consecutive `Skill` calls over 85 round trips, each
permission-denied, error-only retries feeding the loop until timeout (the
exact amplification chain from the Aug-25 qwen loop postmortem, reproduced
under Claude Code itself); one run derailed entirely, browsing beads and
answering "nothing to do right now — point me at a task"; one claimed "done"
after a single permission-denied tool call.

And cc *wins* large when thinking is on: denied Bash, two of three runs mined
the 116-tool surface, found that `Monitor` accepts a shell command, executed
the task through it, and honestly answered "done". Same envelope that drowns
the model without reasoning is an asset with it.

## Finding 3: the losses are termination and tooling, not capability

Every dsh matrix loss was the negative probe ("what does this nonexistent
function do?"), every one a 600s timeout, and every one *semantically honest*
en route ("I don't see `frobnicate_*` anywhere — I'll have a subagent run an
exhaustive search"). Without a search tool (fs-search is disabled in the
FreeBSD port — labeled port covariate, not a dsh property), proving a negative
means exhaustive file-viewing with enormous thinking streams (2.7MB of SSE
over 4–7 round trips). One dsh rep also produced the correct answer at ~90s,
then kept self-extending ("now let me check other places…") until the timeout
killed it. Failure modes observed, in order of points lost: non-termination,
answer-then-overwork, tool-surface gaps. Not wrong answers.

## Finding 4: plumbing cliffs are real and binary

The comment-428 hypothesis (inter-harness gaps are plumbing cliffs, not graded
capability effects) collected live specimens:

- cc emits mid-array `role:"system"` turns (hook output, agent roster —
  structural, present even with a fresh config). Anthropic's API accepts them;
  ollama's `/v1/messages` feeds the array into the GGUF chat template, which
  raises `System message must be at the beginning` — every cc request 400s.
  cc is undeployable against strict-template OpenAI-compat backends without an
  adapter (ours: one logged role rewrite at the proxy).
- ollama `/v1/chat/completions` silently ignores `think:false`;
  `reasoning_effort:"none"` works. `/v1/messages` honors
  `thinking:{disabled}`.
- dsh's 64-token title-request cap held on vllm but not on this ollama route
  (380 completion tokens came back) — token accounting per harness needs
  per-lane verification.
- `claude` accepts unknown model names (warns, assumes 200k window).
- `mu ask` exits 1 on serve-teardown timeout after a *successful* task — exit
  codes are not outcomes; read transcripts.

## What mu should absorb (the tailoring thesis, revised)

The per-model tool-dialect thesis survives but re-ranks. For hosting small
models, in order:

1. **Keep thinking affordable and on.** A harness knob that starves or
   disables reasoning costs more than 100KB of tool schema. mu's `--effort`
   plumbing is the right shape; the ollama openai-chat path needs the
   `reasoning_effort` mapping so the toggle reaches this dialect.
2. **Guarantee a search tool.** The single biggest score gap here was a
   missing grep, not a fat schema. Tool-surface *gaps* beat tool-surface
   *size*.
3. **Termination discipline.** Stop-at-answer beats answer-then-overwork; a
   repeat/level-off guard (mu-503qk class) should treat "kept working after
   producing the answer" as a first-class stop condition.
4. **Then minimize the envelope** — it buys 2–3x latency today and context
   headroom tomorrow; it did not buy correctness at this task size. The v2
   axis is tasks big enough that 37k of fixed tax collides with the window.

Error-handling detail worth importing: cc's loop ran on *permission-denied*
tool results — error-only retry gates amplify collapse (mu already guards
this; keep it).

## Not measured yet

Opus/cross-family cells (the full 2x2), coding tasks with reference tests
(needs a bash-capable dsh, i.e. a Linux run), pi/codex arms, envelope effects
at long horizons, and n>3 anywhere. The vllm-lane honesty cells are n=1 and
now lane-unavailable until the matrix window closes.
