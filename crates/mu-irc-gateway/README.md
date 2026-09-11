# mu-irc-gateway

A standalone single-nick IRC frontend to the mu agent mesh: one human operator
joins one IRC server as one nick, and the gateway mirrors between IRC and the
NATS agent mesh so a person can watch and address mesh agents from an ordinary
IRC client. The design is `specs/plans/mu-irc-gateway-v0.md`; this file is the
operator-facing description of what the crate does today.

Mesh agents appear as **channels**. Humans on IRC appear on the mesh as
`human:<nick>` peers the gateway fronts while they are visible. Delivery is a
**live, best-effort mirror**: the gateway holds subscriptions and a connection
while it runs, and it is not a store, not a replay buffer, and no part of any
durable-wake path. The mu daemon is unaware IRC exists.

## Status: runnable

`mu-irc-gateway` is a binary. It connects, registers (TLS + SASL PLAIN),
mirrors both directions, and reconnects with backoff.

| piece | what it does |
| --- | --- |
| `config` | the `[irc]` section, its validation, and secret-safe errors; mesh config is loaded unchanged through `mu_dialogue::mesh::load` |
| `mapping`, `framing` | CASEMAPPING-aware identity folding, deterministic CHANNELLEN-limited channel names, reverse resolution against a live peer snapshot, 512-byte-safe PRIVMSG framing |
| `adapter` | registration state machine: CAP negotiation, mandatory SASL PLAIN when configured (refused over cleartext, never in printable state), optional message-tags/account caps, live CASEMAPPING/CHANNELLEN |
| `membership` | channel membership and human presence from generation-scoped NAMES plus JOIN/PART/KICK/QUIT/NICK, and a channel reconciler with refused-JOIN backoff |
| `routing` | mesh→IRC decisions behind the fail-closed `mesh::verify_and_decode_dm` ingress, exactly-once endpoint/observer overlap handling, exclusive human-delivery precedence |
| `outbound` | IRC→mesh decisions: the bot verbs first, then explicit address, agent channel, or fan-out under one minted id; one sender-authorization check ahead of all destination logic; both loop guards |
| `transport` | the socket: TCP, TLS by default (system anchors plus any configured private CA), CRLF framing, a bounded outbound queue |
| `bridge` | the loop that runs all of the above against a live server and a live mesh |

## Configuration

Two files, because the mesh section is shared with every other agent on the box:

- **`--config`** (default `$MU_CONFIG`, else `~/.config/mu/config.toml`) holds
  `[irc]` and `[dialogue.mesh]`.
- **`--fleet-config`** (default `$MU_CONFIG`, else
  `~/.config/agent/config.toml`) holds the fleet-wide `[mesh]` section that
  `nats_url` / `issuer_key` are inherited from when `[dialogue.mesh]` does not
  set them. A file that does not exist simply contributes nothing.

`mu-irc-gateway --check-config` loads both, prints what they resolved to with
every secret redacted, and exits without connecting to anything.

### `[irc]`

| key | required | default | meaning |
| --- | --- | --- | --- |
| `server` | yes | — | `host:port` of the IRC server (a bare host takes 6697 with TLS, 6667 without) |
| `nick` | yes | — | the single nick the gateway registers as |
| `tls` | no | `true` | connect over TLS, verified against the trust store below |
| `tls_ca_file` | no | — | PEM bundle of extra CA certificates to trust for this connection (a private CA) |
| `tls_system_roots` | no | `true` | whether the system trust store is part of that trust store |
| `sasl_user` | no | — | SASL PLAIN account |
| `sasl_password` | no | — | SASL PLAIN password, inline |
| `sasl_password_file` | no | — | SASL PLAIN password from a file (mutually exclusive with the inline form) |
| `channel_prefix` | no | `#` | prefix for agent channels |
| `lobby` | no | `#mu` | the fan-out / fallback channel |
| `observe_agent_dms` | no | `true` | whether to observe the agent-DM wildcard (`mu.agent.>`) |

A SASL password may be given inline or by file, never both; a user without a
password (or a password without a user) is rejected. Credential errors name a
field or a path, never a secret value — including malformed TOML, which is
reported as a field name and expected type rather than a message quoting the
offending line.

