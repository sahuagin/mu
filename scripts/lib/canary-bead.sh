#!/bin/sh
# canary-bead.sh — idempotent bead filing for the protocol canaries.
#
# The canaries run from cron. Before this lib every failing run created a
# fresh `<slug>-<hash>` bead, so one persistent failure became a pile of
# identical P1 siblings nobody triaged (eight of them, 2026-07-20..08-31;
# mu-ztmla). Now a canary owns at most ONE live bead at a time:
#
#   - a live (not closed) bead carrying the canary's label exists -> append
#     this run as a dated comment; if the failure set changed and the title
#     is still the machine-generated one, retitle it
#   - none with the label, but a live one with the canary's slug in its id
#     (filed before the label existed) whose title or description is still
#     the machine-generated one -> adopt it: label it, then as above
#   - none                                                        -> create one
#
# The key is the label `canary:<slug>`, set on create. A machine-owned label
# survives the edits a human makes while triaging a P1 (retitle, reprioritise,
# claim); it is checked on every returned record, not just requested of the
# server. The slug in the id is advisory: preferred when present, not
# required. Adoption is the one place the label cannot help, so it searches
# by the canary's own title and description text and requires the slug in
# the id; a pre-label bead a human has retitled AND redescribed, or whose id
# does not carry the slug, is not found and gets a sibling.
#
# Usage — source this file, then:
#   canary_file_bead <slug> <title> <body> [<actor>]
#     slug   id slug, e.g. openai-protocol-canary-drift (also the default actor)
#     title  "<Canary name> failed: <failure set>"; the text before the colon
#            is the canary's name and marks a title as machine-generated
#     body   run detail (host, failures, log path): the description on create,
#            a dated comment on reuse
#
# Prints ONE line for the canary's log and returns 0 when the run is on record:
#   filed <id>      a new bead
#   updated <id>    commented on the live one (and retitled if the set changed)
# with a parenthesised note whenever something degraded on the way, e.g.
#   filed <id> (no dedupe: lookup failed: ...)   the lookup broke, so the run was
#                                                filed without it — an alert never
#                                                waits on dedupe, and never hides it
#   filed <id> (no adoption: lookup failed: ...) the pre-label lookup broke
#   updated <id> (adopted an unlabelled bead)    a pre-label bead was found
#   updated <id> (retitle failed)                the comment landed, title is stale
#   updated <id> (2 live siblings, ...)          more than one live bead exists;
#                                                this run went to the preferred
#                                                one (slug in id, then oldest)
#   filed (id not reported: client rejected ...) the client refused a newer flag
# Prints nothing and returns non-zero when this run was not recorded:
#   1  no beads client, no url, or no temp file
#   2  the comment on the live bead failed (the bead itself stays live — and,
#      on the adoption path, is already labelled)
#   3  create failed in a way that may already have written a bead (never
#      retried blind — see below)
# so the caller's one log line can name the cause; the client's own error
# text is not surfaced.
# Filing is best-effort: callers must not let it change the canary's own exit.
#
# Every beads call is bounded by `timeout` when the host has one
# ($CANARY_BEAD_TIMEOUT seconds each, default 60), so a hung beadsd cannot
# hang a cron run. Not atomic: lookup and create are separate calls with no
# lock, so two runs of the same canary failing in the same second could both
# create. Each host runs one weekly cron, and the pile this lib ends was
# sequential weeks, not concurrent runs; if it ever happens the next run
# reports the sibling count and names the record it used. One small temp file
# holds stderr between calls; a signal mid-call can leave it in $TMPDIR.
#
# beadsd url: $BEADS_REMOTE, else the `mu=` line of ~/.config/beads/remotes.env.
#
# beads CLI surface used — verified 2026-09-03 against br 0.2.15 through the
# `beads --url <u> exec --` relay. scripts/tests/canary-bead-test.sh pins it
# with a strict fake, re-checks it read-only against the real client inside
# the pre-PR gate (CANARY_BEAD_CONTRACT=1), and exercises it end to end,
# adoption included, on a disposable slug under CANARY_BEAD_LIVE=1:
#   list --all --format json --limit N --label <l> | --title-contains <t>
#                                                  | --desc-contains <t>
#       -> {"issues":[{id,status,title,created_at,labels,...}],...}
#          (--all: every status; closed records are dropped here, so nothing
#          depends on the client's default status set)
#   create ... --labels <l> --silent    -> prints the bare id
#   comments add <id> --actor <a> --message <m>
#   update <id> --title <t> | --add-label <l>, with --actor <a>

