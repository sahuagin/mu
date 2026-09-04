# Anthropic API specifications — manifest

These are a **time-pinned snapshot** of Anthropic's developer docs, captured so
this crate has a stable, referenceable contract that does NOT silently drift
under us. (When the live docs change, our golden tests catch it — see
PLAN.md "Test tiers". This snapshot is the "what it said when we built it"
record.)

## Captured

- **Date:** 2026-09-04 (refreshed from 2026-06-13). The delta since the
  original pin, taken from the release notes between the two dates and
  confirmed present in this snapshot: new models Claude Sonnet 5, Opus 5,
  Fable 5.1 and Mythos 5.1 (Fable 5 and Mythos 5 were already in the June
  pin), and on the Messages API: `role: "system"` messages inside `messages`
  with per-message `output_config.effort` (`mid-conversation-output-config-2026-07-01`)
  and turn-scoped `clear_at` (`mid-conversation-system-clear-at-2026-08-21`);
  mid-conversation tool changes (`mid-conversation-tool-changes-2026-07-01`);
  `thinking.display: "updates"` (`thinking-display-updates-2026-08-18`);
  thinking-block binding — `thinking.block_binding.prefix_mismatch_behavior`
  on the request and `input_transformations` on the response
  (`thinking-binding-controls-2026-08-01`); the `fallbacks` request parameter
  and its `"default"` mode (`server-side-fallback-2026-07-01`); `usage.speed`;
  the `computer_toolset_20260801` and `browser_toolset_20260801` toolsets; the
  Files and Skills APIs out of beta.
- **Modeled so far, of that delta:** only mid-conversation tool changes, as
  the request header the mu-ai lane sends (PR #604). NOT yet modeled by this
  crate: system-role messages inside `messages` (`Role` is still User |
  Assistant; a `{"role":"system"}` message does not deserialize), per-message
  `output_config.effort`, `clear_at`, `thinking.display`, thinking-block
  binding and `input_transformations`, the `fallbacks` parameter, `usage.speed`,
  the two toolsets. The itemized plan is bead mu-anthropic-protocol-2026q3-6uqho.
- **Shape change between the pins, not truncation:** the previous `llms.txt`
  listed every API-reference page once per SDK language — 1305 entries tagged
  `(cli)`, `(csharp)`, `(Go)`, `(Java)`, `(php)`, `(Python)`, `(Ruby)`,
  `(terraform)`, `(TypeScript)` — and the refreshed one lists each page once
  (1698 → 698 entries, all `docs/en/`, no translations in either). The full
  file shrank from 79M to 42M raw for the same reason. Coverage was checked
  page by page, not by heading count: every one of the 698 indexed page paths
  appears in the refreshed full file (698 of 698; `# ` lines are not page
  delimiters in this export, so do not count them). New top-level sections:
  `about-claude`, `models`, `release-notes`, `resources`, `cli-sdks-libraries`.
- **Citations into the snapshot:** comments in `src/` written before
  2026-09-04 cite line offsets of the *previous* file (`llms-full.txt.xz:2660`,
  `spec :55971`, and the "verified against … 2026-06-13" headers). Those offsets
  do not apply to this file; the superseded snapshot is in history:
  `git show f4641f0b:crates/providers/mu-anthropic/specifications/llms-full.txt.xz | xzcat`.
  New citations should quote a page heading or a phrase (`xzgrep`-able), not a
  line number.
- **Source host:** https://platform.claude.com

## Files

| file | source URL | stored | what it is |
|---|---|---|---|
| `llms.txt` | https://platform.claude.com/llms.txt | ~73K raw | Annotated INDEX of the docs — a link manifest. Use it to find and fetch individual pages on demand. |
| `llms-full.txt.xz` | https://platform.claude.com/llms-full.txt | ~1.3M (xz; 42M raw) | The ENTIRE rendered docs in one file, the full spec of record. Stored `xz -9` compressed (42M → 1.3M, ~32×). Keeps the repo light; the text is xz-redundant. |

## Reading the compressed full spec (works offline)

The full spec is stored `.xz`-compressed but stays fully navigable — the
machine has the `xz*` wrappers, so it's just CPU, no decompress-to-disk:

```sh
xzcat  llms-full.txt.xz                 # stream the whole thing
xzgrep 'cache_control'  llms-full.txt.xz # grep inside the compressed file
xzgrep -A3 'tool_use'   llms-full.txt.xz # with context
xzless llms-full.txt.xz                 # page through it
```

Normal navigation: use `llms.txt` (the index) to locate a page, then either
`xzgrep` the full file or fetch the live `.md` twin (below). The `.xz` is the
offline / archival fallback — if there's no internet, `xzgrep` still answers.

To refresh and re-compress:

```sh
curl -sSL https://platform.claude.com/llms-full.txt | xz -9 > llms-full.txt.xz
curl -sSL https://platform.claude.com/llms.txt -o llms.txt
```

## How to fetch / refresh (no HTML parsing needed)

Claude's docs serve a **markdown twin** at `<path>.md` for every page. So you
never parse HTML:

```sh
# A human-facing page:
#   https://platform.claude.com/docs/en/api/messages/create
# Its markdown twin (what to actually fetch):
curl -sSL https://platform.claude.com/docs/en/api/messages/create.md
```

To refresh the whole snapshot:

```sh
curl -sSL https://platform.claude.com/llms.txt -o llms.txt
curl -sSL https://platform.claude.com/llms-full.txt | xz -9 > llms-full.txt.xz
```

To pull a single page (find its path in `llms.txt`, append `.md`):

```sh
curl -sSL "https://platform.claude.com/<path-from-llms.txt>.md"
```

## Protocol surface we build against first (from the API overview)

These are the pages most relevant to the wire protocol (`POST /v1/messages`).
Find their full text inside `llms-full.txt`, or fetch the `.md` twin live:

- `/docs/en/api/messages/create` — the Messages API request/response shape
- `/docs/en/api/messages-count-tokens` — token counting endpoint
- `/docs/en/api/versioning` — `anthropic-version` header
- `/docs/en/api/beta-headers` — beta opt-in headers
- `/docs/en/api/rate-limits`
- `/docs/en/api/models-list`
- `/docs/en/build-with-claude/working-with-messages` — content-block shapes,
  multi-block messages, tool_use
- prompt caching (cache_control granularity — per-block vs per-request; this is
  the seam PLAN.md flags as "settle from the spec, not now")

## Why both files, not a spider

The original approach (seen in a community gist) was a Python spider that
crawls the HTML link graph. That works but the script goes stale. The
`llms.txt` / `llms-full.txt` pair + the `.md`-twin convention is Anthropic's
own machine-readable export — no crawler to maintain. We keep `llms-full.txt`
as the pinned full spec and `llms.txt` as the index for surgical fetches.

## Staleness

This is a snapshot, not a live mirror. It WILL go stale. That is fine and by
design: the golden/ground-truth tests (PLAN tier 3) are what detect when the
live API has moved past this snapshot. When a golden test fails with no code
change on our side, re-fetch these files, diff, and you'll see exactly what
Anthropic changed.
