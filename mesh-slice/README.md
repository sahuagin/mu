# mesh-slice (mu-wxc4)

Proof crate for the NATS-backed typed service mesh mu is adopting instead of
reinventing inter-agent messaging as "mail". Detached crate (own `[workspace]`)
for fast iteration.

**Design of record:** [`specs/architecture/mesh-nats-adoption.md`](../specs/architecture/mesh-nats-adoption.md)
— the why, the layers, the C&C model, and the daemon-integration seams. This
README is build/run only.

## What's here (all live, 6 tests)

- `contract.rs` — L1 typed contract: generic `Envelope<C>` / `Reply<R>`.
- `service.rs` — `code_index` as a NATS Micro service (discoverable, subject-addressed).
- `proxy.rs` — the abstraction seam: mu calls today's `recall()`/`status()`.
- `capability.rs` — a biscuit grant verified per message.
- `adapter.rs` — the MCP↔NATS edge (the only MCP-speaking hop).
- `agent.rs` — C&C: presence, DM, teams over one event-driven inbox.

## Run

Needs `nats-server` on PATH (each test spawns one, JetStream on):

    cargo test    # 2 capability unit + 4 live end-to-end (service, MCP edge, DM, team)
