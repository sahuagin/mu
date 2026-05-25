Below is an initial skill/proposal package. I’d split it into two layers:

1. **Review skills** — reusable agent behaviors for codebase/architecture
   reviews.
2. **code-index epic** — implementation work to make `code-index` support those
   reviews better.
3. **code-index usage skill** — teaches agents how to use the current/enhanced
   tool without falling into generic review slop.

I would not make one giant “review project” skill. The review has separable
workstreams, and those map well to parallel agents.

---

# Proposed skill set: architecture review suite

## Skill 1: `evidence-disciplined-review`

### Purpose

Forbid “be careful” / “could happen” review filler unless tied to observed
project evidence.

### When to use

- Any codebase review.
- Any architecture critique.
- Any “what should we work on next?” analysis.
- Any time a model might produce generic scope/complexity warnings.

### Draft `SKILL.md`

```yaml
---
name: evidence-disciplined-review
description: >-
  Evidence discipline for architecture/codebase reviews. Forces every material
  critique, risk, and recommendation to be labeled as observed, inferred,
  speculative, or unknown, with evidence and falsifier. Prevents generic
  "be careful" review filler.
when_to_use: >-
  Use for any project/codebase/architecture review, especially when asked to
  assess focus, ambition, risk, roadmap, or quality. Also use when reviewing
  other agents' reviews for unsupported claims.
status: draft
---
```

### Draft body

```markdown
# Evidence-Disciplined Review

## Rule

Every material claim must be labeled:

- **Observed** — directly supported by code, tests, docs, commits, beads, logs,
  or command output.
- **Inferred** — not directly stated, but strongly supported by multiple local
  signals. Name the chain of inference.
- **Speculative** — possible, but not evidenced locally. Do not turn this into
  a recommendation unless the operator explicitly asks for brainstormed risks.
- **Unknown** — the available evidence does not answer the question.

Do not present speculative concerns as recommendations.

## Required format for critiques

Each critique must include:

```text
Claim:
Label: Observed | Inferred | Speculative | Unknown
Evidence:
Falsifier:
Severity: blocker | important | minor | informational
Concrete next step:
```

## Forbidden generic cautions

Do not write these unless backed by project-specific evidence:

- "lack of focus"
- "too ambitious"
- "scope risk"
- "avoid overengineering"
- "prioritize carefully"
- "watch complexity"
- "ensure tests"
- "maintain discipline"

If the statement would be true of any ambitious project, put it in a
"Generic possibilities — not evidenced here" section and do not make it a
recommendation.

## Before making sequencing/focus claims

You must inspect:

1. First commit or earliest available architecture artifact.
2. Recent commit history.
3. Current open work.
4. Recently closed work.
5. Handoff/current-state docs if present.

If you do not inspect chronology, do not make chronology claims.

## Output requirement

Include a short section:

```markdown
## Claims I am not making

- <generic concern I considered but did not find evidence for>
```

This prevents the review from smuggling unevidenced worries into tone.
```

---

## Skill 2: `architecture-chronology-review`

### Purpose

Determine whether a project’s current architecture is original DNA,
emergent evolution, drift, or retrofit.

### When to use

- Questions about focus, ambition, architecture coherence.
- “Did this project start with the right core?”
- “Did this evolve cleanly or accrete randomly?”
- Before roadmap critiques.

### Draft `SKILL.md`

```yaml
---
name: architecture-chronology-review
description: >-
  Reconstructs a project's architecture over time: first commit, early docs,
  recent commits, closed/open work, and handoffs. Used before making claims
  about focus, drift, sequencing, ambition, or roadmap quality.
when_to_use: >-
  Use when reviewing a project architecture, especially before claiming that a
  project is too ambitious, unfocused, well-sequenced, or drifting from its
  original design.
status: draft
---
```

### Draft body

