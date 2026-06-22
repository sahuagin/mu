# Implementation plan: make `mu` well-suited to all models

*Plan, 2026-06-21. Scope is the model-fit work from [findings.md](findings.md) §4.
No code changed yet. This is a planning artifact for review, not an approved spec —
when a phase is greenlit it should graduate to a numbered `mu-NNN` spec under
`specs/` and a claimed bead.*

> **Status 2026-06-22 — the on-the-wire phases are DONE.** The plumbing fork below
> is settled by terrain: `mu-vcbm` chose per-turn `effort` threading (≈ option A),
> and sampling resolves from the catalog at the provider (≈ option C). Landed to
> `main`: **effort → OpenRouter `reasoning`** (mu-13ve / #356); **tool-dialect rescue
> on the OpenAI wire** (mu-xblz / #357); **per-model sampling** (mu-y8gp / #357,
> clamped + finite-guarded). Remaining: per-model **system-prompt addendum** (modest
> now that rescue landed), **`discover()`→`t4c` parity**, and the **measured eval =
> meta-harness Path A** ([meta-harness-assessment.md](meta-harness-assessment.md)) —
> the headline next step. The numbered phases below are the original plan; the unified
> `GenerationProfile` framing is partly superseded by `mu-vcbm`'s per-turn `effort`
> param plus the catalog.

## Goal

`mu` already expresses model difference in **bookkeeping** (token accounting, turn
caps, output budgets). This plan extends it to express model difference in
**generation** (sampling, tool-call dialect, reasoning requests, prompt), so that a
cheaper/harness-sensitive model like GLM-5.2 can be driven well — without
special-casing it in code and without regressing Claude.

## Non-goals

- Not rewriting the agent loop or the provider trait's core shape.
- Not changing `mu`'s neutral-system-prompt philosophy (we *add* an optional
  per-model addendum; the base stays frontend-supplied).
- Not building the meta-harness search loop here — that's
  [a follow-on](meta-harness-assessment.md) whose prerequisite is this plan.

## Design principle: one resolved "generation profile," sourced from the catalog

The unifying idea. Today the catalog (`model_catalog.rs`) resolves per-model
*limits*; providers consult those only indirectly. We add a typed
**`GenerationProfile`** resolved per model from the catalog, and have **both** request
builders (`anthropic.rs`, `openrouter.rs`) consult it. This turns the descriptive
[`quirks` field](model-catalog-tooling.md) into behavior and gives every later item
(sampling, reasoning, rescue, prompt addendum) a single home instead of scattered
prefix-matching.

```
ModelCatalogEntry (+ new typed fields)  ──resolve_model──▶  GenerationProfile
                                                                  │
                  ┌───────────────────────────────────────────────┤
                  ▼                         ▼                      ▼
        anthropic request builder   openrouter request builder   loop (max_turns,
        (temp/top_p/top_k,           (temperature, top_p,         tool-menu,
         thinking effort)             reasoning req, rescue)      reasoning)
```

`GenerationProfile` (sketch):

```rust
pub struct GenerationProfile {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    pub min_p: Option<f64>,
    pub reasoning: ReasoningRequest,      // Inherit | Off | On | Effort(String)
    pub rescue_text_tool_calls: bool,     // run tool_dialect rescue on this model
    pub system_prompt_addendum: Option<String>,
    // tool-menu + max_turns can live here too, or stay where they are and be
    // overridden from here — see Phase 6.
}
```

Resolution precedence mirrors the existing catalog merge (built-in < generated <
operator `models.toml` < env), so an operator can override any field per model.
Backward-compat: keep a tiny parser that maps known `quirks` string tokens (e.g.
`"temp=0.6"`, `"rescue-text-tool-calls"`, `"thinking=off"`) into the profile, so
existing `quirks=[...]` entries gain effect without a schema migration — but typed
fields are the documented forward path.

### Plumbing decision (needs a call — see Open Decisions)

Recommended: **construct providers with the profile baked in** (option C). Providers
already hold `model`, `api_base`, and (Anthropic) `thinking_effort`; the
daemon's route/session layer already resolves the catalog per model
(`route_catalog.rs`, `serve/handlers/session.rs`). Attach the resolved
`GenerationProfile` at provider-construction time; the request builder reads
`self.generation`. `SwitchProvider` already carries a fresh provider instance, so a
mid-session model switch picks up the new profile for free. Least invasive; matches
existing patterns. (Alternative A — add a `&GenerationProfile` arg to
`Provider::stream` — is more explicit and easier to unit-test at the loop level, at
the cost of touching the trait and every impl. Decide in Phase 0.)

