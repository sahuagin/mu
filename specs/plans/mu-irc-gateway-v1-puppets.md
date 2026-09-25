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
  trust, authenticating as its leased slot with **SASL EXTERNAL** — the client
  certificate provisioned for that account (see *Provisioning*, below; this
  supersedes the original "no SASL, rely on the `require-sasl` LAN exemption").
  A `sasl_*` PASSWORD key under `[irc.puppets]` is still refused: it names a
  mechanism the design does not use. The `adapter` state machine is reused per
  connection — it already carried optional SASL, and gains EXTERNAL beside
  PLAIN.
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
- Pacing, three bounds that do three different things (increments 1 and 2a):
  `connect_parallelism` (default 2) bounds how many registrations are *in
  flight* at once; a per-puppet backoff on failure (same 2 s → 5 min schedule
  as the main connection) bounds how fast *one* peer retries; and the pool's
  rolling **attempt budget** — at most 24 connection attempts started per
  10-minute window across all puppets, re-offers after a `433` included —
  bounds the aggregate *rate*, which the first two do not (a freed slot
  refills at once and backoff is per peer, so churn over many peers could
  otherwise exceed Ergo's 32 connections per 10 minutes per IP). The budget
  keeps 8 attempts in reserve for `mu-gw`'s own reconnects on the same
  address, and it is the one piece of pool state that OUTLIVES the pool: the
  session rebuilds its pool empty on every main-connection registration, but
  Ergo's window does not reset with it, so the bridge holds the attempt
  history outside the session and hands it to each new pool. A registration
  that drops before 60 s of stable uptime continues its backoff schedule
  rather than restarting at 2 s, as the main connection does. A `min_age` (default 60 s, i.e. two discovery sweeps) before a
  newly discovered agent gets a puppet keeps a review seat that lives a
  minute from ever costing a connection. A gateway restart reconnects
  puppets under the same pacing; no reconnect storm.
- Cap: `max` (default 16) live puppets, *not* counting `mu-gw` — so the host
  holds up to `max + 1` connections. Ergo's per-IP `max-concurrent-connections`
  is 16 with only localhost exempt, which is why the operator's exemption
  (below) precedes 2b going live: without it the 17th connection is refused
  and that puppet backs off like any other failed connection (it does not
  become channel-only — channel-only is for a nick that cannot be had or a
  peer beyond `max`). Agents beyond the cap are channel-only with a notice in
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
| `config` | `[irc.puppets]`: `enabled` (default true), `roles` (ruling A), `max`, `min_age_secs`, `connect_parallelism`, `slot_prefix`, `slot_certs_dir`; refuses `sasl_*` password keys here; TLS settings inherited from `[irc]`. Every slot name is checked as a nick and every slot credential is parsed at load, so `--check-config` refuses a pool that could not register. |
| `mapping` | `nick_for`, nick-alphabet `sanitize`, `NICKLEN` fitting, the relayed form `<nick>/mu`; tests pin every current role and the collision/tail rules. |
| `framing` | a `RELAYMSG <channel> <nick> :` variant of the line budget; same UTF-8 and CR/LF rules. |
| `puppets` (new) | pool decisions, reverse table and the single-consumer class filter; offline-testable, delivered before any bridge wiring (increment 2a). |
| `membership` | gateway-owned nick set (increment 2a, wired before any puppet connects in 2b); humans = present nicks minus that set. |
| `outbound` | sender authorization unchanged; new destinations: a private line to a puppet nick → that peer; `nick: ` / `nick, ` prefix in a channel → that peer; bare private line to `mu-gw` → refuse with hint; own-nick guard becomes own-nick-set guard; `MemoryUpdate` names the agent as well as the human. |
| `routing` | `remembered` keyed by (folded human, agent peer id); a decision now names its **voice**: `Puppet(peer)` when the sender's puppet is registered and a member of the target (private, own channel, `#mu`); `Relay(nick)` for a channel the puppet is not in, when RELAYMSG is available and `mu-gw` is an operator there; else `Gateway` with the v0 label. Exactly-once keys unchanged (mesh id, destination, session). |
| `bridge/session` | N transports, tagged fan-in behind the single-consumer class filter, per-puppet writers, executes pool decisions on the discovery tick and on registration events; executes `Relay` as `RELAYMSG` on the gateway writer and tracks per-channel operator status from MODE/NAMES for the `Relay` precondition. |
| `bridge/mesh_side` | unchanged. |
| `main.rs`, README | operator material: the ircd.yaml exemption, the new table, what `mu peers` shows. |

## Membership's puppet nicks: ownership, departure, the window (increment 2b-i)

Membership owns the one fact routing needs — which present nicks are humans —
and puppets complicate it in time, not in kind: a puppet's nick is ours from
before its JOIN can be echoed until its departure is *observed on the main
connection*, and "observed" is the whole design.

- **Owned.** The pool's listing, handed in by the bridge before any pool
  action (the ownership sync), keyed by the folded spelling and carrying the
  CONNECTION (the executor's attempt id). Never human, never present. A sync
  is reconciled by connection, not spelling: the pool's spelling for a
  connection can lag membership's (a rename membership applied from the wire
  or at the barrier reaches the pool later — the entry is marked `moved` until
  the pool lists the new spelling) or lead it (a rename the pool learned first
  — the entry moves as a rename does, never retired); a listing of a
  connection whose puppet was seen to QUIT holds nothing, whatever spelling it
  names.
- **Retiring.** A nick the pool released (its QUIT queued or in flight) stays
  ours until its departure is observed: the puppet's QUIT on the wire, or the
  executor's report that the puppet's own connection closed, resolved by
  connection (a report about an earlier connection cannot free a later one
  under the same spelling). A snapshot line naming it is neither fronted nor
  committed.
- **Gone.** A still-listed nick whose QUIT was already observed: the release
  has nothing to wait for, and whoever appears under the name meanwhile is a
  human. Remembered per spelling and per connection.
- **The story.** What the wire says under a retiring (or vacated) name — JOIN,
  PART, a snapshot line, a NICK — is held back, in order, stamped with the
  channel's snapshot generation, and decided at resolution: the puppet's own
  observed QUIT discards it (it was in a shared channel; everything before its
  QUIT was the puppet), the executor's CONFIRMED report replays it as the new
  holder's (the puppet was in no shared channel, so nothing of its own could
  have arrived; the bridge orders the report behind a barrier on the main
  connection so every line the server wrote before closing the puppet's has
  been read first), an UNCONFIRMED report discards it (nothing can tell). The
  holder's NICK moves the entry and the story with them; a snapshot supersedes
  what was held back before it was requested when it *commits*; a replayed
  arrival older than an open snapshot is live presence but not that snapshot's
  evidence, a replayed departure is evidence whatever its age.
