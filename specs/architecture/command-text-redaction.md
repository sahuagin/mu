# Command-text redaction (mu-09z5y, part 3) — design

Status: increment 1 (permissions) merged as #762; increment 2 (the
`secret_scan` module) is `crates/mu-core/src/secret_scan.rs`. Goal:
progress, not perfection. Each increment below makes
credentials in recorded command text measurably less exposed, and none
claims to catch everything.

## The problem

A tool call's command text is recorded verbatim, and commands carry
credentials: `curl -H "Authorization: Bearer …"`, `export GH_TOKEN=…`,
`psql postgres://u:pw@db/…`, a webhook URL, `mysql -p…`. Part 2 kept
secrets out of the *environment* snapshot. This part is about the
*command text* itself.

## Constraints from the code

1. **The primary log can't be edited.** The same command text lives in two
   events: `ToolCall.arguments` and the tool-call block inside the
   preceding `AssistantMessageEvent`. Resume and continuation
   (`agent/continuation.rs`) rebuild the conversation from the
   `AssistantMessageEvent`s and replay it to the model exactly. A
   placeholder in the log would feed the model a different history than it
   wrote, and invariant 1 (the log is the source of truth) rules that out.
   The provider already has the text anyway, because the model wrote it.
2. **Partial redaction loses.** Part 2 tried to scrub secrets out of values,
   and review found a way around every rule (URL lists, percent-encoded
   keys, nested encoded URLs, `#` fragments, path-embedded webhook tokens).
   When text is judged sensitive, the whole field is withheld, not edited.
3. **Exposure is mostly in copies, not the original.** The local log is read
   by its owner. The risk is everything that copies or consumes it: the
   session archive sync to the analytics host, mu-analytics ingest, the
   behavior judge and other model-driven readers of transcripts, and any
   future export or share.
4. **The original isn't protected today either.** Event logs are created
   under the process umask. On aiteam, claude's profile sets `umask 002`
   (group sharing), so all 24,771 event logs here are `-rw-rw-r--`,
   readable by every user.

## Design: protect the original, tag at write time, redact on the way out

### 1. Permissions on the original (first, smallest, biggest win)

Create the events directory, session logs and the command journal with
explicit modes, independent of umask: directories `0750`, files `0640`
(owner read-write, group read, nothing for others; decided 2026-10-15, so
the shared group can still read). Uses `OpenOptionsExt::mode` on create and
`DirBuilderExt::mode`. Existing files keep their mode until a one-time `mu`
maintenance command tightens them (reported, not silent).

### 2. Detection: a `secret_scan` module, best-effort and measured

A pure function over command text that returns rule hits, never edited
text: `scan(text) -> Vec<Hit { rule_id, kind }>`. It never returns the
secret. Detectors, cheapest first:

- **Secret-named assignments and flags:** `NAME=value` or `export NAME=value`
  where `NAME` passes `bash::is_secret_env_var` (the same check as part 2);
  `-H 'Authorization: …'`, `--header Authorization…`; `curl -u user:pw`;
  `mysql -p…`, `--password=…`, `--token …`.
- **URLs, parsed rather than pattern-matched:** each `scheme://…` candidate
  goes through the `url` crate (already a dependency, 2.5.8, the same type
  as `reqwest::Url`):
  - userinfo, via `username()`/`password()`;
  - decoded query keys that are secret-named, via `query_pairs()`;
  - fragment keys likewise;
  - decoded parameter values parsed again as URLs, with a depth cap.

  Parsing removes the encoding bypasses part 2 hit (`%74oken`, nesting,
  lists).
- **Known credential formats**, as a rules file: Slack webhook paths,
  `AKIA…` AWS keys, `ghp_`/`github_pat_` tokens, `sk-…` API keys,
  `xox[bp]-` Slack tokens, JWTs (`eyJ…​.…​.…`), PEM private-key headers.
  Start with a short hand-written list for the formats actually in use, and
  add rules when the archive replay shows a gap (decided 2026-10-15). This is
  what catches path-embedded secrets, which no structural rule can.
- **High-entropy tokens:** a long base64/hex-alphabet run above an entropy
  threshold, as a lower-confidence rule kind.

The rules and thresholds are config (invariant 6): a shipped default file
(`crates/mu-core/config/secret_scan.default.toml`, embedded like
`models.default.toml`), with an operator file layered on top
(`~/.config/mu/secret_scan.toml`, wired when the scanner is). In the operator
file, a `[[format]]` with an existing id replaces that rule or disables it
(`enabled = false`), a new id adds one, and `[entropy]` fields override. A
bad operator rule is an error naming the file and rule, never a silent drop.
So a new token format is a config change, not a build.

