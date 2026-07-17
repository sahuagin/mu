# mesh-slice (mu-wxc4)

Proof crate for the NATS-backed typed service mesh mu is adopting instead of
reinventing inter-agent messaging as "mail". Detached crate (own `[workspace]`)
for fast iteration.

**Design of record:** [`specs/architecture/mesh-nats-adoption.md`](../specs/architecture/mesh-nats-adoption.md)
— the why, the layers, the C&C model, and the daemon-integration seams. This
README is build/run only.

## What's here (all live, 7 tests)

- `contract.rs` — L1 typed contract: generic `Envelope<C>` / `Reply<R>`.
- `service.rs` — `code_index` as a NATS Micro service (discoverable, subject-addressed).
- `proxy.rs` — the abstraction seam: mu calls today's `recall()`/`status()`.
- `capability.rs` — a biscuit grant verified per message.
- `adapter.rs` — the MCP↔NATS edge (the only MCP-speaking hop).
- `agent.rs` — C&C: presence, DM, teams over one event-driven inbox.
- `bin/bridge.rs` — **`mu-mesh-bridge`**: a deployable stdio MCP server CC
  launches as a subprocess; bridges `code_recall`/`code_status` to the mesh.

## The CC bridge (`mu-mesh-bridge`)

The deployable CC-side edge: a stdio MCP server CC launches, which relays tool
calls to the mesh via `adapter.rs` + `proxy.rs`. Point CC at it in
`~/.claude.json`:

    "mu-mesh": {
      "command": "/path/to/mu-mesh-bridge",
      "args": ["--nats-url", "127.0.0.1:4222"],
      "env": { "MU_MESH_ISSUER_KEY": "<hex Ed25519 private key>" }
    }

`MU_MESH_ISSUER_KEY` (hex) is the key the bridge mints request capabilities
with; the mesh services must trust its public key. Unset → an ephemeral key is
generated and its public key logged (dev / single-tenant). `tests/bridge_smoke.rs`
launches the real binary the same way and calls `code_recall` through it.

## Run

Needs `nats-server` (each test spawns one, JetStream on) — resolved from
`$NATS_BIN` or `nats-server` on `PATH`:

    cargo test    # 2 capability unit + 4 live end-to-end + 1 bridge subprocess
