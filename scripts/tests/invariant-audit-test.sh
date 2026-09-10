#!/usr/bin/env bash
# invariant-audit-test.sh — the audit's fixture suite plus one real run.
#
# bead: mu-invariant-audit-ratchet-8vfks. The self-test builds a throwaway tree
# and checks the three behaviours the gate exists for: at baseline it passes,
# one added site fails naming the rule and EVERY site, removed sites print the
# lower-baseline hint. Then the audit runs against this checkout in report
# mode, which must at least parse the shapes file. No model spend, no network.
set -u
TEST_DIR="$(cd "$(dirname "$0")" && pwd)"
AUDIT="$TEST_DIR/../invariant-audit.py"
[ -f "$AUDIT" ] || { echo "invariant-audit-test: $AUDIT missing" >&2; exit 2; }
rc=0
python3 "$AUDIT" --self-test || rc=1
# Shape smoke: the providers-standalone entry in the REAL shapes file must be the
# parsed kind (a line regex over Cargo syntax leaked: quoting, comments, renames).
python3 - "$TEST_DIR/../../.invariants.toml" <<'PY' || rc=1
import sys, tomllib
inv = next(i for i in tomllib.load(open(sys.argv[1], "rb"))["invariant"] if i["id"] == "providers-standalone")
ok = inv.get("kind") == "cargo-dependency" and inv.get("crate") == "mu-core"
print("providers-standalone shape:", "ok (parsed cargo-dependency on mu-core)" if ok else f"WRONG: {inv}")
sys.exit(0 if ok else 1)
PY

# --no-base: the real run only has to parse the shapes file and enumerate; BASE
# pinning is covered by the fixture suite, and `main` need not resolve in a
# shallow CI checkout.
python3 "$AUDIT" --root "$TEST_DIR/../.." --report --no-base >/dev/null || rc=1
exit $rc
