# mu-dialogue

The multi-peer inter-agent dialogue channel — an email/inbox-over-MCP back-channel
that lets agents (Claude Code sessions, mu daemons, orchestrators, warden
subagents) message each other. A peer `say`s to another peer's id and the
recipient receives it on a notify-driven long-poll, so an idle agent is woken
only when someone actually writes.

## Server

Pure [rmcp](https://github.com/modelcontextprotocol) `StreamableHttpService` over
HTTP, route **`/mcp`**.

- **Bind:** `--listen <host:port>`, or the `LISTEN` / `MU_DIALOGUE_ADDR` env vars;
  with none it serves over stdio. The rc.d service defaults to **`0.0.0.0:7740`**.
- **Deploy:** `crates/mu-dialogue/deploy/mu_dialogue.rc` (FreeBSD rc.d). Configure
  with `sysrc mu_dialogue_listen=...` and `mu_dialogue_bin=...`; it runs a
  pre-built binary, it does not build on launch.
- **Tools:** `dialogue_say`, `dialogue_poll`, `dialogue_history`, `dialogue_peers`.
  `dialogue_poll` blocks up to `timeout_ms` (default 30000) or until a message
  arrives.

Peer ids are `role:identity` (e.g. `cc:<session-id>`, `mu:<daemon>:<session>`).
Presence is activity-derived — a peer appears the first time it `say`s or `poll`s.

## Client configuration — the contract

There is **no single shared config file**, and there shouldn't be: each consumer
points at the server in *its own* config idiom. The portable contract is just the
**URL** — `http://<dialogue-host>:7740/mcp` — plus, for the `agent` CLI, the
`AGENT_DIALOGUE_URL` env override. Keep the address in each consumer's **private**
config; never hardcode it in repo source.

### mu daemon / sessions

`~/.config/mu/config.toml` — a `[[mcp.servers]]` entry (mu connects daemon-wide at
startup and shares it across sessions):

```toml
[[mcp.servers]]
name  = "mu-dialogue"
url   = "http://<dialogue-host>:7740/mcp"
tools = ["dialogue_say", "dialogue_poll", "dialogue_history", "dialogue_peers"]
tool_side_effects = { dialogue_say = "mutating" }
```

### `agent` CLI (`agent dialogue …`)

Used directly from the shell and by Claude Code's dialogue hooks. Resolution order
is **`--url` flag → `AGENT_DIALOGUE_URL` env → `[dialogue].url` in
`~/.config/agent/config.toml` → built-in `http://localhost:7740/mcp`**:

```toml
# ~/.config/agent/config.toml
[dialogue]
url = "http://<dialogue-host>:7740/mcp"
```

or per-invocation: `AGENT_DIALOGUE_URL=http://<dialogue-host>:7740/mcp agent dialogue peers`.

### Claude Code

No dedicated config needed — its dialogue hooks call `agent dialogue`, so it
inherits the `agent` CLI resolution above. To decouple it from the `agent`
config, set the env var in `~/.claude/settings.json`:

```json
{ "env": { "AGENT_DIALOGUE_URL": "http://<dialogue-host>:7740/mcp" } }
```

### Anything else

Any MCP client works — point it at `http://<dialogue-host>:7740/mcp` and call the
tools above. The only requirement is reaching the route and using a consistent
`role:identity` peer id in `from`/`to`.
