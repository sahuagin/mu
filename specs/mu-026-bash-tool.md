# Spec: `bash` tool — controlled shell execution (Phase 1)

| field      | value                                          |
| ---------- | ---------------------------------------------- |
| spec_id    | mu-026                                         |
| status     | ready                                          |
| created    | 2026-05-10                                     |
| updated    | 2026-05-10                                     |
| authors    | tcovert + claude-personal (claude-opus-4.7)    |
| supersedes | none (resolves recon-bash.md Phase 1)          |

## Why

mu currently has read/write/ls/edit/grep/glob — a full filesystem
surface but no way to run *commands*. That blocks the obvious next
class of agent tasks: build, run tests, install dependencies, query
the system, run linters. The recon doc (`specs/recon-bash.md`) laid
out the design space and recommended a phased approach. This is
Phase 1.

Phase 1 ships a usable-but-bounded bash tool: an allowlist of
read-only-ish commands by default, with a deliberate "yolo" escape
hatch for unattended/trusted use. Phase 2 (future spec) adds
`session.input_required` approval flow + per-project config file.
Phase 3 (later) considers OS-level sandboxing.

CONVENTIONS apply.

## Scope

- **In:**
  - `mu-coding/src/tools/bash.rs` — `BashTool` with two modes.
  - **Strict mode (default).** Direct exec via
    `tokio::process::Command::new(prog).args(rest)`. No shell.
    Allowlist match against token-prefix. Metachar rejection.
    Env scrub. 60s timeout, 64KB output cap.
  - **Yolo mode.** Full `bash -c "<command>"`. No allowlist check,
    no metachar rejection, no env scrub. Timeout + output cap still
    apply (deny-of-service guardrail; can disable via config later).
  - **Baked-in default allowlist** of ~15 read-only commands. User
    extends via `--bash-allow <cmd>` (repeatable) on `mu serve`.
  - CLI surface: `--bash-yolo` (boolean), `--bash-allow <cmd>`
    (repeatable). Forwarded by `mu ask` to the spawned `mu serve`.
  - Tool emits standard `AgentEvent::ToolCallStarted/Completed` —
    no special audit code; events land in the event log
    automatically (mu-025).
- **Out:**
  - **Per-project / per-user config files.** Phase 2.
  - **Approval prompts (`session.input_required`).** Phase 2.
  - **OS sandbox / jail / container.** Phase 3.
  - **Pipe-aware allowlist (validate each stage of `a | b`).**
    Phase 2 if pipes prove load-bearing in strict mode.
  - **Stdin to spawned process.** v1: stdin is /dev/null.
  - **Concurrent bash calls.** v1: one at a time per session
    (enforced by AgentLoop's sequential tool execution; no extra
    locking needed).
  - **Working-directory sandboxing.** Bash runs in the daemon's
    cwd (or wherever the agent loop spawned things). Future spec
    if path-escape attacks become real.
  - **Resource caps beyond timeout + output size** (CPU, memory,
    file descriptors). Phase 3.

## Invariants

- **INV-1 (CONVENTIONS apply).**
- **INV-2 (yolo is explicit and loud).** Default mode is strict.
  Yolo only activates via `--bash-yolo` CLI flag, never implicitly.
  When active, the daemon emits a startup log line at WARN level
  and the bash tool's spec description includes "YOLO MODE" so the
  model knows.
- **INV-3 (no implicit shell in strict mode).** Strict mode never
  invokes `bash`, `sh`, or any shell. It parses argv via `shlex`
  and execs directly. Shell metas in the command string are a
  rejection condition.
- **INV-4 (env scrub in strict mode).** Strict-mode env strips any
  variable matching `^[A-Z0-9_]*(API_KEY|TOKEN|SECRET|PASSWORD)$`
  before spawn. Whitelisted: `PATH`, `HOME`, `USER`, `SHELL`,
  `TERM`, `LANG`, `LC_*`, `TZ`, `TMPDIR`, `PWD`. Anything else
  goes through unchanged (so e.g. `CARGO_HOME` works).
  Yolo mode passes the full env.
- **INV-5 (output cap is non-bypassable).** Even yolo mode caps
  output at 64KB combined (stdout + stderr). The cap is a memory-
  pressure / context-window concern, not a security one.
- **INV-6 (timeout is non-bypassable in v1).** 60s default,
  configurable per-call via tool args. Both modes enforce. Future
  spec for unbounded.
- **INV-7 (exit code is honored).** Non-zero exit → `is_error: true`
  in the tool result. Result content includes stdout, stderr,
  exit code, and elapsed time.

## Interfaces

### `BashTool`

```rust
pub struct BashTool {
    mode: BashMode,
}

pub enum BashMode {
    /// Allowlist-checked, direct-exec, scrubbed env.
    Strict {
        allowlist: Vec<Vec<String>>,  // tokenized argv prefixes
    },
    /// Full bash -c, full env. User opts in via --bash-yolo.
    Yolo,
}
```

Tool input schema:

```jsonc
{
  "command": "string",       // required; the command to run
  "timeout_secs": "integer"  // optional; default 60, max 600
}
```

Tool output:

```text
<stdout, capped at 64KB>
<if non-empty> stderr: <stderr, capped>
<if exit != 0> exit: <code>
elapsed: <ms>ms
```

### CLI surface

```text
mu serve [...existing flags...] [--bash-yolo] [--bash-allow <cmd>]...
mu ask   [...existing flags...] [--bash-yolo] [--bash-allow <cmd>]...
```

`mu ask` forwards the flags to its spawned `mu serve`.

### Factory

