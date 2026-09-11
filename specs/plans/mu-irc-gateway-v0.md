# mu-irc-gateway v0

| field | value |
| --- | --- |
| status | plan / implementation slices |
| created | 2026-09-10 |
| related | `at-uws` (mesh gateway), `mu-b1lq` (hierarchical DM subjects), `mu-wxc4` (NATS mesh), `specs/architecture/mesh-nats-adoption.md`, `specs/architecture/session-identity.md` |

`mu-irc-gateway` is a standalone, single-nick IRC frontend to the existing NATS
agent mesh. One human operator joins one IRC server as one nick; the gateway
translates between IRC and the mesh so a person can watch and address mesh
agents from an ordinary IRC client. Delivery is **live and best-effort**: the
gateway holds mesh subscriptions and an IRC connection while it runs and mirrors
between them, but it is not a store, not a replay buffer, and not part of any
durable-wake path. The mu daemon stays entirely unaware that IRC exists.

## Terrain checked (2026-09-10)

- `mu-dialogue::mesh` (`crates/mu-dialogue/src/mesh.rs`) is the existing mesh
  client: `connect()`, `Gateway::{publish_dm, address, front_peer, release_peer,
  srv_agents, live_subject, fronted_peers}`, the `DmEnvelope`/`AgentCommand`
  wire contract, per-message biscuit capabilities, and `$SRV` liveness
  discovery. It is currently a **private `mod mesh;` inside the binary**, so the
  first seam step exposes it through a library target.
- `dm_authorized()` in that module is the fail-closed capability check
  (base64 → biscuit → `right("agent_dm")`, no fail-open path). It is currently
  **private**; both IRC reception paths must reuse it, so it becomes `pub`.
- `mu_peer::PeerId::dm_subject()` is **already role-generic**: it builds
  `mu.agent.<role>.<id>[.<sub>].dm` for any role. A `human:<nick>` peer id
  therefore derives `mu.agent.human.<nick>.dm` with **no change** to the
  subject rule. Human support is an *additive* address-resolution change, not a
  subject rewrite. (The older `mesh-nats-adoption.md` spells DM subjects
  `mu.agent.<id>.dm`; the hierarchical form in `PeerId` is current — follow the
  code.)
- `Gateway::address()` resolves only `PeerId::as_mu()` shapes (mu daemon /
  session), preferring a live session subject and falling back to the daemon
  subject with the session named. Legacy daemons that advertise no `$SRV`
  metadata are reached on their flat `mu.agent.<daemon>.dm` subject. Human
  targets are a new, separate resolution path; the mu fallback is preserved
  byte for byte.
- `InboundDm` carries `{to_peer, from_peer, body, subject}` — **no mesh message
  id**. `publish_dm()` mints a **fresh ulid per call**. The IRC bridge needs the
  id (for loop-guard and exactly-once fan-out) and needs one id shared across a
  multi-endpoint broadcast, so the seam adds opt-in APIs that supply both
  without changing the existing `InboundDm` shape or `publish_dm` callers.
- `Fronted::release()` deliberately `stop()`s the Micro presence service via a
  `stop_anchor` endpoint, because dropping the service does **not** withdraw
  `$SRV` presence (async-nats 0.49 has no `Drop` on `Service`; leaked
  responders were 783 phantom peers on 2026-09-08). Gateway cleanup and rename
  paths must keep using `front_peer`/`release_peer`, never hand-rolled
  presence teardown.

## Non-negotiable constraints

- **Typed mesh contract, no new substrate.** Use shared `PeerId` addressing,
  the existing `DmEnvelope` wire shape, and `$SRV` Micro discovery. No
  persistent roster, no new envelope type, no broadcast subject invented on the
  wire (the mesh has no native broadcast; the gateway fans out to discovered
  endpoints).
- **Fail-closed verification.** Every payload exposed to IRC — whether it
  arrived on a human endpoint subscription or on the observer wildcard — is
  verified with `mesh::dm_authorized` first. A successful subscription is not
  authorization; the check verifies the issuer-signed `agent_dm` capability,
  not the sender label or body. A failed check produces no IRC body and no
  command output, only a body-free diagnostic.
- **Durable-wake boundary untouched.** The gateway publishes through the
  existing mesh path only. It does not create daemon mailbox events, does not
  touch `AgentInput::MailboxMessage`, and adds no side channel into the agent
  loop. IRC is a live mirror, never a delivery guarantee.
