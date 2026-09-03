#!/bin/sh
# canary-bead-test.sh — regression test for scripts/lib/canary-bead.sh
# (mu-ztmla): a canary must reuse its live drift bead instead of filing a
# sibling per failing run, and must say so in its one output line whenever it
# could not.
#
# Three layers, from always-on to opt-in:
#
#   offline (always)  A STRICT fake `beads` on PATH accepts exactly the CLI
#                     surface the lib may use and exits 64 on anything else,
#                     so a flag the lib starts passing without re-verification
#                     fails here rather than silently in cron. It records every
#                     call and answers `list` from per-case fixtures; knobs make
#                     individual calls fail, stall, or emit stderr.
#   contract          CANARY_BEAD_CONTRACT=1: READ-ONLY probe of the real client
#                     (pre-pr-check sets it when a beadsd url resolves): the two
#                     `list` forms on filters nothing matches, plus `--help` of
#                     each mutating subcommand for the flags the lib passes.
#                     Proves the fake's surface is still br's without writing.
#   live              CANARY_BEAD_LIVE=1: the real client, a disposable slug —
#                     a pre-label bead is adopted, commented, retitled — then
#                     closed.
#
# usage: sh scripts/tests/canary-bead-test.sh
set -u
# The dedupe path the offline cases exercise needs jq (the lib itself treats
# jq as optional and says so in its output line); without it there is
# nothing meaningful to assert, so the suite steps aside rather than fail.
if ! command -v jq >/dev/null 2>&1; then
  echo "skip canary-bead-test: jq not on PATH (the lib degrades to create-always without it)"
  exit 0
fi
HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
LIB="$HERE/../lib/canary-bead.sh"
T=$(mktemp -d "${TMPDIR:-/tmp}/canary-bead-test.XXXXXX")
trap 'rm -rf "$T"' EXIT INT TERM
mkdir -p "$T/bin" "$T/empty" "$T/nojq"
REAL_PATH=$PATH
LIVE_REMOTE=${BEADS_REMOTE:-$(sed -n 's/^mu=[[:space:]]*//p' "$HOME/.config/beads/remotes.env" 2>/dev/null | head -n 1 | tr -d '[:space:]' || true)}
FLAG_REJECT="unexpected argument|found argument '[^']*' which wasn't expected"

cat > "$T/bin/beads" <<'FAKE'
#!/bin/sh
# Strict fake of the `beads --url <u> exec -- <br subcommand> ...` relay.
# Surface below = what canary-bead.sh may use, verified against br 0.2.15 on
# 2026-09-03. Anything else exits 64.
# Knobs: FAKE_LIST (payload for --label lists) FAKE_LIST_TITLE / FAKE_LIST_DESC
#        (payloads for --title-contains / --desc-contains lists; empty result
#        when unset) FAKE_LIST_RC
#        FAKE_LIST_TITLE_RC (fail only --title-contains lists)
#        FAKE_LIST_EMPTY (exit 0, print nothing)
#        FAKE_LIST_STDERR FAKE_LIST_SLEEP FAKE_CREATE_RC FAKE_REJECT_SILENT
#        FAKE_REJECT_LABELS FAKE_COMMENT_RC FAKE_UPDATE_RC
printf '%s\n' "$*" >> "$FAKE_CALLS"
bad() { echo "fake beads: $*" >&2; exit 64; }
val() { [ -n "${2:-}" ] || bad "$sub: $1 needs a value"; }
[ "${1:-}" = --url ] && [ -n "${2:-}" ] && [ "${3:-}" = exec ] && [ "${4:-}" = -- ] \
  || bad "unexpected prefix: $*"
