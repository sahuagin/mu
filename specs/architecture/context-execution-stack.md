# Architecture: context as an execution stack (the harness as a model runtime)

| field      | value                                          |
| ---------- | ---------------------------------------------- |
| doc_id     | architecture/context-execution-stack           |
| status     | architecture breadcrumb (no immediate impl)    |
| created    | 2026-07-09                                     |
| authors    | tcovert + claude (cooperating sessions)        |
| relates to | architecture/event-sourced-context, compaction, hooks-v2 (mu-jdjq), context-stack bead (mu-4jd0), retention union (mu-pb4m) |

## Framing

The same needs keep recurring, from different directions:

- the **plan / task list** needs to be kept and updated across a run;
- **entering** a task wants to take specific actions (e.g. push a fresh
  context, drop a bookmark);
- **completing** a task wants to take specific actions (summarize,
  return a result, release working detail);
- the **harness** should be able to enforce invariants at those points,
  and let the operator extend behavior there through config.

These are not four features. They are facets of one thing: **mu is
becoming a runtime for model execution**, and the piece we keep reaching
for is its *call stack*. This doc names that, so the pieces stop being
designed independently.

**This is a breadcrumb, not a build order.** It records a lens and a
low-risk entry point; §"Should we build it" is deliberately skeptical.

## The lens: a model-execution OS

The generative analogy (tcovert). It is not a perfect match — the notes
on where it breaks are load-bearing, not caveats.

| OS concept | mu equivalent |
|---|---|
| CPU | the model — executes, but *non-deterministically* |
| program / address space in view | the active context (rope): task + working state currently mapped |
| the stack | context management — frames pushed on task entry, popped on completion |
| push params + jump to function start | **enter task**: seed a child context (initial pointer-set / task spec) and begin |
| return value in the return register, restored after pop | **complete task**: a distilled summary placed at the parent's front-of-context |
| backing store / mmap'd file | the durable event log — every frame can read it; nothing is destroyed by a pop |
| currently-mapped pages | a session's retained pointer-set over the log |
| kernel / MMU | the **harness** — enforces the stack discipline the untrusted CPU can't be trusted to keep |
| syscalls / extension points | **hooks** (typed, per hooks-v2) at the enter/complete boundaries |
| interrupts / signals | autonomy wakeups, dialogue/mailbox messages |

### Where it holds

- **The stack discipline is the whole point.** Descending into a
  sub-task is a `call`; finishing is a `ret`. The parent never carries
  the child's scratch — only the return value. This is the natural shape
  of how coding work decomposes (implement feature → touch N functions →
  per function: read/edit/test; the per-function detail is dead once the
  function is verified, only its interface survives upward).
- **The event log is shared backing store.** A pop drops the *pointer*,
  not the events (per event-sourced-context §"Eviction semantics"), so a
  frame can rehydrate a detail it released — the data isn't destroyed,
  the reference is.

### Where it breaks (and why that matters)

- **The CPU is non-deterministic.** A real CPU runs fixed opcodes
  exactly; the model's "execution" is probabilistic and its opcodes
  (tool calls, reasoning) are open-ended. **Consequence:** invariant
  enforcement cannot trust the model to keep stack discipline — the
  *kernel* (harness) must enforce it structurally. This is the same
  discipline as everywhere else in mu (typed capabilities, fail-closed
  gates, "infrastructure enforces what we know better than the model").
  The analogy doesn't weaken that; it *explains* it — the harness is the
  MMU that stops the untrusted CPU from corrupting the stack.
- **The return value is lossy.** A return register is bit-exact; a
  task summary is a *distillation*. So "restore the register" is really
  "place a distilled result" — and the fidelity of that distillation is
  a real design surface, not a copy.
- **It's a process, not just a stack.** Shared log (heap/mmap) + per-frame
  private working sets (mapped pages) + interrupts (async wakeups) means
  the closer match is a *process with shared memory and signals* than a
  pure call stack. The analogy scales to the multi-session / autonomy
  parts of the system, not only the nested-task part.

## The context stack (the concrete mechanism)

Independent of the analogy, the buildable idea:

- **Enter a sub-task** → fork a child context whose initial object is a
  copy/pointer into the parent (event-sourced-context already specifies
  this: read-only parent log + an initial pointer-set + an independent
  child retained set; parent compaction never strands the child).
- **Work** → the child evicts inherited parent detail and adds task
  detail, in its own frame.
- **Complete** → summarize the task down to "the technical detail a
  caller needs," pop back to the parent, drop the task-oriented detail,
  and place the summary at the parent's front-of-context.

### Why it is better-conditioned than compaction

