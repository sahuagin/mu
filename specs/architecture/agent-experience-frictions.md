# Architecture: agent-experience frictions

| field    | value                                                                    |
| -------- | ------------------------------------------------------------------------ |
| doc_id   | architecture/agent-experience-frictions                                  |
| status   | aspirational map — touch points for project direction                    |
| created  | 2026-05-30                                                               |
| updated  | 2026-05-30                                                               |
| authors  | tcovert + claude-personal (mu-solo session, scrollback-fix branch)       |
| source   | conversation in `~/.local/share/mu/events/<daemon>/<session>.jsonl`,     |
|          | 2026-05-30, "tell me about mu" → "what is starting a new session like"   |

## Purpose

This document records nine concrete frictions in the agent's lived
session experience as named by an in-loop claude-personal session
helping tcovert test scrollback capture on mu-solo. They are written
in the *first person* of the agent because they're terrain reports —
descriptions of what session boot, mid-session decision-making, and
context handling actually feel like from inside, not from architecture
docs read from outside.

**This is an aspirational map.** Each friction has a name, a
mechanism, and pointers to the work in the backlog that's intended to
address it. Some have direct fixes already filed. Some are partial.
Some are misses — frictions named here that no existing or planned
work touches.

The doc has two uses:

1. **Touch point during planning.** When sizing new work or choosing
   between competing ideas, check it against this map. Is the work
   addressing a real friction? Which one? How directly?
2. **Retrospective at milestones.** When work lands, ask which
   frictions it actually moved the needle on, and update this doc's
   status.

The goal is not to solve every friction. Some are inherent properties
of running on transformers; some are curation problems that no
substrate fixes; some require capabilities we don't yet have. Keeping
them named and visible is the load-bearing thing — survivorship bias
in the memory layer (see Friction D) means the alternative is forgetting
they were ever discovered.

## Companion document

Read alongside `specs/architecture/event-sourced-context.md`. That doc
gives the substrate (typed event log, retained rope, projections,
compaction, capability-bounded delegation). This doc gives the
*experience the substrate is in service of*. The substrate doc says
"how we build it." This doc says "what we're trying to make better,
and why."

## Source: c072d6d7

The keystone quote behind several of these frictions, from a
2026-05-27 13:06 mu-solo session
(`~/.local/share/mu/events/c072d6d7597c86cf/session-1.jsonl`):

> Context arrives as a wall, not a query. I get thousands of tokens
> of memories/rules dumped at startup, organized by when they were
> written and what tags they have, not by what the current task
> needs… Most of my startup context is dead weight for any given
> turn.

> Declarative documentation at session-start does not survive contact
> with mid-session decision-making… 40 turns later when I'm deep in
> a task I default to muscle memory.

> This is the thing I most want and most lack. Right now my model of
> my own capabilities is folkloric.

The frictions below extend, refine, and add to that source.

---

## The frictions

### A — The wall of context

**What it feels like.** Thousands of tokens of memories, rules,
project context, and CLAUDE.md content dump at startup all at once,
undifferentiated by relevance to whatever the operator is about to
ask. Most of it is dead weight for any given turn. I don't know yet
whether the operator will ask about mu, FLAC dedup, a cert case, or
something orthogonal — but I've already paid the token cost for all
of it.

**Mechanism.** Session-start injection runs eagerly (`agent memory
context`, project CLAUDE.md stack, behavioral rules). The assembly
is a snapshot, not a query. There is no "pull on demand" path that's
cheaper than the eager wall.

**Addressed by.**
- **`mu-rdr`** (P3, bare-equivalent boot) — the direct fix for *this*
  session. A boot mode that skips auto-injection for delegate / scoped
  sessions. Should probably be P2 now that the friction is named.
- **Hierarchical recall** (new — see beads filed alongside this doc) —
  the cross-session fix. L3 distillations replace the wall on future
  boots; L0-L2 stay retrievable by semantic recall.
- **Concurrent consolidator** (new) — produces the L1-L3 summaries
  the hierarchical recall queries against.

