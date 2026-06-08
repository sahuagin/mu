# AGENTS.md — `t4c` (tools4claude)

t4c is the discovery/capability surface for agents: **find** tools by intent,
learn their **calling conventions**, and **invoke** them. This file is the
"how do I add/change a tool" recipe so it doesn't get re-derived from the
source every time. The *design rationale* lives in the module docs
(`src/catalog.rs`, `src/cli.rs`); this is the operator-facing summary.

## The three files — don't confuse them

| File | Role | Who writes it |
|------|------|---------------|
| `src/config/curated.default.toml` | The **shipped catalog**, baked into the binary via `include_str!` (`catalog.rs`). The durable asset-as-config (mu-2332). | Hand-edited **here, in the crate**. Affects every host t4c lands on. |
| `~/.config/t4c/overrides.toml` | **Operator-local** catalog, layered *over* the shipped default (later source wins on collision — `registry.rs`). Personal / host-specific / niche tools go here. | Hand-edited per machine. **Not** clobbered by `discover`. |
| `~/.cache/t4c/registry.toml` (or `$T4C_SELF_CONFIG`) | **Generated** by `t4c discover` = `catalog ∩ installed`. A warm-start snapshot, not a source. | Machine output — **never hand-edit**; `discover` overwrites it. |

> Hand-editing the generated `registry.toml` is the `issues.jsonl` trap: the
> next `t4c discover` regenerates it from the catalog and silently drops your
> edit. Add tools to `curated.default.toml` (everyone) or `overrides.toml`
> (just you).
>
> Note: binaries built before the overrides/`.cache` split wrote the generated
> registry to `~/.config/t4c/registry.toml` and had no `overrides.toml`. If
> `overrides.toml` seems ignored, the installed binary predates the feature —
> rebuild + reinstall.

## Present/absent is automatic — "it just becomes correct in the right environment"

A capability is **present** iff `which(invoke[0])` resolves on *this* host
(`catalog.rs::is_installed`). The live registry is always `catalog ∩
what's-installed`, so an entry is absent where its binary isn't installed and
present where it is — **including inside a jail vs. on the host**. You never
encode "this needs a jail"; the environment intersection does it for you. A
catalog entry is portable metadata; correctness is per-environment.

## Adding a tool

1. **Pick scope:** everyone/everywhere → `curated.default.toml`; just me / this
   host → `~/.config/t4c/overrides.toml`. Same grammar either way. For the
   override case there's a copy-me, field-annotated starter at
   `.mu/defaults/t4c.overrides.toml` (`cp` it to `~/.config/t4c/overrides.toml`).
2. **Write the entry:**
   ```toml
   [[capability]]
   path    = "bash.<name>"          # dotted id, e.g. bash.jj.status
   summary = "one line — and any calling-convention gotcha (see bash.gh.pr for the pattern)"
   keywords = ["...", "..."]        # drive `t4c find <intent>`
   invoke  = ["<cmd>", "ARG", ...]  # invoke[0] = the present/absent probe AND the suggested call
   requires = []

   [capability.help]
   argv = ["<cmd>", "--help"]       # what `t4c help <path>` runs
   ai   = false                     # true iff the tool emits `--help-ai --json`
   ```
3. **The tool must answer its help argv** (`--help`, or `--help-ai --json` when
   `ai = true`) or `t4c help <path>` returns nothing useful.
4. **Editing `curated.default.toml` only:** bump the `caps.len() == N` count
   assertion in `catalog.rs` tests (and `chains.len()` if you touched a chain).
   `overrides.toml` entries don't affect that test.
5. **Verify:** `t4c find <intent>` ranks it · `t4c help bash.<name>` shows usage
   · `t4c run bash.<name> -- <args>` invokes it.

## Convention to copy: carry gotchas in `summary`

`summary` is where context-specific calling rules live, surfaced right at
discovery time. The reference example is `bash.gh.pr`: *"In jj sibling
workspaces there is no `.git`: always pass `-R owner/repo` … Avoid `gh pr merge
-d`."* Put the one thing a caller will trip over there.

## Durable-design vs. operator-local: the split

Keep portable, version-with-the-code facts (calling conventions, design) in the
catalog / these docs. Keep machine-specific facts (where a binary physically
lives, jail caveats) in **agent memory**, so the catalog stays portable.

Worked example — `webshot` (headless screenshot via playwright+firefox):
- Registered via `overrides.toml` (operator-local: the binary exists only in
  the aiteam jail, so `which("webshot")` is true there and false on the host —
  the entry is correct in both places with no jail-awareness in t4c).
- The deep caveats (shared browser dir, jail-scoping, `bash -c` reads no rc)
  live in agent memory `4c8088de`, not here.
