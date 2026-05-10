# Delegation: implement mu-002

This file is the prompt sent to a sub-agent (currently `agent-router
--auth codex-oauth`) to implement spec `mu-002`. It is committed to
the repo so the prompt itself is reviewable.

---

You are working in the `mu` Rust workspace at
`/home/tcovert/src/public_github/mu`. The previous spec (`mu-001`) is
already implemented at `crates/mu-core/src/protocol.rs`. Your job is to
add the transport that frames JSON-RPC over stdin/stdout and runs the
dispatch loop.

## Read first (REQUIRED)

1. `specs/mu-002-stdio-transport.md` — the full specification. Primary
   directive. §Invariants are hard constraints, §Behaviors are test
   requirements, §Interfaces is the exact code shape, §Out-of-circuit
   warnings are bug-prevention notes — especially OOC-2 (stdout
   buffering) and OOC-3 (Send bounds on the handler).
2. `crates/mu-core/src/protocol.rs` — the types your transport will
   move. Read it once so you know what `Request<P>`, `Response<R>`,
   `Notification<P>`, and `ErrorObject` look like.
3. `crates/mu-core/src/lib.rs` — you will add ONE line: `pub mod
   transport;`. Don't touch the existing `pub mod protocol;` line.
4. `AGENTS.md` (root) — project-wide rules.
5. `Cargo.toml` (root) — confirms which workspace deps are available.
   You may NOT add new ones.

## Deliverable

Two files modified, no other changes:

- **`crates/mu-core/src/transport.rs`** (new) — implementing the
  §Interfaces block from the spec. The `serve` function body is your
  judgment call within the §Invariants and §Behaviors; the spec
  intentionally leaves "implementer chooses how to structure
  read-task / writer-task / dispatcher within these constraints"
  open. Tests covering every behavior B-1 through B-8 in
  `#[cfg(test)] mod tests`.
- **`crates/mu-core/src/lib.rs`** (modified) — add `pub mod
  transport;`. Don't reorder, don't reformat anything else.

## Tests use `tokio::io::duplex`

For B-1..B-6, B-8 you'll want bidirectional in-memory pipes. The
shape:

```rust
let (mut client, server) = tokio::io::duplex(64 * 1024);
let server_reader = tokio::io::BufReader::new(server);  // see OOC-4

// Write a request from "client" side
let req = json!({"jsonrpc":"2.0","id":1,"method":"ping","params":null});
client.write_all(format!("{req}\n").as_bytes()).await.unwrap();

// In a tokio::spawn task, run serve(server_reader, client_writer, handler)
// (you'll need to split duplex's two halves; see test below)

// Read the response line back from "client" side
let mut buf = String::new();
client_lines.read_line(&mut buf).await.unwrap();
let resp: serde_json::Value = serde_json::from_str(&buf).unwrap();
assert_eq!(resp["id"], 1);
```

For a fully-isolated test you'll likely need TWO duplex pairs (one for
"client → server" stdin direction, one for "server → client" stdout
direction). Build a small test helper that returns
`(client_writer, client_reader, serve_future)` and reuse it across B-1..B-8.

## Verification (run before declaring done)

```sh
cd /home/tcovert/src/public_github/mu
cargo build -p mu-core
cargo nextest run -p mu-core
wc -l crates/mu-core/src/transport.rs    # under 800
grep -E '\bunsafe\b|\.unwrap\(\)|\.expect\(' crates/mu-core/src/transport.rs \
  | grep -v '^[[:space:]]*//' \
  | grep -v 'cfg(test)'
# ^ should print nothing outside test modules
grep -E '^(tree-sitter|tree-sitter-)' crates/mu-core/Cargo.toml
# ^ should print nothing — no new deps added
```

All checks must pass:
1. Build clean, no NEW warnings (existing pre-mu-002 warnings, if any,
   are outside scope)
2. All tests green: B-1..B-8 plus existing `version_is_nonempty` and
   the 10 `protocol::tests::*` tests = at least 19 tests total
3. Transport module under 800 lines including tests
4. No new dependencies in `Cargo.toml`
5. No `unsafe`, no `unwrap`/`expect` outside test modules

## What NOT to do

- Don't add new dependencies. The spec's §INV-8 lists exactly what
  you may use: tokio (full), serde, serde_json, thiserror, tracing,
  async-trait, plus stdlib. `static_assertions` was *suggested* in
  §B-7 but it's not in `Cargo.toml`; use the hand-rolled assert_send
  pattern instead.
- Don't add a `Router` / `Handler` trait. The spec is explicit: the
  caller dispatches by method string inside their handler closure.
  Trait machinery is bigger than this spec wants. (See §Scope `Out:`
  bullet "Per-method handler trait.")
- Don't touch `crates/mu-core/src/protocol.rs`. It's the contract;
  changes there require a new spec or an amendment to mu-001.
- Don't introduce sync mutex types (`std::sync::Mutex`,
  `parking_lot::Mutex`) — the spec mandates a single mpsc channel for
  outbound. Locks are not needed and would be a code smell.
- Don't structure the implementation as multiple sub-modules
  (`transport/codes.rs`, `transport/error.rs`, etc.). One file. §INV-6.
- Don't try to make `serve` cancel-safe via `select!`. OQ-1 in the
  spec says explicit cancellation is deferred; close stdin to stop.

## Output protocol

When done, your final message is the JSON envelope:

```json
{
  "status": "complete",
  "files_changed": ["crates/mu-core/src/transport.rs",
                    "crates/mu-core/src/lib.rs"],
  "tests_added": ["round_trip_request", "..."],
  "spec_coverage": {
    "B-1": "<test name(s)>",
    "B-2": "...",
    "B-3": "...",
    "B-4": "...",
    "B-5": "...",
    "B-6": "...",
    "B-7": "...",
    "B-8": "..."
  },
  "verification_results": {
    "build": "clean",
    "tests_passed": <N>,
    "module_lines": <N>,
    "no_new_deps": true,
    "grep_unsafe_unwrap_outside_tests": "empty"
  },
  "design_notes": "<brief description of the read-task/writer-task structure you chose, since the spec left it open>",
  "notes": "<anything surprising or worth flagging for review>"
}
```

If you hit a blocker (compiler error you can't resolve in 3 attempts,
spec ambiguity, behavior you can't make pass), use
`status: "blocked"` and explain in `notes`.
