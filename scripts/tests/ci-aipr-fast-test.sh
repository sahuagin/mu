#!/bin/sh
# ci-aipr-fast-test.sh — regression tests for the ci-aipr fast path (mu-ash9p).
#
# Two mechanisms, both exercised offline (synthetic seat files and a throwaway
# repo — no model spend, no network):
#
#   1. converge.py `agree` — LIVE-SEAT quorum. A seat that timed out or returned
#      nothing recoverable is ABSENT for the round, not a dissenter (a reply that
#      lists findings without a verdict is a needs-changes, not an absence; a
#      seat whose process failed is named with its error); agreement
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
#   failed                   empty reply, exit=1, stderr names the error (the
#                            dead-roster-entry shape: 21 of 23 absences measured)
#   limit                    exit=1, stderr silent, stdout carries the provider's
#                            one-line refusal (claude -p at the session limit)
#   braces                   a complete needs-changes wrapped in prose with
#                            braces and followed by a second JSON object
#   quoted-example           VERDICT: needs-changes, then a QUOTED approve
#                            envelope from the diff, then the real envelope
#   prefix-only              VERDICT: needs-changes, then only a quoted approve
#                            envelope (the real one never came)
#   last-wins                no VERDICT line; a quoted approve envelope, then
#                            the real needs-changes envelope
#   dissent-then-quote       no VERDICT line; the real needs-changes envelope,
#                            then a quoted approve fixture
#   quotes-only              no VERDICT line; two quoted approve fixtures and
#                            "I still need to investigate"
#   json-only-approve        no VERDICT line; a single approve envelope
#   same-verdict-quote       VERDICT: needs-changes, the real envelope, then a
#                            quoted needs-changes fixture with no findings
#   off-contract             a JSON-only envelope whose verdict is "unclear"
#   off-contract-findings    verdict "blocked" beside a concrete high finding
#   preamble                 one sentence, THEN the VERDICT line and envelope
#   think-verdict            a tentative VERDICT: approve inside <think>, then
#                            the real needs-changes line and envelope
#   quoted-verdict-line      prose quoting "VERDICT: approve / VERDICT:
#                            needs-changes" on its own line, no conclusion
#   verdict-line-only        line 1 is VERDICT: needs-changes, no JSON (the
#                            truncation-safe prefix, kept)
#   quoted-contract-brace    the quoted slash line, then a bare "{", then prose
#   quoted-verdict-example   a quoted single VERDICT: approve line, a quoted
#                            example envelope, then "I still need to..."
#   reject                   VERDICT: reject with a matching envelope
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
    failed)
      : > "$1/r1.$2.out"
      printf 'Error: model_not_found\n' > "$1/r1.$2.err" ;;
    limit)
      printf "You've hit your session limit - resets 3:20pm (America/New_York)\n" > "$1/r1.$2.out" ;;
    braces)
      printf 'Looking at ${VAR} handling, see {below}.\nVERDICT: needs-changes\n{"verdict":"needs-changes","summary":"s","findings":[{"severity":"high","file":"x.rs","line":1,"issue":"a {b} c"}]}\nAside: {"note":"a second object"}\n' \
        > "$1/r1.$2.out" ;;
    quoted-example)
      printf 'VERDICT: needs-changes\nThe test writes {"verdict":"approve","summary":"No review concerns were recorded.","findings":[]} as its fixture.\n{"verdict":"needs-changes","summary":"s","findings":[{"severity":"high","file":"x.rs","line":1,"issue":"real"}]}\n' \
        > "$1/r1.$2.out" ;;
    prefix-only)
      printf 'VERDICT: needs-changes\nThe fixture is {"verdict":"approve","summary":"s","findings":[]} and\n' \
        > "$1/r1.$2.out" ;;
    last-wins)
      printf 'Compare the fixture {"verdict":"approve","summary":"s","findings":[]} with mine:\n{"verdict":"needs-changes","summary":"s","findings":[{"severity":"high","file":"x.rs","line":1,"issue":"real"}]}\n' \
        > "$1/r1.$2.out" ;;
    dissent-then-quote)
      printf '{"verdict":"needs-changes","summary":"s","findings":[{"severity":"high","file":"x.rs","line":1,"issue":"real"}]}\nThe fixture it ships is {"verdict":"approve","summary":"No review concerns were recorded.","findings":[]}.\n' \
        > "$1/r1.$2.out" ;;
    quotes-only)
      printf 'Compare fixture {"verdict":"approve","summary":"s","findings":[]} against fixture {"verdict":"approve","summary":"t","findings":[]}; I still need to investigate.\n' \
        > "$1/r1.$2.out" ;;
    json-only-approve)
      printf '{"verdict":"approve","summary":"s","findings":[]}\n' > "$1/r1.$2.out" ;;
    same-verdict-quote)
      printf 'VERDICT: needs-changes\n{"verdict":"needs-changes","summary":"s","findings":[{"severity":"high","file":"x.rs","line":1,"issue":"real"}]}\nAs in the fixture {"verdict":"needs-changes","summary":"s","findings":[]}.\n' \
        > "$1/r1.$2.out" ;;
    off-contract)
      printf '{"verdict":"unclear","summary":"s","findings":[]}\n' > "$1/r1.$2.out" ;;
    off-contract-findings)
      printf '{"verdict":"blocked","summary":"s","findings":[{"severity":"high","file":"x.rs","line":1,"issue":"unchecked access"}]}\n' > "$1/r1.$2.out" ;;
    preamble)
      printf "I've completed a thorough static review. Findings from my analysis:\nVERDICT: approve\n{\"verdict\":\"approve\",\"summary\":\"clean\",\"findings\":[]}\n" > "$1/r1.$2.out" ;;
    think-verdict)
      printf '<think>\nVERDICT: approve\nno wait, the guard is missing\n</think>\nVERDICT: needs-changes\n{"verdict":"needs-changes","summary":"s","findings":[{"severity":"high","file":"x.rs","line":1,"issue":"guard missing"}]}\n' > "$1/r1.$2.out" ;;
    quoted-verdict-line)
      printf 'The contract says the first line must be one of:\nVERDICT: approve / VERDICT: needs-changes\nI still need to investigate the marker script.\n' > "$1/r1.$2.out" ;;
    verdict-line-only)
      printf 'VERDICT: needs-changes\n' > "$1/r1.$2.out" ;;
    quoted-contract-brace)
      printf 'The required format is:\nVERDICT: approve / VERDICT: needs-changes\n{\nI still need to investigate.\n' > "$1/r1.$2.out" ;;
    quoted-verdict-example)
      printf 'The contract example reads:\nVERDICT: approve\n{"verdict":"approve","summary":"<1-2 sentences>","findings":[]}\nI still need to investigate the marker script.\n' > "$1/r1.$2.out" ;;
    reject)
      printf 'VERDICT: reject\n{"verdict":"reject","summary":"s","findings":[{"severity":"high","file":"x.rs","line":1,"issue":"real"}]}\n' \
        > "$1/r1.$2.out" ;;
  esac
  case "$3" in
    timeout|timeout-parsed) _exit=124; _prov=openrouter ;;
    skipped) _exit=75; _prov=ollama ;;
    failed|limit) _exit=1; _prov=openrouter ;;
    *) _exit=0; _prov=openrouter ;;
  esac
  echo "exit=$_exit retry=0 prov=$_prov model=$2 seam=[$_seam]" > "$1/r1.$2.done"
}

