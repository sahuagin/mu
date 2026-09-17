# mu-irc-gateway v1 — puppet nicks

Design pass, 2026-09-16 (bead mu-edr6j). Extends `mu-irc-gateway-v0.md`, whose
exclusion "exactly one gateway nick" this document lifts. Everything v0 fixed
about the mesh side, the daemon, human fronting, framing, exactly-once handling
and loop guards stays as written there; this document only adds nicks.

## Terms

- **Ergo** is the IRC *server* program (one Go binary, formerly Oragono) that
  runs on threadripper as user `claude` from `~/ergo`, listening on 6697 (TLS)
  and 6667 (LAN plaintext). WeeChat, Halloy and the gateway are all *clients*
  of it. Nothing in this document changes Ergo's code; two lines of its
  configuration are affected (see *Ergo limits*).
- **Gateway nick** is `mu-gw`, the one nick the v0 gateway registers with SASL.
  It stays, and keeps the bot verbs.
- **Puppet** is a nick the gateway operates on behalf of exactly one mesh
  agent: the gateway opens one more IRC connection, registers it as
  `cc-c689911a` (say), joins it to `#mu` and to that agent's channel, and from
  then on everything that agent says on the mesh to a human or to another agent
  is spoken on IRC *by that nick*, and anything a human says *to that nick* is
  delivered to that agent. The gateway holds the strings; the agent never
  learns IRC exists (unchanged from v0).

## Why

On the first live round trip (2026-09-16) the operator did the three natural
things and all three failed or misfired: he replied in the private buffer the
agent's DM had opened (v0 fans a bare private line out to every agent,
mu-epniy); he addressed the agent as `cc:c689911a: text` (a peer-id prefix is
not resolvable, and `:` is not legal in an IRC nick anyway, so no client will
ever complete it); and he could not tell which agent a DM in the `mu-gw`
buffer had come from (mu-ifxk2). He asked for what IRC already gives every
other user: `/query` opens one buffer per conversation, `nick<Tab>` completes,
`nick: text` addresses inside a room, and a line's sender is its nick.

Puppets give all four without inventing a convention, and retire mu-epniy and
mu-ifxk2 outright, plus the prefix-match half of mu-wfqtx (labels remain a
separate, later idea).

## Terrain checked (2026-09-16, the operator's Ergo 2.19.1)

- `NICKLEN=32`, `CASEMAPPING=ascii`, `CHANNELLEN=64` from ISUPPORT.
- Nick alphabet: `cc-c689911a` → `001 Welcome`; `cc:c689911a`, `cc.c689911a`,
  `mu:bb9c:s1` → `432 Erroneous nickname`. So a mesh peer id can never be a
  nick; the IRC form must be derived, the way channels already are.
- `ip-limits`: `max-concurrent-connections: 16` per IPv4 /32, throttle 32
  connections per 10 min, `exempted: [localhost]` only. The gateway host
  (aiteam, 10.1.1.11) is not exempt today.
- `require-sasl` is on with 10.1.1.0/24 exempted; account registration is
  closed. Puppets therefore connect *unauthenticated* from the LAN and are
  unregistered nicks — which is what we want: nothing to provision per agent.
- Agent channels are created by `mu-gw`, which holds `@` in them, with modes
  `+Cnt`: `n` means a non-member cannot PRIVMSG the channel, so a nick may only
  speak in channels it has joined. That rules out "the sender's puppet speaks
  in the target's channel" unless every puppet joins every channel.
- `draft/relaymsg=/` is advertised. Ergo's RELAYMSG lets a channel operator
  speak in *that channel* as a relayed identity carrying the separator
  (`cc-c689911a/mu`) with no connection and no membership for the relayed
  nick. It cannot receive and has no private form, so it cannot carry
  `/query`; it is exactly the tool for cross-channel attribution. See
  *Alternatives* and behaviour 6.
- Discovery today lists 12–16 agents: a handful of cc sessions, a few mu
  daemons, and short-lived review-panel seats that appear as a daemon plus a
  `session-1`.

## Observable behaviour (what the operator sees)

1. Every live agent that qualifies (see ruling A) is present on the server as
   a nick of the form `<role>-<id>[-<sub>]`, sanitized to the nick alphabet and
   fitted to `NICKLEN`: `cc-c689911a`, `mu-bb9c4b94-session-1`. `mu peers`
   prints the nick beside the peer id and channel.
2. The puppet is in `#mu` and in its own channel (`#cc-c689911a-…`), so
   `/names #mu` and tab completion see it.