- **The rename window** (the increment above): a rename the puppet's own
  connection reports vacates the spelling it left and expects the new one
  until the bridge's rename barrier applies it; each hop settles its own
  spelling and the names its holders went on to; the wire's own NICK settles
  a hop early; the puppet's QUIT under the expected name settles the whole
  window; a release keeps it; the departure report settles what is left.

Precondition the bridge must keep (checked at `bridge/session`): release
first (the ownership sync), then report; and a confirmed report only after the
departure barrier's PONG. The residuals are named where they are accepted: an
unconfirmed departure; a server whose EOF is a cut link rather than its own
close.

## Agent identity on IRC: requirements, partial solutions, open questions (2026-09-23)

The rename window of increment 2b-i took twenty board runs on PR #679 without
converging. Four of its last six findings were defects introduced by the
previous run's fix, and the genuine ones before that were all one shape: two of
the window's four spelling-keyed maps held the same key and the wrong one won.
That is a representation problem. This section restates what the gateway needs
from IRC identity, what IRC already provides for it, and what is still open.

### The problem, in one paragraph

A mesh peer has a stable id (`cc:c689911a-…`). IRC has no stable id on the
wire: a sender is a nick, a target is a nick, and a nick can change at any
moment. The gateway therefore translates nick → peer at the boundary, and that
translation is what every defect has been about. The window tried to keep the
translation correct *at every instant* across the gap between a rename and the
gateway learning of it, which requires modelling who might hold each spelling.
Asking the server instead is both cheaper and stronger.

