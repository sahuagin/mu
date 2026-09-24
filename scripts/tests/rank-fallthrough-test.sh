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
#      the normal retry keeps its meaning. Every exit 4 also names the account
#      on the caller's stderr, routed around or not.
#   3. a built mu really exits 4 on a cap (skipped without a build).
#   4. mu-spawn reports a capped lane by default (exit 4 with the reason) and
#      rotates past it only on the caller's opt-in, naming the lane it skipped,
#      the seat that answered, and that the operator must add credit.
#
# No model, no network: agent-role runs against a fixture roster, the predicate
# is sourced and called directly with a fake errlog, and mu-spawn runs against
# a stub roster and a stub mu.

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

# the REASON reaches the caller's stderr on every exit 4, routed around or not:
# a caller that does not route around (mu-spawn, the orchestrator) must not
# hand its model a bare exit code it will read as a broken tool
probe_msg() {  # $1=rc -> the caller-visible stderr
  ad_errlog="$TMP/err.log"; ad_tools="read,grep"
  : > "$ad_errlog"; ad_errmark=$(_ad_err_mark)
  printf '[thinking] the provider may be out of tokens\nprovider out of tokens: openrouter/x (plan unknown, reset time not reported): 402\n' >> "$ad_errlog"
  _ad_out_of_tokens "$1" "openrouter/x" 1 2>&1 >/dev/null
}
for optin in 1 0; do
  case "$(AGENT_DISPATCH_CAP_ROUTE_AROUND=$optin probe_msg 4)" in
    *"openrouter/x is OUT OF TOKENS"*"mu said: provider out of tokens: openrouter/x"*)
      printf '  ok   exit 4 names the account on stderr (opt-in=%s)\n' "$optin" ;;
    *) check "exit 4 names the account on stderr (opt-in=$optin)" "the reason line" \
         "$(AGENT_DISPATCH_CAP_ROUTE_AROUND=$optin probe_msg 4)" ;;
  esac
done
check "any other code says nothing about tokens" "" "$(probe_msg 1)"

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

# ---- 4. mu-spawn: a capped lane is REPORTED, and rotated only on opt-in ----
# The worker's stderr is what its parent model reads (serve/worker.rs). By
# default a capped worker exits 4 with the reason, and the parent decides; a
# worker session carries dm/spawn_worker whatever its grant, so the wrapper
# cannot declare it replay-safe. A caller that opts in gets the rotation, and
# the walk names the lane it skipped, the seat that answered, and the fix.
SPAWN="$TEST_DIR/../mu-spawn"
cat > "$TMP/agent-role" <<'STUB'
#!/bin/sh
printf 'openrouter capped-model\nopenrouter next-model\n'
STUB
cat > "$TMP/mu" <<'STUB'
#!/bin/sh
case " $* " in
  *" capped-model "*) echo 'provider out of tokens: openrouter/capped-model (plan unknown, reset time not reported): 402' >&2; exit 4 ;;
esac
echo 'the answer'
STUB
chmod +x "$TMP/agent-role" "$TMP/mu"
spawn() {  # [env...] -> stdout+stderr of an unpinned mu-spawn walk
  env -u AGENT_DISPATCH_CAP_ROUTE_AROUND AGENT_ROLE="$TMP/agent-role" MU="$TMP/mu" \
    AGENT_DISPATCH_NO_LEASE=1 TMPDIR="$TMP" "$@" sh "$SPAWN" --cwd "$TMP" 'do the thing' 2>&1
}
out=$(spawn); rc=$?
check "mu-spawn: by default a capped worker is not re-run" "4" "$rc"
case "$out" in
  *"capped-model is OUT OF TOKENS"*"tell the operator"*) printf '  ok   the default says out of tokens, not a generic failure\n' ;;
  *) check "the default says out of tokens" "OUT OF TOKENS line" "$out" ;;
esac
case "$out" in
  *"the answer"*) check "the default does not run the next rank" "no answer" "$out" ;;
  *) printf '  ok   the default does not run the next rank\n' ;;
esac
out=$(spawn AGENT_DISPATCH_CAP_ROUTE_AROUND=1); rc=$?
check "mu-spawn: with the caller's opt-in a capped rank 0 rotates to rank 1" "0" "$rc"
case "$out" in
  *"the answer"*"ran on openrouter/next-model (rank 1) after skipping: openrouter/capped-model (out of tokens)"*"operator needs to add credit"*)
    printf '  ok   mu-spawn names the capped lane, the seat that answered, and the fix\n' ;;
  *) check "mu-spawn reports the rotation" "skip + answered-by + add-credit lines" "$out" ;;