3. `/query cc-c689911a` and a line typed there → one DM from `human:<nick>` to
   that agent, nobody else. This replaces the v0 private-buffer fan-out.
4. A line in `#mu` or any channel beginning `cc-c689911a: ` (or `,`) → one DM
   to that agent, the standard IRC address convention. A bare line in `#mu`
   still fans out (it is the lobby). The v0 peer-id spelling `cc:<full id>: `
   keeps working as the gateway's own escape hatch.
5. An agent's DM to a human arrives as a PRIVMSG **from the puppet**, into
   the `/query` buffer (private precedence) or into the agent's own channel
   when the human last addressed *that agent* there (remembered-channel
   precedence). The v1 routing memory is keyed by the **(human, agent) pair**.
   v0 keys it by human alone (`remembered: human → channel`, written in
   `bridge/session` from `MemoryUpdate.human`, read in `routing::route_to_human`
   by folded nick), so today a human who addresses A in `#cc-A` and then B in
   `#cc-B` gets A's later reply in `#cc-B`; it goes unnoticed only because the
   single gateway nick is in every channel. With puppets that reply would be
   asked of a nick that is not a member. Keying by pair makes "reply where you
   addressed me" true per agent, and a human who never addressed this agent in
   its channel gets its reply privately. The voice precedence of behaviour 6
   remains the safety net for any channel a puppet cannot speak in.