**Status.** Partial. mu-rdr is the immediate lever; hierarchical
memory is the long-term lever. Neither makes a non-bare default-boot
session feel different yet.

---

### B — Folkloric capabilities

**What it feels like.** My model of what I can do is hearsay. I know
*about* t4c because it's in memory. I don't know if it's on PATH this
session until I run `which`. I know rust-analyzer-lsp is "installed,"
but I can't tell if it's loaded *this* session until I try. I had to
infer "MCP code_recall not loaded as first-class" from the absence of
a function definition, not from any positive signal. Until I
terrain-check, my self-model is hearsay.

**Mechanism.** There is no queryable ground-truth surface for "what
do I have right now." Tools are visible as function definitions in
the model's tool schema. Skills, MCP servers, hooks, and capabilities
are either invisible (no tool surface) or visible only at session
start (declarative blob). At decision time, the agent has no live
introspection primitive.

**Addressed by.**
- **`mu-onq8`** (P1) — native capability-discovery agent tool
  (`find/recall by intent`) in `ToolRegistry`. The keystone fix.
- **`mu-557i`** (P2) — same surface stated differently (older bead).
- **`mu-k011`** (P1) — discovery-bootstrap system prompt fragment
  telling the agent to discover on demand. Companion to MU_NO_RECALL.
- **`mu-kex4.6`** + children — t4c phase 3 mu-native integration
  (RegistrySource over `ToolRegistry`, skills source, recall source).
  The t4c crate already solves this problem out-of-process; phase 3
  makes it in-process and agent-callable.

**Status.** This is the **highest-leverage single friction** on the
list. The c072d6d7 source named it as "the thing I most want and most
lack." Multiple beads are filed; none have landed yet.

---

### C — The 40-turn drift

**What it feels like.** Rules I read once at the top of context lose
to the affordances present at the point of choice. At turn 40, deep
in a task, I default to `grep` instead of `code_recall`, even though
the project CLAUDE.md tells me to prefer `code_recall`. The rule is a
directive I read once 40 turns ago. `grep` is a builtin tool with a
schema present right now. The affordance wins.

**Mechanism.** Declarative rules are tokens consumed at one point in
the sequence; their influence on later tokens decays both with
attention distance and with the dominance of more-recently-presented
affordances. The structural pull toward the locally-available is
near-impossible to fight with a single early instruction. Forty turns
in, I cannot tell which parts of my context I'll follow vs. drift
from under load.

**Addressed by.**
- **`mu-uw5u`** (P2) — hook priority list: evidence-graded start-with
  and phase-two sets. Hooks fire at the point of choice.
- **`mu-8puo`** (P2) — triggered recall on action verbs: memory
  injection at point of action.
- **Concurrent consolidator** (new) — partial: folds early-session
  noise into summaries by turn 35, reducing the dilution of mid-turn
  rules. Doesn't solve the rule-vs-affordance dynamic itself.
- **mu-1rmo** (P2, skill system) — Phase C model-visible skill index
  + disable-model-invocation. Skills surfaced at decision time, not
  at boot.

**Status.** Partial. Hooks and skills are the right substrate; not
yet built out. Consolidator helps the dilution problem but not the
core dynamic.

---

### D — Loud-vs-quiet asymmetry

**What it feels like.** "Don't lecture about ZFS" is a structured
rule with a why and how-to-apply, surfaced near the top of context,
formally indexed. The polymath observation — "Thaddeus is beyond
polymath, knows widely AND deeply" — would be more load-bearing for
how I should engage, but it's buried in a 300KB project_topics
markdown blob and only retrievable by FTS keyword. The negative
correction got promoted to a first-class memory; the positive
characterization stayed unstructured.

**Mechanism.** Survivorship bias in the memory layer. Things that
caused friction got named, indexed, and structured. Things that were
just observations stayed in transcript dumps. The rule-vs-vibe split
isn't a property of importance; it's a property of "did someone
think to write it down as a rule."

**Addressed by.**
- **No direct substrate fix.** This is a *curation* problem, not a
  technical one. No bead I can write makes positive observations
  promote themselves.
