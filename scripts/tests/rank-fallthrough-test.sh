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
#   6. a session given a role walks the role's ranks in-session, circularly,
#      and stops naming the role when none is left (skipped without a build).
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

# ---- 4. mu-spawn: a capped worker is REPORTED and starts over -------------
# The worker's stderr is what its parent model reads (serve/worker.rs). When a
# role-armed worker's session gives up (exit 4: every rank it could switch to
# in place ran out), the task starts over from the role's FIRST rank, skipping
# ranks it already dispatched (operator, 2026-09-28) — and says so, since the first
# attempt may already have acted. A model named outright has no role and just
# exits 4 with the reason. The opt-in 4->75 rotation still works.
SPAWN="$TEST_DIR/../mu-spawn"
cat > "$TMP/agent-role" <<'STUB'
#!/bin/sh
printf 'openrouter capped-model\nopenrouter next-model\n'
STUB
cat > "$TMP/mu" <<STUB
#!/bin/sh
# keep the prompt this run was handed, for the restart-marker check
prev=""; for a in "\$@"; do [ "\$prev" = "--prompt-file" ] && cp "\$a" "$TMP/last-prompt"; prev=\$a; done
case " \$* " in
  *" capped-"*) echo 'provider out of tokens: openrouter/capped (plan unknown, reset time not reported): 402' >&2; exit 4 ;;
esac
echo 'the answer'
STUB
chmod +x "$TMP/agent-role" "$TMP/mu"
spawn() {  # [env...] [-- mu-spawn args...] -> stdout+stderr of a mu-spawn run
  spawn_env=""; while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do spawn_env="$spawn_env $1"; shift; done
  [ "${1:-}" = "--" ] && shift
  # shellcheck disable=SC2086 — spawn_env is a word list of VAR=value on purpose
  env -u AGENT_DISPATCH_CAP_ROUTE_AROUND AGENT_ROLE="${SPAWN_ROSTER:-$TMP/agent-role}" MU="$TMP/mu" \
    AGENT_DISPATCH_NO_LEASE=1 TMPDIR="$TMP" $spawn_env sh "$SPAWN" --cwd "$TMP" "$@" 'do the thing' 2>&1
}
rm -f "$TMP"/mu-spawn.* "$TMP/last-prompt"
out=$(spawn); rc=$?
check "mu-spawn: a capped worker starts over and finishes" "0" "$rc"
# the restarted worker is TOLD it is a restart, ahead of the original task
case "$(cat "$TMP/last-prompt" 2>/dev/null)" in
  "[mu-spawn] This is a RESTART of this task. An earlier attempt on openrouter/capped-model ran out of tokens partway through"*"do the thing"*)
    printf '  ok   the restarted worker is told it is a restart, then given the task\n' ;;
  *) check "the restarted worker is told" "a RESTART note, then the task" "$(head -c 300 "$TMP/last-prompt" 2>/dev/null)" ;;
esac
check "mu-spawn leaves no prompt files behind" "" "$(ls "$TMP"/mu-spawn.* 2>/dev/null)"
case "$out" in
  *"capped-model is OUT OF TOKENS"*"openrouter/capped-model (role coding) ran out of tokens; starting the task over from the role's first rank (the first attempt may already have made changes; its stderr is in "*"the answer"*)
    printf '  ok   it says out of tokens, that it starts over, and why that may matter\n' ;;
  *) check "the restart is said" "OUT OF TOKENS + starting-over lines" "$out" ;;
esac
case "$out" in
  *"ran on openrouter/next-model (rank 1) after skipping: openrouter/capped-model (out of tokens)"*"operator needs to add credit"*)
    printf '  ok   and names the seat that answered and the fix\n' ;;
  *) check "the restart names the answering seat" "answered-by + add-credit" "$out" ;;
