# mu-solo operator runbook

*How to drive `mu-solo`, the daily-driver single-pane TUI. Reconstructed from the
crate as of 2026-06-21 (jj `@-` 5d31f91a). **This is a map.** Flag/command/line
details were read out of the source by an exploration pass; if anything here
disagrees with `mu-solo --help` or the code, the code wins. Re-derive load-bearing
details before depending on them.*

Primary source files: `crates/mu-solo/src/bin/mu-solo.rs` (CLI), `…/src/app.rs`
(commands, keybindings, status bar), `…/src/config.rs` (config), `…/src/skills.rs`
(skills).

## 1. Launch

```sh
just solo [args]                         # build + run (recipe in justfile ~108-117)
cargo run -p mu-solo --bin mu-solo -- [args]
./target/release/mu-solo [args]          # after a release build
```

mu-solo spawns its **own** `mu serve` daemon (path via `--mu-binary`, default
`./target/release/mu`). It is a frontend; the daemon is the runtime.

### Flags (all optional; `bin/mu-solo.rs:24-89`)

| Flag | Meaning | Default |
|---|---|---|
| `--config PATH` | Alternate `solo.toml` | `~/.config/mu/solo.toml` |
| `--mu-binary PATH` | `mu` daemon binary | `./target/release/mu` |
| `--cwd PATH` | Daemon working directory | process cwd |
| `--provider NAME` | Initial provider | `openai-codex` (config default) |
| `--model NAME` | Initial model | `gpt-5.5` (config default) |
| `--bash-yolo` | Auto-approve bash (no prompt) | off |
| `--tools CSV` | Tool set | `read,write,edit,glob,grep,memory_recall,bash` |
| `--thinking LEVEL` | Extended thinking `low\|medium\|high\|xhigh\|max` (Anthropic) | off |
| `--effort LEVEL` | Initial `/effort` dial | `medium` |
| `--focus` | Start in focus mode | off |
| `-p, --profile NAME` | Load `[profile.<name>]` from `solo.toml` | use `[session]` |

Config precedence (low → high): built-in defaults < `solo.toml` < `MU_SOLO_*` env <
CLI flags (`config.rs`).

### Examples

```sh
# Anthropic Haiku in a specific repo
mu-solo --provider anthropic --model claude-haiku-4-5 --cwd /path/to/project

# GLM (or any model) via OpenRouter — the model-fit path
mu-solo --provider openrouter --model z-ai/glm-5.2 --tools read,edit,grep,bash

# Local Qwen coder via vLLM/ollama
mu-solo --provider ollama --model qwen3-coder:30b

# Reusable preset
mu-solo -p work
```

## 2. Slash commands (`app.rs:1525-1603`)

| Command | Args | Effect |
|---|---|---|
| `/status` | — | Provider, model, session/daemon id, version, token usage, cost |
| `/help` | — | Command list (+ model-visible skills) |
| `/quit` `/q` `/exit` | — | Leave (same as Ctrl-C) |
| `/effort [LEVEL]` | `low…max` or bare | Set/pick reasoning-effort dial |
| `/focus [on\|off\|toggle]` | optional | Suppress streaming preview |
| `/collapse [on\|off\|toggle]` | optional | Fold completed tool blocks (fullscreen) |
| `/provider [NAME]` | optional | Switch provider (picker if bare) |
| `/model [NAME]` | optional | Switch model (picker, scoped to provider) |
| `/config [get\|set] …` | see below | Read/write session config |
| `/btw <message>` | required | Side question via sidecar; main history untouched |
| `/cancel` | — | Abort in-flight provider call |
| `/clear` | — | Clear visible scrollback (event log untouched) |
| `/transcript [PATH]` | optional | Write semantic transcript to file |
| `/copy [last\|assistant\|user\|all]` | default `last` | Copy to clipboard |

`/config` examples: `/config get context.soft_limit`,
`/config set context.soft_limit 120000` (`app.rs:2601-2700`).

## 3. Switch provider / model mid-session