- **Process state is disposable.** Membership, routing memory, notice
  suppression, and loop-tracking live only in the running process. They are
  never session truth and never replay storage; after an IRC or NATS reconnect
  they are rebuilt from fresh discovery and NAMES, with routing memory empty.
- **Design in `specs/`, review per increment.** This document is the canonical
  design. Each capability increment below is independently tested and reviewed
  before the integration increment; any increment exceeding the configured
  review cap (default 2,000 reviewable lines) is split.

## Addressing and mapping contract

- **Peer roles → IRC.** Mesh agents (`mu:…`, `cc:…`, `warden:…`) map to IRC
  channels under the configured `channel_prefix`; a human operator is the
  gateway's own nick. Human peers on the mesh use the new `human:<nick>` role,
  whose DM subject is `mu.agent.human.<folded-nick>.dm` via the existing
  `dm_subject()`.
- **Human identity folding** uses the IRC server's advertised `CASEMAPPING`
  only — no other sanitization — so two humans who differ only by IRC-equivalent
  case are one logical identity, and humans that differ by anything else stay
  distinct.
- **Channel names** are derived from the full peer id; when the full alias would
  exceed the server's `CHANNELLEN`, the tail is replaced by a stable hash so the
  same peer always maps to the same channel. Collisions in the short alias are
  disambiguated by falling back to a label carrying more of the full id.
- **Reverse lookup** (IRC channel/target → mesh peer) resolves against the
  *current* set of discovered peers, never a stored roster, comparing channel
  names under the server's advertised `CASEMAPPING` — the server treats
  case-equivalent channel names as one channel, so the gateway must too;
  ambiguity (including peers whose channels collide only once folded) is
  reported to the operator naming the colliding peers, and no channel is ever
  created for a human.
- **Observer subject.** `observe_agent_dms` subscribes the agent-DM wildcard
  (`mu.agent.>`, everything the mesh addresses as a DM). This overlaps the
  gateway's own human-endpoint subjects (`mu.agent.human.*.dm`), so endpoint and
  observer reception of the same minted id must be de-duplicated to exactly one
  IRC delivery — without suppressing genuinely different destinations that share
  a fan-out id. The de-duplication memory, like the outbound loop guard and the
  per-withdrawal notice suppression, is a fixed-capacity window of
  recently-recorded keys rather than an unbounded set: the duplicate it
  collapses arrives within one dispatch, so a bounded window covers it, and a
  long-lived connection's memory does not grow with total traffic. Who is
  addressed is the sender's choice, not the gateway's, so only an event that
  reaches a delivery decision spends a slot of that window: a destination
  addressing nobody must not evict the keys real deliveries depend on. A bounded
  entry COUNT is only half a memory bound, since every part of that key is
  remote text the shared DM decoder does not size-limit: the window retains a
  fixed-size digest of `(id, destination, session)`, and the gateway's own
  ingress refuses an envelope whose `id`, `session` or destination subject is
  longer than a documented cap before routing sees it at all.

## Ordered increments (each separately reviewed, within the review cap)

1. **Shared seam + configuration.**
   - `specs/plans/mu-irc-gateway-v0.md` (this document).
   - `mu-dialogue` gains a library target exposing `pub mod mesh`; the binary
     imports it instead of declaring a private module. Existing client
     behavior and tests are preserved.
   - `mu_peer` gains the additive human constructor / role, the agent-DM
     observer-subject helper, and human address resolution, with regression
     tests pinning every existing role subject and the `human:` derivation.
   - `mu-dialogue::mesh` gains opt-in APIs: `pub` shared verification, verified
     endpoint/observer events carrying the mesh message id and destination,
     observer verification-failure reporting, connection-transition
     observation, and a fan-out publish that mints **one** id for several
     existing `DmEnvelope`s — leaving `connect`, `InboundDm`, and `publish_dm`
     callers unchanged.
   - The `mu-irc-gateway` crate is added to the workspace with its `[irc]`
     configuration (`server`, `tls`, `nick`, `sasl_user`, `sasl_password`,
     `sasl_password_file`, `channel_prefix`, `lobby`, `observe_agent_dms`),
     loading existing mesh settings without changing daemon defaults and
     rejecting invalid or conflicting credential settings without revealing
     secrets. No IRC adapter yet.