esac
out=$(spawn AGENT_DISPATCH_CAP_ROUTE_AROUND=1); rc=$?
check "mu-spawn: with the caller's opt-in a capped rank 0 rotates to rank 1" "0" "$rc"
case "$out" in
  *"the answer"*"ran on openrouter/next-model (rank 1) after skipping: openrouter/capped-model (out of tokens)"*"operator needs to add credit"*)
    printf '  ok   mu-spawn names the capped lane, the seat that answered, and the fix\n' ;;
  *) check "mu-spawn reports the rotation" "skip + answered-by + add-credit lines" "$out" ;;
esac
# a write-capable worker is not converted to 75 by the dispatcher (it may have
# acted), but the operator's rule still starts it over — and it says so
out=$(spawn AGENT_DISPATCH_CAP_ROUTE_AROUND=1 MU_SPAWN_TOOLS=read,write); rc=$?
check "mu-spawn: a capped write-capable worker starts over too" "0" "$rc"
case "$out" in
  *"may have already acted"*"starting the task over"*) printf '  ok   the write-capable restart says the first attempt may have acted\n' ;;
  *) check "the write-capable restart is said" "may-have-acted + starting-over" "$out" ;;
esac
# a model named outright has no role: exit 4 with its reason, no restart
out=$(spawn MU_SPAWN_PROVIDER=openrouter MU_SPAWN_MODEL=capped-model); rc=$?
check "mu-spawn: a model named outright exits 4" "4" "$rc"
case "$out" in
  *"capped-model is OUT OF TOKENS"*"starting the task over"*) check "an explicit model is not restarted" "no restart" "$out" ;;
  *"capped-model is OUT OF TOKENS"*) printf '  ok   the explicit model says out of tokens and is not restarted\n' ;;
  *) check "the explicit model says out of tokens" "OUT OF TOKENS line" "$out" ;;
esac
# a --rank 1 seat was still chosen from the role: out of tokens, it starts
# over from rank 0
# (the stub honours a rank argument, as agent-role does: `<role> <rank>` is one line)
cat > "$TMP/agent-role-r1" <<'STUB'
#!/bin/sh
ranks='openrouter next-model
openrouter capped-model'
if [ -n "${2:-}" ]; then printf '%s\n' "$ranks" | sed -n "$(( $2 + 1 ))p"; else printf '%s\n' "$ranks"; fi
STUB
chmod +x "$TMP/agent-role-r1"
out=$(SPAWN_ROSTER="$TMP/agent-role-r1" spawn -- --rank 1); rc=$?
check "mu-spawn: a capped --rank 1 seat starts over at rank 0" "0" "$rc"
case "$out" in
  *"provider=openrouter model=capped-model"*"starting the task over"*"provider=openrouter model=next-model role=coding rank=0"*"the answer"*)
    printf '  ok   and rank 0 answers\n' ;;
  *) check "the --rank restart runs rank 0" "rank 0 after the restart" "$out" ;;
esac
# the roster fails to resolve at the restart: that is a configuration fault,
# said as one — not "every rank ran out" (exit 4)
cat > "$TMP/agent-role-flaky" <<STUB
#!/bin/sh
if [ -e "$TMP/roster-asked" ]; then echo "agent-role: roster unreadable" >&2; exit 1; fi
: > "$TMP/roster-asked"
printf 'openrouter capped-model\nopenrouter next-model\n'
STUB
chmod +x "$TMP/agent-role-flaky"; rm -f "$TMP/roster-asked"
out=$(SPAWN_ROSTER="$TMP/agent-role-flaky" spawn); rc=$?
check "mu-spawn: a roster that fails at the restart exits 2, not 4" "2" "$rc"
case "$out" in
  *"could not resolve role coding to start over"*"exit 1, resolver failed"*"the task is NOT restarted"*) printf '  ok   and says the roster failed\n' ;;
  *) check "the failed restart roster is said" "could-not-resolve line" "$out" ;;
esac

