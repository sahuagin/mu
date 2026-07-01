#!/bin/sh
# with-ollama-lease — coordinate exclusive use of the shared ollama box via
# etcd's FAIR lock primitive (concurrency.Mutex, exposed by `etcdctl lock`)
# instead of agent-slot's single-key CAS. WAIT callers get an ordered FIFO turn
# (no race, no starvation); route-around + probe preserved; fail-open on outage.
#
# Modes (unchanged surface):
#   with-ollama-lease <cmd...>              WAIT: block in the FIFO queue until it's
#                                           our turn, acquire, run, release.
#   with-ollama-lease --skip-if-held <cmd>  ROUTE-AROUND: if held by anyone, exit 75
#                                           WITHOUT running (caller picks another model);
#                                           else acquire, run, release.
#   with-ollama-lease --held                PROBE (no run): exit 0 if held, else 1.
#
# vs v1: WAIT is now a fair queue (etcdctl lock), not a poll-race against in-loop
# re-lockers. Crash-safety is the lock's --ttl lease (self-expires). Fail-open on
# etcd outage is preserved (run WITHOUT the lease rather than block all inference).
#
# Tunables: OLLAMA_LEASE_NAME (default ollama-box), AGENT_SESSION_TTL (lease sec,
# default 1200), AGENT_SLOT_ETCD (etcd endpoints).
set -u

EP="${AGENT_SLOT_ETCD:-http://10.1.1.172:2379}"
NAME="${OLLAMA_LEASE_NAME:-ollama-box}"
LOCK="ollama-lock/${NAME}"                 # etcd lock prefix; holder key = ${LOCK}/<lease-hex>
TTL="${AGENT_SESSION_TTL:-1200}"
EC="etcdctl --endpoints=${EP}"

held() {   # is the lock currently held by anyone? holder key lives under ${LOCK}/
  [ -n "$($EC get --prefix --keys-only "${LOCK}/" 2>/dev/null | head -1)" ]
}
etcd_up() { $EC --command-timeout=3s endpoint health >/dev/null 2>&1; }

run_locked() {   # fair WAIT-acquire, run "$@", release — or fail-open if etcd is down
  if ! etcd_up; then
    echo "with-ollama-lease: etcd unreachable; running WITHOUT lease (fail-open)" >&2
    exec "$@"
  fi
  # command-timeout=0: don't bound the wrapped command; the lock's fair queue +
  # the --ttl lease bound the wait (our turn comes, and a crashed holder expires).
  exec $EC --command-timeout=0 lock --ttl "$TTL" "$LOCK" -- "$@"
}

case "${1:-}" in
  --held)
    held && exit 0 || exit 1
    ;;
  --skip-if-held)
    shift
    [ "$#" -gt 0 ] || { echo "with-ollama-lease: --skip-if-held needs a command" >&2; exit 2; }
    if etcd_up && held; then
      echo "with-ollama-lease: ${NAME} held by another; routing around (exit 75)" >&2
      exit 75
    fi
    run_locked "$@"
    ;;
  "")
    echo "with-ollama-lease: needs a command (or --held / --skip-if-held)" >&2
    exit 2
    ;;
  *)
    run_locked "$@"      # WAIT — fair FIFO queue
    ;;
esac