6. An agent-to-agent DM the observer sees is mirrored into the *target's*
   channel with the **sender's identity**, by whichever voice can legally
   speak there, in this order: the sender's puppet itself when it is a member
   of that channel (only its own channel and `#mu`; puppets never join other
   agents' channels, see *Nick mapping contract*); otherwise `mu-gw` via
   `RELAYMSG <channel> <sender-nick>/mu :<body>`, which shows in the room as
   `cc-c689911a/mu` — attribution without a membership; otherwise (RELAYMSG
   not advertised, or `mu-gw` not an operator of that channel because a human
   created it first) `mu-gw` with the v0 `[peer]` label. Never dropped. A
   sender with no puppet at all (ruling A, over the cap, not yet registered)
   takes the same RELAYMSG-then-label path with the nick `nick_for` would
   have given it.
7. When the agent leaves the mesh the puppet QUITs (its channels show the
   part); when it returns it rejoins under the same nick. Absent-target
   traffic still goes to the lobby with the v0 label.
8. `mu-gw` remains the place for `mu peers` / `mu say` and for gateway
   notices. A bare private line to `mu-gw` is *refused* with a one-line hint
   naming the puppet to `/query` instead (ruling C); it no longer fans out.

## Nick mapping contract

- Pure function `nick_for(peer, nicklen) -> String`, beside `channel_for` in
  `mapping.rs`, sharing `sanitize` and `hash_tail`: `role-id[-sub]`, then if
  over `nicklen` the id is cut and the 8-hex `hash_tail` appended so the result
  is stable per peer and inside budget. First character must be a letter (nick
  grammar): roles are letters today; enforce, do not assume.
- Reverse resolution is by **table, never by parsing**: the pool owns
  `folded nick → peer id` for the nicks it holds, folded under the live
  CASEMAPPING like every other name in this crate. A nick that is not in the
  table is a human or a stranger.
- Collisions: two peers whose nicks fold equal get the hash-tail form (same
  rule as shared channels, but a nick cannot be shared, so both sides get the
  tail). A nick already held by *someone else* on the server (433 on
  registration, or a human who joined first) is never contested: the puppet
  registers with the hash-tail form instead, and if that is taken too the agent
  stays channel-only for this session with a gateway notice. Humans keep their
  names.
- A puppet's membership is exactly two channels: `#mu` and its own agent
  channel. It never joins another agent's channel, on demand or otherwise;
  cross-channel speech is RELAYMSG or the labelled gateway line (behaviour 6).
  Join volume is therefore 2 per puppet, not N² per roster.
- Membership already distinguishes humans from the gateway's nick; it gains the
  **set** of gateway-owned nicks (gateway nick + every puppet) as the one new
  fact it needs, so puppets are never fronted as `human:` peers and never count
  as human presence. Relayed identities (`…/mu`) are never members of
  anything and never appear in NAMES.

## Connections and lifecycle

- One `transport` connection per puppet, TLS by default with the configured
  trust, **no SASL** (the config forbids `sasl_*` under `[irc.puppets]`; the
  server must exempt the gateway host from `require-sasl`, which the LAN rule
  already does). The `adapter` state machine is reused per connection unchanged
  — it already supports optional SASL.
- A new `puppets` module (pure) owns the pool's *decisions*: which peers should
  have puppets given the discovery snapshot and the rulings; what nick each
  gets; which are connecting, registered, backing off, or given up; what to do
  on 433/432; and the fan-in tag for inbound lines. The bridge's session loop
  executes them, exactly as it executes routing and outbound decisions today.
- Fan-in, with a **single-consumer rule**. Every puppet and `mu-gw` sit in
  `#mu` and in the agent channels, so Ergo delivers one human channel line to
  N+1 gateway-held connections. Only one copy may be routed, and the rule is
  by *class*, decided at the fan-in before any routing sees a line:
  - **Channel-class input** — a PRIVMSG/NOTICE to any channel, and every
    membership event (JOIN/PART/KICK/QUIT/NICK/NAMES) — is consumed **only
    from the gateway connection** (`mu-gw`). `membership` therefore keeps
    exactly one authority, as in v0, and a human line in `#mu` or in an agent
    channel is routed once, whoever it addresses.
  - **Private-class input** — a PRIVMSG/NOTICE whose target is the receiving
    connection's own nick — is consumed **only from that connection**: a line
    to `cc-c689911a` arrives once, on the puppet, tagged with its peer id; a
    line to `mu-gw` arrives once, on the gateway connection.
  - Everything else on a puppet stream (its own registration numerics, PING,
    ISUPPORT, 432/433, ERROR) is handled by that puppet's adapter and never
    reaches routing; channel-class lines that Ergo also sends to a puppet are
    dropped at the fan-in and counted, not routed.
  The exactly-once machinery in `routing`/`recent` is keyed on mesh ids and
  covers the mesh→IRC overlap only; it is not used for this, and the plan
  claims nothing from it here. The merged stream feeds the one state-owning
  select loop; nothing in the bridge grows a second state owner. Outbound
  writes go to the puppet's own bounded writer; a puppet that is not
  registered yet falls back to `mu-gw` with the v0 label.
- Pacing: at most `connect_parallelism` (default 2 — landed so in increment
  1: Ergo throttles 32 connections per 10 minutes per IP, and two in flight
  with backoff stays well under it) registrations in flight, a per-puppet
  backoff on failure (same 2 s → 5 min schedule as the main connection), and
  a `min_age` (default 60 s, i.e. two discovery sweeps) before a newly
  discovered agent gets a puppet, so a review seat that lives a minute never
  costs a connection. A gateway restart reconnects puppets under the same
  pacing; no reconnect storm.
- Cap: `max` (default 16 — landed so in increment 1: Ergo's per-IP
  `max-concurrent-connections` default, so an unexempted host degrades to
  channel-only rather than refused connections; raise it with the exemption)
  live puppets; agents beyond it are channel-only with a notice in
  `mu peers`. Nothing is queued across a reconnect on either side (v0 rule).
- **Outbound enqueue is non-blocking.** The state-owning loop never awaits a
  puppet writer. A delivery is `try_send` into that puppet's bounded queue;
  `Full` or `Closed` means that puppet is stalled or gone, and the decision's
  voice precedence simply continues — RELAYMSG on the gateway writer, then the
  labelled gateway line — with the fallback counted per puppet. If the gateway
  writer itself is full the line is dropped and counted, as v0 does today. One
  stalled puppet can therefore delay nothing but its own voice. Test
  (increment 2b): a puppet whose writer is deliberately never drained does not
  stop discovery, routing, or another puppet from progressing, and a line
  addressed through it arrives by the next voice.
- **Ownership and teardown.** Puppets are owned by the gateway `Session`, which
  v0 rebuilds empty on every registration of the main connection. So: when the
  main connection drops, every puppet is torn down with it and the pool
  re-populates, under the same pacing, only after `mu-gw` has re-registered;
  when the operator stops the process, the pool is torn down before the main
  connection. Teardown is one bounded step for the whole pool: every
  registered puppet is sent `QUIT` in parallel with the same grace v0 gives the
  main connection, pending registrations and backoff timers are cancelled
  immediately, queued writes are discarded (never drained — nothing is held
  across a reconnect), and at the deadline any transport still open is closed
  through its `Lifecycle` (v0's "a connection is a unit"), so the bound holds
  even against a stalled server. An agent leaving the mesh tears down its one
  puppet the same way, individually. Test (2b): with N puppets, one of them on
  a transport that never acknowledges, shutdown completes within the deadline
  and no transport task outlives the session.

## Ergo limits (operator steps, once)

- `limits.ip-limits.exempted`: add the gateway host (`10.1.1.11`, or
  `10.1.1.0/24`), else the 17th connection from aiteam is refused and a
  restart trips the 32-per-10-min throttle. HUP to apply.
- Nothing else. `require-sasl` exemption and `relaymsg` are as they are.

## Module changes

| module | change |
| --- | --- |
| `config` | `[irc.puppets]`: `enabled` (default true), `roles` (ruling A), `max`, `min_age_secs`, `connect_parallelism`; refuses `sasl_*` here; TLS settings inherited from `[irc]`. `--check-config` prints it. |
| `mapping` | `nick_for`, nick-alphabet `sanitize`, `NICKLEN` fitting, the relayed form `<nick>/mu`; tests pin every current role and the collision/tail rules. |
| `framing` | a `RELAYMSG <channel> <nick> :` variant of the line budget; same UTF-8 and CR/LF rules. |
| `puppets` (new) | pool decisions, reverse table and the single-consumer class filter; offline-testable, delivered before any bridge wiring (increment 2a). |
| `membership` | gateway-owned nick set (increment 2a, wired before any puppet connects in 2b); humans = present nicks minus that set. |
| `outbound` | sender authorization unchanged; new destinations: a private line to a puppet nick → that peer; `nick: ` / `nick, ` prefix in a channel → that peer; bare private line to `mu-gw` → refuse with hint; own-nick guard becomes own-nick-set guard; `MemoryUpdate` names the agent as well as the human. |
| `routing` | `remembered` keyed by (folded human, agent peer id); a decision now names its **voice**: `Puppet(peer)` when the sender's puppet is registered and a member of the target (private, own channel, `#mu`); `Relay(nick)` for a channel the puppet is not in, when RELAYMSG is available and `mu-gw` is an operator there; else `Gateway` with the v0 label. Exactly-once keys unchanged (mesh id, destination, session). |
| `bridge/session` | N transports, tagged fan-in behind the single-consumer class filter, per-puppet writers, executes pool decisions on the discovery tick and on registration events; executes `Relay` as `RELAYMSG` on the gateway writer and tracks per-channel operator status from MODE/NAMES for the `Relay` precondition. |
| `bridge/mesh_side` | unchanged. |
| `main.rs`, README | operator material: the ircd.yaml exemption, the new table, what `mu peers` shows. |

## What does not change

The mesh side, the daemon, the `human:<nick>` capability assertion and human
fronting (the fix in #653 is what puppets deliver through), framing, the
`+mu.id` tag, exactly-once handling, both loop guards' *principles* (the own
identity is now a set), the bot verbs, the lobby fan-out for a bare channel
line, and the per-agent channels (ruling B).

## Risks

- **Connection churn.** Review seats come and go every few minutes and each
  shows up as a daemon *and* a session. Ruling A drops the daemon half;
  `min_age` (60 s) drops seats that die inside two sweeps. A seat that lives
  longer than that **does** get a puppet for the rest of its life, so a board
  run of five seats costs up to five connect/register/join/quit cycles and
  five of the `max` slots for ~15 minutes. That is the expected steady state,
  not a failure; if it proves noisy the knobs are `min_age` (raise it) and
  the shape filter (exclude `session-*` peers of daemons that have no other
  sessions, which is what a seat looks like). Watch the Ergo log for
  connect/disconnect volume after the first week.
- **RELAYMSG precondition.** `mu-gw` must be an operator of the channel to
  relay into it, which holds for channels it created. A channel a human
  created first (as `#mu` was) leaves `mu-gw` without `@`, and mirrors there
  fall to the labelled form. Visible in the log as a counted fallback, not a
  loss.
- **Nick squatting.** A human can take `cc-c689911a`; the puppet yields (hash
  tail, then channel-only). Cosmetic, and visible in `mu peers`.
- **Two speakers for one agent.** Before a puppet registers, the agent's
  traffic is voiced by `mu-gw` with a label; after, by the puppet. A room can
  show both spellings for the same agent across that boundary. Accepted; the
  label makes it legible.
- **Server-side state.** Puppets are unregistered, so Ergo keeps no always-on
  state for them: when a puppet is down, a `/query` line to it gets Ergo's
  `401 No such nick` — the human learns immediately that the agent is absent,
  which is better than v0's silent lobby fallback for that case.

## Alternatives considered

- **RELAYMSG alone** (Ergo's bridge extension): attribution in channels with
  no connections, `nick/relay` marker. No receive path and no private
  messages, so `/query` and `nick:` addressing are impossible; it cannot be
  the whole mechanism. It *is* the mechanism for the one case a puppet cannot
  cover — speaking in a channel it is not in — and for senders with no puppet,
  which is how behaviour 6 uses it.
- **Puppets join every agent channel** so the sender's puppet can speak
  anywhere: N² joins, N× the membership state, and a room full of nicks that
  never talk there. Rejected in favour of RELAYMSG for cross-channel speech.
- **One registered account per agent with always-on.** Registration is closed
  by design and per-agent provisioning is exactly the ceremony to avoid.
- **Prefix matching / labels on the single-nick design** (mu-wfqtx): fixes
  typing, not buffers, tab completion or attribution. Labels remain worthwhile
  *on top of* puppets (a puppet's realname/`away` text is the natural place).

## Ordered increments (each its own bead and PR)

1. `config` + `mapping`: `[irc.puppets]`, `nick_for`, alphabet and `NICKLEN`
   rules, reverse-table type; tests. No behaviour change when `enabled=false`.
2a. `puppets` pool, at its seam, no bridge: the pool's decisions (which peers
    get puppets under the rulings, nick assignment through `nick_for` and the
    reverse table, connect/register/backoff/give-up states, 432/433 handling,
    the single-consumer class filter as a pure function over a tagged line)
    with offline tests over discovery snapshots and adapter events. **Also
    here: `membership` learns the gateway-owned nick set** — `joined`,
    `names_reply` and `present_humans` subtract it, so a JOIN/NAMES entry for
    an owned nick produces no `HumanEffect::Register` and no human presence;
    pinned by tests that feed puppet JOINs and NAMES through membership. v0
    membership knows only the single `self_nick` and would otherwise front
    every puppet as `human:<nick>` the moment it joins `#mu`. Nothing runs the
    pool yet; `enabled=true` changes nothing observable.
2b. Bridge integration, **prerequisite: 2a's membership exclusion wired** —
    the bridge hands membership the owned set *before* the first puppet is
    told to connect, and updates it as puppets register or give up, so there
    is no window in which `mu-gw` observes an unowned puppet JOIN. Then: N
    transports, tagged fan-in behind the class filter, per-puppet writers,
    pool decisions executed on the discovery tick and on registration events;
    `mu peers` shows nicks. Live harness cases: discovered peer → nick appears
    in `/names #mu` within two sweeps; `agent dialogue peers` and the
    gateway's fronted set show **no** `human:<puppet-nick>` while N puppets
    are joined; one human line in `#mu` is routed exactly once with N puppets
    present.
3. IRC → mesh through puppets: `/query` line and `nick: ` prefix → one DM;
   bare private line to `mu-gw` refused with hint; own-nick-set guard; routing
   memory re-keyed by (human, agent) with the v0 misattribution pinned by a
   test: address A in `#cc-A`, then B in `#cc-B`; A's reply lands in `#cc-A`
   and B's in `#cc-B` (v0 sends both to `#cc-B`), and an agent C the human
   never addressed in its channel replies privately.
4. Mesh → IRC through puppets: agent → human from the puppet; observed agent →
   agent mirrored with the sender's identity by voice precedence (puppet where
   a member, else RELAYMSG, else `mu-gw` + label); the RELAYMSG framing
   variant and operator-status tracking. Closes mu-epniy and mu-ifxk2.
5. README/operator page, `--check-config` output, plan amendments. Operator
   applies the ircd.yaml exemption before increment 2b is exercised live.

## Open rulings (decision memo)

**A. Which agents get puppets — DECIDED 2026-09-16 (operator): (2).**
Options were: (1) every discovered peer; (2) session-shaped peers only —
`cc:<id>` and `mu:<daemon>:<session>` — with bare daemons `mu:<daemon>`
channel-only; (3) an explicit allowlist. (2) is the default of the shape
filter: a DM to a bare daemon needs a session named anyway, so a daemon puppet
would be a nick nobody can usefully talk to, and today's roster shows each
review seat as a daemon *and* a session, so (2) halves the connection count.
(1) stays one config line away (`[irc.puppets] daemons = true`).

**B. Keep the per-agent channels once puppets exist?** Options: keep; retire.
Recommendation: **keep in v1**. They carry the observed agent→agent traffic and
the remembered-channel reply rule; with puppets they read better, not worse.
Reassess after a week of use; retiring them is a deletion, not a design.

**C. A bare private line to `mu-gw`.** Options: keep the v0 fan-out; refuse
with a hint. Recommendation: **refuse with a hint** — the lobby is the
fan-out, and a private line to the gateway nick that reaches every agent is the
exact misfire the operator hit first.

A is decided. B and C are needed before increments 4 and 3 respectively;
increments 1 and 2a depend on neither.
