# Architecture: session records and context orchestration

| field | value |
| --- | --- |
| doc_id | architecture/session-context-orchestration |
| status | proposed architecture; incremental implementation required |
| created | 2026-08-05 |
| authors | tcovert + mu cooperating sessions |
| supersedes | none |
| related | architecture/session-lifecycle, architecture/event-sourced-context, mu-031, mu-032, mu-037, mu-038, mu-040, mu-046 |

## Problem

A model context window is neither the session nor the application state. It is one bounded projection assembled for one semantic decision. Treating the model call as the agent's event loop obscures this distinction and creates several failures:

- context that was compacted out is durable but difficult to retrieve;
- child work either pollutes the parent's prompt or returns as an unstructured blob;
- asynchronous requests and results can bypass the durable event path;
- context assembled from another session lacks a common reference, routing, and authorization contract;
- concurrent background work tempts multiple writers to mutate one context branch;
- models must remember that retrieval exists before they can retrieve anything.

Mu already has the required substrate pieces: append-only session logs, retained ropes, `SessionRef`, mesh `PeerId`, delegated sessions, capabilities, mailboxes, compaction policies, and provider-neutral model calls. This document defines how they fit together. It is an end-state architecture and an incremental migration guide, not a request for a big-bang rewrite.

## Thesis

The daemon is moving toward a deterministic application runtime. At present, mu-046 provides the reference discipline at command ingress, while parts of live loop state remain in-memory and session-log gateway writes are best-effort. The architecture extends that discipline downward incrementally: model inference is one effect the runtime may schedule when semantic judgment is required; it is not the clock driving every event. This does not claim that model outputs are deterministic.

A session's durable record, a model-visible context, and a materialized provider request are different objects:

```text
SessionRecord
    complete append-only event stream for one session

ContextView
    bounded, ordered, queryable projection over one or more SessionRecords

ContextSnapshot
    immutable ContextView at named source watermarks

ProviderRequest
    eager rendering of one ContextSnapshot for one provider call
```

A session is a primary context source, not itself a context. Provider status, receipts, usage, capability transitions, attachments, and other application events belong in the `SessionRecord` but normally not in a `ContextView`.

## Existing substrate and migration posture

This proposal names and connects behavior that is partly present; it must not create parallel machinery where a proven contract already exists:

- `SessionRef` and mesh `PeerId` already provide the logical session namespace and lossless conversion.
- The command journal's `CommandReceived` to exactly-one terminal receipt contract (mu-046) is the reference pattern for all new obligation pairs. New context, worker, dispatch, and adoption obligations should reuse its ticket/idempotency and reducer discipline where possible.
- Session delegation, `spawn_worker`, typed event logs, retained ropes, capability attenuation, strict resume, and mailbox projection are implementation seams to extend rather than replace.
- `ContextSnapshot`, remotely queryable `ContextView`, reconstructable `JoinAll`, and delegated context-read authority are genuinely new substrate.

The first load-bearing correction is the existing N=1 spawn/return path: it lacks a journaled parent-side obligation and durable dead-letter/terminal delivery. `JoinAll` must not amplify that gap. Local snapshot and return correctness therefore precede remote mesh resolution, whose cross-daemon trust and fine-grained read grammar are separate prerequisites.

## Invariants

1. **The event log remains the source of truth.** Any content-bearing state needed after restart, including generated compaction summaries and adopted child findings, is persisted before becoming authoritative in memory.
2. **Every obligation terminates durably.** A request that creates an obligation has exactly one correlated success, failure, cancellation, rejection, or timeout terminal event. Tool calls, context queries, judge calls, dispatch assignments, and child returns follow this rule.
3. **One writer per context branch.** Parallel work occurs in separate sessions. Information crosses branches through explicit publish, inspect, and adopt events, never by concurrent mutation of one rope.
4. **References are logical; offsets are indexes.** Durable references name sessions, events, fields/spans, and ranges. File paths and byte offsets are resolver implementation details.
5. **A reference is not authority.** Every local or remote materialization checks a context-read grant. Action/tool authority is a separate capability axis. Child summaries, findings, and requested grants are untrusted data and never confer authority.
6. **No model starts from nothing.** The runtime deterministically constructs a bounded bootstrap view containing the assignment, obligations, context catalog, query API, and selected initial evidence.
7. **Generated information is an event.** Model-created summaries and child findings are stored as content, not only as IDs or in-memory spans.
8. **Spawn authority is explicit and attenuated.** Delegated workers cannot spawn by default. A coordinator receives bounded dispatch authority intentionally.
9. **Model calls occur at semantic boundaries.** Mesh delivery, authorization, reference resolution, artifact verification, join accounting, and state reduction are deterministic unless a configured policy explicitly requires a model.
10. **Old information remains queryable.** Compaction changes an active view; it does not erase the underlying record.

