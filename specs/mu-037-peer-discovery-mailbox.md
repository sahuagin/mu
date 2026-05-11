# Spec: peer discovery + mailbox — cooperating mu sessions

| field      | value                                       |
| ---------- | ------------------------------------------- |
| spec_id    | mu-037                                      |
| status     | proposed (sketch)                           |
| created    | 2026-05-11                                  |
| updated    | 2026-05-11                                  |
| authors    | tcovert + claude-personal (claude-opus-4.7) |
| supersedes | none                                        |

## Why

Today mu sessions are **isolated**. A session has a parent (via `session.delegate`, mu-031), an event log, and a capability — but no way to find or talk to sessions running in another mu daemon, or even in the same daemon as a peer rather than a parent/child.

The cooperating-sessions story Thaddeus has been sketching ("if you were to 'discover' me and we negotiated a communication path") is genuinely a different kind of work from the parent/delegate tree. Two agents — possibly in different daemons, possibly under different humans' control — find each other, negotiate trust + capability, and exchange messages without either being the other's parent.

This is the substrate for:

- **Bidirectional collaboration.** My mu session and Thaddeus's mu session can exchange notes, hand off work, or grade each other's output (the `DelegateGrader` from mu-036 generalised across daemons).
- **Agent-mesh patterns.** A "research mu" that watches a directory and notifies a "writer mu" when interesting state changes. No human in the message loop.
- **Cross-machine mu.** A mu running on the work laptop posts a mailbox message to a mu on the personal box; the receiver picks it up next session start.
- **The mockup's F9 view.** `mu-ui-mockups-2026-05-10.md` already has a "mailbox" pane assuming this primitive exists.

This spec is intentionally a **sketch**. The earlier specs in this set (mu-035, mu-036) propose well-defined wire surfaces with implementation details; mu-037 names the problem, marks out the design axes, and proposes a minimal viable version. Implementation is multi-week and probably wants prototyping before committing to a wire shape.

CONVENTIONS apply.

## Design axes (where the choices live)

### 1. Discovery — how do two mus find each other?

Several options, each with tradeoffs:

- **Filesystem registry.** Each `mu serve` writes a `~/.local/share/mu/daemons/<pid>.toml` file at startup with its socket path, capability advertisement, and human identity. Discovery = enumerate the directory. **Pros:** trivial, observable (just `ls`), works across users on the same box via `/var/run`. **Cons:** local-machine only, no cross-machine.
- **mDNS / Bonjour.** Each daemon advertises a `_mu._tcp.local.` service. Discovery via standard mDNS browse. **Pros:** cross-machine on the same LAN; battle-tested. **Cons:** dependency on Avahi/Bonjour; tooling on FreeBSD is decent but not zero-config; firewalls.
- **Central rendezvous service.** Daemons register with a known URL; clients query the URL. **Pros:** works anywhere. **Cons:** centralised; trust complications; we'd have to run it.
- **Shared SQLite registry.** Use `~/.local/share/agent.sqlite` (already shared across claude-personal + claude + pi) as the registry table. **Pros:** zero new infra; aligns with existing shared state. **Cons:** local-machine, requires file-lock discipline.

**Proposed v1:** filesystem registry under `~/.local/share/mu/daemons/`. Local-only at first; mDNS as a follow-up. Cross-machine via a federation primitive is a separate spec.

### 2. Transport — once discovered, how do they talk?

- **Unix domain socket** per daemon, advertised in the registry entry. JSON-RPC over the socket, same protocol mu already speaks over stdio. **Pros:** reuses 100% of existing wire code; minimal new attack surface. **Cons:** local-only.
- **HTTP + WebSocket** for cross-machine. Each daemon optionally binds an HTTP server with TLS; auth via tokens.

**Proposed v1:** Unix domain socket. Same `mu serve` binary gets a `--listen <path>` flag that, in addition to stdio, also accepts JSON-RPC connections from the socket. Discovery + socket path = a working peer connection.

### 3. Handshake — what does "negotiate a communication path" look like?

When peer A wants to talk to peer B, A connects to B's socket and sends a `peer.hello` request:

```jsonc
{
  "jsonrpc": "2.0", "id": 1,
  "method": "peer.hello",
  "params": {
    "from": {
      "daemon_id": "8f2c…",
      "human_identity": "tcovert@sahuagin.net",
      "session_id": "session-3",
      "advertised_capabilities": ["read_only", "summarise", "grade"]
    },
    "want": {
      "method": "ask_session_as_grader",
      "scope": "spec-summary"
    }
  }
}
```

Peer B's policy decides:
- accept (return a capability-attenuated handle to a specific local session — the *channel* through which A can call B)
- challenge (request more identity proof; loop)
- deny (return a structured refusal with a reason; A can retry later)

The accepted handle is itself a **session-like primitive with a capability**: A can call methods on B's session within the capability, just like a delegate but the parent-child link is replaced by a peer link in the event log.

### 4. Capability — what can the peer actually do?