```rust
pub struct BashSettings {
    pub yolo: bool,
    pub extra_allow: Vec<String>,  // strings to parse + merge into default
}

// build_tools signature grows:
pub fn build_tools(
    names: &[String],
    bash: &BashSettings,
) -> Result<Vec<Arc<dyn Tool>>>;
```

### Default allowlist (baked)

```text
git status
git log
git diff
git show
git branch
git remote
git rev-parse
ls
pwd
cat
head
tail
wc
file
which
date
echo
uname
```

These were chosen as the read-only "where am I / what's the state /
what does this file say" set. The model uses them constantly for
orientation in unfamiliar projects.

Notably absent (require explicit opt-in via `--bash-allow`):
- `cargo build` / `cargo test` / `cargo check` — write to
  `target/`; build dirs can fill disk
- `git fetch` / `git pull` / `git push` — network + history
- `npm` / `pip` / `apt` / `pkg` — install operations
- `make` — arbitrary side effects per project
- `find` — read-only but `-delete`/`-exec` are footguns
- `rg` / `fd` — these have their own dedicated tools

The model is told via the tool description that `git status`
is allowed but `git push` is not, so it can plan accordingly.

## Behaviors

1. **B-1 (strict: allowlisted command runs):** `bash("git status")`
   in a real repo → exit 0, stdout has the status, `is_error: false`.
2. **B-2 (strict: not-allowlisted command refused):** `bash("rm
   /tmp/foo")` → `is_error: true`, message names the disallowed
   prefix and lists the option for adding to allowlist.
3. **B-3 (strict: extended allowlist):** `--bash-allow "cargo
   check"`; `bash("cargo check")` runs.
4. **B-4 (strict: shell metas rejected):** `bash("ls; rm -rf /")`
   → `is_error: true`, message names the offending character.
5. **B-5 (strict: env scrub):** spawned process's env does NOT
   contain `ANTHROPIC_API_KEY` even if the daemon's env had it.
6. **B-6 (strict: timeout):** `bash("sleep 5")` with
   `timeout_secs: 1` → `is_error: true`, message says "timed out
   after 1s".
7. **B-7 (strict: output cap):** `bash("yes")` (would produce
   infinite output) — capped at 64KB, result includes truncation
   marker. `is_error` may be true (timeout) or false (the cap was
   hit and we killed the process) — both acceptable.
8. **B-8 (strict: non-zero exit → is_error):** `bash("false")`
   → `is_error: true`, content includes `exit: 1`.
9. **B-9 (yolo: pipes work):** `BashSettings { yolo: true, ... }`,
   `bash("echo hi | tr a-z A-Z")` → exit 0, stdout contains "HI".
10. **B-10 (yolo: full env passes):** spawned process's env
    contains all daemon env vars (including the test-injected
    `MU_TEST_SECRET` if the test sets it).
11. **B-11 (token-prefix matching):** allowlist `git`; allows
    `git anything`. Allowlist `git status`; allows `git status -s`
    but not `git push`.
12. **B-12 (audit via event log):** A `mu serve` session with one
    `bash` call produces an event log containing both `ToolCall`
    (with the bash command in arguments) and `ToolResult` (with
    the captured output). Test via the existing `event_log`
    snapshot API.

## Acceptance

- New: `crates/mu-coding/src/tools/bash.rs`
- Modified: `crates/mu-coding/src/tools/mod.rs` — re-export
- Modified: `crates/mu-coding/src/serve/factory.rs` — build_tools
  signature grows `BashSettings`; bash arm
- Modified: `crates/mu-coding/src/bin/mu.rs` — `--bash-yolo`,
  `--bash-allow`
- Modified: `crates/mu-coding/src/ask.rs` — forward both flags
- `cargo build` clean.
- `cargo nextest run` passes (216 → ~228).
- Live: `mu ask --tools bash "run git status here"` works in
  this repo without yolo. With `--bash-yolo`, `mu ask --tools
  bash "echo hi | tr a-z A-Z"` works.

## Out-of-circuit warnings

- **OOC-1 (yolo is dangerous).** Don't enable yolo for any session
  driven by untrusted input (web pages, GitHub issues, agent-router
  delegating from a model whose prompt came from elsewhere).
  Documented in the tool description and the daemon's startup log
  line.

- **OOC-2 (allowlist isn't a sandbox).** Even strict mode can run
  `cat ~/.ssh/id_rsa` if `cat` is allowed and the file is readable
  by the daemon user. The allowlist gates *which programs run*,
  not *what files they touch*. True isolation requires Phase 3
  (jail / container).

- **OOC-3 (token-prefix has a class of misuses).** If the
  allowlist contains `git`, the agent can do `git push` — anything
  starting with `git`. Be aware of this when extending the allowlist.
  A future allowlist DSL could match argv 2+ to forbid specific
  sub-commands; v1 keeps it simple.

- **OOC-4 (env scrub is a regex, not exhaustive).** It catches
  common patterns. Variables with unusual naming (`MY_PROVIDER_KEY`
  without `API_KEY`/`TOKEN`/`SECRET`) leak. User can extend the
  scrub list in a future config; v1 is best-effort defense.

- **OOC-5 (output cap is per-call).** A model that calls bash 100
  times in a session can still produce 6.4MB of context. Per-session
  budget is a future hardening item.

## Prior work / context

- `specs/recon-bash.md` — Phase 1 design rationale.
- `specs/mu-025-…` (forthcoming, currently this evening's event-log
  commit) — bash events flow through the standard ToolCall/ToolResult
  event paths; no special audit code.
- mu-022/023/024 — the other Phase-1 tools.

## Changelog

- 2026-05-10 — initial draft (claude-personal).