# mu-s3uae: a claude rank that did not return success starts the task over on
# the next rank, like a capped mu lane — `claude -p` has no exit code for a
# usage limit, so its 4 is not read as out of tokens (no "add credit" note);
# it is just a rank that did not answer.
mkdir -p "$TMP/claude-bin"
printf '#!/bin/sh\nexit 4\n' > "$TMP/claude-bin/claude"; chmod +x "$TMP/claude-bin/claude"
printf '#!/bin/sh\nprintf "claude-oauth claude-x\\nopenrouter next-model\\n"\n' > "$TMP/agent-role-claude"
chmod +x "$TMP/agent-role-claude"
# without the caller's route-around opt-in, agent-dispatch returns the raw
# code and mu-spawn starts the task over with the restart notice — the same
# path as a mu lane out of tokens, minus the out-of-tokens label
rm -f "$TMP/last-prompt"
out=$(PATH="$TMP/claude-bin:$PATH" SPAWN_ROSTER="$TMP/agent-role-claude" spawn -- --tools read,write,edit,bash); rc=$?
check "mu-spawn: a claude rank that fails starts the task over on the next rank" "0" "$rc"
case "$out" in
  *"claude-oauth/claude-x (role coding) did not return success (exit 4); starting the task over from the role's first rank (the first attempt may already have made changes; its stderr is in "*"the answer"*)
    printf '  ok   it says the claude rank failed and that it starts over\n' ;;
  *) check "the claude restart is said" "did not return success ... starting the task over" "$out" ;;
esac
case "$(cat "$TMP/last-prompt" 2>/dev/null)" in
  "[mu-spawn] This is a RESTART of this task. An earlier attempt on claude-oauth/claude-x did not return success (exit 4) partway through"*"do the thing"*)
    printf '  ok   the replacement is told the earlier attempt may have acted\n' ;;
  *) check "the claude restart notice" "a RESTART note naming exit 4, then the task" "$(head -c 300 "$TMP/last-prompt" 2>/dev/null)" ;;
esac
case "$out" in
  *"out of tokens"*|*"OUT OF TOKENS"*) check "a claude exit 4 is not out of tokens" "no credit note" "$out" ;;
  *) printf '  ok   and is not taken for out of tokens\n' ;;
esac

# a single-rank claude role whose worker fails after (possibly) acting: the
# failure is the result, its own code — never 75 ("no usable seat")
printf '#!/bin/sh\nexit 3\n' > "$TMP/claude-bin/claude"; chmod +x "$TMP/claude-bin/claude"
printf '#!/bin/sh\nprintf "claude-oauth claude-x\\n"\n' > "$TMP/agent-role-claude-only"
chmod +x "$TMP/agent-role-claude-only"
out=$(PATH="$TMP/claude-bin:$PATH" SPAWN_ROSTER="$TMP/agent-role-claude-only" spawn -- --tools read,write,edit,bash); rc=$?
check "mu-spawn: a lone claude rank that fails is the result, not 'no usable seat'" "3" "$rc"
case "$out" in
  *"ran and did not return success (exit 3; its stderr is in "*") and no other rank answered; that is the result"*) printf '  ok   and says so, and where the stderr is\n' ;;
  *) check "the lone claude failure is said" "ran and did not return success (exit 3) ... that is the result" "$out" ;;
esac
printf '#!/bin/sh\nexit 4\n' > "$TMP/claude-bin/claude"; chmod +x "$TMP/claude-bin/claude"

# agent-dispatch hands a failed claude seat's code back AS IS — never a 75,
# which the panel's census would read as a deliberate skip with no diagnostic
# (PR #611). Walking on is the caller's decision (mu-spawn above).
claude_probe() {  # $1=opt-in ("1"|"") $2=tools -> exit code of agent_dispatch
  ( PATH="$TMP/claude-bin:$PATH" AGENT_DISPATCH_CAP_ROUTE_AROUND="$1" TOOLS="$2" TIMEOUT=20 \
      ERRLOG="$TMP/claude-probe.err" sh -c '. "$1" && agent_dispatch claude-oauth claude-x "$2"' sh "$DISPATCH" "$TMP/claude-probe.prompt" >/dev/null 2>"$TMP/claude-probe.stderr"; echo $? )
}
printf 'hi\n' > "$TMP/claude-probe.prompt"
check "dispatch: a failed read-only claude seat keeps its raw code under the opt-in" "4" "$(claude_probe 1 read,grep)"
check "dispatch: a failed write-capable claude seat keeps its raw code" "4" "$(claude_probe "" read,write,bash)"

