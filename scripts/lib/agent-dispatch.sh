#!/bin/sh
# agent-dispatch.sh — shared model dispatch for the agent toolchain.
#
# ONE function, agent_dispatch, runs a single model on a prompt file and prints
# its stdout (stderr -> $ERRLOG). It routes by provider, ToS-cleanly:
#   claude-oauth   -> `claude -p`              (the $0 Max subscription via the
#                                               approved client; NEVER OAuth-via-mu)
#   anything else  -> `mu ask --bare --provider <p>`  (codex / ollama / openrouter / ...)
# Both are HERMETIC: mu's --bare and claude's --exclude-dynamic-system-prompt-
# sections strip recall / product scaffolding, so the model sees only the prompt
# (+ $SYSPROMPT if set) — not a CLAUDE.md kernel that would make every model
# self-identify as "claude".
#
# Tool grant is driven by $TOOLS (mu names: read,write,edit,glob,grep,ls,bash;
# MCP-imported names like code_recall/code_status opt mu into MCP but are not
# passed through `--tools`, which only accepts built-ins):
#   - mu path passes built-in names via `--tools`, adds `--enable-mcp` for
#     MCP-imported names, and adds `--bash-yolo` when `bash` is granted.
#   - claude path maps the names to `--allowedTools Read Write Edit Glob Grep LS Bash`,
#     and adds `--permission-mode bypassPermissions` when a WRITE tool (write/edit/
#     bash) is granted (a `-p` worker can't answer per-command prompts).
# Read-only callers (review/plan/adjudicate: TOOLS=read,grep[,ls]) get NO bypass
# and NO --bash-yolo — byte-identical to the original review dispatch.
#
# Extracted from scripts/ai-review.sh::run_review (mu repo) so the review gate,
# the orchestrator pipeline, and future spawns share ONE dispatch.
#
# Usage — source this file, then:
#   agent_dispatch <provider> <model> [<prompt-file>]   # prompt-file default $PROMPT_FILE
#
# Tunables (read from the CALLER's scope; defaults applied when unset):
#   TOOLS      mu tool CSV                                      default "read,grep"
#              ("" => zero tools: omits --tools/--allowedTools; MAX_TURNS still
#              applies on the mu path when explicitly set)
#   SYSPROMPT  system-prompt file (optional; overrides daemon)  default unset
#   TIMEOUT    wall-clock backstop, seconds                     default 900
#   MAX_TURNS  mu --max-turns (mu path); unset/empty = provider default,
#              0 = explicitly uncapped
#   THINKING   mu/claude thinking level                         default low
#   MU         mu binary                                        default `command -v mu`
#   ERRLOG     stderr sink (appended)                           default /tmp/agent-dispatch.$$.err
#   AGENT_DISPATCH_NO_LEASE  =1 skips the shared-ollama-box lease    default unset
#              (see the LOTO-acquire note in the mu-providers branch)
#   AGENT_DISPATCH_OLLAMA_SKIP_IF_HELD =1 makes ollama dispatch use
#              `with-ollama-lease --skip-if-held`: exit 75 immediately when
#              the shared box is already held instead of waiting in the fair
#              queue. ci-aipr/review-panel enables this so a held local box
#              drops the ollama reviewer and lets hosted reviewers proceed.
#   AGENT_DISPATCH_CAP_ROUTE_AROUND =1 declares THIS CALLER's task re-runnable,
#              so a seat whose lane is out of tokens (usage cap / no credit)
#              exits 75 and the caller's rank walk continues instead of the run
#              halting. Default OFF: only the caller knows whether re-running
#              its task elsewhere is safe (see _ad_out_of_tokens). ci-aipr's
#              review panel sets it — a seat produces a verdict and nothing else.
#   AGENT_SESSION_OWNER/_TTL passed through to with-ollama-lease when it wraps an
#              ollama dispatch (export one OWNER to let a multi-call run share the lease)

# mu-cbmru: byte offset of the errlog before a dispatch, so the classifiers
# below read only THIS invocation's stderr. The log is append-only and
# mu-spawn walks every rank in one process without setting ERRLOG, so all
# ranks share one per-PID file: a fixed `tail -n` window can match the
# PREVIOUS rank's message and misclassify this one.
_ad_err_mark() {  # -> byte count (0 when the log does not exist yet)
  [ -r "${ad_errlog:-/dev/null}" ] || { printf '0'; return; }
  wc -c < "$ad_errlog" 2>/dev/null | tr -d ' ' || printf '0'
}
# The TERMINAL region of this invocation's stderr: the last few lines, after
# the byte mark. Both bounds matter. The mark keeps a previous rank's message
# from classifying this one; the line bound keeps the MODEL's own output from
# doing it — `mu ask` prints reasoning (`[thinking] <body>`, multi-line and
# verbatim) and tool-argument echoes to this same stream, so anything a seat
# read or reasoned about can otherwise look like a provider error. A real
# provider failure is the last thing written before the non-zero exit.
_ad_err_tail() {  # $1=mark [$2=lines, default 5] -> terminal stderr region
  [ -r "${ad_errlog:-/dev/null}" ] || return 0
  tail -c "+$(( ${1:-0} + 1 ))" "$ad_errlog" 2>/dev/null | tail -n "${2:-5}"
}

