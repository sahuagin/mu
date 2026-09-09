#!/bin/sh
# ci-aipr-fast-test.sh — regression tests for the ci-aipr fast path (mu-ash9p).
#
# Two mechanisms, both exercised offline (synthetic seat files and a throwaway
# repo — no model spend, no network):
#
#   1. converge.py `agree` — LIVE-SEAT quorum. A seat that timed out or returned
#      nothing recoverable is ABSENT for the round, not a dissenter (a reply that
#      lists findings without a verdict is a needs-changes, not an absence); agreement
#      still needs MU_REVIEW_MIN_LIVE_SEATS seats that actually answered (default
#      3, a majority of the five-seat roster). Two measured runs are pinned here:
#      four seats agreeing every round while a fifth emitted unparseable JSON and
#      the gate reported ESCALATE (PR #608), and a PASS on only 2/5 live seats
#      (PR #611) — the reason the quorum is a majority and not two.
#   2. seat-timeout.sh — the per-provider-class seat cap: a local seat gets
#      longer than an API seat because measured local seats take 26-36 min while
#      every API seat answered inside 13.
#   3. ci-green-marker.sh — the "fmt/clippy/tests already green at THIS commit"
#      receipt that lets `just ci-aipr` skip repeating them.
#
# usage: sh scripts/tests/ci-aipr-fast-test.sh
set -u
HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
CONVERGE="$HERE/../review-panel/converge.py"
MARKER="$HERE/../ci-green-marker.sh"
fails=0

TMP=$(mktemp -d "${TMPDIR:-/tmp}/ci-aipr-fast.XXXXXX") || exit 1
trap 'rm -rf "$TMP"' EXIT

# Hermetic: this runs inside `just ci-aipr`, so the operator's own review knobs
# must not steer its assertions (panel finding, PR #611 — a tuned seat cap in
# the environment failed the gate's self-test on a green tree). Every check
# below sets what it needs explicitly, and the bare cap probes must classify by
# provider name alone, never by the real ~/.config/mu/config.toml.
unset MU_REVIEW_MIN_LIVE_SEATS MU_REVIEW_SEAT_TIMEOUT_SECS \
      MU_REVIEW_LOCAL_SEAT_TIMEOUT_SECS MU_REVIEW_REASK_TIMEOUT_SECS \
      MU_REVIEW_FORCE_CHECK
MU_REVIEW_PROVIDER_CONFIG="$TMP/no-such-config.toml"; export MU_REVIEW_PROVIDER_CONFIG

# --- 1. live-seat quorum ---------------------------------------------------

# Write one round-1 seat. Shapes are the ones dispatch.sh actually produces:
#   approve | needs-changes  a contract-shaped reply
#   unparsed                 prose, no JSON at all (what a chatty seat emits)
#   noverdict                valid JSON with no verdict field and no findings
#   findings-noverdict       valid JSON listing a finding, verdict field blank
#   timeout                  .done carries exit=124, as `timeout` leaves it
#   skipped                  an ollama seat that routed around a held box:
#                            empty reply, .done exit=75 prov=ollama
#   timeout-parsed           exit=124 but the reply finished streaming before the
#                            kill: a complete needs-changes with a finding
seat() { # $1=dir $2=tag $3=shape[:seam]   (seam marks an EXCLUSIVE seat)
  _seam="${3#*:}"; [ "$_seam" = "$3" ] && _seam=""
  set -- "$1" "$2" "${3%%:*}"
  case "$3" in
    approve|needs-changes)
      printf 'VERDICT: %s\n{"verdict":"%s","summary":"s","findings":[]}\n' "$3" "$3" \
        > "$1/r1.$2.out" ;;
    unparsed)
      printf 'I was unable to finish reviewing this change in the time available.\n' \
        > "$1/r1.$2.out" ;;
    noverdict)
      printf '{"summary":"s","findings":[]}\n' > "$1/r1.$2.out" ;;
    findings-noverdict)
      printf '{"summary":"s","findings":[{"severity":"high","file":"x.rs","line":1,"issue":"unchecked"}]}\n' \
        > "$1/r1.$2.out" ;;
    timeout|skipped)
      : > "$1/r1.$2.out" ;;
    timeout-parsed)
      printf 'VERDICT: needs-changes\n{"verdict":"needs-changes","summary":"s","findings":[{"severity":"high","file":"x.rs","line":1,"issue":"unchecked"}]}\n' \
        > "$1/r1.$2.out" ;;
  esac
  if [ "$3" = timeout ] || [ "$3" = timeout-parsed ]; then
    echo "exit=124 retry=0 prov=openrouter model=$2 seam=[$_seam]" > "$1/r1.$2.done"
  elif [ "$3" = skipped ]; then
    echo "exit=75 retry=0 prov=ollama model=$2 seam=[$_seam]" > "$1/r1.$2.done"
  else
    echo "exit=0 retry=0 prov=openrouter model=$2 seam=[$_seam]" > "$1/r1.$2.done"
  fi
}