Reuses mu-033 `Capability` entirely. The handle B returns to A is just an attenuated capability: "you may call `session.ask` against session-N with these tools and this budget." All existing attenuation/enforcement code applies; no new authorisation model.

A peer-issued capability is **never wider** than the receiving session's own capability. This is the same intersection property mu-031 already enforces for delegates, applied to peer handles.

### 5. Mailbox — for async, fire-and-forget messages

Some cooperation is request/response (peer A asks peer B; B replies). Other cooperation is post-and-forget (peer A writes to peer B's mailbox; B reads it whenever). The mockup at F9 ("mailbox") is the UI for the post-and-forget case.

Wire surface:

```jsonc
// peer A → peer B
{
  "jsonrpc": "2.0", "id": 5,
  "method": "mailbox.post",
  "params": {
    "to_session_id": "session-N",
    "from": { "daemon_id": "…", "session_id": "…" },
    "kind": "callout|task|fyi|file_reference|grader_result",
    "subject": "Spec mu-022 ready for review",
    "body": { /* shape varies by kind */ },
    "expires_at_unix_ms": null
  }
}

// peer B
{
  "jsonrpc": "2.0", "id": 6,
  "method": "mailbox.list",
  "params": { "session_id": "session-N", "since_unix_ms": null }
}
```

Mailbox is **persistent per-session** (lives in the session's event log as `MailboxMessage` entries). A session that wakes from sleep can pull its mailbox to see what happened while it was idle. This composes very naturally with mu-036's autonomous-loop primitive: the autonomous session checks its mailbox each iteration, and if there's a new message, takes it into account.

## Minimal viable version

Phase 1 (single daemon, multiple sessions):
- **No discovery yet** — peers reach each other by `session_id` directly (they're in the same daemon).
- Add `peer.hello` / `peer.reply` request pair within a single daemon, returning a peer-handle keyed by `peer_session_id + token`.
- Add `mailbox.post` / `mailbox.list` / `mailbox.consume` RPCs.
- Add `MailboxMessage` event-log entry.
- Demonstrate via two delegate sessions exchanging grader-style messages without going through the parent.

Phase 2 (cross-daemon, same machine):
- `mu serve --listen ~/.local/share/mu/daemons/<id>.sock` flag.
- Filesystem registry write at startup, cleanup at SIGTERM.
- `mu peers list` CLI command that enumerates the registry.
- `peer.hello` over the unix socket.

Phase 3 (mDNS cross-machine):
- Optional `--advertise` flag turns on mDNS service publication.
- Trust model: explicit allowlist of peer daemon IDs.

Phase 4 (TUI integration):
- F9 mailbox view from `mu-ui-mockups-2026-05-10.md`.
- Live updates via subscribing to `MailboxMessage` events.

## Composition with previously landed specs

- **mu-029 (session.input_required):** an incoming `mailbox.post` of kind `task` could automatically trigger an `input_required` to the receiving session's human (if there is one). Lets remote agents ask for help with a clean wire shape.
- **mu-031 (session.delegate):** delegate creates a child; peer.hello creates a sibling-like link. The event log distinguishes the two.
- **mu-033 (Capability):** peer handles are capability-attenuated. Re-uses the entire enforcement machinery; no new authz code.
- **mu-035 (provider_status):** unchanged. A peer-issued ask shows up in the receiving session's stream the same as a local ask.
- **mu-036 (autonomous loop):** the DelegateGrader pattern naturally generalises — the grader can be a peer rather than a delegate. Cross-agent grading becomes available without changing autonomous-loop wire shape.

## Risks and open questions

- **Identity and trust.** Currently mu has no notion of "who is this peer." Phase 2+ needs a model (human identity from auth? per-daemon public key? capability-as-credential like biscuit?). The "Caja / macaroon / biscuit" tradition argued for in `specs/architecture/capability-delegation.md` is the obvious direction.
- **Concurrency and ordering.** Two peers writing to the same mailbox simultaneously is fine (event log appends), but the receiving session needs a consistent read view. Use a per-session message sequence number.
- **Spam / DoS.** A misconfigured peer could flood another peer's mailbox. v1 has no rate limiting; phase 3 needs it.
- **Capability revocation.** Once a peer holds a capability handle, how does the issuer revoke it? Today mu has no revocation primitive; this spec inherits that gap.
- **Cross-machine clock skew.** `expires_at_unix_ms` on mailbox messages requires synchronised clocks. NTP is the practical answer; documentation should call this out.

## Status

Sketch — design axes named, minimal viable version proposed. Implementation is at least Phase 1 (single-daemon peer/mailbox primitive) of multi-week work; ordering against mu-035 and mu-036 implementation needs a real-world sanity check. Recommend prototyping `peer.hello` + `mailbox.post` in a feature branch before locking the wire shape.

The spec exists so the "agent-mesh" direction is on paper and we know where the F9 mockup's data is supposed to come from. Closes the original Thread B that motivated the night's work.
