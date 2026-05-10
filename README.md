# mu

A coding agent toolkit. Backend/frontend split: a JSON-RPC core daemon
(`mu serve`) drives any frontend — TUI (`mu tui`), one-shot CLI
(`mu ask`), or orchestrator (`mu orchestrate`) that coordinates many
core daemons in parallel.

`mu` is the answer the agent gives when the question's premise is wrong.

## Status

MVP working through mu-010 — the **read vertical slice** is end-to-
end, including a real LLM and real tool execution.

What runs:
- `mu serve [--provider <name>] [--model <id>] [--tools <csv>]` —
  JSON-RPC daemon over stdio
- `mu ask "<prompt>" [same flags]` — one-shot CLI
- `mu versions` — workspace smoke test

Providers: `faux` (default), `anthropic-api` (real Claude via
`ANTHROPIC_API_KEY`).

Tools: `read` (more pending).

Try it (FreeBSD/macOS/Linux):

```sh
# Smoke test, no API needed:
mu ask "hello"
# → hello

# Real Claude reading a file end-to-end:
export ANTHROPIC_API_KEY=...
mu ask --provider anthropic-api --tools read \
  "Use the read tool to read /etc/hostname. Just the hostname, nothing else."
```

Live integration tests are gated on `MU_LIVE_ANTHROPIC=1` so CI
never spends. Run them locally with:

```sh
MU_LIVE_ANTHROPIC=1 cargo nextest run --test anthropic_read_smoke
```

## Workspace layout

```
crates/
  mu-core/     agent loop, JSON-RPC protocol, transport, state
  mu-ai/       LLM provider abstraction: anthropic, openai, openrouter
  mu-coding/   the binary — modes (rpc/tui/ask), tools, sessions, extensions
```

## Specs

Each spec is in `specs/`. Implementation history:

| Spec | What |
|------|------|
| mu-001 | JSON-RPC 2.0 protocol types |
| mu-002 | stdio transport (newline-framed JSON, concurrent dispatch) |
| mu-003 | Queue-driven agent loop + Provider/Tool traits |
| mu-004 | `mu serve` end-to-end + `FauxProvider` |
| mu-005 | `mu ask` one-shot CLI |
| mu-006 | Anthropic API Provider (text-only v1) |
| mu-007 | `read` tool (first concrete `Tool` impl) |
| mu-008 | Anthropic Provider tool support (request + parsing) |
| mu-009 | `--provider` / `--model` / `--tools` flag wiring |
| mu-010 | end-to-end integration test for the read slice |

Multi-agent build flow: mechanical specs delegated to gpt-pro via
`agent-router`; architectural specs implemented directly. See
`specs/delegations.md` for the ledger.

## Design choices

- **One binary, multiple modes**, dispatched by subcommand. `mu serve` is
  the JSON-RPC daemon; everything else is a frontend that owns one or
  more daemons. Like `git`, not like a daemon-plus-client pair.
- **Async via tokio**. The LLM ecosystem is async-first; fighting that
  costs more than going with it.
- **Built-in MCP servers** for memory and code search, configured the
  same way third-party servers would be — same machinery for first-party
  and external extensions.
- **Sqlite for sessions and state**. Bundled rusqlite so binary deploys
  without runtime deps.
- **Reference, don't fork**. `pi_ts` (`@earendil-works/pi-*`) is the
  architectural blueprint; `pi_agent_rust` is consulted for Rust-specific
  implementation details. Neither is a dependency.

## License

BSD-3-Clause. See [LICENSE](LICENSE).