```markdown
# Architecture Chronology Review

## Goal

Review architecture as a timeline, not a snapshot.

A snapshot can make coherent projects look broad and broad projects look
coherent. Chronology distinguishes:

- original architectural DNA;
- deliberate evolution;
- implementation filling known seams;
- accidental drift;
- speculative bolt-ons.

## Required investigation

From the repository root:

```sh
git rev-list --max-parents=0 HEAD
git show <first-commit> --stat
git show <first-commit>:README.md 2>/dev/null || true
git show <first-commit>:AGENTS.md 2>/dev/null || true
jj log -r '::@' --limit 40 --no-graph
br list --status closed --limit 30 2>/dev/null || true
br list --status open --limit 40 2>/dev/null || true
ls -lt HANDOFF* MORNING* 2>/dev/null | head
```

Use `jj` preferentially in jj-managed repos, but `git show <commit>:path` is
acceptable for reading historical files.

## Output sections

### 1. First-commit architecture

- What seams existed from commit one?
- What crate/module/process boundaries were already named?
- What was explicitly future/planned?

### 2. Architectural evolution

- What major ideas appeared later?
- Did they fit inside the original seams or replace them?
- Which ideas deepened the architecture?

### 3. Recent trajectory

Summarize recent commits as a dependency chain, not just a list.

Example:

```text
provider projection cutover
→ recall reaches providers
→ diagnostics/visibility
→ daily-driver UI
→ dogfood UX fixes
```

### 4. Current work

Classify open work:

- direct continuation of current arc;
- substrate completion;
- projection/UI over existing substrate;
- independent subsystem;
- stale/historical.

### 5. Verdict

Choose one. Do not hedge without evidence.

- Original architecture preserved and deepened.
- Architecture evolved coherently from early seams.
- Architecture drifted from original seams.
- Snapshot insufficient; chronology incomplete.
```

---

## Skill 3: `substrate-compression-review`

### Purpose

Evaluate whether broad feature surface is actually many independent systems,
or many cheap projections over a small substrate.

### When to use

- Projects with event logs, projections, shared runtime cores.
- “Is this too ambitious?”
- “Is this architecture paying off?”
- Reviews of mu-like systems.

### Draft `SKILL.md`

```yaml
---
name: substrate-compression-review
description: >-
  Evaluates architectural compression: whether many features are independent
  subsystems or projections/extensions of a small core substrate. Especially
  useful for event-sourced systems where audit, replay, metrics, compaction,
  and UI separation may be cheap consequences of the substrate.
when_to_use: >-
  Use when a project appears broad or ambitious but may have a unifying
  substrate. Use before making scope/ambition critiques.
status: draft
---
```

### Draft body

```markdown
# Substrate Compression Review

## Core question

Do ambitious features become cheaper because of the substrate?

Do not count features. Classify them.

## Step 1 — identify substrate primitives

List the minimal core primitives that explain the rest.

Examples:

- typed event log;
- projection/materialization;
- retained context rope;
- provider renderer;
- capability policy;
- protocol boundary;
- durable telemetry envelope.

## Step 2 — classify features

For each major feature, classify it:

| Class | Meaning |
|---|---|
| A | Projection over existing events/context |
| B | Provider-rendering/capability extension |
| C | Frontend/client over existing protocol |
| D | New independent subsystem |
| E | Unclear / needs investigation |

Only D and E are candidates for scope-risk critique.

## Step 3 — cash-out analysis

For each A/B/C feature, explain why the substrate makes it cheaper.

Use this structure:

```text
Feature:
Normal/transcript-blob implementation would require:
mu/substrate implementation reuses:
Evidence:
Remaining work:
```

## Step 4 — bypass detection

Look for features that bypass the substrate.

Examples:

- model-affecting config stored only in UI local state;
- prompt injection not recorded as context span/event;
- tool authority changed without capability event/policy;
- metrics scraped from logs instead of projected from typed events;
- provider-specific behavior hardcoded outside provider capabilities.

## Output verdict

Use one of:

- High architectural compression: broad surface is generated by small core.
- Moderate compression: many features reuse core, but some side systems exist.
- Low compression: features are mostly independent systems.
- Unknown: insufficient evidence.
```

---

## Skill 4: `implemented-vs-aspirational-audit`

### Purpose

Stop reviewers from treating specs as code or missing already-implemented
features.

### When to use

