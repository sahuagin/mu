# review-gate v2 — ollama model substrate

Reproducible definitions for the two local models the orchestrated review gate
(bead `mu-u1it`) runs on. Checking these in turns "the box happens to have the
right models loaded" into "reconstitute from repo."

| model | role | base | baked `num_ctx` | ~VRAM |
|-------|------|------|-----------------|-------|
| `qwen-orch`   | orchestrator (fans the worker over a diff, synthesizes the verdict) | `qwen3.6:27b` | 131072 | 27 GB |
| `gpt-oss-rev` | per-file reviewer (single-shot recall, one file per call)          | `gpt-oss:20b` | 49152  | 14 GB |

Both co-reside on the 2×24 GB box (~43 GB, 100% GPU).

## Why `num_ctx` is baked into the Modelfiles

mu talks to ollama over the **Anthropic Messages wire** (`/v1/messages`, bead
`mu-fmas`) for native `tool_use` + thinking blocks. That wire carries **no
`num_ctx`** — it's an ollama-native option only. So a review request would make
ollama (re)load the model at the server default (`OLLAMA_CONTEXT_LENGTH=262144`),
ballooning qwen to 32 GB and **evicting the co-resident worker**.

Baking `num_ctx` into each model artifact pins the context independent of the
context-less wire, which both fixes the eviction and preserves the *asymmetric*
sizing (big orchestrator, small worker) the topology wants.

Full finding + end-to-end proof: agent memory
`ollama-anthropic-wire-numctx-modelfile-fix-2026-06-08` (`a721c14d`).

## Usage

```bash
# create/refresh the models (idempotent; shares base weights)
./create-models.sh

# load them resident at their baked contexts, keep_alive 24h
./warm.sh
```

Both target `OLLAMA_HOST=10.1.1.143:11434` by default; override `OLLAMA_HOST`
for a different box.
