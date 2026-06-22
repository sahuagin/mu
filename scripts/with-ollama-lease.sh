#!/bin/sh
# with-ollama-lease — run a command while holding exclusive use of the shared
# ollama box, coordinated through agent-slot's etcd session registry.
#
# WHY: the box can't co-resident two large models; concurrent different-model
# loads evict/reload-thrash each other (bead mu-0pqk). mu never sends num_ctx
# (it talks ollama over the Anthropic wire), so the window is fixed at the
# server and the ONLY lever against thrash is serialising who uses the box.
# This is a COOPERATIVE lock: every consumer (the mu ask / claude -p launcher,
# the review panel, mu-solo, benchmarks) must acquire it or route around it.
#
# Reuses agent-slot's session registry as a named mutex on "ollama-box":
# `register` is atomic CAS that REFUSES a name held by a different owner, and
# the etcd lease self-expires at TTL so a crashed holder never wedges the box.
# The etcd endpoint comes from agent-slot's own default (AGENT_SLOT_ETCD).
#
# Deploy: tracked here; ~/.local/bin/with-ollama-lease is a symlink to this file
# (callable bare by the review scripts and by the operator for `with-ollama-lease
# mu-solo`). Source of truth is the repo, not the bin copy.
#
# Modes:
#   with-ollama-lease <cmd...>              WAIT: poll until free, acquire, run, release.
#   with-ollama-lease --skip-if-held <cmd>  ROUTE-AROUND: if another owner holds it,
#                                           exit 75 (EX_TEMPFAIL) WITHOUT running so the
#                                           caller can pick a non-ollama model; else
#                                           acquire, run, release.
#   with-ollama-lease --held                PROBE (no run): exit 0 if held by ANOTHER
#                                           owner (caller should route around), else 1.
#
# Identity: the holder is $AGENT_SESSION_OWNER. A multi-call consumer (e.g. one
# ai-review run) should export ONE owner so its own calls agree and OTHER runs
# are excluded; defaults to a per-invocation id otherwise.
#
# Fail-open: an etcd outage makes the read-only probe report "not held" and the
# acquire fail, so the command runs WITHOUT the lease rather than blocking all
# local inference — a coordination outage must not halt work (it logs a warning).
#
# Tunables:
#   OLLAMA_LEASE_NAME          mutex name        (default: ollama-box)
#   AGENT_SESSION_OWNER        holder identity   (default: ollama-lease/<host>/<pid>)
#   AGENT_SESSION_TTL          lease TTL seconds (default: 1200, > the 900s reviewer cap)
#   OLLAMA_LEASE_WAIT_TIMEOUT  WAIT-mode cap     (default: 1800)
#   OLLAMA_LEASE_POLL          WAIT poll seconds (default: 5)
#   AGENT_SLOT_ETCD            etcd endpoints    (default: agent-slot's own default)
set -u

NAME="${OLLAMA_LEASE_NAME:-ollama-box}"
: "${AGENT_SESSION_OWNER:=ollama-lease/$(hostname -s)/$$}"
export AGENT_SESSION_OWNER
: "${AGENT_SESSION_TTL:=1200}"
export AGENT_SESSION_TTL
POLL="${OLLAMA_LEASE_POLL:-5}"
WAIT_TIMEOUT="${OLLAMA_LEASE_WAIT_TIMEOUT:-1800}"

# Read-only: is NAME registered by an owner OTHER than me? agent-slot sessions
# prints "SESSION OWNER TTL" columns; match the name row and compare owner.
held_by_other() {
  agent-slot sessions 2>/dev/null | awk -v n="$NAME" -v me="$AGENT_SESSION_OWNER" '
    $1==n && $2!=me { found=1 }
    END { exit(found?0:1) }
  '
}

acquire() { agent-slot register "$NAME" >/dev/null 2>&1; }   # 0=ours now, !=0=held by other
release() { agent-slot unregister "$NAME" >/dev/null 2>&1 || true; }

case "${1:-}" in
  --held)
    held_by_other && exit 0 || exit 1
    ;;
  --skip-if-held)
    shift
    [ "$#" -gt 0 ] || { echo "with-ollama-lease: --skip-if-held needs a command" >&2; exit 2; }
    # Route-around decision is the read-only probe, so an etcd OUTAGE (probe
    # fails -> "not held") falls through to acquire-or-fail-open rather than
    # being misread as "held".
    if held_by_other; then
      echo "with-ollama-lease: ollama box held by another owner; skipping (route around)" >&2
      exit 75
    fi
    if acquire; then
      trap release EXIT INT TERM
      "$@"
    else
      echo "with-ollama-lease: could not acquire lease (etcd unreachable or lost a race); proceeding WITHOUT lease (fail-open)" >&2
      "$@"
    fi
    ;;
  "")
    echo "with-ollama-lease: needs a command (or --held / --skip-if-held)" >&2
    exit 2
    ;;
  *)
    # WAIT: block while another owner holds it (read-only probe, so an etcd
    # outage reads as "free" and falls through to acquire-or-fail-open).
    waited=0
    while held_by_other; do
      if [ "$waited" -ge "$WAIT_TIMEOUT" ]; then
        echo "with-ollama-lease: timed out after ${WAIT_TIMEOUT}s waiting; proceeding WITHOUT lease (fail-open)" >&2
        break
      fi
      sleep "$POLL"; waited=$((waited + POLL))
    done
    if acquire; then
      trap release EXIT INT TERM
      "$@"
    else
      echo "with-ollama-lease: could not acquire lease (etcd unreachable or lost a race); proceeding WITHOUT lease (fail-open)" >&2
      "$@"
    fi
    ;;
esac