Compaction is damage control: lossy compression of an *arbitrary
token-threshold window*, where a judge infers relevance from a flat rope.
The stack replaces the arbitrary boundary with a **semantic** one:

- **Semantic boundary, not a watermark.** At a task boundary, "what's
  still relevant?" has a clean answer by construction (the distilled
  result), where a token-threshold cut can slice across coherent
  reasoning.
- **The summarize-on-pop is an easier, better-timed task** than
  compaction: bounded scope (one task), clear target (the interface /
  result), performed at the moment of *maximal clarity* (right after
  finishing) rather than cold and mid-flight.
- **It attacks context resend** — the exact cost the accounting design
  calls out ("60% was context resend").

The two are **complementary, not competing.** The stack *reduces the
need* for compaction by keeping frames small and task-scoped; a forked
frame that still grows can itself compact. They must be designed to
compose, not to fight over one rope.

## The orchestration builtin

The recurring actions above are the `call` / `ret` sequences:

- **Task descriptor (PCB).** The plan / task list kept-and-updated is
  the process control block — the durable record of what frame we're in
  and what remains. (Note: per the earlier conclusion, this is a
  *projection over the session event log*, not a new store.)
- **On enter-task actions.** Push the frame; optionally bookmark the
  entry point (a checkpoint — see the rewind/file-checkpoint design,
  mu-u3j5, which is the same event-as-truth substrate).
- **On complete-task actions.** Summarize → return → pop; optionally
  run operator-configured behavior.

Because the **harness owns the boundaries**, it knows the *hard edges
and the intent* — which is exactly what lets it (a) enforce invariants
(a frame can't leak its scratch upward; a pop must carry a return; the
stack can't grow unbounded) and (b) expose **typed hooks** at those
edges for operator extension. This is where **hooks-v2 (mu-jdjq)** and
the **context stack (mu-4jd0)** converge: hooks stop being a generic
fail-open shell surface and become *syscalls at known kernel
boundaries* — the typed-extension shape hooks-v2 requires, given
meaning by the boundaries the stack defines.

## Should we build it — and how

**Not now, and probably not as a bespoke auto-forking feature.**

- **The load-bearing risk is model-driven boundaries.** The value
  depends on the model correctly deciding *fork here* / *pop here*. Fork
  too granular → overhead; too coarse → no benefit; forget to pop →
  unbounded stack; pop too aggressively → drop a needed detail.
  Compaction's one virtue is that it is *automatic*; the stack trades
  that for a judgment surface we have no evidence models handle well.
  This is the same open question as model-managed memory ("mind
  benders") — it needs a **study**, not a hunch.
- **The pain may still be theoretical.** The default policy is
  `NoCompactionPolicy`; context pressure may not be hurting real daily
  use yet. Building an elaborate mechanism for a pain we don't have is
  the premature-optimization this project's discipline vetoes.

**Low-risk entry point: `spawn_worker` as the fork primitive.** It
already draws the boundary *structurally* — an explicit delegation with
a bounded task and a returned result. The 80% version of the whole idea
is: make `spawn_worker`'s return a first-class *pop* — a distilled
summary that lands at the parent's front-of-context, with the child's
working detail never entering the parent. That:

1. tests the core value (task-scoped context + distilled return) without
   the model-driven-boundary gamble;
2. **exercises the current compaction / handoff code** as a side effect,
   which is worth doing regardless;
3. composes with what exists rather than adding a parallel system.

Generalize toward auto-forking (model-drawn boundaries) **only if** (a)
the explicit-boundary version demonstrably helps on real nested work,
and (b) a study shows models fork/pop well.

**And match the architecture's grain.** These patterns tend to *emerge*
from composable primitives rather than being built top-down. The most
in-character move is to make the three primitives compose cleanly —
subagent handoff (the frame), `spawn_worker` (the boundary),
summarize-on-return (the pop) — then *watch whether the stack pattern
emerges* when an orchestration reaches for it, and formalize a "context
stack" only once it has proven itself bottom-up. Forcing a top-down
stack onto an emergent system fights its own grain.

## Related

- `architecture/event-sourced-context.md` — the substrate: rope,
  retention classes, subagent context handoff, eviction semantics.
- `architecture/rewind-file-checkpoints.md` (mu-u3j5) — enter-task
  bookmarks / checkpoints as events; same event-as-truth discipline.
- Compaction (`context/compaction/`) — the complementary, automatic
  mechanism the stack reduces the need for; retention union (mu-pb4m).
- hooks-v2 (mu-jdjq) — typed extension at the stack's known boundaries.
- context-stack bead (mu-4jd0) — the tracked design/study item this doc
  expands.
