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
  *current* set of discovered peers, never a stored roster; ambiguity is
  reported to the operator naming the colliding peers, and no channel is ever
  created for a human.
- **Observer subject.** `observe_agent_dms` subscribes the agent-DM wildcard
  (`mu.agent.>`, everything the mesh addresses as a DM). This overlaps the
  gateway's own human-endpoint subjects (`mu.agent.human.*.dm`), so endpoint and
  observer reception of the same minted id must be de-duplicated to exactly one
  IRC delivery — without suppressing genuinely different destinations that share
  a fan-out id.

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
2. **mesh → IRC.** IRC adapter over a maintained Rust IRC client (one
   connection, TLS, mandatory SASL PLAIN when configured, optional
   message-tags/account caps, CASEMAPPING/CHANNELLEN handling); pure mapping
   functions; IRC framing within the 512-byte budget with UTF-8 splitting,
   continuation markers, injection prevention, and `+mu.id` only when tags are
   negotiated; disposable membership/channel lifecycle from NAMES and
   JOIN/PART/KICK/QUIT/NICK plus fresh discovery; mesh→IRC routing with agent
   channels, collision labels, lobby fallbacks, exclusive human routing,
   per-withdrawal bodiless-notice suppression, and exactly-once endpoint/observer
   overlap handling.
3. **IRC → mesh.** Delivery for present-agent channels and lobby/private
   fan-out, canonical human senders, one shared id per broadcast, refusal of
   absent/human destinations and nonmember private senders, and routing-memory
   updates only for specifically addressed messages; both loop guards (ignore
   the gateway's own nick; suppress `human:` senders and gateway-minted ids,
   tracked before publication can race observation).
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
