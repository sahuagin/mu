# The `--help-ai --json` standard

A tiny, CLI-agnostic discoverability contract: a tool describes itself to an
*agent* consumer in structured JSON, so a discovery layer (t4c, or mu's
capability manifest) can register it without bespoke parsing. It is the CLI
analogue of MCP's tool schema, and the producer side of t4c's
`HelpAiProbeSource`.

## The contract

A conforming tool, invoked as `<tool> --help-ai --json`, prints a single JSON
object to stdout and exits 0:

```json
{
  "name": "code-index",
  "summary": "semantic + lexical code search over an indexed repo",
  "keywords": ["code", "search", "symbol"],
  "subcommands": [
    { "name": "recall", "summary": "search the index by intent" },
    { "name": "status", "summary": "index health" }
  ]
}
```

| field | type | required | meaning |
|---|---|---|---|
| `name` | string | yes | the tool's canonical name (becomes the tool segment of its path, `<class>.<name>`) |
| `summary` | string | no | one line, discovery-facing |
| `keywords` | string[] | no | extra match terms for lexical/semantic ranking |
| `subcommands` | object[] | no | each `{name, summary}` becomes a leaf `<class>.<name>.<sub>` |

Unknown fields are ignored (forward-compatible). `--help-ai` *without* `--json`
MAY print a human-oriented variant; `--json` is the machine contract.

## Emitting it

- **Rust + clap:** `t4c::helpai::from_clap(&Cmd::command())` builds the doc from
  your clap `Command` by introspection; print `t4c::helpai::to_json(&doc)`. t4c
  itself does this (`t4c --help-ai --json`) — it self-registers (turtles).
- **Shell tools:** see `templates/help-ai.sh` — a heredoc that prints the JSON.

## Why

A fresh agent boots not knowing the toolset. If every tool can describe itself
in one structured call, discovery becomes generic: the harness enumerates and
ranks tools with no per-tool special-casing. Conformance cost is ~zero, and the
ecosystem becomes self-describing — which is the whole bet behind t4c.
