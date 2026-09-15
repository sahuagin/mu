# mu-irc-gateway

A standalone single-nick IRC frontend to the mu agent mesh: the gateway joins
one IRC server as one nick and mirrors, live and best-effort, between IRC and
the NATS agent mesh, so a person can watch and address mesh agents from an
ordinary IRC client. Mesh agents appear as channels; humans on IRC appear on
the mesh as `human:<nick>` peers while they are visible. The mu daemon is
unaware IRC exists.

This file is the operator's page: how to configure and run the binary, how to
run the live harness, and what the gateway deliberately does not do. How the
crate works — module responsibilities, routing and precedence rules, loop
guards, alias and channel naming, framing, reconnect behaviour — is in the
rustdoc, and is not repeated here:

```sh
cargo doc -p mu-irc-gateway --open
```

The design of record is `specs/plans/mu-irc-gateway-v0.md`.

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
| `tls` | no | `true` | connect over TLS, verified against the system trust store plus `tls_ca_file` |
| `tls_ca_file` | no | — | PEM bundle of extra CA certificates to trust for this connection (a private CA) |
| `tls_system_roots` | no | `true` | whether the system trust store is part of that trust; `false` leaves `tls_ca_file` as the only anchors |
| `sasl_user` | no | — | SASL PLAIN account. Once set, SASL is mandatory: registration fails rather than completing unauthenticated, and `tls = false` is refused |
| `sasl_password` | no | — | SASL PLAIN password, inline |
| `sasl_password_file` | no | — | SASL PLAIN password from a file. Exactly one of the two password forms must accompany `sasl_user`, and neither without it |
| `channel_prefix` | no | `#` | prefix for agent channels |
| `lobby` | no | `#mu` | the fan-out / fallback channel |
| `observe_agent_dms` | no | `true` | whether to observe the agent-DM wildcard (`mu.agent.>`). If the server refuses that subscription the gateway logs it and keeps running humans-only, as it does with `false` |

### `[dialogue.mesh]` / `[mesh]`

Unchanged from every other mesh client — `enabled`, `nats_url`, `issuer_key`.
The gateway adds no mesh settings of its own.

### Not configurable in v0

Discovery is swept every 30 seconds (that sweep is also the channel
reconciliation tick), the reconnect backoff runs 2 s → 5 min, registration must
complete within 60 s, and a refused JOIN backs off 30 s → 10 min. These are
constants in the `bridge` and `membership` modules; making one an operator
dial is a config-surface change with its own review.

## Trusting a private CA

The trust model behind `tls_ca_file` and `tls_system_roots` — why the anchors
are additive, why validation is eager, and why there is no "skip verification"
switch — is in `specs/plans/mu-irc-gateway-v0.md`, **Amendment, 2026-09-11**,
and in the `transport` module's rustdoc. Read those before changing any of it;
what follows is only how to set it up.

### Making one with `openssl`

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

# 3. The SANs are the part that matters, and the part a hand-made certificate
#    usually gets wrong: list every name AND address the gateway (and your
#    other clients) will connect by. The name is checked exactly — connecting
#    by IP needs an IP: entry; a DNS: entry or the CN does not cover it.
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

### The same CA on a phone

Your own IRC client has to trust the CA too, and a phone will not read the
gateway's config. On iOS with **Igloo IRC**: mail or AirDrop `ca.pem` to the
device, open it (Settings offers to install a *profile*), then — and this is the
step that is easy to miss — **Settings → General → About → Certificate Trust
Settings** and switch the CA on. Installing the profile alone is not enough; iOS
keeps root trust behind that second toggle, and Igloo will keep refusing the
connection until it is flipped. Only `ca.pem` ever leaves the machine: never
`ca.key`, and never the server's key.

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

6. Talk to an agent. Everything is ordinary IRC text; there are no slash
   commands. Prefix a line with the agent's peer id and a colon, in `#mu` or in
   the agent's own channel:

   ```text
   cc:9f2c: what's the state of the deploy?
   ```

   In the agent's own channel (`#cc-9f2c`) the prefix is optional, and the
   reply comes back there. Typed anywhere else — `#mu`, another agent's
   channel, or a private message to the gateway — the reply comes back as a
   private message. A line in `#mu` with no prefix, or a private message to
   the gateway's nick, goes to every discovered agent.

7. Stop it with Ctrl-C or `kill -TERM`: it sends a `QUIT` and releases every
   `human:` endpoint it fronted before exiting. The `IRC session ended` log
   line carries the session's drop and refusal counters; a non-zero
   `observer_verify_failures` is the only sign that an observer envelope could
   not be verified, since such an envelope is invisible to both its sender and
   its recipient.

## Bot verbs

Two textual commands, typed in any channel the gateway is in or privately to
its nick. Both answer privately, to whoever typed them:

```text
<alice> mu peers
     (privately, from mu-gw)  2 agents on the mesh right now:
     (privately, from mu-gw)  cc:abc — #cc-abc
     (privately, from mu-gw)  mu:d5 — #mu-d5

<alice> mu say cc-abc deploy is green
     (nothing comes back: the line was delivered to cc:abc)
```

`mu say` takes either the full peer id or the channel alias (`cc-abc`) and
behaves exactly like the `cc:abc: text` form above, replies included. A verb
the gateway does not implement, or one with the wrong arguments, gets a
one-line usage reply and publishes nothing.

## Best-effort gaps (by design)

The mechanics are in the `bridge` rustdoc; these are the consequences an
operator will see:

- **No replay across a reconnect, on either side.** Whatever was queued for a
  dead IRC connection dies with it, and mesh events arriving while IRC is down
  are dropped, not delivered late. Membership, channels and routing memory
  start empty after either side reconnects.
- **A full outbound queue drops lines.** A server that stops reading loses
  lines rather than growing the gateway's memory.
- **Discovery is up to 30 seconds stale.** An agent that appeared a moment ago
  may not be addressable yet; one that just died may still be addressed, to a
  subject nobody is listening on.
- **Publishing is fire-and-forget.** Core NATS does not report whether anyone
  was subscribed. A failed publish is logged (body-free) and dropped.
- **No native mesh broadcast.** A fan-out is N ordinary DMs sharing one id.
- **A NATS outage is survived by the client's own reconnection**, plus repair
  on link-up: every human still present is re-fronted, buffered publishes are
  dropped, routing memory is cleared and discovery refreshed. That path has
  not been exercised against a broker restart.

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
Neither server is faked: what needs a server and does not have one is reported
as a skip, never mocked.

## Not here yet

- **Multi-nick / multi-operator fan-in** — exactly one gateway nick.
- **IRC-side persistence, scrollback or history** — none, deliberately.
- **Daemon changes**, or any IRC awareness in `mu-coding` — none.
- **A native mesh broadcast subject** — fan-out goes to discovered endpoints.
- **Provisioning IRC or NATS servers** — live runs use operator-provided
  services and credentials.