shift 4; sub=${1:-}; [ -n "$sub" ] && shift
case $sub in
  list)
    by=label
    while [ $# -gt 0 ]; do
      case $1 in
        --all) shift ;;
        --format) val "$@"; [ "$2" = json ] || bad "list --format $2"; shift 2 ;;
        --limit|--label) val "$@"; shift 2 ;;
        --title-contains) val "$@"; by=title; shift 2 ;;
        --desc-contains) val "$@"; by=desc; shift 2 ;;
        *) bad "list: unknown arg $1" ;;
      esac
    done
    [ -z "${FAKE_LIST_SLEEP:-}" ] || sleep "$FAKE_LIST_SLEEP"
    [ "${FAKE_LIST_RC:-0}" -eq 0 ] || { echo "error: fake list failure" >&2; exit "$FAKE_LIST_RC"; }
    [ "${FAKE_LIST_STDERR:-0}" -eq 0 ] || echo "warning: relay notice on a successful list" >&2
    [ "${FAKE_LIST_EMPTY:-0}" -eq 0 ] || exit 0
    if [ "$by" = title ]; then
      [ "${FAKE_LIST_TITLE_RC:-0}" -eq 0 ] || { echo "error: fake title-list failure" >&2; exit "$FAKE_LIST_TITLE_RC"; }
      [ -n "${FAKE_LIST_TITLE:-}" ] && cat "$FAKE_LIST_TITLE" || printf '{"issues":[]}'
    elif [ "$by" = desc ]; then
      [ -n "${FAKE_LIST_DESC:-}" ] && cat "$FAKE_LIST_DESC" || printf '{"issues":[]}'
    else
      cat "$FAKE_LIST"
    fi ;;
  create)
    silent=0; labels=0
    while [ $# -gt 0 ]; do
      case $1 in
        --title|--slug|--type|--priority|--description|--actor) val "$@"; shift 2 ;;
        --labels) val "$@"; labels=1; shift 2 ;;
        --silent) silent=1; shift ;;
        *) bad "create: unknown arg $1" ;;
      esac
    done
    [ "${FAKE_CREATE_RC:-0}" -eq 0 ] || { echo "error: connection reset by peer" >&2; exit "$FAKE_CREATE_RC"; }
    if [ "$silent" -eq 1 ] && [ "${FAKE_REJECT_SILENT:-0}" -eq 1 ]; then
      echo "error: unexpected argument '--silent' found" >&2; exit 2
    fi
    if [ "$labels" -eq 1 ] && [ "${FAKE_REJECT_LABELS:-0}" -eq 1 ]; then
      echo "error: unexpected argument '--labels' found" >&2; exit 2
    fi
    if [ "$silent" -eq 1 ]; then echo "mu-fake-created-1"
    else echo "Created mu-fake-created-2 (plain form, stdout is prose)"; fi ;;
  comments)
    [ "${1:-}" = add ] && [ -n "${2:-}" ] || bad "comments: $*"; shift 2
    while [ $# -gt 0 ]; do
      case $1 in --actor|--message) val "$@"; shift 2 ;; *) bad "comments add: unknown arg $1" ;; esac
    done
    exit "${FAKE_COMMENT_RC:-0}" ;;
  update)
    [ -n "${1:-}" ] || bad "update: missing id"; shift
    while [ $# -gt 0 ]; do
      case $1 in --title|--actor|--add-label) val "$@"; shift 2 ;; *) bad "update: unknown arg $1" ;; esac
    done
    exit "${FAKE_UPDATE_RC:-0}" ;;
  *) bad "unknown subcommand '$sub'" ;;
esac
FAKE
chmod +x "$T/bin/beads"

# A PATH with the fake and the lib's other tools but NO jq (and no timeout).
for tool in sed tr cut date head cat grep mktemp rm; do
  p=$(command -v "$tool") && ln -s "$p" "$T/nojq/$tool"
done
ln -s "$T/bin/beads" "$T/nojq/beads"

FAKE_CALLS="$T/calls"; FAKE_LIST="$T/list.json"
export FAKE_CALLS FAKE_LIST
PATH="$T/bin:$PATH"; export PATH
BEADS_REMOTE="http://fake.invalid/mcp"; export BEADS_REMOTE

# shellcheck source=../lib/canary-bead.sh
. "$LIB"

SLUG=openai-protocol-canary-drift
LABEL="canary:$SLUG"
NAME="OpenAI protocol canary failed"
TITLE_A="$NAME: live_codex"
TITLE_B="$NAME: live_codex live_public_capture"
TITLE_H="Codex token expired on the clone, see comment 4"
BODY="OpenAI protocol canary on testhost detected: live_codex. See /dev/null."
LIST_LABEL="list --all --format json --limit 500 --label $LABEL"
LIST_TITLE="list --all --format json --limit 500 --title-contains $NAME"
LIST_DESC="list --all --format json --limit 500 --desc-contains OpenAI protocol canary on "
CREATE="create --title $TITLE_A --slug $SLUG --type bug --priority P1 --description $BODY --actor openai-protocol-canary"

bead() { # $1=id $2=status $3=title $4=created_at [$5=labels-json]
  printf '{"id":"%s","status":"%s","title":"%s","created_at":"%s","labels":%s}' \
    "$1" "$2" "$3" "$4" "${5:-[\"$LABEL\"]}"
}
fixture() { # $@ = bead json objects -> {"issues":[...]}
  sep=""; printf '{"issues":['; for b in "$@"; do printf '%s%s' "$sep" "$b"; sep=","; done; printf ']}'
}