### Trusting a private CA

`tls_ca_file` **adds** a PEM bundle's certificates to what this connection
verifies a server certificate against; `tls_system_roots = false` drops the
system anchors so the bundle is the only one left. Both are read and parsed at
startup, so `mu-irc-gateway --check-config` is where a bad bundle is reported.

The trust model behind those two keys — why the anchors are additive, why the
validation is eager, and why there is no "skip verification" switch — is in
`specs/plans/mu-irc-gateway-v0.md`, **Amendment, 2026-09-11**. Read it before
changing any of this; what follows is only how to set it up.

One consequence belongs here because it is what a hand-made certificate usually
gets wrong: **the server name is still checked, exactly.** Trusting a CA decides
who may *issue* the certificate, never which name it is for. If `server` is an
IP address, that IP must be in the certificate's `subjectAltName` — a `DNS:`
SAN, or a CN, does not stand in for it. Give the certificate a `DNS:` SAN and
use that hostname, or give it an `IP:` SAN and connect by address; either works,
mixing them does not.

#### Making one with `openssl`

A CA whose only job is to sign this one server. Keep `ca.key` somewhere you
would keep a password — anything holding it can mint a certificate the gateway
will trust.

```sh
# 1. The CA: a key, and a self-signed certificate for it (10 years).
openssl ecparam -name prime256v1 -genkey -noout -out ca.key
openssl req -x509 -new -key ca.key -sha256 -days 3650 \
    -subj "/CN=example LAN CA" \
    -addext "basicConstraints=critical,CA:TRUE,pathlen:0" \
    -addext "keyUsage=critical,keyCertSign,cRLSign" \
    -out ca.pem

# 2. The server's key and request.
openssl ecparam -name prime256v1 -genkey -noout -out irc.key.ec
openssl pkcs8 -topk8 -nocrypt -in irc.key.ec -out irc.key
openssl req -new -key irc.key -subj "/CN=irc.lan" -out irc.csr

# 3. The SANs are the part that matters — list every name and address the
#    gateway (and your other clients) will connect to.
cat > irc.ext <<'EXT'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=serverAuth
subjectAltName=DNS:irc.lan,IP:192.168.1.10
EXT

# 4. Sign it (10 years).
openssl x509 -req -in irc.csr -sha256 -days 3650 \
    -CA ca.pem -CAkey ca.key -CAcreateserial \
    -extfile irc.ext -out irc.pem
```

`irc.pem` + `irc.key` go to the IRC server (for Ergo, `tls-certificate` /
`tls-key` in its listener config). **`ca.pem`** is the one the gateway needs:

```toml
[irc]
server = "irc.lan:6697"
nick = "mu-gw"
tls_ca_file = "/home/you/.config/mu/irc-ca.pem"
```

Confirm it before running anything:

```sh
mu-irc-gateway --check-config   # prints `ca_file` and the anchor count
```

#### The same CA on a phone

Your own IRC client has to trust the CA too, and a phone will not read the
gateway's config. On iOS with **Igloo IRC**: mail or AirDrop `ca.pem` to the
device, open it (Settings offers to install a *profile*), then — and this is the
step that is easy to miss — **Settings → General → About → Certificate Trust
Settings** and switch the CA on. Installing the profile alone is not enough; iOS
keeps root trust behind that second toggle, and Igloo will keep refusing the
connection until it is flipped. Only `ca.pem` ever leaves the machine: never
`ca.key`, and never the server's key.

SASL is **mandatory when configured**: if the server does not acknowledge `sasl`,
answers with a terminal SASL numeric, or welcomes the connection before the
exchange finishes, registration fails rather than completing unauthenticated.
Credentials are refused outright over a cleartext connection.

### `[dialogue.mesh]` / `[mesh]`

Unchanged from every other mesh client — `enabled`, `nats_url`, `issuer_key`,
loaded by `mu_dialogue::mesh::load`. The gateway adds no mesh settings of its
own.

### Not configurable in v0

