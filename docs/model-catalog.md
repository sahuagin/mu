# Model catalog

`mu` has a layered provider/model catalog. Built-in defaults are embedded from
`crates/mu-core/config/models.default.toml`; user overrides live at:

```text
~/.config/mu/models.toml
```

The catalog is loaded with Figment, so keyed tables deep-merge. A user override
can change one field without copying the built-in entry:

```toml
[models.qwen3_6_35b]
max_output_tokens = 24576
```

Top-level sections:

- `[providers.<name>]` — provider/wire-protocol metadata (`kind`, aliases,
  labels, auth, usage semantics, quirks).
- `[models.<name>]` — exact model metadata (`model`, aliases, labels, context
  limits, max output tokens, reasoning behavior, quirks).
- `[model_rules.<name>]` — prefix rules for families or dynamically discovered
  local models.
- `[favorites.<name>]` — operator-facing provider+model combinations for UI
  pickers and shortcuts.

Example local reasoning override:

```toml
[model_rules.qwen36_local]
prefix = "qwen3.6:"
max_output_tokens = 16384
reasoning_in_output = true
quirks = [
  "thinking_counts_against_max_tokens",
  "may_return_empty_visible_output_at_low_max_tokens",
]

[favorites.local_reasoner]
provider = "ollama"
model = "qwen3.6:35b-a3b-q8_0"
label = "Local Qwen 35B"
aliases = ["qwen36", "local-reasoner"]
default_effort = "medium"
```

Quirks are free-form strings; a provider consumes the ones it knows and
ignores the rest. The Anthropic lane reads three kinds off the resolved model:
`mid_conversation_tool_changes` (send that beta header); the `rejects_*`
request rules, each naming a request shape Anthropic's API answers with a 400,
which the lane refuses before the wire on that API and sends with a warning
from any other endpoint; and the warn-only rules `ignores_fast_mode` (a shape
the model accepts and disregards) and `retired` (an id gone from Anthropic's
API; the request goes and Anthropic's own not-found is the authority, and off
that API the rule is silent). A rule with no `prefix`/`prefixes` matches nothing
and is warned about at load, since a renamed built-in rule leaves an override
keyed on the old name orphaned. The shipped rules, with the spec page each one
comes from, are the comment block over `[model_rules.*]` in
`models.default.toml`.

`daemon.list_routes` exposes the catalog-derived metadata on each route,
including provider aliases/quirks, model aliases/quirks, `max_output_tokens`,
and matching favorites.
