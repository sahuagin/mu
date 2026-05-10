# Spec: `mu serve` config (provider + tools)

| field      | value                                          |
| ---------- | ---------------------------------------------- |
| spec_id    | mu-009                                         |
| status     | ready                                          |
| created    | 2026-05-10                                     |
| updated    | 2026-05-10                                     |
| authors    | tcovert + claude-personal (claude-opus-4.7)    |
| supersedes | none                                           |

## Why

After mu-008, `AnthropicProvider` can do tool calls and `ReadTool`
exists, but `mu serve` still hardcodes `FauxProvider::echo()` with
`Vec::new()` for tools. mu-009 wires CLI flags into the daemon so
real provider + real tools can be selected at startup. mu-010 (next)
verifies the whole pipe runs end-to-end against live Claude.

This is intentionally a small spec. CONVENTIONS apply.

## Scope

- **In:**
  - `--provider <name>` flag on `Command::Serve`. Values:
    `faux`, `anthropic-api`. Default: `faux`.
  - `--model <name>` flag on `Command::Serve`. Default depends on
    provider: faux ignores it; anthropic-api defaults to
    `claude-haiku-4-5-20251001`.
  - `--tools <csv>` flag on `Command::Serve`. Default: empty. Values:
    `read` (more added by future specs). Comma-separated for
    multiple, e.g. `--tools read,write` (only `read` is recognized today).
  - Same flags pass through `Command::Ask` to the child `mu serve`
    subprocess. `mu ask --provider anthropic-api --tools read "..."`
    works end-to-end.
  - Small provider/tool factory in `mu-coding` mapping flag values
    to `Arc<dyn Provider>` and `Vec<Arc<dyn Tool>>`. Lives in
    `crates/mu-coding/src/serve/factory.rs`.
  - `serve::run` and `serve::serve_with_io` gain a `tools` parameter.
  - `dispatch::handle_create_session` uses the configured tools
    (currently hardcoded `Vec::new()`).
  - One new integration test verifying `mu ask --provider faux "..."`
    still works (smoke for the flag-passthrough). Existing tests
    continue to pass with default-arg shape.
  - One new integration test verifying that an unknown provider name
    surfaces as an `anyhow::Error` from `Command::Serve`'s arm
    rather than panicking.

- **Out:**
  - Provider config from a config file (TOML). v1 is CLI-only.
  - Per-session provider override via `CreateSessionRequest.provider`
    (the field exists in mu-001 but is currently ignored). Future
    spec wires it through.
  - OpenAI / OpenRouter providers. Future specs.
  - Tools beyond `read`. Future specs add them and update the
    factory's match.
  - mu-010's actual end-to-end live verification — that's its own spec.

## Invariants

- **INV-1 (CONVENTIONS apply).** See
  `specs/delegations/CONVENTIONS.md`.
- **INV-2 (unknown values fail clean).** Unknown `--provider` /
  `--tools` values produce a clear error message via
  `anyhow::bail!`. Don't silently fall back to defaults.
- **INV-3 (default behavior unchanged).** `mu serve` with no flags
  still uses FauxProvider::echo() and no tools. Existing
  serve_smoke/ask_smoke tests must continue passing without
  modification.

## Interfaces

```rust
// crates/mu-coding/src/serve/factory.rs

pub fn build_provider(name: &str, model: Option<&str>) -> anyhow::Result<Arc<dyn Provider>> {
    match name {
        "faux" => Ok(Arc::new(FauxProvider::echo())),
        "anthropic-api" => {
            let model = model.unwrap_or("claude-haiku-4-5-20251001").to_string();
            Ok(Arc::new(AnthropicProvider::from_env(model)?))
        }
        other => anyhow::bail!("unknown provider: {other} (expected: faux, anthropic-api)"),
    }
}

pub fn build_tools(names: &[String]) -> anyhow::Result<Vec<Arc<dyn Tool>>> {
    names.iter().map(|n| match n.as_str() {
        "read" => Ok(Arc::new(ReadTool::new()) as Arc<dyn Tool>),
        other => anyhow::bail!("unknown tool: {other} (expected: read)"),
    }).collect()
}

// Helper for parsing --tools csv:
pub fn parse_tools_csv(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}
```

