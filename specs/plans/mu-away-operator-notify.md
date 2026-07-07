# mu away-operator notification channel (design)

| field | value |
| --- | --- |
| status | design proposal — needs operator decisions before build |
| date | 2026-07-06 |
| bead | (file under mu tracker on acceptance; roadmap item 2 remainder) |
| terrain checked | mu-solo notify surface, serve forwarder, autonomy events, grep for existing out-of-band channels |

## Problem

Roadmap item 2 (current-state doc): *"long tool waits, parks, errors, and
`input_required` need to reach an away operator through a visible
notification surface."*

What exists today reaches only an **attached** operator:

- mu-solo maps autonomy transitions to notification bodies
  (`crates/mu-solo/src/app.rs` `autonomy_notification_body`: `input_required`,
  `autonomous_scheduled_wakeup`, `autonomous_iteration_started/completed`
  incl. `escalating_to_human` / `iteration_error` / `goal_met`,
  `autonomous_terminated`) and long tool waits
  (`long_tool_notification_body`, 8s threshold).
- Delivery is an OSC 99 escape to the enclosing terminal, gated on the pane
  being unfocused (`crates/mu-solo/src/notify.rs::should_notify`), and each
  emission is journaled (`viewport.rs::journal_notify`).
- The daemon has **no out-of-band channel at all** (Observed: grep for
  ntfy/pushover/webhook/slack/telegram/smtp across `mu-coding`, `mu-core`,
  `scripts/` — zero hits).

Consequence: a genuinely-away operator — autonomous overnight run, no
attached TUI, walked away from the desk — learns about a parked ask or a
dead run only when they come back. The parked-ask rejection work (f4ib.2)
made the state *visible*; nothing makes it *reach out*.

## Design principles (from repo invariants + operator standards)

1. **Daemon-side.** Frontends are hats; the away case is precisely "no hat
   attached." The emitter must live with `mu serve`.
2. **Event-driven.** No polling loop watching logs; hook the existing
   notification flow at emission time (operator event-driven standard —
   see mu-rkhj's rejection of poll+cursor designs).
3. **Transport-neutral, operator-owned.** No hardcoded endpoint, vendor,
   or protocol in mu (same rule that killed hardcoded LAN IPs in
   agent_tools, at-xiw). The operator configures a command; mu invokes it.
4. **Fail-open and non-blocking.** A notification failure must never block
   or fail the agent loop. Fire-and-forget with a timeout; failures are
   logged and journaled, not fatal (mirrors the two-tier durability
   stance: this is best-effort, not write-ahead).
5. **Rate-limited and deduplicated.** Autonomous loops can emit bursts;
   the channel must not become a pager storm (per-session min-interval,
   collapse repeats of the same kind).
6. **Shareable-log discipline.** Payloads carry ids, kinds, timestamps,
   and short summaries — never message bodies, tool arguments, or
   anything secret-shaped. The payload should be safe to relay through a
   third-party push service.

## Proposal (v1)

A `[notify]` config section, daemon-side, consumed by `mu serve`:

```toml
[notify]
# argv, invoked per event; payload arrives as one JSON object on stdin.
command = ["mu-notify-relay"]           # operator-owned script; no default
# which event kinds fan out; everything else stays journal-only
events = ["input_required", "parked", "autonomous_terminated",
          "iteration_error", "long_tool_wait"]
min_interval_secs = 60                  # per (session, kind) floor
timeout_secs = 10                       # kill the relay if it hangs
```

Payload (one JSON object, stdin):

```json
{"kind": "input_required", "daemon": "d-...", "session": "s-...",
 "ts": "2026-07-06T23:41:00Z", "summary": "approval requested: bash",
 "wake_at": null}
```

Mechanics:

- Hook point: the serve-side notification fan-out the frontends already
  consume (the forwarder emits `session.*` notifications from typed
  events) — one choke point, no new event types, the daemon-side emitter
  is just another projection consumer. The summaries reuse the same
  mapping mu-solo's `autonomy_notification_body` encodes (candidate:
  lift that mapping into `mu-core` so both consumers share it).
- Spawn `command` detached with the JSON on stdin, wall-clock timeout,
  stderr to the daemon log. No shell — argv exec only.
- Journal every attempt (`kind="notify"`, `trigger`, delivered/failed) —
  parallel to mu-solo's `journal_notify`, so "did it page me?" is an
  event-log query, not a mystery.
- `command` unset ⇒ feature entirely off (today's behavior).

## Alternatives considered

- **mu-dialogue push to an operator peer.** Couples operator paging to
  the dialogue substrate whose inbound-push design (mu-rkhj) is itself
  unresolved, and dialogue delivery targets *sessions*, not humans.
  Complementary, not competing: agent→agent stays dialogue; daemon→human
  is this channel.
- **Built-in ntfy/pushover/webhook client.** Hardcodes a vendor + adds
  an HTTP credential surface inside the daemon. The argv seam gets the
  same result with the credential held by the operator's script.
- **Status quo (OSC 99 + always-attached tmux).** Works only while a
  terminal emulator relaying OSC 99 stays connected; fails exactly the
  away/headless case this targets.

## Operator decisions needed (do not build past these)

1. Transport: is an operator-owned relay script (`command = [...]`) the
   right seam, or do you want a built-in transport after all?
2. Default event set — in particular whether `autonomous_scheduled_wakeup`
   (parks with a known wake time) should page or stay journal-only.
3. Should sessions with an autonomy grant auto-enable notify when
   `[notify].command` is configured, or is it per-session opt-in at
   create (a `CreateSessionRequest` field)?
4. Where the summary mapping lives: lift `autonomy_notification_body`
   into `mu-core` (shared projection) vs duplicate daemon-side.

## Build sketch (after decisions)

1. `mu-core`: shared summary mapping (if decision 4 says lift).
2. `mu-coding` serve: `[notify]` config parse + emitter subscribed at the
   forwarder fan-out + journal entries + rate limiter. Unit tests with a
   fake command (a script that appends to a file).
3. Smoke: `just smoke`-style faux run driving `input_required`, assert
   the fake relay received one payload and the journal recorded it.
