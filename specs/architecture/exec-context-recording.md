# Exec-context recording (mu-09z5y)

What mu writes to a session's durable event log about where and how its
`bash` and `watch` commands run, and what it deliberately keeps out.

## What is recorded

- **Per call: `cwd` on `ToolCall`.** The directory the call's command starts
  in (`Tool::working_dir`). It's read at dispatch, before the gates, so it is
  where the call runs *if* it runs. A refused call still carries it, and the
  `ToolResult` says whether anything ran. Children inherit the daemon's
  directory, and `cwd` is that directory as `getcwd` reports it at the time.
- **Per session: one `ExecContext` event**, written when a session that can
  run a command is built in a daemon. That means the daemon offers bash
  (watch is injected exactly then and runs through bash's mode) and the
  session's grant allows bash or watch. A `tool_free` session, or one
  granted neither, records none. One granted only watch still records.
  It's written again on rehydration, because a resumed session can land in a
  daemon with a different mode or environment. Bash settings are daemon-level
  and never change mid-session, so there are no deltas. Fields:
  - `bash_mode`: `strict`, `strict_prompt` or `yolo`.
  - `allowlist`: strict mode's allowed command prefixes (empty for yolo).
  - `env`: variables recorded by value (below).
  - `env_withheld`: names, without values, of every other variable the child
    receives.

## Which environment is described

Only what a child **receives**:
- **strict:** the whitelist (`PATH HOME USER SHELL TERM LANG TZ TMPDIR PWD`)
  minus anything secret-named. It is the same function the command builder
  uses (`bash::strict_child_env`), so the log and the child can't disagree.
- **yolo:** the daemon's whole environment.

The environment is read with `std::env::vars_os`. A variable that isn't
valid Unicode never fails a session: it is withheld, under a lossy rendering
of its name.

## Recorded by value, and when it isn't

A variable is recorded by value only if:
1. its name is on `[tools] record_env` (unset = the built-in list in
   `exec_context::DEFAULT_RECORD_ENV`), matched exactly, case included; and
2. its name is **not** secret-named (`bash::is_secret_env_var`, which is
   case-insensitive), by the `[names]` lists of the daemon's installed
   secret scanner: `crates/mu-core/config/secret_scan.default.toml` plus
   the operator's `secret_scan.toml`. As shipped: it is or
   ends in `_KEY`, `_TOKEN`, `_SECRET`, `_PASSWORD`, `_PASSWD`, `_PASS`,
   `_CREDENTIAL(S)` or `_COOKIE`; it has an AUTH component (`AUTH`,
   `OAUTH`, `AUTHORIZATION`, `AUTHORISATION`, `AUTHZ`, `AUTHN`, or one
   ending in `AUTH`); or it's a known credential name
   (`AWS_ACCESS_KEY_ID`). This holds whatever `record_env` says. (Narrowed
   2026-10-17 from "contains AUTH" and "every `AWS_*`", which caught
   `AUTHOR` and `AWS_PROFILE`; the check is shared with `secret_scan`.)

3. its value contains no URL (`://`). A value with a URL is withheld and the
   variable is recorded by name only.

Values are recorded **whole or not at all**, and URLs are never recorded.
No rule can tell a URL that carries a credential from one that doesn't, and
review found a way around every narrower version:
- partial redaction (stripping `user:pass@`, redacting secret-named query
  parameters) missed lists of URLs, percent-encoded parameter names and
  credentials inside an encoded nested URL;
- withholding values with named dangerous characters (`@ ? %`) missed a `#`
  fragment, where OAuth's implicit flow puts tokens;
- a safe-character allowlist can't see a secret embedded in a path, as in a
  webhook URL (`https://hooks.slack.com/services/T…/B…/secret`).

The default recording list holds no URLs, so nothing in it is lost.

**To record no values at all:** `[tools] record_env = []`. Names are still
recorded.

## Limits

URLs are never recorded. A secret in any other shape (a bare token, a JSON blob, a `key=value` config string) inside a
variable whose name isn't secret-looking is recorded as-is. So `record_env`
should list only variables known not to carry secrets. The built-in list is
chosen that way.

Names are always recorded for variables the child receives. A name reveals
only that a variable exists. If even that matters, run the daemon with a
trimmed environment.

Command text (`ToolCall.arguments`) is recorded verbatim and isn't covered
here. Redacting it is part 3 of mu-09z5y.
