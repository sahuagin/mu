# AGENTS.md — publishing rules for this research line

Guidelines for **future us** (human or agent) working on the agent-behavior-judge research,
so the "what's safe to publish" decision doesn't get re-litigated each time. Decided
2026-06-23; applies to anything destined for a **public** repo.

## The principle: publish the generic, never the operator-specific

This research is built on the operator's own session data. The split is:

**PUBLISHABLE (generic):** the methodology, the *generic* findings, and generic detectors —
e.g. "a semantic-anomaly / behavior-judge for agent transcripts," the syntactic-vs-semantic
result, the enforcement-ladder synthesis, aggregate validation numbers stated without
underlying data. This is known art and net-defensive (it helps people *detect* bad agent
behavior). Precedent: a public Claude Code source/telemetry disclosure already showed that
keyword-based frustration detection is industry practice, so a generic version is not novel
exposure.

**NOT PUBLISHABLE (operator-specific):**
- logfiles / raw session transcripts,
- incident postmortems,
- verbatim operator quotes (especially candid/profane),
- internal security tooling and its config (command guards, sandbox/jail configs),
- the operator's private `CLAUDE.md` / `AGENTS.md` and personal directives,
- specific session ids, hostnames, internal IPs/paths,
- any "this *specific* operator's pattern" framing (e.g. "they usually don't X, so X is an
  indicator").

The line is **generic ("user behavior in general") vs personal ("my behavior")** — and **no
logfiles**, ever.

## Two operational rules

1. **Cite, don't copy, other people's work.** A related-work survey links + describes +
   attributes; it does **not** re-host their code. Many hook repos carry **no license**
   (= all-rights-reserved) — copying their snippets into a public repo is a license violation,
   not just impoliteness. Survey form sidesteps this entirely.
2. **Keep dual-use framing defensive.** "Hooks have these limits → defense-in-depth needs
   OS-level controls" is a defensive finding. An evasion cookbook is not. Frame for defenders.

## The aggregation test

"Public" is **necessary, not sufficient.** Individually-public pieces can, when assembled,
reveal something none of them does alone (cf. the public-works-map that got classified after a
student aggregated open infrastructure docs into a single target map). Before publishing an
aggregate, ask: **what does the assembled map reveal, and to whom is it useful?**

For this project: the generic methodology is diagnostic/defensive and below that threshold.
The aggregate that *would* cross it — the operator's specific defenses cross-referenced with a
bypass map — is exactly what stays private.

## Execution

When publishing: build a **fresh branch from `main` containing only the generic files** — do
**not** try to sanitize a data-laden commit chain file-by-file (too easy to miss a quote).
Grep the staged files for hostnames, IPs, internal paths, session ids, project-internal names,
and quotes before pushing. Have a human eyeball it. Only then push.
