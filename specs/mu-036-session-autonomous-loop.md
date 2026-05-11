# Spec: `session.start_autonomous` — bounded self-driving session loops

| field      | value                                       |
| ---------- | ------------------------------------------- |
| spec_id    | mu-036                                      |
| status     | proposed                                    |
| created    | 2026-05-11                                  |
| updated    | 2026-05-11                                  |
| authors    | tcovert + claude-personal (claude-opus-4.7) |
| supersedes | none                                        |

## Why

Today every mu session waits for a human to drive it. `ask_session` injects work; the agent loop runs to `session.done`; control returns to the caller. That works for interactive coding, but it blocks the whole class of work that benefits from a sub-agent running unattended:

- "Watch this directory; if a file matching X changes, summarise the diff and post a callout."
- "Re-run the delegation experiment every Sunday at 3am and write the result to a known file."
- "Refactor every file in this crate that fails `cargo clippy`; loop until clippy is clean or until you've used $1 of budget."
- "Track this issue thread; when a comment from user Y arrives, propose a reply."

These patterns share a shape: a goal + a termination condition + bounded autonomy. mu already has the substrate (sessions, event log, capability attenuation, delegate, input_required) but no primitive for *running a session without a human in the loop*.

mu-036 adds that primitive. After this spec lands:

- `session.start_autonomous { session_id, goal, autonomy_options }` puts a session into a loop where the agent decides its own next action between turns, no `ask_session` required.
- A new state machine — distinct from "ready for ask_session" and "completed" — represents "running autonomously."
- Termination is **capability-enforced**, not goodwill-enforced: bounded by iterations, total tool calls, wall clock, token budget, **or** an external "goal_grader" oracle (which can itself be another mu session). Whichever bound trips first wins.
- A new RPC `session.schedule_wakeup` lets the agent self-suspend until a future time, enabling polling/watchdog patterns without consuming model budget while idle.
- Existing primitives compose: an autonomous session can still escalate to a human via `session.input_required` (mu-029), still spawn delegates with `session.delegate` (mu-031), and operates entirely within its `Capability` (mu-033). Autonomy is just a new axis on the Capability that says "you may run without an ask between turns."

This is the primitive that lets mu host long-running agents — the foundation under "agent that runs in the background while I code," "cron-like scheduled agents," and (eventually, with peer-discovery from a later spec) "cooperating agents that talk to each other without me brokering every message."

CONVENTIONS apply.

## Scope

### In

- **Protocol additions** in `mu-core/src/protocol.rs`:
  - `StartAutonomousRequest { session_id: String, goal: String, options: AutonomyOptions }` and `StartAutonomousResponse { accepted: bool }` — wire method `session.start_autonomous`.
  - `AutonomyOptions { max_iterations: u32, goal_check_interval: u32, goal_check_method: Option<GoalCheckMethod>, escalate_on_idle_after_ms: Option<u64> }`. All bounds are optional **here** because the *real* enforcement bounds live on the session's `Capability` (so a delegate can't widen its own autonomy budget by passing different options).
  - `GoalCheckMethod`: how the loop decides if the goal is met.
    - `SelfReport` (default) — agent emits a `session.callout { kind: "goal_status", body: { satisfied: bool, reason: String } }` at the end of each iteration; the loop terminates if `satisfied: true`.
    - `DelegateGrader { grader_session_id: String, grader_prompt_template: String }` — between iterations, the autonomous session calls `session.ask` against a sibling/delegate session (the grader) with the latest snapshot; terminates when the grader returns `satisfied: true`. This is **the structural grader pattern from the 2026-05-11 delegation experiment** raised to a runtime primitive.
    - `ExternalSignal { signal_name: String }` — the loop waits for a notification with this name (`session.external_signal`) injected by another process. Useful for "stop when CI passes."
  - `ScheduleWakeupRequest { session_id: String, wake_at_unix_ms: Option<u64>, sleep_for_ms: Option<u64>, reason: String }` and `ScheduleWakeupResponse { accepted: bool, scheduled_for_unix_ms: u64 }` — wire method `session.schedule_wakeup`. Exactly one of `wake_at_unix_ms` / `sleep_for_ms` must be set.
  - New `AgentEvent` variants (also event-log payloads): `AutonomousIterationStarted { iteration: u32, motivation: String }`, `AutonomousIterationCompleted { iteration: u32, outcome: AutonomousIterationOutcome }`, `AutonomousScheduledWakeup { wake_at_unix_ms: u64, reason: String }`, `AutonomousTerminated { reason: AutonomousTerminationReason }`.