fails=0
calls() { grep -c -- "$1" "$FAKE_CALLS" 2>/dev/null || true; }
expect() { # $1=want-count $2=needle  (reports; the case's || branch counts)
  got=$(calls "$2")
  [ "$got" -eq "$1" ] && return 0
  echo "     expected $1 call(s) matching '$2', got $got"; return 1
}
starts() { case $1 in "$2"*) return 0 ;; esac; return 1; }
reset_knobs() {
  unset FAKE_LIST_RC FAKE_LIST_TITLE_RC FAKE_LIST_EMPTY FAKE_LIST_STDERR FAKE_LIST_SLEEP FAKE_CREATE_RC FAKE_COMMENT_RC \
        FAKE_UPDATE_RC FAKE_REJECT_SILENT FAKE_REJECT_LABELS FAKE_LIST_TITLE FAKE_LIST_DESC CANARY_BEAD_TIMEOUT
}
run() { # $1=label $2=label-list payload [$3=title-list payload] [$4=desc-list payload]
  label=$1; : > "$FAKE_CALLS"; printf '%s' "$2" > "$FAKE_LIST"
  if [ -n "${3:-}" ]; then printf '%s' "$3" > "$T/title.json"; FAKE_LIST_TITLE="$T/title.json"; export FAKE_LIST_TITLE; fi
  if [ -n "${4:-}" ]; then printf '%s' "$4" > "$T/desc.json"; FAKE_LIST_DESC="$T/desc.json"; export FAKE_LIST_DESC; fi
  out=$(canary_file_bead "$SLUG" "$TITLE_A" "$BODY" openai-protocol-canary 2>&1); rc=$?
  reset_knobs
}
ok() { echo "ok   $label"; }
ko() { echo "FAIL $label: rc=$rc out='$out'"; fails=$((fails + 1)); }

# 1. Nothing live anywhere -> label lookup, adoption lookup, one labelled create.
run "no live bead files a new one" "$(fixture)"
[ "$rc" -eq 0 ] && [ "$out" = "filed mu-fake-created-1" ] \
  && expect 1 "$LIST_LABEL" && expect 1 "$LIST_TITLE" && expect 1 "$LIST_DESC" \
  && expect 1 "$CREATE --labels $LABEL --silent" \
  && expect 0 "comments add" && expect 0 " update " && ok || ko

# 2. Same failure set live -> one comment, no create, no retitle, no adoption lookup.
run "live bead with the same failure set gets a comment only" \
  "$(fixture "$(bead mu-$SLUG-abc1 open "$TITLE_A" 2026-08-24T13:37:00Z)")"
[ "$rc" -eq 0 ] && [ "$out" = "updated mu-$SLUG-abc1" ] \
  && expect 1 "comments add mu-$SLUG-abc1 --actor openai-protocol-canary --message run " \
  && expect 0 "$LIST_TITLE" && expect 0 "create" && expect 0 " update " && ok || ko

# 3. Failure set changed, machine title -> comment AND retitle, still no create.
run "live bead with a different failure set is retitled" \
  "$(fixture "$(bead mu-$SLUG-abc1 open "$TITLE_B" 2026-08-24T13:37:00Z)")"
[ "$rc" -eq 0 ] && [ "$out" = "updated mu-$SLUG-abc1" ] \
  && expect 1 "comments add mu-$SLUG-abc1" \
  && expect 1 "update mu-$SLUG-abc1 --title $TITLE_A --actor openai-protocol-canary" \
  && expect 0 "create" && ok || ko

# 4. A human's triage title is left alone.
run "a hand-edited title is kept" \
  "$(fixture "$(bead mu-$SLUG-abc1 open "$TITLE_H" 2026-08-24T13:37:00Z)")"
[ "$rc" -eq 0 ] && [ "$out" = "updated mu-$SLUG-abc1" ] \
  && expect 1 "comments add mu-$SLUG-abc1" && expect 0 " update " && expect 0 "create" && ok || ko

# 4a. A machine title a human EXTENDED in place is theirs now: kept.
run "a machine title extended by hand is kept" \
  "$(fixture "$(bead mu-$SLUG-abc1 open "$TITLE_A - expired codex token on the clone" 2026-08-24T13:37:00Z)")"
[ "$rc" -eq 0 ] && [ "$out" = "updated mu-$SLUG-abc1" ] \
  && expect 1 "comments add mu-$SLUG-abc1" && expect 0 " update " && expect 0 "create" && ok || ko

# 4b. A title carrying a tab and a newline (a human can type both) must not
#     break the reuse path: only an id and two integers leave the lookup.
run "a title with a tab and a newline is handled" \
  "$(fixture "$(bead mu-$SLUG-abc1 open "Codex\\ttoken\\nexpired" 2026-08-24T13:37:00Z)")"
[ "$rc" -eq 0 ] && [ "$out" = "updated mu-$SLUG-abc1" ] \
  && expect 1 "comments add mu-$SLUG-abc1" && expect 0 " update " && expect 0 "create" && ok || ko