panel() { # $1=dir-name, then <tag>=<shape> pairs
  d="$TMP/$1"; rm -rf "$d"; mkdir -p "$d"; shift
  for pair in "$@"; do seat "$d" "${pair%%=*}" "${pair#*=}"; done
  printf '%s\n' "$d"
}

check() { # $1=label $2=dir $3=expected-first-line $4=expected-census $5=expected-rc
  out=$(python3 "$CONVERGE" agree "$2/r1" 2>&1); rc=$?
  got=$(printf '%s\n' "$out" | sed -n '1p')
  seats=$(printf '%s\n' "$out" | sed -n '2p')
  if [ "$got" != "$3" ]; then
    echo "FAIL $1: expected '$3', got '$got'"; fails=$((fails + 1)); return
  fi
  if [ "$seats" != "SEATS $4" ]; then
    echo "FAIL $1: expected census 'SEATS $4', got '$seats'"; fails=$((fails + 1)); return
  fi
  if [ "$rc" -ne "$5" ]; then
    echo "FAIL $1: expected exit $5, got $rc"; fails=$((fails + 1)); return
  fi
  echo "ok   $1"
}

# Baseline: nothing absent, nothing changed about a whole-panel agreement.
d=$(panel all-live rank0.a=approve rank1.b=approve rank2.c=approve)
check "a fully live panel still agrees" "$d" "AGREE approve" "live 3/3" 0

# (a) the measured PR #608 shape: four seats agree, one emits unparseable prose.
d=$(panel unparsed-seat rank0.opus-5=approve rank1.gpt-5.5=unparsed \
          rank2.glm=approve rank3.kimi=approve rank4.opus-4-8=approve)
check "4 live agree + 1 unparsed seat converges" "$d" \
  "AGREE approve" "live 4/5: gpt-5.5 unparsed" 0

# (b) same, but the fifth seat hit the wall-clock cap.
d=$(panel timeout-seat rank0.opus-5=approve rank1.gpt-5.5=approve \
          rank2.glm=approve rank3.kimi=timeout rank4.opus-4-8=approve)
check "4 live agree + 1 timed-out seat converges" "$d" \
  "AGREE approve" "live 4/5: kimi timeout" 0

# The cap kills the PROCESS: a reply that finished before the kill is a real
# opinion, and hiding it behind the synthetic timeout would let three approves
# pass over a complete needs-changes (panel finding, PR #611).
d=$(panel timeout-parsed-seat rank0.a=approve rank1.b=approve rank2.c=approve \
          rank3.d=timeout-parsed)
check "a timed-out seat whose reply parses is live, and dissents" "$d" \
  'SPLIT {"rank0.a": "approve", "rank1.b": "approve", "rank2.c": "approve", "rank3.d": "needs-changes"}' \
  "live 4/4" 1

# JSON with no verdict field is the same class of absence as prose.
d=$(panel noverdict-seat rank0.a=approve rank1.b=approve rank2.c=approve \
          rank3.d=noverdict)
