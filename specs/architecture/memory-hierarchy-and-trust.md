# Memory hierarchy and trust

Status: design. Consolidates the 2026-06-03/04 operator+claude design
conversation (threaded across beads mu-5xbp, mu-42x8, mu-8puo, mu-68u5 and
agent memory `36a2866b`). Spans two stores: `agent.sqlite` (the shared
cross-account store, where the motivating incidents happened) and mu's
event-log-native L0–L3 hierarchy (mu-jsde / mu-5xbp lineage).

## Motivating incidents (one week, three failures, one disease)

1. **The jail belief** (2026-05-31/06-01): recall returned a stale fact;
   the session took the first hit as ground truth and ran with it —
   including a month-long false belief that rust work ran inside jails,
   whose *correction had been uttered* in a later session but landed as a
   new coexisting memory that never masked the stale one.
2. **The Linux-ELF diagnosis**: a session concluded claude-code "can't
   work on FreeBSD — it's a Linux ELF!" while executing inside that
   working binary. Memory plus overconfidence, no terrain check.
3. **The war-story purge** (2026-06-03): compressed memory blurbs (L3)
   survived while their source sessions (L0) were garbage-collected by an
   independent retention process that didn't know the references existed.
   The summaries silently became the only tier.

One disease: **memories present as facts when they are testimony** — and
the store has no vocabulary for the ways testimony degrades.

## Trust vocabulary

Every recall hit carries, and every consumer displays:

| Label | Meaning | Set by |
|---|---|---|
| provenance | source session/event (`daemon:session:event_seq` or transcript path) | writer |
| `recorded_at` / `verified_at` | when written; when last terrain-checked | writer / any verifier |
| **superseded** (tombstone) | a newer fact masks this one (`supersedes` edge) | consolidator or explicit correction |
| **orphaned** | provenance no longer resolvable — testimony that can never be terrain-checked again | retention auditor |

Tombstone = *superseded*. Orphan = *unverifiable*. Both are trust
downgrades the current store cannot express; both were observed in
production the same week.

Read rule: **a superseded fact is never returned without its successor.**
Standing prompt rule (identity tier): *recall results are testimony with
dates, not ground truth — terrain-check before consequential action.*

## Hierarchy: LSM semantics, continuous consolidation

The L0–L3 design (2026-05-30 session) is an LSM tree and should adopt its
discipline explicitly:

- **Levels** = compaction depth: L0 the raw event log (full fidelity),
  L1–L3 increasingly dense summaries. Recall weight ~ `1/(depth+1)`.
- **Compaction runs continuously**, like RocksDB background threads —
  not as offline "dreaming." Triggers: write volume, detected
  contradiction, recall-miss feedback. ("Why wait?")
- **Tombstones**: the consolidator writes `supersedes` edges when it
  detects contradiction or correction. Hard part: facts have no primary
  key — *fact identity resolution is the consolidator's job* (cluster by
  entity/subject, judge same-fact). This is the expensive cognition that
  justifies a model in the loop.