# 5. A closed record in the payload is not live -> create.
run "closed bead does not count as live" \
  "$(fixture "$(bead mu-$SLUG-old1 closed "$TITLE_A" 2026-07-06T13:37:00Z)")"
[ "$rc" -eq 0 ] && expect 1 "create" && expect 0 "comments add" && ok || ko

# 6. A claimed (in_progress) bead is still live -> comment.
run "in_progress bead counts as live" \
  "$(fixture "$(bead mu-$SLUG-abc2 in_progress "$TITLE_A" 2026-08-24T13:37:00Z)")"
[ "$rc" -eq 0 ] && expect 1 "comments add mu-$SLUG-abc2" && expect 0 "create" && ok || ko

# 7. The slug-in-id check is advisory: a labelled bead whose id lacks it is still ours.
run "labelled bead whose id lacks the slug is still reused" \
  "$(fixture "$(bead mu-drift-trunc1 open "$TITLE_A" 2026-08-24T13:37:00Z)")"
[ "$rc" -eq 0 ] && [ "$out" = "updated mu-drift-trunc1" ] && expect 0 "create" && ok || ko

# 7b. The label is checked here, not just asked of the server: a record the
#     server returned without our label is not ours, whatever its id says.
run "a returned record without our label is not reused" \
  "$(fixture "$(bead mu-$SLUG-abc1 open "$TITLE_A" 2026-08-24T13:37:00Z '[]')" \
             "$(bead mu-unrelated-p1 open "Something else entirely" 2026-01-01T00:00:00Z '["other"]')")"
[ "$rc" -eq 0 ] && [ "$out" = "filed mu-fake-created-1" ] && expect 0 "comments add" && ok || ko

# 8. ...but when both kinds are live, the slug-bearing id is preferred — and
#    the label-only one is still counted as a sibling to close.
run "slug-bearing id is preferred over a label-only match" \
  "$(fixture "$(bead mu-drift-trunc1 open "$TITLE_A" 2026-07-01T13:37:00Z)" \
             "$(bead mu-$SLUG-abc1 open "$TITLE_A" 2026-08-24T13:37:00Z)")"
[ "$rc" -eq 0 ] && [ "$out" = "updated mu-$SLUG-abc1 (2 live siblings, this run went to mu-$SLUG-abc1; close the rest)" ] \
  && expect 0 "create" && ok || ko

# 9. Several live siblings (a pre-fix pile, or a same-second race) -> the oldest
#    is the record and the line reports the count.
run "oldest live bead wins and the sibling count is reported" \
  "$(fixture "$(bead mu-$SLUG-new9 open "$TITLE_A" 2026-08-31T13:37:00Z)" \
             "$(bead mu-$SLUG-old7 open "$TITLE_A" 2026-07-20T13:37:00Z)")"
[ "$rc" -eq 0 ] && [ "$out" = "updated mu-$SLUG-old7 (2 live siblings, this run went to mu-$SLUG-old7; close the rest)" ] \
  && expect 1 "comments add mu-$SLUG-old7" && expect 0 "create" && ok || ko

# 10. Nothing labelled, but a pre-label bead with our slug is live -> adopt: label it, comment.
run "a pre-label bead with the slug is adopted and labelled" "$(fixture)" \
  "$(fixture "$(bead mu-$SLUG-pre1 open "$TITLE_A" 2026-08-24T13:37:00Z '[]')")"
[ "$rc" -eq 0 ] && [ "$out" = "updated mu-$SLUG-pre1 (adopted an unlabelled bead)" ] \
  && expect 1 "update mu-$SLUG-pre1 --add-label $LABEL --actor openai-protocol-canary" \
  && expect 1 "comments add mu-$SLUG-pre1" && expect 0 "create" && ok || ko

# 10b. A pre-label bead a human retitled is still adopted through its description.
run "a retitled pre-label bead is adopted through its description" "$(fixture)" "" \
  "$(fixture "$(bead mu-$SLUG-pre2 open "$TITLE_H" 2026-08-24T13:37:00Z '[]')")"
[ "$rc" -eq 0 ] && [ "$out" = "updated mu-$SLUG-pre2 (adopted an unlabelled bead)" ] \
  && expect 1 "$LIST_TITLE" && expect 1 "$LIST_DESC" \
  && expect 1 "update mu-$SLUG-pre2 --add-label $LABEL" && expect 0 " --title " && expect 0 "create" && ok || ko

# 11. Adoption is strict about the slug: a title match with another id is not ours.
run "adoption ignores a title match whose id lacks the slug" "$(fixture)" \
  "$(fixture "$(bead mu-anthropic-protocol-canary-drift-zz1 open "$TITLE_A" 2026-08-24T13:37:00Z '[]')")"