## Core vocabulary

### `SessionRecord`

The ordered durable events for one `SessionRef`. It contains everything that happened, including information that should never enter a prompt. In-process session state is a projection over this record.

### `ContextSourceRef`

Names a source session. Reuse `mu_core::protocol::SessionRef` as the canonical operator/reference form (`mu:<daemon>/<session>`) and losslessly convert to the existing mesh `mu_peer::PeerId` (`mu:<daemon>:<session>`) for routing. Do not invent another daemon/session grammar.

Within an already established source namespace, event and span references may use a compact relative form. Crossing a session or daemon boundary requires the complete namespace.

### `ContentRef`

A stable logical reference to source content:

```text
ContentRef {
    source: SessionRef,          // elidable only inside a bound namespace
    event_id: u64,
    field: ContentField,
    ranges: [TextRange],         // optional; whole field when empty
}
```

`ContentField` identifies a text-bearing field or generated span within an event. This is a fragment grammar layered on `SessionRef`, not functionality the existing type already provides; whole-session routing and sub-session content addressing remain distinct types. Ranges require a canonical coordinate convention; v1 should use UTF-8 byte offsets because Rust strings and resolver indexes use bytes, reject non-character boundaries, and expose line/character conveniences at the API edge. Adjacent ranges can be normalized; custom compression should follow measurement, not precede it.

Some static inputs (tool schemas, system material, artifacts) are not naturally an event field. They require a content-bearing event or a content-addressed object reference before a snapshot may depend on them durably.

### `ContextViewSpec`

A declarative request for a bounded view:

```text
ContextViewSpec {
    sources: [SourceAtWatermark],
    selected: [ContentRef],
    searchable_scope: Scope,
    ordering: OrderingPolicy,
    budget: ContextBudget,
}
```

A view may reference multiple sessions without merging their ropes. Only selected materialized slices consume provider context; all retain foreign provenance.

### `ContextSnapshot`

The immutable result of resolving a `ContextViewSpec` against explicit source watermarks. It records the complete ordered span/reference manifest plus any generated spans. It is sufficient to re-materialize the model-visible logical context without replaying current policy over a changed log. It extends the open `mu-m7x` `ContextAssembly` v2 source-map work rather than inventing a parallel provenance format.

Snapshot retention should reuse the epoch-capsule/`EpochPinned` concepts in `memory-hierarchy-and-trust.md` where they fit, and preserve the compaction/cache invariant: a snapshot must identify the post-hint, post-compaction rope actually rendered without destabilizing the cacheable prefix.

A per-call `ContextAssembly` may remain compact operational telemetry, but it should reference a snapshot or manifest. `ContextSnapshot` does not make a second mutable rope canonical: it is either a persisted manifest of logical refs/generated content or an identity whose byte-stable manifest is deterministically replayable. The implementation bead must choose and prove one representation before remote resolution depends on it. Persisting a duplicated provider request for every call is optional. A rendered-request digest is useful forensic verification, not the substrate.

### `ContextCatalog`

A small, always-visible map of external information available to a model, for example source sessions, dropped/searchable spans, child returns, artifacts, and prior decisions. It addresses the retrieval bootstrap problem: models cannot query an archive they do not know exists.

### `ContextResolver`

Materializes logical references. The caller sees one API; the resolver chooses:

- local indexed event-log reads;
- a request to the owning daemon over the mesh;
- content-addressed artifact retrieval.

