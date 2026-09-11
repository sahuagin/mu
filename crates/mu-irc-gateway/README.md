# mu-irc-gateway

A standalone single-nick IRC frontend to the mu agent mesh: one human operator
joins one IRC server as one nick, and the gateway mirrors between IRC and the
NATS agent mesh so a person can watch and address mesh agents from an ordinary
IRC client. The design is `specs/plans/mu-irc-gateway-v0.md`; this file is the
operator-facing status of what the crate actually does today.

## Status: offline library, no runnable bridge yet

This crate is built in independently reviewed capability slices, and **every
slice landed so far is an offline library** — pure, tested capabilities with no
socket, no live mesh, and no binary. What is present:

- **Configuration** (`config`) — the `[irc]` section, its validation, and
  secret-safe errors; mesh-config loading is delegated unchanged to
  `mu_dialogue::mesh::load`.
- **Mapping & framing** (`mapping`, `framing`) — CASEMAPPING-aware identity
  folding, deterministic CHANNELLEN-limited channel names, reverse resolution
  against a live peer snapshot, and 512-byte-safe PRIVMSG framing.
- **Adapter** (`adapter`) — a registration/capability *state machine* over a
  small `Transport` seam: CAP negotiation, mandatory SASL PLAIN when configured
  (refused over cleartext; never retained in printable state), optional
  message-tags/account capabilities, and live CASEMAPPING/CHANNELLEN from
  ISUPPORT.
- **Membership** (`membership`) — disposable channel membership and human
  presence reconciled from generation-scoped NAMES and JOIN/PART/KICK/QUIT/NICK,
  plus a channel reconciler with refused-JOIN backoff.
- **mesh→IRC routing** (`routing`) — decisions behind the fail-closed
  `mesh::verify_and_decode_dm` ingress, with exactly-once endpoint/observer
  overlap handling and exclusive human-delivery precedence.
- **IRC→mesh routing** (`outbound`) — a human's line becomes a mesh publish
  decision (explicit address, agent channel, or fan-out under one minted id),
  with one sender-authorization check ahead of all destination logic (a nick the
  gateway observes nowhere is refused whatever destination its line names, so an
  explicit address cannot route around the gate), the
  destination/ambiguity/human refusals — an explicit destination counts as
  present only on exact peer-id membership in the discovery snapshot, never on a
  shared DM subject — memory writes only for specifically-addressed lines, and
  both loop guards.

These decide effects; they do not perform them. The state is disposable — after
an IRC or NATS reconnect it is rebuilt from fresh discovery and NAMES, with
routing memory empty and no traffic retained.

## Configuration

The `[irc]` section (the mesh side is the shared `[mesh]` / `[dialogue.mesh]`
config, loaded unchanged):

| key | required | default | meaning |
| --- | --- | --- | --- |
| `server` | yes | — | `host:port` of the IRC server |
| `nick` | yes | — | the single nick the gateway registers as |
| `tls` | no | `true` | connect over TLS (SASL PLAIN is refused without it) |
| `sasl_user` | no | — | SASL PLAIN account |
| `sasl_password` | no | — | SASL PLAIN password (inline) |
| `sasl_password_file` | no | — | SASL PLAIN password from a file (mutually exclusive with the inline form) |
| `channel_prefix` | no | `#` | prefix for agent channels |
| `lobby` | no | `#mu` | the fan-out / fallback channel |
| `observe_agent_dms` | no | `true` | whether to observe the agent-DM wildcard |

A SASL password may be given inline or by file, never both; a user without a
password (or a password without a user) is rejected. Credential errors name a
field or path, never a secret value.

## Mapping rules (as implemented)

- The gateway registers as its own nick, which is NOT a human operator: a line
  whose sender folds to that nick is the gateway's own output looping back, and
  is dropped. Every other nick the gateway observes in a shared channel is a
  human operator (mesh agents are channels, never IRC members), and two humans
  differing only by IRC-equivalent case (per the server's `CASEMAPPING`) are one
  identity.
- A mesh agent maps to a channel `<channel_prefix><alias>` built from its full
  peer id (e.g. `cc:abc` → `#cc-abc`); when the alias would exceed `CHANNELLEN`
  the tail becomes a stable 8-hex BLAKE3 hash so the same peer always maps to the
  same channel.
- Reverse lookup resolves against the *current* discovered peers, folded under
  the server's `CASEMAPPING`; peers whose channels collide (even only once
  folded) resolve as ambiguous, naming the colliding peers, and no channel is
  ever created for a human.

## Not here yet (deferred to their increments)

- The maintained Rust IRC client, the real TLS socket, and the read/write event
  loop that executes these decisions (integration, increment 5).
- Wiring the mesh subscriptions — human-endpoint `front_peer`/`release_peer` and
  the optional observer-wildcard fallback — and the `mu-dialogue::mesh` fan-out
  publish that mints one id across several envelopes.
- Textual bot verbs (`mu peers`, `mu say …`) — increment 4.
- A real-server acceptance harness (`MU_IRC_TEST_SERVER`) — integration.

## v0 boundaries

Best-effort live mirror only: no IRC-side persistence, scrollback, or replay; no
durable-wake path and no daemon changes; exactly one gateway nick; no native mesh
broadcast subject (fan-out goes to discovered endpoints).
