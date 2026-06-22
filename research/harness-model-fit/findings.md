# Harness–model fit: findings

*Research report, 2026-06-21. Sources at the end. `mu` claims are terrain-checked
with file:line anchors; treat the external benchmark numbers as directional —
they come from secondary sources of varying reliability, but the magnitude and
direction are consistent across independent ones.*

> **Reconciled 2026-06-22.** The §4 "gaps" are now **closed in `main`**: per-turn
> effort threading landed via `mu-vcbm`; **mu-13ve** wired OpenRouter `reasoning`;
> **mu-xblz** added tool-dialect rescue on the OpenAI wire; **mu-y8gp** added
> per-model `temperature`/`top_p` (clamped + finite-guarded). Read §4's "sends no
> sampling params / rescue on one provider / `quirks` descriptive-only" as the
> *pre-merge* scorecard. Forward direction: the meta-harness Path A — see [README](README.md).

## Thesis

The "loop + tools" skeleton of an agent harness is the part that **doesn't** vary
much between harnesses. What makes the *same model weights* score very differently
across harnesses is the set of **adaptation layers** wrapped around the loop. A
model is post-trained against a particular set of those assumptions (Claude →
Claude Code; GPT → Codex; GLM → its own templates + Claude Code/Roo). A harness
"fits" a model when its adaptation layers match what the model was trained to
expect. The cheaper open models are *more* harness-sensitive than the frontier
ones — which is exactly why GLM-5.2 reaches Opus-parity "only with the right
harness": Opus brings its own robustness; GLM needs the harness to bring it.

## 1. The effect is real and large

Same weights, different harness:

| Model (weights fixed) | Harness A | Harness B | Swing |
|---|---|---|---|
| GPT-5.5 | 61.5% (native Codex) | 87.2% (Cursor) | **+25.7 pts** |
| Claude Opus 4.7 | 87.2% (Claude Code) | 91.1% (Cursor) | +3.9 pts |
| **GLM-5** | 40.4% | 48.3% (different wrapper) | **+7.9 pts** |
| GLM-5.1 | 63.5 (standalone) | 66.5 (Claude Code scaffolding) | +3.0 pts |
| Opus 4.6 | hand-designed harnesses | 76.4% (Stanford IRIS *Meta-Harness*) | beat all hand-built |

(Terminal-Bench-family numbers.) The load-bearing observations: (a) the harness can
move a score more than swapping the model does, and (b) **the spread is bigger for
the cheaper/open model.** Opus is relatively harness-robust; GPT and GLM swing
wildly. That asymmetry is the whole reason a cost-driven move to GLM lives or dies
on harness fit.

## 2. The mechanisms — why a harness fits one model and not another

For each: the knob, and how GLM-class open models differ from Claude.

**1. Tool-call wire encoding (the biggest one).** A model is RL-trained to emit
tool calls in a specific surface form. Anthropic models → the Messages API's
structured `tool_use` blocks. GLM/Qwen/DeepSeek → their *own* native templates,
often an XML-ish dialect (`<function=grep><parameter=pattern>…`) or a
`<tool_call>{json}</tool_call>` wrapper. When the serving stack's template parser
doesn't recover the native form, the model "leaks" an unexecuted tool call as plain
assistant text and the loop terminates thinking the turn is done. **`mu` has
already hit this exact bug** — it is the entire reason `tool_dialect.rs` exists
(qwen3-coder leaking XML at 50–75% of turns, measured 2026-06-04;
`crates/mu-ai/src/providers/tool_dialect.rs`). A harness that speaks only one wire
dialect and can't rescue/normalize the others *structurally cannot* drive those
models well.

**2. System-prompt shape and steerability.** Two sub-issues. *Where* it goes:
Anthropic has a top-level `system` field with `cache_control`; OpenAI-compatible
backends inline `{role:"system"}`; Codex's `instructions` silently truncates >8KB.
*What* it says: Claude Code's prompt is tuned to Claude's instruction-following
idioms — reusing it verbatim sometimes helps a foreign model (GLM-5.1: +3 with
Claude Code scaffolding) and sometimes fights it. Cheaper models often need more
explicit, more redundant instruction (e.g. "call tools via the function interface;
never write a tool call as text" — which, not coincidentally, attacks mechanism #1
at the source).