## Phases

Ordered by leverage ÷ risk. Phases 0–1 are the spine; 2–6 are independent leaves
that can land in any order once the spine exists.

### Phase 0 — `GenerationProfile` foundation *(spine; do first)*
- Define `GenerationProfile` + `ReasoningRequest` (likely in `mu-core`, next to
  `capabilities.rs`).
- Add typed fields to `ModelCatalogEntry` / `ModelRuleConfig` (`model_catalog.rs:42-65`)
  and resolve them in `resolve_model` (`:467-511`); add the quirk-token fallback parser.
- Pick the plumbing (C recommended); thread the resolved profile to provider
  construction. No behavior change yet (all fields `None`/`Inherit` → identical wire).
- **Tests:** resolution precedence; empty profile produces byte-identical requests to
  today (parity tests, mirroring the existing `yqeq6_parity_*` discipline).
- **Bead:** `mu-<slug>: generation-profile foundation`.

### Phase 1 — Sampling parameters to the wire *(highest leverage)*
- Anthropic: populate `with_temperature`/`with_top_p`/`with_top_k`
  (`mu-anthropic/src/request.rs:503+`, currently only called in a test at `:936`)
  from the profile.
- OpenRouter/OpenAI-compatible: add `temperature`/`top_p`/`min_p` to
  `build_request_body` + `build_request_body_from_projection`
  (`openrouter.rs:286-317`, `:491-508`).
- Seed catalog defaults: GLM tool-loop `temperature=0.6` (vendor guidance); leave
  Claude unset (provider default). Document the "tune temp *or* top_p, not both" GLM
  note in the seeded comment.
- **Tests:** profile → exact JSON fields present/absent; default-empty parity.
- **Bead:** `mu-<slug>: per-model sampling`.
- **Why first:** single change most likely to lift GLM tool-loop reliability; tiny
  surface.

### Phase 2 — Provider-agnostic tool-dialect rescue
- Lift `rescue_assistant_message` (today only `ollama.rs:190`) into the shared
  OpenAI-compatible `Done` assembly so `openrouter` and `vllm` get it too. Gate on
  `profile.rescue_text_tool_calls` so it's a no-op for models that never leak.
- The rescue is already conservative-by-construction (aborts on ambiguity/unknown
  tool); enabling it on more paths is low-risk.
- **Tests:** the existing `tool_dialect.rs` suite already covers the parser; add a
  provider-level test that an openrouter turn ending in dialect text gets rescued
  when the quirk is on, untouched when off.
- **Bead:** `mu-<slug>: tool-dialect rescue on openai-compatible providers`.

### Phase 3 — Reasoning-request policy (OpenAI-compatible path)
- `mu` already *parses* incoming `reasoning`/`reasoning_content` (`openrouter.rs:593`).
  Add the ability to *request* a thinking mode/effort from the profile
  (`ReasoningRequest`) in the request body (provider/model-specific field names —
  GLM thinking toggle, etc.). Anthropic already has effort via `apply_thinking`; wire
  it from the profile instead of only the `--thinking` flag.
- Decide per-model whether thinking is on for agentic work (quality) or off (cost).
- **Tests:** request body carries the directive; parity when `Inherit`.
- **Bead:** `mu-<slug>: per-model reasoning request`.

### Phase 4 — Per-model system-prompt addendum
- At the compose seam (`crates/mu-coding/src/serve/discovery_bootstrap.rs:66`),
  append `profile.system_prompt_addendum` when present. Keep the neutral base.