check "a verdict-less JSON seat is absent, not a dissenter" "$d" \
  "AGREE approve" "live 3/4: d unparsed" 0

# ...unless it lists findings: then it reviewed and left the field blank, and
# three approves must not outvote its defect into a round-1 PASS that never
# airs it (panel finding, PR #611).
d=$(panel findings-noverdict-seat rank0.a=approve rank1.b=approve rank2.c=approve \
          rank3.d=findings-noverdict)
check "a verdict-less seat WITH findings is a needs-changes, not absent" "$d" \
  'SPLIT {"rank0.a": "approve", "rank1.b": "approve", "rank2.c": "approve", "rank3.d": "needs-changes"}' \
  "live 4/4" 1

# Absence must not soften a BLOCK into a PASS.
d=$(panel block-with-absent rank0.a=needs-changes rank1.b=needs-changes \
          rank2.c=needs-changes rank3.d=unparsed)
# (exit 0 = the panel agreed, whatever it agreed ON; ai-review.sh reads the
# verdict, not this status.)
check "an absent seat cannot turn needs-changes into approve" "$d" \
  "AGREE needs-changes" "live 3/4: d unparsed" 0

# An EXCLUSIVE seam seat (seam="conformance" on the roster) is the only
# reviewer of its checklist. Its absence must not let the others approve past
# it; a block needs no missing reviewer (panel finding, PR #611).
d=$(panel seam-absent rank0.a=approve rank1.b=approve rank2.c=approve rank3.d=timeout:conformance \
          rank4.e=approve)
check "an absent exclusive seam seat withholds an approve" "$d" \
  'SPLIT {"rank0.a": "approve", "rank1.b": "approve", "rank2.c": "approve", "rank3.d": "timeout", "rank4.e": "approve"}' \
  "live 4/5: d timeout (approve withheld: exclusive seam seat d=conformance absent)" 1
d=$(panel seam-absent-block rank0.a=needs-changes rank1.b=needs-changes rank2.c=needs-changes \
          rank3.d=unparsed:conformance rank4.e=needs-changes)
check "an absent exclusive seam seat does not withhold a block" "$d" \
  "AGREE needs-changes" "live 4/5: d unparsed" 0
d=$(panel seam-live rank0.a=approve rank1.b=approve rank2.c=approve rank3.d=approve:conformance \
          rank4.e=timeout)
check "an absent GENERAL seat still lets a live exclusive seat's approve stand" "$d" \
  "AGREE approve" "live 4/5: e timeout" 0
# ...including a seat the loader dropped entirely (a lease-skipped ollama seat
# is neither live nor absent in the census, but it is still the only reviewer
# of its checklist).
d=$(panel seam-skipped rank0.a=approve rank1.b=approve rank2.c=approve rank3.d=skipped:conformance)
check "a lease-skipped exclusive seam seat still withholds an approve" "$d" \
  'SPLIT {"rank0.a": "approve", "rank1.b": "approve", "rank2.c": "approve"}' \
  "live 3/3 (approve withheld: exclusive seam seat d=conformance absent)" 1

# (c) live disagreement is still a split — this is the property absence must not
# be allowed to erode.
d=$(panel live-split rank0.a=approve rank1.b=approve rank2.c=needs-changes)
check "3 live seats that disagree do not converge" "$d" \
  'SPLIT {"rank0.a": "approve", "rank1.b": "approve", "rank2.c": "needs-changes"}' \
  "live 3/3" 1

# The measured PR #611 shape: two seats agreed and the gate passed in one round
# while three were absent. Two live seats out of five is not a panel — under the
# default majority quorum this round must NOT converge.
d=$(panel two-live rank0.opus-5=timeout rank1.gpt-5.5=unparsed rank2.glm-5.2=timeout \
          rank3.kimi=approve rank4.opus-4-8=approve)