- Architecture reviews with many specs.
- Roadmap planning.
- “What works today?”
- Pre-PR / pre-epic triage.

### Draft `SKILL.md`

```yaml
---
name: implemented-vs-aspirational-audit
description: >-
  Separates implemented behavior from planned specs, open beads, closed beads,
  docs, and stale handoffs. Prevents reviews from recommending already-done
  work or treating aspirational docs as current behavior.
when_to_use: >-
  Use during roadmap reviews, architecture reviews, and before recommending
  new work in a repo with specs/beads/handoffs.
status: draft
---
```

### Draft body

```markdown
# Implemented vs Aspirational Audit

## Rule

Do not say "mu has X" or "mu lacks X" until you classify the evidence.

## Evidence classes

- **Implemented** — code path exists and tests/smoke/docs show it works.
- **Partially implemented** — code exists but known gaps remain.
- **Specified** — design/spec exists, no implementation or incomplete.
- **Filed** — bead/issue exists.
- **Discussed** — handoff/memory/conversation only.
- **Rejected/deferred** — explicit decision not to do now.
- **Stale/unknown** — document may not reflect current state.

## Required table

```markdown
| Capability | Status | Evidence | Gaps | Existing bead/spec |
|---|---|---|---|---|
```

## Commands to check

```sh
br list --status open --limit 0 2>/dev/null
br list --status closed --limit 0 2>/dev/null
rg "<concept>|<bead-id>|<spec-id>" .
code-index recall --full "<concept>"
```

## Review discipline

Before recommending an item:

1. Search code.
2. Search specs/docs.
3. Search beads.
4. Check whether it is already closed.
5. If already filed, recommend priority/sequence, not creation.
```

---

## Skill 5: `ecosystem-comparison-review`

### Purpose

Make ecosystem comparisons concrete instead of generic.

### When to use

- “Compare this to current ecosystem.”
- “How does this differ from Claude Code/Codex/Aider/LangChain/MCP?”
- README/positioning work.

### Draft `SKILL.md`

```yaml
---
name: ecosystem-comparison-review
description: >-
  Evidence-based comparison between a project and its ecosystem. Forces
  concrete center-of-gravity, implemented-vs-aspirational, and structural
  difference analysis instead of generic "less polished but ambitious" prose.
when_to_use: >-
  Use when asked to compare a project to existing tools/frameworks/products.
status: draft
---
```

### Draft body

```markdown
# Ecosystem Comparison Review

## Rule

Do not compare by vibes. Compare centers of gravity and implemented surfaces.

## Required comparison dimensions

For each comparator:

```markdown
### <Comparator>

| Dimension | Comparator | This project |
|---|---|---|
| Center of gravity | | |
| What it does better today | | |
| What this project does structurally differently | | |
| Implemented overlap | | |
| Aspirational overlap | | |
| Evidence | | |
```

## Suggested comparators for agent runtimes

- Claude Code
- OpenAI Codex CLI/TUI
- Aider
- Continue/Cursor/Zed agent surfaces
- LangChain / AutoGen / CrewAI
- MCP ecosystem
- Project-specific ancestors or inspirations

## Forbidden phrases without evidence

- "more polished"
- "more ambitious"
- "less mature"
- "enterprise-ready"
- "production-grade"
- "local-first"
- "agentic"

If used, tie to concrete behavior.
```

---

## Skill 6: `roadmap-from-evidence`

### Purpose

Produce next-work recommendations only after implemented/current/open work has
been inspected.

### When to use

- “What should we work on next?”
- “Recommend a timeline.”
- “Prioritize the roadmap.”

### Draft `SKILL.md`

```yaml
---
name: roadmap-from-evidence
description: >-
  Builds a roadmap from observed current state, recent trajectory, open work,
  and dependency order. Avoids generic prioritization advice.
when_to_use: >-
  Use when asked for timeline, next steps, sequencing, or prioritization.
status: draft
---
```

### Draft body

