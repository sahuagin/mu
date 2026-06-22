# Path A loop — design + pre-registration

The profile-search sibling loop (`profile-search.sh`) is the **remaining glue** for
harness-model-fit Path A (see [`AGENTS.md`](./AGENTS.md)). This doc is the
pre-registration: the architecture, the terrain that shaped it, the objective, and
the first experiment + hypothesis — written before the first scored run, per the
methodology ("pre-register the design to disk before the first model call").

## What it does

```
fan out N profiles --> run the agentic-bench corpus per profile (agent-dispatch)
                   --> grade each answer (arch_score) --> aggregate a numeric
                   --> OBJECTIVE per profile --> select = argmax(score)
```

It is the **numeric** sibling of `scripts/orchestrator/orchestrate.sh`. The
orchestrator's CONVERGE/REVIEW stages grade *unquantifiable code diffs* with an LLM
judge / ci-aipr; profile search has a **number**, so a deterministic `argmax` is
strictly better and free (decided with `cc:a92625d4`, 2026-06-22). It REUSES, never
rebuilds:

| Piece | Reused from |
|---|---|
| worker (one hermetic model run) | `scripts/lib/agent-dispatch.sh` |
| task corpus | `agentic-bench/arch_cases/agentic_*.json` |
| grader | `agentic-bench/arch_score.grade_agentic` (via `grade_one.py`) |
| run discipline (RUN_DIR / provenance.jsonl / summary.md) | `orchestrate.sh` shape |

## Prior-art gate (`prior_art.py`) — run before recommending new work

A harness that reads a *moving* `main` can produce findings that are correct yet
**already addressed** — the analysis is right, it just arrived late. On 2026-06-22,
two of three findings (unbounded tool grep; `timeout` not reaping children) turned
out to be instances of already-tracked themes (`mu-bkjr`/`mu-gqi1`; `mu-e6qa`); only
one (addendum/sampling cross-provider coverage) was novel. Recommending the first two
as "new" would have duplicated existing work.

So the gate is a **standing discipline**: before any finding becomes a "build X"
recommendation (a new bead, a proposed PR), run

```sh
./prior_art.py "<finding description>" [keyword ...]
```

