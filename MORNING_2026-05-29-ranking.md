# mu: goal session 2026-05-29 — t4c ranking layer (mu-d2iy)

Added the discovery-**quality** (ranking) layer to t4c and **validated the thesis at the gate**. Branch `t4c-ranking` off `t4c-forward-build`, commit-only, **UNPUSHED**. Experiment doc: `~/.claude-personal/experiments/goal-2026-05-29-mu-t4c-ranking.md`.

## What landed

| Commit | Bead | One-line |
|---|---|---|
| `lqypnpwk` | mu-d2iy.1 | config grammar — chain (collapse) vs neighborhood (disambiguate) |
| `vwuslmws` | mu-d2iy.2 | chain resolution + 3-state tombstone (active/superseded/absent) |
| `srwkuszs` | mu-d2iy.3 | Embedder trait + FakeEmbedder + ConfigEmbedder |
| `qszlwxym` | mu-d2iy.4 | SemanticRanker + vector cache, wired into `find` |
| `vposqnuo` | mu-d2iy.5 | find-quality benchmark harness (`t4c bench`) |
| (gate) | mu-d2iy.6 | live re-dogfood — **8/8** |

## Test state
`TMPDIR=~/tmp cargo test -p t4c` → **38 passed**. clippy clean. Leaf invariant intact (reqwest is the only new dep, third-party).

## Goal status — COMPLETE (gate passed)
- **mu-d2iy: ranking layer built + thesis validated.** `.1`–`.6` closed; `.7` (this report + memory) done.

## The gate result (the headline)
`t4c bench` **LIVE** (qwen3-embedding-8b via OpenRouter — the default endpoint works) = **8/8**; fake/lexical baseline = 7/8. The +1 is the adversarial case *"locate the bug in this module"* → `mcp.code-index.recall`, which the lexical ranker routed to `diff-pretty`. **Semantic ranking routes the intent lexical conflates ⇒ `find` earns its front-door role.** The mu-d33g confident-wrong failure is addressed by embeddings + the chain/neighborhood model.

## How the design landed
- **chains** (interchangeable impls → collapse to first-installed + tombstone the rest) vs **neighborhoods** (related-but-distinct → keep all, embeddings disambiguate). Author tags synonyms-vs-neighbors.
- **preference baked at resolve-time** (tombstone), so the ranker stays pure-semantic — not a runtime weight.
- **small corpus** ⇒ embed-all at `discover` + cached vectors + brute-force cosine at query (one intent-embed). No vector DB.
- **graceful lexical fallback** when offline / endpoint-down.

## Things noticed / follow-ups
- Optional **gpt-in-mu second opinion** as a cross-check — non-blocking; the 7→8 delta on the discriminator is decisive on its own.
- `ConfigEmbedder` endpoint/model are env-overridable (`$T4C_EMBED_ENDPOINT`/`MODEL`); default OpenRouter + qwen3-embedding-8b confirmed working live.
- **usage-prior** (the ~2-month logs) still deferred — a future enhancement, not needed for the win.
- Branch **UNPUSHED**; `t4c-ranking` off `t4c-forward-build` (both unmerged). Phase-3 mu-native integration (`mu-kex4.6`) remains the separate gated item.

## Cost / turns / wall-clock
Supervised foreground, one sitting. 6 feature commits (`.1`–`.5` + the gate run). 38 tests, clippy clean throughout. No pushes; `/btw` stack and the mu-d33g chore branch untouched.