Local event-id-to-byte-offset, timestamp, kind, and text indexes may make resolution cheap, but are disposable projections. The durable reference never contains a file offset.

## Deterministic application loop

The runtime follows a fetch/decode/execute/effect shape:

```text
persist inbound command/event
    -> reduce application state
    -> classify required effects
    -> execute deterministic effects
    -> persist results
    -> invoke a model only when semantic judgment is required
    -> repeat
```

Representative dispatch table:

| Input/state | Runtime action | Model call? |
| --- | --- | --- |
| mesh message | authenticate, persist, decode, reduce | normally no |
| context query | authorize, route, resolve, persist result | no |
| tool request | register obligation, capability-check, dispatch | no |
| tool result completes a turn chain | assemble continuation snapshot | yes |
| child progress | persist/update projection | no |
| child context request | authorize and resolve | usually no |
| child guidance request | create explicit semantic boundary | yes |
| child terminal result | validate, verify artifacts, update join | no |
| join barrier satisfied | build joined return view | yes, coordinator |
| compaction proposed | validate source watermark and output | no |
| compaction policy requires judgment | run correlated sidecar call/session | yes, sidecar |
| operator request | bootstrap/retrieval policy, assemble snapshot | yes |

This table should become explicit reducer/effect code over time. Existing agent-loop behavior migrates behind it incrementally; this spec does not require replacing the loop in one commit.

## Rehydration and bootstrap

"Rehydration" comprises three operations:

1. **Session rehydration:** deterministically rebuild application projections and unresolved obligations from a `SessionRecord`.
2. **Context materialization:** authorize and resolve a `ContextViewSpec` into an immutable `ContextSnapshot`.
3. **Cognitive continuation:** invoke a stateless model with a rendered snapshot. Hidden model state is never restored.

Before a first child or resumed model call, the runtime supplies a deterministic bootstrap view containing:

- identity and operating instructions;
- assignment/current operator request;
- current goal and unresolved obligations;
- parent and source watermarks;
- compact `ContextCatalog`;
- context-query operations;
- bounded initial evidence selected by policy.

Retrieval is both policy- and model-driven. The harness should automatically search when deterministic cues appear (for example, “last week,” “we decided,” a known incident, or a compacted provenance reference). The model can then inspect or widen candidates through the query API.

Temporal narrowing is supported as an optional filter. Relative human language is resolved to an absolute interval in the operator's timezone and that interpretation is recorded. Search may widen an empty/weak interval explicitly. Time filters candidate sources; semantic/lexical ranking still selects within them.

## Mesh context resolution

A child assignment may carry a `ContextViewSpec`, not the bodies of all referenced messages:

```text
ChildAssignment {
    assignment_id,
    assignment,
    context_view_spec,
    context_read_grant,
    action_capability,
}
```

The receiving daemon:

1. persists the assignment;
2. authenticates the sender and verifies lineage;
3. attenuates action capabilities;
4. validates the context-read grant;
5. resolves initial references locally or through owning daemons;
6. persists the resulting bootstrap snapshot;
7. invokes the model.

A foreign resolution request names a `SessionRef`, source watermark, logical refs/query, request ID, and proof/grant. The source daemon authorizes against its record and returns either correlated content/hits or a terminal denial/failure. The mesh transports requests; the owner remains the authority over source material.

Parent and child may maintain a durable dialogue. Progress, context requests, guidance requests, and completion are typed protocol messages rather than strings requiring semantic classification.

## Context-read capability

Context authority and action authority are separate:

```text
ContextReadGrant {
    sources,
    max_event_id_by_source,
    allowed_event_kinds,
    allowed_fields_or_artifacts,
    maximum_materialized_bytes_or_tokens,
    expiration,
}
```

Delegation intersects each axis. A child cannot substitute another `SessionRef`, read events appended past its watermark, or request a broader sensitive field merely because the source daemon can access it.

