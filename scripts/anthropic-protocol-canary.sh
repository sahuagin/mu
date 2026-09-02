#!/usr/bin/env bash
# anthropic-protocol-canary.sh — protocol-drift canary for the typed
# `mu-anthropic` crate, the sibling of scripts/openai-protocol-canary.sh
# (which was written in this crate's image but was the only one to get a
# cron wrapper — mu-canary-hardening-1ljgx closes that gap).
#
# What it does: replay a corpus of captured Anthropic Messages-API responses
# through the typed model via examples/drift_check.rs and NOTIFY if any
# deviates from what we model — a field the types drop on round-trip, or a
# content block of an unmodeled type. Plus the crate's own tests and a spec
# sanity check. Designed for cron; logs to a file and (optionally) files a
# bead.
#
# Usage:
#   anthropic-protocol-canary.sh [--live] [--alert=bead]
#
# Corpus: the crate's checked-in fixtures always (xz fixtures are
# decompressed to a temp dir first), plus every *.json under
# $MU_ANTHROPIC_CANARY_CORPUS (a dir of captured real responses) if set.
#
# Live checks (--live): run ONLY when ANTHROPIC_API_KEY is ALREADY in the
# invoking environment. Unlike the OpenAI sibling, this script never fetches
# the key from any config: the metered anthropic key is operator-controlled
# and usually does not exist (operator directive 2026-07-10/2026-08-31) —
# the operator exports it for the one invocation when a live run is wanted.
#
# Exit: 0 = clean, non-zero = drift / failure (also logged + alerted).

set -euo pipefail

# Cron-robust PATH.
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin:$PATH"

alert="none"
live=0
for arg in "$@"; do
  case "$arg" in
    --live) live=1 ;;
    --alert=*) alert="${arg#--alert=}" ;;
  esac
done

repo="${MU_REPO:-$(cd "$(dirname "$0")/.." && pwd)}"
crate="$repo/crates/providers/mu-anthropic"
spec="$crate/specifications/llms-full.txt.xz"
log="${MU_ANTHROPIC_CANARY_LOG:-$HOME/.local/share/anthropic-protocol-canary.log}"
log_prefix="[anthropic-protocol-canary]"
mkdir -p "$(dirname "$log")"

say() { echo "$log_prefix $*" | tee -a "$log"; }

failures=()
run_check() {
  local name="$1"; shift
  say "running $name"
  if ! "$@" >>"$log" 2>&1; then failures+=("$name"); say "FAIL $name"; fi
}

say "=== run $(date) on $(hostname); repo=$repo ==="

# (1) THE drift signal: replay every captured message through drift_check.
#     Exit 3 from the example means a modeled type dropped/changed a field or
#     an unmodeled block type arrived. xz fixtures are decompressed to a temp
#     dir (cleaned on exit) so the corpus is uniform plain JSON.
tmp="$(mktemp -d "${TMPDIR:-/tmp}/anthropic-canary.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
corpus=()
for f in "$crate"/tests/fixtures/*.json; do [ -e "$f" ] && corpus+=("$f"); done
for f in "$crate"/tests/fixtures/*.json.xz; do
  [ -e "$f" ] || continue
  out="$tmp/$(basename "${f%.xz}")"
  xz -dc "$f" > "$out"
  corpus+=("$out")
done
if [ -n "${MU_ANTHROPIC_CANARY_CORPUS:-}" ] && [ -d "$MU_ANTHROPIC_CANARY_CORPUS" ]; then
  while IFS= read -r f; do corpus+=("$f"); done \
    < <(find "$MU_ANTHROPIC_CANARY_CORPUS" -name '*.json' -type f)
fi
# drift_check parses single RESPONSE messages only (unlike the OpenAI
# sibling's example, which also takes stream captures) — filter the corpus
# to top-level `"type": "message"` documents so a stream capture or a cc log
# in fixtures/ doesn't read as false drift.
replay=()
for f in "${corpus[@]}"; do
  if jq -e '.type? == "message"' "$f" >/dev/null 2>&1; then
    replay+=("$f")
  else
    say "skipping non-response corpus file: $(basename "$f")"
  fi
done
corpus=("${replay[@]}")
if [ "${#corpus[@]}" -eq 0 ]; then
  # A blind canary must fail loudly, not pass vacuously or die on a usage
  # error from an argument-less drift_check invocation.
  say "FAIL drift_replay (corpus empty after filtering — no response-message fixtures to replay)"
  failures+=(drift_replay_empty_corpus)
else
  say "replaying ${#corpus[@]} captured message file(s) through drift_check"
  run_check drift_replay cargo run --quiet --manifest-path "$crate/Cargo.toml" \
    --example drift_check -- "${corpus[@]}"
fi

# (2) The typed model's own tests (round-trip + spec-exact + golden fixtures).
run_check mu_anthropic_unit cargo test --quiet --manifest-path "$crate/Cargo.toml"

# (3) Spec sanity: the vendored docs snapshot still has the surface we model.
run_check spec_stop_reason xzgrep -q 'stop_reason' "$spec"
run_check spec_event_name xzgrep -q 'content_block_delta' "$spec"

# (4) Optional live check against the real backend (gated; see header — the
#     key must already be exported by the operator, never fetched here).
if [ "$live" = 1 ]; then
  if [ -n "${ANTHROPIC_API_KEY:-}" ]; then
    # Two MU_LIVE_ANTHROPIC-gated tests exist under different name stems;
    # cargo test takes one filter, so run each explicitly.
    run_check live_anthropic env MU_LIVE_ANTHROPIC=1 \
      cargo test --quiet --manifest-path "$repo/Cargo.toml" -p mu-ai live_anthropic
    run_check live_text_smoke env MU_LIVE_ANTHROPIC=1 \
      cargo test --quiet --manifest-path "$repo/Cargo.toml" -p mu-ai live_text_smoke
  else
    say "ANTHROPIC_API_KEY not in environment; skipping live checks (operator-key-only by policy)"
  fi
fi

if [ "${#failures[@]}" -eq 0 ]; then
  say "ok"
  exit 0
fi

msg="Anthropic protocol canary failed: ${failures[*]}"
say "$msg"

if [ "$alert" = "bead" ] && command -v beads >/dev/null 2>&1; then
  url="${BEADS_REMOTE:-$(awk -F= '/^mu=/{print $2}' "$HOME/.config/beads/remotes.env" 2>/dev/null || true)}"
  if [ -n "$url" ]; then
    beads --url "$url" exec -- create \
      --title "$msg" \
      --slug anthropic-protocol-canary-drift \
      --type bug --priority P1 \
      --description "Anthropic protocol canary on $(hostname) detected: ${failures[*]}. See $log." \
      --actor anthropic-protocol-canary >/dev/null 2>&1 || true
  fi
fi
exit 1
