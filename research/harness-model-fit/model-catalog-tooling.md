# Model-catalog tooling: probe → generate → incorporate

*Terrain-checked against the tree as of 2026-06-21 (jj `@-` 5d31f91a). File:line
anchors are load-bearing; trust the code over this doc if they drift.*

> **Reconciled 2026-06-22.** The catalog now carries per-model **`temperature` /
> `top_p`** (`mu-y8gp`, on `ModelCatalogEntry` / `ModelRuleConfig` /
> `ResolvedModelSettings`), and OpenRouter/vLLM consume them — so the "mu sends no
> sampling params" gap is closed. The `quirks`-is-descriptive-only and inert-pricing
> notes below still hold; pricing matters for the meta-harness Path A's
> *score-per-dollar* (see [README](README.md)).

This answers the follow-up: *"there is already a `mu` tool which queries
providers/models and writes the information into a file — I don't know how or when
it runs, and how or if the data is incorporated after it's generated."*

## Short answer

- The tool is **`mu models sync`**. It is **manual** — nothing runs it
  automatically (not on daemon start, not on session create, no cron).
- It probes each configured provider's HTTP API and writes one file per provider:
  `~/.config/mu/models.generated.<provider>.toml`.
- The generated data **is** incorporated automatically at runtime — the loader
  merges those files on every catalog load, and the values flow through model
  resolution into a session's context limits. **No hand-copy step.**
- So it is a **closed loop at runtime, with a manual refresh trigger.** Two
  things leak out of the loop: probed **pricing** is written but never read, and
  the running daemon caches the catalog (`OnceLock`) so a fresh sync isn't seen
  until restart.
- It does **not** probe sampling parameters (temperature/top_p), and the
  `quirks` field it can carry is **descriptive-only** — nothing reads it to change
  wire behavior. Both matter for the [implementation plan](implementation-plan.md).

## The data flow

### Refresh (manual): `mu models sync`

1. **Operator runs `mu models sync`.** CLI at `crates/mu-coding/src/bin/mu.rs:266-341`,
   dispatched via `run_models` (`:638-659`) → `mu_coding::models_sync::sync(...)`.
2. **Selection comes from the operator's `models.toml` only.**
   `models_sync::sync` (`crates/mu-coding/src/models_sync.rs:101-119`) reads
   `~/.config/mu/models.toml` and builds a selection set of model ids/aliases via
   `catalog_sync::operator_selection` (`crates/mu-core/src/catalog_sync.rs:97-113`).
   If empty, it prints "nothing to sync" and exits. **Implication:** sync only
   *enriches models you've already named* in `models.toml`; it does not import the
   provider's whole catalog. Use `mu models list <provider>` to discover, then add
   the ones you want, then `mu models sync` to fill in their limits.
3. **Provider list** (`models_sync.rs:45-55`): openrouter if `OPENROUTER_API_KEY`
   set, vllm if `VLLM_API_BASE` set, ollama always. Override with `--provider`.
4. **HTTP probe** per provider (`mu-ai/src/catalog_probe.rs`):
   - **openrouter**: `GET {base}/api/v1/models` → `id`, `context_length` →
     `context_hard_limit`, `top_provider.max_completion_tokens` →
     `max_output_tokens`, `pricing.{prompt,completion}` → per-Mtok
     (`catalog_probe.rs:46-103`).
   - **vllm**: `GET {base}/v1/models` → `max_model_len` → `context_hard_limit`;
     no pricing (`:142-175`).
   - **ollama**: `GET {base}/api/tags` to list, then `POST {base}/api/show` per
     *selected* id → the baked `num_ctx` parameter → `context_hard_limit`
     (`:198-265`). Absent `num_ctx` → `None`, never fabricated.
   - **Not probed:** temperature / top_p / any sampling knob. The ollama
     `parameters` blob is scanned only for `num_ctx`.
5. **Intersect + write.** `build_generated_entries` keeps only probed models in the
   selection (`catalog_sync.rs:119-140`); `write_generated_provider` atomically
   writes `~/.config/mu/models.generated.<provider>.toml` with a
   `# GENERATED … DO NOT EDIT` header (`:142-200`). Unreachable providers are
   skipped, preserving their prior file (`models_sync.rs:158-162`).

### Consume (automatic, at runtime)

6. **Loader merges the generated layers.** `model_catalog::load(None)`
   (`crates/mu-core/src/model_catalog.rs:146-187`) merges, lowest → highest
   precedence:
   `config/models.default.toml` (built-in) **<** each `models.generated.*.toml`
   (globbed, `:117-134`, merged `:157-161`) **<** operator `~/.config/mu/models.toml`
   **<** `MU_MODELS_*` env. `global()` memoizes this in a `OnceLock` (`:352-354`).
