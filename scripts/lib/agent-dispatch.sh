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
#              ("" => zero tools: omits --tools/--max-turns/--allowedTools)
#   SYSPROMPT  system-prompt file (optional; overrides daemon)  default unset
#   TIMEOUT    wall-clock backstop, seconds                     default 900
#   MAX_TURNS  mu --max-turns (mu path, when TOOLS non-empty)   default 15
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
#   AGENT_SESSION_OWNER/_TTL passed through to with-ollama-lease when it wraps an
#              ollama dispatch (export one OWNER to let a multi-call run share the lease)

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
  local ad_clsys ad_sysflags ad_cltools ad_perm ad_yolo ad_lease ad_mcpflag
  local ad_mu_tools ad_tool ad_old_ifs ad_env
  ad_prov="$1"; ad_model="$2"
  ad_pf="${3:-${PROMPT_FILE:-}}"
  [ -n "$ad_pf" ] || { echo "agent_dispatch: no prompt file (arg 3 or \$PROMPT_FILE)" >&2; return 2; }
  ad_tools="${TOOLS-read,grep}"            # '-' not ':-': honour an explicit empty TOOLS
  ad_timeout="${TIMEOUT:-900}"
  ad_maxturns="${MAX_TURNS:-15}"
  ad_thinking="${THINKING:-low}"
  ad_mu="${MU:-$(command -v mu || true)}"
  ad_errlog="${ERRLOG:-${TMPDIR:-/tmp}/agent-dispatch.$$.err}"

  # Write tools (write/edit/bash) need extra flags so a non-interactive worker
  # doesn't deadlock (claude) and can run a shell (mu). Read-only sets stay clean.
  ad_perm=""; ad_yolo=""
  case ",$ad_tools," in *,write,*|*,edit,*|*,bash,*) ad_perm="--permission-mode bypassPermissions" ;; esac
  case ",$ad_tools," in *,bash,*) ad_yolo="--bash-yolo" ;; esac

  # claude-oauth: reach the $0 Max subscription via the approved client. Prompt on
  # STDIN, not argv (a ~1MB prompt overflows ARG_MAX, mu-b6tl). --exclude-dynamic-
  # system-prompt-sections strips claude's agent scaffolding. Because this lane's
  # contract is OAuth/subscription billing, scrub Anthropic API/Bedrock/Vertex env
  # before invoking `claude`: the CLI can otherwise prefer API-key / credit-pool
  # mode when ANTHROPIC_API_KEY leaks in from the operator shell (mu-odtc).
  if [ "$ad_prov" = "claude-oauth" ]; then
    ad_clsys=""
    [ -n "${SYSPROMPT:-}" ] && [ -r "$SYSPROMPT" ] && ad_clsys="--append-system-prompt-file $SYSPROMPT"
    ad_mcpflag=""
    [ -n "${MCP_CONFIG:-}" ] && ad_mcpflag="--mcp-config $MCP_CONFIG"
    ad_cltools=""
    if [ -n "$ad_tools" ]; then
      ad_cltools="$(_ad_claude_tools "$ad_tools")"
      [ -n "$ad_cltools" ] && ad_cltools="--allowedTools $ad_cltools"
    fi
    # shellcheck disable=SC2086 — $ad_clsys/$ad_mcpflag/$ad_perm/$ad_cltools intentionally word-split
    (
      # Force Claude Code's OAuth/subscription path. Preserve CLAUDE_* OAuth
      # state (e.g. CLAUDE_CODE_OAUTH_TOKEN), but remove Anthropic API/provider
      # selectors that could reroute the approved client to metered API billing.
      for ad_env in $(env | sed -n 's/^\(ANTHROPIC[A-Za-z0-9_]*\)=.*/\1/p'); do
        unset "$ad_env"
      done
      unset CLAUDE_CODE_USE_BEDROCK CLAUDE_CODE_USE_VERTEX
      timeout "$ad_timeout" claude -p --model "$ad_model" $ad_clsys $ad_mcpflag $ad_perm \
        --exclude-dynamic-system-prompt-sections \
        $ad_cltools --output-format text <"$ad_pf" 2>>"$ad_errlog"
    )
    return
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

  # shellcheck disable=SC2086 — $ad_lease/$ad_sysflags/$ad_yolo/$ad_mcpflag/tool flags intentionally word-split
  if [ -n "$ad_tools" ] && [ -n "$ad_mu_tools" ]; then
    $ad_lease timeout "$ad_timeout" "$ad_mu" ask --bare --provider "$ad_prov" --model "$ad_model" \
      --thinking "$ad_thinking" $ad_sysflags $ad_yolo $ad_mcpflag --max-turns "$ad_maxturns" --tools "$ad_mu_tools" \
      --prompt-file "$ad_pf" 2>>"$ad_errlog"
  elif [ -n "$ad_tools" ]; then
    $ad_lease timeout "$ad_timeout" "$ad_mu" ask --bare --provider "$ad_prov" --model "$ad_model" \
      --thinking "$ad_thinking" $ad_sysflags $ad_yolo $ad_mcpflag --max-turns "$ad_maxturns" \
      --prompt-file "$ad_pf" 2>>"$ad_errlog"
  else
    $ad_lease timeout "$ad_timeout" "$ad_mu" ask --bare --provider "$ad_prov" --model "$ad_model" \
      --thinking "$ad_thinking" $ad_sysflags --prompt-file "$ad_pf" 2>>"$ad_errlog"
  fi
}
