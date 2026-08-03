#!/bin/sh
# converge-audit-test.sh — regression test for the convergence erasure audit.
#
# Fixtures under fixtures/converge/ are REAL captured panel runs (mu-mhzo,
# PR #504), not synthetic — do not regenerate them by hand:
#   erased-dissent  a known-bad diff the old gate passed; must ERASE
#   dead-seats      same defect filed `medium`; must ERASE, and is the only
#                   fixture that can discriminate the audit floor
#   clean-approve   benign; must stay CLEAN, so the gate can't "pass" by
#                   refusing everything
#
# usage: sh scripts/tests/converge-audit-test.sh
set -u
HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
CONVERGE="$HERE/../review-panel/converge.py"
FX="$HERE/fixtures/converge"
fails=0

check() { # $1=label $2=fixture $3=final-round $4=expect(CLEAN|ERASED) [$5=must-name]
  label="$1"; fx="$2"; rnd="$3"; want="$4"; needle="${5:-}"
  out=$(python3 "$CONVERGE" audit "$FX/$fx" "$rnd" 2>&1); rc=$?
  got=$(printf '%s' "$out" | awk '{print $1; exit}')
  if [ "$got" != "$want" ]; then
    echo "FAIL $label: expected $want, got: ${out%%$(printf '\n')*}"; fails=$((fails + 1)); return
  fi
  # CLEAN must exit 0, ERASED must exit nonzero — consensus.sh branches on this.
  if [ "$want" = CLEAN ] && [ "$rc" -ne 0 ]; then
    echo "FAIL $label: CLEAN must exit 0, got $rc"; fails=$((fails + 1)); return
  fi
  if [ "$want" = ERASED ] && [ "$rc" -eq 0 ]; then
    echo "FAIL $label: ERASED must exit nonzero"; fails=$((fails + 1)); return
  fi
  if [ -n "$needle" ] && ! printf '%s' "$out" | grep -q "$needle"; then
    echo "FAIL $label: expected report to name '$needle'"; fails=$((fails + 1)); return
  fi
  echo "ok   $label"
}

# The report must name the file, so the operator can adjudicate.
check "known-bad diff escalates instead of passing" erased-dissent 4 ERASED viewport.rs
check "dead-seat run would also have escalated"      dead-seats     4 ERASED viewport.rs
check "benign unanimous approve still passes"        clean-approve  1 CLEAN

# --- refutation semantics: synthetic round 4 over the real rounds 1-3 ---
# erased-dissent leaves TWO distinct open findings in viewport.rs, which is what
# makes it the fixture for per-finding matching.
TMP=$(mktemp -d "${TMPDIR:-/tmp}/converge-audit.XXXXXX") || exit 1
trap 'rm -rf "$TMP"' EXIT
V=crates/mu-solo/src/viewport.rs
CLEAR_SPAN='{"file":"'$V'","claim":"the narrowed clear span leaves rows 0..top uncleared in the shrink branch","evidence":"Read viewport.rs:640-660 and set_height():712 — the shrink branch clears old_y..old_y+old_height before the call, so rows 0..top are already blank."}'
TEST_ORACLE='{"file":"'$V'","claim":"the new test asserts clear span equals draw span, encoding the buggy invariant","evidence":"Read viewport.rs:1093 clear_span_never_exceeds_draw_span_after_viewport_moves_down — the helpers derive from independent formulas, so the equality is a real oracle."}'

round4() { # $1..: refute elements for rank0; other seats plainly approve
  rm -f "$TMP"/r4.*
  refutes=$(printf '%s,' "$@"); refutes=${refutes%,}
  printf 'VERDICT: approve\n{"verdict":"approve","summary":"s","refute":[%s],"findings":[]}\n' \
    "$refutes" > "$TMP/r4.rank0.gpt-5.5.out"
  echo "exit=0 retry=0 openai-codex/gpt-5.5" > "$TMP/r4.rank0.gpt-5.5.done"
  for seat in rank1.moonshotai_kimi-k3 rank2.z-ai_glm-5.2; do
    printf 'VERDICT: approve\n{"verdict":"approve","summary":"agree","findings":[]}\n' > "$TMP/r4.$seat.out"
    echo "exit=0 retry=0 x/y" > "$TMP/r4.$seat.done"
  done
}
expect() { # $1=label $2=CLEAN|ERASED
  out=$(python3 "$CONVERGE" audit "$TMP" 4 2>&1)
  case "$out" in
    "$2"*) echo "ok   $1" ;;
    *) echo "FAIL $1: expected $2, got: ${out%%\{*}"; fails=$((fails + 1)) ;;
  esac
}
cp "$FX"/erased-dissent/r[1-3].* "$TMP/" 2>/dev/null

