# harness-internals

Source-level analyses of competing agent harnesses: clone the repo, read the
actual mechanisms, and extract what transfers to mu (and what deliberately
does not). Complements [harness-model-fit/](../harness-model-fit/), which
studies how *models* fit harnesses; this topic studies how the harnesses
themselves are built.

Each analysis terrain-checks its mu claims against mu `main` on the stated
date and files beads for anything actionable, so the documents stay research
records rather than pending work lists.

## Documents

| Date | Document | Subject | Outcome |
|---|---|---|---|
| 2026-08-26 | [deepseek-harness-cordis](2026-08-26-deepseek-harness-cordis.md) | DeepSeek Harness (`dsh`) and its vendored Cordis micro-kernel | Beads `mu-eyfd8` (rope span epoch invalidation), `mu-0xhja` (per-session disposer tree); model-visible-means-logged gap located at rope assembly; capability-seam idea dissolved (mu already has it) |
| 2026-08-27 | [codex](2026-08-27-codex.md) | OpenAI Codex CLI (`codex-rs`) at trunk 694edc23, TUI focus | The scrollback-commitment model as a parts list for mu-solo (single mutation frontier, newline-gated commits, exactly-once insertion, adaptive drain, log-replay testing); SQ/EQ + rollout notes; agent-roles convergence |

## Prior art note

An earlier codex investigation (≈2026-07-07, UI-focused) was lost with pruned
session transcripts — its session ended in a hang that took the focus. The
2026-08-27 document re-does that work durably; writeups now land here in the
same session that produces them.
