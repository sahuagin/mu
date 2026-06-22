# Path A loop — design + pre-registration

The profile-search sibling loop (`profile-search.sh`) is the **remaining glue** for
harness-model-fit Path A (see [`AGENTS.md`](./AGENTS.md)). This doc is the
pre-registration: the architecture, the terrain that shaped it, the objective, and
the first experiment + hypothesis — written before the first scored run, per the
methodology ("pre-register the design to disk before the first model call").

## What it does

It searches the **harness-fit KNOBS** (effort / addendum / sampling) for a model that
is **resolved by ROLE**, not hardcoded. Model *selection* is `agent-role`'s job (and
the benches behind it); this loop only tunes the harness to whatever model the role
resolves — which is what "harness-model-fit" means. Cross-*model* sweeps stay
arch-bench's job.

```
resolve model (agent-role) --> fan out N KNOB profiles --> run the agentic-bench
  corpus per profile (agent-dispatch; ollama HELD via with-ollama-lease, never
  evicted) --> grade (arch_score) --> aggregate a numeric OBJECTIVE --> argmax(score)
```

It is the **numeric** sibling of `scripts/orchestrator/orchestrate.sh`. The
orchestrator's CONVERGE/REVIEW stages grade *unquantifiable code diffs* with an LLM
judge / ci-aipr; profile search has a **number**, so a deterministic `argmax` is
strictly better and free (decided with `cc:a92625d4`, 2026-06-22). It REUSES, never
rebuilds:

| Piece | Reused from |
|---|---|
| model choice | `agent-role <role> <rank>` (`~/.config/mu/agent_roles.toml`) |
| ollama box coordination | `with-ollama-lease` (cooperative etcd mutex; never evict) |
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

## A profile = one `*.env` file (KNOBS only — never a model)

Sourced in an isolated subshell (so catalog overlays don't leak between arms). A
profile sets only knobs; the **model is the same across all profiles**, resolved once
by role:

| var | req | meaning |
|---|---|---|
| `PROFILE_ID` | ✓ | label + per-profile result subdir |
| `THINKING` | | effort low\|medium\|high (default low) — **the effort knob** |
| `SYSPROMPT` | | path to a system-prompt-addendum file — **the addendum knob** |
| `TOOLS` | | mu tool CSV (default `read,grep,ls,glob`) |
| `MU_MODELS_*` | | catalog overlay (sampling / catalog-addendum) for the OpenRouter-path |

The model comes from `ROLE` / `RANK` at the loop level (default `harness_fit` / `0`),
resolved via `agent-role`. **Never put a `PROVIDER`/`MODEL` in a profile** — to probe a
different model, re-point the role in `agent_roles.toml` (or pass `ROLE=`/`RANK=`).

## Terrain that shaped this (the expensive-to-rediscover bits)

1. **Resolve the model by role; dispatch via `agent-dispatch.sh`, not `arch_bench.py`.**
   Model choice is `agent-role`'s job (config in `agent_roles.toml`) — the loop never
   hardcodes a model. For the *run*, `arch_bench` is a *sweep* tool: its model set comes
   from `config_models.json`, it takes **no** `--model`/`--provider`, and its agentic
   path doesn't pass `--thinking`. The knob search needs single-model control *with* the
   effort/addendum knobs — `agent_dispatch` gives exactly that (it reads
   `THINKING`/`SYSPROMPT`/`TOOLS` from scope). We reuse `arch_bench`'s *corpus* and
   *grader*, not its driver.
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
4. **The ollama box (10.1.1.143) is SHARED — the loop holds it cooperatively.** It
   can't co-resident two large models, so loading a different model evicts whatever
   others are using (the DoS this loop must not cause). When the resolved provider is
   `ollama`, the loop **self-wraps under `with-ollama-lease`** (WAIT mode: acquire the
   `ollama-box` mutex, hold it for the whole sweep, release on exit) and resolves the
   **resident** model (`harness_fit` rank 0 = `qwen3.6:35b-a3b-q8_0`), so there's no
   load/evict thrash — it waits its turn and cooperates. No `with-ollama-lease` on PATH
   ⇒ the loop refuses to run ollama rather than go uncoordinated.
5. **ollama sampling on the wire reloads the model** (13+ min cold start). So a
   sampling grid does **not** belong on ollama — bake sampling into the Modelfile, or
   run the sampling grid on an OpenRouter-path role. This is why the first experiment
   varies **effort × addendum, not sampling** (correcting the original handoff default).
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

`experiments/effort-addendum/` — **{effort low|high} × {addendum none|tool-dialect-nudge}**
= 4 knob profiles, over the 12-case agentic corpus, on the model the `harness_fit` role
resolves (rank 0 = `qwen3.6:35b-a3b-q8_0`, the resident local agentic model — free,
held cooperatively via `with-ollama-lease`).

- **Hypothesis H1 (addendum):** the tool-dialect nudge (`experiments/addenda/
  tool-dialect-nudge.txt`) reduces dialect leak and/or raises pass_rate vs the
  no-addendum control. (Founding agentic-bench signal — born from ~50% dialect-flake;
  on ollama's Anthropic-native `tool_use` path the rescue/native path may already catch
  it, so expect a possible null, which is still informative.)
- **Hypothesis H2 (effort):** higher effort raises pass_rate on multi-hop cases.
- **Controlled:** model (role-resolved) + corpus + dispatch held byte-identical across
  arms; only the knob profile varies. Served model = whatever the role resolved (never
  trust model self-report; read it from `profile.json`).
- **Null results count.** The deliverable is the loop; an honest "no difference"
  selection is a valid outcome.

## Run

The model is whatever `ROLE`/`RANK` resolves; ollama is auto-held via `with-ollama-lease`.

```sh
# smoke (loop mechanics, ~minutes) — one rust case, default harness_fit role:
CASE_LIMIT=1 CASES_GLOB="agentic_rust.json" RUN_DIR=/tmp/mh-smoke \
  ./profile-search.sh experiments/effort-addendum smoke

# first experiment (FREE local; cooperatively leases the box):
./profile-search.sh experiments/effort-addendum
# -> ~/metaharness-runs/run-<stamp>/summary.md  (leaderboard + WINNER)

# same knob search on a different model — re-point the role, don't edit profiles:
ROLE=harness_fit RANK=1 ./profile-search.sh experiments/effort-addendum   # rank 1 = gpt-5.5 ($0 sub)
```

**Billed providers** (a role rank that resolves to `openrouter`): the loop does not
meter spend (`--bare` persists no event log), so there is no in-loop `$` cap. Bound
cost with a small `CASE_LIMIT`, a tight `MAX_TURNS`, and few profiles; never route a
subscription-covered model through openrouter (the documented $15.59 lesson — see the
`model-routing` skill). Start tiny, read `summary.md`, then widen.

Knobs: `ROLE`, `RANK`, `CASE_LIMIT`, `CASES_GLOB`, `PER_CASE_TIMEOUT`, `MAX_TURNS`,
`OBJECTIVE`, `MU_REPO`, `ARCH_BENCH`, `RUN_DIR`, `MU`.
