# RFC: Recall provenance + redacted-tombstone audit

**Status:** Draft (RFC — request for comments)
**Bead:** `mu-recall-provenance-audit-vnc9` (epic)
**Date:** 2026-06-08
**Related:** `mu-recall-operator-controls-5y6a` (recall controls), `mu-8stm` (self-classified-authority / security posture), `mu-w4o` (biscuit capability tokens), mu-phl (content-addressable memory)

## Problem

mu's event log records the **conversation** (user/assistant/tool events), not the assembled **context**. The recall / system-prompt content — the identity kernel from `agent memory context`, the project files (`MU.md`/`AGENTS.md`), the bootloader preamble, ~5k tokens total — is assembled into the in-memory rope and sent to the model, but is **never written to the log as an event** (verified 2026-06-08: 2 of 2405 session logs contain the identity kernel, and those are sessions that explicitly *dumped* their context; the automatic injection leaves no trace).

Two failures fall out of the same gap:

1. **No reconstruction.** You cannot replay "what was in the model's window at turn N." The log is an incomplete record of what the model saw.
2. **Undetectable influence.** Anything that reaches `agent.sqlite` shapes a session with **no audit trace**. That's a context-injection channel with no detection — the sibling, one layer down, of the self-classified-authority class in `mu-8stm`: not "what did the session claim it could do," but "what shaped what it wanted to do."

These sit under **competing requirements**:

- **Privacy / data-minimization** — don't spread personal or secret content (the identity kernel; a leaked key) into every session transcript on disk. (The current no-logging behavior accidentally satisfies this.)
- **Auditability** — you must be able to know what influenced a session, which naively requires logging the injected content. (The current behavior fails this.)

They look irreconcilable. They are not.

## Resolution: provenance without content

Log a **reference** per injected span — `{source, content-hash, token-count}` — not the text. mu already computes a blake3 `stable_id` per `RecalledItem` (`memory-<hash>` is a content hash; `file-<hash>` hashes the canonical *path* — the rope-dedup identity — so P0 carries a separate full-width `content_hash` field for the actual tamper-evidence). It simply never reached an event. Emit it.

This gives detection (every injection has a logged fingerprint), tamper-evidence (resolve the hash later: match → verbatim, mismatch → you *know* it changed and what), and no content spread (the bytes stay in one place; logs carry hashes).

## Core principles

1. **Provenance, not content.** For sensitive spans, the log holds the ref, never the text.

2. **Redacted-tombstone.** A sensitive span appears in the log as a self-describing redacted marker carrying the ref plus a removal/compaction-protection marker. The design line is **parse-open, resolve-gated**: any consumer (a `grep`, the cc-console, an external analytics pass) can read the log, see `[redacted span, ref=<hash>, source=<type>]`, and skip it without choking; only a tool with the capability *and* store access follows the ref to the bytes. The log becomes freely shareable. This extends mu's existing `Tombstone` event payload (today: compensating-over-poisoned-record / resume-head-attach) to a new use: a redacted-but-resolvable injected span.

3. **Retention-pin.** A content-version referenced by any event log is protected from GC/compaction, so "the data that was there at the time" stays resolvable. Without this, the ref is best-effort: edit/GC the source and you keep the hash + source but lose the bytes. The pin is a property of the store, bridging to the mu-phl v1 content-addressable / `MemoryIngest` direction.

4. **Strike-through, not erase (the field-journal rule).** The log is append-only — pencil, never erased. *True* deletion of a secret (a leaked key, a password) is permitted **only as a recorded strike event** carrying **action + authority + reason** — the surveyor's "you may strike through, but you must initial it." The strike releases the retention-pin and purges the referenced bytes, but the **strike record persists forever**: you always know that something was there, who removed it, when, and why. This is the primitive that reconciles immutable audit retention with genuine secret-erasure / right-to-erasure — both in one mechanism.

5. **Redaction policy rides the memory selector/scope.** Not everything injected is sensitive: `MU.md`/`AGENTS.md`/bootloader spans are fine in the clear; the identity kernel is not. Redact-vs-plain keys off the source's sensitivity/scope tag — the same selector machinery that scopes injection — so a `personal`-scoped memory's provenance entry is a redacted-tombstone while a file/`shared` span logs its ref (or content) plainly. One policy, driven by metadata already present.

6. **Recall is an event through the single chokepoint.** Recall must stop being a direct side-effect that assembles content into the rope. Instead it **emits a recall event into the input queue**, like every other event. The **write process is the chokepoint**: it detects recall/sensitive events, lays down the redacted marker + provenance, and stores the secured content capability-gated + retention-pinned. Injection into the session context happens at **process-time, under a read capability**. The facility to **modify** memory is **split from** recall — distinct facilities, distinct capabilities (least privilege; recall is read-only on memory). The chokepoint + emit-everything discipline is what makes "no undetectable injection" *structural* rather than conventional — there is no path to context that does not pass the instrumented writer.

## Invariants

- All context entering the rope flows through the instrumented assembly — no side-doors. Every injection has a provenance record.
- The event log is freely shareable: sensitive spans are redacted-tombstoned; resolution is capability-gated.
- Append-only / strike-through-not-erase. Deletion is itself an authorized, reasoned, recorded event.
- Recall is read-only with respect to memory; modification is a separate capability.

## Phasing (careful steps; plan before each)

This is an epic. The cheap audit/privacy win does **not** require the architectural refactor.

- **P0 — provenance logging (cheap, additive, no architecture change).** Emit `source + content-hash + tokens` for *today's* synchronous recall (the `stable_id` is already computed), and define the redacted-tombstone format for sensitive spans. Immediate detection + shareable-log win, no agent-loop change. **Shipped** (bead `mu-recall-provenance-audit-vnc9.1`): the `recall_provenance` event — one per session creation, emitted at `build_and_register_session` before the session becomes observable; `Memory` spans are redacted-tombstoned (`hash + source-type`, no name), file/bootloader spans carry their ref plus name in the clear.
- **P1 — retention-pin.** Protect content-versions referenced by logs from GC/compaction. Store-side; ties to mu-phl v1.
- **P2 — strike-through deletion.** Authorized + reasoned redaction (strike) events; release pin + purge bytes + keep the strike record.
- **P3 — capability-gated resolution.** A biscuit capability to follow tombstone refs to the bytes (`mu-w4o`).
- **P4 — recall-as-event-through-queue (endgame).** Recall emits events; the writer enforces redaction/provenance at the chokepoint; recall/modify split. The agent-loop/startup context-assembly refactor.

## Open choices (to settle before the gated phases)

- **Authorization model:** store-access-as-authorization (simplest — read on the store = resolve) vs a biscuit capability gating resolution (`mu-w4o`).
- **Retention timing:** commit to the retention-pin now (a store change, bigger) vs ship P0 tombstone+ref as best-effort verbatim first.
- **Redaction scope:** redact everything uniformly vs redact-by-sensitivity-tag (principle 5).
- **Provenance granularity:** `hash + source-type` (privacy-max; learning *which* source requires resolving against the store) vs `hash + name` (audit-easy; a sensitive name like `thaddeus-political-views` is itself a minor leak). Default lean: hash + source-type, names only for non-sensitive scopes.

## Why this is the right shape

The two requirements that looked like a binary choice — privacy vs audit — are resolved by separating **existence/provenance** (always logged, freely shareable) from **content** (referenced, gated, retention-pinned, strike-deletable). You give up exactly one thing: verbatim reconstruction of content that has been *authorizedly struck* — and even then you retain the record that it existed and was removed. That is the correct trade for a system that must be both an immutable audit log and a place secrets can be genuinely erased.