expect_eq() { # $1=label $2=expected $3=got
  if [ "$3" = "$2" ]; then echo "ok   $1"
  else echo "FAIL $1: expected '$2', got '$3'"; fails=$((fails + 1)); fi
}

findings_n() { # $1=file -> "<verdict> <number of findings>" of the envelope converge.py picks
  python3 -c 'import sys; sys.path.insert(0, sys.argv[1]); import converge as cv
d = cv.parse_out(sys.argv[2]) or {}; print(cv.seat_verdict(d), len(d.get("findings") or []))' \
    "$HERE/../review-panel" "$1"
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

# (a) the measured PR #608 shape: four seats agree, one returned nothing —
# a 404 on a roster entry that no longer existed. That read "unparsed" for four
# days; the census now names the error.
d=$(panel failed-seat rank0.opus-5=approve rank1.gpt-5.5=failed \
          rank2.glm=approve rank3.kimi=approve rank4.opus-4-8=approve)
check "4 live agree + 1 failed seat converges, and the failure is named" "$d" \
  "AGREE approve" "live 4/5: gpt-5.5 failed (exit 1: Error: model_not_found)" 0

# (a') a chatty seat: prose, no JSON at all.
d=$(panel unparsed-seat rank0.opus-5=approve rank1.gpt-5.5=unparsed \
          rank2.glm=approve rank3.kimi=approve rank4.opus-4-8=approve)
check "4 live agree + 1 unparsed seat converges" "$d" \
  "AGREE approve" "live 4/5: gpt-5.5 unparsed" 0

# A provider's one-line refusal on stdout with silent stderr: the census
# carries the line (claude -p at the operator's session limit, run 7).
d=$(panel limit-seat rank0.a=approve rank1.b=approve rank2.c=approve rank3.d=limit)
check "a seat refused by its provider is named with the refusal" "$d" \
  "AGREE approve" "live 3/4: d failed (exit 1: You've hit your session limit - resets 3:20pm (America/New_York))" 0

# An off-contract verdict is absent and named; "reject" reads as needs-changes.
d=$(panel off-contract-seat rank0.a=approve rank1.b=approve rank2.c=approve rank3.d=off-contract)
check "an off-contract verdict is absent, named with its verdict" "$d" \
  "AGREE approve" "live 3/4: d unparsed (verdict 'unclear' is off contract)" 0
if python3 "$HERE/../review-panel/parse.py" --check "$d/r1.rank3.d.out" 2>/dev/null; then
  echo "FAIL parse.py --check accepted an off-contract verdict (no re-ask would fire)"; fails=$((fails + 1))
else
  echo "ok   parse.py --check rejects an off-contract verdict so the re-ask fires"
fi
d=$(panel off-contract-findings-seat rank0.a=approve rank1.b=approve rank2.c=approve rank3.d=off-contract-findings)
check "an off-contract verdict WITH findings is dissent, not absence" "$d" \
  'SPLIT {"rank0.a": "approve", "rank1.b": "approve", "rank2.c": "approve", "rank3.d": "needs-changes"}' \
  "live 4/4" 1
d=$(panel preamble-seat rank0.a=approve rank1.b=approve rank2.c=preamble)
check "a preamble sentence before the VERDICT line does not void a declared approve" "$d" \
  "AGREE approve" "live 3/3" 0
d=$(panel verdict-line-shapes rank0.a=think-verdict rank1.b=quoted-verdict-line rank2.c=verdict-line-only)
expect_eq "a tentative VERDICT line inside reasoning is not the declaration" \
  "needs-changes 1" "$(findings_n "$d/r1.rank0.a.out")"
expect_eq "a quoted VERDICT line in prose with no envelope after it declares nothing" \
  "unparsed 0" "$(findings_n "$d/r1.rank1.b.out")"
expect_eq "a lone VERDICT line on line 1 is still scored (truncation-safe prefix)" \
  "needs-changes 0" "$(findings_n "$d/r1.rank2.c.out")"
d=$(panel quoted-shapes rank0.a=quoted-contract-brace rank1.b=quoted-verdict-example)
expect_eq "a quoted contract line followed by a bare brace declares nothing" \
  "unparsed 0" "$(findings_n "$d/r1.rank0.a.out")"
expect_eq "a quoted VERDICT line and example envelope followed by more prose declare nothing" \
  "unparsed 0" "$(findings_n "$d/r1.rank1.b.out")"
d=$(panel reject-seat rank0.a=approve rank1.b=approve rank2.c=approve rank3.d=reject)
check "VERDICT: reject reads as needs-changes" "$d" \
  'SPLIT {"rank0.a": "approve", "rank1.b": "approve", "rank2.c": "approve", "rank3.d": "needs-changes"}' \
  "live 4/4" 1

# A complete verdict wrapped in braces-bearing prose with a second object after
# it: the old first-'{' to last-'}' slice threw this away (measured on PR #611).
d=$(panel braces-seat rank0.a=approve rank1.b=approve rank2.c=approve rank3.d=braces)
check "a verdict wrapped in brace-bearing prose is still read" "$d" \
  'SPLIT {"rank0.a": "approve", "rank1.b": "approve", "rank2.c": "approve", "rank3.d": "needs-changes"}' \
  "live 4/4" 1
if python3 "$HERE/../review-panel/parse.py" --check "$d/r1.rank3.d.out"; then
  echo "ok   parse.py --check reads the same reply (no spurious re-ask)"
else
  echo "FAIL parse.py --check rejected a reply converge.py scored"; fails=$((fails + 1))
fi

# Which envelope is the reply when a reply carries more than one? This repo's
# own diffs quote `{"verdict":"approve"...}` fixtures (panel findings, PR #611).
d=$(panel envelope-choice rank0.a=quoted-example rank1.b=prefix-only rank2.c=last-wins rank3.d=same-verdict-quote)
expect_eq "a quoted approve before the real envelope does not win (VERDICT line agrees with the envelope)" \
  "needs-changes 1" "$(findings_n "$d/r1.rank0.a.out")"
expect_eq "a VERDICT line with only a quoted envelope after it keeps its own verdict" \
  "needs-changes 0" "$(findings_n "$d/r1.rank1.b.out")"
expect_eq "with no VERDICT line a needs-changes envelope is the reply" \
  "needs-changes 1" "$(findings_n "$d/r1.rank2.c.out")"
expect_eq "a trailing same-verdict fixture does not replace the real findings" \
  "needs-changes 1" "$(findings_n "$d/r1.rank3.d.out")"
d=$(panel envelope-choice-2 rank0.a=dissent-then-quote rank1.b=quotes-only rank2.c=json-only-approve)
expect_eq "a quoted approve AFTER the real needs-changes does not flip the seat" \
  "needs-changes 1" "$(findings_n "$d/r1.rank0.a.out")"
expect_eq "several quoted envelopes and no conclusion score nothing" \
  "unparsed 0" "$(findings_n "$d/r1.rank1.b.out")"
expect_eq "an approve with no VERDICT line is not an approval" \
  "unparsed 0" "$(findings_n "$d/r1.rank2.c.out")"

# The reply contract is the LAST thing a convergence prompt says, and it is the
# CONVERGENCE contract (concede/maintain/refute), not the round-1 one.
printf 'diff --git a/x b/x\n' > "$d/diff.txt"
python3 "$HERE/../review-panel/converge.py" prompt "$d/r1" 2 "$d/diff.txt" rank0.a "$d/r2.rank0.a.prompt" >/dev/null 2>&1
if tail -c 900 "$d/r2.rank0.a.prompt" | grep -q "REPLY FORMAT, restated" \
   && tail -c 900 "$d/r2.rank0.a.prompt" | grep -q '"refute"'; then
  echo "ok   the convergence prompt restates ITS contract last (refute array included)"
else
  echo "FAIL the convergence prompt does not end with the convergence contract"; fails=$((fails + 1))
fi

# ...and a per-seat clause (focus / seam) does not push it up the prompt again.
printf 'shared prompt\n' > "$d/shared.txt"
( HERE="$HERE/../review-panel"; . "$HERE/seat-prompt.sh"; seat_prompt "$d/shared.txt" "$d/seat.txt" "error handling" "" "" "" >/dev/null )
if tail -c 700 "$d/seat.txt" | grep -q "REPLY FORMAT, restated"; then
  echo "ok   a focused seat prompt still ends with the reply contract"
else
  echo "FAIL a focused seat prompt does not end with the reply contract"; fails=$((fails + 1))
fi

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
REASK_REPLY=approve
agent_dispatch() { # stub: record the cap the re-ask ran under, answer as told
  printf '%s\n' "${TIMEOUT:-unset}" > "$reask_dir/timeout-seen"
  if [ "$REASK_REPLY" = approve ]; then
    printf 'VERDICT: approve\n{"verdict":"approve","summary":"No review concerns were recorded.","findings":[]}\n'
  else
    printf 'VERDICT: needs-changes\n{"verdict":"needs-changes","summary":"s","findings":[{"severity":"high","file":"x.rs","line":3,"issue":"unchecked"}]}\n'
  fi
}
reask() { # $1=extra env assignment
  printf 'I looked at the change and it seems fine. What would you like next?\n' > "$reask_dir/r1.rank0.a.out"
  rm -f "$reask_dir/timeout-seen" "$reask_dir/r1.rank0.a.out.orig"
  ( HERE="$HERE/../review-panel"; . "$HERE/verdict-retry.sh"; ERRLOG=/dev/null
    eval "$1"; reask_if_unparsed openrouter m "$reask_dir/r1.rank0.a.out" )
  cat "$reask_dir/timeout-seen" 2>/dev/null
}
expect_cap "the verdict re-ask carries its own 180s cap, not the seat's" 180 \
  "$(reask 'TIMEOUT=900')"
expect_cap "the re-ask cap is env-settable" 60 \
  "$(reask 'TIMEOUT=900; MU_REVIEW_REASK_TIMEOUT_SECS=60')"

# The re-ask may rescue dissent; it may not manufacture an approve (measured:
# a turn-exhausted seat re-asked into "No review concerns were recorded").
if python3 "$HERE/../review-panel/parse.py" --check "$reask_dir/r1.rank0.a.out" 2>/dev/null; then
  echo "FAIL an approve reformatted from unfinished notes was promoted"; fails=$((fails + 1))
else
  echo "ok   a re-ask that approves is not promoted; the seat stays unparsed"
fi
REASK_REPLY=needs-changes; reask '' >/dev/null
if python3 "$HERE/../review-panel/parse.py" --rescuable "$reask_dir/r1.rank0.a.out" 2>/dev/null \
   && [ -f "$reask_dir/r1.rank0.a.out.orig" ]; then
  echo "ok   a re-ask that dissents with findings is promoted, original kept as .orig"
else
  echo "FAIL a needs-changes re-ask was not promoted (or the original was not kept)"; fails=$((fails + 1))
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