```markdown
# Roadmap From Evidence

## Required inputs

Inspect:

1. Current working tree.
2. Latest handoff.
3. Recent commits.
4. Open beads/issues.
5. Recently closed beads/issues.
6. Relevant specs.
7. Failing tests or known WIP notes.

## Roadmap principles

Order by:

1. Preserve current WIP.
2. Close active dogfood friction.
3. Complete substrate seams that block projections.
4. Build projections over completed substrate.
5. Only then build new independent subsystems.

This is a heuristic, not a generic scold. If the timeline shows a different
successful ordering, preserve it.

## Required output

```markdown
## Current arc

## Blocking issues

## Already-filed work

## Proposed sequence

| Phase | Duration | Work | Why now | Evidence | Exit criteria |
|---|---|---|---|---|---|

## Things not recommended now

For each:
- Why it is tempting
- Why not now
- What would make it timely
```

## Evidence rule

Every roadmap item must cite at least one of:

- open bead;
- recent handoff;
- failing test;
- code gap;
- current WIP;
- operator-stated priority;
- dependency relationship.
```

---

# Meta skill: `architecture-review-orchestrator`

This ties the others together and is the entrypoint for “review this project.”

## Draft `SKILL.md`

```yaml
---
name: architecture-review-orchestrator
description: >-
  Orchestrates a multi-part architecture/codebase review using specialized
  review skills: chronology, substrate compression, implemented-vs-aspirational
  audit, ecosystem comparison, and roadmap-from-evidence. Designed so each
  section can be delegated to separate agents and synthesized.
when_to_use: >-
  Use when asked to review/examine a project, explain what it is and why,
  compare to ecosystem, assess architecture, or recommend next work.
status: draft
---
```

## Draft body

```markdown
# Architecture Review Orchestrator

## Goal

Produce an evidence-backed project review without generic consultant filler.

## Subskills

Run these in order:

1. `architecture-chronology-review`
2. `substrate-compression-review`
3. `implemented-vs-aspirational-audit`
4. `ecosystem-comparison-review`
5. `roadmap-from-evidence`

Apply `evidence-disciplined-review` across all outputs.

## Parallel mode

These can be delegated independently:

| Worker | Skill | Output |
|---|---|---|
| Chronology worker | architecture-chronology-review | timeline + verdict |
| Substrate worker | substrate-compression-review | primitive map + feature classification |
| Status worker | implemented-vs-aspirational-audit | capability status table |
| Ecosystem worker | ecosystem-comparison-review | comparator table |
| Roadmap worker | roadmap-from-evidence | proposed sequence |

The orchestrator synthesizes. Workers must not make global final claims outside
their assigned scope.

## Required final report

```markdown
# <Project> architecture review

## Executive summary

## What it is

## Why it exists

## Original architecture vs current architecture

## Core substrate

## Architecture cash-out

## Implemented vs aspirational

## Ecosystem comparison

## Risks and critiques

Each critique must use the evidence discipline table.

## Recommended timeline

## Claims not made

## Provenance
```

## Quality gate

Before finalizing, check:

- Did any critique lack evidence?
- Did any recommendation duplicate existing closed work?
- Did any roadmap claim ignore current handoff/WIP?
- Did any "risk" appear only because the project has many docs?
- Did the report distinguish architecture maturity from product polish?
```

---

# Proposal: code-index epic

Working title:

> **Epic: code-index project-intelligence substrate**

## Problem statement

`code-index` currently helps locate code symbols and chunks. That is useful,
but architecture reviews need more than source code. In mu, project truth lives
across:

- source code;
- README / AGENTS;
- architecture specs;
- implementation specs;
- measurement docs;
- handoffs;
- MORNING files;
- beads;
- commit history.

Without those artifacts, agents can find symbols but still make unsupported
claims about focus, ambition, roadmap, or whether work is already done.

## Goal

Extend `code-index` from code-symbol recall into typed project-artifact recall,
while preserving filters so retrieval does not become noisy.

The key design:

> one index, typed chunks, artifact-aware filters, freshness metadata, and
> review-oriented retrieval packs.

## Non-goals

- Do not build a full project-management system.
- Do not replace `br`.
- Do not replace `jj`/`git`.
- Do not infer implementation status solely from docs.
- Do not dump all markdown into embeddings with no artifact typing.