The exact token/certificate representation must follow `capability-event-log-trust.md`: ordinary JSONL capability fields are untrusted data and cannot bootstrap authority. Durable or cross-daemon authority requires self-authenticating/anchored proof (the existing Biscuit and mesh trust direction); local enforcement may ship first against the live in-process capability. Cross-daemon resolution reuses the decided typed NATS mesh envelope/reply and mu-046 Router ingest seams rather than introducing a side transport. The semantic checks and denial events must exist before remote materialization ships.

## Context queries as obligations

Context discovery and reads use paired durable events:

```text
ContextQueryRequested { query_id, spec, requester }
ContextQueryCompleted { query_id, hits, resolved_watermarks }
ContextQueryFailed | ContextQueryDenied | ContextQueryCancelled | ContextQueryTimedOut
```

A continuation model call cannot be assembled with an open context-query obligation it depends upon. This is the same structural invariant that requires every assistant tool call to receive a matching tool result.

## Compaction as a context transformation

Compaction transforms one snapshot into another; it does not mutate unrecorded memory.

For heuristic drop:

- record policy ID/version and effective config;
- record complete input watermark/snapshot identity;
- record keep/drop decisions;
- persist the complete output manifest or a deterministic delta from the input.

For model-judged compaction:

- the judge may use a different provider/model (Haiku, Ollama, or another route);
- record a correlated judge request and terminal response/failure;
- persist the raw or canonical parsed result according to retention policy;
- persist the generated summary as content in its own event/generated span;
- record absorbed refs and the complete resulting manifest;
- adopt only if the source snapshot still matches or an explicit rebase succeeds.

The current `CompactionAssembly::Summarized` ID-only record is insufficient for recovery if summary content exists only in the in-memory rope. Closing that durability gap is an early prerequisite for broader context orchestration.

A compactor can later be implemented as a specialized sidecar session, giving it route choice, accounting, cancellation, and paired events through the common runtime. Proposal and adoption remain separate: stale background results may be rejected without changing the active view.

## Coordinator and worker hierarchy

Spawn authority is a capability, not an inherited agent property.

```text
operator session
    -> coordinator (explicit bounded dispatch grant)
        -> workers (no dispatch authority by default)
```

A coordinator grant should bound at least depth, total/concurrent children, allowed roles/routes, spend/time, and the child capability ceiling. An ordinary worker may publish `AdditionalWorkerRequested`; it does not spawn unless authority is expanded or the coordinator performs the spawn.

Background coordination runs in a separate session branched from an operator-session snapshot. This preserves one writer per branch and keeps the operator-facing context responsive and free of worker traffic. Status can be displayed without entering the operator model's context.

## Initial dispatch pattern: `JoinAll`

The first orchestration policy is an explicit barrier over the existing per-child streaming completion primitive, not a timing batch and not a replacement for streaming delivery:

```text
DispatchStarted { dispatch_id, assignments, pattern: JoinAll, deadline }
Assignment terminal events arrive independently
DispatchJoined { dispatch_id, terminal return refs in assignment order }
coordinator model wakes once
```

Terminal assignment states are success/result published, failure, cancellation, or timeout. Progress, context requests, and guidance requests do not satisfy the barrier. Guidance may wake the coordinator as an explicit exception; deterministic context requests do not.

The join projection is reconstructable from events and idempotent. Individual terminal results are persisted and remain inspectable as they arrive; the `JoinAll` policy merely suppresses the routine coordinator-model wake until the barrier opens. Duplicate terminal delivery cannot satisfy an assignment twice. Daemon restart resumes the barrier from the log. A deadline is deterministic input to timeout generation, not an unrecorded timer assumption. Guidance requests and operator intervention may wake early by explicit policy; a failed or cancelled barrier can degrade to the available terminal set rather than losing it.

`AsCompleted`, quorum, first-success, and optional-worker patterns are later extensions, not v1 requirements.

## Structured child returns

A child publishes a queryable return package rather than sending its transcript:

```text
ChildReturn {
    assignment_id,
    child_session,
    child_watermark,
    based_on_parent_snapshot,
    status: completed | partial | blocked | failed,
    summary,
    findings: [{ finding_id, claim, confidence?, evidence_refs }],
    artifacts: [{ artifact_ref, digest, description }],
    unresolved_questions,
    suggested_followups,
}
```