[ "$rc" -eq 0 ] && [ "$out" = "filed mu-fake-created-1" ] && expect 0 "comments add" && ok || ko

# 11b. The adoption lookup itself fails -> still files, and the line says adoption was lost.
FAKE_LIST_TITLE_RC=3; export FAKE_LIST_TITLE_RC
run "adoption lookup failure files and says so" "$(fixture)"
[ "$rc" -eq 0 ] && [ "$out" = "filed mu-fake-created-1 (no adoption: lookup failed: error: fake title-list failure)" ] \
  && expect 1 "create" && expect 0 "comments add" && ok || ko

# 12. No beads client -> returns 1 and touches nothing.
label="no beads client returns 1 silently"; : > "$FAKE_CALLS"
out=$(PATH="$T/empty" canary_file_bead "$SLUG" "$TITLE_A" "$BODY" 2>&1); rc=$?
[ "$rc" -eq 1 ] && [ -z "$out" ] && [ ! -s "$FAKE_CALLS" ] && ok || ko

# 13. No url anywhere -> returns 1 and touches nothing.
label="no beadsd url returns 1 silently"; : > "$FAKE_CALLS"
out=$(BEADS_REMOTE= HOME="$T/empty" canary_file_bead "$SLUG" "$TITLE_A" "$BODY" 2>&1); rc=$?
[ "$rc" -eq 1 ] && [ -z "$out" ] && [ ! -s "$FAKE_CALLS" ] && ok || ko

# 14. The lookup itself fails -> still files, and the line says dedupe was lost.
FAKE_LIST_RC=3; export FAKE_LIST_RC
run "lookup failure files without dedupe and says so" "$(fixture)"
[ "$rc" -eq 0 ] && [ "$out" = "filed mu-fake-created-1 (no dedupe: lookup failed: error: fake list failure)" ] \
  && expect 1 "create" && expect 0 "comments add" && ok || ko

# 15. A bare-array payload is understood the same as {"issues":[...]}.
run "bare-array list payload is accepted" \
  "[$(bead mu-$SLUG-abc1 open "$TITLE_A" 2026-08-24T13:37:00Z)]"
[ "$rc" -eq 0 ] && [ "$out" = "updated mu-$SLUG-abc1" ] && expect 0 "create" && ok || ko

# 16. Garbage payload -> files, and the line names the payload problem.
run "unparseable list payload files without dedupe and says so" "this is not json"
[ "$rc" -eq 0 ] && starts "$out" "filed mu-fake-created-1 (no dedupe: unexpected list payload: " \
  && expect 1 "create" && expect 0 "comments add" && ok || ko

# 16a. An exit-0 list that prints nothing is a changed contract, not "none live".
FAKE_LIST_EMPTY=1; export FAKE_LIST_EMPTY
run "an empty list payload is reported, not read as no live bead" "$(fixture)"
[ "$rc" -eq 0 ] && [ "$out" = "filed mu-fake-created-1 (no dedupe: unexpected list payload: empty list payload)" ] \
  && expect 1 "create" && expect 0 "comments add" && ok || ko

# 16b. A valid JSON envelope WITHOUT an issues array is a changed contract, not "none live".
run "an envelope without an issues array is reported, not read as empty" '{"beads":[]}'
[ "$rc" -eq 0 ] && starts "$out" "filed mu-fake-created-1 (no dedupe: unexpected list payload: " \
  && expect 1 "create" && expect 0 "comments add" && ok || ko

# 17. stderr noise on a SUCCESSFUL list must not reach the parser -> reuse.
FAKE_LIST_STDERR=1; export FAKE_LIST_STDERR
run "stderr on a successful list does not break dedupe" \
  "$(fixture "$(bead mu-$SLUG-abc1 open "$TITLE_A" 2026-08-24T13:37:00Z)")"
[ "$rc" -eq 0 ] && [ "$out" = "updated mu-$SLUG-abc1" ] && expect 0 "create" && ok || ko

# 18. A stalled client is cut off by the timeout bound and reported.
if command -v timeout >/dev/null 2>&1; then
  FAKE_LIST_SLEEP=3 CANARY_BEAD_TIMEOUT=1; export FAKE_LIST_SLEEP CANARY_BEAD_TIMEOUT
  run "a hung list call is bounded by the timeout and reported" "$(fixture)"
  [ "$rc" -eq 0 ] && [ "$out" = "filed mu-fake-created-1 (no dedupe: lookup failed: exit 124)" ] \
    && expect 1 "create" && ok || ko
else
  echo "skip a hung list call is bounded by the timeout: no timeout(1) on this host"
fi

