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

| field                | default   | meaning                                                         |
| -------------------- | --------- | --------------------------------------------------------------- |
| `name`               | required  | tool name; unique; not a reserved name (below)                  |
| `description`        | required  | model-facing prose; the grant is appended                       |
| `grant`              | required  | `required_grant` of the tool; passed to the runner verbatim     |
| `runner`             | required  | absolute path, or bare name resolved once on absolute `PATH` entries |
| `command`            | `[]`      | argv the runner execs after materializing the grant             |
| `cwd`                | none      | must be a directory                                             |
| `timeout_secs`       | 900       | outer timeout, > 0; also the most a call may request            |
| `max_output_bytes`   | 256 KiB   | per stream, > 0; also the bound on what reaches model context   |
| `capture_grace_secs` | 2         | > 0; see Capture                                                |
| `env_passthrough`    | `[]`      | daemon variable names passed to the runner; see Environment     |
| `allow_args`         | false     | whether the model may append `args`                             |
| `side_effects`       | external  | `external` or higher; lower is refused                          |
| `permission`         | allow     | the grant gate is the control; `ask` for a mutating grant       |

Every entry is validated when tools are built, selected or not; any failure
refuses startup and names the entry and field. Reserved names: the tools
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

## Environment

The runner starts from an empty environment plus the non-secret basics the
`bash` tool also keeps (`PATH HOME USER SHELL TERM LANG TZ TMPDIR PWD`, minus
any whose name looks like a secret) plus exactly the `env_passthrough` names.
Daemon credentials and loader controls are not inherited. The catalog that
maps grant names to authority belongs to the runner; mu never reads it, so a
runner that wants the catalog version on record reports it in its output.

## Containment

The runner leads its own process group. On timeout, cancel, and also after a
clean exit, the group is terminated (SIGTERM, then SIGKILL). Once the leader
has been reaped its id no longer pins the group, so each signal is preceded
by a membership probe and an empty group is never signalled. This is the limit
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
timeout_secs, summary,
stdout, stderr, truncated:{stdout,stderr,limit_bytes}, runner:{path,command,
args,cwd}}`. `summary` is stdout parsed as JSON when it parses; `stdout` is
then null.

The result is delivered verbatim (no ingestion filter), so the JSON reaches
the model intact; `max_output_bytes` is what bounds it.

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

- A config schema error drops the whole config, runner entries included,
  unless `MU_CONFIG_STRICT=1` (mu-a6xrr).
- A grant's `policy` is not conveyed to the runner; such grants are refused.