**3. Reasoning / thinking handling.** Reasoning models stream thinking on a side
channel (`reasoning_content`), and that thinking **counts against the output
budget**. If `max_tokens` is capped low, a reasoning model can burn the whole
budget thinking and emit *empty* content (`finish=length`) — `mu` measured exactly
this (gpt-oss:20b at a 4k cap; `output_limits.rs:48-56`). GLM has a *toggleable*
thinking mode and `tool_stream=true`; Anthropic uses adaptive extended thinking
with effort levels. Whether the harness reserves enough output budget, parses the
reasoning channel at all, and preserves vs. drops thinking across tool calls all
change behavior — differently per model.

**4. Sampling parameters (the most underappreciated).** Z.ai's published guidance:
GLM-5.2 defaults to `temperature=1.0, top_p=0.95`, but for **agentic tool loops
they recommend `temperature≈0.6`** for structured-tool-selection stability (general
tool-use best practice is 0–0.2). Claude is tuned to perform at its API defaults and
Anthropic recommends not fiddling. The "right" sampling config is **model-specific**,
and a harness that sends one fixed value — or none — leaves correctness on the table
for the model that wanted a different one. **`mu` currently sends no sampling
parameters at all** (see §4).

**5. Context management / compaction.** Models degrade differently as context grows
and have different effective (vs. advertised) windows. A compaction policy tuned to
one model's "what's safe to drop" can starve another. This is where `mu`'s receipts
(`ContextAssembly`, `CompactionAssembly`) are a real advantage — most harnesses
compact blind.

**6. Tool-menu size.** Smaller/cheaper models degrade faster with large tool menus
(selection errors, hallucinated args). Frontier models tolerate 15+ tools; a cheaper
model may want 5. A harness that can *shrink the menu per model* gets more out of
weak models — which is exactly what `mu`'s `discover`/`t4c` is for.

**7. Smaller but real:** error-feedback format, output budget, loop/turn cap
(OpenAI-style models empirically dispatch *more* tool calls per task, so they need a
higher cap — `mu` already encodes this), parallel-tool-call support.

## 3. The GLM specifics

- **Naming:** in mid-2026 the relevant models are **GLM-5.1 / GLM-5.2** vs **Opus
  4.8**. "GLM2.5" in the video was almost certainly **GLM-5.2**.
- **Cost:** Opus 4.8 = $5/$25 per M tokens (Fast Mode $10/$50). GLM-5.2 ≈ $1–2 in /
  $3–6 out per M, *or* flat subscription ($10/$30/$80 per month). Per-token that's
  roughly **¼–⅛** of Opus on output; on a coding-plan subscription for heavy use the
  *effective* ratio approaches **1/10**. "Almost 1/10" is fair.
- **It's the harness-sensitive one** (the 40.4→48.3 and 63.5→66.5 swings above).
- **Endpoint:** vendor-confirmed launch integrations (Claude Code, Cline, OpenCode,
  Roo Code, Goose, Crush, Kilo Code) all go through the **OpenAI-compatible
  endpoint** — which is `mu`'s `openrouter`/`vllm` path. (Aider and Cursor *lacked*
  native GLM wiring at launch — the harness has to be built for it.)
- **Knobs a good harness must expose for GLM:** `temperature≈0.6` for tool loops,
  thinking-mode toggle, `tool_stream`, and tolerance for its native tool-call dialect.

## 4. Where `mu` stands (scorecard)

`mu` is in much better shape than most harnesses — it built the right seams.

**Already there:**
- ✅ Clean provider seam: `Provider` trait + `capabilities()` → `ProviderCapabilities`
  (`crates/mu-core/src/agent/capabilities.rs`).
- ✅ Tool-dialect rescue exists, conservative-by-construction
  (`crates/mu-ai/src/providers/tool_dialect.rs`).
- ✅ Per-provider turn caps: `default_max_turns_for` (Anthropic 20, OpenAI 35,
  OpenRouter 30 — `crates/mu-core/src/agent/loop_/mod.rs:680`).