# 19. Comment lands, retitle fails -> the run is on record; the line says the title is stale.
FAKE_UPDATE_RC=1; export FAKE_UPDATE_RC
run "retitle failure after a successful comment is reported, not fatal" \
  "$(fixture "$(bead mu-$SLUG-abc1 open "$TITLE_B" 2026-08-24T13:37:00Z)")"
[ "$rc" -eq 0 ] && [ "$out" = "updated mu-$SLUG-abc1 (retitle failed)" ] \
  && expect 1 "comments add" && expect 0 "create" && ok || ko

# 20. Comment on the live bead fails -> nothing recorded, and NO sibling is created.
FAKE_COMMENT_RC=1; export FAKE_COMMENT_RC
run "comment failure returns 2 without creating a sibling" \
  "$(fixture "$(bead mu-$SLUG-abc1 open "$TITLE_A" 2026-08-24T13:37:00Z)")"
[ "$rc" -eq 2 ] && [ -z "$out" ] && expect 0 "create" && ok || ko

# 21. Client rejects --silent -> retry keeps the label, drops only --silent.
FAKE_REJECT_SILENT=1; export FAKE_REJECT_SILENT
run "a rejected --silent is retried with the label kept" "$(fixture)"
[ "$rc" -eq 0 ] && [ "$out" = "filed (id not reported: client rejected --silent, filed with the dedupe label)" ] \
  && expect 2 "create --title" && expect 1 "$CREATE --labels $LABEL$" && ok || ko

# 22. Client rejects --labels too -> the pre-lib form is the last resort.
FAKE_REJECT_SILENT=1 FAKE_REJECT_LABELS=1; export FAKE_REJECT_SILENT FAKE_REJECT_LABELS
run "when the label flag is rejected as well, the pre-lib form still files" "$(fixture)"
[ "$rc" -eq 0 ] && starts "$out" "filed (id not reported: client rejected the newer create flags, filed the old way without the dedupe label" \
  && expect 3 "create --title" && expect 1 "$CREATE$" && ok || ko

# 23. Create fails for any OTHER reason (it may have written) -> no blind retry.
FAKE_CREATE_RC=7; export FAKE_CREATE_RC
run "a non-flag create failure is not retried and returns 3" "$(fixture)"
[ "$rc" -eq 3 ] && [ -z "$out" ] && expect 1 "create --title" && ok || ko

# 24. No jq on the host -> files, and the line says dedupe was lost.
label="missing jq files without dedupe and says so"; : > "$FAKE_CALLS"; printf '%s' "$(fixture)" > "$FAKE_LIST"
out=$(PATH="$T/nojq" canary_file_bead "$SLUG" "$TITLE_A" "$BODY" 2>&1); rc=$?
[ "$rc" -eq 0 ] && [ "$out" = "filed mu-fake-created-1 (no dedupe: jq missing)" ] \
  && expect 0 "list" && expect 1 "create" && ok || ko