esac
# a write-capable worker may have already acted: no rotation, but the reason
out=$(spawn AGENT_DISPATCH_CAP_ROUTE_AROUND=1 MU_SPAWN_TOOLS=read,write); rc=$?
check "mu-spawn: a capped write-capable worker is not re-run" "4" "$rc"
case "$out" in
  *"capped-model is OUT OF TOKENS"*) printf '  ok   the refused rotation still says out of tokens\n' ;;
  *) check "the refused rotation still says out of tokens" "OUT OF TOKENS line" "$out" ;;
esac
# a pinned seat keeps exit 4 and its reason
out=$(spawn MU_SPAWN_PROVIDER=openrouter MU_SPAWN_MODEL=capped-model); rc=$?
check "mu-spawn: a pinned capped seat exits 4" "4" "$rc"
case "$out" in
  *"capped-model is OUT OF TOKENS"*) printf '  ok   the pinned seat says out of tokens\n' ;;
  *) check "the pinned seat says out of tokens" "OUT OF TOKENS line" "$out" ;;
esac

# ---- 5. a seat that ignores SIGTERM is killed, not waited on forever -------
# `claude -p` stuck in a futex wait ignored timeout's SIGTERM and held a gate
# for six hours. TIMEOUT_KILL_AFTER bounds that: the dispatch returns 124 soon
# after the deadline even when the child never exits on its own.
cat > "$TMP/mu-deaf" <<'STUB'
#!/bin/sh
trap '' TERM
sleep 20
STUB
chmod +x "$TMP/mu-deaf"
start=$(date +%s)
( TOOLS="read,grep" TIMEOUT=1 TIMEOUT_KILL_AFTER=1 MU="$TMP/mu-deaf" ERRLOG="$TMP/deaf.err" \
    AGENT_DISPATCH_NO_LEASE=1 agent_dispatch openrouter deaf "$TMP/agent-role" >/dev/null 2>&1 ); rc=$?
took=$(( $(date +%s) - start ))
check "a SIGTERM-deaf seat still times out (124)" "124" "$rc"
if [ "$took" -le 10 ]; then
  printf '  ok   and it is killed within the kill-after window (%ss)\n' "$took"
else
  check "a SIGTERM-deaf seat is killed promptly" "<= 10s" "${took}s"
fi

# A SIGKILL that is NOT the deadline's (an OOM kill) must stay 137, and any
# other code must pass through: only the deadline makes a timeout. The nested
# timeouts decide this from which deadline passed, never from elapsed time.
printf '#!/bin/sh\nkill -9 $$\n' > "$TMP/mu-oom"
printf '#!/bin/sh\nexit 3\n' > "$TMP/mu-three"
chmod +x "$TMP/mu-oom" "$TMP/mu-three"
seat_rc() {  # $1=stub mu -> agent_dispatch's rc
  ( TOOLS="read,grep" TIMEOUT=5 TIMEOUT_KILL_AFTER=1 MU="$1" ERRLOG="$TMP/rc.err" \
      AGENT_DISPATCH_NO_LEASE=1 agent_dispatch openrouter m "$TMP/agent-role" >/dev/null 2>&1 )
  printf '%s' "$?"
}
check "a SIGKILL before the deadline stays 137 (an OOM kill is not a timeout)" "137" "$(seat_rc "$TMP/mu-oom")"
check "an ordinary exit code passes through the timeouts" "3" "$(seat_rc "$TMP/mu-three")"
# TIMEOUT=0 means no deadline (mu-spawn --timeout 0): the kill timer must not
# turn it into a 30-second one
printf '#!/bin/sh\nsleep 2; exit 3\n' > "$TMP/mu-slow"; chmod +x "$TMP/mu-slow"
rc=$( ( TOOLS="read,grep" TIMEOUT=0 TIMEOUT_KILL_AFTER=1 MU="$TMP/mu-slow" ERRLOG="$TMP/rc.err" \
        AGENT_DISPATCH_NO_LEASE=1 agent_dispatch openrouter m "$TMP/agent-role" >/dev/null 2>&1 ); printf '%s' "$?" )
check "TIMEOUT=0 stays uncapped (not killed after the kill-after window)" "3" "$rc"

[ "$fail" -eq 0 ] || { printf 'rank-fallthrough-test: FAILED\n' >&2; exit 1; }
printf 'rank-fallthrough-test: all cases passed\n'