# Run the client under a wall-clock bound when the host has `timeout`.
cb_beads() {
  if [ -n "$cb_timeout_bin" ]; then
    "$cb_timeout_bin" "${CANARY_BEAD_TIMEOUT:-60}" beads "$@"
  else
    beads "$@"
  fi
}

# cb_lookup <advisory|strict> <list filter args...>
# Prints "id live-count retitle" (space-separated: an id and two integers, so
# no free text such as a title crosses the boundary) for the oldest live
# match, nothing when there is none. live-count is of every live record the
# mode treats as ours, not only the preferred subset, so a label-only sibling
# is still counted; retitle is 1 when the record's title is still exactly a
# machine-generated one ("<canary name>: <check names>", nothing a human
# added, even after that prefix) and differs from ours. advisory (the label lookup): only records that carry our
# label count, those whose id also carries the slug are preferred; strict (the
# adoption lookups): the slug in the id is required, the label is not.
# Returns 1 (client call failed) or 2 (payload unparseable, or a valid JSON
# envelope without an issues array) with the reason as the first line of
# $cb_err. stdout and stderr are kept apart so a warning on
# a successful list cannot corrupt the parse.
cb_lookup() {
  cb_mode=$1; shift
  cb_list=$(cb_beads --url "$cb_url" exec -- list --all --format json --limit 500 "$@" 2>"$cb_err")
  cb_rc=$?
  if [ "$cb_rc" -ne 0 ]; then
    [ -s "$cb_err" ] || echo "exit $cb_rc" > "$cb_err"
    return 1
  fi
  # A bare array or an object carrying an `issues` array; anything else —
  # including an exit-0 call that printed nothing, which jq would pass
  # through as zero values — is a changed envelope and must be reported,
  # not read as "no live bead".
  if [ -z "$(printf '%s' "$cb_list" | tr -d '[:space:]')" ]; then
    echo "empty list payload" > "$cb_err"; return 2
  fi
  printf '%s\n' "$cb_list" | jq -r --arg key "-$cb_slug-" --arg mode "$cb_mode" \
      --arg label "$cb_label" --arg title "$cb_title" --arg name "$cb_name: " '
      (if type == "array" then .
       elif type == "object" and (.issues | type) == "array" then .issues
       else error("list payload has no issues array") end)
      | [ .[] | select(.status != "closed")
            | select($mode == "strict" or ((.labels // []) | index($label) != null)) ] as $live
      | [ $live[] | select(.id | contains($key)) ] as $ours
      | (if $mode == "strict" then $ours else $live end) as $all
      | (if ($ours | length) > 0 or $mode == "strict" then $ours else $live end)
      | sort_by(.created_at)
      | if length == 0 then empty
        else .[0] as $r
           | "\($r.id) \($all | length) \(if (($r.title // "") | startswith($name))
                 and (($r.title[($name | length):]) | test("^[a-z0-9_ ]+$"))
                 and ($r.title != $title) then 1 else 0 end)"
        end' 2>"$cb_err" || return 2
}

cb_create() {
  cb_beads --url "$cb_url" exec -- create --title "$cb_title" --slug "$cb_slug" \
    --type bug --priority P1 --description "$cb_body" --actor "$cb_actor" "$@"
}

# Did the client refuse a flag? A heuristic on the client's stderr, keyed to
# the argument-parser messages br (clap) prints before doing any work; the
# create retry below rests on it, so it is kept narrow on purpose.
cb_flag_rejected() {
  grep -qiE "unexpected argument|found argument '[^']*' which wasn't expected" "$1"
}

canary_file_bead() {
  cb_slug=$1; cb_title=$2; cb_body=$3; cb_actor=${4:-$1}
  cb_label="canary:$cb_slug"
  cb_name=${cb_title%%:*}
  command -v beads >/dev/null 2>&1 || return 1
  cb_url=${BEADS_REMOTE:-$(sed -n 's/^mu=[[:space:]]*//p' "$HOME/.config/beads/remotes.env" 2>/dev/null | head -n 1 | tr -d '[:space:]' || true)}
  [ -n "$cb_url" ] || return 1
  cb_timeout_bin=$(command -v timeout 2>/dev/null || true)
  cb_err=$(mktemp "${TMPDIR:-/tmp}/canary-bead.XXXXXX") || return 1

  # --- lookup ---------------------------------------------------------------
  # Found, none, or the lookup itself broke; the third is reported in the
  # output line, never swallowed.
  cb_existing=""; cb_note=""
  if ! command -v jq >/dev/null 2>&1; then
    cb_note="no dedupe: jq missing"
  elif cb_existing=$(cb_lookup advisory --label "$cb_label"); then
    if [ -z "$cb_existing" ]; then
      # Nothing labelled. Beads filed before the label existed (the pre-lib
      # code, or the last-resort create below) carry the slug in their id but
      # no label: adopt the oldest live one and label it, so the next run's
      # label lookup finds it directly. The server can only filter on text,
      # so the search is by the canary's own title, then by its description
      # (which a human triaging the bead is less likely to have rewritten).
      # A failing adoption lookup is reported like a failing primary lookup —
      # it is not the same as nothing to adopt.
      if cb_found=$(cb_lookup strict --title-contains "$cb_name") \
         && { [ -n "$cb_found" ] || cb_found=$(cb_lookup strict --desc-contains "${cb_name% failed} on "); }; then
        if [ -n "$cb_found" ]; then
          cb_existing=$cb_found
          cb_id=${cb_existing%% *}
          if cb_beads --url "$cb_url" exec -- update "$cb_id" --add-label "$cb_label" \
               --actor "$cb_actor" >/dev/null 2>&1; then
            cb_note="adopted an unlabelled bead"
          else
            cb_note="adopted an unlabelled bead; could not add the dedupe label"
          fi
        fi
      else
        case $? in
          2) cb_note="no adoption: unexpected list payload: $(head -n 1 "$cb_err")" ;;
          *) cb_note="no adoption: lookup failed: $(head -n 1 "$cb_err")" ;;
        esac
      fi
    fi
  else
    case $? in
      2) cb_note="no dedupe: unexpected list payload: $(head -n 1 "$cb_err")" ;;
      *) cb_note="no dedupe: lookup failed: $(head -n 1 "$cb_err")" ;;
    esac
    cb_existing=""
  fi

  # --- reuse ----------------------------------------------------------------
  if [ -n "$cb_existing" ]; then
    cb_id=${cb_existing%% *}
    cb_rest=${cb_existing#* }
    cb_live=${cb_rest%% *}
    cb_retitle=${cb_rest#* }
    if ! cb_beads --url "$cb_url" exec -- comments add "$cb_id" --actor "$cb_actor" \
           --message "run $(date -u +%Y-%m-%dT%H:%M:%SZ): $cb_body" >/dev/null 2>&1; then
      rm -f "$cb_err"; return 2
    fi
    # Retitle only a machine-generated title that changed; a human's triage
    # title stays (the lookup decided, see cb_lookup).
    if [ "${cb_retitle:-0}" = 1 ] \
       && ! cb_beads --url "$cb_url" exec -- update "$cb_id" --title "$cb_title" \
              --actor "$cb_actor" >/dev/null 2>&1; then
      cb_note="${cb_note:+$cb_note; }retitle failed"
    fi
    if [ "${cb_live:-1}" -gt 1 ]; then
      cb_note="${cb_note:+$cb_note; }$cb_live live siblings, this run went to $cb_id; close the rest"
    fi
    rm -f "$cb_err"
    echo "updated $cb_id${cb_note:+ ($cb_note)}"
    return 0
  fi

  # --- create ---------------------------------------------------------------
  if cb_id=$(cb_create --labels "$cb_label" --silent 2>"$cb_err"); then
    rm -f "$cb_err"
    echo "filed ${cb_id:-(id not reported)}${cb_note:+ ($cb_note)}"
    return 0
  fi
  # Retry ONLY when the client's stderr says it refused a flag, which the
  # parser does before anything is written. Any other failure may have
  # created the bead server-side, and a blind retry would file the very
  # sibling this lib exists to prevent — so that case is reported as not
  # filed instead. --silent is the likelier casualty of a
  # client skew, so the label is kept on the first retry; the pre-lib form
  # (no label) is the last resort, and the adoption lookup above finds and
  # labels such a bead on the next run.
  cb_flag_rejected "$cb_err" || { rm -f "$cb_err"; return 3; }
  if cb_create --labels "$cb_label" >/dev/null 2>"$cb_err"; then
    rm -f "$cb_err"
    echo "filed (id not reported: client rejected --silent, filed with the dedupe label${cb_note:+; $cb_note})"
    return 0
  fi
  cb_flag_rejected "$cb_err" || { rm -f "$cb_err"; return 3; }
  rm -f "$cb_err"
  cb_create >/dev/null 2>&1 || return 3
  echo "filed (id not reported: client rejected the newer create flags, filed the old way without the dedupe label${cb_note:+; $cb_note})"
}
