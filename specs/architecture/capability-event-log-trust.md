# Architecture: putting capabilities in the event log (trust model)

| field      | value                                                              |
| ---------- | ------------------------------------------------------------------ |
| doc_id     | architecture/capability-event-log-trust                            |
| status     | architecture breadcrumb — design agreed, NOT implemented           |
| created    | 2026-06-18                                                         |
| updated    | 2026-06-18                                                         |
| authors    | tcovert + claude-personal (mu-solo session)                        |
| gates      | mu-nqn5 (capability persistence); biscuits-at-MCP-borders work     |
| see also   | [`capability-delegation.md`](capability-delegation.md) (biscuits + tool policy), [`mu-capability-substrate.md`](mu-capability-substrate.md), [`event-sourced-context.md`](event-sourced-context.md), [`memory-hierarchy-and-trust.md`](memory-hierarchy-and-trust.md), [`os-enforced-agent-sandboxing.md`](os-enforced-agent-sandboxing.md) |

## Framing

Standing intent: **capabilities should live in the event log**, like
everything else (capabilities are state; the log is mu's system of
record). They don't today — not by principle, but because capability
persistence (`mu-nqn5`) is unbuilt and `session.resume` fails closed to
`Capability::read_only()` rather than reconstruct authority from a log it
can't yet trust. That fail-closed is *scaffolding*, not the design.

This note is the trust model that has to hold **before** authority goes
into the log, so that feature ships a guarantee instead of an escalation
primitive. It builds on [`capability-delegation.md`](capability-delegation.md)
(capabilities are biscuits; attenuation is narrowing-only; the runtime —
not the model — decides what runs).

## The threat that makes this non-trivial

The event log is a plaintext JSONL file. The adversary is **not**
"someone outside mu" — it's the actors that can already reach the bytes:

- **The model, through its own tools.** A prompt-injected session with a
  `write`/`bash` tool can append a forged event to its own log and wait
  for it to be read. It never has to defeat the RPC/message gate — it
  goes under it, to the store.
- **Bidirectional MCP.** mu both serves and connects MCP; an inbound peer
  of mixed trust is another untrusted message origin.
- **The file directly**, on any shared/multi-tenant/synced store.

So: **gating the message is not securing the action.** The trust boundary
is the *integrity of the store and the provenance of each event*, not the
dispatch check. A capability check at the RPC layer is theatre if the same
actor can write the file the capability is rehydrated from.