check "2 of 5 live seats do not carry a verdict" "$d" \
  'SPLIT {"rank0.opus-5": "timeout", "rank1.gpt-5.5": "unparsed", "rank2.glm-5.2": "timeout", "rank3.kimi": "approve", "rank4.opus-4-8": "approve"}' \
  "live 2/5 (quorum 3 unmet): opus-5 timeout, gpt-5.5 unparsed, glm-5.2 timeout" 1

# (d) one seat agreeing with itself is a single review, not a panel.
d=$(panel one-live rank0.a=approve rank1.b=unparsed rank2.c=timeout)
check "1 live seat is below the quorum and escalates" "$d" \
  'SPLIT {"rank0.a": "approve", "rank1.b": "unparsed", "rank2.c": "timeout"}' \
  "live 1/3 (quorum 3 unmet): b unparsed, c timeout" 1

# ...and the quorum is the reason, not the verdicts: lower it and the same
# round converges.
out=$(MU_REVIEW_MIN_LIVE_SEATS=1 python3 "$CONVERGE" agree "$d/r1" 2>&1 | sed -n '1p')
if [ "$out" = "AGREE approve" ]; then
  echo "ok   MU_REVIEW_MIN_LIVE_SEATS=1 admits the lone live seat"
else
  echo "FAIL MU_REVIEW_MIN_LIVE_SEATS=1: expected 'AGREE approve', got '$out'"
  fails=$((fails + 1))
fi

# A bad knob value must not silently disable the quorum.
out=$(MU_REVIEW_MIN_LIVE_SEATS=two python3 "$CONVERGE" agree "$d/r1" 2>/dev/null | sed -n '1p')
case "$out" in
  SPLIT*) echo "ok   an unparsable MU_REVIEW_MIN_LIVE_SEATS falls back to 3" ;;
  *) echo "FAIL bad MU_REVIEW_MIN_LIVE_SEATS relaxed the quorum: '$out'"; fails=$((fails + 1)) ;;
esac

# --- 2. per-provider-class seat caps ---------------------------------------

. "$HERE/../review-panel/seat-timeout.sh"

cap() { seat_timeout "$1" "${2:-}"; }
cap_with() { # $1=env assignments $2=provider [$3=roster timeout_secs]
  ( eval "$1"; seat_timeout "$2" "${3:-}" )
}
expect_cap() { # $1=label $2=expected $3=got
  if [ "$3" = "$2" ]; then echo "ok   $1"
  else echo "FAIL $1: expected $2, got $3"; fails=$((fails + 1)); fi
}

# The operator's measurement, as two assertions: a local seat that answers at
# 20 min is still inside its cap; an API seat at 20 min is long past its own.
if [ "1200" -lt "$(cap ollama)" ]; then
  echo "ok   an ollama seat answering at 1200s is inside the local cap"
else
  echo "FAIL a 1200s ollama seat would be dropped (cap $(cap ollama))"; fails=$((fails + 1))
fi
if [ "1200" -gt "$(cap openrouter)" ]; then
  echo "ok   an API seat answering at 1200s is past the API cap"
else
  echo "FAIL a 1200s API seat would still be waited on (cap $(cap openrouter))"; fails=$((fails + 1))
fi

expect_cap "vllm counts as local"           1800 "$(cap vllm)"
expect_cap "claude-oauth counts as API"      900 "$(cap claude-oauth)"
expect_cap "the local cap is env-settable"  1200 "$(cap_with 'MU_REVIEW_LOCAL_SEAT_TIMEOUT_SECS=1200' ollama)"
expect_cap "the API cap is env-settable"    2400 "$(cap_with 'MU_REVIEW_SEAT_TIMEOUT_SECS=2400' openrouter)"
# The roster knows a seat's real latency; it outranks both env defaults.
expect_cap "a ranked timeout_secs outranks the env cap" 2400 \
  "$(cap_with 'MU_REVIEW_SEAT_TIMEOUT_SECS=900' openrouter 2400)"
expect_cap "a junk timeout_secs falls back to the class cap" 900 "$(cap openrouter soon)"
expect_cap "a zero timeout_secs does not uncap the seat"     900 "$(cap openrouter 0)"