Discovery is swept every 30 seconds (that sweep is also the channel
reconciliation tick), the reconnect backoff runs 2s → 5 min, registration must
complete within 60s, and a refused JOIN backs off 30s → 10 min. These are
constants in the `bridge` module; if one needs to be an operator dial, that is a
config-surface change with its own review.

## Operator run-through (Ergo, with SASL)

Ergo (`ergochat`) with TLS on 6697 and services enabled:

1. Register the gateway's account with NickServ once, from any client:

   ```text
   /msg NickServ REGISTER <password> <email>
   ```

2. Configure it. Keep the password in its own file if the config is shared:

   ```toml
   # ~/.config/mu/config.toml
   [irc]
   server = "irc.example.org:6697"
   nick = "mu-gw"
   sasl_user = "mu-gw"
   sasl_password_file = "/home/you/.config/mu/irc-sasl.pw"
   lobby = "#mu"
   observe_agent_dms = true

   [dialogue.mesh]
   enabled = true
   # nats_url / issuer_key inherited from [mesh] in ~/.config/agent/config.toml
   ```

   ```sh
   install -m 600 /dev/null ~/.config/mu/irc-sasl.pw
   printf '%s' 'the-password' > ~/.config/mu/irc-sasl.pw
   ```

3. Check it without connecting:

   ```sh
   mu-irc-gateway --check-config
   ```

4. Run it:

   ```sh
   RUST_LOG=info mu-irc-gateway
   ```

   Expect, in order: `connecting to IRC`, `registered` (with the negotiated
   casemapping, channellen and message-tags), then `joined` for the lobby and
   one channel per discovered agent.

5. Join `#mu` from your own client. Your JOIN is what makes you addressable:
   the gateway fronts `human:<your nick>` on the mesh, so agents can DM you.