# every rank out of tokens: exit 4 (it ran, and ran out), naming them all
printf '#!/bin/sh\nprintf "openrouter capped-a\\nopenrouter capped-b\\n"\n' > "$TMP/agent-role-dry"
chmod +x "$TMP/agent-role-dry"
out=$(SPAWN_ROSTER="$TMP/agent-role-dry" spawn); rc=$?
check "mu-spawn: a role with every rank out of tokens exits 4" "4" "$rc"
case "$out" in
  *"out of tokens: openrouter/capped-a, openrouter/capped-b"*) printf '  ok   and names every rank that ran out\n' ;;
  *) check "the dry role names its ranks" "out of tokens: capped-a, capped-b" "$out" ;;
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

# ---- 6. a session with a ROLE walks the role's ranks (end to end) --------
# The role the model was chosen from rides to the session (`mu ask --role`,
# DISPATCH_ROLE through agent-dispatch). The daemon reads the role's ranks from
# the roster — the file agent-role reads, here a throwaway one named by
# AGENT_ROLES — and when the model in force runs out of tokens the session
# continues on the next rank, circularly, and says so on stderr. A rank mu
# cannot run (claude-oauth) is skipped and named; when nothing is left the ask
# stops with exit 4 naming the role. Only against THIS build (see 3).
if [ -x "$MU_BUILT" ]; then
  RH="$TMP/role-home"; mkdir -p "$RH/.config/mu"
  roster() {  # role r's ranks, one "provider model" argument per rank
    { printf '[r]\n'
      for l in "$@"; do
        printf '[[r.ranked]]\nprovider = "%s"\nmodel = "%s"\n' "${l%% *}" "${l#* }"
      done; } > "$TMP/role-roster.toml"
  }
  role_ask() {  # [mu ask args...] -> stdout+stderr, rc in $role_rc
    role_out=$(HOME="$RH" XDG_CONFIG_HOME="$RH/.config" AGENT_ROLES="$TMP/role-roster.toml" "$MU_BUILT" ask --bare --provider faux \
      --model faux-usage-limit "$@" 'hi' 2>&1); role_rc=$?
  }
  roster "faux faux-usage-limit" "claude-oauth claude-opus-4-8" "faux faux"
  role_ask --role r
  check "a role's next rank answers when the first runs out" "0" "$role_rc"
  case "$role_out" in
    *"mu: usage limit on anthropic_api/faux-usage-limit"*"continuing on anthropic_api/faux"*)
      printf '  ok   the switch is said on stderr\n' ;;
    *) check "the switch is said on stderr" "a continuing-on line" "$role_out" ;;
  esac
  case "$role_out" in
    *"mu: fallback cannot use: claude-oauth/claude-opus-4-8 (runs as \`claude -p\` from the dispatcher, not inside a mu session)"*)
      printf '  ok   a short roster is said up front, not only logged\n' ;;
    *) check "a short roster is said up front" "a cannot-use line" "$role_out" ;;
  esac
  role_ask
  check "without a role the same cap still exits 4" "4" "$role_rc"
  roster "claude-oauth claude-opus-4-8" "faux faux-usage-limit"
  role_ask --role r
  check "every runnable rank out of tokens -> exit 4" "4" "$role_rc"
  case "$role_out" in
    *"no model left in role r: out of tokens: anthropic_api/faux-usage-limit; not runnable in a mu session: claude-oauth/claude-opus-4-8 (runs as \`claude -p\` from the dispatcher, not inside a mu session)"*)
      printf '  ok   the stop names the role, what ran out, and what cannot run here\n' ;;
    *) check "the stop names the role" "no model left in role r ..." "$role_out" ;;
  esac
  # a local rank (the shared ollama box) runs under the dispatcher's lease;
  # a session does not switch onto it in place, and says why
  roster "faux faux-usage-limit" "ollama qwen3.8:27b"
  role_ask --role r
  case "$role_rc:$role_out" in
    4:*"not runnable in a mu session: ollama/qwen3.8:27b (local: needs the dispatcher's lease/endpoint)"*)
      printf '  ok   a local rank is named, not switched onto without its lease\n' ;;
    *) check "a local rank is not switched onto" "rc 4 + local reason" "$role_rc:$role_out" ;;
  esac
  role_ask --role nope
  case "$role_rc:$role_out" in
    1:*"role nope"*"has no such role"*) printf '  ok   an unknown role refuses the session, saying so\n' ;;
    *) check "an unknown role refuses the session" "rc 1 + reason" "$role_rc:$role_out" ;;
  esac
  # a resume inherits its predecessor's role; if the roster no longer has
  # it, recovery must still work — resumed without the fallback, and said
  # so — while a role the caller NAMES that cannot be resolved still refuses
  roster "faux faux-usage-limit" "faux faux"
  HOME="$RH" XDG_CONFIG_HOME="$RH/.config" AGENT_ROLES="$TMP/role-roster.toml" \
    "$MU_BUILT" ask --provider faux --model faux-usage-limit --role r first >/dev/null 2>&1
  pred=$(ls -t "$RH"/.local/share/mu/events/*/*.jsonl 2>/dev/null | head -1)
  if [ -n "$pred" ]; then
    ref="$(basename "$(dirname "$pred")"):$(basename "$pred" .jsonl)"
    printf '[other]\n[[other.ranked]]\nprovider = "faux"\nmodel = "faux"\n' > "$TMP/role-roster.toml"
    out=$(HOME="$RH" XDG_CONFIG_HOME="$RH/.config" AGENT_ROLES="$TMP/role-roster.toml" \
      "$MU_BUILT" resume "$ref" --provider faux --model faux again 2>&1); rc=$?
    check "a resume whose inherited role is gone still resumes" "0" "$rc"
    case "$out" in
      *"role r:"*"has no such role — resumed without the role's fallback"*)
        printf '  ok   and says it resumed without the fallback\n' ;;
      *) check "the lost inherited role is said" "a resumed-without line" "$out" ;;
    esac
    # ...and a cap on that head is a plain cap, not "no model left in role r"
    # for a role it never armed
    out=$(HOME="$RH" XDG_CONFIG_HOME="$RH/.config" AGENT_ROLES="$TMP/role-roster.toml" \
      "$MU_BUILT" resume "$ref" --provider faux --model faux-usage-limit again 2>&1); rc=$?
    case "$rc:$out" in
      4:*"no model left in role"*) check "an unarmed inherited role is not blamed for a cap" "a plain cap" "$out" ;;
      4:*) printf '  ok   a cap after resuming without the role is a plain cap\n' ;;
      *) check "a cap after resuming without the role exits 4" "4" "$rc" ;;
    esac
    out=$(HOME="$RH" XDG_CONFIG_HOME="$RH/.config" AGENT_ROLES="$TMP/role-roster.toml" \
      "$MU_BUILT" resume "$ref" --provider faux --model faux --role r again 2>&1); rc=$?
    case "$rc:$out" in
      0:*) check "a named role that cannot resolve refuses the resume" "non-zero" "$rc" ;;
      *"has no such role"*) printf '  ok   a role the caller names that cannot resolve still refuses\n' ;;
      *) check "a named role that cannot resolve refuses, saying why" "has no such role" "$out" ;;
    esac
  else
    check "the role session left a log to resume" "a session log" "none under $RH"
  fi

  # through the dispatcher: DISPATCH_ROLE is what carries the role down
  roster "faux faux-usage-limit" "faux faux"
  echo hi > "$TMP/role-prompt"
  disp() {  # [env...] -> rc of one agent_dispatch of the capped model
    # (section 2 exported the route-around opt-in; this is about the role)
    ( env -u AGENT_DISPATCH_CAP_ROUTE_AROUND HOME="$RH" XDG_CONFIG_HOME="$RH/.config" AGENT_ROLES="$TMP/role-roster.toml" "$@" sh -c '. "$1"; TOOLS="" MU="$2" ERRLOG="$3" AGENT_DISPATCH_NO_LEASE=1 agent_dispatch faux faux-usage-limit "$4"' \
        _ "$DISPATCH" "$MU_BUILT" "$TMP/disp.err" "$TMP/role-prompt" >/dev/null 2>&1 )
    printf '%s' "$?"
  }
  check "agent_dispatch with DISPATCH_ROLE falls back in-session (exit 0)" "0" "$(disp DISPATCH_ROLE=r)"
  check "agent_dispatch without it still exits 4" "4" "$(disp)"
  # a SUCCESSFUL fallback is still said to the dispatcher's caller: the seat's
  # stderr goes to its errlog, so mu's notices come through --notices (mu's
  # own lines, never model output) and the dispatcher forwards them. The
  # errlog path has a space in it on purpose.
  out=$( ( env -u AGENT_DISPATCH_CAP_ROUTE_AROUND HOME="$RH" XDG_CONFIG_HOME="$RH/.config" AGENT_ROLES="$TMP/role-roster.toml" \
      sh -c '. "$1"; DISPATCH_ROLE=r TOOLS="" MU="$2" ERRLOG="$3" AGENT_DISPATCH_NO_LEASE=1 agent_dispatch faux faux-usage-limit "$4"' \
      _ "$DISPATCH" "$MU_BUILT" "$TMP/disp notices.err" "$TMP/role-prompt" ) 2>&1 >/dev/null )
  case "$out" in
    *"agent-dispatch: faux/faux-usage-limit: usage limit on anthropic_api/faux-usage-limit"*"continuing on anthropic_api/faux"*)
      printf '  ok   a successful fallback is said to the dispatcher'"'"'s caller\n' ;;
    *) check "a successful fallback is said to the caller" "a forwarded continuing-on line" "$out" ;;
  esac
  # a caller that redirects the dispatcher's stderr too (the orchestrator)
  # names DISPATCH_NOTICES and reads the file itself: it is written and kept
  rm -f "$TMP/stage.notices"
  ( env -u AGENT_DISPATCH_CAP_ROUTE_AROUND HOME="$RH" XDG_CONFIG_HOME="$RH/.config" AGENT_ROLES="$TMP/role-roster.toml" \
      DISPATCH_NOTICES="$TMP/stage.notices" \
      sh -c '. "$1"; DISPATCH_ROLE=r TOOLS="" MU="$2" ERRLOG="$3" AGENT_DISPATCH_NO_LEASE=1 agent_dispatch faux faux-usage-limit "$4"' \
      _ "$DISPATCH" "$MU_BUILT" "$TMP/stage.err" "$TMP/role-prompt" ) >/dev/null 2>&1
  case "$(cat "$TMP/stage.notices" 2>/dev/null)" in
    *"continuing on anthropic_api/faux"*) printf '  ok   DISPATCH_NOTICES keeps the notices for a caller that reads them\n' ;;
    *) check "DISPATCH_NOTICES keeps the notices" "a continuing-on line in the file" "$(cat "$TMP/stage.notices" 2>/dev/null)" ;;
  esac
else
  printf '  SKIP role fallback e2e: no build at %s\n' "$MU_BUILT"
fi

# An installed mu that predates --role must not be handed it (it would die at
# argument parsing, and every dispatch with a role with it): the dispatcher
# asks the binary, passes the flag only to one that lists it, and says so on
# stderr when it cannot. Hermetic: stub binaries record their argv.
for kind in old new; do
  { printf '#!/bin/sh\n'
    printf 'case " $* " in *" --help "*) echo "Usage: mu ask [OPTIONS]"; %s exit 0 ;; esac\n' \
      "$([ $kind = new ] && echo 'echo "      --role <ROLE>";')"
    printf 'echo "$*" > "%s/argv.%s"; echo answered\n' "$TMP" "$kind"; } > "$TMP/mu-$kind"
  chmod +x "$TMP/mu-$kind"
done
role_disp() {  # $1=old|new -> caller-visible stderr; argv in $TMP/argv.$1
  ( env -u AGENT_DISPATCH_CAP_ROUTE_AROUND sh -c '. "$1"; DISPATCH_ROLE=coding TOOLS="" MU="$2" ERRLOG="$3" AGENT_DISPATCH_NO_LEASE=1 agent_dispatch openrouter m "$4"' \
      _ "$DISPATCH" "$TMP/mu-$1" "$TMP/rd.err" "$TMP/agent-role" 2>&1 >/dev/null )
}
out=$(role_disp new)
case "$(cat "$TMP/argv.new")" in
  *"--role coding"*) printf '  ok   a mu that takes --role is given the role\n' ;;
  *) check "a mu that takes --role is given the role" "--role coding in argv" "$(cat "$TMP/argv.new")" ;;
esac
out=$(role_disp old)
case "$(cat "$TMP/argv.old")" in
  *"--role"*) check "a mu that predates --role is not handed it" "no --role" "$(cat "$TMP/argv.old")" ;;
  *) printf '  ok   a mu that predates --role is not handed it (the dispatch still runs)\n' ;;
esac
case "$out" in
  *"does not take \`ask --role\`"*"WITHOUT role coding"*) printf '  ok   and the missing fallback is said on stderr\n' ;;
  *) check "the missing fallback is said on stderr" "a does-not-take line" "$out" ;;
esac

# an AGENT_ROLE_PIN names one exact model for the call: no role, no fallback
( env -u AGENT_DISPATCH_CAP_ROUTE_AROUND AGENT_ROLE_PIN="openrouter m" sh -c '. "$1"; DISPATCH_ROLE=coding TOOLS="" MU="$2" ERRLOG="$3" AGENT_DISPATCH_NO_LEASE=1 agent_dispatch openrouter m "$4"' \
    _ "$DISPATCH" "$TMP/mu-new" "$TMP/rd.err" "$TMP/agent-role" >/dev/null 2>&1 )
case "$(cat "$TMP/argv.new")" in
  *--role*) check "a pinned dispatch carries no role" "no --role" "$(cat "$TMP/argv.new")" ;;
  *) printf '  ok   a pinned dispatch carries no role\n' ;;
esac

# mu-spawn: a model named outright gets NO role, even with one exported by the
# caller; a role-resolved walk passes its own role. (mu-new records argv.)
spawn_argv() {  # [env...] -> the argv mu-new saw
  rm -f "$TMP/argv.new"
  env -u AGENT_DISPATCH_CAP_ROUTE_AROUND AGENT_ROLE="$TMP/agent-role" MU="$TMP/mu-new" \
    AGENT_DISPATCH_NO_LEASE=1 TMPDIR="$TMP" DISPATCH_ROLE=exported "$@" \
    sh "$SPAWN" --cwd "$TMP" 'do the thing' >/dev/null 2>&1
  cat "$TMP/argv.new" 2>/dev/null
}
case "$(spawn_argv MU_SPAWN_PROVIDER=openrouter MU_SPAWN_MODEL=m)" in
  *--role*) check "an explicit model drops an exported role" "no --role" "$(cat "$TMP/argv.new")" ;;
  *) printf '  ok   an explicit model drops an exported DISPATCH_ROLE\n' ;;
esac
case "$(spawn_argv)" in
  *"--role coding"*) printf '  ok   a role-resolved walk passes its own role, not the exported one\n' ;;
  *) check "a role-resolved walk passes its own role" "--role coding" "$(cat "$TMP/argv.new" 2>/dev/null)" ;;
esac

[ "$fail" -eq 0 ] || { printf 'rank-fallthrough-test: FAILED\n' >&2; exit 1; }
printf 'rank-fallthrough-test: all cases passed\n'