# A provider that is neither ollama nor vllm is classified by the base_url its
# [[providers.endpoints]] entry resolves to — our hardware or someone else's.
if command -v tq >/dev/null 2>&1 && command -v jq >/dev/null 2>&1; then
  CFG="$TMP/providers.toml"
  cat > "$CFG" <<'TOML'
[[providers.endpoints]]
name     = "lanbox"
protocol = "openai-chat"
base_url = "http://10.1.1.143:11435"

[[providers.endpoints]]
name     = "hosted"
protocol = "openai-chat"
base_url = "https://api.example.invalid/v1"

[[providers.endpoints]]
name     = "v6remote"
protocol = "openai-chat"
base_url = "http://[2001:db8::1]:8080"

[[providers.endpoints]]
name     = "v6loop"
protocol = "openai-chat"
base_url = "http://[::1]:11434"
TOML
  expect_cap "a LAN endpoint provider gets the local cap" 1800 \
    "$(cap_with "MU_REVIEW_PROVIDER_CONFIG=$CFG" lanbox)"
  expect_cap "a hosted endpoint provider gets the API cap" 900 \
    "$(cap_with "MU_REVIEW_PROVIDER_CONFIG=$CFG" hosted)"
  expect_cap "an unknown provider gets the API cap" 900 \
    "$(cap_with "MU_REVIEW_PROVIDER_CONFIG=$CFG" nosuchprovider)"
  expect_cap "a bracketed public IPv6 endpoint gets the API cap" 900 \
    "$(cap_with "MU_REVIEW_PROVIDER_CONFIG=$CFG" v6remote)"
  expect_cap "a bracketed loopback IPv6 endpoint gets the local cap" 1800 \
    "$(cap_with "MU_REVIEW_PROVIDER_CONFIG=$CFG" v6loop)"
else
  echo "skip endpoint-name classification (tq/jq absent)"
fi

# --- 2b. the verdict re-ask is bounded --------------------------------------

# A non-empty reply that parses to nothing gets ONE re-ask for its verdict. It
# used to inherit the seat's full cap, so a seat could cost 2x its cap in every
# round (panel finding, PR #611); now it carries its own short one.
reask_dir="$TMP/reask"; mkdir -p "$reask_dir"
agent_dispatch() { # stub: record the cap the re-ask ran under, answer in contract
  printf '%s\n' "${TIMEOUT:-unset}" > "$reask_dir/timeout-seen"
  printf 'VERDICT: approve\n{"verdict":"approve","summary":"s","findings":[]}\n'
}
reask() { # $1=extra env assignment
  printf 'I looked at the change and it seems fine. What would you like next?\n' > "$reask_dir/r1.rank0.a.out"
  rm -f "$reask_dir/timeout-seen"
  ( HERE="$HERE/../review-panel"; . "$HERE/verdict-retry.sh"; ERRLOG=/dev/null
    eval "$1"; reask_if_unparsed openrouter m "$reask_dir/r1.rank0.a.out" )
  cat "$reask_dir/timeout-seen" 2>/dev/null
}
expect_cap "the verdict re-ask carries its own 180s cap, not the seat's" 180 \
  "$(reask 'TIMEOUT=900')"
expect_cap "the re-ask cap is env-settable" 60 \
  "$(reask 'TIMEOUT=900; MU_REVIEW_REASK_TIMEOUT_SECS=60')"
if python3 "$HERE/../review-panel/parse.py" --check "$reask_dir/r1.rank0.a.out" 2>/dev/null; then
  echo "ok   a parseable re-ask is promoted to the seat's reply"
else
  echo "FAIL the re-ask answer was not promoted"; fails=$((fails + 1))
fi
unset -f agent_dispatch

# --- 3. ci-green marker gate (e) -------------------------------------------

REPO="$TMP/repo"
mkdir -p "$REPO"
( cd "$REPO" && git init -q -b main . && git -c user.email=t@example.invalid \
    -c user.name=t commit -q --allow-empty -m "c1" ) >/dev/null 2>&1
