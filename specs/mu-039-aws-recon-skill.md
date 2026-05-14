# Spec: aws-recon skill as the first AWS capability demo

| field      | value                                       |
| ---------- | ------------------------------------------- |
| spec_id    | mu-039                                      |
| status     | proposed                                    |
| created    | 2026-05-14                                  |
| updated    | 2026-05-14                                  |
| authors    | tcovert + pi                                |
| supersedes | none                                        |
| beads      | mu-zvi                                      |

## Why

`AwsCapability` and `AwsCapabilityCatalog` give Mu a typed, fail-closed way to
represent and resolve AWS authority, but no session uses that authority yet. The
smallest useful end-to-end demonstration is an `aws-recon` skill:

```text
operator enables aws-recon skill
  -> session receives an attenuated `aws.scout.readonly` capability
  -> skill exposes narrow recon/planner/audit tool surfaces
  -> tool execution routes through the AWS sandbox runner/catalog
  -> AWS/IAM enforces the boundary
  -> Mu records enough metadata to join tool calls to CloudTrail later
```

This spec is intentionally not a general AWS executor. It is a read-only recon
path plus local planning/audit helpers, designed to prove the capability stack
without granting broad ambient AWS credentials to the model or to `mu-core`.

## Existing pieces

Already landed in Mu:

- `Capability::aws: HashSet<AwsCapability>` — session-held AWS grants.
- `Capability::attenuate()` and `Capability::intersect()` — narrowing-only
  algebra across axes.
- `AwsCapabilityCatalog` / `AwsCapabilityCatalogEntry` — pure JSON catalog
  resolution and materialized-vs-planned validation (`mu-ysh`).
- `SpanKind::SkillActivation` and `SpanKind::ToolSchema` — rope-visible skill
  and tool activation surfaces.

Already present in the AWS sandbox infra repo:

- `capabilities/aws.json` — operator-managed capability catalog.
- `scripts/mu-aws-capability-run.sh` — clears ambient AWS env and selects the
  concrete AWS profile for a named capability.
- `scripts/aws-recon.sh` / `scripts/aws-recon.py` — read-only inventory.
- `scripts/aws-plan-from-recon.sh` / `.py` — local no-AWS-call planner from a
  recon report.
- `scripts/aws-cloudtrail-session-events.sh` / `.py` — auditor review path.

## Scope

### In

- A skill definition named `aws-recon`.
- Skill activation produces a `SkillActivation` span describing:
  - requested AWS capability names;
  - catalog path or catalog digest;
  - materialized catalog entry summaries;
  - runner path/version metadata;
  - audit join fields when available.
- The session capability is narrowed to include exactly the requested AWS grants,
  normally:

  ```json
  {
    "aws": [{ "name": "aws.scout.readonly" }]
  }
  ```

- Narrow tool schemas exposed by the skill:
  - `aws_recon` — calls the read-only recon runner.
  - `aws_plan_from_recon` — local planner over a recon report directory.
  - `aws_cloudtrail_session_events` — auditor-only helper requiring
    `aws.audit.security`.
- Every AWS-backed tool call resolves its `AwsCapability` through
  `AwsCapabilityCatalog::resolve_materialized()` before invoking a runner.
- Unknown, planned, or unmaterialized capability names fail before execution.
- Tool-call events record enough metadata for audit:
  - Mu session id;
  - AWS capability name;
  - catalog digest/ref;
  - local AWS profile name from the catalog;
  - role ARN from the catalog;
  - runner command path;
  - report directory or output artifact path;
  - STS session/caller metadata if the runner returns it.

### Out

- General `aws_cli` shell passthrough.
- Applying Terraform/OpenTofu or mutating AWS resources.
- Inline AWS `session_policy` narrowing. The field exists on `AwsCapability`,
  but both-Some policy intersection remains deferred.
- Cross-daemon Biscuit serialization.
- OS-level sandboxing of the runner. The runner should be explicit and narrow,
  but Capsicum/Casper/jail enforcement is a later layer.
- Reading live AWS credentials in `mu-core`. `mu-core` only owns types and
  validation; `mu-coding` or a broker process owns execution.

