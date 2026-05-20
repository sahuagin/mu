# Architecture: session lifecycle as projection over the event log

| field      | value                                                                          |
| ---------- | ------------------------------------------------------------------------------ |
| doc_id     | architecture/session-lifecycle                                                 |
| status     | architecture breadcrumb (implementation pending — see follow-on beads)         |
| created    | 2026-05-21                                                                     |
| updated    | 2026-05-21                                                                     |
| authors    | tcovert + claude (cooperating sessions)                                        |
| supersedes | none                                                                           |
| related    | event-sourced-context, capability-delegation, mu-capability-substrate          |

## Framing

mu has historically treated session lifetime informally. A session "is open" while its TUI pane is attached or while it's actively streaming; it "becomes closed" when the user dismisses it or the autonomous loop terminates. This works at small scale but creates ambiguity at the boundaries: what does it mean to *resume* a session that was paused for a year? What does `/handoff` actually transfer when authority and context are decoupled from any "live" process? Where does Capsicum sandboxing intersect with session authority?

A conversation between tcovert and Claude on 2026-05-20 surfaced the resolution: **there is no active/inactive distinction at the substrate layer.** Sessions are immutable event logs. "Live" is itself a projection over recent events, not a property of the session. The TUI is one consumer of that projection (it draws a Live sessions panel); the autonomous loop's heartbeat is another; cross-machine observers can build their own. None of them holds the canonical aliveness state because there is none to hold.

This dissolves three previously-thorny cases into one:

1. `/handoff` to a new session in the same TUI ("same operator, fresh context").
2. `/handoff` to a separately spawned worker process ("different runtime, same lineage").
3. Resumption after arbitrary pause — minutes, days, a year — when the original process is long gone.

All three collapse to: *make a new session whose event log starts with a provenance pointer (or a copy) of the prior log's context map; the new session re-establishes authority by reading the log and walking the capability chain against current time/count bounds.* The TUI being attached or not is irrelevant to the substrate operation.

This document records the framing, the invariants it implies, and the follow-on work it enables.

## Thesis

> A session is an event log. Authority for a session is established by walking the capability chain in the log and intersecting it with the current state of capability axes (time bounds, count bounds, attenuation history). "Active" and "inactive" are not substrate properties — they are projections over recent activity (`LiveProjection`). No operation is ever gated on aliveness; all gates are capability checks against current event-log state.

This separates four concerns that were previously conflated:

| Concern                  | Owned by                                  | Lives in                       |
| ------------------------ | ----------------------------------------- | ------------------------------ |
| What happened            | Event log (durable, append-only)          | `SessionEventLog` JSONL files  |
| Who is allowed to act    | Capability/Biscuit chain (in events)      | Embedded in events             |
| When did it last happen  | `LiveProjection` (derived, ephemeral)     | Computed on demand from log    |
| Who is currently looking | TUI presence / open streams (ephemeral)   | Process state, not log state   |

The first two are substrate. The last two are projections. Conflating them is the historical mistake that made handoff and resumption feel asymmetric.

## The substrate move

This is the same architectural move mu applies elsewhere:

- The rope (`crates/mu-core/src/context/rope.rs`) is the substrate; the flat `content` string is a projection.
- The event log is the substrate; the TUI transcript is a projection.
- The capability chain is the substrate; "is this tool allowed right now" is a projection over the chain + current time.

Applied to session lifetime: **the event log is the substrate; "this session is alive" is a projection over the log's recent activity.** Once that lens is applied, the surface use-cases (resume, handoff, archive, audit-replay) all become operations over the log with different temporal or capability constraints, not separate code paths.

See [substrate-thinking memory `5245ad79`] for the broader pattern this instantiates.

## `LiveProjection`

A `LiveProjection` is a pure function of the event log + current wall-clock time. Its output is a small struct answering "what is currently happening on this session?" Consumers include:

- The TUI's `F1` / `F2` Live sessions panel (today renders from runtime state; should derive from `LiveProjection`).
- The autonomous loop's heartbeat (`mu-coding/src/serve/forwarder.rs` task telemetry).
- External observers (an SRE dashboard, a future `mu-status` CLI, a remote audit endpoint).

### Rules

A session is *live* iff at least one of:

1. An event with timestamp ≥ `now - LIVENESS_WINDOW` exists in the log. (Default `LIVENESS_WINDOW = 30s`; configurable.)
2. An in-flight provider stream is open against this session (tracked via `MessageStart` without matching `MessageEnd` or `Done`).
3. An autonomous loop is registered against this session (tracked via `AutonomousLoopStarted` without matching `AutonomousLoopStopped`).
4. A TUI attachment is registered (tracked via `TUIAttached` without matching `TUIDetached`).

Conditions 2-4 are themselves event-log facts. Only condition 1 is wall-clock-dependent. The function is deterministic given (log, now).

### Why this matters

Today the TUI maintains its own "Live sessions" list as runtime state. A second observer (say a remote `mu-status` query) would re-implement its own version, and the two could drift. Centralizing the projection means:

- The TUI's `F1` panel and any external observer see identical answers to "what's live."
- A session that completes streaming but stays open in the TUI is "live" by rule 4 only — distinguishable from a session that's actively producing tokens (rules 2 + 4).
- "Idle" is a derived classification: live (rules 2-4) + no activity in the last 30s (rule 1 false).

### Out of scope for this spec

- The implementation of `LiveProjection` (a follow-on bead).
- The migration of `F1`'s current code from runtime state to projection — a separate bead, blocked on the projection landing.
- Cross-daemon `LiveProjection` (querying liveness on another machine) — relevant only after cross-daemon delegation lands (mu-hoe0).

## Reconstitution

Resuming a session — whether after seconds, days, or arbitrary pause — is the operation of establishing authority and context against a stored event log. It has two layers of authority check, distinct in source and in failure mode.

### Layer 1: descriptor authority (can I open this log?)

Source: outside the session's event log. Either:

- OS-level: the operator owns the session's event-log files; the mu-server process has read access.
- Capsicum-sandboxed: mu-server granted the session-worker's descriptor at fork; the worker can read what was granted, nothing else.
- Biscuit-bearer: a separate root capsule attests "this principal can open logs matching pattern X." The capsule itself is held outside the log (operator's keyring, a remote authentication server, etc.).

This layer answers *whether the reading process is allowed to see the log at all.* If layer 1 fails, the reconstitution returns "not authorized to read" without revealing any log contents.

### Layer 2: runtime authority (can I act on what I read?)

Source: the event log itself. Walk the capability chain (delegation events, attenuation events, approval grants) from the session's origin to its most recent state. Apply current wall-clock to time-bounded axes (`expires_at_unix_ms`); apply current count state to count-bounded axes (`max_tool_calls_remaining` — these decrement as tool calls fire and persist in the log).

The result is the **current effective Capability** for the session. It may differ from the Capability the session held at its last "live" moment:

- Time-bound axes that expired during pause are now empty (e.g., a session paused at `T+30min` with `expires_at_unix_ms = T+1h`, resumed at `T+2h`, has empty `allowed_tools` for any tool gated on that capability).
- Count-bound axes are unchanged by pause length; they reflect the count state at last decrement.
- Attenuation history is unchanged.

If layer 2 returns the empty Capability for some axis, the resumed session cannot use that axis until it re-acquires authority (typically: operator approval through the standard delegation flow). Other axes remain available.

### The asymmetry

Authority-to-read (layer 1) is binary: yes or no. It doesn't decay with time.

Authority-to-act (layer 2) is multi-axis and time-bounded. It can decay even while the log is fully readable.

This asymmetry is the answer to "if I have permission to read the log, do I have permission to use what's in it?" Mostly yes — but capability axes that have expired in wall-clock time are dead until re-granted, even though the log entries that granted them remain readable.

### Provenance

A reconstituted session emits an event (`SessionResumed`) recording:

- The source session's ID and last-event timestamp.
- The current Capability after layer-2 reconciliation.
- Which axes (if any) were reduced or eliminated by time/count bounds.
- The principal who authorized the read (bootstrap authority — see §Bootstrap).

This event is itself part of the new session's log, making the resumption trace queryable by future audit.

## Handoff modes

`/handoff` is "make a new session whose event log starts with a pointer to (or copy of) this session's context map; the new session re-establishes authority via the reconstitution process above." There are two modes, encoded in the handoff event payload, chosen per-handoff:

### Mode A — parent pointer

The new session's first event is `SessionHandoffStarted { from: <parent-session-id>, mode: ParentPointer, context_window: <last-N-spans-id> }`. Context resolution at runtime walks the parent's log to materialize the spans named in the context_window.

**Pros:**
- Cheap. No data duplication.
- Edits to the parent log (if any) are visible to the child (rare, since logs are append-only — but compaction summaries and projection rebuilds can affect what spans are visible).
- Lineage is explicit and queryable: child can answer "what session did I come from?"

**Cons:**
- Requires the parent log to remain accessible. If the parent log is GC'd, archived, or moved to cold storage, the chain breaks.
- Cross-machine handoff requires reaching the parent's storage from the child's location.

**Use when:** the parent is alive or expected to remain accessible. Worker-spawn pattern (a parent dispatches a sub-session and waits for it to complete) is the canonical fit. `/handoff` to a fresh session in the same TUI also fits, since the parent log is local.

### Mode B — copy with origin reference

The new session's first event is `SessionHandoffStarted { from: <parent-session-id>, mode: CopyWithOrigin, context_spans: <inlined-spans> }`. The relevant context is *copied* into the child's log. A `originated_from: <parent-session-id>` annotation preserves the lineage record without requiring runtime access to the parent.

**Pros:**
- Durable. Survives parent-log GC, archival, or migration.
- Cross-machine handoff is straightforward (the new session is self-contained).
- Forward-recoverable: even if the parent log is later destroyed entirely, the child has everything needed to resume.

**Cons:**
- Storage cost — context spans are duplicated.
- Edits to the parent are invisible to the child (acceptable for append-only logs, but worth being explicit about).

**Use when:** the handoff is to a separate process, a different machine, or "later" (after the parent's natural lifetime). Long-pause resumption ("resume tomorrow," "resume next year") naturally fits mode B because the parent process is gone by then.

### How the choice is made

Two paths:

1. **Default by handoff target.** Same-process / same-machine / same-TUI defaults to mode A. Cross-process / cross-machine defaults to mode B. The handoff operator (the agent or human invoking `/handoff`) can override.
2. **Explicit operator choice.** `/handoff --mode copy` forces mode B even when mode A is available. `/handoff --mode pointer` forces mode A and fails if the parent isn't reachable.

The mode is recorded in the `SessionHandoffStarted` payload so audit can reconstruct which path was taken.

### Out of scope

- The actual encoding of `context_window` and `context_spans` in events (defined alongside `/handoff` implementation; see follow-on beads).
- A "promote pointer to copy" operation (later-binding the choice when archival is impending). Useful but not in v1.

## Model migration as event

The operator may resume a session after the originally-used provider or model is unavailable: the model was retired, the provider's API changed, or the operator chose to switch. The substrate treatment: **model migration is an event, not an exception.**

### Event variant: `ModelMigrated`

```rust
EventPayload::ModelMigrated {
    from_provider: ProviderKind,
    from_model: String,
    to_provider: ProviderKind,
    to_model: String,
    reason: ModelMigrationReason,
}

enum ModelMigrationReason {
    Retired,         // Original model no longer available
    OperatorSwitch,  // Operator chose a different model
    ProviderError,   // Original provider returned a permanent error
    PolicyChange,    // Capability/cost policy now excludes original
}
```

This event lands in the session's log at the moment of migration. Subsequent turns use the new provider/model. Older turns retain their original provenance.

### Per-turn provider/model identity

Every `ModelCallStart` (or equivalent — names match the existing event taxonomy in `event-sourced-context.md`) already carries provider + model fields. The `ModelMigrated` event is the *transition marker*; the per-turn fields are the canonical record of which provider/model handled which turn.

This makes a multi-model session reconstructable:

```text
turn 1-50: anthropic_api / claude-opus-4-7
ModelMigrated { reason: Retired }
turn 51-72: anthropic_api / claude-opus-5-0
ModelMigrated { reason: OperatorSwitch }
turn 73-: openai_codex / gpt-5.5
```

Any audit query ("what did the model see at turn 67?") resolves correctly against the per-turn provenance.

### Resumption across migration

The reconstitution process (above) is identical whether the resumption stays within the same model or crosses a migration. The only difference is: at resumption time, if the originally-used model is unavailable, a `ModelMigrated` event is emitted *before* the next provider call. The session continues; the audit trail records the choice.

This is the substrate-thinking move applied to model availability: don't treat migration as a special case requiring branch logic; treat it as a marker event that downstream projections (transcript, audit, cost accounting per model) can reason about.

## Bootstrap

For any session to be readable, *some* authority must come from outside the log. The log records authority *use* and *delegation* — it does not contain the root of its own authorization. This is a soundness property: a log cannot bootstrap its own legitimacy.

Concretely, the root of authority for mu sessions today is:

- **OS-level**: tcovert at the operator's machine. The mu-server process runs as that user; it can open files owned by that user. This is the present default.
- **Future — Biscuit root capsule**: a token signed by a root key, held outside the log, that attests "principal P can open logs matching pattern X." Biscuit's offline-verifiable token chain is a good fit because the capsule itself can be moved across machines without re-issuance.
- **Future — federated identity**: an external identity provider (an SSO endpoint, a corporate directory) vouches for the principal; mu-server treats the federated identity as the bootstrap authority.

In all three cases, the log records the *use* of the bootstrap authority (`SessionOpened { principal: ..., bootstrap: BootstrapSource::OS|Biscuit|Federated }`), but the source of the root is upstream.

### Why this matters for `/handoff` and resumption

If the bootstrap principal at resumption time is identical to the bootstrap principal at session creation time, no special handling is needed — the resuming process simply opens the log and proceeds through reconstitution.

If the bootstrap principal differs (e.g., handoff to a different operator, federated identity migration), the new principal must independently hold authority to read the source log. This is a layer-1 check (descriptor authority). The log records who opened it and under what bootstrap; an audit trail of cross-principal accesses is preserved.

mu v1 assumes a single bootstrap (OS user). v2+ designs across principals will extend this section.

## Capsicum granularity is layering-independent

Capsicum is the OS-level sandboxing mechanism (`specs/architecture/os-enforced-agent-sandboxing.md`). It constrains what file descriptors a process holds and what syscalls it can make. It is orthogonal to the application-level Capability check that gates per-session authority.

Two designs exist; this spec works with either:

### Design 1 — centralized mu-server, application-level checks

One mu-server process holds all session event logs and all interface capabilities. Each incoming request specifies a session ID; mu-server applies the Capability check against that session's log and either acts or refuses. Capsicum constrains mu-server's overall reach (what filesystems it can see, what network calls it can make), but does not separate sessions from each other within the process.

**Pros:** Single locus of authority enforcement. Easy to reason about. Cheap (one process).
**Cons:** A code-level compromise of mu-server (e.g., a deserialization bug) potentially exposes all sessions' logs.

### Design 2 — per-session capsicum-sandboxed workers

Each session runs in its own worker process, sandboxed via Capsicum (`cap_enter` after binding only the descriptors needed for that session). Inter-session access is impossible at the OS layer; the application-level Capability check is a second-line defense.

**Pros:** OS-enforced isolation. A code-level compromise of one worker cannot escalate to other sessions.
**Cons:** More IPC. More processes. More complex orchestration.

### How the spec is independent

The substrate (event log, Capability chain, `LiveProjection`, handoff modes, `ModelMigrated` event) is identical under both designs. The difference is *which process opens the descriptor*: in design 1, mu-server; in design 2, the per-session worker. The application-level Capability check runs in either case; the event log is the same shape; reconstitution and handoff work the same way.

mu v1 may launch with design 1 (simpler, faster to ship) and migrate to design 2 when the threat model demands it. This spec is unchanged by that migration. The migration itself is operational (deployment topology) rather than architectural.

## Invariants

Six testable contracts derived from the framing above. These belong in `specs/architecture/seams.md` (the seams suite, bead `mu-ml6m`) as part of the **tool authority seam** and **session lifecycle seam** auditor checklists.

**INV-LIV-1 — No operation is gated on session aliveness.**
For every code path that decides whether to permit an operation (tool dispatch, model call, event emission, log read), the gate is a Capability check against current event-log state — never a check of "is the session active." Auditor verification: grep for conditional logic against `is_active`, `is_live`, `session.alive`, etc.; every match must be replaced with a Capability check or removed.

**INV-LIV-2 — Liveness is a derived projection.**
The substrate has no aliveness field. Any consumer wanting to know "is this session live" calls `LiveProjection::compute(&log, now)`. The TUI, autonomous loop, and any external observer must use this projection rather than maintaining independent aliveness state. Auditor verification: search for runtime structures named `LiveSessions`, `ActiveSet`, `OpenSessions`, etc.; each must be replaced with a `LiveProjection` query or documented as a *cache* of the projection with a stated freshness window.

**INV-LIV-3 — Resumption is structurally identical to creation.**
Reconstituting a session executes the same code path as opening a fresh one: open log (layer-1 descriptor authority), walk capability chain (layer-2 runtime authority), apply current time/count bounds, proceed. There is no `resume_session` distinct from `open_session`; both are `open_session` with different inputs. Auditor verification: only one entry point exists for session establishment.

**INV-LIV-4 — Provider/model identity is per-turn, not per-session.**
Every model-call event carries explicit provider + model fields. Sessions that span migrations record each transition as a `ModelMigrated` event. Auditor verification: no global "session model" field; every `ModelCallStart` (or equivalent) has populated provider/model fields; cross-migration sessions have at least one `ModelMigrated` event.

**INV-LIV-5 — Handoff mode is per-handoff and explicit.**
Every handoff event records its mode (`ParentPointer` or `CopyWithOrigin`). No code path chooses the mode implicitly without recording the choice. Auditor verification: `SessionHandoffStarted` events always carry a `mode` field; no other event variant performs handoff semantics.

**INV-LIV-6 — Bootstrap authority is recorded but not contained.**
The session log records *use* of bootstrap authority via `SessionOpened.bootstrap` field, but the bootstrap principal's authority is established outside the log. Auditor verification: no code path attempts to validate a bootstrap source by re-reading the same log it is authorizing.

## Follow-on work

Concrete beads enabled by this spec:

- **`LiveProjection` implementation** — a follow-on bead (to be filed) for the projection module + tests. Migrates `F1`/`F2`'s current runtime-state-based Live sessions logic to the projection.
- **`ModelMigrated` event variant** — a follow-on bead for the event-log schema extension. Includes per-turn provider/model validation that the variant is correctly emitted.
- **Reconstitution check at session-open** — a follow-on bead to refactor the existing `session_open` path (`mu-coding/src/session/`) to apply the two-layer authority check explicitly, with `SessionResumed` events recording any axis reductions.
- **`mu-d033` (existing bead — /handoff)** — gets its handoff-mode choice resolved by this spec. Mode A vs B encoding is now specified.
- **`mu-ml6m` (existing bead — seams.md)** — adds a **session lifecycle seam** row to the seams table, with this spec as the canonical reference and INV-LIV-1 through INV-LIV-6 as the contract tests.
- **Capsicum migration plan** — when/if mu moves from design 1 to design 2, the migration plan can reference this spec as evidence that the substrate is unchanged.

## Open questions

These are not blockers for this spec landing; they are recorded for future resolution.

- **Event-log immortality**: are session logs ever GC'd, archived to cold storage, or deleted? If yes, mode A (parent pointer) handoffs become fragile across archival; mode B should be the default for handoffs whose lifetime might cross an archival event. If no, mode A is always safe. mu v1 leans toward "logs are immortal" but has not committed.
- **Cross-daemon liveness queries**: `LiveProjection` is local. If a second daemon wants to query liveness of a session on this daemon, that's a cross-daemon RPC (related to mu-hoe0). Out of scope for v1.
- **Multi-principal handoffs**: when bootstrap authority changes hands (operator A hands off a session to operator B), what events record the principal transition? Likely a new event variant (`SessionPrincipalChanged`); not designed in this spec.
- **Handoff atomicity**: in design 2 (per-session capsicum workers), the handoff requires the new worker's process to receive descriptor grants from mu-server. The handoff event lands in the source log; the new worker's first event records the receipt. What happens if mu-server crashes between the two? Recovery procedure is a follow-on operational concern.
- **`LIVENESS_WINDOW` default**: 30s is the recommended default. Operators may want shorter for tight monitoring or longer for noisy sessions. Configurable per-deployment.

## References

- `specs/architecture/event-sourced-context.md` — the event-log substrate this spec extends with lifecycle semantics
- `specs/architecture/capability-delegation.md` — the Capability/Biscuit machinery whose checks this spec mandates be applied against current event-log state
- `specs/architecture/mu-capability-substrate.md` — the capability axes (`allowed_tools`, `expires_at_unix_ms`, `max_tool_calls_remaining`, etc.) whose current state layer-2 reconstitution evaluates
- `specs/architecture/os-enforced-agent-sandboxing.md` — the Capsicum mechanism whose granularity choice is layering-independent of this spec
- Tracking bead: `mu-c7zh`
- Source conversation: claude-personal session 2026-05-20 → 2026-05-21 (operator + claude-opus, after the daemon ebccc9 session-2 multi-agent review framework writeup)