Confidence is advisory model output, not authority. Evidence refs and artifact digests are mechanically validated where possible.

Return lifecycle states are distinct:

- **available:** durable terminal return exists;
- **inspected:** selected details were temporarily materialized for the parent/coordinator;
- **adopted:** selected content was persisted into the receiving session with provenance;
- **active:** adopted content is currently projected into a model-visible view.

Completion does not paste content into the parent rope. Under `JoinAll`, the coordinator initially sees compact cards for all terminal outcomes and can inspect evidence, request a follow-up, or adopt selected pieces.

Adoption records source child/watermark, selected return fields, copied/generated parent spans, evidence refs, and the source capability/provenance relevant to interpreting the information. Adoption is information flow only: it never imports or widens the child's authority, and the child return is untrusted input to the parent. This makes the receiving session self-contained for adopted information while retaining lineage. Later compaction may remove adopted spans from the active view without erasing the adoption event.

## Child lifecycle and follow-up

Distinguish:

- `AssignmentCompleted`: a terminal return was published;
- idle/no active effects: follow-ups can be accepted;
- `ChildReaped`: runtime resources are released, durable record remains.

After reaping, evidence remains resolvable. A follow-up may attach a fresh live head to the child record or create a new child assignment linked to the prior return. Reaping is not deletion.

Every return states the parent snapshot it used. If the parent advanced, the runtime marks the result stale relative to the current watermark; the receiver may adopt, inspect changes, request revalidation, or reject. Context-transform results must never silently replace newer state.

## Model-facing API

Exact tool names are deferred, but the semantic API should be stable:

```text
session.describe / search / read / timeline / artifacts / children / context_at
context.describe / active / search / read / origin / diff / branch
child_return.list / inspect / read_evidence / request_followup / adopt / defer
```

The API is typed and provider-neutral. Models should not repeatedly invent Python or shell archaeology to query context. Search results are small namespaced references/excerpts; surrounding material is fetched only when selected.

## Recovery and consistency

Reducers must reconstruct:

- unresolved context/tool/judge obligations;
- dispatch membership and terminal state;
- available and adopted child returns;
- source watermarks and stale-result status;
- current context snapshot identity.

Effects require stable idempotency keys. On recovery, the runtime may redeliver a request or terminal result, but reducers cannot double-apply it. A durable terminal event precedes any lossy wake/notification; a wake is only a hint.

Snapshot reads use source watermarks so append-only growth does not change their meaning. Cross-daemon responses echo resolved watermarks and content digests. If a source is unavailable, resolution fails explicitly; it never silently substitutes current or partial context.

## Implementation beads

The dependency-ordered rollout is tracked by epic `mu-session-context-orchestration-rurq`, with children `.1` through `.18` corresponding to the staged work below. The tracker dependencies, not the numeric suffix alone, are authoritative; `beads dep tree mu-session-context-orchestration-rurq` shows the live graph.

## Incremental delivery plan

Each stage below should land as a coherent commit/bead with tests. Later stages depend on earlier contracts rather than modifying everything at once.

