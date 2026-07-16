# mesh-slice (mu-wxc4)

Proof of the NATS-backed typed service mesh mu is adopting instead of
reinventing inter-agent messaging as "mail". The **whole architecture**,
proven end-to-end over a live `nats-server`.

## What it demonstrates (all live, 6 tests)

- **L1 typed contract** (`contract.rs`) — a GENERIC envelope `Envelope<C>` /
  `Reply<R>` over any `MeshCommand`; ours, transport-agnostic. Adding a
  service = a new command enum, never a new wire.
- **service** (`service.rs`) — `code_index` as a NATS **Micro** service:
  discoverable by name (`$SRV`), addressed by subject, **no `ip:port`**.
- **proxy** (`proxy.rs`) — mu calls the SAME `recall()`/`status()` surface it
  calls today; the proxy interprets it into request/reply. Caller never sees
  the bus.
- **capability** (`capability.rs`) — a **biscuit** grant rides IN each
  message, verified before any work; non-issuer/wrong-right/empty refused,
  no fail-open.
- **adapter** (`adapter.rs`) — the MCP↔NATS edge: the ONLY MCP-speaking hop.
  CC speaks MCP 1.0; the adapter bridges to the mesh via the same proxy. The
  fleet never speaks MCP internally.
- **agent** (`agent.rs`) — C&C, all three uses: **presence** (who's
  available, via `$SRV`), **DM** (work with another agent), **teams**
  (multicast). One event-driven inbox; every message capability-checked.

## Run

Needs `nats-server` on PATH (each test spawns one, JetStream on):

    cargo test    # 2 capability unit + 4 live end-to-end (service, MCP edge, DM, team)

## Status

The proof is complete. Next is real integration: promote to `crates/mu-mesh`,
wire the mu daemon to register/consume services over the mesh, point
`code_index` at its real backend (off the hardcoded `10.1.1.172:7622`), and
repeat for memory/kx/beadsd. Detached crate (own `[workspace]`) for fast
iteration.