- **Capability extension** in `mu-core/src/capability.rs`:
  - `Capability` gains `autonomy: AutonomyCapability` field. `AutonomyCapability` is either `Disallowed` (default) or `Allowed { max_iterations: u32, max_wall_clock_ms: u64, max_total_tool_calls_in_autonomy: u32, allow_schedule_wakeup: bool, allow_delegate_grader: bool }`.
  - `attenuate()` intersects these like every other field — child sessions can only have a narrower autonomy budget than parents.
  - A new `CapabilityCheck` variant `DeniedAutonomyDisallowed` so the dispatch refusal is structured.
- **Agent loop changes** in `mu-core/src/agent/loop_.rs`:
  - New state: `RunMode::Autonomous { iteration_count: u32, goal: String, options: AutonomyOptions, started_at: Instant }`. The agent loop's outer driver respects this mode by looping over "decide → act → check-goal" instead of looping on user inputs.
  - Iteration cycle: at the top of each iteration, the agent loop emits `AutonomousIterationStarted` with the model-reported "motivation" (one-sentence "what I'm doing this turn and why"). Body: model picks a tool call, executes it, observes the result, decides whether to call again or finish the iteration. End-of-iteration: run the configured `goal_check_method`, emit `AutonomousIterationCompleted` with the outcome, terminate or continue.
  - Wake-up scheduling: `session.schedule_wakeup` puts the session into `RunMode::Sleeping` with a tokio sleep future. While sleeping, the session does not consume model or tool budget; only the wall-clock budget counts. On wake, returns to `RunMode::Autonomous` with the wake reason injected as the next iteration's `motivation`.
  - Bounds checks at every iteration boundary: capability `max_iterations`, `max_wall_clock_ms`, `max_total_tool_calls_in_autonomy`, plus the per-call options. Whichever trips first terminates with the relevant `AutonomousTerminationReason`.
- **Daemon wiring** in `mu-coding/src/serve/dispatch.rs`:
  - `handle_start_autonomous` validates the session's capability (must include `autonomy: Allowed`), enqueues the autonomous-mode message into the session's input channel, returns accepted.
  - `handle_schedule_wakeup` looks up the session, checks `allow_schedule_wakeup`, signals the agent loop's wakeup channel.
- **Event log** in `mu-core/src/event_log.rs`:
  - New `EventPayload` variants matching the AgentEvent set above. Each iteration's start + completion is recorded so post-hoc analysis can reconstruct the autonomous run, and so a future TUI replay view can step through it.
- **Tests:**
  - Unit (capability): `attenuate` correctly intersects `AutonomyCapability::Allowed` fields and downgrades to `Disallowed` if parent is `Disallowed`.
  - Unit (loop): scripted `RunMode::Autonomous` over a FauxProvider; assert correct iteration counts, that `max_iterations` terminates exactly at the boundary, that `schedule_wakeup` parks the session and resumes it.
  - Integration (forwarder): autonomous session emits exactly the expected event sequence (iteration_started → tool_call → tool_result → iteration_completed → ...) and wire notifications mirror the events.
  - End-to-end smoke: a 3-iteration autonomous run on FauxProvider with `SelfReport` goal-check (satisfied returns true on iteration 2), assert termination at iteration 2.
  - End-to-end smoke (delegate grader): autonomous parent session uses a sibling session as `DelegateGrader`. Both run on FauxProvider with canned responses; assert the autonomous session terminates when the grader satisfies.

### Out

- **Persistence of sleeping sessions across daemon restart.** A daemon restart drops in-flight sessions today; that's broader infra. Wake-up only works if the daemon stays alive. Persistence-across-restart is a separate spec.
- **Cron-style schedule specs.** `wake_at_unix_ms` is a single point in time; "every Monday at 3am" requires a calendar abstraction. Use a higher-level wrapper for now.
- **External `cron` integration.** That's "an outside cron daemon calls `session.start_autonomous`," not part of mu itself. Operators can wire whatever scheduler they want.
- **Goal grader compose semantics** beyond `SelfReport` / `DelegateGrader` / `ExternalSignal`. A "compose two graders with AND/OR" surface is plausible but premature.
- **Cross-session orchestration semantics.** When an autonomous session spawns delegates whose results feed back into its goal-check loop, the delegates *complete* (mu-031) and the parent reads the event log. That works today; nothing new needed.
- **Resource fairness across many autonomous sessions.** If 50 sessions are sleeping/looping, we trust the daemon's existing tokio runtime to schedule them fairly. Fancy quotas later.

