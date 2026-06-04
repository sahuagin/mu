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

## Implementation slices (ordered)

1. **agent.sqlite schema + CLI** (one session, agent_tools): columns
   `verified_at`, `orphaned`, `source_ref`; table
   `memory_supersedes(old_id, new_id, reason, created_at)`; `agent memory
   search` self-labels every hit and never shows a masked fact without
   its successor; `agent memory correct OLD --with NEW` writes the edge.
   Fixes the store where the incidents happened, this week.
2. **Retention auditor** (cron or session-start hook): resolve
   provenance refs; mark orphans. Cheap, mechanical.
3. **mu L0–L3 + consolidator** (mu-jsde → mu-5xbp): EventLogView range
   queries, then the continuous consolidator with fact-identity
   resolution and tombstone writing. The model-in-the-loop part.
4. **Injection rework** (mu-42x8 experiment): identity tier extraction,
   tail-injection via triggered recall (mu-8puo), measured by the
   per-section token breakdowns shipped in PR #161.

## Cross-references

- Beads: mu-5xbp (consolidator), mu-jsde (EventLogView), mu-42x8
  (tiering experiment), mu-8puo (triggered recall), mu-68u5
  (context.list/rehydrate), mu-tlri (pin stable prefix), mu-wsgx
  (trigger calibration — the feedback-predictor pattern is the same
  trust-the-terrain discipline applied to token counts).
- Memory: `36a2866b` (recall-is-testimony), `42577731` (usage-accounting
  traps), `dd7eb13d` (2026-05-30 design session, updated).
- The archival incident record: `~/src/career_book/transcript-archive/INDEX.md`.
