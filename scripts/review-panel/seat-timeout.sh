# seat-timeout.sh — how long ONE panel seat may take (bead mu-ash9p).
#
# Sourced by dispatch.sh (round 1) and consensus.sh (convergence rounds) so both
# rounds cap a seat the same way. Defines seat_timeout(); defines no state.
#
# WHY per seat and not one number: measured 2026-09-09 from panel artifacts,
# remote API seats answer well inside 15 min (opus-5 13 min, opus-4-8 5, glm and
# kimi 1-2 when healthy), while LOCAL seats on our own boxes took 26 and 36 min
# in other sessions' panels. A flat cap has to be either long enough to waste
# 15 idle minutes on a dead API seat or short enough to throw away every healthy
# local one. So the cap follows the seat's provider class:
#
#   local  (ollama / vllm / an endpoint whose base_url is our hardware)
#          MU_REVIEW_LOCAL_SEAT_TIMEOUT_SECS, default 1800 — a slow local seat
#          costs wall-clock only; there is no per-token bill for waiting.
#   API    everything else: MU_REVIEW_SEAT_TIMEOUT_SECS, default 900 — past that
#          a hosted seat is hung, not thinking.
#
# A ranked entry in agent_roles.toml may state `timeout_secs` and that WINS: the
# roster is the one place that knows a particular seat's real latency. (Read
# straight from the ranked JSON both callers already parse — `agent-role` has no
# generic per-field accessor, only --cmd/--max-turns/--env, so honouring this
# needs no change to agent-role.)
#
# A seat that hits its cap is ABSENT for that round, not a dissenter, and the
# round finishes with the others (converge.py), which is what makes a short API
# cap safe.
#
# env: MU_REVIEW_SEAT_TIMEOUT_SECS        API seat cap, seconds (default 900)
#      MU_REVIEW_LOCAL_SEAT_TIMEOUT_SECS  local seat cap, seconds (default 1800)
#      MU_REVIEW_PROVIDER_CONFIG          mu config holding [[providers.endpoints]]
#                                         (default ~/.config/mu/config.toml)

# base_url of a named [[providers.endpoints]] entry, empty when there is none.
# Degrades silently without tq/jq/config: the caller then treats the seat as API,
# which errs toward the SHORTER cap — a misjudged local seat loses one round, a
# misjudged API seat would cost 30 idle minutes.
_seat_endpoint_base_url() { # $1=provider name
  _cfg="${MU_REVIEW_PROVIDER_CONFIG:-$HOME/.config/mu/config.toml}"
  [ -r "$_cfg" ] || return 1
  command -v tq >/dev/null 2>&1 || return 1
  command -v jq >/dev/null 2>&1 || return 1
  tq -o json -f "$_cfg" providers.endpoints 2>/dev/null \
    | jq -r --arg n "$1" '(.[]? | select(.name == $n) | .base_url) // empty' 2>/dev/null
}

# Our own hardware: loopback, RFC1918, .local, a dotless LAN hostname, or an
# IPv6 loopback / link-local / ULA literal. Any other IPv6 literal is somebody
# else's box (a bracketed host used to reduce to "[" and read as local — panel
# finding, PR #611).
_seat_url_is_local() { # $1=base url
  _h="${1#*://}"; _h="${_h%%/*}"
  case "$_h" in
    \[*) _h="${_h#\[}"; _h="${_h%%\]*}" ;;
    *)   _h="${_h%%:*}" ;;
  esac
  case "$_h" in
    localhost|127.*|*.local) return 0 ;;
    10.*|192.168.*|172.1[6-9].*|172.2[0-9].*|172.3[01].*) return 0 ;;
    ::1|fe80:*|f[cd]??:*) return 0 ;;
    *:*) return 1 ;;
    *.*) return 1 ;;
    "") return 1 ;;
    *) return 0 ;;
  esac
}

seat_is_local() { # $1=provider
  case "$1" in
    ollama|vllm|ollama-*|vllm-*) return 0 ;;
  esac
  _b="$(_seat_endpoint_base_url "$1")" || return 1
  [ -n "$_b" ] || return 1
  _seat_url_is_local "$_b"
}

# seat_timeout <provider> [<timeout_secs from the ranked entry>] -> seconds
seat_timeout() {
  _cfg_tmo="${2:-}"
  # Only a positive integer from the roster is honoured; anything else (a typo,
  # a comment, an empty field) falls through to the class default rather than
  # silently uncapping or zeroing the seat.
  case "$_cfg_tmo" in
    ''|*[!0-9]*|0) _cfg_tmo="" ;;
  esac
  if [ -n "$_cfg_tmo" ]; then
    printf '%s\n' "$_cfg_tmo"
  elif seat_is_local "$1"; then
    printf '%s\n' "${MU_REVIEW_LOCAL_SEAT_TIMEOUT_SECS:-1800}"
  else
    printf '%s\n' "${MU_REVIEW_SEAT_TIMEOUT_SECS:-900}"
  fi
}