TARGET="$TMP/target"
mkdir -p "$TARGET"

# These assertions are about the marker's GIT path. The marker probes `jj root`
# first and jj walks upward from cwd, so a TMPDIR inside a jj workspace would
# put the fixture in jj mode and assert against that workspace (panel finding,
# PR #611). A jj shim that always fails pins git mode wherever TMP lands.
mkdir -p "$TMP/bin"
printf '#!/bin/sh\nexit 1\n' > "$TMP/bin/jj"; chmod +x "$TMP/bin/jj"
MPATH="$TMP/bin:$PATH"

gate() { ( cd "$REPO" && PATH="$MPATH" CARGO_TARGET_DIR="$TARGET" "$@" sh "$MARKER" gate >/dev/null 2>&1 ); }
mpath() { ( cd "$REPO" && PATH="$MPATH" CARGO_TARGET_DIR="$TARGET" sh "$MARKER" path 2>/dev/null ); }
mid() { ( cd "$REPO" && PATH="$MPATH" CARGO_TARGET_DIR="$TARGET" sh "$MARKER" id 2>/dev/null ); }
mwrite() { ( cd "$REPO" && PATH="$MPATH" CARGO_TARGET_DIR="$TARGET" sh "$MARKER" write "$@" >/dev/null 2>&1 ); }
head_id() { ( cd "$REPO" && git rev-parse HEAD 2>/dev/null ); }
commit() { ( cd "$REPO" && git -c user.email=t@example.invalid -c user.name=t \
               commit -q --allow-empty -m "$1" ) >/dev/null 2>&1; }
no_marker() { [ -z "$(ls "$TARGET"/ci-green-* 2>/dev/null)" ]; }
ok_or_fail() { # $1=label $2=condition-result(0/1)
  if [ "$2" -eq 0 ]; then echo "ok   $1"; else echo "FAIL $1"; fails=$((fails + 1)); fi
}

if [ -z "$(mpath)" ]; then
  echo "FAIL ci-green-marker resolved no commit id in a fresh git repo"
  fails=$((fails + 1))