# Refuting ONE finding must not retire an unrelated one in the same file.
round4 "$CLEAR_SPAN"
expect "refuting one finding leaves the other open" ERASED
round4 "$CLEAR_SPAN" "$TEST_ORACLE"
expect "refuting every open finding clears the ledger" CLEAN

# Bare assertion must retire nothing, or the audit buys nothing.
round4 '{"file":"'$V'","claim":"c","evidence":"I re-read it and it is fine."}'
expect "unevidenced assertion retires nothing" ERASED

# load() fabricates a medium finding on pseudo-path <reviewer> for exit=124.
# Nothing can cite it, so one flaky seat would block every later approve.
round4 "$CLEAR_SPAN" "$TEST_ORACLE"
printf 'VERDICT: approve\n{"verdict":"approve","findings":[]}\n' > "$TMP/r3.rank9.slow.out"
echo "exit=124 retry=1 prov=openrouter model=slow" > "$TMP/r3.rank9.slow.done"
expect "a timed-out seat does not poison the ledger" CLEAN

# Scope boundary, pinned deliberately: approving while still holding a finding
# is live dissent, not erasure (mu-wmww).
rm -f "$TMP"/r3.rank9.slow.*
round4 "$CLEAR_SPAN" "$TEST_ORACLE"
python3 - "$TMP/r4.rank2.z-ai_glm-5.2.out" <<'EOF'
import sys
open(sys.argv[1], "w").write(
    'VERDICT: approve\n{"verdict":"approve","summary":"holding","findings":'
    '[{"file":"crates/mu-solo/src/viewport.rs","line":651,"severity":"high",'
    '"issue":"the narrowed clear span still leaves rows 0..top uncleared in the shrink branch"}]}\n')
EOF
expect "a finding still held in the final round is not erasure" CLEAN

# MUST use dead-seats, whose only open finding is medium: on a fixture carrying
# high findings this passes whichever way the fallback goes, i.e. proves nothing.
floor_audit() { MU_REVIEW_AUDIT_FLOOR="$1" python3 "$CONVERGE" audit "$FX/dead-seats" 4 2>/dev/null; }
case "$(floor_audit high)" in
  CLEAN*) echo "ok   a high floor really does ignore a medium finding" ;;
  *) echo "FAIL floor=high should not flag a medium finding — fixture is not discriminating"; fails=$((fails + 1)) ;;
esac
case "$(floor_audit medum)" in
  ERASED*) echo "ok   invalid audit floor falls back to medium, not high" ;;
  *) echo "FAIL invalid floor relaxed the gate to high"; fails=$((fails + 1)) ;;
esac