- **Referential retention**: nothing GC-able while referenced from a
  higher tier (ZFS-snapshot / git-reachability semantics). Two rules:
  1. No summary without resolvable pointers to its sources.
  2. Retention honors cross-tier reference counts; if a source vanishes
     anyway (external deletion), mark dependents **orphaned** rather than
     pretending. The war-story purge is the canonical violation: an
     external GC (claude-code's 30-day cleanup) deleted L0 with no
     knowledge of the L3 blurbs pointing at it.

## Injection economics: small kernel, discoverable tail

Baseline measured 2026-06-03 (session `c76f6949`): 15,890 tokens of
standing memory injection = 21% of post-compaction context; the session's
own assessment: "irrelevant wallowing."

- **Identity tier** (always inject, target 600–800 tokens): processing
  style, anti-sycophancy/anti-fabrication calibration, humor register,
  pointer-not-payload sensibility references, and the standing recall
  rule above. Tier, not topic: these are universal; topic classifiers
  break on session drift.
- **Everything else recall-only**, surfaced through the same discovery
  interface as tools/skills (t4c phase-3 direction): one `discover`
  surface, three corpora.
- **Dynamic injections land at the tail as Warm spans** — never appended
  to a pinned front-of-rope monolith. Three wins at once: zero prefix-
  cache invalidation (the tail is uncached anyway), recency-position
  attention, and eviction-by-construction (they age out under normal
  compaction; re-injectable on demand). Cache math from the same session:
  keeping ~6K of stable front-of-rope content cached costs ≈$0.04 for a
  session's remainder; evicting it cost ≈$0.83 in re-cache. Stable
  content stays pinned at the prefix; volatile content lives at the tail;
  compaction drops from the tail region only (see mu-tlri).

## Working-set prefetch: context planning, not inject-everything

Reviewing [TencentDB Agent Memory's v1 task
canvas](https://github.com/TencentCloud/TencentDB-Agent-Memory/tree/v1.0.1) in
2026-08 produced an independent convergence and one useful refinement. Its
short-term subsystem does not retrieve tool history through SQLite similarity
search. It continuously folds tool-call/result pairs into a small,
model-authored task graph; each graph node maps back through JSONL entries to
the full raw result. The representation is Mermaid, but the durable property is
**a compact task-state map with a resolvable road back to evidence**. (Its edges
are inferred workflow relations, not a formal DAG or a substitute for event
causality.)

Mu already has the stronger substrate for this: the event log retains the full
history, context is a projection, and compaction removes spans from the working
rope without deleting their source events. The refinement is to treat context
assembly as **working-set management with recoverable misses**:

- the active prompt is the hot working set;
- compact task/project maps are the page table;
- source events and evicted spans are backing storage;
- rehydrating an evidence ID is a context page fault;
- a cheap local model may speculatively prefetch likely-needed cold evidence for
  an expensive model.

This does **not** replace mu's continuous background compaction. The current
online policy — retain recent causal continuity, demote lower-value spans before
the hard limit, and keep the session moving — remains the normal path. Prefetch
and evidence rehydration make demotion reversible when an older dependency
becomes relevant again. They avoid both extremes: hoarding all history until a
cliff compaction and rebuilding the entire prompt on every turn.

### Two retrieval problems, not one

Keep these separate because they have different keys and failure modes:

1. **Active-task cold history.** Session/task identity and event lineage already
   define the search domain. Build a compact projection of goal, established
   findings, failed paths, unresolved questions, recent changes, and evidence
   IDs. No semantic search is required to decide which task the evidence belongs
   to; model judgment is only needed to compress or infer relations. Known causal
   edges from the event log must remain distinguishable from inferred edges.
2. **Cross-session durable memory.** This still requires lexical/semantic recall
   and a relevance decision. Giving the whole current user turn to FTS/vector
   search and injecting the top hits every turn merely recreates the failed
   inject-everything experiment at a smaller scale. Giving the model a memory
   tool also does not help when the model treats it as a write-only sink and
   never queries it.

The circularity is real: the best query depends on understanding the task, while
understanding the task may depend on the missing evidence. The proposed escape
is coarse-to-fine planning, not pretending the circle is solved. A cheap local
planner reads the identity kernel plus compact task/project maps, identifies
entities, operations, constraints, and unresolved questions, retrieves candidate
evidence summaries, and emits a bounded context capsule. Its token ceiling is
explicit rather than qualitative:

```text
capsule_budget = min(configured_capsule_max,
                     soft_limit - stable_prefix_estimate - frontier_reserve)
```

`frontier_reserve` must be positive, and prefetch is disabled when the remainder
is non-positive. The experiment chooses `configured_capsule_max`; no result may
exceed the computed budget. The planner need not solve the main task; like a
prefetcher or database query planner, it predicts the expensive model's likely
working set. False negatives remain recoverable through evidence IDs. False
positives consume attention and therefore remain measurable failures, not
harmless extras.

### Context epochs and prompt caching

Reassembling a provider request from projections does not itself invalidate a
prompt cache; changed bytes or ordering do. Organize frontier work into context
epochs:

```text
stable identity / project instructions / tool schemas   (long-lived prefix)
epoch-initialization task capsule                        (frozen this epoch)
frontier conversation and tool loop                     (append-only tail)
```

An epoch capsule is not a Warm recall injection into an existing conversation.
It initializes a new context projection, is immutable for that epoch, and is
replaced exactly once when the next epoch begins. Its implementation requires an
explicit epoch-scoped stable retention class (`EpochPinned`, or equivalent),
with `cacheable = true`: protected from ordinary within-epoch compaction, but
retired when its `epoch_id` is replaced. Do not overload Warm (which would break
the contiguous cacheable prefix) or unscoped Pinned (which has no retirement
lifecycle). Exactly one capsule may exist in an epoch.

The prior frontier tail remains in the event log but is not carried verbatim
after replacement; the new capsule must preserve whatever task state the next
epoch needs. A new epoch therefore intentionally forfeits the old
capsule-and-tail cache while retaining any cache hit through the stable prefix.
This is a boundary cost to amortize across the following tool loop, not a claim
of zero invalidation. Context pressure is also an epoch-boundary signal: if
ordinary tail compaction cannot get below the soft limit while preserving the
capsule and positive frontier reserve, retire the epoch and rebuild a smaller
capsule under the budget formula rather than accumulating capsules or crossing
the hard limit.

The earlier tail-injection rule still governs recall during an epoch: newly
rehydrated evidence lands as a Warm span at the append-only tail, gets recency
attention, and remains eviction-by-construction. A material miss may either be
served by that ordinary tail injection or, when it changes the task's working
set enough to justify paying the boundary cost, end the epoch and build a new
capsule. Do not rewrite a capsule in place, append multiple capsules, or prepend
a different recall block on every turn.

### Experimental integration surface

The first experiment should be a new selectable `CompactionPolicy`, not a
replacement for the effective heuristic default. A `working-set` policy can run
at the existing soft-limit trigger, use a cheap/local asynchronous judge to
produce the task capsule and evidence handles from the current `RetainedRope`,
and return the ordinary `CompactionResult` audit/metrics. Configuration can then
compare `heuristic`, `hash-and-summary`, `working-set`, and `no-compaction` on the
same sessions and thresholds (mu-working-set-compaction-policy-9dpq).

Do not force the whole design through that trait. `CompactionPolicy::compact`
currently receives only `&RetainedRope` and a token target; it neither queries
cold event history nor runs at task boundaries when the rope is below pressure.
Keep a separate `ContextPlanner`/evidence-resolver surface over `EventLogView`
and the mu-or85 rehydration handles. The compaction policy may consume that
surface later, but v1 can test whether a task capsule preserves continuity using
only the hot rope. This separates a measurable compaction algorithm from the
still-unsolved general recall trigger.

### The unresolved part is the trigger

This refinement supplies a substrate and recovery path, not a general relevance
oracle. Narrow decision-point triggers such as mu-8puo work because the tool name
and action classify the need mechanically. General memory recall has no trigger
of comparable quality yet.

Candidate signals to test include task/epoch transitions, references to an
entity whose spans are cold, unresolved evidence IDs, explicit model requests,
repeated reads/questions, and compaction demotion of spans linked from the active
task map. None should become standing auto-injection without measurement. Track
which prefetched evidence the expensive model actually reads or cites, which
page faults follow a miss, token/cache cost, latency, and task outcome. The
trigger/query experiment is successful only if it improves the expensive
model's effective context, not merely recall cosine scores.

Privacy is also architectural here: the planner sees broad history precisely so
it can minimize the frontier model's capsule. It should default to a local model;
sending raw history to a cloud model for memory extraction defeats that boundary.

## Recall scoring

`score = f(semantic, recency, verified_at, tier_depth, orphan_penalty)`
with **three static weight profiles** chosen by the caller (or a cheap
trigger heuristic — mu-8puo's action verbs), not a per-query model call:

- **operational** ("how do I push this repo"): verified + recent
  dominate; orphans heavily penalized.
- **narrative** (war stories, history): provenance-rich originals beat
  summaries; recency nearly irrelevant.
- **identity** (working style, preferences): tier dominates; stability is
  the point.

Better-is-the-enemy-of-good clause: profiles are static until evidence
shows they misroute; no intent classifier in v1.

## Does a database already do this? (survey, 2026-06)

Verdict first: **no product ships the whole shape, and the part none ship
(model-judged consolidation) is ours regardless. The storage substrate
that fits this stack is SQLite + FTS5 + sqlite-vec + a thin
schema — the trust semantics are ~4 columns and an edge table, not a
database engine.**

| Candidate | What it genuinely gives | Why not the substrate |
|---|---|---|
| Datomic | Assertion/retraction (tombstones!), as-of time travel, full audit | JVM, server-shaped, no vector search; retraction ≠ supersession-with-successor |
| XTDB | Bitemporal facts, schemaless docs, SQL in v2 | JVM/Clojure heft; vectors immature; same gap on successor edges |
| Dolt | Git-for-SQL: versioned tables, diffs, branches | Versions *tables*, not *facts*; MySQL server footprint; FreeBSD support thin |
| TerminusDB / immudb | Versioned graph / immutable+cryptographic log | Same shape mismatch: history ≠ supersession; operational heft |
| Postgres + pgvector | Mature hybrid search, FKs for referential integrity | A daemon where a file should be; the team's database-aversion is earned |
| **SQLite + FTS5 + sqlite-vec** | Already deployed (agent.sqlite has FTS5); sqlite-vec adds vectors in-process; edges/labels are plain tables | Brings none of the semantics — but neither does anything else; here they're a migration, not an adoption |

The temporal databases are the closest *conceptual* relatives — they
prove the assertion/retraction/as-of model works — but they version
*time*, and what we need versions *belief* (`supersedes` is a judgment,
not a timestamp). Their lesson, minus their JVMs: never delete, only
mask; always answer "as of when, said by whom."

## Implementation state and experiment sequence

1. **Shipped trust substrate** (agent_tools): `verified_at`, `orphaned`,
   `source_ref`, supersession edges, labeled recall, and explicit correction.
2. **Shipped measurement and narrow-trigger groundwork**: per-section injection
   measurements in PR #161 and action-time tail recall (mu-8puo) in PR #173.
   The broader offline tiering experiment mu-42x8 remains open; narrow mechanical
   triggers are evidence, not a solved general relevance oracle.
3. **Open retention auditor**: resolve provenance refs and mark orphans. Cheap,
   mechanical, and independent of model-planned context.
4. **Open event-query and consolidation substrate** (mu-jsde → mu-5xbp):
   `EventLogView` range queries, then continuous consolidation with fact-identity
   resolution and tombstone writing.
5. **Open reversible evidence path** (mu-or85 / mu-or85.2): elide cold spans to
   evidence handles and append verbatim source content on `context_recall`.
   Mu-68u5 remains the separately scoped passive `context.list()` introspection
   experiment; it is not the task-state-map implementation.
6. **Proposed working-set policy experiment**
   (mu-working-set-compaction-policy-9dpq, design refinement
   mu-context-working-set-prefetch-8632): first compare a task capsule built from
   the current rope against existing policies, then compose EventLogView and the
   reversible evidence path for cold-history misses and non-pressure triggers.

## Cross-references

- Beads: mu-5xbp (consolidator), mu-jsde (EventLogView), mu-42x8
  (tiering experiment), mu-8puo (triggered recall), mu-68u5 (passive
  `context.list()`), mu-or85 / mu-or85.2 (elision + verbatim rehydration),
  mu-working-set-compaction-policy-9dpq (selectable policy experiment),
  mu-context-working-set-prefetch-8632 (this refinement), mu-tlri (pin stable
  prefix), mu-wsgx
  (trigger calibration — the feedback-predictor pattern is the same
  trust-the-terrain discipline applied to token counts).
- Memory: `36a2866b` (recall-is-testimony), `42577731` (usage-accounting
  traps), `dd7eb13d` (2026-05-30 design session, updated).
- The archival incident record: `~/src/career_book/transcript-archive/INDEX.md`.
