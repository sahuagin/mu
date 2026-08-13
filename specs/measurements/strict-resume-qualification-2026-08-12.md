# Strict resume qualification — 2026-08-12

Bead: `mu-strict-resume-qualification-9x39`

## Purpose

Qualify strict resume as the recovery spine before relying on it for the
session-context-orchestration and orchestrator-refinement programs. The expected
contract is: conversation and lineage resume automatically at a provider-sendable
boundary; authority fails closed unless it is regranted through a trusted path.

## Matrix

| Case | Result | Evidence |
|---|---|---|
| Clean completed turn | Pass | `mh4_resume_forks_clean_session_at_tail` |
| Completed tool call/result chain | Pass | `clean_tool_loop_projects_fully` |
| Unanswered tool call | Pass: strict refusal names the ragged event | `ragged_unanswered_tool_call_strict_refuses_with_diagnosis` |
| Ragged tail repair projection | Pass at projection boundary: tail is identified and tombstoned records project cleanly | `recover_path_truncates_ragged_tail_to_boundary`, `tombstoned_ragged_tail_projects_clean` |
| User-visible `recover` command | **Fail: not implemented** | `project_to_clean_boundary` has no production caller; strict refusal now says automated recovery is unavailable |
| Cold capability | Pass: fails closed to read-only | `resume_of_cold_session_does_not_yield_root_authority` |
| Provider selection on resume | Pass: the new head's `SessionCreated` records the selected route; lineage is separate in `HeadAttached` | handler construction and resume smoke test |
| Compacted predecessor | Blocked as expected | `CompactionAssembly` lacks generated summary content; tracked by `mu-session-context-orchestration-rurq.1` |
| Repeated resume / lineage chain | **Bug found and fixed** | second resume previously lost inherited history because `seed_messages` was process-local; new `ContinuationSeeded` event and repeated-resume smoke assertion |
| Cross-daemon / process-restart resolution | Pass for exact references | handler resolves `<events_dir>/<daemon>/<session>.jsonl` directly and never lets a bare-id collision select another daemon; prefix matching remains unimplemented. Follow-up `mu-resume-daemon-resolution-4yca` covers broader identity/UX work |
| CLI operator flow | Qualified only for same-daemon/in-memory resolution | strict errors preserve and diagnose the log but offer no automated repair; successful resume exposes the new session id |

## Recovery-blocking defect fixed

A resumed session received predecessor messages only through in-memory
`AgentConfig::seed_messages`. Its own event log contained `HeadAttached`, but not
the inherited provider-sendable history. Consequently, resuming that head a
second time projected only child-local events and silently discarded all earlier
conversation.

Resume now appends a content-bearing `ContinuationSeeded` event before the new
session is registered. Continuation projection treats that event as the exact
base history and then folds child-local events over it. Strict resume fails closed
when a `HeadAttached` lineage marker lacks a matching seed; the repairing
projection remains able to inspect legacy or partially persisted heads. The seed
is appended durably before `HeadAttached`; strict projection treats every live
`HeadAttached` as requiring a matching earlier seed. Disk-backed session construction writes and syncs the complete
bootstrap to a `.jsonl.pending` file, then atomically renames it into the
session-discovery namespace and syncs the parent directory before registration.
Failures before publication leave no artifact (best-effort cleanup); a failure
to sync and roll back an already-published coherent file may leave that final
file discoverable, but the session is not registered and the durability error
names both failures. Ephemeral
in-memory sessions preserve order without claiming persistence. Legacy
`HeadAttached` records remain wire-compatible, but strict projection now refuses
every lineage-bearing head without a matching seed rather than silently
projecting child-local history. This makes resume transitive across repeated daemon/session heads while preserving the existing strict ragged-tail checks. An on-disk JSONL
round-trip test covers the new event's durability boundary.

## Remaining boundaries

- Compacted sessions cannot be faithfully reconstructed until generated
  compaction summaries and output manifests are durable
  (`mu-session-context-orchestration-rurq.1`).
- Capability authority cannot be recovered from unsigned JSONL. Cold resume
  correctly remains read-only pending a trusted explicit regrant path.
- Exact cross-daemon resolution is qualified: the handler resolves the supplied
  daemon/session pair directly and applies the read-only capability floor to a
  non-local daemon. Prefix matching and broader identity UX remain tracked by
  `mu-resume-daemon-resolution-4yca`.
- `ContinuationSeeded` intentionally stores one provider-sendable context
  snapshot as a single JSONL event. Resume chains therefore duplicate bounded
  provider context across heads, and line parsing/rendering cost scales with that
  snapshot. Chunking or content-addressing is future storage hardening.
- The user-visible `mu recover` command does not exist yet. Strict refusal now
  says so explicitly. `project_to_clean_boundary` and tombstone behavior are
  tested substrate, not a working operator recovery path. This is
  the next recovery-spine blocker and requires its own implementation bead.
- The full process-restart matrix still needs a disk-backed serve fixture; the
  current smoke server deliberately runs without an events directory.

## Verification

```text
cargo test -p mu-core agent::continuation -- --nocapture
cargo test -p mu-coding resume_of_cold_session_does_not_yield_root_authority -- --nocapture
cargo test -p mu-coding --test serve_smoke mh4_resume -- --nocapture
just ci
```