7. **Route catalog enriches from the loaded catalog.** `RouteCatalog::from_env()`
   resolves `global()` and calls `resolve_model(model)` per model; the catalog's
   `context_soft_limit`/`context_hard_limit` **win** over hardcoded fallbacks
   (`crates/mu-core/src/route_catalog.rs:254-274`). Built at daemon startup
   (`serve/mod.rs:305-318`).
8. **Sessions resolve limits from the route catalog.** On `SessionCreated`,
   `resolve_context_limits` (`serve/handlers/session.rs:1645-1661`) reads the
   per-model limits and stamps them on the event log; they drive compaction.

**The closing link:** generated file → `load()` merge → `global()` →
`resolve_model` → `RouteEntry` limits → `resolve_context_limits` → compaction.
Nobody hand-copies anything.

## Operator commands

| Command | Effect |
|---|---|
| `mu models list <openrouter\|vllm\|ollama> [query] [--timeout 30]` | Discover models from a provider; prints, writes nothing (`bin/mu.rs:330-340`). |
| `mu models sync [--provider …] [--dry-run] [--timeout 30] [--config PATH]` | Probe + write `models.generated.<provider>.toml` for models named in `models.toml` (`bin/mu.rs:315-329`). |

(`mu capabilities discover <intent>` is unrelated — that's the tool/skill finder,
not model metadata.)

## What leaks out of the loop (gaps the plan cares about)

1. **Probed pricing is inert.** `GeneratedModelEntry` serializes
   `pricing_input_per_mtok` / `pricing_output_per_mtok`
   (`catalog_sync.rs:80-82`), but `ModelCatalogEntry` (`model_catalog.rs:42-52`)
   has **no pricing fields**, so on load those keys are silently dropped (no
   `deny_unknown_fields`). The `RouteEntry` pricing shown in the status bar comes
   from a *separate static table* `pricing::for_model` (`route_catalog.rs:340-341`),
   **not** from probed data. So syncing a new model gets you its context window and
   output cap live, but its cost still depends on a hardcoded table. *(Comment at
   `catalog_sync.rs:66-71` says capturing-now-consuming-later is intentional.)*
2. **No sampling params anywhere in the pipeline.** The probe doesn't read them and
   the catalog has no field for them. This is why every model currently runs at the
   provider's default temperature — see [findings.md](findings.md) §"sampling" and
   the plan's Item ①.
3. **`quirks` is descriptive-only.** It round-trips through `resolve_model`
   (`model_catalog.rs:505-507`) and into `RouteEntry.quirks` for display
   (`route_catalog.rs:330-334`), but **no request builder reads a quirk string to
   alter the wire request.** The behavior a quirk *describes* (e.g. "reasoning
   model, give it 16k output") is instead hardcoded by model-name prefix in
   `output_limits.rs:48-56`. Making `quirks` behavioral is the plan's Item ③.
4. **Stale-until-restart.** `global()`'s `OnceLock` means a running daemon won't
   see a freshly-synced file until it restarts.
5. **ollama runtime discovery has no limits.** `with_ollama_models`
   (`route_catalog.rs:170-207`) discovers *names* live at startup but leaves limits
   `None`; only `mu models sync` (or a hand `models.toml` entry) fills the real
   window.

## Per-model fields the catalog supports today

`ModelCatalogEntry` (`model_catalog.rs:42-52`): `model`, `family`, `label`,
`aliases`, `context_soft_limit`, `context_hard_limit`, `max_output_tokens`,
`reasoning_in_output`, `quirks`. Prefix-matched defaults via `ModelRuleConfig`
(`:54-65`). `FavoriteConfig` (`:69-77`) additionally carries per-model `tools`
and `default_effort`. The probed/generated layer carries only `model`,
`context_hard_limit`, `max_output_tokens` (+ inert pricing) — **never
`context_soft_limit`**, a deliberate safety invariant so a bad probe can't crank
compaction (`catalog_sync.rs:24-32`).

This is the substrate the [implementation plan](implementation-plan.md) extends —
sampling, behavioral quirks, and reasoning policy all want to live right next to
`max_output_tokens`.

## Provenance

Reconstructed by a code-reading agent on 2026-06-21 from `catalog_probe.rs`,
`catalog_sync.rs`, `model_catalog.rs`, `route_catalog.rs`, `models_sync.rs`,
`bin/mu.rs`, and `serve/handlers/session.rs`. Two flags the agent raised:
the `pricing::for_model` module itself wasn't opened (the "pricing is a separate
static table" claim is inferred from the struct lacking pricing fields + probed
keys being unread), and the "no auto-sync" finding is absence-of-evidence from
broad greps. Both are worth a 30-second confirmation before anyone leans hard on
them.
