# Architecture: capability delegation via tool policy + biscuits

| field      | value                                          |
| ---------- | ---------------------------------------------- |
| doc_id     | architecture/capability-delegation             |
| status     | architecture breadcrumb (partial impl in v1)   |
| created    | 2026-05-10                                     |
| updated    | 2026-05-10                                     |
| authors    | tcovert + claude (cooperating sessions)        |
| supersedes | none                                           |

## Framing

mu has two design directions converging:

1. **Tools should have structured runtime metadata**, not just an
   LLM-facing description. Today the runtime knows a tool's name and
   input schema and nothing else; the description is for the model.
   This conversation: tools should also carry policy that the runtime
   acts on directly (side-effect class, retry posture, permission
   level, idempotence). Memory `796e3263`-adjacent surface — the
   "model as co-implementer of safety" insight in structured form.

2. **Sub-sessions / delegates need attenuable capabilities**. From
   earlier in the evening (memory `b27e6b4a`): biscuit-auth as the
   right primitive for in-process agent delegation, deferred until
   mu is daily-usable. Now relevant because tool policy is the
   surface that capabilities attenuate.

**The unification:** tool policy lives in a daemon-level registry as
the *most-permissive* form. Each session carries a biscuit (or none,
for the root session) that **narrows** what the session can do. The
runtime checks both: (a) the registry's max policy, and (b) the
session's biscuit. Tool dispatch fails closed if either check fails.

The macaroon/biscuit attenuation guarantee — a biscuit can only be
narrowed by its holder, never widened — is the load-bearing math.
Even a confused or compromised parent agent cannot grant a child
more scope than the parent has.

## Thesis

> A session's capability is the biscuit it holds. Tool dispatch is
> a verification step: the tool's daemon-level policy ∧ the session's
> biscuit caveats ∧ the call's specific arguments. The runtime owns
> "what's allowed to happen"; the model owns "what to try."

This separates two concerns:

| | Owned by | Stored where |
|---|---|---|
| What the tool *can* do | daemon | `ToolRegistry::max_policy` |
| What this session can *invoke* | session | `Session::capability_biscuit` |
| Which call to make right now | model | `AssistantMessage::ToolCall` |

The runtime is the only thing that can answer "yes, run this." The
model is never trusted to honor policy; it's invited to *suggest*
calls, but the runtime decides.

## Goals

- **Bounded blast radius for agents.** A coding agent that should
  only read files cannot accidentally `rm -rf` because its biscuit
  doesn't carry the `Mutating` or `Destructive` capability.
- **Defensible against prompt-injection-driven delegation.** A
  parent agent confused by an adversarial prompt cannot grant a
  child more scope than it has. The math forbids it.
- **Runtime-enforced retry policy.** Tools mark `retry: Never` when
  retries don't make sense (e.g. allowlist rejections); the
  AgentLoop refuses duplicate calls regardless of what the model
  wants. Prevents the kind of multi-turn tool loop observed during
  the bash live test (2026-05-10).
- **Audit trail composes with the event log.** Each
  `SessionBranched` event records the biscuit summary (allowed
  tools, max calls, expiry); subsequent `ToolCall` events can be
  checked against the granting biscuit during postmortem.
- **TUI / CLI surface knows side-effect class.** Read-only tools
  rendered differently from mutating; destructive tools rendered as
  red. The model's description doesn't have to do this work —
  policy says it explicitly.

## Non-goals

- Replacing the existing `--bash-yolo` / `--bash-allow` CLI surface
  in v1. Tool policy strengthens it without breaking compatibility.
- Implementing biscuit verification before sub-session spawning
  exists. The "delegate session" primitive (`session.delegate` or
  equivalent) is a prerequisite; tool policy v1 ships without it.
- Defining a full permission grammar (allowlists per tool argument,
  argument-shape constraints, etc.). v1 is coarse-grained: per-tool
  on/off, side-effect class, retry posture.

