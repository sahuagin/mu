# research/

Long-form research and findings that inform `mu`'s direction but are **not**
design specs. Specs (the numbered `mu-NNN` docs, the `architecture/` subdir)
say *what we are building and why it is shaped that way*. Research here says
*what we learned about the world* — external models, competing harnesses,
benchmark behavior, third-party tools we might adopt — before that learning
hardens into a spec.

## Convention

One **directory per research topic**, so a topic can accumulate documents over
time (findings, follow-up investigations, an eventual plan) without turning into
one ever-growing file. Each topic directory has its own `README.md` index.

When a research topic produces a concrete build decision, the *plan* may live
here, but the resulting **design** belongs in `specs/` and the **operational
reference** belongs in `docs/`. Cross-link rather than duplicate.

## Topics

| Topic | Status | Summary |
|---|---|---|
| [harness-model-fit/](harness-model-fit/) | on-the-wire knobs landed 2026-06-22; meta-harness Path A next | Why harnesses suit some models more than others + what `mu` changed to fit all models. The three on-the-wire slices (effort→reasoning, dialect rescue, per-model sampling) are merged; the forward direction is a Stanford-IRIS *meta-harness*–style self-improvement loop over those knobs. |

## Related, but not here

- Operator reference for running the daily-driver TUI:
  [`docs/mu-solo-runbook.md`](../docs/mu-solo-runbook.md) (operational, lives in
  `docs/`; cross-linked from the harness-model-fit topic).