`serve::run` signature update:

```rust
// before:
pub async fn run(provider: Arc<dyn Provider>) -> anyhow::Result<()>

// after:
pub async fn run(
    provider: Arc<dyn Provider>,
    tools: Vec<Arc<dyn Tool>>,
) -> anyhow::Result<()>
```

`serve::serve_with_io` likewise. `dispatch::handle_create_session`
takes tools (probably as `Arc<[Arc<dyn Tool>]>` so cloning per
session is cheap).

CLI shape:

```rust
Command::Serve {
    #[arg(long, default_value = "faux")]
    provider: String,
    #[arg(long)]
    model: Option<String>,
    #[arg(long, default_value = "")]
    tools: String,  // CSV; parsed via factory::parse_tools_csv
}

Command::Ask {
    prompt: String,
    #[arg(long, default_value = "faux")]
    provider: String,
    #[arg(long)]
    model: Option<String>,
    #[arg(long, default_value = "")]
    tools: String,
}
```

`ask::run` accepts the same flags and passes them to the spawned
`mu serve`.

## Behaviors

1. **B-1 (default flags unchanged).** `mu serve` with no flags
   creates a daemon with `FauxProvider::echo()` and no tools.
   Existing tests still pass.
2. **B-2 (anthropic-api flag works).** `mu serve --provider
   anthropic-api` (with `ANTHROPIC_API_KEY` set) creates a
   daemon with `AnthropicProvider`. Tested via factory unit tests
   (without actually calling the API).
3. **B-3 (read tool flag works).** `mu serve --tools read` creates
   a daemon whose sessions have `ReadTool` available. Verified by
   factory unit test (the integration test covering read+anthropic
   together is mu-010).
4. **B-4 (unknown provider errors clearly).** `mu serve --provider
   bogus` returns an `anyhow::Error` containing the string "unknown
   provider".
5. **B-5 (mu ask passes through flags).** `mu ask --provider faux
   "hello"` continues to work (echo via FauxProvider). Verified via
   integration test similar to existing ask_smoke.
6. **B-6 (factory unit tests).** `build_provider("faux", None)`
   succeeds. `build_provider("anthropic-api", Some("foo"))` succeeds
   when `ANTHROPIC_API_KEY` is set. `build_provider("bogus", None)`
   returns `Err`. Same shape for `build_tools`.

## Acceptance

- New file: `crates/mu-coding/src/serve/factory.rs`.
- Modified files:
  - `crates/mu-coding/src/serve/mod.rs` (re-export factory; update
    run/serve_with_io signatures)
  - `crates/mu-coding/src/serve/dispatch.rs` (use tools)
  - `crates/mu-coding/src/bin/mu.rs` (CLI flags for Serve and Ask;
    factory invocation)
  - `crates/mu-coding/src/ask.rs` (pass flags through to child)
  - `crates/mu-coding/tests/ask_smoke.rs` (cover at least one
    flag passthrough case; existing tests continue to pass)
- `cargo build` clean.
- `cargo nextest run` passes — every existing test plus the new
  factory tests + any new ask_smoke case.
- `mu serve --help` shows the new flags.
- `mu serve --provider bogus` exits non-zero with a useful error.

## Out-of-circuit warnings

- **OOC-1:** Updating `serve::run`'s signature breaks `bin/mu.rs`'s
  call site. Update both atomically.
- **OOC-2:** `Arc<[Arc<dyn Tool>]>` is the cheapest-clone shape for
  passing the tool list to per-session creation, but `Vec<Arc<dyn
  Tool>>` is also fine (clone is O(N) shallow Arc clones — a few
  pointer copies). Pick whichever feels simpler.
- **OOC-3:** When `--provider anthropic-api` is set but
  `ANTHROPIC_API_KEY` isn't, the error from
  `AnthropicProvider::from_env` should propagate cleanly via `?`.
  Don't double-wrap in another `anyhow::bail!`.

## Prior work

- mu-008 — Anthropic tool support.
- mu-007 — `read` tool.
- mu-005 — `mu ask`.

## Changelog

- 2026-05-10 — initial draft (claude-personal).