# 25. Contract probe (read-only, real client): the surface the fake pins is br's.
if [ "${CANARY_BEAD_CONTRACT:-0}" = 1 ]; then
  PATH=$REAL_PATH; export PATH
  label="contract: real br accepts the list/create/comments/update surface (read-only)"
  if [ -z "$LIVE_REMOTE" ] || ! command -v beads >/dev/null 2>&1; then
    echo "skip $label: no beads client or beadsd url"
  else
    # Every probe call is bounded like the lib's own calls, and a failure that
    # is not the parser refusing a flag means beadsd is unreachable: that is
    # a skip, not contract drift. pb runs inside command substitutions, i.e.
    # in a subshell, so the first such reason is passed out through a file.
    rm -f "$T/unreachable"
    pb() { # $1=stderr file, rest = br subcommand and args
      pb_err=$1; shift
      # Once beadsd proved unreachable, no further probe is worth a timeout.
      [ ! -s "$T/unreachable" ] || { : > "$pb_err"; return 1; }
      if command -v timeout >/dev/null 2>&1; then
        timeout "${CANARY_BEAD_TIMEOUT:-60}" beads --url "$LIVE_REMOTE" exec -- "$@" 2>"$pb_err"
      else
        beads --url "$LIVE_REMOTE" exec -- "$@" 2>"$pb_err"
      fi
      pb_rc=$?
      if [ "$pb_rc" -ne 0 ] && ! grep -qiE "$FLAG_REJECT" "$pb_err" && [ ! -s "$T/unreachable" ]; then
        { head -n 1 "$pb_err"; echo "exit $pb_rc"; } | head -n 1 > "$T/unreachable"
        [ -s "$T/unreachable" ] || echo "exit $pb_rc" > "$T/unreachable"
      fi
      return "$pb_rc"
    }
    p1=$(pb "$T/probe.err" list --all --format json --limit 1 \
           --label canary:contract-probe-nothing-carries-this); rc1=$?
    p2=$(pb "$T/probe2.err" list --all --format json --limit 1 \
           --title-contains "canary-bead contract probe: no bead has this title"); rc2=$?
    p4=$(pb "$T/probe4.err" list --all --format json --limit 1 \
           --desc-contains "canary-bead contract probe: no bead has this description"); rc4=$?
    # Real records, in the form the lib sends (--all, no --status): the
    # per-record fields the lib indexes, the labels array the key rests on,
    # and the literal status it treats as "not live". Then a record carrying
    # a label the lib itself wrote earlier (a closed selftest bead), so the
    # create --labels -> list --label -> .labels round trip is checked on the
    # real store without writing; noted, not failed, when the store has none.
    p3=$(pb "$T/probe3.err" list --all --format json --limit 50); rc3=$?
    p5=$(pb "$T/probe5.err" list --all --format json --limit 1 --label canary:canary-bead-selftest); rc5=$?
    # The text filters, on words the lib's own selftest bead carries in its
    # title and description: proves substring-contains semantics, not just
    # that the flags are accepted. Noted, not failed, when the store has none.
    p6=$(pb "$T/probe6.err" list --all --format json --limit 1 --title-contains "selftest failed"); rc6=$?
    p7=$(pb "$T/probe7.err" list --all --format json --limit 1 --desc-contains "canary-bead-test live"); rc7=$?
    helps=1
    for spec in "create:--silent" "create:--labels" "comments add:--message" "comments add:--actor" \
                "update:--title" "update:--actor" "update:--add-label"; do
      sub=${spec%%:*}; flag=${spec#*:}
      # shellcheck disable=SC2086  # $sub is one or two words on purpose
      h=$(pb "$T/probe-help.err" $sub --help) || true
      [ -s "$T/unreachable" ] && break
      printf '%s\n' "$h" | grep -q -- "$flag" || { echo "     $sub --help does not list $flag"; helps=0; }
    done
    if [ -s "$T/unreachable" ]; then
      echo "skip $label: beadsd unreachable ($(head -n 1 "$T/unreachable"))"
    else
      # The same two shapes the lib accepts: a bare array or {"issues":[...]}.
      rows='(if type == "array" then . elif type == "object" and (.issues | type) == "array" then .issues else error("envelope") end)'
      envelope="($rows | type) == \"array\""
      # The filters name things nothing carries, so a filter that filters
      # returns nothing; a filter the server ignored would return a record.
      empty="($rows | length) == 0"
      # labels may be absent on older records; the lib reads it as (.labels // []).
      record="[$rows[] | has(\"id\") and has(\"title\") and has(\"created_at\") and (.status | type) == \"string\" and ((.labels // []) | type) == \"array\"] | all"
      # The record the lib itself wrote (a closed selftest bead) is the one
      # whose shape is REQUIRED; the unfiltered sample is shared-store content
      # the test does not own, so its shape is reported, never failed on.
      labelled="($rows | length) == 0 or ($rows[0] | has(\"id\") and has(\"title\") and has(\"created_at\") and (.status | type) == \"string\" and (.labels | index(\"canary:canary-bead-selftest\") != null))"
      if [ "$rc1" -eq 0 ] && [ "$rc2" -eq 0 ] && [ "$rc3" -eq 0 ] && [ "$rc4" -eq 0 ] && [ "$rc5" -eq 0 ] \
         && [ "$rc6" -eq 0 ] && [ "$rc7" -eq 0 ] \
         && printf '%s\n' "$p6" | jq -e "$envelope" >/dev/null 2>&1 \
         && printf '%s\n' "$p7" | jq -e "$envelope" >/dev/null 2>&1 \
         && printf '%s\n' "$p1" | jq -e "$empty" >/dev/null 2>&1 \
         && printf '%s\n' "$p2" | jq -e "$empty" >/dev/null 2>&1 \
         && printf '%s\n' "$p4" | jq -e "$empty" >/dev/null 2>&1 \
         && printf '%s\n' "$p3" | jq -e "$envelope" >/dev/null 2>&1 \
         && printf '%s\n' "$p5" | jq -e "$envelope and ($labelled)" >/dev/null 2>&1 \
         && [ "$helps" -eq 1 ]; then
        printf '%s\n' "$p3" | jq -e "($rows | length) > 0" >/dev/null 2>&1 \
          || echo "     note: empty store, record fields not sampled"
        printf '%s\n' "$p3" | jq -e "$record" >/dev/null 2>&1 \
          || echo "     note: some records in the shared store lack a field the lib reads with a default (title, created_at, status, labels)"
        printf '%s\n' "$p3" | jq -e "[$rows[] | .status == \"closed\"] | any" >/dev/null 2>&1 \
          || echo "     note: no closed bead among the first records, the closed literal not seen"
        if printf '%s\n' "$p5" | jq -e "($rows | length) > 0" >/dev/null 2>&1; then
          printf '%s\n' "$p6" | jq -e "($rows | length) > 0 and ($rows[0].title | contains(\"selftest failed\"))" >/dev/null 2>&1 \
            || echo "     note: --title-contains did not return the selftest bead by a substring of its title"
          printf '%s\n' "$p7" | jq -e "($rows | length) > 0 and ($rows[0].description | contains(\"canary-bead-test live\"))" >/dev/null 2>&1 \
            || echo "     note: --desc-contains did not return the selftest bead by a substring of its description"
        else
          echo "     note: no bead carries a lib-written label yet, label round trip, own-record shape and text-filter semantics not checked"
        fi
        ok
      else
        echo "FAIL $label: label-list rc=$rc1 title-list rc=$rc2 all-list rc=$rc3 desc-list rc=$rc4 labelled-list rc=$rc5 selftest-title rc=$rc6 selftest-desc rc=$rc7 payloads='$(printf '%s' "$p1" | head -c 80)' / '$(printf '%s' "$p2" | head -c 80)' / '$(printf '%s' "$p3" | head -c 120)' / '$(printf '%s' "$p4" | head -c 80)' / '$(printf '%s' "$p5" | head -c 120)' stderr='$(head -n 1 "$T/probe2.err" "$T/probe3.err" "$T/probe4.err" "$T/probe5.err" 2>/dev/null | tr '\n' ' ')'"
        fails=$((fails + 1))
      fi
    fi
  fi
fi

# 26. Live (opt-in): the real client, a disposable slug, a pre-label bead adopted,
#     commented, retitled, then closed.
if [ "${CANARY_BEAD_LIVE:-0}" = 1 ]; then
  PATH=$REAL_PATH; export PATH
  BEADS_REMOTE=$LIVE_REMOTE; export BEADS_REMOTE
  label="live: pre-label bead adopted, commented, retitled on ONE bead, then closed"
  ls=canary-bead-selftest; ln="Canary selftest failed"; ta="$ln: alpha"; tb="$ln: alpha beta"
  if ! beads --url "$BEADS_REMOTE" exec -- close --help 2>&1 | grep -q -- --reason; then
    echo "FAIL $label: close --help does not list --reason; not running (could not clean up)"; fails=$((fails + 1))
  else
    pre=$(beads --url "$BEADS_REMOTE" exec -- create --title "$ta" --slug "$ls" --type bug --priority P1 \
            --description "canary-bead-test live: pre-label bead (disposable)" --actor canary-bead-test --silent 2>&1)
    case $pre in *-$ls-*) ;; *) pre="" ;; esac
    if [ -z "$pre" ]; then
      echo "FAIL $label: could not create the pre-label bead"; fails=$((fails + 1))
    else
      o1=$(canary_file_bead "$ls" "$ta" "canary-bead-test live run 1" canary-bead-test 2>&1)
      o2=$(canary_file_bead "$ls" "$ta" "canary-bead-test live run 2" canary-bead-test 2>&1)
      o3=$(canary_file_bead "$ls" "$tb" "canary-bead-test live run 3" canary-bead-test 2>&1)
      # Verify through the same list surface the lib uses: the bead must now
      # carry the label and the retitled title.
      after=$(beads --url "$BEADS_REMOTE" exec -- list --all --format json --limit 50 \
                --label "canary:$ls" 2>/dev/null)
      if [ "$o1" = "updated $pre (adopted an unlabelled bead)" ] && [ "$o2" = "updated $pre" ] \
         && [ "$o3" = "updated $pre" ] \
         && printf '%s\n' "$after" | jq -e --arg id "$pre" --arg l "canary:$ls" --arg t "$tb" \
              '[.issues[] | select(.id == $id)] | length == 1 and (.[0].labels | index($l) != null) and .[0].title == $t' >/dev/null 2>&1; then ok
      else echo "FAIL $label: o1='$o1' o2='$o2' o3='$o3' after='$(printf '%s' "$after" | head -c 200)'"; fails=$((fails + 1)); fi
      beads --url "$BEADS_REMOTE" exec -- close "$pre" --actor canary-bead-test \
        --reason "canary-bead-test live contract check; disposable" >/dev/null 2>&1 \
        || { echo "FAIL $label: could not close $pre — close it by hand"; fails=$((fails + 1)); }
    fi
  fi
fi

PATH=$REAL_PATH
[ "$fails" -eq 0 ] && { echo "canary-bead-test: all ok"; exit 0; }
echo "canary-bead-test: $fails failure(s)"; exit 1
