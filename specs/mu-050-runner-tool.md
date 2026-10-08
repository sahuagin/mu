# Spec: runner tool — a configured command run under a named grant

| field      | value                                                |
| ---------- | ---------------------------------------------------- |
| spec_id    | mu-050                                               |
| status     | implemented                                          |
| created    | 2026-10-06                                           |
| authors    | cc (claude-opus-5-5)                                 |
| supersedes | mu-039 (the AWS-specific `aws_recon` tool)           |
| bead       | mu-aws-mi2-18xx1.4 (relates: mu-a6xrr)               |

## Why

mu carries no provider-specific code. A tool that acts on an external system
under delegated authority is a command the operator configures, run through
an external **runner** that turns a grant name into real authority (assumes a
role, selects a key). mu's part is the gate, the bounds and the record. See
`specs/architecture/capability-delegation.md` (grant axis).

## Configuration: `[[tools.runner]]` (`RunnerToolConfig`)

The section exists only where `Config` has a `tools` field (added with the
wiring); before that, `Config` denies it as an unknown field.

| field                | default   | meaning                                                         |
| -------------------- | --------- | --------------------------------------------------------------- |
| `name`               | required  | tool name; unique; not a reserved name (below)                  |
| `description`        | required  | model-facing prose; the grant is appended                       |
| `grant`              | required  | `required_grant` of the tool; passed to the runner verbatim     |
| `runner`             | required  | absolute path, or bare name resolved once on absolute `PATH` entries |
| `command`            | `[]`      | argv the runner execs after materializing the grant             |
| `cwd`                | none      | must be a directory                                             |
| `timeout_secs`       | 900       | outer timeout, > 0; also the most a call may request            |
| `max_output_bytes`   | 256 KiB   | per stream, > 0; bounds the record (see Result)                 |
| `capture_grace_secs` | 2         | > 0; see Capture                                                |
| `env_passthrough`    | `[]`      | daemon variable names passed to the runner; see Environment     |
| `allow_args`         | false     | whether the model may append `args` (total ≤ `max_output_bytes`) |
| `side_effects`       | external  | `external` or higher; lower is refused                          |
| `permission`         | allow     | the grant gate is the control; `ask` for a mutating grant       |

The `[tools]` section is read strictly at startup
(`Config::load_default_runner_tools`): unlike the rest of the config, which
falls back to defaults on a schema error (mu-a6xrr), an invalid `[tools]`
section, or a layer that is not valid TOML, refuses startup. `build_tools`
then builds every entry, selected or not (`RunnerTool::from_config` names the
entry and field on failure), and refuses duplicate or reserved names. Reserved names: the tools
`build_tools` builds (`read write ls edit grep glob memory_recall bash
final_answer`), the session-injected tools (`spawn_worker mailbox watch
start_autonomous schedule_wakeup discover`), the rebound dialogue tools
(`dialogue_say dialogue_poll dm who`) and the mesh code-index tools
(`code_recall code_status code_sources`).

## Invocation and gating

`<runner> <grant> -- <command...> [args...]`, stdin closed. Cancellation and
the deadline are checked before every spawn attempt; a spawn that fails with
ETXTBSY is retried a few times. The dispatch gate
refuses the call unless the session holds `grant` (and refuses a grant held
with a `policy`, which no runner interface conveys yet). `derived_effects`
marks the tool as reaching the network and spending.

A root session holds no grants. The operator hands one over at creation:
`CreateSessionRequest.grants` (`mu ask --grant <name>`, repeatable), applied
directly on the root capability like the autonomy grant and never reachable by
the model; a child holds at most its parent's grants (attenuation is
intersect-only). Grants are not on the event log, so a rehydrated session comes
back without them (mu-59hmw).

## Environment

The runner starts from an empty environment plus the non-secret basics the
`bash` tool also keeps (`PATH HOME USER SHELL TERM LANG TZ TMPDIR PWD`, minus
any whose name looks like a secret) plus exactly the `env_passthrough` names.
Daemon credentials and loader controls are not inherited. The catalog that
maps grant names to authority belongs to the runner; mu never reads it, so a
runner that wants the catalog version on record reports it in its output.

## Containment

The runner runs in a process group led by an anchor process mu owns (`sh -c
'read _'`, no pipes but its stdin). The anchor, alive or a zombie, pins the
group id until teardown reaps it, after the final group signal, so no group
signal can reach a reused id. On timeout or cancel the group gets SIGTERM and
a grace for the runner to exit, then SIGKILL; after a clean exit (stragglers
only) it gets SIGKILL directly. `env_passthrough` is the operator's explicit
choice and may name secrets the runner needs (e.g. a session token). This is the limit
of mu's reach: a descendant that calls `setsid`/`setpgid` leaves the group.
Containing such a process (jail, cgroup, reaper) is the runner's job.

## Capture

Each stream is drained up to `max_output_bytes`. The drain deadline starts at
the outer deadline plus grace and is shortened to *now + grace* when the
runner exits, is killed, or the call is cancelled. A drain stops at EOF, a
read error, its deadline, or when the call ends, and keeps what it read. The
drains are owned by the call and aborted if its future is dropped.

## Result

Success: `{"kind":"runner_result", tool, grant, exit_code, duration_ms,
timeout_secs, stdout, stderr, truncated:{stdout,stderr,limit_bytes},
runner:{path,command,args,cwd}}`. `stdout` is the raw captured text, never
re-parsed (a runner that emits JSON is read from it as-is).

The record is compact JSON delivered verbatim (no ingestion filter). stdout,
stderr and the echoed `args` are each at most `max_output_bytes` and are
carried as JSON strings, so a call adds at most about 3 × 6 ×
`max_output_bytes` plus the configured command and a small envelope to
context; ordinary text costs about one byte per byte.

Every other outcome is an error: `{"kind":"runner_refusal", reason, message,
tool, grant, stderr, stderr_capture, runner}`. Once the runner
has started, the result also carries `stdout_partial`, `stdout_capture`,
`duration_ms`, and, after it exits, `exit_code`. A `*_capture` object is
`{truncated, timed_out, error}`.

| reason               | meaning                                                         |
| -------------------- | --------------------------------------------------------------- |
| `invalid_args`       | bad `timeout_secs`/`args`, or a horizon that cannot be scheduled |
| `spawn_failed`       | the runner could not be started                                  |
| `timeout`            | the outer timeout fired (our limit); group killed if started     |
| `cancelled`          | the call was cancelled; group killed if started                  |
| `wait_failed`        | waiting on the runner failed; group killed                       |
| `capture_timeout`    | the runner exited but a holder kept the pipe past the grace      |
| `capture_failed`     | a read error or a failed drain                                   |
| `nonzero_exit`       | the runner exited unsuccessfully                                 |

## Known limits

- A schema error elsewhere in the config still drops the rest of it to
  defaults unless `MU_CONFIG_STRICT=1` (mu-a6xrr); the `[tools]` section is
  read strictly regardless.
- A grant's `policy` is not conveyed to the runner; such grants are refused.
