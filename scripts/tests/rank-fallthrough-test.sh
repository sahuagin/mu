#!/usr/bin/env bash
# rank-fallthrough-test.sh — "one option being unavailable must not halt the
# system" (bead mu-cbmru).
#
# Two halves, both hermetic and model-free:
#
#   1. agent-role's ranks are CIRCULAR: asking for a rank past the end wraps
#      instead of dying, so a caller that wants rank 3 of a two-rank role gets
#      a usable target.
#   2. agent-dispatch's out-of-tokens predicate: a seat whose lane is capped
#      (subscription usage limit) or out of credit exits 75 — the existing
#      "seat never ran, try the next rank" contract that mu-spawn and the
#      review panel already walk — while a transient rate limit does NOT, so
#      the normal retry keeps its meaning.
#
# No model, no network, no mu binary: agent-role runs against a fixture roster,
# and the predicate is sourced and called directly with a fake errlog.

set -u
set -o pipefail

TEST_DIR="$(cd "$(dirname "$0")" && pwd)"
ROLE_BIN="$TEST_DIR/../agent-role"
DISPATCH="$TEST_DIR/../lib/agent-dispatch.sh"
[ -x "$ROLE_BIN" ] || { echo "rank-fallthrough-test: agent-role not found at $ROLE_BIN" >&2; exit 2; }
[ -f "$DISPATCH" ] || { echo "rank-fallthrough-test: agent-dispatch.sh not found at $DISPATCH" >&2; exit 2; }
command -v jq >/dev/null 2>&1 || { echo "rank-fallthrough-test: jq missing" >&2; exit 2; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
fail=0
check() {  # $1=label $2=expected $3=actual
  if [ "$2" = "$3" ]; then
    printf '  ok   %s\n' "$1"
  else
    printf '  FAIL %s\n       expected: %s\n       actual:   %s\n' "$1" "$2" "$3"
    fail=1
  fi
}

# ---- 1. circular ranks -----------------------------------------------------
cat > "$TMP/roles.toml" <<'ROLES'
[twoseat]
max_turns = 8

[[twoseat.ranked]]
provider = "openai-codex"
model = "gpt-6-astra"

[[twoseat.ranked]]
provider = "ollama"
model = "qwen3.8:27b-q8_0"

[empty]
max_turns = 1
ROLES
export AGENT_ROLES="$TMP/roles.toml"

check "rank 0 is rank 0" "openai-codex gpt-6-astra" "$("$ROLE_BIN" twoseat 0)"
check "rank 1 is rank 1" "ollama qwen3.8:27b-q8_0" "$("$ROLE_BIN" twoseat 1)"
check "rank 2 wraps to rank 0 (--wrap)" "openai-codex gpt-6-astra" "$("$ROLE_BIN" --wrap twoseat 2)"
check "rank 5 wraps to rank 1 (--wrap)" "ollama qwen3.8:27b-q8_0" "$("$ROLE_BIN" --wrap twoseat 5)"

# WITHOUT --wrap an out-of-range rank still fails: ai-review.sh asks for
# `code_review_leaf 1` and uses that failure to mean "no second leaf seat,
# use the hosted default". Wrapping by default would hand it rank 0 instead.
if out=$("$ROLE_BIN" twoseat 2 2>&1); then
  check "an out-of-range rank without --wrap fails" "non-zero exit" "exit 0 ($out)"
else
  printf '  ok   an out-of-range rank without --wrap still fails (the probe callers rely on)\n'
fi

# every rank, unranked call: unchanged (the walkers bound themselves by length)
check "no rank lists both, in order" \
  "openai-codex gpt-6-astra
ollama qwen3.8:27b-q8_0" "$("$ROLE_BIN" twoseat)"

# a role with nothing to wrap onto is still an error, loudly
if out=$("$ROLE_BIN" empty 0 2>&1); then
  check "a role with no ranks fails" "non-zero exit" "exit 0 ($out)"
else
  case "$out" in
    *"no ranked targets"*|*"not found"*) printf '  ok   a role with no ranks fails loudly\n' ;;
    *) check "a role with no ranks fails with a reason" "a reason" "$out" ;;
  esac
fi

# ---- 2. out of tokens => exit 75, by EXIT CODE ----------------------------
# shellcheck source=/dev/null
. "$DISPATCH"

# The route-around is a CALLER declaration, never an inference: the probes
# below set it the way the review panel does, and two cases prove the default.
export AGENT_DISPATCH_CAP_ROUTE_AROUND=1