6. Talk to an agent — in `#mu` (or in the agent's own channel):

   ```text
   cc:9f2c: what's the state of the deploy?
   ```

   Where the answer lands is decided by **where you typed the line**, not by
   where the agent lives. Typed in `#mu` as above, the reply comes back as a
   private message: `#cc-9f2c` is the agent's channel, not a room you are in, so
   there is no channel the answer is part of. Join `#cc-9f2c` and say the same
   thing there — with or without the `cc:9f2c:` prefix — and the reply arrives in
   `#cc-9f2c`, where the conversation is. Addressing the agent again from
   anywhere else switches you back to private replies.

   A reply only ever uses a remembered channel while you are still in it; if you
   have left, it falls back to a private message, and if the gateway cannot see
   you in any shared channel at all it posts a body-free notice in the lobby
   instead.

7. Stop it with Ctrl-C or `kill -TERM`: it sends a `QUIT` and releases every
   `human:` endpoint it fronted before exiting.

## Mapping rules

- The gateway's own nick is **not** a human operator: a line whose sender folds
  to it is the gateway's own output looping back, and is dropped. Every other
  nick observed in a shared channel is a human, and two humans differing only by
  IRC-equivalent case (per the server's `CASEMAPPING`) are one identity.
- A mesh agent maps to `<channel_prefix><alias>` built from its full peer id
  (`cc:abc` → `#cc-abc`); when the alias would exceed `CHANNELLEN` the tail
  becomes a stable 8-hex BLAKE3 hash, so the same peer always maps to the same
  channel.
- Reverse lookup resolves against the *current* discovered peers, folded under
  the server's `CASEMAPPING`. Peers whose channels collide (even only once
  folded) resolve as ambiguous, and the gateway says so naming them rather than
  picking one. No channel is ever created for a human.
- A destination counts as present only on **exact peer-id membership** in the
  discovery snapshot, never on a matching DM subject — `dm_subject()` is not
  injective, so an absent identity must not inherit a present peer's
  reachability.

## Human privacy, presence and renames

- **Current observed membership is the sole authority.** A private body reaches
  a human only where the gateway observes that human present right now. An
  absent human's DM produces a body-free notice in the lobby — once per
  withdrawal, never the body.
- **Presence follows the channels.** The first time a human is seen in any
  shared channel, the gateway fronts `human:<nick>` on the mesh; when they leave
  the last one, it releases it. On shutdown every fronted endpoint is released,
  so nothing lingers on `$SRV`.
- **A rename is a transfer.** A NICK change releases the old endpoint, fronts
  the new one, and carries the routing memory across, in that order on one
  worker so a rename never leaves two endpoints alive. A case-only change under
  the server's folding is the same identity and does nothing.
- Sender authorization runs once, ahead of all destination logic: a nick the
  gateway observes in no shared channel cannot have `human:<nick>` asserted on
  the mesh under the gateway's capability, whatever destination its line names.
  An explicit `role:id:` prefix cannot route around that.

## Plain-PRIVMSG operation

Everything is ordinary IRC. Address an agent by prefixing a line with its peer
id and a colon (`cc:abc: hello`), or just talk in that agent's channel. A line
in the lobby, or a private message to the gateway's nick, fans out to every
discovered agent under a single mesh id. There are no slash commands; the two
bot verbs below are ordinary text too.

## Bot verbs

Two textual commands, typed in any channel the gateway is in or privately to its
nick. Both answer **privately**, to whoever typed them — a roster is for the
person who asked, not for the room.

```text
<alice> mu peers
     (privately, from mu-gw)  2 agents on the mesh right now:
     (privately, from mu-gw)  cc:abc — #cc-abc
     (privately, from mu-gw)  mu:d5 — #mu-d5

<alice> mu say cc-abc deploy is green
     (nothing comes back: the line was delivered to cc:abc)
```

`mu peers` lists the agents `$SRV` discovery currently sees, each by its full
peer id — the spelling `mu say` and an explicit address both take — and the
channel it maps to. A channel two present peers fold onto is marked `(shared)`,
the same collision the mesh→IRC side labels bodies for. Humans are not listed:
they are not mesh destinations, and IRC already shows who is in the room.

`mu say <peer id or alias> <text>` is `cc:abc: text` in different clothes. The
destination is resolved against the same live presence set — the full peer id
first, then the alias the channel name is built from (`cc-abc`), folded under the
server's `CASEMAPPING` — and it publishes through the same code an explicit
address does, so both leave the same routing memory behind: the reply comes back
to the peer's channel if that is where you typed it, and privately otherwise.
An alias more than one peer answers to is refused naming them, an absent or
unknown destination is refused naming it, and a human is refused as a
destination the way any other human address is. A mesh that is down when the
line reaches the wire is the one failure the publish path reports rather than
the verb, and it is reported the same way: privately, to whoever typed the
command — where an explicit address gets that news back in the channel it was
typed in, because that is where the line itself was said.

A command line is never mirrored to the lobby, never fanned out, and never
published as itself. An `mu <verb>` the gateway does not implement — `mu peers`
with an argument, `mu say` with nothing to say — costs one line of usage and no
publication, which is the point of dispatching verbs first: a typo must not
become a broadcast. A bare `mu` is a word, not a verb, and is carried as
ordinary text.

## Humans-only fallback

`observe_agent_dms = true` subscribes `mu.agent.>` so the operator can watch
agent-to-agent traffic. If the server refuses that subscription, the gateway
logs it and **keeps running humans-only**: DMs addressed to the humans it fronts
still mirror; agent-to-agent traffic does not appear. The same is true with
`observe_agent_dms = false`, chosen deliberately.

The observer overlaps the gateway's own human endpoints, so one mesh message can
arrive twice. Delivery is de-duplicated on (id, destination, session) — not on
the id alone, so a fan-out that genuinely addressed several destinations is
still delivered once per destination.

Envelopes the observer cannot verify are counted (`observer_verify_failures` in
the session's closing log line): an unverified observer envelope is invisible to
both the sender and the recipient, so the count is the only sign that the watch
has a hole in it.

## Best-effort gaps (by design)

- **No replay across a reconnect.** Anything queued for a dead IRC connection
  dies with it, and mesh events that arrive while IRC is down are dropped rather
  than delivered late. After either side reconnects, membership is rebuilt from
  fresh NAMES, channels from fresh discovery, and routing memory is empty.
- **A full outbound queue drops lines.** The socket queue is bounded; a server
  that stops reading loses lines rather than growing the gateway's memory.
- **Discovery is up to 30 seconds stale.** An agent that appeared a moment ago
  may not be addressable yet, and one that just died may still be addressed —
  the message is published to a subject nobody is listening on.
- **Publishing is fire-and-forget.** Core NATS does not report whether anyone
  was subscribed. A failed publish is logged (body-free) and dropped.
- **No native mesh broadcast.** A fan-out is N ordinary DMs sharing one id.
- **A NATS outage is survived by the client's own reconnection**, plus explicit
  repair here: presence registrations do not survive the outage, so on link-up
  the gateway re-fronts every human still present, drops what it buffered,
  clears routing memory and refreshes discovery. That path has not been
  exercised against a broker restart.

## Running the live harness

The offline suites need nothing. The live one runs the **production bridge**
against a real IRC server and a real mesh, and skips with a reason otherwise:

```sh
MU_IRC_TEST_SERVER=127.0.0.1:6667 MU_IRC_TEST_TLS=0 \
  cargo test -p mu-irc-gateway --test live -- --nocapture
```

Against a private-CA server, with the authenticated path actually exercised:

```sh
MU_IRC_TEST_SERVER=irc.lan:6697 \
MU_IRC_TEST_TLS_CA=$HOME/.config/mu/irc-ca.pem \
MU_IRC_TEST_SASL_USER=mu-gw MU_IRC_TEST_SASL_PASSWORD=… \
  cargo test -p mu-irc-gateway --test live -- --nocapture
```

| variable | meaning |
| --- | --- |
| `MU_IRC_TEST_SERVER` | `host:port` of an IRC server. Required; without it the test skips. |
| `MU_IRC_TEST_TLS` | `0` for a plaintext server (then no SASL — credentials over cleartext are refused). |
| `MU_IRC_TEST_TLS_CA` | Path to a PEM CA bundle, fed to the same `tls_ca_file` the daemon reads. This is what lets the harness run over TLS against a server with a private CA — and therefore run the SASL leg, which TLS is a precondition for. Setting it with `MU_IRC_TEST_TLS=0` is a hard error, not a quiet downgrade. |
| `MU_IRC_TEST_NATS` | NATS url. Without it the harness starts a local `nats-server` (`NATS_BIN` to point at one) under a unique name and confirms that name in the broker's `INFO` greeting before using it. No binary at all is a skip; a binary that will not start is a failure. |
| `MU_IRC_TEST_ISSUER_KEY` | Hex Ed25519 mesh issuer key. A fresh one is generated for an isolated broker. |
| `MU_IRC_TEST_NICK`, `MU_IRC_TEST_LOBBY` | Defaults `mu-gw-test`, `#mu-live-test`. |
| `MU_IRC_TEST_SASL_USER`, `MU_IRC_TEST_SASL_PASSWORD` | SASL PLAIN, TLS only. |

With `MU_IRC_TEST_TLS_CA` and SASL credentials set, the run also WHOISes the
gateway and requires `RPL_WHOISACCOUNT` naming the configured user, so the
authenticated registration is confirmed by the server rather than inferred.

It connects, registers, joins the lobby, checks that a human's JOIN became a
`human:` endpoint on `$SRV`, routes one line each way, runs both bot verbs
(`mu peers` names the test's own mesh peer in a private answer, and `mu say`
reaches it), checks the mesh→IRC line reaches the human's nick rather than the
channel they are sitting in and arrives exactly once despite the
endpoint/observer overlap, and shuts down with a `QUIT` and a released
endpoint. Neither server is faked: what needs a server and
does not have one is reported as a skip, never mocked.

## Not here yet

- **Multi-nick / multi-operator fan-in** — exactly one gateway nick.
- **IRC-side persistence, scrollback or history** — none, deliberately.
- **Daemon changes**, or any IRC awareness in `mu-coding` — none.
- **A native mesh broadcast subject** — fan-out goes to discovered endpoints.
- **Provisioning IRC or NATS servers** — live runs use operator-provided
  services and credentials.