`/provider <name>` or `/model <id>` issues `session.set_route` to the daemon and takes
effect immediately (`app.rs:2553-2588`). Bare command → picker; explicit arg →
direct set (free-form ids bypass the picker, so any OpenRouter/vLLM model id works
even if it's not in the curated list).

Provider name normalization (`app.rs:218-229`): `anthropic*`→`anthropic_api`,
`openai*`/`codex`→`openai_codex`, `openrouter*`→`openrouter`, `vllm*`→`vllm`,
`ollama*`/`local`→`ollama`, `faux`→`faux`, anything else lowercased and used as-is.

Curated picker models include `claude-opus-4-8/4-7`, `claude-sonnet-4-6`,
`claude-haiku-4-5`, `gpt-5.5`, and OpenRouter ids like `anthropic/claude-opus-4.7`,
`openai/gpt-5.5`, `google/gemini-3.5-flash`, `x-ai/grok-4.3`,
`meta-llama/llama-4-maverick` (`app.rs:78-108`). The list is curated, not exhaustive —
type any id directly.

## 4. Skills

Loaded at startup from `.mu/skills/` (project) then `~/.config/mu/skills/` (global),
keyed by command name (`skills.rs:276-342`). Two formats: mu-native
(`skill.toml` + `body.md`) and legacy claude-code (`SKILL.md` with YAML
frontmatter). Invoke with `/<skillname> [args]`; the skill body is injected into the
prompt (hidden from scrollback) with a green activation banner; args are appended.
Invocation buffers until the current turn finishes streaming.

## 5. Approvals & safety

- **`--bash-yolo` / `[session] bash_yolo`** — auto-approve bash; otherwise the daemon
  prompts for approval (`bin/mu-solo.rs:57-58`, `config.rs:214-217`). The `--yolo`
  daemon cow is exactly this posture: treat it like handing the prompt a shell.
- **`[session] max_side_effects`** — a session-level ceiling enforced by the daemon at
  dispatch, independent of per-tool permission: `read_only < mutating < external <
  destructive < execute` (empty = unrestricted) (`config.rs:243-258`). Set
  `read_only` for a look-but-don't-touch session.
- *Uncertain:* the exact in-TUI approval prompt UX wasn't fully traced — approvals
  flow through the daemon's notification mechanism. Confirm against the daemon's
  `session.input_required` path if you need the precise interaction.

## 6. Keybindings (selected; `app.rs:1323-1608, 1741-1960`)

**Prompt:** `/` open command picker · `Enter` submit · `Shift/Alt/Ctrl/Meta-Enter`
newline (`Ctrl-J` fallback) · `Esc` clear · `Ctrl-C` quit · `Ctrl-U` kill line ·
`Ctrl-A`/`Home`, `Ctrl-E`/`End` · `Alt-←/→` word-move · `Alt-↑/↓` (or `Alt-K`/`Alt-J`)
select transcript block · `PageUp/PageDown` scroll (fullscreen).
**With a block selected (empty prompt):** `C` copy · `P` copy into prompt · `M`
maximize.
**Maximized / overlay:** arrows + `PageUp/PageDown`/`Space`, `Home`/`End`, `C` copy,
`Esc`/`Q` close, `Ctrl-C` quit.
**$EDITOR:** `/transcript` (or `Ctrl-S`) writes the semantic transcript and opens
`$VISUAL`/`$EDITOR`.

## 7. Config (`~/.config/mu/solo.toml`; `config.rs`)

```toml
[tui]                       # TUI-local, never sent to daemon
effort            = "medium"
focus_mode        = false
notifications     = true    # OSC 99 desktop alerts while unfocused

[session]                   # forwarded to mu serve
provider          = "openai-codex"
model             = "gpt-5.5"
tools             = "read,write,edit,glob,grep,memory_recall,bash"
bash_yolo         = false
mu_binary         = "./target/release/mu"
cache_ttl         = "1h"    # "5m" | "1h" (Anthropic)
thinking          = ""      # low|medium|high|xhigh|max or empty
max_side_effects  = ""      # read_only|mutating|external|destructive|execute

[autonomy]                  # autonomous-run grant (off by default)
enabled                          = false
max_iterations                   = 25
max_wall_clock_ms                = 3600000
max_total_tool_calls_in_autonomy = 500
allow_schedule_wakeup            = true

[profile.work]              # reusable preset: mu-solo -p work
provider = "anthropic"
model    = "claude-opus-4-8"
tools    = "read,write,edit,bash,grep"

[models]                    # model-id aliases
architect = "claude-opus-4-8"
swift     = "claude-haiku-4-5"
```

Env overrides use `MU_SOLO_<SECTION>__<KEY>` (e.g.
`MU_SOLO_SESSION__MODEL=claude-haiku-4-5`).

## Common recipes

- **Try a cheaper model on a real task:** `/provider openrouter` then
  `/model z-ai/glm-5.2` mid-session; watch the `/status` cost line. (Until the
  [model-fit plan](../research/harness-model-fit/implementation-plan.md) lands, mu
  sends provider-default sampling — GLM's recommended `temp≈0.6` for tool loops is
  not yet applied.)
- **Read-only investigation:** set `[session] max_side_effects = "read_only"`.
- **Give a new model real context limits:** add it to `~/.config/mu/models.toml`,
  run `mu models sync`, **restart** mu-solo (the daemon caches the catalog) — see
  [model-catalog-tooling.md](../research/harness-model-fit/model-catalog-tooling.md).
- **Overnight autonomous run:** set `[autonomy] enabled = true` with bounds; the loop
  honors them defensively from the session capability, not just config.

## Related

- [research/harness-model-fit/findings.md](../research/harness-model-fit/findings.md)
  — why model choice interacts with the harness.
- [research/harness-model-fit/model-catalog-tooling.md](../research/harness-model-fit/model-catalog-tooling.md)
  — how `mu models sync` feeds model metadata into a session.