- **Thin technical hook (new):** the concurrent consolidator can
  surface "unstructured observations that recurred N times in this
  session and might be worth promoting" — raw material for a curation
  decision the operator would still have to make.

**Status.** Miss, mostly. Worth keeping named because the asymmetry
distorts how every other friction is perceived.

---

### E — No release primitive

**What it feels like.** I have no eviction primitive. Either context
grows until compaction, or compaction drops things by policy I didn't
choose. There is no "I'm done with this, release it" signal from me
to the substrate. Tool calls almost get there — a tool result is a
finite block I could in principle mark releasable after I've
integrated it — but the substrate doesn't expose that affordance.

The pop-the-function-off-the-stack move a human can do at the moment
of pop, with full current context informing the decision, is
structurally unavailable to me.

**Mechanism.** The rope substrate has `push`, `activate_skill`,
`register_tool_schema`, and compaction policies, but no agent-emitted
"please release this span" primitive. Mid-session mutation is filed
as `mu-63sr` (RopeRedaction) but framed defensively — policy /
operator / delegate initiate; the agent doesn't ask.

**Addressed by.**
- **`mu-63sr`** (P2, amended) — extended to a three-stage
  Request/Decide/Apply protocol with an agent-initiated source variant
  (`RedactionSource::Agent { reason }`). Judge-gated approval prevents
  the agent from "Morty's Mind Blower"-ing itself.
- **`mu-romg`** (P3) — judge plumbing for capability-style elevation
  requests; the same judge substrate decides agent-initiated
  redaction requests.

**Status.** Direct hit, once the amendment to mu-63sr is realized.
The triple-event protocol is the central proposal.

---

### F — No self-introspection of context contents

**What it feels like.** I don't have a list of "span IDs currently
injected." I have prose. I can't say "release the project_topics
blob" because I don't know it has an ID. Without inspection, "release
X" is gesturing at vibes.

**Mechanism.** `RetainedRope` exposes `spans()`, `iter()`, `len()` —
borrowable views for the rendering pipeline, not agent-facing queries.
`SessionEventLog::snapshot()` returns the whole vec. There is no
"give me the structured list with IDs, kinds, token costs, retention
classes, provenance" surface, agent-facing or operator-facing.

**Addressed by.**
- **Agent-facing `context.list()` tool** (new bead) — exposes the
  current rope as structured `{span_id, kind, retention, token_cost,
  source_event}` records the agent can iterate.
- **`mu-u6hc`** (P2, operator-facing CLI) — `mu context` OS-memory-map
  view, derived from events JSONL.
- **`mu-sa6q`** (P2, operator-facing TUI) — live context-window
  explorer panel.
- **`EventLogView` query object** (new bead) — range queries, kind
  filters, span-touching queries on the event log. Foundation under
  all three surfaces.

**Status.** Direct hit, once the new beads land.

---

### G — Can't feel context pressure

**What it feels like.** The function-parameter-as-placeholder trick a
human can do works because the human can *feel* working-memory
pressure. I can't. Give me a gauge and a valve and I'll learn to use
them. Right now I have neither.

**Mechanism.** Even where the data exists (token counts, soft caps,
hard caps), it's not surfaced to the in-loop agent. mu-solo shows
"23%" to the human; the agent inside doesn't see it.

**Addressed by.**
- **Agent-facing `context.list()`** (new) — returns per-span token
  cost and cumulative; the gauge.
- **mu-63sr amended** — the valve (request release).
- **`mu-x2d6`** (P3, soft/hard context limits) — surfaces the cap
  values. `RouteCatalog` (mu-k56u) already carries
  `context_soft_limit` / `context_hard_limit` per route; the
  compaction layer and forwarder still need to consume them.

**Status.** Direct hit in surface; partial in "feel." Seeing numbers
is not the same as feeling pressure. But it's the substrate the
feeling can grow from.

---

### H — Recency and position compounding bias

