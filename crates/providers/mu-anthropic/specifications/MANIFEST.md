# Anthropic API specifications — manifest

These are a **time-pinned snapshot** of Anthropic's developer docs, captured so
this crate has a stable, referenceable contract that does NOT silently drift
under us. (When the live docs change, our golden tests catch it — see
PLAN.md "Test tiers". This snapshot is the "what it said when we built it"
record.)

## Captured

- **Date:** 2026-06-13
- **Source host:** https://platform.claude.com

## Files

| file | source URL | stored | what it is |
|---|---|---|---|
| `llms.txt` | https://platform.claude.com/llms.txt | ~179K raw | Annotated INDEX of the docs — a link manifest. Use it to find and fetch individual pages on demand. |
| `llms-full.txt.xz` | https://platform.claude.com/llms-full.txt | ~1.1M (xz; 79M raw) | The ENTIRE rendered docs in one file, the full spec of record. Stored `xz -9` compressed (79M → 1.1M, ~70×). Keeps the repo light; the text is xz-redundant. |

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