else
  expect_cap "the fixture runs the marker in git mode (id is the repo's HEAD)" "$(head_id)" "$(mid)"
  gate env && { echo "FAIL gate passed with no marker"; fails=$((fails + 1)); } \
    || echo "ok   no marker means the check runs"

  # The normal shape: capture, run the checks, and the tree has not moved.
  id0=$(mid); mwrite "$id0"
  p=$(mpath)
  if [ -f "$p" ]; then echo "ok   write records $(basename "$p")"
  else echo "FAIL write left no marker at $p"; fails=$((fails + 1)); fi

  if gate env; then echo "ok   marker for this commit skips the check"
  else echo "FAIL gate rejected a marker written for this very commit"; fails=$((fails + 1)); fi

  # A marker naming some other commit must not be honoured.
  mv "$p" "$TARGET/ci-green-0000000000000000000000000000000000000000"
  if gate env; then
    echo "FAIL a marker for another commit was honoured"; fails=$((fails + 1))
  else
    echo "ok   a marker for another commit still runs the check"
  fi
  rm -f "$TARGET"/ci-green-*

  # Same thing the way it actually happens: the commit moves under a live marker.
  mwrite "$(mid)"
  before=$(mpath)
  commit c2
  after=$(mpath)
  if [ "$before" = "$after" ]; then
    echo "skip a new commit did not move the marker id (temp dir sits under another VCS root)"
  elif gate env; then
    echo "FAIL the marker survived a new commit"; fails=$((fails + 1))
  else
    echo "ok   a new commit invalidates the marker"
  fi

  # The PR #611 panel finding: resolving the id at WRITE time would fold an edit
  # made DURING the run into it, certifying a tree the checks never saw. The id
  # is captured first, so a moved tree must produce NO receipt...
  rm -f "$TARGET"/ci-green-*
  id0=$(mid)
  commit c3
  mwrite "$id0"
  ok_or_fail "a tree that moved during the run writes no receipt" "$(no_marker; echo $?)"
  # ...and the next run must therefore do the check.
  gate env && { echo "FAIL the check was skipped after a voided receipt"; fails=$((fails + 1)); } \
    || echo "ok   a voided receipt does not skip the next check"

  # Under git the id does NOT move when files change, so the dirty worktree is
  # the only signal that the checked tree is gone.
  rm -f "$TARGET"/ci-green-*
  id0=$(mid)
  : > "$REPO/scratch.txt"
  mwrite "$id0"
  ok_or_fail "a dirty worktree writes no receipt (git)" "$(no_marker; echo $?)"
  rm -f "$REPO/scratch.txt"

  # ...and a tree dirty at CAPTURE yields no id at all: the checks may have run
  # with an uncommitted fix that is discarded before write, when HEAD reads
  # clean and a receipt would certify a tree the checks never saw.
  rm -f "$TARGET"/ci-green-*
  : > "$REPO/scratch.txt"
  id0=$(mid); rm -f "$REPO/scratch.txt"
  ok_or_fail "a dirty worktree at capture yields no id (git)" "$([ -z "$id0" ]; echo $?)"
  mwrite "$id0"
  ok_or_fail "...so the run writes no receipt once the tree is clean again" "$(no_marker; echo $?)"

  # An untracked new file is dirt even when the repo hides untracked files from
  # `git status` (status.showUntrackedFiles=no) — panel finding, PR #611.
  rm -f "$TARGET"/ci-green-*
  mwrite "$(mid)"
  ( cd "$REPO" && git config status.showUntrackedFiles no )
  : > "$REPO/new_test.rs"
  if gate env; then
    echo "FAIL a receipt was honoured over an untracked file hidden by showUntrackedFiles=no"; fails=$((fails + 1))
  else
    echo "ok   an untracked file is dirt even when git config hides it"
  fi
  rm -f "$REPO/new_test.rs"; ( cd "$REPO" && git config --unset status.showUntrackedFiles )

  # A FAILING `git status` is not a clean tree (panel finding, PR #611): shim
  # git so that `status` fails while everything else runs the real binary.
  REAL_GIT=$(command -v git)
  printf '#!/bin/sh\n[ "$1" = status ] && exit 128\nexec "%s" "$@"\n' "$REAL_GIT" > "$TMP/bin/git"
  chmod +x "$TMP/bin/git"
  rm -f "$TARGET"/ci-green-*
  id0=$(mid)
  ok_or_fail "a failing git status yields no id (not a clean tree)" "$([ -z "$id0" ]; echo $?)"
  rm -f "$TMP/bin/git"; mwrite "$(mid)"; printf '#!/bin/sh\n[ "$1" = status ] && exit 128\nexec "%s" "$@"\n' "$REAL_GIT" > "$TMP/bin/git"; chmod +x "$TMP/bin/git"
  if gate env; then
    echo "FAIL a receipt was honoured while git status was failing"; fails=$((fails + 1))
  else
    echo "ok   a failing git status does not honour a receipt"
  fi
  rm -f "$TMP/bin/git" "$TARGET"/ci-green-*

  # A receipt with nothing to certify against is the bug itself.
  rm -f "$TARGET"/ci-green-*
  mwrite
  ok_or_fail "write without a captured id records nothing" "$(no_marker; echo $?)"

  # The operator's escape hatch outranks a valid marker.
  mwrite "$(mid)"
  if gate env MU_REVIEW_FORCE_CHECK=1; then
    echo "FAIL MU_REVIEW_FORCE_CHECK did not force the check"; fails=$((fails + 1))
  else
    echo "ok   MU_REVIEW_FORCE_CHECK runs the check anyway"
  fi
fi

[ "$fails" -eq 0 ] && { echo "ci-aipr-fast: all checks passed"; exit 0; }
echo "ci-aipr-fast: $fails check(s) FAILED"; exit 1