2. **mesh → IRC**, split into two independently reviewed capability slices.
   (2a was cut to fit the review cap; review-driven fixes and their tests then
   grew it to ~2,020 reviewable lines, 18 over the default, and its third board
   ran with `MU_REVIEW_SIZE_OVERRIDE=1` rather than splitting it again.)
   - **2a — configuration + pure mapping/framing.** The `mu-irc-gateway` crate
     (library only, no runnable bridge) with: gateway-local `[irc]`
     configuration (all keys, defaults, credential-pair and mutually-exclusive
     password-source validation, password-file loading, secret-safe errors and
     debug — deserialization faults reported as field name plus expected type
     rather than a serde message that would quote an unquoted password, and the
     mesh URL printed with any userinfo credential redacted), delegating
     mesh-config loading to `mu_dialogue::mesh::load`
     unchanged; pure CASEMAPPING-aware nick folding and human `PeerId`
     construction; the specified role aliases, deterministic CHANNELLEN-limited
     channel names with a stable hash tail, human channel exclusion, and reverse
     resolution against a supplied current-peer snapshot with explicit unknown
     and ambiguous results; UTF-8-safe PRIVMSG framing that budgets the complete
     serialized line against 512 bytes with marked continuations, CR/LF
     injection prevention covering the attacker-supplied mesh id as well as the
     target and body, single-channel-or-nick target validation that also refuses
     the target-list and parameter delimiters (`,`, space, a leading `:`), and
     `+mu.id` only when message-tags is negotiated.
     Offline regression tests only; **no IRC client, adapter, membership, or
     routing yet.** This is the completion boundary of the present increment.
   - **2b — adapter, membership, routing.** IRC adapter over a maintained Rust
     IRC client (superseded by the amendment below; one connection, TLS,
     mandatory SASL PLAIN when configured — mandatory meaning fail-closed: a
     CAP reply that does not acknowledge `sasl`, a terminal SASL numeric in
     either exchange phase, and a welcome numeric before `903` all fail
     registration rather than completing it unauthenticated, an unsolicited
     `sasl` ACK is ignored rather than entering the exchange, and the response
     is chunked at 400 base64 characters with the
     `AUTHENTICATE +` terminator an exact multiple requires; the configured nick
     is validated before it is interpolated into `NICK`/`USER`, and every
     outbound `AUTHENTICATE` payload is redacted in `Debug`), optional
     message-tags/account caps — `CAP LS 302` implies cap-notify, so a later
     `CAP DEL` clears the withdrawn flags in every phase including after
     registration, and a `CAP NEW` is recorded as available but not requested,
     v0 negotiating once — live CASEMAPPING/CHANNELLEN handling);
     disposable membership/channel lifecycle from NAMES and
     JOIN/PART/KICK/QUIT/NICK plus fresh discovery; mesh→IRC routing with agent
     channels whose presence is exact peer-id membership in the discovered set
     (folded channel collisions decide only whether a present target's body
     needs a disambiguating label), lobby fallbacks that name the intended
     target, exclusive human routing, a `human`-role destination carrying no
     usable nick (none at all, an empty subject component, a component the
     mesh's own subject derivation could not have emitted — one carrying the `:`
     that `PeerId::parse` would re-split into a different peer, whitespace, or a
     control character — or text an IRC line cannot carry) dropped body-free
     rather than routed as an agent, every
     outbound line — the bodiless notice included — built through the framing
     module, per-withdrawal bodiless-notice suppression in a bounded window, and
     exactly-once endpoint/observer overlap handling over a bounded window of
     recently-seen keys that only delivered events consume.

     The 2b slice lands as **offline library capabilities only**, mirroring 2a:
     the adapter is a registration/capability *state machine* over a small
     single-connection transport trait — the trait's real TLS socket, the
     client itself, and event-loop wiring are the integration increment's job
     (the "maintained IRC client crate" this originally named is superseded by
     the amendment below). Membership and routing are pure state machines
     driven by *injected* IRC events and discovery snapshots that emit typed
     *effects and decisions* (which channels to JOIN/PART, which PRIVMSG lines
     to frame); the executor that runs those effects against the live mesh/IRC
     connections — using `front_peer`/`release_peer` and the observer
     subscription — is deferred to increment 5. Nothing here opens a socket or
     subscribes a subject; every capability is exercised offline.

     **Amendment, 2026-09-10 — the integration increment ships a plain TLS line
     transport, not a maintained IRC client crate.** The two mentions above are
     superseded; `crates/mu-irc-gateway/src/transport.rs` implements the
     adapter's `Transport` trait over `tokio` + `tokio-rustls` directly.

     *Why.* By the time 2b landed, the adapter already owned the whole
     registration handshake: CAP LS 302 negotiation with cap-notify, CAP
     DEL/NEW, mandatory fail-closed SASL PLAIN (chunked at 400 base64
     characters, terminator included), nick rejection, and live
     CASEMAPPING/CHANNELLEN handling from ISUPPORT — all reviewed and covered by
     offline regression tests. Every maintained Rust IRC client wants to own
     that same handshake. Adopting one therefore meant either running two
     registration state machines against one socket, or reaching past the
     crate's API to suppress its own; neither leaves the reviewed one in charge.
     What was actually missing is the part a client crate adds no value to: a
     socket, TLS to the system trust store, CRLF framing, a bounded outbound
     queue, and reconnection. Both `tokio` and `tokio-rustls` were already in
     the workspace dependency graph, so this direction added no new third-party
     surface, where a client crate would have.

     *What it costs.* Protocol decisions a client crate would have made for us —
     IRCv3 message-tag parsing, numeric tables, CTCP, SASL mechanisms beyond
     PLAIN — are now ours to make and to keep correct, and a protocol bug here
     is ours to find. The line transport is deliberately small (framing,
     lifecycle, TLS setup) to keep that surface bounded; the protocol itself
     lives in the adapter, which is where the tests are.

     *What would reopen it.* v0 exclusions coming back in — multi-connection or
     multi-nick operation, SASL mechanisms beyond PLAIN (SCRAM, EXTERNAL),
     server-time/batch/chathistory, or IRCv3 capabilities whose state machines
     are substantially more than a flag — would make a maintained crate's
     protocol coverage worth two state machines, and this decision should be
     re-taken rather than extended.