## Skill activation model

The skill activation has two effects:

1. **Context/projection effect** — the rope gains `SkillActivation` and
   `ToolSchema` spans.
2. **Authority effect** — the child/session capability includes a narrowed AWS
   axis.

Conceptual activation event:

```jsonc
{
  "kind": "skill_activated",
  "skill_id": "aws-recon",
  "capability_request": {
    "aws": [{ "name": "aws.scout.readonly" }]
  },
  "catalog": {
    "path": ".../capabilities/aws.json",
    "schema_version": 1,
    "digest": "sha256:..."
  },
  "materialized_caps": [
    {
      "name": "aws.scout.readonly",
      "aws_profile": "mu-readonly-scout",
      "role_arn": "arn:aws:iam::...:role/mu-readonly-scout",
      "mutation_allowed": false
    }
  ],
  "tool_schemas": ["aws_recon", "aws_plan_from_recon"]
}
```

The model may see the skill instructions and tool schemas. The operator sees a
collapsed badge such as:

```text
[skill:aws-recon] aws.scout.readonly via catalog sha256:...
```

The auditor view should retain the full catalog/capability metadata.

## Tool surfaces

### `aws_recon`

Read-only AWS inventory.

Input:

```jsonc
{
  "capability": "aws.scout.readonly",   // default from skill activation
  "call_timeout_secs": 45,
  "output_dir": null                    // optional; default ignored reports dir
}
```

Pre-dispatch checks:

1. the tool's runtime policy declares `required_aws_capability:
   "aws.scout.readonly"`;
2. the agent loop verifies the session capability contains
   `AwsCapability { name: "aws.scout.readonly", ... }` before `Tool::execute`
   runs;
3. catalog has a materialized entry for `capability`;
4. catalog entry has `mutation_allowed == false` unless operator explicitly
   allows a mutating recon variant (not in v1);
5. runner path is configured and executable.

Execution:

```text
scripts/mu-aws-capability-run.sh aws.scout.readonly -- \
  scripts/aws-recon.sh aws.scout.readonly --call-timeout 45