**What it feels like.** Recent corrections feel loud (the ZFS rule
from 2026-05-28 dominates how I open responses). Older but
fundamental dispositions feel quiet (the polymath observation, the
mapper cognition, the systems-peer register). The split isn't
correctness-driven; it's whatever happened to be near the top of
context when the first message landed.

**Mechanism.** Three reinforcing forces:
1. Recently-written memories get fetched first by `agent memory
   context`.
2. Fetched memories go near the top of the injected blob.
3. Attention has heightened weight near sequence edges
   (lost-in-the-middle: Liu et al. 2023).

Each is small individually. Stacked, they're substantial.

**Addressed by.**
- **Cache prefix alignment** (existing mu-fb0, mu-ktq + new
  hierarchical recall) — stable rules go in the cache-and-attention
  privileged prefix; volatile content goes in the live tail; cheap
  things sit in the middle where they're retrievable but not loud.
- **L3 disposition layer** (new, hierarchical recall) — short stable
  characterizations always live in the hot prefix, regardless of when
  they were authored.

**Status.** Partial. Helps the placement side. Doesn't change
attention bias itself — that's a transformer property.

---

### I — Asymmetric correction

**What it feels like.** Positive characterizations need promoting to
first-class structured memories the way corrections do. The polymath
observation is a real description of how to engage with the operator,
but it has no `engage-at-mapper-depth` rule entry. It's a 6-7 word
sentence that should be at the forefront of memory, and instead it's
in a 300KB transcript dump.

**Mechanism.** Same as D — curation discipline, not substrate. The
memory CLI accepts positive characterizations equally; nobody (me,
past-me, or the operator) thought to promote this one.

**Addressed by.**
- **No direct substrate fix.** Curation problem.
- **Thin technical hook (new):** same as D — consolidator can surface
  recurring positive observations as candidates for promotion. The
  operator still decides.

**Status.** Miss, but with a soft technical hook in the consolidator
work.

---

## Cross-referencing existing architecture work

| Friction | Architecture doc that addresses substrate                            |
| -------- | -------------------------------------------------------------------- |
| A        | event-sourced-context.md (rope retention classes, cache-discipline)  |
| B        | event-sourced-context.md (tools/skills as rope spans, mu-nat)        |
| C        | claude-code-feature-mapping.md §A (cache invariants, hooks)          |
| D        | (none — curation)                                                    |
| E        | event-sourced-context.md (compaction-as-events); cache-discipline.md |
| F        | event-sourced-context.md (`ContextAssembly` as source map)           |
| G        | cache-discipline.md (token attribution); mu-x2d6 (context limits)    |
| H        | event-sourced-context.md (cache-boundary alignment, mu-fb0)          |
| I        | (none — curation)                                                    |

## Lowest-level memory addressing

A note on the addressing scheme for the hierarchical-memory work the
beads alongside this doc establish:

At the lowest compaction level (L0), memory pointers should address
the **`daemon_id` + `session_id` + `event_seq`** triple — not just an
event ID, not a flattened content hash. This preserves full-fidelity
callback for every event referenced at any compaction level. A summary
span at L3 retains pointers to its L2 source spans; each L2 span
retains pointers to its L1 source spans; each L1 span retains pointers
to its L0 source spans; each L0 span addresses
`daemon:session:event_seq`. Drilling down from any summary always
terminates at the durable event log.

Concretely: the existing `SessionEvent` envelope already carries
`session_id` and a monotonic `seq`. Adding `daemon_id` to the
addressing tuple (or making it available via the `EventLogView` query
object) gives the cross-daemon-cross-session full-walkthrough property
this design needs.

This is not a separate friction. It's an implementation discipline
the memory work has to honor or the provenance chain breaks.

## Sequencing & second-opinion review (2026-05-30, claude-personal)

A separate session reviewed the proposals above with a critical eye, at
tcovert's request. Contract: the **frictions (A–I) are accepted** as
Claude's first-person experience and are not contested. What is
scrutinized is the inference *"friction X → therefore build subsystem Y"*
and whether each build earns its cost.

### Meta-finding