- ✅ Per-model output budgets (`output_limits.rs` + `models.toml`), with a bump for
  reasoning models.
- ✅ Per-provider usage accounting (`UsageSemantics`).
- ✅ Reasoning-channel handling (`reasoning`/`reasoning_content`; openrouter.rs).
- ✅ Rich per-model catalog (`model_catalog.rs`: context limits, max_output,
  `reasoning_in_output`, `quirks`, per-favorite `tools` / `default_effort`).
- ✅ Neutral system prompt — supplied by the frontend, not hardcoded to Claude.
- ✅ **The differentiator:** event-sourced receipts make harness-fit *measurable*.

**The gaps (terrain-verified):**
- ❌ **No sampling parameters ever reach the wire.** The `mu-anthropic` crate *has*
  `with_temperature`/`with_top_p`/`with_top_k` builders — but the only caller is a
  unit test (`crates/providers/mu-anthropic/src/request.rs:936`). The OpenRouter
  request body sets none. Every model runs at provider-default sampling; GLM never
  gets its recommended `temp≈0.6`.
- ❌ **Tool-dialect rescue is wired to one provider.** `rescue_assistant_message`
  is called only from `crates/mu-ai/src/providers/ollama.rs:190`. GLM/Qwen/DeepSeek
  served via `openrouter` or `vllm` — the paths those models actually arrive
  through — get no rescue.
- ❌ **`quirks` is descriptive-only.** It round-trips through resolution and display
  but no request builder reads it to change the wire request. It's the natural hook
  to make all of the above data-driven; see [implementation-plan.md](implementation-plan.md).
- ❌ **No per-model reasoning *request* policy** on the OpenAI-compatible path (it
  parses incoming reasoning but can't request a thinking mode / effort).
- ❌ **No per-model system-prompt adaptation** (neutral base is good; an optional
  per-model addendum would let us nudge dialect-leaky models at the source).

## 5. The reframe worth keeping

`mu`'s architecture already assumes models differ — it just currently expresses
that difference in **bookkeeping** (token accounting, turn caps, output budgets)
and not yet in **generation** (sampling, tool dialect, reasoning requests, prompt).
Close that gap and the "well-suited to all models" property mostly falls out of the
catalog `mu` already has. And because every call is logged, `mu` can do something
almost no other harness can: *prove* which configuration fits which model, with
receipts — which is the bridge to the [meta-harness assessment](meta-harness-assessment.md).

## Sources

- [GLM 5.2 vs Claude Opus 4.8 — Codersera](https://codersera.com/blog/glm-5-2-vs-claude-opus-4-8-coding-2026/amp/)
- [Agent Harnesses Beat Model Upgrades — MindStudio](https://www.mindstudio.ai/blog/agent-harnesses-beat-model-upgrades-5-benchmarks)
- [Why 70% of Your AI Agent's Performance Lives Outside the Model — Medium](https://medium.com/@tentenco/the-agent-harness-why-70-of-your-ai-agents-performance-lives-outside-the-model-5093cfe03df1)
- [Building Agentic Systems with Z.AI GLM-5 (thinking, tool calling, streaming) — MarkTechPost](https://www.marktechpost.com/2026/04/03/how-to-build-production-ready-agentic-systems-with-z-ai-glm-5-using-thinking-mode-tool-calling-streaming-and-multi-turn-workflows/)
- [Migrate to GLM-5.2 — Z.AI Developer Docs](https://docs.z.ai/guides/overview/migrate-to-glm-new)
- [GLM-5.1: Towards Long-Horizon Tasks — Z.ai](https://z.ai/blog/glm-5.1)
- [GLM-5 vs Claude Opus 4.6 — Creole Studios](https://www.creolestudios.com/glm-5-vs-claude-opus-4-6-performance-pricing-agentic-coding-comparison/)
- [AI Coding Agent Benchmarks — Artificial Analysis](https://artificialanalysis.ai/agents/coding-agents)
- Stanford IRIS Meta-Harness — [repo](https://github.com/stanford-iris-lab/meta-harness) · [paper](https://arxiv.org/abs/2603.28052) · [site](https://yoonholee.com/meta-harness/)