```

Output:

```jsonc
{
  "report_dir": "reports/aws-recon/20260514T...Z",
  "summary_path": "reports/aws-recon/20260514T...Z/summary.json",
  "capability_used": "aws.scout.readonly",
  "aws_profile": "mu-readonly-scout",
  "caller_arn": "...",
  "findings_count": 0
}
```

### `aws_plan_from_recon`

Local no-AWS-call planner over a recon report.

Input:

```jsonc
{
  "report_dir": "reports/aws-recon/20260514T...Z"
}
```

Authority:

- Requires file read access to the report directory.
- Does **not** require AWS capability because it makes no AWS calls.
- Should still record the originating `capability_used` from the report summary
  if available.

### `aws_cloudtrail_session_events`

Auditor helper for joining Mu tool calls to CloudTrail.

Input:

```jsonc
{
  "aws_assumed_role_session_name": "...",
  "output_path": "reports/cloudtrail/<session>.json"
}
```

Authority:

- Requires `aws.audit.security` in the session's AWS capability set.
- Resolves `aws.audit.security` through the catalog.
- Uses the same runner discipline as `aws_recon`.

## Audit join

The recon runner should surface a stable join key whenever possible:

```text
MU_SESSION_ID                 // Mu session
MU_TOOL_CALL_ID               // Mu tool-call event id or generated id
MU_AWS_CAPABILITY             // e.g. aws.scout.readonly
MU_AWS_CATALOG_DIGEST         // catalog version used for resolution
MU_AWS_ASSUMED_ROLE_ARN       // from sts get-caller-identity / runner metadata
MU_AWS_ASSUMED_ROLE_SESSION   // session name visible in CloudTrail, if available
```

CloudTrail can then answer: what AWS calls actually happened under this Mu tool
call? Mu can answer: which session, capability, skill activation, and model
context led to that call?

## Invariants

- **INV-1 (no ambient AWS authority).** The model never receives raw AWS
  credentials. Tool execution clears ambient AWS environment variables before
  selecting the catalog-backed profile/role.
- **INV-2 (agent-loop AWS gate before execution).** Tools that require AWS
  authority declare `ToolPolicy::required_aws_capability`; the agent loop refuses
  dispatch before `Tool::execute` unless the session holds that AWS grant.
- **INV-3 (catalog resolution before runner execution).** A named AWS capability is not
  executable until `resolve_materialized()` succeeds.
- **INV-4 (planned means not executable).** Catalog entries with `status:
  "planned"` are visible for design/operator context but fail before runner
  execution.
- **INV-5 (read-only first).** The v1 skill grants only `aws.scout.readonly` by
  default. Auditor review uses `aws.audit.security`; mutation capabilities are
  out of scope.
- **INV-6 (local planning is separate).** `aws_plan_from_recon` consumes saved
  reports and does not make AWS calls; it should not require AWS credentials.
- **INV-7 (event log is source of truth).** Compaction may drop pointers to raw
  recon output from AgentView, but the event log/artifact refs remain available
  for OperatorView/AuditorView/replay.
- **INV-8 (fail closed on ambiguity).** Unknown capabilities, planned catalog
  entries, missing runner paths, and ambiguous inline session policies deny
  execution rather than guessing.

## Implementation phases

### Phase A — spec and catalog plumbing

- Land this spec.
- Keep `mu-core` as pure type/validation layer.
- Decide the runtime owner for skill/tool execution (`mu-coding` vs external
  broker process) before adding live runner calls.

### Phase B — local skill/tool skeleton

- Add `ToolPolicy::required_aws_capability` and enforce it in the agent loop.
- Add `aws-recon` skill metadata.
- Add tool schemas for `aws_recon` and `aws_plan_from_recon`.
- Implement dry-run tests with fixture catalog and fixture recon reports.
- No live AWS calls in CI.

### Phase C — runner-backed local execution

- Wire `aws_recon` to the sandbox runner behind explicit local config.
- Configure live local execution with:
  - `MU_AWS_CAPABILITY_CATALOG=/path/to/capabilities/aws.json`
  - `MU_AWS_RECON_RUNNER=/path/to/scripts/mu-aws-capability-run.sh`
  - `MU_AWS_RECON_SCRIPT=/path/to/scripts/aws-recon.py`
  - optional `MU_AWS_RECON_CWD=/path/to/mu-aws-sandbox-infra`
- The tool invokes:

  ```text
  $MU_AWS_RECON_RUNNER aws.scout.readonly -- \
    $MU_AWS_RECON_SCRIPT --call-timeout <n> [--out-dir <dir>]
  ```

- Return report/artifact paths and runner stdout summary metadata.
- Keep live AWS tests opt-in/manual; CI remains fixture/mock-runner only.

### Phase D — audit join

- Propagate Mu session/tool-call ids into the runner environment.
- Record returned STS/CloudTrail join keys in event-log projections.
- Add auditor helper for `aws.audit.security`.

## Open questions

- Where should the catalog path be configured: daemon config, skill config, or
  per-session activation argument?
- Should the runner be a local script initially or a broker RPC from day one?
- What is the stable event payload shape for `SkillActivation` capability
  metadata?
- Should report artifacts be content-addressed before entering the rope, or is a
  path + digest enough for v1?
- How should Mu represent local filesystem authority for ignored report
  directories before the future `fs` capability axis exists?

## Related

- `specs/architecture/capability-delegation.md`
- `specs/architecture/event-sourced-context.md`
- `specs/architecture/mu-capability-substrate.md`
- `specs/architecture/os-enforced-agent-sandboxing.md`
- AWS sandbox infra (`/home/tcovert/src/personal/mu-aws-sandbox-infra`):
  - `docs/mu-integration.md`
  - `docs/recon.md`
  - `capabilities/aws.json`
  - `scripts/mu-aws-capability-run.sh`
  - `scripts/aws-recon.sh`
  - `scripts/aws-plan-from-recon.sh`
  - `scripts/aws-cloudtrail-session-events.sh`