- Seed a default addendum for dialect-leaky models: *"Emit tool calls only through
  the function-calling interface; never write a tool call as plain text."* — attacks
  the dialect leak (Phase 2's problem) at the source, cheaper than rescuing after.
- **Tests:** addendum present iff profile set; cache-prefix stability respected.
- **Bead:** `mu-<slug>: per-model system-prompt addendum`.

### Phase 5 — Tool-menu shrinking for cheaper models
- Cheaper models degrade faster with big tool menus. Make a small curated menu the
  default for a non-frontier model class, reusing `FavoriteConfig.tools`
  (`model_catalog.rs:69-77`) and the `discover`/`t4c` intent-finder so the full menu
  is reachable on demand.
- Optionally fold `max_turns` override into the profile (note: OpenAI-style models
  already get 35 via `default_max_turns_for` — keep that, let the profile override).
- **Tests:** resolved tool set per model class; discover still reaches hidden tools.
- **Bead:** `mu-<slug>: per-model tool-menu defaults`.

### Phase 6 — Measured harness-fit eval *(the payoff; also the meta-harness seed)*
- Build a small harness that runs a fixed task corpus across
  `{model × profile}` and reads the **event log** (`ContextAssembly`, usage, tool
  outcomes) to score correctness, tokens, cost, and tool-call success rate.
- Output: a per-model "best profile" table with receipts — turns "which harness fits
  GLM-5.2" from vibes into measurement. Score *per dollar*, since mu logs cost.
- This is deliberately the skeleton of [meta-harness Path A](meta-harness-assessment.md)'s
  eval runner — build it so a propose→evaluate→store loop can wrap it later.
- **Bead:** `mu-<slug>: harness-fit eval corpus + scorer`.

## Cross-cutting / cleanup (fold into the phases above)
- **OnceLock staleness:** `global()` memoizes the catalog
  ([model-catalog-tooling.md](model-catalog-tooling.md) gap #4); document "restart the
  daemon after `mu models sync`," or add a refresh path if it bites.
- **Provider-switch resets:** the loop already drops the feedback anchor on
  `SwitchProvider` (`loop_/mod.rs:1002-1004`); confirm the new profile rides along.
- **Probed pricing (optional):** decide whether to add pricing fields to
  `ModelCatalogEntry` so `mu models sync` data stops being inert
  ([model-catalog-tooling.md](model-catalog-tooling.md) gap #1). Enables score-per-dollar
  in Phase 6 from probed (not hardcoded) numbers.
- **Probe sampling (optional):** the probe could capture provider-recommended
  sampling where exposed (ollama `parameters` already lists `temperature`), seeding
  Phase 1 defaults instead of hand-entering them.

## Dependency graph

```
Phase 0 (profile foundation)
   ├─▶ Phase 1 (sampling)          ┐
   ├─▶ Phase 2 (rescue)            │ independent leaves,
   ├─▶ Phase 3 (reasoning)         │ any order, parallelizable
   ├─▶ Phase 4 (prompt addendum)   │
   └─▶ Phase 5 (tool-menu)         ┘
                 └─▶ Phase 6 (eval) ──▶ (future) meta-harness Path A
```

## Risk register

| Risk | Mitigation |
|---|---|
| Regressing Claude / breaking wire parity | Every phase ships a "profile empty ⇒ byte-identical request" parity test, mirroring `yqeq6_parity_*`. |
| Sampling values wrong per model | Seed only well-sourced defaults (GLM 0.6); leave unknowns unset (provider default); make all operator-overridable. |
| Rescue false-positives on more providers | Rescue is already conservative (aborts on ambiguity/unknown tool); gate behind a per-model quirk so it's opt-in. |
| Catalog schema churn | Add fields as `Option`/`#[serde(default)]`; no `deny_unknown_fields`; quirk-token fallback avoids forcing a migration. |
| Daemon serves stale catalog after sync | Documented restart, or a refresh hook (cross-cutting). |
| Scope creep into meta-harness | Hard stop at Phase 6 (the eval); the search loop is a separate, reviewed effort. |

## Sequencing recommendation

If you want the 80/20: **Phase 0 + Phase 1 + Phase 2.** That converts `mu` from
"model-aware in bookkeeping" to "model-aware on the wire" — the axis that decides
whether GLM-5.2 reaches Opus-parity. Phases 3–5 are incremental polish on the same
seam; Phase 6 is the payoff and the bridge to self-improvement.

## Open decisions for Thaddeus

1. **Plumbing:** bake profile into providers (C, recommended) vs. add a
   `Provider::stream` arg (A, more explicit/testable)?
2. **Profile home:** extend `models.toml` (one file, matches today) vs. a separate
   `harness-profiles.toml` (cleaner separation, more files)?
3. **Pricing:** consume probed pricing now (unblocks score-per-dollar) or stay on the
   static table until Phase 6 needs it?
4. **Meta-harness:** do we want Path A on the roadmap, or is hand-tuned-per-model
   (Phases 1–5) enough for the model set we actually run? (Affects how much to invest
   in Phase 6's generality.)
5. **Beads/spec:** file a parent bead + per-phase children now, or wait until Phase 0
   is greenlit?

## How to verify, per phase

`just ci` (fmt + clippy + test) is the gate (AGENTS.md). Each phase adds parity +
behavior tests. For live model behavior, `MU_LIVE_ANTHROPIC=1` gates Anthropic; a
GLM/openrouter live smoke (`mu ask --provider openrouter --model <glm> --tools …`)
confirms the sampling/rescue path end-to-end before trusting the eval.