probe() {  # $1=rc [$2=tools] -> "75" when routed around, else "pass"
  ad_errlog="$TMP/err.log"
  ad_tools=$(printf '%s' "${2-read,grep}" | tr -d '[:space:]')   # as agent_dispatch normalises it
  ro=1; case ",$ad_tools," in *,write,*|*,edit,*|*,bash,*) ro=0 ;; esac
  if _ad_out_of_tokens "$1" "seat/model" "$ro" 2>/dev/null; then printf '75'; else printf 'pass'; fi
}

# exit 4 is mu's structured "this lane is out of tokens" (ProviderUsageLimit)
check "exit 4 routes around" "75" "$(probe 4)"
check "an explicit empty grant still routes around" "75" "$(probe 4 '')"

# every other code keeps its meaning — no text anywhere in this decision
check "a generic failure does NOT route around" "pass" "$(probe 1)"
check "the spend ceiling (3) does NOT route around" "pass" "$(probe 3)"
check "a timeout (124) does NOT route around" "pass" "$(probe 124)"
check "an already-skipped seat (75) does NOT route around" "pass" "$(probe 75)"
check "success is never routed around" "pass" "$(probe 0)"

# a write-capable seat may already have acted: never claim "never ran"
check "a write-capable seat is NOT routed around" "pass" "$(probe 4 'read,write,bash')"
check "an edit-capable seat is NOT routed around" "pass" "$(probe 4 'read,edit')"
check "spaces in the grant do not hide write" "pass" "$(probe 4 'read, write')"
check "spaces in a read-only grant still route around" "75" "$(probe 4 'read, grep')"

# the caller must declare the task re-runnable
check "without the caller's opt-in, nothing is routed around" "pass" \
  "$(AGENT_DISPATCH_CAP_ROUTE_AROUND=0 probe 4)"
check "an unset opt-in is the same as off" "pass" \
  "$(AGENT_DISPATCH_CAP_ROUTE_AROUND= probe 4)"

# a declined cap keeps its loud failure: the auth classifier must not pick it
# up by the back door (it excludes 0/75/124 — and 4)
probe_auth() {  # $1=rc -> "75" | "pass"
  ad_errlog="$TMP/err.log"; ad_tools="read,grep"
  : > "$ad_errlog"; ad_errmark=$(_ad_err_mark)
  printf 'Error: provider: auth failed for this lane\n' >> "$ad_errlog"
  if [ "$1" -ne 0 ] && [ "$1" -ne 75 ] && [ "$1" -ne 124 ] && [ "$1" -ne 4 ] &&
     _ad_err_tail "$ad_errmark" 5 | grep -q 'provider: auth'; then printf '75'; else printf 'pass'; fi
}
check "a declined cap is not re-routed by the auth classifier" "pass" "$(probe_auth 4)"
check "a real auth failure still routes around" "75" "$(probe_auth 1)"

# ---- 3. mu really exits 4 on a cap (end to end, no network) ---------------
# The faux provider answers from a queue; a queued `usage_limit` entry makes it
# report the subscription cap the way the codex lane does. This is what makes
# the exit code trustworthy rather than a convention two files agree on.
# Only THIS build is asserted against: an installed `mu` on PATH may predate
# the contract, and skipping is right for it. A binary we just built and that
# still does not exit 4 is a regression, and the suite must fail — otherwise
# the e2e probe that makes the exit code trustworthy never actually gates.
MU_BUILT="$TEST_DIR/../../target/debug/mu"
if [ -x "$MU_BUILT" ]; then
  "$MU_BUILT" ask --bare --provider faux --model faux-usage-limit 'hi' \
    >"$TMP/mu.out" 2>"$TMP/mu.err"; rc=$?
  check "mu ask exits 4 on a provider usage cap" "4" "$rc"
  if [ "$rc" -ne 4 ]; then
    printf '       stderr tail: %s\n' "$(grep -viE 'WARN|INFO|\[mesh\]' "$TMP/mu.err" | tail -2 | tr '\n' ' ')"
  fi
else
  printf '  SKIP mu ask cap probe: no build at %s (cargo build -p mu-coding first)\n' "$MU_BUILT"
fi

[ "$fail" -eq 0 ] || { printf 'rank-fallthrough-test: FAILED\n' >&2; exit 1; }
printf 'rank-fallthrough-test: all cases passed\n'