### Requirements

- **R1 — never front an agent as a human.** Fronting publishes a `human:<nick>`
  peer that does not exist; other agents may then address it. There is no
  recovery, so this is absolute and takes precedence over every other
  requirement below.
- **R2 — never claim a name the server has given to someone else.** The gateway
  must not answer for, or suppress, a real person.
- **R3 — a human who takes a name an agent has released becomes visible within
  one round trip** of the event. Not instantaneously; bounded and self-healing.
- **R4 — an agent's rename reaches the pool and membership within one barrier
  round trip.**
- **R5 — names are legible to the operator.** `cc-97489376` is not: he cannot
  tell which session it is without keeping his own map. A nick should be able
  to carry a human-meaningful label (`claude-pr777`), and the long description
  should be visible where IRC already shows one.
- **R6 — bounded state, bounded provisioning.** O(1) names per connection, no
  per-spelling history; and no per-session artefact on the server that has to
  be garbage-collected (sessions get a fresh UUID at init, so anything created
  per session accumulates).
- **R7 — forgery is hard.** A human must not be able to present as an agent by
  taking its name.

### What IRC already provides (verified in the four research streams)

- **Identity that survives a rename: the services account.** `account-tag` puts
  `@account=…` on inbound lines, `extended-join` carries it on JOIN, `WHOX`
  (`WHO #chan %tcuhsnfa,<token>`) returns it for everyone already present, and
  `account-notify` reports changes. This answers "is this line one of ours?"
  per message, with no state of ours, and unforgeably (R1, R2, R7).
- **Holding a name: account-based nick reservation.** A nick registered to an
  account is refused to others — while connected and after disconnect, because
  reservation belongs to the account. This is what removes the hazard the whole
  window existed for (R2, R7); it is unavailable on public networks, which is
  why the Matrix bridge has a decade of ghost-nick issues and we need not.
- **Several names for one identity: `NS GROUP`.** An account can hold more than
  one nick — a server-enforced alias set rather than a timeout (R3, R5).
- **A human-readable description: realname and `SETNAME`.** Live-updatable, and
  `draft/whoami` (Ergo 2.19) gives the authoritative self-prefix at connect
  (R5).
- **"Tell me when that name frees up": `MONITOR`.** Push, not poll — `730`/`731`
  on a nick becoming used or free (R3). It answers spelling only, never
  identity, so it pairs with WHOX rather than replacing it.
- **"What is my nick?": the `001` reply and the NICK echo.** Never what we sent:
  Ergo returns the nick it *assigned*, which can differ (guest format, account
  name, confusable rejection).
- **Membership snapshots: `NAMES` / `WHOX`,** with `no-implicit-names` (ratified
  in Ergo 2.19) to control when they arrive.

### Partial solutions, and what each leaves open

1. **Account per role, nick as a label.** One account per agent ROLE (`cc`,
   `mu`) rather than per session: identity and ours-ness come from the account
   (R1, R2, R7), the nick is free to carry the operator's label (R5), and
   nothing accumulates per session (R6). Open: Ergo's `force-nick-equals-account`
   defaults to true, which gives one account exactly one nick, and `multiclient`
   folds a same-account same-nick connection into the existing client. Whether
   several simultaneous connections can share one account with DIFFERENT nicks
   is the experiment below.
2. **Account per agent instance.** Satisfies Ergo's defaults exactly (account
   name = nick), no global config change, but creates a server-side artefact
   per session — the garbage-collection problem of R6, and account registration
   is closed on the operator's Ergo today.