---

## Phase 1 — Markdown artifact indexing

### Scope

Index:

- `README.md`
- `AGENTS.md`
- `CLAUDE.md` if present in repo
- `specs/**/*.md`
- `HANDOFF*.md`
- `MORNING*.md`
- `docs/**/*.md` if present

### Chunking

Markdown-aware heading chunks:

```text
# H1
## H2
### H3
```

Store heading path:

```text
specs/architecture/event-sourced-context.md
> Thesis
> Projections
```

Preserve:

- tables;
- code fences;
- list structure;
- frontmatter/status tables.

### Schema additions

Add chunk metadata fields:

```text
artifact_type:
  code | readme | agents | spec | measurement | handoff | morning | doc

heading_path
doc_id
status
created_at
updated_at
last_modified_unix_ms
source_path
```

### CLI

```sh
code-index ingest .
code-index recall "event sourced context" --kind spec
code-index recall "current state" --kind handoff,morning
code-index recall "architecture" --kind code,spec,readme
```

### Acceptance criteria

- `code-index status` reports artifact counts by type.
- Recall output labels artifact type.
- Existing code recall still works.
- Markdown chunks include heading path in `--full` output.

---

## Phase 2 — Beads indexing

### Scope

Index `.beads/issues.jsonl` as structured artifacts.

Each bead is one chunk with metadata:

```text
bead_id
title
status
priority
type
created_at
updated_at
closed_at
blocked_by
labels
```

Include comments/descriptions as searchable text.

### CLI

```sh
code-index recall "session lifecycle" --kind bead
code-index beads "context explorer"
code-index status --beads
```

Maybe skip new subcommands initially and rely on `recall --kind bead`.

### Output requirement

Recall output must show:

```text
OPEN P2 mu-u6hc — mu context — OS-memory-map view...
CLOSED P1 mu-u1ld — Sessions persist across daemon restart...
```

### Acceptance criteria

- Agents can search for already-filed work before recommending it.
- Closed/open status is visible in recall output.
- Bead IDs are searchable exact tokens.

---

## Phase 3 — Commit/history indexing

### Scope

Index commit metadata, not full diffs initially.

Fields:

```text
commit_id
change_id if jj available
author_time
description
changed_files
```

Optional later:

- first-introduced symbol;
- last-touched symbol;
- diff summary.

### CLI

```sh
code-index timeline "provider renderer"
code-index recent --concept "mu-solo viewport"
code-index first-touch "ContextAssembly"
```

Initial minimal form can be:

```sh
code-index recall "DynamicViewport" --kind commit
```

### Acceptance criteria

- Architecture reviews can retrieve chronology evidence.
- First-commit architecture can be surfaced.
- Recent implementation trajectory can be summarized from indexed commits.

---

## Phase 4 — Cross-reference extraction

### Scope

Extract and store links among artifacts:

- bead IDs: `mu-u6hc`, `mu-035`;
- spec paths;
- Rust identifiers in backticks;
- file paths;
- commit SHAs;
- issue/PR URLs.

### Relationship examples

```text
doc chunk mentions EventPayload
code comment mentions mu-035
bead mentions specs/architecture/session-lifecycle.md
commit message closes mu-u1ld
```

### CLI

```sh
code-index related EventPayload
code-index related mu-u6hc
code-index related specs/architecture/event-sourced-context.md
```

### Acceptance criteria

- Given a symbol, show related specs/beads/commits.
- Given a bead, show likely code/spec references.
- Output labels relationship type:
  `mentions`, `defines`, `closes`, `references_path`.

---

## Phase 5 — Retrieval controls and ranking

### Problem

Semantic recall can over-rank tiny module declarations or stale docs.

### Features

Add filters:

```sh
--kind code,spec,bead
--exclude-kind module
--min-lines 5
--status open,closed
--freshness current,historical,all
--purpose architecture|current-state|roadmap|implementation|review
```

Initial useful subset:

```sh
--kind
--exclude-kind
--min-lines
```

### Ranking adjustments

