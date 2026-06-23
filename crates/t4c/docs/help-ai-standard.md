# The `--help-ai --json` standard

A tiny, CLI-agnostic discoverability contract: a tool describes itself to an
*agent* consumer in structured JSON, so a discovery layer (t4c, or mu's
capability manifest) can register it without bespoke parsing. It is the CLI
analogue of MCP's tool schema, and the producer side of t4c's
`HelpAiProbeSource`.

The schema is a **superset**: a small required *core* (enough to discover and
rank a tool) plus *optional rich fields* a tool MAY add so an agent can also
call it correctly without a second probe.

## The contract

A conforming tool, invoked as `<tool> --help-ai --json`, prints a single JSON
object to stdout and exits 0:

```json
{
  "name": "code-index",
  "summary": "semantic + lexical code search over an indexed repo",
  "keywords": ["code", "search", "symbol"],
  "subcommands": [
    {
      "name": "recall",
      "summary": "search the index by intent",
      "args": [
        { "name": "query", "positional": true, "required": true, "help": "the search intent" },
        { "name": "top", "long": "--top", "takes_value": true, "value_name": "N", "default": ["10"], "help": "max hits" }
      ]
    },
    { "name": "status", "summary": "index health" }
  ]
}
```

### Core (required)

| field | type | required | meaning |
|---|---|---|---|
| `name` | string | **yes** | the tool's canonical name (becomes the tool segment of its path, `<class>.<name>`). NOT `command`. |
| `summary` | string | **yes** | one line, discovery-facing. NOT `about`. |
| `keywords` | string[] | no | extra match terms for lexical/semantic ranking; may appear on any node |
| `subcommands` | object[] | no | **recursive** — each entry is itself a node with this same schema (`name`, `summary`, and any optional `keywords`/`args`/`subcommands`/…). Each leaf becomes `<class>.<name>.<sub>…`. |

### Rich (optional — emit when cheaply available)

These let a consumer build an invocation or a JSON-Schema input without a second
call. Any of them MAY appear on the root node or any subcommand node.

| field | type | meaning |
|---|---|---|
| `args` | object[] | per-argument calling convention; entry schema below |
| `usage` | string | one-line usage / synopsis |
| `output_schema` | object | JSON Schema of the command's stdout (for tools with structured output) |
| `aliases` | string[] | alternate names that invoke this node |
| `invokable` | bool | true if this node is directly runnable (a leaf); false for a pure group |
| `path` | string | full invocation path, space-joined (e.g. `"agent memory add"`) |

#### `args` entry

| field | type | required | meaning |
|---|---|---|---|
| `name` | string | **yes** | argument id |
| `long` | string | no | long flag incl. `--` (e.g. `--top`) |
| `short` | string | no | short flag incl. `-` (e.g. `-t`) |
| `positional` | bool | no | true for a positional arg |
| `required` | bool | no | true if the arg must be supplied |
| `takes_value` | bool | no | true if it consumes a value (false = bare flag) |
| `multiple` | bool | no | true if repeatable |
| `value_name` | string | no | metavar for the value |
| `help` | string | no | one-line help |
| `possible_values` | string[] | no | enumerated allowed values |
| `default` | string[] | no | default value(s) when omitted |

Unknown fields are ignored (forward-compatible): a producer MAY emit more than
this and a consumer MUST tolerate it. `--help-ai` *without* `--json` MAY print a
human-oriented variant; `--json` is the machine contract.

## Field-name standardization

The discovery-facing names are `name` and `summary`. Two pre-standard dialects
differ and are being aligned to this standard:

- **clap-catalog** emitted clap-native `about` → maps to `summary`.
- **code-index** emitted `command` → maps to `name`.

A consumer keying off the standard names treats those legacy fields as absent,
so producers should emit `name` / `summary`.

## Emitting it

- **Rust + clap:** use the [`clap-catalog`] crate — it walks the live
  `clap::Command` tree and renders this schema (recursive subcommands + rich
  `args`), so the catalog can never drift from the actual CLI. t4c self-registers
  with it — `t4c --help-ai --json` *is* `clap_catalog::catalog`'s output — so t4c
  is a tool in its own registry (turtles), consumed by `HelpAiProbeSource` like
  any other tool.
- **Shell tools:** see `templates/help-ai.sh` — a heredoc that prints the JSON.

[`clap-catalog`]: https://github.com/sahuagin/agent_tools/tree/main/clap-catalog

## Why

A fresh agent boots not knowing the toolset. If every tool can describe itself
in one structured call, discovery becomes generic: the harness enumerates and
ranks tools with no per-tool special-casing, and — for tools that emit the rich
fields — can call them without a second probe. Conformance cost is ~zero, and
the ecosystem becomes self-describing — which is the whole bet behind t4c.