It fans out across the places work is recorded — **beads (open AND closed)**, **agent
memory**, **GitHub PRs (all states)**, and **jj history (main)** — and prints ranked
candidates plus a verdict (`POSSIBLE PRIOR ART` / `INCONCLUSIVE` / likely novel). A
failed channel reports as an error, never a silent zero (a silent-empty beads channel
once produced a false "novel" — see the channel's loud-empty guard). Treat a non-novel
verdict as "go read these and decide already-addressed | partial | novel," not a hard
block.

## A profile = one `*.env` file

Sourced in an isolated subshell (so catalog overlays don't leak between arms):

| var | req | meaning |
|---|---|---|
| `PROFILE_ID` | ✓ | label + per-profile result subdir |
| `PROVIDER` | ✓ | agent-dispatch provider (ollama / openrouter / vllm / claude-oauth) |
| `MODEL` | ✓ | model id |
| `THINKING` | | effort low\|medium\|high (default low) — **the effort knob** |
| `SYSPROMPT` | | path to a system-prompt-addendum file — **the addendum knob** |
| `TOOLS` | | mu tool CSV (default `read,grep,ls,glob`) |
| `MU_MODELS_*` | | catalog overlay (sampling / catalog-addendum) for openrouter/vllm |

## Terrain that shaped this (the expensive-to-rediscover bits)

1. **`agent-dispatch.sh` is the right driver, not `arch_bench.py`.** `arch_bench` is a
   *sweep* tool: its model set comes from `config_models.json` and it takes **no**
   `--model`/`--provider` flag, and its agentic path does **not** pass `--thinking`.
   A profile search needs per-profile single-model control *with* the effort knob —
   `agent_dispatch` gives exactly that (it reads `THINKING`/`SYSPROMPT`/`TOOLS` from
   scope). We reuse `arch_bench`'s *corpus* and *grader*, not its driver.
2. **Provider × knob × cost matrix** (verified against mu `crates/mu-ai/src/providers`):
   | backend | wire | catalog sampling/addendum | dispatch `SYSPROMPT` | cost |
   |---|---|---|---|---|
   | openrouter | OpenAI chat/completions | ✓ | ✓ | $ |
   | vllm | OpenAI chat/completions (composes `OpenRouterProvider`) | ✓ | ✓ | free* |
   | ollama | Anthropic Messages (composes `AnthropicProvider`, `/v1/messages`) | ✗ | ✓ | free |
   | openai | OpenAI **Responses** (typed `mu-openai` crate) | ✗ | ✓ | $ |
   | anthropic | Anthropic Messages | ✗ | ✓ | $ |
   The catalog `system_prompt_addendum` (slice #4) and `sampling` (mu-y8gp) live **only
   in `OpenRouterProvider`**, so they reach **only the OpenRouter-path providers
   (openrouter, and vllm via composition)** — *not* the predicate "OpenAI-compatible"
   (mu's `openai` provider speaks the Responses API and is a separate path). `ollama`
   composes `AnthropicProvider` (native `tool_use`, top-level `system`), so it gets
   neither. **But the provider-agnostic `--append-system-prompt` (dispatch `SYSPROMPT`)
   reaches every provider including ollama** — so the addendum *hypothesis* is testable
   **free on ollama**, even though the production catalog field only fires for the
   OpenRouter-path providers.
   - *Coverage gap (harness-fit follow-up):* the per-model addendum/sampling are
     OpenRouter-path-only — `openai`/`anthropic`/`ollama` models get neither. Covering
     them needs the field plumbed into those providers, or hoisted to a
     provider-agnostic layer.
   - *Dialect leak note:* the tool-dialect-leak failure (`<function=...>` as text) is an
     OpenAI-compat-dialect phenomenon. On ollama's current Anthropic-native `tool_use`
     path there is no text dialect to leak, so the leak hypothesis really only applies
     to the OpenRouter-path providers — exactly where the catalog addendum fires.
   - *vllm is free but **not currently running** (127.0.0.1:8000 dead); standing one up
     is the path to a free *catalog*-addendum / sampling test.
3. **`mu` in PATH is `emu`** — an auto-build launcher (`.mu/emu`). Running it can
   rebuild `target/release/mu` mid-experiment. The loop **pins a frozen copy** of the
   binary into `RUN_DIR/mu` so every arm sees byte-identical mu.
4. **The ollama box (10.1.1.143) is a SHARED resource.** Loading/warming/evicting a
   model evicts whatever other sessions are actively using. Do **not** run ollama
   profiles — and never set `WARMUP=1` — without **exclusive use** of the box
   (coordinate with the operator, who stops other models first). `WARMUP` defaults
   OFF for this reason. The free-but-uncontended alternatives are a local vllm
   server or a tiny openrouter run.
5. **ollama sampling on the wire reloads the model** (13+ min cold start). So a
   sampling grid does **not** belong on ollama — bake sampling into the Modelfile, or
   run the sampling grid on vllm/openrouter. This is why the first experiment varies
   **effort × addendum, not sampling** (correcting the original handoff default).
6. **Cost.** `agent_dispatch` uses `mu ask --bare`, which persists no event log, so
   per-run tokens aren't metered here. For free (ollama) runs cost = 0 and the
   objective is `pass_rate`. `pass_per_dollar` is wired but degenerate at $0; the
   billed-provider path (pull cost from the event log via mu-analytics, or from
   `arch_bench`'s OpenRouter usage report) is the documented next step.

## Objective

`OBJECTIVE=pass_rate` (default) or `pass_per_dollar`. `pass_rate = passes / graded`,
where graded excludes timeout/error runs (matching `arch_score.grade_row`). Selection
is `argmax(score)` with deterministic tie-breaks: score desc → fewer dialect leaks →
cheaper → profile id. Leaks and fabrications are reported alongside (the addendum
specifically targets leaks).

## First experiment (pre-registered)

`experiments/effort-addendum-ollama/` — **{effort low|high} ×
{addendum none|tool-dialect-nudge}** = 4 profiles, the full 12-case agentic corpus.

> **Backend/model pending operator coordination.** The `*.env` files are templates
> (they currently name `gpt-oss:20b`, the initial pick — *rejected*: don't load it on
> the shared box). Before the scored run, confirm with the operator: either (a)
> **exclusive** ollama use + the agreed model, or (b) a local **vllm** server, or (c)
> a tiny **openrouter** run. See terrain note #4 (shared box). The loop is validated
> (grading/argmax on synthetic + real data; live-dispatch mechanics via smoke); only
> a live *passing* run remains, which is what this scored run delivers.

- **Hypothesis H1 (addendum):** the tool-dialect nudge (`experiments/addenda/
  tool-dialect-nudge.txt`) reduces dialect leak and/or raises pass_rate vs the
  no-addendum control. (This is the agentic-bench founding signal — the benchmark was
  born from ~50% dialect-flake; if the rescue layer already catches it, expect a null
  result, which is still informative.)
- **Hypothesis H2 (effort):** higher effort raises pass_rate on multi-hop cases.
- **Controlled:** model + corpus + dispatch held byte-identical across arms; only the
  profile varies. Served model = whatever `--provider/--model` says (never trust model
  self-report).
- **Null results count.** The deliverable is the loop; an honest "no difference"
  selection is a valid outcome.

## Run

```sh
# smoke (loop mechanics, ~minutes):
CASE_LIMIT=1 CASES_GLOB="agentic_rust.json" RUN_DIR=/tmp/mh-smoke \
  ./profile-search.sh experiments/_smoke smoke

# addendum A/B on OpenRouter (BILLED ~pennies; no shared-box contention):
CASE_LIMIT=2 CASES_GLOB="agentic_rust.json" \
  ./profile-search.sh experiments/addendum-openrouter

# ollama grid (FREE, but EXCLUSIVE-USE ONLY — see terrain note #4):
WARMUP=1 ./profile-search.sh experiments/effort-addendum-ollama
# -> ~/metaharness-runs/run-<stamp>/summary.md  (leaderboard + WINNER)
```

**Billed providers:** the loop does not meter spend (`--bare` persists no event log),
so there is no in-loop `$` cap. Bound cost the only ways available: small `CASE_LIMIT`,
a tight `MAX_TURNS`, and a few profiles. Start tiny, read `summary.md`, then widen.

Knobs: `CASE_LIMIT`, `CASES_GLOB`, `PER_CASE_TIMEOUT`, `MAX_TURNS`, `OBJECTIVE`,
`WARMUP`, `MU_REPO`, `ARCH_BENCH`, `RUN_DIR`, `MU`.
