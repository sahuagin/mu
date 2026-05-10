# mu

A coding agent toolkit. Backend/frontend split: a JSON-RPC core daemon
(`mu serve`) drives any frontend — TUI (`mu tui`), one-shot CLI
(`mu ask`), or orchestrator (`mu orchestrate`) that coordinates many
core daemons in parallel.

`mu` is the answer the agent gives when the question's premise is wrong.

## Status

Pre-MVP. Workspace scaffolded, no working binary yet.

## Workspace layout

```
crates/
  mu-core/     agent loop, JSON-RPC protocol, transport, state
  mu-ai/       LLM provider abstraction: anthropic, openai, openrouter
  mu-coding/   the binary — modes (rpc/tui/ask), tools, sessions, extensions
```

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
