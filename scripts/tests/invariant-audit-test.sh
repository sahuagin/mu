#!/usr/bin/env bash
# invariant-audit-test.sh — this repo's shapes file against the installed
# audit tool. The tool itself (agent_tools `invariant-audit`, tree-sitter;
# bead at-zzb) carries its own fixture suite; what only this repo can check is
# that ITS .invariants.toml is wired the way its rules mean, and that the
# installed tool parses it. No model spend, no network.
set -u
TEST_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$TEST_DIR/../.."
rc=0
if ! command -v invariant-audit >/dev/null 2>&1; then
  echo "invariant-audit-test: the installed tool is missing — cargo install --git https://github.com/sahuagin/agent_tools --rev <pin in AGENTS.md> invariant-audit" >&2
  exit 2
fi
# Shape smoke: the providers-standalone entry must be the parsed kind (a line
# regex over Cargo syntax leaked: quoting, comments, renames).
python3 - "$ROOT/.invariants.toml" <<'PY' || rc=1
import sys, tomllib
invs = tomllib.load(open(sys.argv[1], "rb")).get("invariant", [])
inv = next((i for i in invs if i.get("id") == "providers-standalone"), None)
if inv is None:
    print("providers-standalone shape: MISSING — the entry was removed or renamed")
    sys.exit(1)
ok = inv.get("kind") == "cargo-dependency" and inv.get("crate") == "mu-core"
print("providers-standalone shape:", "ok (parsed cargo-dependency on mu-core)" if ok else f"WRONG: {inv}")
sys.exit(0 if ok else 1)
PY
# The installed tool must parse and enumerate this repo's shapes file, invoked
# exactly as the `invariants-report` recipe invokes it. --no-base: BASE pinning
# is the tool's own test suite's job, and `main` need not resolve in a shallow
# CI checkout. --report never fails on counts; a shapes file the tool cannot
# read is exit 2 (its documented contract), which is what this catches.
out="$(invariant-audit --root "$ROOT" --report --no-base 2>&1)" || { echo "$out"; rc=1; }
exit $rc