- Downweight tiny chunks unless exact lexical match.
- Downweight old handoffs for architecture queries.
- Boost measurements for claims involving performance/cost.
- Boost open beads for roadmap queries.
- Boost code for implementation queries.

### Acceptance criteria

- Broad architecture queries return meaningful docs/code, not just `pub mod`.
- Current-state queries return handoffs/open beads.
- Implementation queries return code first.

---

## Phase 6 — Review packs

### Scope

Add preset commands that assemble evidence bundles for agents.

### CLI

```sh
code-index review-pack architecture
code-index review-pack current-state
code-index review-pack roadmap
code-index review-pack substrate
```

### Example: `review-pack architecture`

Outputs:

- first commit README/AGENTS if available;
- current README/AGENTS;
- workspace crate/module map;
- top event/protocol/provider/session symbols;
- key architecture specs;
- recent commits;
- open/closed high-priority beads.

### Acceptance criteria

- A review agent can run one command and get a structured evidence pack.
- Pack output is deterministic and artifact-labeled.
- It does not exceed a configurable token/line budget.

---

## Phase 7 — Concept map mode

### Scope

Cluster recall results into architectural groups.

### CLI

```sh
code-index map "mu architecture"
```

Example output:

```text
Event substrate:
  EventPayload
  SessionEventLog
  AgentEvent
  forward_events

Provider projection:
  Provider
  ProviderRenderer
  CacheStrategy
  MessageInput

Session runtime:
  Sessions
  build_and_register_session
  handle_ask_session

Frontend clients:
  mu-tui
  mu-solo::App
  DynamicViewport
```

### Acceptance criteria

- Helps agents understand codebase shape before reading chunks.
- Links each concept group to concrete symbols/docs.

This can be heuristic at first: cluster by path/module/kind and semantic
similarity.

---

# Proposed implementation epic structure

## Epic title

`code-index: typed project-artifact indexing for evidence-backed reviews`

## Beads/tasks

### `code-index-001`: Markdown heading chunker

- Parse markdown into heading-path chunks.
- Preserve code fences/tables.
- Store `artifact_type`, `heading_path`, `mtime`.

### `code-index-002`: Artifact-type schema + status output

- Extend DB schema.
- Update `status` to show counts by artifact type.
- Ensure migration from existing DB.

### `code-index-003`: Recall filters by artifact kind

- Add `--kind` / `--exclude-kind`.
- Label output with artifact type.

### `code-index-004`: Beads JSONL indexer

- Parse `.beads/issues.jsonl`.
- Store one chunk per bead.
- Include status/priority/type metadata.
- Output open/closed status in recall.

### `code-index-005`: Commit metadata indexer

- In jj repos: collect `jj log` metadata.
- Fallback to git.
- Index commit descriptions and changed files.

### `code-index-006`: Cross-reference extractor

- Extract bead IDs, paths, symbols in backticks, SHAs.
- Store edges in relation table.

### `code-index-007`: Related command

- Implement `code-index related <symbol|bead|path>`.
- Show docs/beads/code/commits connected by extracted refs.

### `code-index-008`: Ranking fixes for tiny chunks

- Downweight tiny module declarations.
- Add `--min-lines`.
- Add tests for broad architecture queries.

### `code-index-009`: Review pack presets

- `architecture`
- `current-state`
- `roadmap`
- `substrate`

### `code-index-010`: Concept map prototype

- Cluster top recall results by artifact type/module/path.
- Output grouped summary.

---

# Proposed skill: `code-index-investigation`

This skill should exist now, before the tool enhancements. It teaches agents
to use the current tool and later can be updated to use new flags.

## Draft `SKILL.md`

```yaml
---
name: code-index-investigation
description: >-
  Use code-index as the first-pass codebase investigation tool. Covers when to
  ingest, how to combine semantic and lexical recall, how to verify results
  with direct reads, and how to avoid treating recall as proof. Future version
  will use typed docs/beads/history indexing.
when_to_use: >-
  Use whenever investigating a codebase, answering architecture questions,
  finding implementation seams, reviewing a project, or preparing a roadmap.
status: draft
---
```

## Draft body

