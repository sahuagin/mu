# Architecture vision: Mu capability substrate

| field      | value                                      |
| ---------- | ------------------------------------------ |
| doc_id     | architecture/mu-capability-substrate       |
| status     | vision / integration map                   |
| created    | 2026-05-13                                 |
| updated    | 2026-05-13                                 |
| authors    | tcovert + pi                               |
| related    | architecture/event-sourced-context, architecture/capability-delegation, architecture/os-enforced-agent-sandboxing |

## Thesis

Mu's durable substrate is an append-only event log plus retained pointer sets,
attenuable capabilities, and hard enforcement backends.

The event log records what happened. Contexts, user displays, audit reports,
mailboxes, discovery lists, and replay/debug tools are projections over that
record. Capabilities describe what authority a session may exercise; OS, broker,
and cloud primitives enforce what is physically possible.

```text
append-only event log
  -> retained pointer sets / ropes
      -> AgentView      (what the model sees)
      -> OperatorView   (what the human sees)
      -> AuditorView    (what authority was exercised and why)
      -> ReplayView     (what happened, in order)
      -> MailboxView    (coordination messages and handles)

session capability
  -> tool policy decision
  -> broker / Capsicum / Casper / jail / IAM enforcement
  -> audit event with requested authority and effective enforcement
```

This document is glue. The detailed designs live in the related specs; this file
names the shared model so future work does not duplicate or contradict it.

## Event log is the source of truth

Model context is derived from the event log. The event log is not derived from
model context.

Compaction, display truncation, provider-specific rendering, and mailbox
summaries must not delete or rewrite the underlying record. They only affect
which pointers are retained in a particular projection.

Useful event families include:

```text
SessionCreated
SessionDelegated
CapabilityAttenuated
PointerSetInitialized
PointerSetUpdated
ToolRequested
ToolAllowed / ToolDenied
ToolExecuted
ExternalIdentityAssumed
BrokerEscalationRequested
HumanApproved / HumanDenied
MailboxMessageSent
ContextProjected
CompactionSummaryCreated
```

The same event can be useful to several views. A large tool result may be full
fidelity in AgentView, collapsed in OperatorView, and represented by a digest plus
authority metadata in AuditorView.

## Context is pointer management

A session context is a retained pointer set over immutable source events and
synthetic summary spans.

A useful mental model is:

```text
stable base
  system instructions
  active skills
  active tool schemas
  active capability summary

working set
  current task prompt
  recent turns
  relevant file/tool results
  current plan and findings

summaries
  synthetic spans summarizing older event ranges
  backrefs to source event ids / span ids
```

The stable base should normally survive compaction. The working set changes
frequently. Summaries are added when the model no longer needs every detailed
pointer in its immediate context.

Compaction means:

```text
add pointer to summary S(events E10..E22)
drop this projection's pointers to E10..E22
keep E10..E22 in the append-only event log
```

It does not mean deleting history.

## Delegation and cloning

Spawning or cloning an agent is pointer-set copy plus capability attenuation.

```text
child_session =
  selected parent pointers
  + child task prompt
  + synthetic handoff spans, if any
  + child capability = parent capability ∩ requested attenuation
```

The child starts with a precise, auditable snapshot of what it inherited. It can
then compact or drop pointers independently. Nothing is lost when the child
"forgets"; it merely stops retaining that pointer in its own projection.

A `SessionDelegated` event should make the inheritance explicit:

```text
parent_session_id
child_session_id
initial_pointer_set_manifest_ref
initial_pointer_set_digest
parent_capability_ref
attenuation_request
child_capability_ref
spawn_reason / task
```

This lets an auditor answer: what did the child know, what authority did it hold,
and why did it act?

## Capability algebra is fail-closed

Capabilities are attenuate-only. A child may receive the intersection of the
parent's capability and a requested narrowing. There is no widening operation.

Some axes do not have a safe or obvious merge. In those cases Mu should return
`None` / deny / drop that axis rather than guessing which side wins.

```text
safe intersection:
  parent tools {read, grep, write}
  requested tools {read, grep}
  -> {read, grep}

ambiguous policy merge:
  parent aws.scout.readonly with session_policy A
  requested aws.scout.readonly with session_policy B
  if A ∩ B is not implemented or not provably narrowing
  -> None for that capability, not "pick A" or "pick B"
```

The invariant is stronger than convenience: a union or ambiguous combination must
never create more authority. Fail closed and make the missing merge operation an
explicit implementation task.

## Authority and information flow

Information may flow sideways through mailbox messages. Authority does not,
unless it is explicitly delegated and attenuated.

```text
authority for a tool call =
  current session capability
  ∩ request/message authority, if the call is acting on a request
  ∩ daemon tool policy
  ∩ concrete enforcement backend
```

Examples:

- A researcher can tell a coder "I found likely bug X." The coder may edit only
  if the coder's own capability permits editing.
- An auditor can flag a policy violation without gaining authority to mutate
  infra.
- An infra delegate can hold `aws.sandbox.build` without inheriting unrelated
  filesystem or mailbox authority.

This avoids confused-deputy and authority-laundering failures: one session's
message cannot smuggle broader authority into another session's tool call.

## Biscuits are delegation credentials, not the sandbox

Biscuit-style tokens are the portable proof of delegated authority, especially
for cross-daemon sessions, mailbox handles, and persisted delegation records.
They should encode the same attenuate-only facts represented by the in-process
`Capability` type.

They are not the final enforcement boundary. The intended stack is:

| Layer | Role |
| ----- | ---- |
| `Capability` / Biscuit | decision plane and delegation proof |
| tool policy | runtime dispatch and side-effect classification |
| brokers | audited authority holders and escalation surface |
| Capsicum / Casper / Flower-like primitives | local OS-enforced restriction |
| jails / containers / rctl / devfs | coarse process/filesystem/network boundary |
| AWS IAM / STS session policy | cloud-side hard enforcement |

Same-daemon delegation can use the Rust `Capability` struct directly. Cross-
daemon or durable delegation should carry a signed token/reference so the
receiver can verify the scope instead of trusting a claim.

## Auditor role

Auditor should be a first-class Mu session role, not only an AWS role.

A Mu auditor needs read-only access to the event log, session tree, capability
lineage, mailbox metadata, and enforcement records. It should be able to answer:

- what capability did this session hold at each tool call?
- was the call allowed, denied, escalated, or brokered?
- which external identity or OS profile enforced the decision?
- did a mailbox request cause a tool call under inappropriate authority?
- did the model's AgentView at the time contain the evidence it later cited?

Auditor power should be epistemic by default: broad read, narrow or no write.
Mutation requires a separate explicit capability.

## Related specs

- `specs/architecture/event-sourced-context.md` — retained pointer sets,
  AgentView/OperatorView, compaction, subagent handoff.
- `specs/architecture/capability-delegation.md` — tool policy, Biscuit-shaped
  attenuation, delegation invariants.
- `specs/architecture/os-enforced-agent-sandboxing.md` — Capsicum, Casper,
  jails, brokers, and hard OS enforcement.
- `specs/mu-037-peer-discovery-mailbox.md` — mailbox and peer communication;
  trust model points toward capability credentials.
- `specs/mu-038-projection-queries-and-discovery.md` — session/event queries and
  discovery projections.