## ToolPolicy (v1, runtime-side only)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolPolicy {
    pub side_effects: SideEffects,
    pub permission: PermissionLevel,
    pub retry: RetryPolicy,
    pub idempotent: bool,
    pub side_effects_note: Option<String>,  // human-readable
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffects {
    ReadOnly,       // grep, glob, read, ls, git status
    Mutating,       // edit, write, bash with file ops
    Destructive,    // rm, drop-table, force-push (none of mu's
                    // current tools are here in strict mode)
    External,       // network, fetch (not implemented yet)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionLevel {
    Allow,           // dispatch immediately
    Ask,             // emit session.input_required, wait for approval
    AskOnce,         // first call asks; subsequent ones auto-allow
    Deny,            // refuse immediately
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RetryPolicy {
    Never,                     // refuse the same call+args twice
    UpTo { times: u32 },       // bounded retry
    ModelDecides,              // current behavior — let the model loop
}
```

Each existing tool declares its v1 policy:

| Tool | side_effects | retry (strict) | retry (yolo) | idempotent |
|---|---|---|---|---|
| read | ReadOnly | ModelDecides | — | yes |
| glob | ReadOnly | ModelDecides | — | yes |
| grep | ReadOnly | ModelDecides | — | yes |
| ls | ReadOnly | ModelDecides | — | yes |
| write | Mutating | ModelDecides | — | yes (same content) |
| edit | Mutating | ModelDecides | — | yes (old_string unique) |
| bash (strict) | Mutating | **Never** on allowlist-reject | — | depends on command |
| bash (yolo) | Destructive | ModelDecides | ModelDecides | depends on command |

The `bash (strict) retry: Never on allowlist-reject` is the specific
v1 fix for the live-test hang observed 2026-05-10 evening.

## Tool registry

Daemon-level. The registry is the *complete* set of tools the daemon
knows how to dispatch. A session's biscuit can never reference a
tool not in the registry.

```rust
pub struct ToolRegistry {
    tools: HashMap<String, RegisteredTool>,
}

pub struct RegisteredTool {
    pub spec: ToolSpec,             // LLM-facing + ToolPolicy
    pub max_policy: ToolPolicy,     // most-permissive form
    pub factory: Arc<dyn Fn() -> Arc<dyn Tool> + Send + Sync>,
}
```

`max_policy` answers "what's the most this daemon will allow for
this tool, even in yolo mode?" — bash yolo is `Destructive`, but
some hypothetical tool could be capped at `Mutating` even when
biscuit caveats would allow `Destructive`.

## Biscuit shape (v2 — after sub-session primitive exists)

Each session carries an `Option<Biscuit>`:

```rust
pub struct SessionCapability {
    pub biscuit: Option<Biscuit>,   // None = root session
}
```

Biscuits carry caveats that tool-dispatch verifies. Caveats span:

```text
tool name set            allowed_tools = ["read", "grep", "glob"]
side-effect cap          max_side_effects = ReadOnly
call quota               max_tool_calls = 10
token budget             max_total_tokens = 50_000
time budget              expires_at = now + 5min
parent session id        parent = "session-42"
delegation depth         max_subdelegation_depth = 0  // child cannot
                                                       // delegate further
```

Minting a child biscuit (when sub-session spawning lands):

```rust
let child_biscuit = parent_session.capability.biscuit
    .as_ref()
    .map(Biscuit::clone)
    .unwrap_or_else(Biscuit::root_for(daemon_root_key))
    .attenuate()
    .with_allowed_tools(["read", "grep", "glob"])
    .with_max_side_effects(SideEffects::ReadOnly)
    .with_max_tool_calls(10)
    .with_expires_at(now + Duration::from_secs(300))
    .build();
```

The math: every caveat the parent has is preserved; new caveats can
only narrow further. The child cannot widen any of them.

## Verification path

Every tool dispatch:

```text
1. ToolRegistry::lookup(call.name)  → max_policy
2. max_policy.allow(&call)?         → daemon-level baseline
3. session.capability.biscuit.verify(&call, &tool_policy)?
                                    → session-level narrowing
4. RetryPolicy check: has this exact call+args run in the last
   N turns?                         → runtime-enforced loop guard
5. PermissionLevel check: Allow / Ask / Deny
                                    → emit session.input_required
                                      if Ask
6. dispatch tool
7. ToolCallStarted event with policy summary
8. ... tool runs ...
9. ToolCallCompleted event
```

Each step fails closed. The first failing step aborts dispatch with
a clear error to the model AND a `session.callout` to the UI/log.

## Composition with prior threads

| Thread | How capability delegation composes |
|---|---|
| **Event-sourced context** (`event-sourced-context.md`) | `SessionBranched` event records the minted biscuit's caveats; future replay can verify each tool dispatch against the granting biscuit |
| **Session tree** (memory `7e44f7ad`) | Biscuits travel *down* the tree only. Child sessions inherit parent's biscuit attenuated. TUI shows the capability scope at each node. |
| **`session.input_required`** (Phase 2 bash, future spec) | The "who approves?" question: walk up the biscuit chain to the nearest ancestor with `permission: Allow` for this tool. Often the human at the root, sometimes a parent agent. |
| **Cooperating sessions / mailbox** | Inter-session messages carry biscuit references so the recipient knows the sender's scope. |
| **Cost-aware orchestration** (long-term) | `max_total_tokens` caveat *is* the budget. Orchestrators mint biscuits sized to delegated-task complexity; runtime enforces. |
| **`--bash-yolo` / `--bash-allow`** (mu-026) | These remain as daemon-startup defaults. A session's biscuit can only *narrow* what they allow; yolo mode + a `ReadOnly` biscuit still results in read-only enforcement because the biscuit check happens *after* the daemon-level policy. |

## Phased implementation

**v1 (next, ~1.5–2h):** ToolPolicy struct on ToolSpec. Each existing
tool declares its policy. AgentLoop enforces `RetryPolicy::Never` —
duplicate (tool, args) within N turns is refused at dispatch. No
biscuits yet. Fixes the bash retry-loop bug. Lays type-system
foundation.

**v2:** `session.input_required` notification + dispatch-time gate
on `PermissionLevel::Ask`. Single root session can be permission-
prompted by the user/UI. Still no biscuits.

**v3 (after sub-session primitive lands):** Biscuit minting on
session delegation. Verification on dispatch. Audit trail in the
event log. This is when the math kicks in.

**v4 (later):** Argument-shape constraints in biscuits (Datalog
caveats: "allowed_paths starts_with /home/user/project"). Bigger
spec when concrete use case exists.

## Out-of-circuit warnings

- **OOC-1 (root key management).** Biscuits need a daemon-level
  root keypair to mint and verify. Persist in
  `~/.config/mu/auth/biscuit-root.json` mode 0600. If the key is
  lost, all outstanding biscuits become unverifiable; sessions
  effectively reset.
- **OOC-2 (tool policy is not a sandbox).** Even
  `SideEffects::ReadOnly` tools can read `~/.ssh/id_rsa` if the
  daemon user can. Tool policy gates *which tools run*, not *what
  files they touch*. True isolation needs OS sandboxing (Phase 3
  of bash; future).
- **OOC-3 (biscuit verification cost).** Datalog evaluation per
  tool call is small (microseconds for typical caveats) but
  measurable. Keep biscuits coarse. Per-call argument checks
  through Datalog will dominate cost if abused.
- **OOC-4 (model description still matters).** The structured
  policy is *for the runtime*. The tool's `description` still
  needs to tell the model "this is read-only" or "this command
  was rejected; do not retry variants" — the model can't read
  `retry: Never` and adjust its behavior. The two surfaces co-
  evolve.

## Suggested follow-up specs

```text
mu-028 (or next):  ToolPolicy struct + RetryPolicy enforcement
                   (the v1 above; coming next this session)
mu-XXX:           session.input_required + Ask/AskOnce permission
                   gate
mu-XXX:           session.delegate (or session.spawn_child) RPC —
                   the sub-session primitive
mu-XXX:           Biscuit minting + verification in the agent loop
                   (depends on session.delegate)
mu-XXX:           biscuit caveat: max_total_tokens hooked to the
                   accounting layer
mu-XXX:           argument-shape caveats (Datalog)
```

## Related memories

- `796e3263` — feedback memory about not asking permission to stop;
  cited here because the conversation that prompted this doc was
  the user explicitly choosing forward motion over check-ins.
- `b27e6b4a` — biscuit-auth direction; deferred until daily-usable.
  Tool policy v1 is the first step toward making it concrete.
- `7e44f7ad` — TUI session tree; biscuits travel down the tree.
- `b0e06d20` — per-turn vs cumulative accounting; biscuit
  `max_total_tokens` will compose with the cumulative view.
- `17e4a19d` — accounting requirement; biscuit budget enforcement is
  the runtime answer to "this delegate ran out of budget."

## AWS-capability axis (mu-f5o, 2026-05-13)

The `Capability` struct grows an `aws` axis: a typed namespace of
AWS-role grants the session holds. Matches the catalog at
`mu-aws-sandbox-infra/capabilities/aws.json` (entries like
`aws.scout.readonly`, `aws.sandbox.build`).

```rust
pub struct AwsCapability {
    pub name: String,                              // catalog name
    pub session_policy: Option<serde_json::Value>, // optional inline narrowing
}

// On Capability:
pub aws: HashSet<AwsCapability>,                   // empty = no AWS access

// On CapabilityAttenuations:
pub aws: Option<Vec<AwsCapability>>,               // None = no narrowing requested
```

**Shape rationale (HashSet, not `Option<AwsCapability>` or
enum-of-variants):**

- `HashSet` over `Option` so a worker can legitimately hold multiple
  AWS caps at once (e.g. `aws.scout.readonly` + `aws.sandbox.build`) —
  the per-experiment design fork in `mu-aws-sandbox-infra/docs/mu-integration.md`
  required this for the broker pattern.
- Struct-of-axes over enum-of-variants: the earlier `Capability` enum
  proposal in the AWS-side doc predates this codebase's design.
  `Capability` is already a multi-axis struct (`allowed_tools`,
  `expires_at_unix_ms`, `max_tool_calls_remaining`, `autonomy`, ...);
  AWS as another axis preserves uniform attenuation algebra and
  composes with the broker pattern without enum-match scaffolding at
  every dispatch site.

**Hash/Eq:** `serde_json::Value` lacks a `Hash` impl, so `AwsCapability`
hand-implements `Hash` on `name` only while keeping `PartialEq` over
both fields. `HashSet<AwsCapability>` therefore stores caps as distinct
elements whenever either field differs; the practical invariant
"one cap per name" is enforced by the `intersect` operation, which
collapses same-name pairs.

### Narrowing-only semantics (INV-1 generalized)

`AwsCapability::intersect(&self, other) -> Option<Self>`:
- Different name → `None` (incompatible; drop on intersect).
- Same name, both `session_policy = None` → `Some` with `None` policy.
- Same name, exactly one `Some` policy → `Some` with that policy
  (the `Some` side is narrower than the `None` side, which represents
  "use the role's identity policy as-is").
- Same name, both `Some` policies → `None`. AWS-style policy
  intersection (the algebra over Effect/Action/Resource/Condition) is
  deferred to a future bead. Conservative `None` preserves the
  narrowing-only invariant rather than producing a possibly-too-broad
  combined policy.

`intersect_aws_sets(a, b) -> HashSet<AwsCapability>`: pairwise per-name
intersect; names present on only one side are dropped. Result ⊆ a and
result ⊆ b by name.

Two top-level operations consume this:

1. `Capability::attenuate(&self, &CapabilityAttenuations)` —
   asymmetric (parent + delegate's narrowing request). For AWS:
   - `attenuations.aws = None` → child inherits parent's set (no
     narrowing requested on this axis).
   - `attenuations.aws = Some(req)` → child gets
     `intersect_aws_sets(parent.aws, req-as-set)`.
2. `Capability::intersect(&self, &Self) -> Self` (new) — symmetric
   composition of two grants (broker-pattern primitive: parent's
   grant ∩ judge's grant). For AWS: `intersect_aws_sets` directly.

INV-1 (narrowing-only) is the load-bearing invariant: every result of
`intersect` and `attenuate` is ⊆ both inputs on every axis, the AWS
axis included. Out of `Capability::root()` (empty AWS set), no
sequence of `attenuate` or `intersect` calls can produce a non-empty
AWS set — caps must be explicitly granted at construction.

### AWS catalog resolution (mu-ysh, 2026-05-14)

`AwsCapability` is the session-held grant; the operator-managed catalog is the
external map from grant name to concrete AWS materialization metadata. The first
Mu-side integration layer is intentionally pure data/validation:

```rust
pub struct AwsCapabilityCatalog {
    pub schema_version: u32,
    pub default_region: Option<String>,
    pub capabilities: BTreeMap<String, AwsCapabilityCatalogEntry>,
}
```

The catalog shape matches `mu-aws-sandbox-infra/capabilities/aws.json`. It is
loaded by future broker/runner code from a trusted path; `mu-core` itself does
no AWS I/O and assumes no roles. It only answers:

1. Does this `AwsCapability.name` exist in the catalog?
2. Is it materialized now (`aws_profile` + `role_arn`, and not `status:
   "planned"`)?
3. What operator/auditor metadata should be preserved (description, mutation
   flag, policies, constraints, smoke-test hints)?

Resolution is fail-closed:

- unknown name -> `UnknownCapability`
- known but planned/unmaterialized -> `CapabilityNotMaterialized`
- materialized -> entry may be handed to a broker/runner layer

This is the bridge between the in-process attenuation algebra and hard AWS
identity enforcement. It does not grant authority; it prevents future execution
code from treating a bare string as sufficient authority.

### Deferred

- `session_policy` intersection algorithm (the AWS-policy algebra over
  Effect/Action/Resource/Condition). Field is carried; intersect of
  two `Some` returns `None` for now.
- The capability-broker runtime (judge + orchestrator agents that
  produce attenuated grants from a session's stated need).
- Implementing the `aws-recon` skill specified in
  `specs/mu-039-aws-recon-skill.md`: activation with an `AwsCapability`, narrow
  recon/planner/auditor tool schemas, and runner-backed execution through
  `mu-aws-capability-run.sh`.
- Cross-daemon biscuit-auth serialization (a future swap of the
  in-process types for signed biscuit tokens; the algebra stays
  identical).

## Changelog

- 2026-05-10 — initial doc, drafted from a late-evening design
  conversation. Captures the convergence of tool-policy structured
  metadata + biscuit attenuation into one coherent capability
  substrate. No immediate implementation beyond v1
  (`RetryPolicy::Never` enforcement) coming in the next commit.
- 2026-05-13 — mu-f5o adds the `aws` axis on `Capability` (typed
  AWS role-grants) and a new symmetric `Capability::intersect()` as
  the broker-pattern primitive. AWS axis follows narrowing-only
  semantics (INV-1 generalized). `session_policy` intersection
  deferred.
