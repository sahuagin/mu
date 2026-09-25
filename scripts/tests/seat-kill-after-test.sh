#!/usr/bin/env bash
# seat-kill-after-test.sh — "a cap that cannot be enforced is not a cap"
# (bead mu-r2kz6).
#
# `timeout N cmd` sends SIGTERM at N and then waits FOREVER for a child that
# ignores it. agent-dispatch.sh bounds that with TWO NESTED timeouts:
#
#     timeout "$ad_timeout" timeout -s KILL "$ad_killat" cmd
#
# The outer one's deadline passing is what reports 124 — however the inner
# child died — and a SIGKILL BEFORE the deadline still surfaces as 137, so an
# external kill is not mistaken for our escalation. That shape was chosen over
# `timeout -k` because GNU timeout reports a -k SIGKILL as 137 (it signals its
# own process group), which every caller reads as a hard error, not a timeout.
#
# rank-fallthrough-test.sh §5 proves the dispatcher's own path with a fake mu.
# This test covers what that one does not:
#   1. the nested form against BOTH timeout implementations on this host
#      (FreeBSD /usr/bin/timeout and GNU coreutils under the linuxulator),
#      with a child that traps and ignores SIGTERM — the exact shape of the bug;
#   2. a well-behaved child is unaffected (dies at the cap, not cap+grace);
#   3. the CONTRACT: no panel script runs a seat under a bare `timeout` — a
#      dispatch path added later without the inner `-s KILL` reintroduces the
#      hang silently, and nothing else would notice until a board sat overnight.
set -u
set -o pipefail

TEST_DIR="$(cd "$(dirname "$0")" && pwd)"
DISPATCH="$TEST_DIR/../lib/agent-dispatch.sh"
[ -f "$DISPATCH" ] || { echo "seat-kill-after-test: agent-dispatch.sh not found at $DISPATCH" >&2; exit 2; }

fail=0
check() {  # $1=label $2=expected $3=actual
  if [ "$2" = "$3" ]; then printf '  ok   %s\n' "$1"; else printf '  FAIL %s\n       expected: %s\n       actual:   %s\n' "$1" "$2" "$3"; fail=1; fi
}

echo "seat-kill-after-test: the cap is enforceable"

# ── 1. nested form, both implementations, SIGTERM-ignoring child ──────────
for t in /usr/bin/timeout /compat/linux/usr/bin/timeout; do
  [ -x "$t" ] || continue
  start=$(date +%s)
  ( exec 2>/dev/null; "$t" 3 "$t" -s KILL 5 /bin/sh -c 'trap "" TERM; sleep 60' >/dev/null 2>&1 )
  rc=$?; elapsed=$(( $(date +%s) - start ))
  check "$t: a SIGTERM-ignoring child is killed, not waited on" "yes" "$([ "$elapsed" -lt 15 ] && echo yes || echo "no (${elapsed}s)")"
  check "$t: and the outer deadline reports 124, the code the panel retries on" "124" "$rc"
done

# ── 2. a well-behaved child dies at the cap, not cap+grace ─────────────────
start=$(date +%s)
timeout 2 timeout -s KILL 32 /bin/sh -c 'sleep 60' >/dev/null 2>&1
polite=$(( $(date +%s) - start ))
check "a well-behaved child still dies at the cap, not cap+grace" "yes" "$([ "$polite" -lt 10 ] && echo yes || echo "no (${polite}s)")"

# An EARLY external SIGKILL is not laundered: the outer deadline has not
# passed, so the status comes back as the kill it was.
( exec 2>/dev/null; timeout 30 timeout -s KILL 60 /bin/sh -c 'kill -9 $$' >/dev/null 2>&1 ); rc=$?
check "a SIGKILL before the deadline still surfaces as 137, not 124" "137" "$rc"

# ── 3. contract: every seat cap in every panel script is nested ────────────
SEAT_FILES="$DISPATCH $TEST_DIR/../review-panel/dispatch.sh $TEST_DIR/../review-panel/consensus.sh $TEST_DIR/../ai-review.sh"
bare=""
for f in $SEAT_FILES; do
  [ -f "$f" ] || continue
  hit=$(grep -nE '(^|[^-[:alnum:]_])timeout[[:space:]]+("?\$[A-Za-z_{]|[0-9])' "$f" | grep -vE '^[0-9]+:[[:space:]]*#' | grep -vE 'timeout[[:space:]]+("?\$[A-Za-z_{][^ ]*"?|[0-9]+)[[:space:]]+timeout[[:space:]]+-s[[:space:]]+KILL' || true)
  [ -n "$hit" ] && bare="$bare
$(basename "$f"): $hit"
done
check "no seat dispatch in any panel script runs under a bare timeout" "" "$bare"
nested=$(grep -cE 'timeout "\$ad_timeout" timeout -s KILL "\$ad_killat"' "$DISPATCH")
check "and the dispatcher's own four call sites are nested" "4" "$nested"

if [ "$fail" -eq 0 ]; then echo "seat-kill-after-test: all ok"; else echo "seat-kill-after-test: FAILED"; fi
exit "$fail"