Calibration (so we don't over-build): if the attacker owns the operator's
UID or the operator console, the game is already over — they ptrace the
daemon, swap the binary, or just mint a fresh root session. That is **out
of scope by construction**; mu is a footnote to a host compromise. The bar
is to make the *in-UID, model-via-tools / MCP-peer / log-tamper* paths
**expensive and tamper-evident**, not perfect. (tcovert: "a bike lock
isn't to stop the thief — it's to be a harder target than the bike next to
yours." Our stack adds what a bike lock lacks: it leaves a mark when
someone tries.)

## The key insight

Provenance-gating and store-integrity are the **same mechanism at two
times**:

- **write time** — "is this actor allowed to make this capability
  transition?"
- **read/replay time** — "was this event made by an actor allowed to make
  it?"

Answer both by making the capability token **self-authenticating**, rather
than by trusting the channel or the store it arrived through.

## The design (four cheap, composable layers)

1. **Capabilities are Biscuit tokens** (already the chosen primitive — see
   `capability-delegation.md`). A biscuit verifies against the operator
   **root public key**, so it proves itself *wherever it sits* — MCP
   message, tool call, or a line in the JSONL. A forged capability event
   is a biscuit whose signature chain fails → rejected on read. The log
   being writable is irrelevant to authority. Biscuit offline-attenuation
   is narrowing-only and needs no secret.

2. **Fast hash-chain over all events** (blake3/sha256 — integrity, *not*
   encryption; we protect tamper-evidence and ordering, not secrecy). Each
   event commits to its predecessor, so the **past cannot be edited or
   reordered** without breaking every later link.

3. **Seal biscuits for frozen / locked-down sessions.** Sealing is a
   native biscuit operation: a sealed token cannot be attenuated further.
   This *is* "frozen lockdown" — and it closes attenuate-DoS-on-replay: an
   attacker who copies a session's token from the plaintext log cannot
   produce a weaker valid version, only a byte-identical replay (no
   effect). `--lock-down` = seal the root (and only spawn sealed-or-
   narrower children); the subtree's authority is fixed for its lifetime.

4. **Anchored head** (signed head pointer / monotonic counter, persisted
   **outside** the writable log) — defeats **truncation/rollback**. A bare
   hash-chain is tamper-evident against edits but *blind to tail-deletion*:
   the surviving prefix still verifies, and truncating past an attenuation
   or a seal silently **re-widens** authority on replay. The anchor (a
   remembered head hash / sequence number the attacker can't also revert)
   detects it. It lives at mint-key trust level, not log level.

### The line: authority vs. data

A biscuit secures **authority**. Everything else in the log is **untrusted
data with no authority weight** — e.g. a forged `SessionConfigResolved`
carrying `context.soft_limit` is at worst a benign compaction DoS (huge →
context overflow; tiny → thrash). The rule is **carry authority as
biscuits; treat the rest as data**, not "authenticate the whole log."
(This is why the `mu-context-limits-wire` config-value work was safe to
ship through the event path: those events are data, not authority.)

## TCB and key handling

Verification uses the root **public** key — it can live everywhere (every
session, the daemon). The root **private mint key** signs *grants only*,
is needed *rarely*, and the model never needs it (verify and attenuate are
both keyless of the mint secret). So the secret-TCB is small and cold:

> **TCB = daemon process + root mint key + operator auth credential + the
> head anchor.** Minimize and harden that. You cannot capability-your-way
> out of its compromise — every capability system bottoms out at a trusted
> grantor, and that grantor is the authenticated operator origin.

Authority-affecting messages must therefore be gated by **provenance**
(authenticated operator origin), not merely by a capability bit — because
the message bus is multi-origin. mu already has the substrate: `Origin` /
connection identity in the transport, per-connection `AuthState` in the
MCP surface. Inbound model-tools and MCP peers are untrusted *input*: they
may *request* action, never *amplify*. Exactly one origin grants.

## Why both directions of capability change are privileged

The capability-security maxim "attenuation is always safe" is about
**escalation** only — narrowing can't grant new authority. It says nothing
about **availability**: dropping privileges is destructive (zero the
budget, empty the tool set → a bricked session). So a self-attenuate verb,
once it exists, must be **gated**, not ungated-because-it-only-narrows —
otherwise a writable log / injected model can DoS by shrinking. Both
directions are privileged operations: widen → escalation (and impossible
without the mint key), narrow → DoS. Sealing is the clean answer for
sessions that must not change at all.

## Residuals we explicitly accept

- **DoS on a non-sealed session** (an attacker who can write the log can
  append a validly-attenuated weaker token; mitigated by sealing the
  sessions that matter, and by honoring only daemon-minted attenuations).
- **Total console/host/UID compromise** — out of scope (see Calibration).

What a file-writer is reduced to, with the full stack: edit → chain
breaks; forge stronger → biscuit fails (no mint key); attenuate-DoS →
seal blocks; truncate/rollback → head anchor catches. Detectable even
where not preventable.

## What this gates

`mu-nqn5` (capability persistence) must **not** ship "write capability
events to the log" until biscuits + hash-chain + anchored-head exist, or
it ships an escalation primitive. The same model governs the planned
biscuits-at-MCP-borders work: the border and the log are the *same* kind
of boundary, secured by the *same* self-authenticating token — the log
was never special.