## Invariants

- **INV-1 (autonomy is opt-in and capability-gated).** `Capability::root()` has `autonomy: Disallowed` — the default. A session can only enter autonomous mode if its capability explicitly grants it. Cannot be widened by `attenuate()` (intersection only).
- **INV-2 (bounds are enforced, not requested).** `max_iterations`, `max_wall_clock_ms`, `max_total_tool_calls_in_autonomy` are checked at every iteration boundary by the daemon, not voluntarily by the model. A model that "decides to keep going" past `max_iterations` is still terminated.
- **INV-3 (autonomy doesn't escape the session sandbox).** Tool allowlist, file root, budget — every existing `Capability` enforcement applies during autonomy. Autonomous sessions are not a privilege escalation.
- **INV-4 (escalation to human always works).** An autonomous session can emit `session.input_required` (mu-029) at any point; the loop blocks until a human responds (or until `escalate_on_idle_after_ms` itself trips — then the loop terminates with `EscalationTimedOut`). Autonomy includes "ask for help when stuck."
- **INV-5 (sleeping sessions consume no model budget).** While in `RunMode::Sleeping`, the session is durable but quiescent — no provider calls, no tool calls. Wall-clock counts, but nothing else.
- **INV-6 (every iteration is in the event log).** `AutonomousIterationStarted` and `AutonomousIterationCompleted` bracket every iteration. Replays and audits can reconstruct the run without referring to the live session state.
- **INV-7 (`AutonomousTerminated` is always emitted on exit).** Whether the loop hit a goal, a bound, or an error, a single terminal event with the structured reason is the last entry from this session in autonomous mode. Then the session returns to `RunMode::Idle` and is addressable via `ask_session` again — autonomy is a *mode*, not a final state.

## Wire surface

### Start

```jsonc
// request
{
  "jsonrpc": "2.0", "id": 50,
  "method": "session.start_autonomous",
  "params": {
    "session_id": "session-7",
    "goal": "Run `cargo clippy --all-targets`; if it reports warnings, fix one warning per iteration and re-run; stop when clippy is clean.",
    "options": {
      "max_iterations": 12,
      "goal_check_interval": 1,
      "goal_check_method": { "tag": "self_report" },
      "escalate_on_idle_after_ms": 600000
    }
  }
}

// response
{ "jsonrpc": "2.0", "id": 50, "result": { "accepted": true } }
```

### Wake-up scheduling (called from inside the session, via the agent's tool surface or a dispatched RPC)

```jsonc
// request
{
  "jsonrpc": "2.0", "id": 51,
  "method": "session.schedule_wakeup",
  "params": {
    "session_id": "session-7",
    "sleep_for_ms": 3600000,
    "reason": "Recheck CI status in 1 hour."
  }
}

// response
{ "jsonrpc": "2.0", "id": 51, "result": { "accepted": true, "scheduled_for_unix_ms": 1763460389012 } }
```

### Wire notifications (notifications, no id)

```jsonc
{
  "jsonrpc": "2.0",
  "method": "session.autonomous_iteration_started",
  "params": { "session_id": "session-7", "iteration": 3, "motivation": "Reading clippy report for warning #3 (unused_variables in lib.rs:42)." }
}

{
  "jsonrpc": "2.0",
  "method": "session.autonomous_iteration_completed",
  "params": { "session_id": "session-7", "iteration": 3, "outcome": { "tag": "continue" } }
}

{
  "jsonrpc": "2.0",
  "method": "session.autonomous_terminated",
  "params": { "session_id": "session-7", "reason": { "tag": "goal_met", "detail": "clippy reports 0 warnings" } }
}
```

## Implementation sketch

The agent loop already has a top-level `select!` that races provider events, cancels, and (post mu-035) status ticks. Add a wakeup channel and an autonomy state machine:

```rust
enum RunMode {
    Idle,
    Asking { /* existing */ },
    Autonomous {
        iteration: u32,
        goal: String,
        options: AutonomyOptions,
        started_at: Instant,
        tool_calls_consumed: u32,
    },
    Sleeping { wake_at: Instant, reason: String },
}
```

Each iteration starts a sub-conversation: the agent gets a system message that pins the goal and the iteration number, the user message names the prior iteration's outcome, and the model picks one or more tool calls. After tools run, the model is asked "in one sentence: was the goal satisfied this iteration?" and emits a `session.callout { kind: "goal_status" }` callout. The loop reads that callout and decides.

`session.schedule_wakeup` works like `session.input_required` mechanically: it's a "park the loop" primitive. The agent loop transitions to `RunMode::Sleeping`, holds a tokio sleep future, and on wake re-enters `RunMode::Autonomous` with the next iteration injecting the wake reason as motivation.

For `DelegateGrader`, the autonomous loop pauses at the end of each iteration, dispatches an `ask_session` to the grader session (which is just another mu session, possibly delegated from this one), drains the result, parses it, and decides. The grader's response shape is constrained — the grader-side prompt template includes "respond with JSON of shape `{ satisfied: bool, reason: string }`." Validation rejects malformed responses and treats the iteration as "continue" with a warning event.

## Tests

1. **Capability attenuation:** parent `Allowed { max_iterations: 10 }` attenuated by child request `Allowed { max_iterations: 20 }` produces `Allowed { max_iterations: 10 }`. Parent `Disallowed` attenuated to anything stays `Disallowed`.
2. **Bound enforcement:** autonomous session with `max_iterations: 3` runs exactly 3 iterations against FauxProvider, then emits `AutonomousTerminated { reason: { tag: "iteration_cap" } }`.
3. **Wake-up parking:** start autonomous → call `schedule_wakeup` with `sleep_for_ms: 100` → assert session goes to `Sleeping`, no provider calls during sleep, wakes up and continues at iteration N+1.
4. **Goal-met termination:** FauxProvider configured to emit `callout { kind: "goal_status", body: { satisfied: true } }` on iteration 2 → loop terminates at iteration 2 with `reason: { tag: "goal_met" }`.
5. **Delegate grader:** parent + grader sessions on FauxProvider; parent asks grader after each iteration; grader returns `satisfied: true` on the 2nd query → parent terminates at iteration 2. Verifies event log on both sessions.
6. **Escalate-on-idle:** autonomous session with `escalate_on_idle_after_ms: 1000` running tool calls that take 2s → after 1s of no progress, emits `session.input_required` to ask the human "should I keep waiting on this?" Verifies the escalation is gated by the option.

## Risks and follow-ups

- **Runaway loops.** The single biggest risk. Mitigated by INV-2 (bounds enforced server-side) and `AutonomyCapability` being default-off. A misbehaving prompt cannot self-grant more autonomy.
- **Goal-grader-as-a-mu-session is slow.** Each goal-check costs a model call. `goal_check_interval` lets the user trade off cost vs. responsiveness ("only grade every 3 iterations"). For high-frequency loops, prefer `SelfReport` or `ExternalSignal`.
- **Sleeping sessions stuck across daemon restart.** Out of scope, but a real operational concern. Document clearly: "if you restart `mu serve`, sleeping sessions die." A future spec adds persistence.
- **Composition with mu-031 `session.delegate`.** Autonomous sessions often want to spawn delegates for sub-work. This already works — `session.delegate` is callable from inside an autonomous loop. The delegate completes synchronously to the parent's perspective (parent waits on its result). Worth a follow-up: `session.delegate_async` for parents that want to fire-and-forget.
- **Companion: mu-035 observability.** During autonomous loops the `session.provider_status` notifications (mu-035) plus `session.autonomous_iteration_started/completed` give a rich firehose. The TUI's command-centre and session-detail views (per `mu-ui-mockups-2026-05-10.md`) need both to render "iteration 3 of 12, currently thinking (4s)" — the Command Center's "phase:" line is exactly this composition.
- **TUI flows for autonomous sessions:** start (palette command), supervise (subscribe to iteration events, render a progress strip), pause/resume (via `cancel_outstanding` and a future `resume` RPC), terminate (manual cancel). All of these are TUI commands over the same primitives.
- **The peer-discovery / cooperating-sessions thread.** Once two mu sessions can find and negotiate with each other (the "Thread B" still pending), `DelegateGrader` can target a grader running in a *different* mu daemon. That makes "your agent and my agent collaborate" implementable: my agent autonomously works toward a goal, your agent grades it.