1. **Add the `SessionRecord` projection.** Extend existing mu-038 folds with a pure record projection for identity, provider/model, status, cursor/watermark, lineage, child links, and activity; do not smuggle untrusted durable authority into it.
2. **Close compaction durability gap.** Persist generated summary content and a complete post-compaction manifest/delta; prove recovery recreates the post-compaction rope.
3. **Introduce logical context reference types.** `ContentRef`, ranges, source watermarks, normalization, and `SessionRef`/`PeerId` reuse; align the namespace with open handoff bead `mu-x9j`; serde and boundary tests only.
4. **Add local event indexes and resolver.** Resolve logical refs from local logs without full scans; indexes remain rebuildable projections.
5. **Define context snapshots/manifests.** Extend `mu-m7x`: build ordered immutable snapshots locally, reuse appropriate epoch-pinned retention semantics, and make `ContextAssembly` reference them; add round-trip/replay verification.
6. **Add context-read capability axis.** Attenuation, source/watermark/range checks, structured denials, and tests; local in-process enforcement first. Do not treat JSONL data as durable authority; persistence waits for the trust substrate in `capability-event-log-trust.md`.
7. **Add paired local context-query events/API.** Search/read/describe requests and terminal outcomes through the application event path.
8. **Add mesh context resolution.** Route via `SessionRef`/`PeerId`, authenticate, authorize at owner, correlate replies, and test cross-daemon denial/unavailability.
9. **Add child bootstrap view specification.** Delegate with bounded selected refs, searchable scope, and deterministic bootstrap catalog/snapshot.
10. **Add structured child return protocol and repair N=1 delivery.** Journal a parent-side spawn assignment obligation; publish/validate/query return manifests and artifacts; route success, failure, timeout, and cancellation through one durable terminal/dead-letter path before any wake.
11. **Add selective adoption.** Inspect and copy selected return content into receiver record with provenance; stale-watermark handling and recovery tests.
12. **Add bounded dispatch capability and typed child messages.** Add spawn/depth/total/concurrency ceilings to the attenuate-only capability algebra and enforce them in both native delegation and subprocess-worker paths; workers default to no spawn; progress/context/guidance/terminal messages are distinct.
13. **Add reconstructable `JoinAll`.** Build on the proven N=1 obligation path with durable dispatch/assignment/barrier events, deadlines, duplicate handling, restart recovery, and one coordinator wake.
14. **Add background coordinator sessions.** Branch from operator snapshot, run join/follow-up rounds independently, and publish compact updates/final returns without mutating operator context.
15. **Extract deterministic runtime reducer/effect boundaries.** Move existing model/tool/mesh/context handling behind explicit state transitions incrementally, preserving behavior with state-machine/property tests.
16. **Add retrieval policy and temporal/federated search.** Always-visible catalog, cue-triggered retrieval, recorded timezone range interpretation, widening, and multi-session ranked references.
17. **Unify model-judged compaction with sidecar sessions.** Use common request/result/accounting/cancellation and proposal/adoption contracts; evaluate candidate policies against fixed snapshots.

The order deliberately delivers recovery and reference correctness before remote orchestration. `JoinAll` depends on structured terminal returns; background coordination depends on both. The runtime extraction can begin in parallel once event contracts are stable, but no stage should require a flag-day replacement of the current agent loop.

## Verification strategy

- serialization and forward/backward compatibility tests for every event/protocol type;
- reducer replay tests from JSONL, including duplicate delivery and truncated process lifetime;
- property tests asserting one terminal event per obligation and one satisfaction per join assignment;
- local/remote resolver equivalence for the same logical reference;
- negative capability tests for substituted sessions, widened ranges, expired grants, and post-watermark reads;
- compaction crash/recovery tests proving generated summaries survive;
- cross-daemon integration tests with source unavailable, delayed, denied, and duplicated replies;
- parent-advances-while-child-runs stale-result tests;
- operator/coordinator concurrency tests proving separate session writers and no operator-rope contamination;
- fixed-snapshot benchmarks comparing retrieval and compaction policies by downstream task outcomes, cost, and latency rather than exact model text.

## Non-goals for the initial implementation

- unrestricted recursive worker spawning;
- arbitrary recursion depth;
- concurrent model writers in one session;
- storing every rendered provider request body;
- custom compression before reference-size measurements;
- automatic semantic merging of entire session ropes;
- treating model confidence as verification;
- hard-coding one provider/model for compaction or coordination;
- implementing every future dispatch pattern before `JoinAll` is proven.

## Open decisions

These should be resolved by the corresponding implementation beads with terrain and measurements:

- whether generated/static content lives inline in events or in a content-addressed store;
- event-envelope/schema-version work needed before adding many new variants;
- exact authentication token used for cross-daemon context-read grants;
- retention and privacy policy for raw compaction judge prompts/responses;
- whether snapshot manifests are inline, delta-encoded, or content-addressed after measuring typical sizes;
- how long completed child runtimes remain idle before reaping;
- which deterministic cues trigger retrieval and how candidate injection is budgeted;
- how build provenance identifies development binaries independently of session semantics.
