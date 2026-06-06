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

`daemon.list_routes` exposes the catalog-derived metadata on each route,
including provider aliases/quirks, model aliases/quirks, `max_output_tokens`,
and matching favorites.
