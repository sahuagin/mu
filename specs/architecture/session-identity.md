# Session identity: the log is the noun, everything else is an argument

| field   | value                                                            |
| ------- | ---------------------------------------------------------------- |
| status  | accepted (operator-ratified design philosophy, 2026-06-06)        |
| authors | tcovert + claude-personal (claude-opus-4-8), with one corollary from a gpt-5.5 session (`mu:a0d2e6c477c091a2`) |
| scope   | design philosophy — names the principle mu's architecture already embodies, so future decisions can be checked against it |

## The principle

A model invocation is a stateless function application. The model does not
run between turns; it is dormant and elsewhere (a provider, an ollama box).
Each call/response is based entirely on what is in the context at the moment
of the call. Therefore:

> **The model's entire reality is the assembled context at call time.
> Identity lives in the durable session log. Everything else — the model,
> the harness, the role, the schedule — is an argument to an invocation.**

mu committed to half of this before it was articulated ("the event JSONL is
the source of truth; everything else is a projection"). This document names
the other half: the things that *feel* stateful — the running agent, its
personality, its model, its sense of being notified — are all call-time
parameters over that log.

## The decomposition

| Thing               | What it actually is                                            | Consequence |
| ------------------- | -------------------------------------------------------------- | ----------- |
| **Session**         | The durable, append-only event log. The only pet in the system. | Crash-proof identity; survives any process, binary, or provider. |
| **Model**           | A config object used to invoke a call (provider, model id, params — cf. the model catalog). Not what is running; nothing is running. | Swappable per ask (`SwitchProvider`, per-ask overrides). "An opus session" is just a session whose recent calls were routed through the opus config object. |
| **Harness/daemon**  | A disposable executor that projects the log into context and applies a model to it. | Hot restart/upgrade: stop old binary, start new, rehydrate, resume. The session does not participate in its own upgrade. |
| **Orientation/role**| A context stratum injected after the bootloader (the `/agents`-md slot). | Not simulated history — *constituted* state. N agents = N orientation suffixes over one shared (cacheable) base. |
| **Notification**    | A durable inbox append (with timestamp) + a wake executor.      | No recipient process required. "Notified at 3am" means the log grew at 3am and the model was applied shortly after. |
| **Personality**     | Negative-space constraints in the dossier + whatever emerges.   | Guardrails, not choreography ("emergent that doesn't irritate me"). |

## Corollaries

### 1. Genesis vs. reattach — the startup tax is paid once

The wake-up cost (orientation triage, "dossier shock") exists only at session
genesis: the one moment the model meets standing context with no lived history
after it. A mid-session rehydration with **exact reassembly** is invisible to
the model — it cannot notice the death of a process it never perceived. The
conditional "exact" is where the engineering lives (see invariant below).

### 2. Live restart and live upgrade

Because the executor is disposable, upgrading mu mid-session is: stop the old
binary, start the new one, rehydrate, resume. The guarantee splits:

- **Conversation history must reassemble exactly.** It is the session's lived
  past and the cacheable prompt prefix. History *rendering* must therefore be
  versioned and append-stable — a renderer change that re-serializes past
  events rewrites the model's history underneath it and silently invalidates
  cache. (The signed event chain proposal makes this drift detectable rather
  than silent.)
- **Ambient context may float.** Tool specs, system spans, capability sets are
  upgrade-mutable by design; the model already experiences tools changing
  between turns.

The drain inventory at restart is, by definition, the un-evented process
state: parked wakeups already survive (durable `AutonomousScheduledWakeup`);
watches do not until their lifecycle is evented (mu-dvmu). **Every gap in
live-restart is exactly a leak in event-sourcing.** The leak inventory is the
work list. (Implementation seat: mu-mh4.)

### 3. Sessions are trees — /rewind, decomposed

claude-code's `/rewind` is, beneath the UI: *truncate the context to point P
and try again.* Most users experience it as an undo button. Under this
document's decomposition it generalizes: a session log with a chosen point P
is a **fork site**, and "try again" is just one fork that discards its
sibling. Keep both siblings and vary any argument at P:

| Vary at P        | You get                                                        |
| ---------------- | -------------------------------------------------------------- |
| nothing (retry)  | `/rewind` / sampling variance probe                            |
| the model        | "how would opus have answered right here?" — cross-model differential |
| the time         | the replay-probe canary (same prefix, same config, different day → provider-state drift detection) |
| one context stratum | controlled experiments (e.g. the bootloader A/B: fork at genesis, vary one segment) |

"I wonder how a different model or time would respond to this prompt" does
not require a new session played forward from scratch. It requires a snapshot
of the exact moment — which the event log *is* — and a different argument.
Prompt caching makes sibling forks nearly free: they share the cached prefix.
A session is a line only by convention; the log supports a tree.

### 4. The config object is a pointer — "same model" is an assumption to test

The model argument (`claude-opus-4-8`) names what was *requested*. What
answers the call is mutable provider infrastructure at that moment. An
operator quality mark therefore labels a `(context, config-object,
provider-state-at-time-T)` tuple whose third element is hidden — which is
the entire degradation-measurement program in one sentence. The replay canary
exists because the pointer's target can change while the pointer doesn't.

### 5. Stratified context is also cache-optimal context

The phenomenologically right ordering (bootloader → dossier → orientation →
task: stable orientation-about-orientation first, volatile work last) is the
same ordering that maximizes provider prompt-cache hits. The strata that vary
per agent sit after the strata shared by all agents. Two independent
arguments, one layout — usually the sign of a load-bearing design.

## Design consequences (the invariant)

**Deterministic context reassembly from the log** is the single property that
buys: live restart, live upgrade, replay probes, and session forking. Treat
it as a first-class invariant:

1. Anything that influences assembled context must be derivable from the log
   (or from versioned, declared ambient config).
2. History rendering is versioned; renderer changes never re-serialize
   existing events' contribution to context.
3. Process state that cannot be reconstructed at rehydration is a bug class,
   not a fact of life — event it or accept losing it explicitly.

## Provenance

Distilled 2026-06-06 from an operator/agent design conversation that began
with session-startup phenomenology (gpt-5.5 session `mu:a0d2e6c477c091a2` —
the "bootloader" coinage and the dossier/stratification corollary) and ended
at this decomposition. Related artifacts: agent memories
`mu-event-command-architecture-riff`, `rehydrate-vs-attach-duality`,
`context-constitution-principle`; beads mu-mh4 (restart persistence),
mu-dvmu (durable watch lifecycle), mu-m7x (per-segment source-map),
mu-recall-bootloader-flag-nxpo (first experiment instrument); experiment
`~/.claude-personal/experiments/bootloader-startup-ab-2026-06-06.md`;
spec mu-036 (the wakeup primitives, PRs #209/#211).
