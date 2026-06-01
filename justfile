# mu — common workflows
#
# `just --list` shows what this project supports without reading scripts/ or README.
# Recipes are thin wrappers around the underlying scripts and cargo commands —
# they're the front door, not the enforcement. The PR-promotion gate stays in
# scripts/gh-wrapper (intercepts `gh pr create` and `gh pr ready`), so e.g.
# bypassing `just check` before `gh pr create` still trips the wrapper.
#
# bead: mu-7s3x

# Use bash so recipes can use ${@:2}, [[ ]], and other bashisms. just's default
# (sh / dash) is too lean for the pr recipe's positional-args forwarding.
set shell := ["bash", "-cu"]

# Default recipe: list every available recipe (same as bare `just`).
list:
    @just --list

# ── pre-PR gate ────────────────────────────────────────────────────────────

# Full pre-PR check: fmt + clippy + test. Mirrors CI.
check:
    ./scripts/pre-pr-check.sh

# Quick pre-PR check: fmt + clippy only (skip tests). Good for fast loops.
check-quick:
    PRE_PR_QUICK=1 ./scripts/pre-pr-check.sh

# Exactly what CI runs: fmt-check + clippy + test, fail-fast in CI order (mirrors .github/workflows/ci.yml; fmt is check-only, never edits files). bead: mu-608b
ci: fmt-check clippy test

# Pre-PR cross-provider review gate (bead mu-6qst): run CI, then have a
# SEPARATE-provider model review the diff before a PR — a third check on top of
# CI and the human/agent. Local only (needs provider auth + network; not a CI
# step). Verdict comes from the reviewer's stdout, not its exit code. Reviewer
# defaults to ollama/qwen3-coder:30b (local, free, non-Claude); use codex with
# MU_REVIEW_PROVIDER=openai-codex MU_REVIEW_MODEL=gpt-5.5 when its OAuth is
# healthy. Disagree with a REJECT via MU_REVIEW_OVERRIDE=1. See scripts/ai-review.sh.
ci-aipr: ci
    scripts/ai-review.sh

# ── individual cargo steps ────────────────────────────────────────────────

# Format every crate in place.
fmt:
    cargo fmt --all

# Check formatting without writing — same gate CI uses.
fmt-check:
    cargo fmt --all -- --check

# Clippy with -D warnings across the whole workspace.
clippy:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Run the test suite.
test:
    cargo test --workspace --all-features --no-fail-fast

# ── dev / smoke ───────────────────────────────────────────────────────────

# Faux-provider smoke for `mu ask` — no API key needed.
smoke prompt="hello":
    cargo run -q -p mu-coding --bin mu -- ask "{{prompt}}"

# Pass-through to `mu ask` with arbitrary flags.
ask *args:
    cargo run -q -p mu-coding --bin mu -- ask {{args}}

# Pass-through to `mu serve` (manual JSON-RPC session).
serve *args:
    cargo run -q -p mu-coding --bin mu -- serve {{args}}

# Print the version of each workspace crate.
versions:
    cargo run -q -p mu-coding --bin mu -- versions

# Defaults for `just tui`. Override per-invocation:
#   just provider=anthropic model=claude-opus-4-7 tui
provider := "openai-codex"
model := "gpt-5.5"

# Build mu + mu-tui and launch the TUI against the local mu binary.
# Sources every provider key we know about from ~/.config/agent/config.toml
# at launch. Missing keys are silently treated as empty (2>/dev/null ||
# true), so the daemon only sees env vars for providers you've actually
# configured — no spurious "key set but empty" confusion.
tui *args:
    cargo build --release --bin mu --bin mu-tui
    ANTHROPIC_API_KEY=$(tq -f ~/.config/agent/config.toml -r anthropic.api_key 2>/dev/null || true) \
    OPENROUTER_API_KEY=$(tq -f ~/.config/agent/config.toml -r openrouter.api_key 2>/dev/null || true) \
        ./target/release/mu-tui \
            --provider {{provider}} \
            --model {{model}} \
            --bash-yolo \
            --mu-binary ./target/release/mu \
            {{args}}

# Build mu + mu-solo and launch the standalone single-pane TUI. Same
# provider/model defaults as `just tui` — override per-invocation:
#   just provider=anthropic model=claude-haiku-4-5 solo
#   just provider=openrouter model=x-ai/grok-2-latest solo
solo *args:
    cargo build --release --bin mu --bin mu-solo
    ANTHROPIC_API_KEY=$(tq -f ~/.config/agent/config.toml -r anthropic.api_key 2>/dev/null || true) \
    OPENROUTER_API_KEY=$(tq -f ~/.config/agent/config.toml -r openrouter.api_key 2>/dev/null || true) \
        ./target/release/mu-solo \
            --provider {{provider}} \
            --model {{model}} \
            --bash-yolo \
            --mu-binary ./target/release/mu \
            {{args}}

# ── PR flow (jj-aware) ────────────────────────────────────────────────────

# scripts/gh-wrapper auto-runs pre-pr-check.sh at `gh pr create`, so don't
# pre-run `just check` — that'd just double the work. Use MU_SKIP_PR_CHECK=1
# to bypass (escape hatch in the wrapper).

# Push current jj @ as <bookmark> and open a PR. Extra args forward to gh pr create.
# [positional-arguments] preserves shell quoting on the forwarded args so titles
# like `feat(scope): foo` survive (parens would otherwise be re-tokenized as a
# subshell by the recipe's shell).
[positional-arguments]
pr bookmark *gh_args:
    @echo "==> bookmark $1 on @ → push → gh pr create"
    jj bookmark create "$1" -r @ 2>/dev/null || jj bookmark set "$1" -r @
    jj git push --bookmark "$1"
    gh pr create --base main --head "$1" "${@:2}"