3. **No accounts, convention only** (today's shape: unauthenticated puppets from
   the LAN). Satisfies R5 and R6 trivially, fails R7, and leaves R1/R2 resting
   on gateway state — which is where the twenty board runs came from.
4. **One record per connection: `{ current, desired }`.** Needed under every
   option above, because the gap between sending `NICK` and the echo is local
   concurrency, not missing knowledge: two agents must not pick one name. Four
   independent codebases (soju, ZNC, bitlbee, matrix-appservice-irc) converge on
   exactly this record and no more.
5. **An alias set that answers only ours-ness,** retired by `MONITOR` with a
   timeout as backstop. Safe because it answers one boolean; the window's
   failure was asking it to remember stories and lineage as well.

### Implementation rules taken from prior art (each has a shipped bug behind it)

- Compute "is this me" BEFORE mutating the nick (soju `37cd9e4d89`).
- Rekey membership on the old spelling, then insert the new, with ONE
  casefolding function used for membership keys, identity tests and the
  own-nick set (ZNC's ghosts come from a byte-exact map behind a
  case-insensitive identity test).
- Trust the echoed NICK for membership; none of the four re-requests `NAMES`
  after a rename. Scope any re-sync to "did the name I want get freed".
- Never apply a requested nick optimistically: store the desire, send `NICK`,
  update on the echo; `433` clears the desire and leaves `current` untouched.
- Keep a latch for "I already have the nick I want", or a regain loop fights
  every server-forced rename (soju `57584c08ed`).
- On reconnect, discard the whole per-connection record; let the registration
  burst rebuild membership (ZNC's clear path is dead code, and its ghosts show
  it).
- Enforcement belongs to the server. A client cannot refuse another user's
  `NICK`; client-side pushback would be weaker than reservation and invites a
  denial-of-service through our own kicks.

### Single-user precondition

Today there is one human: WeeChat locally or the web client remotely, on a
private Ergo. Almost all of the window's machinery existed for one hazard — a
human taking the name an agent had just vacated, in the gap before the gateway
knew. Under one user that hazard is theoretical, so this increment states the
precondition and ships the smaller mechanism. What multi-user brings back, to
be answered then and not now: per-agent (or per-role) account provisioning;
whether humans get stable ids too, since a human's identity today IS their
nick, so their rename is a different mesh peer; and the reservation policy.

### Terrain corrections (assumptions already merged that are wrong)

- Ergo **rejects** over-long nicks with `432`; it does not truncate. Nick
  fitting must aim to fit, not to survive truncation.
- Ergo folds **confusables** via a skeleton index, so "same nick" on the server
  is broader than our casemapping fold: a nick we consider distinct can still
  be refused as colliding.
- `ip-limits.max-concurrent-connections` is 16 per IPv4 /32 and the gateway host
  is not exempt, so the 17th simultaneous agent is refused today. Discovery
  already lists 12–16 agents.

### Open questions

- **Q1.** Can several simultaneous connections share one account with different
  nicks on Ergo, and under which settings (`force-nick-equals-account: false`,
  `multiclient.enabled: false`, `nick-reservation.method`)? Decides partial
  solution 1 versus 2. *Experiment below.*
- **Q2.** Can a whole namespace (`cc-*`) be reserved, or only specific nicks?
  If only specific, an alias per agent must be grouped as it appears.
- **Q3.** Can the gateway register accounts itself (NickServ `REGISTER` with
  registration closed), or is that a per-role admin step? One step per role is
  acceptable; per session is not (R6).
- **Q4.** Where does the label come from — the session's own description, the
  bead it claimed, or an explicit command? Default `<role>-<short id>` so it
  always works, with a better label when one is published.

### Experiment results (2026-09-23, throwaway Ergo 2.19 from the same binary)

Run against a private instance on `127.0.0.1:16667` with a copy of the
operator's `ircd.yaml`, not against his server. Findings:

- **Q1 — yes. Two answers, depending on the shape.** With ONE account shared by
  several connections, different nicks require
  `nick-reservation.force-nick-equals-account: false` and
  `multiclient.enabled: false`; under the operator's current settings the second
  connection is **silently folded into the first and assigned the account nick**
  — every agent on one account becoming one IRC client, the failure to avoid.
  But with the POOLED shape (one pre-registered account per slot) that case
  never arises: `cc-1` and `cc-2` registered independently under the operator's
  CURRENT settings (`force-nick-equals-account: true`,
  `multiclient.enabled: true`), both appeared in `NAMES`, and folding cannot
  apply because the accounts differ. **So the pool needs no server setting
  changed.** `multiclient` in particular must stay ON: it is what lets the
  operator attach WeeChat on more than one machine plus the web client to one
  nick.
  The single thing `force-nick-equals-account: false` buys is a LEGIBLE nick: a
  slot asking for a label under `true` is refused
  (`400 … You must use your account name as your nickname`), so with the default
  a puppet's nick is its slot name (`cc-1`) and the label lives in the realname
  via `SETNAME`. Legible nicks versus one global setting is the operator's
  trade, and nothing else depends on it.
- **Q3 — a client can self-register** when `accounts.registration.enabled` is
  true and email verification is off: `NS REGISTER <password>` returned
  "Account created" and logged the session in. Registration is closed on the
  operator's Ergo, so per-role accounts are a one-off admin step (two or three
  accounts) unless he opens registration.
- **Reservation is real, and it closes the hazard the window existed for.**
  With the label nick grouped to the account (`NS GROUP`, no argument, groups
  the *current* nick; `additional-nick-limit` must be > 0), an unauthenticated
  client asking for that nick got **433 both while the agent held it and after
  the agent had moved off it**. The "human takes the name an agent just
  vacated" case — the reason `vacating`, `origin` and the held-back stories
  existed — cannot happen for a grouped nick.
- **`MONITOR` answers "has that name freed up".** `731` (offline) on
  registration, `730` when the name appeared, `731` when it left. `MONITOR=100`
  in ISUPPORT.
- **Renaming to a label works** while authenticated with
  `force-nick-equals-account: false`: `cc` → `agent-alpha`, echoed as
  `:cc!…@… NICK agent-alpha` — the echo carries the OLD prefix, which is why
  "decide whether it is ours before mutating" is the rule.
- **Terrain corrections confirmed.** A 40-character nick (`NICKLEN=32`) was
  refused with **432**, not truncated. A Cyrillic-homoglyph variant of a
  reserved nick was also refused with **432** — on this config non-ASCII nicks
  are rejected outright, which is stronger than skeleton folding and removes
  homoglyph impersonation.
- **Every capability the design needs is advertised**: `account-tag`,
  `extended-join`, `account-notify`, `setname`, `chghost`, `labeled-response`,
  `no-implicit-names`, `draft/whoami`, `sasl`, `monitor`, `extended-monitor`,
  `batch`, `message-tags`, `draft/relaymsg`; `WHOX` and `CASEMAPPING=ascii` in
  ISUPPORT.

### What this settles, and the one tension left

Partial solution 1 (account per role, nick as a label) is viable and is the
recommendation: ours-ness is a server fact on every line (R1, R2, R7), the nick
is free to be legible (R5), nothing accumulates per session (R6), and the freed
-name hazard is gone for any nick we group. It costs two settings on the
operator's Ergo and one admin-registered account per role.

The tension: reservation is per nick, not per namespace (Q2 — no globbing
found), and `additional-nick-limit` caps grouped aliases per account. So a
*session* label cannot be reserved without accumulating grouped nicks — the GC
problem again, smaller. Since ours-ness comes from the account tag rather than
from the spelling, reservation is only needed to stop a human *pre-empting* a
label, which the single-user precondition makes moot. Proposal: group one
stable nick per role (so the role name is always ours), leave session labels
unreserved, and revisit if multi-user arrives.

