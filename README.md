# mu

A coding agent toolkit. Backend/frontend split: a JSON-RPC core daemon
(`mu serve`) drives any frontend — TUI (`mu tui`), one-shot CLI
(`mu ask`), or orchestrator (`mu orchestrate`) that coordinates many
core daemons in parallel.

`mu` is the answer the agent gives when the question's premise is wrong.

## Status

MVP working. As of mu-006:

- `mu serve` — JSON-RPC daemon over stdio
- `mu ask "<prompt>"` — one-shot CLI; spawns `mu serve`, sends a
  message, prints the response, exits
- `mu versions` — workspace smoke test
- `AnthropicProvider` — direct Anthropic API (text-only); verified
  end-to-end against the live API (`MU_LIVE_ANTHROPIC=1`)
- `FauxProvider` — echo / scripted responses for tests and dev

Try it:
```sh
echo '{"jsonrpc":"2.0","id":1,"method":"ping","params":null}' | mu serve
mu ask "hello"   # echoes back via FauxProvider; AnthropicProvider needs wiring
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