3. **IRC → mesh.** Delivery for present-agent channels and lobby/private
   fan-out, canonical human senders, one shared id per broadcast, refusal of
   absent/human destinations — an explicit destination is present only on exact
   peer-id membership in the discovery snapshot, never on a matching DM subject,
   since `dm_subject()` is not injective (`mu:d:s` and `mu:d.s` share
   `mu.agent.mu.d.s.dm`) and an absent identity must not inherit a present
   peer's reachability — and one sender-authorization check applied to
   every IRC-originated line ahead of all destination logic — the gateway
   asserts `human:<nick>` under its own mesh capability, so a sender it observes
   in no shared channel is refused whatever destination the line names, and an
   explicit `role:id:` address cannot select a path that skips the check.
   Routing-memory updates only for specifically addressed messages; both loop
   guards (ignore the gateway's own nick, matched against the nick's wire
   spelling folded under the CURRENT casemapping; suppress `human:` senders and
   gateway-minted ids, tracked before publication can race observation, in the
   same bounded window the mesh→IRC overlap dedup uses).

   Like 2b, this increment is an **offline library capability**: it decides what
   to publish (one caller-supplied minted id per fan-out, the set of destination
   peers, the routing-memory updates) and records minted ids for loop-guarding
   *before* the caller's publication effect can become observable. The actual
   `publish_dm`/fan-out call, and the `mu-dialogue::mesh` seam that mints one id
   across several `DmEnvelope`s, remain the integration increment's work — this
   slice does not reopen increment 1. Textual bot-command dispatch stays in
   increment 4.
4. **Bot verbs.** `mu peers` and `mu say <peer> <text>` dispatched ahead of
   ordinary routing, with live full-id/alias resolution, ambiguity replies
   naming colliding peers, correct explicit-address routing-memory updates, and
   no publication for unsupported commands.
5. **Integration + acceptance.** Wire the tested capabilities into the
   standalone binary using existing mesh registration/release APIs for human
   endpoints, optional observer fallback, verification-failure counters and
   body-free logs, and connection-aware dropping that never replays buffered
   traffic after either side reconnects. Offline regression suites plus an
   opt-in real-server harness gated by `MU_IRC_TEST_SERVER` (explicit skip
   reason when unset), exercising the production bridge. Crate README covering
   every configuration key, operator Ergo/SASL examples, mapping/collision
   rules, human privacy and rename rules, plain-PRIVMSG operation, humans-only
   fallback, best-effort gaps, the missing native broadcast subject, and the v1
   exclusions.

## v0 exclusions

- No IRC-side persistence, scrollback replay, or history buffering.
- No multi-nick / multi-operator fan-in; exactly one gateway nick.
- No daemon changes and no IRC awareness in `mu-coding`.
- No native mesh broadcast subject; fan-out is to discovered endpoints only.
- No provisioning or installation of IRC/NATS servers by this work; live
  acceptance uses operator-provided services and credentials.