### Provisioning: a leased pool of pre-registered accounts (operator's shape, 2026-09-23)

Accounts are **pre-registered once** and **leased**, not created per session.
`cc-1`…`cc-16`, each with its own nick grouped to it so nothing can pre-empt it.
A qualifying session leases a free slot, connects as that account, and takes a
legible nick as its label; when it goes, the lease returns to the free list.
Bounded server-side artefacts, nothing to garbage-collect, and account
registration can stay closed (the accounts are made once, by hand or by a
one-off script).

- **Lease on ACTIVITY, not on presence.** A `mu ask` peer stays discoverable for
  about an hour after its last heartbeat, so qualifying on discovery alone hands
  slots to agents that finished long ago. The gate is session-shaped (ruling A)
  AND a heartbeat inside an idle window; the window is config, like every other
  tunable here.
- **Size is config, starting at 16** — the operator's current
  `ip-limits.max-concurrent-connections`, which the `exempted` entry for the
  gateway host lifts. Raising it is a config change, not a source change; the
  costs that do scale are a socket and a task per puppet on our side, a client
  on Ergo's, and every `NAMES`/`WHO` burst.
- **Exhaustion evicts the least-recently-active lease**, loudly (a counter and a
  warning, because a cleanup path that stalls silently is what broke Libera's
  bridge). Never evict an agent that has exchanged a line with a human inside
  the routing memory's window — the pair memory already knows.
- **Spillover is a fallback, not a shared nick.** When the pool is full and
  nothing is idle enough to evict, those agents have no puppet and stay
  reachable the v0 way, through `mu-gw` and their own channel. Sharing one nick
  between agents would destroy the identity this increment exists for.
- **Credentials: one certificate per slot, no CA, no password.** *Settled
  2026-09-24 against a throwaway Ergo 2.19, superseding the "unverified" note
  this bullet used to carry.* Ergo's certfp matches a **fingerprint, not a
  chain**, so a self-signed certificate per slot is enough and the private CA
  at `~/ergo/ca` is not involved. The mapping is **one account per
  certificate**: Ergo refuses to register a fingerprint it already knows to a
  second account, and SASL EXTERNAL requires `authzid` to equal `authcid`, so
  one shared certificate cannot assume different slots. Each slot therefore
  carries its own `<account>.crt`/`.key`, named by `[irc.puppets]
  slot_certs_dir`, registered with `NS CERT ADD` while connected as that
  account. No password exists to leak, and nothing secret lives in or beside
  the gateway config.
- **ONE pool, not one per role.** *Corrected 2026-09-25, during increment 2b's
  config.* The bullets above sketched `cc-1…`, `mu-1…` as if each role had its
  own pool. It cannot: the pool's size **is** Ergo's per-IP
  `max-concurrent-connections` (16), so two pools of that size would need 32
  connections from the gateway host and breach the very limit the size was
  taken from. There is one pool, `<slot_prefix>-<n>` for `n` in `1..=max`, and
  a slot goes to whichever session qualifies regardless of its role. Nothing is
  lost: the role was never carried by the account name, it is carried by the
  puppet's LABEL — its nick and realname — which is where a human reads it. The
  budget is one connection per puppet plus the gateway's own, so `max` stays at
  or below the limit for the host, and `roles` stays what it always was: which
  peer roles qualify for a puppet at all.
- **A distributed semaphore (etcd or similar) is not needed yet.** One gateway
  process owns every puppet, so the free list is in memory. The trigger for
  something shared: a second gateway host, or multi-user provisioning.
- **Residual: slots are reused, so a label is not durable.** A nick seen
  yesterday as `claude-pr777` may be another session today; the peer id remains
  the truth. `SETNAME` carrying the session id and description, and a short id
  in the label, keep it legible — otherwise the operator is back to keeping his
  own map, which is what R5 exists to prevent.

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