# mu-cbmru: is this seat's lane OUT OF TOKENS — a subscription usage cap, or a
# metered lane with no credit left? The signal is mu's EXIT CODE (4), never a
# phrase in its output: `mu ask` prints the model's own reasoning to stderr
# verbatim, so a text match cannot tell a provider's error from a model
# reasoning about one, and the material a seat reads is full of both.
# (`mu ask` maps its typed ProviderUsageLimit to exit 4; 3 is the spend
# ceiling, 124 a timeout, 75 an already-skipped seat.)
#
# Exit 75 is a CONTRACT — "this seat never ran, so re-running the task on the
# next rank cannot double-execute anything" — and the cap cannot prove that: a
# multi-turn worker can write a file, then exhaust its quota on the NEXT model
# request. Nor does the tool grant bound the session (the daemon adds tools the
# caller never named: MCP imports, the mesh `dm` tool, a caller's MCP_CONFIG).
#
# So the route-around is the CALLER's declaration, not an inference:
# AGENT_DISPATCH_CAP_ROUTE_AROUND=1 means "my task is re-runnable — if this
# seat is out of tokens, walk on". Default off: a capped seat fails loudly,
# exactly as a timeout keeps 124 for the same reason. A grant that NAMES
# write/edit/bash is refused even with the opt-in, as a backstop.
_ad_out_of_tokens() {  # $1=rc $2=seat-label $3=write-free? -> 0 when routable
  [ "$1" -eq 4 ] || return 1
  [ "${AGENT_DISPATCH_CAP_ROUTE_AROUND:-}" = "1" ] || return 1
  if [ "${3:-0}" -ne 1 ]; then
      printf 'agent-dispatch: %s is out of tokens (exit 4; see %s), but this seat was granted write tools (%s) and may have already acted — NOT re-running it elsewhere despite AGENT_DISPATCH_CAP_ROUTE_AROUND. Failing loudly instead.\n' \
        "$2" "$ad_errlog" "${ad_tools:-<provider default>}" >&2
      return 1
  fi
  printf 'agent-dispatch: %s is out of tokens (exit 4; see %s). Skipping this seat (exit 75).\n' \
    "$2" "$ad_errlog" >&2
  # mu-cbmru: leave a MARKER for the caller, beside its errlog. Exit 75 alone
  # says "skipped", not why, and a skip for no-credit is the one an operator
  # must act on (a subscription cap refills; a prepaid balance does not). A
  # file's existence is the signal — the caller never parses output for it,
  # and its content is this seat's label for a human reading the census.
  if [ -n "${ad_errlog:-}" ]; then
    printf '%s\n' "$2" > "${ad_errlog%.err}.out-of-tokens" 2>/dev/null || true
  fi
  return 0
}

# Map a mu tool CSV -> claude `--allowedTools` names (space-separated).
_ad_claude_tools() {  # $1=csv
  printf '%s\n' "$1" | tr ',' '\n' | while IFS= read -r _t; do
    case "$_t" in
      read) printf 'Read ' ;; write) printf 'Write ' ;; edit) printf 'Edit ' ;;
      glob) printf 'Glob ' ;; grep) printf 'Grep ' ;; ls) printf 'LS ' ;;
      bash) printf 'Bash ' ;;
    esac
  done
}