**Known gaps (regex shell parsing).** The assignment and flag detectors
read shell syntax with regexes. Review found and fixed many corners
(quoting, concatenated pieces, ANSI-C `$'…'`, `${X:-fallback}`,
`$(…)literal`), but a regex only approximates the grammar. Known remaining
gaps: the flag detector (`--password=…`) checks only the first character
of a value, so concatenated or ANSI-C values (`--password=""hunter2`,
`--password=$'hunter2'`) are missed; nested command substitutions and
arithmetic `$((…))`; escaped quotes inside values; array assignments; and
assignments built from variable names. The follow-up is to read assignments and flags from a real shell
parse (the tree-sitter decomposer, mu-gj0fd; tracked as mu-2fh93) instead.
Decided 2026-10-17:
merge with these gaps documented rather than keep patching regexes.

**Measuring progress.** Replay the detector over the session archive with
the harness from mu-analytics' `scripts/shell_decompose.py`, ported or
reused. Report hit counts
per rule, never matched text, and track them between versions. False
positives cost little (something gets withheld that didn't need to be).
False negatives are the leaks, so the corpus of known shapes from part 2's
review rounds becomes a regression suite.

### 3. Tag at write time (the log stays exact)

When a `ToolCall` is recorded, scan its arguments. On any hit, add an
optional field to that event, `sensitive: [rule_id, …]`. That's the
classification, never the secret. The `AssistantMessageEvent` holding the
same call is covered through `call_id`: the tag on the `ToolCall` applies
to the matching block. The content itself is unchanged, so resume, fork and
the model's history stay exact.

Tagging at write time, rather than rescanning on export, means:
- every consumer gets the same decision;
- the decision is itself on the record (auditable, and countable for the
  progress numbers);
- a later, better detector can re-tag old sessions without editing them, by
  APPENDING a tag event to the same log that names the earlier event ids
  (decided 2026-10-15: no sidecar, no rewrite).

The append-only mark event is shared with the confidential envelope
(mu-bcd09), which marks old sessions the same way. An event's tags are the
ones written with it plus any later mark event in the same log that names
it. A live session's mark goes through its daemon (one writer); a closed
session's is appended by an offline tool.

### 4. Redact on the way out: one projection every copy goes through

A `redacted_view` projection over a session's events. For every tagged call
it replaces the whole affected text in both places (the `ToolCall.arguments`
and the matching `AssistantMessageEvent` block's arguments) with:

```json
{"withheld": true, "rules": ["slack-webhook"], "hmac": "…"}
```

`hmac` is an HMAC-SHA256 of the original text under a key kept outside the
log (for example `~/.config/mu/redaction.key`, `0600`). It is ONE key shared
by both machines (decided 2026-10-15), so redacted commands correlate across
the archives. It lets analysis match identical commands without being
reversible by guessing, the way a bare hash of a short secret would be.

Exposed as `mu export --redacted <session>` and as a function in
`mu-events-py`, so Python consumers use the same projection. Then switch the
copies to it:
- the session archive sync ships redacted views, not raw logs (the sync
  tool lives outside mu);
- mu-analytics ingest and the behavior judge read the archive, so they
  inherit it.

The archive keeps raw logs for now (decided 2026-10-15; to be revisited), so
the redacted view is for consumers that send content anywhere, not for the
archive itself.

### 5. Later: tool results

Command *output* carries secrets too (`cat .env`, `env`,
`gh auth token`). It gets the same tag-and-project treatment. It is
deliberately a later increment, because results are larger and noisier and
the detector should earn trust on commands first.

## What this doesn't claim

- A secret the scanner doesn't recognize is copied in the redacted view. The
  docs say so, and each increment's numbers say how much is caught.
- Nothing here hides a secret from the model or the provider. The model
  wrote the command.
- The primary log still holds everything. Its protection is file
  permissions plus the owner's machine.

## Increments (each its own PR)

1. **Permissions:** explicit `0750`/`0640` on create. **Done, #762.** The
   existing logs on aiteam were tightened with a one-time `chmod` instead of
   a maintenance command.
2. **`secret_scan` module:** detectors, the rules config with shipped
   defaults (`crates/mu-core/config/secret_scan.default.toml`, plus an
   optional operator layer), and unit tests seeded from part 2's corpus. It
   isn't wired into anything yet.
3. **Archive replay harness:** per-rule hit counts over the archive, never
   the matched text. This is the baseline for progress.
4. **Tag at write time:** the `ToolCall.sensitive` field, plus an offline
   re-tagger that appends mark events to old logs.
5. **`redacted_view` + `mu export --redacted` + the mu-events-py binding,**
   with tests that resume from a tagged log is exact and the export
   withholds both places.
6. **Switch the copies:** the archive sync and other consumers (outside mu:
   the sync tool, mu-analytics).
7. **Tool results:** the same pipeline.

## Decisions (2026-10-15)

1. File modes: `0640` files, `0750` directories.
2. The archive keeps raw logs for now (to be revisited).
3. One shared HMAC key, so both machines' archives correlate.
4. A short hand-written rule list, grown from the archive replay.

Related: mu-09z5y parts 1–2 (#759, #761); mu-bcd09 (confidential envelope, shares the append-only mark event); mu-gj0fd (argv-level
decomposition, which can later sharpen the flag detectors);
`specs/architecture/exec-context-recording.md`.
