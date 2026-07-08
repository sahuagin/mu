# Rewind & file checkpoints

| field | value |
| --- | --- |
| status | design / proposed (implementation deferred) |
| bead | mu-u3j5 (spec half) |
| relates to | event-sourced-context, session-lifecycle, edit tool (mu-t731) |

## Why

Claude Code's `/rewind` lets an operator restore the conversation OR the
files (or both) to a prior point. mu has neither axis today
(`claude-code-feature-mapping.md` lists it unbuilt). The conversation
axis is *nearly* free in mu because the session is already event-sourced
and resumable (`continuation::project_strict` projects a log to a clean
boundary); the file axis needs a new durable artifact — a pre-edit
snapshot of every file a tool is about to mutate.

This spec designs the file-checkpoint substrate and how rewind composes
the two axes. It is design-only; implementation is a later bead.

## The mu-native framing

mu's invariant is **the on-disk event log is the source of truth; state
is a projection** (AGENTS.md). Rewind must live inside that model, not
beside it. So:

- A **checkpoint is an event**, not a side file. Before a mutating tool
  (`edit`, `write`, and any future patch tool) writes, the runtime
  appends a `FileCheckpoint` event carrying the pre-image of each path
  it will touch. The event log already is the durable, ordered,
  replayable spine — checkpoints ride it and inherit its durability
  (spec mu-046 disk-before-memory) and its ordering guarantees for free.
- **Rewind is a projection**, symmetric with resume. "Rewind files to
  event E" = walk the log forward from session start to E, replaying
  `FileCheckpoint` pre-images to reconstruct the tree as of E, then
  write the diff back to disk. "Rewind conversation to E" is the
  existing `project_strict` truncation. "Rewind both" composes them.

This keeps rewind honest: the same log that says "what the model knew"
(`ContextAssembly`) now also says "what the files were," from one
source.

## FileCheckpoint event

Emitted by the tool-dispatch path, immediately before a mutating tool
executes, for each path in its declared write-set:

```
FileCheckpoint {
    tool_call_id: String,      // ties the checkpoint to the edit that follows
    path: String,              // absolute, normalized
    pre_image: PreImage,       // see below
    existed: bool,             // false ⇒ tool is creating the file (rewind deletes it)
}
```

`PreImage` storage strategy (decision, not open):

- **Small files (< 256 KB): inline** the full pre-image bytes
  (compressed). Simplest; the common case for source edits.
- **Large files: content-addressed** — store the pre-image once in a
  session-local blob store (`<state_dir>/checkpoints/<daemon>/<session>/<hash>`)
  and reference it by hash. Dedupes repeated edits to the same big file.
- Binary/huge files (> a hard cap, e.g. 8 MB): store only a
  `existed + hash` stub and mark the checkpoint `restore_unavailable`;
  rewind past it warns rather than silently losing data.

The write-set must be known *before* execution. `edit`/`write` know
their single target trivially. A future multi-edit/patch tool declares
its paths in its arguments. Tools that can't declare a write-set can't
be rewound and must say so (fail-legible, not fail-silent).

## How it hooks into dispatch

In the same `execute_tools` path that PR2's hooks gate lives:

1. Tool passes all gates (capability/retry/validate/hook/permission).
2. If the tool declares `mutating_paths(arguments) -> Vec<PathBuf>`
   (new optional trait method; default `None` = not checkpointable),
   the runtime reads each path's current bytes and appends a
   `FileCheckpoint` event **before** calling `execute`.
3. Tool executes and mutates the file.

The read-before-write this requires is a bonus: it also gives the edit
tool a natural place to enforce read-before-edit later if desired, and
the pre-image is exactly what a future `diff`/undo surface wants.

Cost discipline: checkpointing only fires for tools with a declared
write-set, only on the paths they'll touch, and inline storage is capped
— a read-only session pays nothing, and a normal edit pays one extra
small append. Gate the whole feature behind
`[session].file_checkpoints = true` (default on once proven; off lets a
cost-sensitive operator opt out).

## Rewind operations (frontend surface)

New daemon methods, mirroring resume's shape:

- `session.rewind_preview { to_event_id }` — returns the set of paths
  that would change and a summary diff, without touching disk. Read-only;
  safe to call freely (mu-solo can show it before the operator commits).
- `session.rewind { to_event_id, files: bool, conversation: bool }` —
  performs the rewind. Files: reconstruct-and-write as above.
  Conversation: fork-at-tail a new session seeded from the truncated
  history (reusing the resume machinery), so the original log is never
  rewritten — rewind creates a descendant, it doesn't mutate history.
  This preserves the append-only invariant: even "undo" is a forward
  operation that leaves an auditable trail.

A rewind is itself logged (`RewindPerformed { to_event_id, files,
conversation }`) so the history shows that a rewind happened — you can
rewind a rewind.

## mu-solo UX (sketch, not this spec's scope)

- A `/rewind` command listing recent checkpoints (from the log) with
  their triggering edit and a one-line diff summary.
- Select a point → `rewind_preview` → confirm → `rewind`.
- Default to files-and-conversation; flags for one axis only.

## Interaction with compaction

Compaction rewrites the *context projection*, not the event log
(`ContextAssembly` / the rope). `FileCheckpoint` events live in the
durable log, which compaction never drops — so file rewind reaches back
past a compaction boundary even when the conversation context of that
era is gone. Rewinding *conversation* past a compaction boundary
restores from the pre-compaction log (the truth), not the compacted
view. The two axes degrade independently and honestly.

## Why deferred

The conversation axis is close to free but the file-checkpoint substrate
is a real new durable artifact with storage-lifecycle questions (when
are a dead session's checkpoint blobs GC'd? does resume inherit the
predecessor's checkpoints?). Those deserve their own implementation bead
with its own tests, not a rushed ride-along. This spec fixes the design
(event-not-sidefile, projection-not-mutation, declared-write-set) so the
implementation can't drift from mu's invariants.

## Open questions

- Checkpoint GC: tie blob lifetime to session-log retention, or a
  separate TTL? (Leaning: same lifetime as the session log — they're
  part of it conceptually.)
- Does a resumed/forked session see its predecessor's checkpoints for
  rewind-across-the-fork? (Leaning: yes, read-only, via the same
  `branched_at_parent_event_id` chain resume already follows.)
- Multi-edit atomicity: if a patch tool touches 5 files and fails on
  file 3, the 5 checkpoints are already logged — rewind-to-just-before
  cleanly restores all 5. Confirm the dispatch path emits all
  checkpoints before any write, not interleaved.