# Ledger identity must not depend on arrival order, and a refutation must clear
# EVERY fragment of the defect it answers.
ORD=$(mktemp -d "${TMPDIR:-/tmp}/converge-order.XXXXXX") || exit 1
res=$(python3 - "$ORD" "$HERE/../review-panel" <<'EOF'
import json, os, sys
d, mod = sys.argv[1], sys.argv[2]
sys.path.insert(0, mod)
import converge

X1 = "the narrowed clear span leaves rows above top uncleared during the shrink branch"
X2 = "rows above top stay uncleared when the shrink branch narrows the clear span"
Y  = "the added unit test hardcodes an expected width and will break on resize"
def out(seat, issue):
    open(os.path.join(d, "r1.%s.out" % seat), "w").write(
        'VERDICT: needs-changes\n' + json.dumps({"verdict": "needs-changes", "findings": [
            {"file": "a/b.rs", "line": 1, "severity": "high", "issue": issue}]}) + "\n")
    open(os.path.join(d, "r1.%s.done" % seat), "w").write("exit=0 retry=0 x/y\n")

shapes = []
for order in (("rank0.a", "rank1.b", "rank2.c"), ("rank2.c", "rank1.b", "rank0.a")):
    for f in os.listdir(d):
        os.remove(os.path.join(d, f))
    for seat, issue in zip(order, (X1, X2, Y)):
        out(seat, issue)
    led = converge.build_ledger(d, 1)
    shapes.append(sorted(len(e["gists"]) for e in led))
ok_order = shapes[0] == shapes[1] == [1, 2]

# one evidenced refutation of defect X must clear the whole X entry
open(os.path.join(d, "r2.rank0.a.out"), "w").write(
    'VERDICT: approve\n' + json.dumps({"verdict": "approve", "refute": [
        {"file": "a/b.rs", "claim": X1,
         "evidence": "Read a/b.rs:1-60 — set_height() clears those rows before repaint, so the narrowed span in the shrink branch is correct."}],
        "findings": []}) + "\n")
open(os.path.join(d, "r2.rank0.a.done"), "w").write("exit=0 retry=0 x/y\n")
led = converge.build_ledger(d, 2)
x = [e for e in led if len(e["gists"]) == 2]
ok_ref = len(x) == 1 and x[0]["resolved"]

# A file with ONE open finding is the common case; an off-topic refutation must
# not resolve it.
for f in os.listdir(d):
    os.remove(os.path.join(d, f))
out("rank0.a", X1)
open(os.path.join(d, "r2.rank0.a.out"), "w").write(
    'VERDICT: approve\n' + json.dumps({"verdict": "approve", "refute": [
        {"file": "a/b.rs", "claim": "the logging macro is too verbose",
         "evidence": "Read a/b.rs:200-240 — tracing::debug!() there is gated behind a feature flag, so log_volume() stays bounded."}],
        "findings": []}) + "\n")
open(os.path.join(d, "r2.rank0.a.done"), "w").write("exit=0 retry=0 x/y\n")
solo = converge.build_ledger(d, 2)
ok_solo = len(solo) == 1 and not solo[0]["resolved"]
print("ORDER_OK=%s REFUTE_OK=%s SOLO_OK=%s shapes=%s" % (ok_order, ok_ref, ok_solo, shapes))
EOF
)
rm -rf "$ORD"
case "$res" in
  *ORDER_OK=True*) echo "ok   ledger identity is independent of arrival order" ;;
  *) echo "FAIL ledger shape depends on arrival order: $res"; fails=$((fails + 1)) ;;
esac
case "$res" in
  *REFUTE_OK=True*) echo "ok   one refutation clears every fragment it answers" ;;
  *) echo "FAIL refutation left a fragment open: $res"; fails=$((fails + 1)) ;;
esac
case "$res" in
  *SOLO_OK=True*) echo "ok   off-topic refutation cannot retire a file's lone finding" ;;
  *) echo "FAIL single-entry file skipped the substance check: $res"; fails=$((fails + 1)) ;;
esac

# A seat whose output carries a broken multibyte sequence must stay in the
# ledger rather than collapsing to None.
UTF=$(mktemp -d "${TMPDIR:-/tmp}/converge-utf8.XXXXXX") || exit 1
python3 - "$UTF" <<'EOF'
import os, sys
d = sys.argv[1]
good = (b'VERDICT: needs-changes\n{"verdict":"needs-changes","findings":[{"file":'
        b'"a/b.rs","line":1,"severity":"high","issue":"real defect in the shrink branch"}]}\n')
open(os.path.join(d, "r1.rank0.m.out"), "wb").write(good[:40] + b"\xe2\x80" + good[40:])
open(os.path.join(d, "r1.rank0.m.done"), "w").write("exit=0 retry=0 x/y\n")
EOF
n=$(python3 -c "
import sys; sys.path.insert(0,'$HERE/../review-panel'); import converge
print(len(converge.build_ledger('$UTF', 1)))" 2>/dev/null)
rm -rf "$UTF"
if [ "$n" = "1" ]; then
  echo "ok   a seat with invalid UTF-8 still reaches the ledger"
else
  echo "FAIL invalid-UTF-8 seat dropped out of the ledger (entries=$n)"; fails=$((fails + 1))
fi

[ "$fails" -eq 0 ] && { echo "converge-audit: all checks passed"; exit 0; }
echo "converge-audit: $fails check(s) FAILED"; exit 1
