# Mesh: NATS-backed typed service mesh (mu-wxc4)

Status: adopted direction; first slice landed (proof crate `mesh-slice/` +
daemon transport adapter `crates/mu-coding/src/serve/mesh.rs`).

Supersedes the compose-your-own substrate path
([messaging-substrate-composition.md](messaging-substrate-composition.md),
Aeron+SBE+bespoke discovery). Operator directive (2026-07-16): adopt an
existing bus that already solves presence, discovery, addressing, directed +
broadcast/multicast, and store-and-forward; extend it to the other services;
do **not** reinvent it as "mail". MCP stays only at the foreign (CC) edge.

## Why NATS

The bus must already solve the things inter-agent messaging keeps
reinventing. NATS provides, off the shelf:

- **Service discovery + presence** — NATS Micro (`$SRV.PING`/`INFO`). A
  service registering IS its presence signal; no roster we maintain.
- **Subject addressing** — services and agents are addressed by *subject*
  (`mu.svc.<name>`, `mu.agent.<id>.dm`, `mu.team.<team>`), never `ip:port`.
  This retires the hardcoded `10.1.1.172:<port>` endpoints.
- **Request/reply and pub/sub fanout** — directed calls and team multicast on
  one substrate.
- **JetStream** — durable store-and-forward / replay when a service wants it.
- Mature Rust client (`async-nats`), MCP-bridgeable at the edge.

## Layers

1. **L1 typed contract** (ours, transport-agnostic — "MCP 2.0"). A generic
   `Envelope<C>` / `Reply<R>` over a `MeshCommand` trait. Adding a service is a
   new command enum, never a new wire format. Carried as an opaque payload on
   whatever bus subject addresses the peer.

2. **Capabilities in-band.** A biscuit grant rides IN each request and is
   verified before any work — attenuable, offline-verifiable, no fail-open. A
   grant not signed by the issuer is refused/dropped, never served. This is the
   mesh's native auth, distinct from the daemon's connection-scoped bearer
   handshake (see "Daemon integration" caveat).

3. **Service = NATS Micro service.** Discoverable by name, addressed by
   subject. `serve()` decodes → authorizes → runs → encodes.

4. **Proxy = the abstraction seam.** mu calls the SAME surface it calls today
   (e.g. `code_index.recall()/status()`); the proxy interprets that into
   directed request/reply and relays the typed result back. **The caller never
   sees the bus.** A memory call stays a memory call; the service layer decides
   it means "directed 1-1 to the memory service, relay the reply".

5. **MCP only at the foreign edge.** CC speaks MCP 1.0; an MCP↔NATS adapter is
   the ONLY MCP-speaking hop. The fleet never speaks MCP internally.

## Command & control (agents as first-class peers)

Three operator uses, done natively over the mesh (event-driven, fire-and-
forget — what PR #492 wrongly bolted onto MCP):

1. **Who is available** — presence via `$SRV`.
2. **Work with another agent** — a directed message (DM) to `mu.agent.<id>.dm`.
3. **Launch a team** — join `mu.team.<team>` and multicast to its members.

All inbound traffic (DMs + team messages) arrives on ONE event stream; every
message is capability-checked before delivery.

## Daemon integration (the mu-046 seams — no side channels)

mu is a black box to the bus: it only reads requests from, and writes replies
to, subjects. The transport adapter (`serve/mesh.rs`) is a first-class peer of
the stdio (#1) and MCP (#2) adapters, integrated through the SAME mu-046 seams
— **not** side-channel injection, polling, or a second consumer on the
outbound queue:

- **Inbound** crosses `pipeline::ingest` — journaled + sequenced at the one
  border — becoming an ordinary command.
- **Outbound** rides an outbound `Router` lane the adapter registers and is the
  SOLE consumer of, routed per `request_id → reply subject`. Even immediate
  rejects take the one egress path.
- Config-gated (`[mesh].enabled`); the adapter handle aborts its tasks on drop,
  tying it to the daemon shutdown cascade.

**Auth posture (fail-closed, mu-iqo8).** The adapter serves ONE auth state and
ONE Router lane for ALL peers multiplexed on the subject, so per-connection
auth cannot isolate them — once any peer authenticated, every peer would be
authorized. Rather than ship that bypass, the daemon **fails closed**: when an
*enforcing* auth mechanism is configured, `spawn_mesh_adapter` refuses to
expose the mesh (logs an error, runs without it). The mesh therefore runs only
where the daemon is already pre-authenticated (no enforcing mechanism — a
single-operator / trusted-network deployment). Multi-peer auth on an enforcing
daemon is per-request biscuit capabilities (layer 2) + per-peer identity, and
lands with mu-iqo8.

**Reply correlation.** JSON-RPC ids are client-local, but the subject
multiplexes peers onto one Router lane keyed by request id. Each inbound
request is rewritten to a unique per-adapter correlation id for its pipeline
hop (so two peers' `id: 1` cannot collide); the client's own id is restored on
the reply. The pending correlation→(id, reply-subject) map is bounded so
unanswered requests cannot leak.

## Promotion path

`mesh-slice/` is a detached proof crate (own `[workspace]`) for fast
iteration. Next: promote to `crates/mu-mesh`; reconcile biscuit vs mu's
`Capability` model; point `code_index` at its real backend (off the hardcoded
endpoint); repeat for memory/kx/beadsd; deploy the CC-facing MCP↔NATS edge.