```markdown
# code-index Investigation

## Purpose

`code-index` accelerates orientation. It does not replace direct reads,
tests, git/jj history, beads, or evidence discipline.

Use it to find likely code seams quickly, then verify with terrain reads.

## Pre-flight

From repo root:

```sh
code-index status
```

If stale or missing:

```sh
code-index ingest .
```

If embeddings are expensive/unavailable and lexical is enough:

```sh
code-index ingest --no-embed .
```

## Basic query pattern

Use both semantic and lexical modes.

```sh
code-index recall --full --mode semantic "<concept>" --limit 10
code-index recall --full --mode lexical "<exact terms>" --limit 10
```

Use semantic for concepts:

```sh
code-index recall --full --mode semantic \
  "provider abstraction renderer cache strategy compaction"
```

Use lexical for known names:

```sh
code-index recall --full --mode lexical "DynamicViewport"
```

## Do not trust recall alone

For every important result:

1. Read the file directly.
2. Inspect surrounding context.
3. Check tests if relevant.
4. If making history/roadmap claims, inspect commits/beads/handoffs.

## Current limitations

As of draft:

- source-code chunks only;
- docs/specs/beads may not be indexed;
- semantic recall can over-rank tiny module declarations;
- chronology requires git/jj commands;
- roadmap requires beads/handoff inspection.

## Required architecture-review companion checks

For architecture reviews, pair code-index with:

```sh
git rev-list --max-parents=0 HEAD
git show <first-commit>:README.md 2>/dev/null || true
jj log -r '::@' --limit 40 --no-graph
br list --status open --limit 40 2>/dev/null || true
br list --status closed --limit 30 2>/dev/null || true
ls -lt HANDOFF* MORNING* 2>/dev/null | head
```

## Good uses

- Find load-bearing symbols.
- Map concepts to code.
- Locate provider/session/event/tool seams.
- Discover tests related to a concept.
- Compare implementation to docs.

## Bad uses

- Declaring whether a project is focused.
- Declaring chronology.
- Deciding whether work is already filed.
- Treating top recall result as authoritative.
- Recommending roadmap without beads/handoffs.

## Output discipline

When citing code-index, include:

```text
Query:
Mode:
Result:
Verified by direct read: yes/no
```

## Future enhanced-code-index mode

When typed artifacts land, prefer:

```sh
code-index recall "<query>" --kind code,spec,bead
code-index review-pack architecture
code-index related <symbol-or-bead>
```
```

---

# How these skills compose in multi-agent review

A full review can be orchestrated like this:

```text
orchestrator
  ├─ chronology worker
  │    skill: architecture-chronology-review
  │    output: timeline + original/current seam comparison
  │
  ├─ substrate worker
  │    skill: substrate-compression-review
  │    output: substrate primitives + feature classification
  │
  ├─ status worker
  │    skill: implemented-vs-aspirational-audit
  │    output: capability status table
  │
  ├─ ecosystem worker
  │    skill: ecosystem-comparison-review
  │    output: comparator table
  │
  └─ roadmap worker
       skill: roadmap-from-evidence
       output: phased recommendation
```

All workers load:

```text
evidence-disciplined-review
code-index-investigation
```

The orchestrator’s job is not to redo every investigation. It checks:

- whether each worker supplied evidence;
- whether workers conflict;
- whether a recommendation duplicates closed/filed work;
- whether speculative claims leaked into final recommendations.

---

# Recommended first implementation order

For skills:

1. `evidence-disciplined-review`
2. `code-index-investigation`
3. `architecture-chronology-review`
4. `substrate-compression-review`
5. `implemented-vs-aspirational-audit`
6. `architecture-review-orchestrator`
7. `ecosystem-comparison-review`
8. `roadmap-from-evidence`

For `code-index`:

1. Markdown heading chunks.
2. Artifact type filters.
3. Beads indexing.
4. Commit metadata indexing.
5. Cross-links.
6. Review packs.
7. Concept map.

The reason: evidence discipline + current code-index usage improve reviews
immediately. The first `code-index` implementation win is typed markdown/docs,
because that closes the biggest gap exposed by this review.