agent_dispatch() {  # $1=provider $2=model [$3=prompt-file]
  local ad_prov ad_model ad_pf ad_tools ad_timeout ad_maxturns ad_thinking ad_mu ad_errlog
  local ad_clsys ad_sysflags ad_cltools ad_perm ad_yolo ad_lease ad_mcpflag ad_turnflag
  local ad_mu_tools ad_tool ad_old_ifs ad_rc ad_errmark ad_readonly
  ad_prov="$1"; ad_model="$2"
  ad_pf="${3:-${PROMPT_FILE:-}}"
  [ -n "$ad_pf" ] || { echo "agent_dispatch: no prompt file (arg 3 or \$PROMPT_FILE)" >&2; return 2; }
  ad_tools="${TOOLS-read,grep}"            # '-' not ':-': honour an explicit empty TOOLS
  # mu-cbmru: normalise the grant ONCE. A roster writing `tools = "read, write"`
  # grants write (mu trims each entry, factory.rs parse_tools_csv), but the raw
  # string does not match a `*,write,*` test — which silently cost the
  # bypassPermissions/--bash-yolo flags below too. Tool names carry no internal
  # whitespace, so stripping all of it is exactly the normalisation mu applies.
  ad_tools=$(printf '%s' "$ad_tools" | tr -d '[:space:]')
  ad_timeout="${TIMEOUT:-900}"
  ad_maxturns="${MAX_TURNS-}"
  ad_thinking="${THINKING:-low}"
  ad_mu="${MU:-$(command -v mu || true)}"
  ad_errlog="${ERRLOG:-${TMPDIR:-/tmp}/agent-dispatch.$$.err}"

  # Write tools (write/edit/bash) need extra flags so a non-interactive worker
  # doesn't deadlock (claude) and can run a shell (mu). Read-only sets stay clean.
  ad_perm=""; ad_yolo=""; ad_turnflag=""
  case ",$ad_tools," in *,write,*|*,edit,*|*,bash,*) ad_perm="--permission-mode bypassPermissions" ;; esac
  case ",$ad_tools," in *,bash,*) ad_yolo="--bash-yolo" ;; esac
  # mu-cbmru: a seat that NAMES a write tool is declared dangerous here, as a
  # backstop. It is only a backstop: the grant does not bound what the session
  # can do — the daemon adds tools the CSV never named (imported MCP servers
  # under `--enable-mcp`, the mesh `dm` tool when [mesh].dialogue is on, and
  # whatever a caller-supplied MCP_CONFIG hands claude). Proving a task
  # re-runnable from the tool list is therefore not possible, which is why the
  # route-around is an explicit caller opt-in (AGENT_DISPATCH_CAP_ROUTE_AROUND)
  # rather than an inference.
  ad_readonly=1
  case ",$ad_tools," in *,write,*|*,edit,*|*,bash,*) ad_readonly=0 ;; esac
  [ -n "$ad_maxturns" ] && ad_turnflag="--max-turns $ad_maxturns"

  # claude-oauth: reach the $0 Max subscription via the approved client. Prompt on
  # STDIN, not argv (a ~1MB prompt overflows ARG_MAX, mu-b6tl). --exclude-dynamic-
  # system-prompt-sections strips claude's agent scaffolding.
  if [ "$ad_prov" = "claude-oauth" ]; then
    # The nested-cc "hang" was never structural: the child finished its work
    # and then the cc Stop-hook idle watcher long-polled before the process
    # returned (diagnosed by probe 2026-07-30). The watcher lives OUTSIDE
    # this repo — agent_tools scripts/hooks/dialogue-rewake.sh, wired via
    # ~/.claude/settings.json — and reads DIALOGUE_REWAKE_MAX, whose default
    # is the 30-minute cap itself (`max="${DIALOGUE_REWAKE_MAX:-1800}"`,
    # dialogue-rewake.sh:75). Setting it to 0 suppresses the watch: correct
    # for a one-shot review seat, which never wants an idle rewake.
    # Wire-verified nested 2026-09-02: pong in ~30s where the old path sat
    # for the full watch. The former CLAUDECODE skip guard (exit 75) is
    # gone; both claude panel seats are live again. If a host lacks the
    # hook, nothing changes (no watcher, no wait); if a future hook ignores
    # the var, the seat degrades to `timeout $ad_timeout` below — bounded,
    # not a hang — and shows up as a timed-out seat, not a frozen caller.
    # No claude binary on PATH means this seat cannot run in this
    # environment at all — route around rather than error (exit 75).
    if ! command -v claude >/dev/null 2>&1; then
      echo "agent-dispatch: claude-oauth seat unusable: no 'claude' binary on PATH. Skipping this seat (exit 75)." >&2
      return 75
    fi
    ad_clsys=""
    [ -n "${SYSPROMPT:-}" ] && [ -r "$SYSPROMPT" ] && ad_clsys="--append-system-prompt-file $SYSPROMPT"
    ad_mcpflag=""
    [ -n "${MCP_CONFIG:-}" ] && ad_mcpflag="--mcp-config $MCP_CONFIG"
    ad_cltools=""
    if [ -n "$ad_tools" ]; then
      ad_cltools="$(_ad_claude_tools "$ad_tools")"
      [ -n "$ad_cltools" ] && ad_cltools="--allowedTools $ad_cltools"
    fi
    # mu-cbmru: an unconstrained claude seat runs claude's default tool set
    # (Write/Edit/Bash included), so the backstop refuses it outright.
    [ -n "$ad_cltools" ] || ad_readonly=0
    # shellcheck disable=SC2086 — $ad_clsys/$ad_mcpflag/$ad_perm/$ad_cltools intentionally word-split
    # OAuth/subscription lane: scrub the metered-API selectors before `claude` —
    # if ANTHROPIC_API_KEY / ANTHROPIC_BASE_URL leak in from the operator shell,
    # the CLI silently switches to per-token API billing (the mu-odtc trap).
    # A timed-out seat stays exit 124, deliberately. 75 means "seat never
    # ran — safe to re-run the task on another rank" (mu-spawn:184 falls
    # through on 75 only); a timed-out claude PROCESS may have partially
    # executed, and for a write-capable worker re-running elsewhere risks
    # double-execution. Keeping 124: the panel's retry loops
    # (review-panel/dispatch.sh, consensus.sh, ai-review.sh) retry a
    # timed-out seat once; a rank-walk aborts loudly instead of silently
    # re-running — the conservative failure if the out-of-repo rewake
    # contract above ever stops holding.
    ad_rc=0
    timeout "$ad_timeout" env -u ANTHROPIC_API_KEY -u ANTHROPIC_BASE_URL \
      DIALOGUE_REWAKE_MAX=0 \
      claude -p --model "$ad_model" $ad_clsys $ad_mcpflag $ad_perm \
      --exclude-dynamic-system-prompt-sections \
      $ad_cltools --output-format text <"$ad_pf" 2>>"$ad_errlog" || ad_rc=$?
    # mu-cbmru: NOT checked for out-of-tokens. Exit 4 is mu's convention for
    # its typed ProviderUsageLimit; `claude -p` has its own exit-code
    # vocabulary (and prints its cap message to stdout, which this errlog
    # never sees), so reading 4 here would be guessing. A capped claude seat
    # fails loudly until that lane has a signal of its own.
    return "$ad_rc"
  fi

  # mu providers (codex / ollama / openrouter / ...): hermetic --bare session.
  ad_sysflags=""
  [ -n "${SYSPROMPT:-}" ] && [ -r "$SYSPROMPT" ] && ad_sysflags="--append-system-prompt $SYSPROMPT"
  # `mu ask --tools` accepts built-ins only. MCP-imported tools are granted by
  # enabling MCP on the one-shot daemon, then letting MCP import them at startup.
  ad_mcpflag=""; ad_mu_tools=""
  if [ -n "$ad_tools" ]; then
    ad_old_ifs=$IFS; IFS=,
    for ad_tool in $ad_tools; do
      IFS=$ad_old_ifs
      ad_tool=$(printf '%s' "$ad_tool" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')
      case "$ad_tool" in
        "") ;;
        code_recall|code_status) ad_mcpflag="--enable-mcp" ;;
        *) ad_mu_tools="${ad_mu_tools:+$ad_mu_tools,}$ad_tool" ;;
      esac
      IFS=,
    done
    IFS=$ad_old_ifs
  fi

  # LOTO acquire: when dispatching to the shared ollama box, hold the cooperative
  # lease for the run so concurrent ollama workers SERIALISE instead of evicting
  # each other (bead mu-0pqk: 256k-context models don't co-reside). This is the
  # acquire half that composes with agent-role's demote-when-held half (#383):
  # demote steers *resolvers* off a box already held; this serialises the workers
  # that still land on ollama (e.g. several resolved to it while the box was free).
  # Bare WAIT mode + with-ollama-lease's own fail-open mean an etcd outage runs
  # WITHOUT the lease rather than blocking. Opt out with AGENT_DISPATCH_NO_LEASE=1.
  # ci-aipr/review-panel sets AGENT_DISPATCH_OLLAMA_SKIP_IF_HELD=1 so its ollama
  # rank exits 75 immediately when an interactive operator already holds the box,
  # rather than waiting in the fair queue and stalling the whole gate.
  ad_lease=""
  case "$ad_prov" in
    ollama|ollama-*)
      if [ -z "${AGENT_DISPATCH_NO_LEASE:-}" ] && command -v with-ollama-lease >/dev/null 2>&1; then
        if [ "${AGENT_DISPATCH_OLLAMA_SKIP_IF_HELD:-}" = "1" ]; then
          ad_lease="with-ollama-lease --skip-if-held"
          # A down/unreachable box is functionally the same as a held one: skip
          # fast (exit 75) so a hosted rank forms the verdict, instead of acquiring
          # the (free) lock and then hanging on the dead box for the whole timeout.
          # Probe the base mu itself dials; short cap; a missing curl just skips
          # the probe (old behaviour, no regression).
          local _ad_ob="${OLLAMA_API_BASE:-http://10.1.1.143:11434}"
          if command -v curl >/dev/null 2>&1 &&
             ! curl -fsS -m "${AGENT_DISPATCH_OLLAMA_HEALTH_TIMEOUT:-3}" "${_ad_ob%/}/api/version" >/dev/null 2>&1; then
            printf 'agent-dispatch: ollama box %s unreachable; skipping rank (exit 75, same as held)\n' "$_ad_ob" >>"${ad_errlog:-/dev/stderr}"
            return 75
          fi
        else
          ad_lease="with-ollama-lease"
        fi
        # Ensure the lease outlives a long run (with-ollama-lease defaults TTL to
        # 1200s > the 900s reviewer cap; only override for a larger timeout, and
        # never clobber a caller-set TTL).
        if [ -z "${AGENT_SESSION_TTL:-}" ] && [ "$ad_timeout" -gt 1080 ]; then
          AGENT_SESSION_TTL=$((ad_timeout + 120)); export AGENT_SESSION_TTL
        fi
      fi
      ;;
  esac

  # shellcheck disable=SC2086 — $ad_lease/$ad_sysflags/$ad_yolo/$ad_mcpflag/$ad_turnflag/tool flags intentionally word-split
  ad_rc=0
  ad_errmark=$(_ad_err_mark)
  if [ -n "$ad_tools" ] && [ -n "$ad_mu_tools" ]; then
    $ad_lease timeout "$ad_timeout" "$ad_mu" ask --bare --provider "$ad_prov" --model "$ad_model" \
      --thinking "$ad_thinking" $ad_sysflags $ad_yolo $ad_mcpflag $ad_turnflag --tools "$ad_mu_tools" \
      --prompt-file "$ad_pf" 2>>"$ad_errlog" || ad_rc=$?
  elif [ -n "$ad_tools" ]; then
    $ad_lease timeout "$ad_timeout" "$ad_mu" ask --bare --provider "$ad_prov" --model "$ad_model" \
      --thinking "$ad_thinking" $ad_sysflags $ad_yolo $ad_mcpflag $ad_turnflag \
      --prompt-file "$ad_pf" 2>>"$ad_errlog" || ad_rc=$?
  else
    $ad_lease timeout "$ad_timeout" "$ad_mu" ask --bare --provider "$ad_prov" --model "$ad_model" \
      --thinking "$ad_thinking" $ad_sysflags $ad_turnflag --prompt-file "$ad_pf" 2>>"$ad_errlog" || ad_rc=$?
  fi
  # mu-cbmru: the lane is OUT OF TOKENS (a subscription/plan usage cap, or a
  # metered lane with no credit left). Whether the next rank may take the task
  # is the CALLER's call, not an inference from here — see _ad_out_of_tokens.
  _ad_out_of_tokens "$ad_rc" "$ad_prov/$ad_model" "$ad_readonly" && return 75
  # mu-hqr6: provider-auth failure = seat unusable in this environment
  # (missing/expired credential), same route-around class as the held-box
  # guard. Safe for these one-shot dispatches: an auth
  # failure means the task never ran, so trying the next rank cannot
  # double-execute anything. Narrow signature ("provider: auth" is mu's
  # error prefix) on the errlog tail; timeouts (124) and lease skips (75)
  # keep their own meanings.
  # mu-cbmru: still the last 5 lines, but now scoped to THIS invocation — the
  # errlog is shared across a rank walk, so an earlier rank's auth failure
  # could classify this one. Widening the window instead would be worse, not
  # better: the model's own reasoning and tool echoes are on this stream.
  # ...and never 4: an out-of-tokens run the route-around DECLINED (no caller
  # opt-in, or a write-capable seat) must keep its loud failure. Letting it
  # fall through here would convert it to 75 by the back door.
  if [ "$ad_rc" -ne 0 ] && [ "$ad_rc" -ne 75 ] && [ "$ad_rc" -ne 124 ] &&
     [ "$ad_rc" -ne 4 ] &&
     _ad_err_tail "$ad_errmark" 5 | grep -q 'provider: auth'; then
    echo "agent-dispatch: $ad_prov seat unusable: provider auth failed (see $ad_errlog). Skipping this seat (exit 75)." >&2
    return 75
  fi
  return "$ad_rc"
}
