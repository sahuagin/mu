# seat-slot.sh — a seat on our own llama-server never queues on a full box
# (bead mu-review-lease-flashnext-t2jah).
#
# Sourced by dispatch.sh (round 1) and consensus.sh (convergence rounds) AFTER
# seat-timeout.sh (needs its _seat_endpoint_base_url / _seat_url_is_local).
# Defines seat_slots_state and seat_route; defines no state.
#
# WHY: the conformance seam sits on `flashnext`, the LAN llama-server (-np 2:
# one slot is the operator's interactive session, the other is whoever asks
# first). An ollama seat routes around a held box (with-ollama-lease
# --skip-if-held -> exit 75, seat absent); a llama-server seat had no such
# path — its request queued INSIDE the server until a slot freed, so with two
# boards running the second's seam seat waited up to its local cap, and
# sessions took to serializing whole boards by hand over the dialogue channel,
# which idled a session for half an hour on 2026-09-18. Operator: "is it not
# possible to just skip that spot and run another on openrouter?" It is:
# llama-server answers GET /slots with each slot's is_processing, so the seat
# asks first and, finding no free slot, runs its SAME prompt on the rank's
# declared fallback instead of waiting. Boards no longer wait on each other;
# a collision costs one seat on a hosted model.
#
# Roster keys on the ranked entry (agent_roles.toml):
#   fallback_provider / fallback_model   where the seat goes when the box is
#       full or down. Both or neither. With neither the seat dispatches as
#       before and waits on the box — never worse than today. An EXCLUSIVE seam
#       seat should declare one: converge.py withholds an approve while it is
#       absent (PR #611), so a skipped seam seat would stall the board instead.
#
# env: MU_REVIEW_SLOT_PROBE=0               every seat dispatches on its primary
#                                          (an explicit local-only run; mirrors
#                                          AI_REVIEW_OLLAMA_SKIP_IF_HELD=0)
#      MU_REVIEW_SLOT_PROBE_TIMEOUT_SECS    cap on the /slots probe (default 5)
#
# The probe is a snapshot: two boards probing in the same second can both see
# the one free slot, and then one of them queues as before. That costs one slow
# seat, not a failed one, and is accepted rather than locked around.

# seat_slots_state <base_url> [<api_key>] -> free | busy | down | noprobe
#   free     at least one slot is idle
#   busy     every slot is processing
#   down     the endpoint did not answer (connection refused, timeout)
#   noprobe  nothing is known: it answered but not with a slot list (/slots
#            disabled — llama-server --no-slots returns an error object — or
#            a server with no such route: vllm, ollama), or the prober itself
#            is missing (no curl). The caller dispatches as before.
# The api key, when the endpoint has one, goes as the Bearer header mu sends
# with a request; a box behind `--api-key` answers 401 to a bare probe. It is
# handed to curl as a config on stdin (-K -), never in argv, where any local
# process could read it from ps for the life of the probe.
# llama-server has reported a slot as `is_processing` (current) and as
# `state` (0 = idle, older builds); both are read.
seat_slots_state() {
  command -v curl >/dev/null 2>&1 || { printf 'noprobe\n'; return 0; }
  if [ -n "${2:-}" ]; then
    _ss_body=$(printf 'header = "Authorization: Bearer %s"\n' "$(printf '%s' "$2" | sed 's/[\\"]/\\&/g')" \
      | curl -fsS -m "${MU_REVIEW_SLOT_PROBE_TIMEOUT_SECS:-5}" -K - "${1%/}/slots" 2>/dev/null)
  else
    _ss_body=$(curl -fsS -m "${MU_REVIEW_SLOT_PROBE_TIMEOUT_SECS:-5}" "${1%/}/slots" 2>/dev/null)
  fi
  _ss_rc=$?
  case "$_ss_rc" in
    0)  ;;
    22) printf 'noprobe\n'; return 0 ;;        # an HTTP error: the server is up, /slots is not
    126|127) printf 'noprobe\n'; return 0 ;;   # the prober, not the box, is broken
    *)  printf 'down\n'; return 0 ;;
  esac
  _ss_state=$(printf '%s' "$_ss_body" | jq -r '
    if type == "array" and length > 0 then
      (if any(.[]; (.is_processing == false) or (.is_processing == null and .state == 0))
       then "free" else "busy" end)
    else "noprobe" end' 2>/dev/null)
  case "$_ss_state" in
    free|busy) printf '%s\n' "$_ss_state" ;;
    *) printf 'noprobe\n' ;;
  esac
}

# seat_route <provider> <model> [<fallback_provider> <fallback_model>]
#   -> "<provider> <model> <route>", the seat to actually dispatch:
#   primary                    not a probed seat, or the box has a free slot,
#                              or nothing could be learned (noprobe)
#   fallback:<busy|down>       the box is full / down and the seat goes to its
#                              roster fallback
#   queued:<busy|down>         the box is full / down and the rank declares no
#                              fallback: dispatch as before (it waits)
# Only a seat whose provider resolves to a [[providers.endpoints]] entry on our
# own hardware is probed; ollama/vllm seats have their own paths (the lease,
# no /slots) and hosted seats have nothing to probe.
seat_route() {
  _sr_prov=$1; _sr_model=$2; _sr_fprov=${3:-}; _sr_fmodel=${4:-}
  if [ "${MU_REVIEW_SLOT_PROBE:-1}" = "0" ]; then
    printf '%s %s primary\n' "$_sr_prov" "$_sr_model"; return 0
  fi
  case "$_sr_prov" in
    ollama|ollama-*|vllm|vllm-*) printf '%s %s primary\n' "$_sr_prov" "$_sr_model"; return 0 ;;
  esac
  _sr_base=$(_seat_endpoint_base_url "$_sr_prov") || _sr_base=""
  if [ -z "$_sr_base" ] || ! _seat_url_is_local "$_sr_base"; then
    printf '%s %s primary\n' "$_sr_prov" "$_sr_model"; return 0
  fi
  _sr_state=$(seat_slots_state "$_sr_base" "$(_seat_endpoint_api_key "$_sr_prov")")
  case "$_sr_state" in
    free|noprobe) printf '%s %s primary\n' "$_sr_prov" "$_sr_model" ;;
    *)
      if [ -n "$_sr_fprov" ] && [ -n "$_sr_fmodel" ]; then
        printf '%s %s fallback:%s\n' "$_sr_fprov" "$_sr_fmodel" "$_sr_state"
      else
        printf '%s %s queued:%s\n' "$_sr_prov" "$_sr_model" "$_sr_state"
      fi ;;
  esac
}
