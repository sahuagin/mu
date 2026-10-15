#!/usr/bin/env bash
# mu-explicit-empty-tools-y2tgo: preserve an explicit empty argument, not omission.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
cat > "$TMP/mu" <<'STUB'
#!/usr/bin/env python3
import json, os, sys
with open(os.environ['ARGS'], 'w') as f:
    json.dump(sys.argv[1:], f)
STUB
chmod +x "$TMP/mu"
printf 'hello\n' > "$TMP/prompt"
for mode in unset empty nonempty; do
  (
    unset TOOLS DISPATCH_ROLE SYSPROMPT MAX_TURNS
    TIMEOUT=5 TIMEOUT_KILL_AFTER=1
    case "$mode" in
      empty) TOOLS='' ;;
      nonempty) TOOLS='read,grep' ;;
    esac
    export ARGS="$TMP/$mode.json"
    MU="$TMP/mu" ERRLOG="$TMP/err" AGENT_DISPATCH_NO_LEASE=1
    export MU ERRLOG AGENT_DISPATCH_NO_LEASE
    . "$ROOT/scripts/lib/agent-dispatch.sh"
    agent_dispatch openai test "$TMP/prompt"
  )
done
python3 - "$TMP" <<'PY'
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
for mode, expected in [('unset', 'read,grep'), ('empty', ''), ('nonempty', 'read,grep')]:
    args = json.loads((root / (mode + '.json')).read_text())
    assert args.count('--tools') == 1, (mode, args)
    assert args[args.index('--tools') + 1] == expected, (mode, args)
    assert '--ephemeral' not in args, args
    print('PASS forwarding:', mode)
PY