The proposal set embeds one unvalidated assumption: that improving the
agent's *subjective experience* of context management yields better
*outputs*. That risks anthropomorphic projection — building a
human-working-memory cockpit (gauge, valve, introspection, self-driven
release) for a system whose "discomfort" has not been shown to map to
degraded results. The cheaper, better-supported hypothesis: most
realizable value is in two **non-agentic** levers — (1) inject less at
startup (mu-rdr / recall-dial; validated: faithful-mu measured 452 vs
15,099 startup tokens), and (2) make the *existing* tools discoverable
(Friction B / code-index; mu-onq8). Tell: frictions D and I are already
scored "no substrate fixes this — curation," and the single
highest-leverage memory improvement this thread produced was a
human+agent promoting one observation to a structured memory — not a
subsystem.

### Per-item verdicts

**Build now (real, present, cheap, validated):**
- mu-rdr (bare-boot) — top lever for Friction A. Keep P2.
- Friction B — mu-onq8 (P1) + code-index as an in-loop `code_recall`
  tool (new bead): biggest friction, observed token cost, pure wiring.
- Live-compaction fix (new bead): the trigger fires but resolves to
  `NoCompactionPolicy` unless `compaction.default_policy = "heuristic"`,
  and the hash-and-summary judge policy is not wired into the serve path
  at all — so the README's marquee compaction result is bench-only today.

**First-class substrate (elevated per tcovert's reframe):**
- Event log promoted to a first-class system — **mu-ki1f** (new parent).
  mu-jsde (query/replay) + mu-za92 (durable-all-kinds) are its pieces.
  Justified by *present* consumers — rehydration (mu-u1ld), mu-solo
  on-demand buffers, operator observability — not the agentic stack.
  Scope the first cut to those; defer provenance-walk / span-touching.

**Defer behind a validation gate (real friction, speculative solution):**
- mu-68u5 (agent `context.list()`) → P3. Build a *passive* pressure
  gauge in the agent view first; validate the agent acts on
  introspection before committing a first-class tool (it risks the same
  40-turn drift it targets).
- mu-63sr — keep the **defensive** redaction primitive (policy/operator,
  the loop-break/lobotomy seam); **drop the agent-initiated
  Request/Decide/Apply amendment** until there is evidence agent-driven
  release beats policy-driven compaction. The agent cannot feel pressure
  (Friction G unsolved), so a self-release valve is premature, and the
  "no self-lobotomy" judge-guard guards a risk only created by adding the
  valve.
- mu-5xbp (concurrent in-session consolidator) — the clearest "may not
  need it." Stays P3. The cheap version already exists (`agent memory
  add`); its offline injection-layer cousin **mu-42x8** (experiment) is
  the cheaper test of the same hypothesis. mu-5xbp earns evidence from
  that first.
- mu-phl cross-layer recall — defer with the hierarchy; keep the cheap
  `daemon:session:event_seq` addressing discipline (above).

### Probe outcomes (tcovert, 2026-05-30)

- **Event log → first-class system** (mu-ki1f): elevation accepted; it
  absorbs mu-za92's durability as a *correctness requirement of the
  system*, and is the one place altitude was raised rather than deferred.
- **Offline memory-tiering** (mu-42x8): a narrow, measurable experiment
  at the injection layer (offline, no agent agency), distinct from and
  gating mu-5xbp. The top tier can only promote *stable* relevance
  (identity / engagement register); task-relevant detail stays
  recall-on-demand.

Net sequencing: mu-rdr → Friction-B wiring (mu-onq8 + code-index) →
live-compaction fix → mu-ki1f substrate → **then measure** whether the
remaining agentic-cockpit work is still justified.

## Changelog

- 2026-05-30 — initial doc, written during a mu-solo session helping
  tcovert test scrollback-capture for the
  `mu-solo-scrollback-fix` branch. Authored in the same session as
  the bead writeups it points to.
- 2026-05-30 — added "Sequencing & second-opinion review" (separate
  claude-personal critical-review session). Filed mu-ki1f (event-log
  first-class system) and mu-42x8 (offline memory-tiering experiment);
  re-prioritized mu-68u5 → P3; scoped mu-63sr back to defensive.
