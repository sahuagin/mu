//! The opt-in live harness: the PRODUCTION bridge against a real IRC server and
//! a real NATS, driven from the outside by a stand-in human.
//!
//! Nothing here re-implements the gateway. The thing under test is
//! [`mu_irc_gateway::bridge::run`] — the same function `mu-irc-gateway`'s
//! `main` calls, with the same configuration type — and the only test-side
//! machinery is a person's IRC client (built on the gateway's own
//! [`transport`](mu_irc_gateway::transport), so even the socket is production
//! code) and an ordinary mesh peer (built on `mu_dialogue::mesh`, the same
//! client the daemon uses).
//!
//! # Running it
//!
//! ```text
//! MU_IRC_TEST_SERVER=127.0.0.1:6667 MU_IRC_TEST_TLS=0 \
//!   cargo test -p mu-irc-gateway --test live -- --nocapture
//! ```
//!
//! | variable | meaning |
//! | --- | --- |
//! | `MU_IRC_TEST_SERVER` | `host:port` of an IRC server. **Required**; without it every test here skips. |
//! | `MU_IRC_TEST_TLS` | `0` for a plaintext server. Default: TLS (and then SASL, if configured). |
//! | `MU_IRC_TEST_NATS` | NATS url. Optional: without it the harness spawns a local `nats-server` on an OS-assigned port under a name nothing else has, and proves it owns the port by reading that name back out of the broker's `INFO` greeting. It skips only when there is no `nats-server` binary at all (`NATS_BIN` to point at one); a binary that is present and will not start is a failure, not a skip. |
//! | `MU_IRC_TEST_ISSUER_KEY` | Hex Ed25519 mesh issuer key. Optional: a fresh one is generated for an isolated broker. |
//! | `MU_IRC_TEST_NICK` | The gateway's nick. Default `mu-gw-test`. |
//! | `MU_IRC_TEST_LOBBY` | The lobby channel. Default `#mu-live-test`. |
//! | `MU_IRC_TEST_SASL_USER` / `MU_IRC_TEST_SASL_PASSWORD` | SASL PLAIN, both or neither. TLS only (the adapter refuses credentials over cleartext), so with `MU_IRC_TEST_TLS=0` the SASL leg is skipped aloud; half a pair is a configuration error and fails the run. |
//!
//! # What it does NOT fake
//!
//! There is no in-process stand-in for either server. The mesh client is a real
//! NATS client and the transport is a real socket, so a piece that needs a
//! server and does not have one is reported as a skip with the reason, never
//! papered over with a mock that would prove something else.

use std::time::Duration;

use mu_dialogue::mesh::{self, MeshDmEvent, MeshTarget};
use mu_peer::PeerId;
use tokio::sync::{mpsc, watch};

use mu_irc_gateway::adapter::{IrcMessage, Transport as _};
use mu_irc_gateway::bridge;
use mu_irc_gateway::config::{GatewayConfig, IrcConfig, MeshConfig, SaslCreds, Secret};
use mu_irc_gateway::transport::{self, ConnectionGuard, FromServer, LineWriter};

/// How long any single "wait for the server/mesh to do the thing" step gets.
const STEP: Duration = Duration::from_secs(30);

// ─────────────────────────────── The test ───────────────────────────────────

/// One connection, one registration, one join, one human presence, and one line
/// in each direction — through the production bridge.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_bridge_registers_joins_fronts_a_human_and_routes_both_ways() {
    let Some(server) = env("MU_IRC_TEST_SERVER") else {
        skip(
            "MU_IRC_TEST_SERVER is unset. Set it to an IRC server's `host:port` \
             (add MU_IRC_TEST_TLS=0 for a plaintext one) to run this against a real server. \
             Nothing here is faked: without a server there is nothing to prove.",
        );
        return;
    };
    let tls = env("MU_IRC_TEST_TLS").as_deref() != Some("0");
    // Read before anything is started: a half-configured credential pair is a
    // configuration error, and the operator should hear about it now rather
    // than after a broker and two connections have been spent.
    let sasl = sasl(tls);
    let sasl_account = sasl.as_ref().map(|creds| creds.user.clone());

    // The mesh half. The bridge connects to NATS before it connects to IRC, so
    // there is no version of this test that runs without one.
    let Some((mesh_url, _nats)) = mesh_url().await else {
        skip(
            "MU_IRC_TEST_SERVER is set, but there is no mesh to bridge to: MU_IRC_TEST_NATS \
             is unset and no `nats-server` could be started (set NATS_BIN, or put one on PATH). \
             The gateway speaks NATS through the real mesh client, so this half cannot be \
             stood in for — the round trip is NOT being checked.",
        );
        return;
    };
    let issuer_key = env("MU_IRC_TEST_ISSUER_KEY")
        .unwrap_or_else(|| biscuit_auth::KeyPair::new().private().to_bytes_hex());

    let nick = env("MU_IRC_TEST_NICK").unwrap_or_else(|| "mu-gw-test".to_string());
    let lobby = env("MU_IRC_TEST_LOBBY").unwrap_or_else(|| "#mu-live-test".to_string());
    // Lower-case on purpose: every server's CASEMAPPING folds it to itself, so
    // the test does not have to guess which rule this server advertises.
    let human_nick = format!("mu-human-{}", std::process::id() % 10_000);
    let agent = PeerId::parse(&format!("cc:live-{}", std::process::id()));

    let mesh_cfg = MeshConfig {
        enabled: true,
        nats_url: mesh_url.clone(),
        issuer_key: issuer_key.clone(),
    };

    // An ordinary mesh peer, fronted BEFORE the bridge starts so the gateway's
    // first `$SRV` sweep already sees it — this is the agent the human will
    // address and the one that will DM them back.
    let (peer_gw, _store_rx) = mesh::connect(&mesh_cfg)
        .await
        .expect("the test's own mesh peer connects");
    let (agent_tx, mut agent_rx) = mpsc::unbounded_channel::<MeshDmEvent>();
    peer_gw
        .front_peer_events(&agent.to_string(), agent_tx)
        .await
        .expect("fronting the test agent on the mesh");

    // A person, on IRC, in the lobby.
    let mut human = Client::connect(&server, tls, &human_nick).await;
    human.send(&format!("JOIN {lobby}"));
    human
        .wait_for("our own JOIN", |m| {
            m.command == "JOIN" && m.params.first().is_some_and(|c| eq(c, &lobby))
        })
        .await;

    // ── The thing under test ────────────────────────────────────────────────
    let config = GatewayConfig {
        irc: IrcConfig {
            server: server.clone(),
            tls,
            nick: nick.clone(),
            sasl,
            channel_prefix: "#".to_string(),
            lobby: lobby.clone(),
            observe_agent_dms: true,
        },
        mesh: mesh_cfg,
    };
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let bridge = tokio::spawn(bridge::run(config, shutdown_rx));

    // 1. It registers and joins the lobby. Asked for from the outside, by a
    //    person in the channel, so this is the server's view and not the
    //    gateway's own bookkeeping.
    human
        .wait_for("the gateway to join the lobby", |m| {
            m.command == "JOIN"
                && m.params.first().is_some_and(|c| eq(c, &lobby))
                && nick_of(m).eq_ignore_ascii_case(&nick)
        })
        .await;

    // 1b. …and, when SASL was configured, it registered AUTHENTICATED. Asserted
    //     before anything else is checked: a run the operator set credentials
    //     for must not report a green round trip that happened anonymously.
    if let Some(account) = &sasl_account {
        assert_authenticated(&mut human, &nick, account).await;
    }

    // 2. The human's presence in that channel became a mesh endpoint. Read back
    //    through `$SRV` discovery — the same sweep any peer uses.
    let human_peer = PeerId::human(&human_nick);
    wait_until("the human to appear on the mesh", || async {
        peer_gw
            .srv_agents()
            .await
            .map(|agents| agents.contains_key(&human_peer.to_string()))
            .unwrap_or(false)
    })
    .await;

    // 3. IRC → mesh: the human addresses the agent in the lobby.
    let outbound_body = format!("hello from irc {}", std::process::id());
    human.send(&format!("PRIVMSG {lobby} :{agent}: {outbound_body}"));
    let received = tokio::time::timeout(STEP, agent_rx.recv())
        .await
        .expect("the agent receives the human's line within the step timeout")
        .expect("the agent's endpoint stream stays open");
    assert_eq!(received.body, outbound_body, "the body crosses unchanged");
    assert_eq!(
        received.from,
        human_peer.to_string(),
        "the sender is the human the gateway fronts, asserted under the gateway's capability"
    );

    // 4. mesh → IRC: the agent answers. The human is present in the lobby but
    //    not in the agent's channel, so this arrives privately — to the human's
    //    nick and not into the channel they are sitting in — and exactly once,
    //    though the endpoint subscription AND the observer wildcard both match
    //    it.
    let inbound_body = format!("hello from the mesh {}", std::process::id());
    peer_gw
        .publish_dm(
            &agent.to_string(),
            &MeshTarget {
                subject: human_peer.dm_subject(),
                session: None,
            },
            &inbound_body,
            None,
        )
        .await
        .expect("the agent publishes to the human's endpoint");
    let delivered = human
        .wait_for("the mesh DM to reach IRC", |m| {
            m.command == "PRIVMSG" && m.params.get(1).is_some_and(|t| t.contains(&inbound_body))
        })
        .await;
    assert!(
        nick_of(&delivered).eq_ignore_ascii_case(&nick),
        "it came from the gateway, not from another client: {:?}",
        delivered.prefix
    );
    // …and it was addressed to the HUMAN. The wait above is deliberately
    // target-blind so that a public copy is caught here rather than skipped
    // over: the human is in the lobby, so a regression that answered a private
    // mesh DM into the channel would satisfy the command, the body and the
    // sender, and "this arrives privately" is the whole claim being made.
    assert!(
        delivered
            .params
            .first()
            .is_some_and(|target| target.eq_ignore_ascii_case(&human_nick)),
        "the mesh DM was delivered to {:?} rather than privately to `{human_nick}`: \
         a private body put on a channel is a disclosure, not a delivery",
        delivered.params.first()
    );
    // Exactly once: a second copy would be the endpoint/observer overlap
    // leaking through the de-duplication window. Target-blind on purpose too,
    // so an extra PUBLIC copy alongside the correct private one still fails —
    // the pair "right delivery plus a leak" is not a pass.
    assert!(
        human
            .try_wait_for(Duration::from_secs(3), |m| {
                m.command == "PRIVMSG" && m.params.get(1).is_some_and(|t| t.contains(&inbound_body))
            })
            .await
            .is_none(),
        "the same mesh DM was delivered twice (to any target: a second copy on the lobby \
         is the same defect as a second private one)"
    );

    // 5. Shutdown: a clean QUIT, and the human endpoint released from `$SRV`.
    shutdown_tx.send(true).expect("the bridge is still running");
    tokio::time::timeout(STEP, bridge)
        .await
        .expect("the bridge stops on the shutdown signal")
        .expect("the bridge task does not panic")
        .expect("the bridge exits cleanly");
    human
        .wait_for("the gateway to QUIT", |m| {
            (m.command == "QUIT" || m.command == "PART") && nick_of(m).eq_ignore_ascii_case(&nick)
        })
        .await;
    wait_until("the human endpoint to be released", || async {
        peer_gw
            .srv_agents()
            .await
            .map(|agents| !agents.contains_key(&human_peer.to_string()))
            .unwrap_or(false)
    })
    .await;
}

// ────────────────────────────── The stand-ins ───────────────────────────────

/// A person's IRC client: the gateway's own transport, driven by hand. It runs
/// no gateway logic — it types and reads, which is the whole point.
///
/// The socket is owned by a background task ([`drain`]) rather than by the wait
/// helpers, because the waits are not the only time this connection has to stay
/// alive. Steps 3, 4 and 5 wait on the *mesh* side for up to [`STEP`] each while
/// nobody is reading IRC; a server with a shorter idle timeout PINGs during one
/// of those, gets no PONG, and closes — and the harness then fails for a reason
/// that has nothing to do with the gateway. The drain answers PING at all times
/// and hands everything else to whoever is waiting.
struct Client {
    /// Lines this test types. The drain task owns the writer and sends them, so
    /// a PONG it has to emit is never queued behind a wait.
    outbox: mpsc::UnboundedSender<String>,
    /// Everything the drain saw, in order, PINGs already answered.
    inbound: mpsc::UnboundedReceiver<FromServer>,
    /// Ends the drain (and with it the connection) when the client is dropped.
    _drain: DrainGuard,
}

/// Aborts the drain task, which owns the [`ConnectionGuard`]; the socket goes
/// with it. Held by the client so a panicking test tears its connection down.
struct DrainGuard(tokio::task::JoinHandle<()>);

impl Drop for DrainGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Own the connection: answer every PING the moment it arrives, send whatever
/// the test types, and forward the rest. Ends when the client is dropped or the
/// connection does.
async fn drain(
    mut writer: LineWriter,
    guard: ConnectionGuard,
    mut inbound: mpsc::Receiver<FromServer>,
    mut outbox: mpsc::UnboundedReceiver<String>,
    seen: mpsc::UnboundedSender<FromServer>,
) {
    // Held for the task's lifetime: dropping it closes the socket.
    let _guard = guard;
    loop {
        tokio::select! {
            typed = outbox.recv() => {
                let Some(line) = typed else { return };
                if writer.send_line(&line).is_err() {
                    return;
                }
            }
            event = inbound.recv() => {
                let Some(event) = event else { return };
                if let FromServer::Line(line) = &event {
                    let msg = IrcMessage::parse(line);
                    if msg.command == "PING" {
                        let token = msg.params.last().cloned().unwrap_or_default();
                        let _ = writer.send_line(&format!("PONG :{token}"));
                    }
                }
                let closed = matches!(event, FromServer::Closed(_));
                if seen.send(event).is_err() || closed {
                    return;
                }
            }
        }
    }
}

impl Client {
    async fn connect(server: &str, tls: bool, nick: &str) -> Self {
        let conn = transport::connect(server, tls, Duration::from_secs(20))
            .await
            .expect("the test client connects to the IRC server");
        let (outbox_tx, outbox_rx) = mpsc::unbounded_channel();
        let (seen_tx, seen_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(drain(
            conn.writer,
            conn.guard,
            conn.inbound,
            outbox_rx,
            seen_tx,
        ));
        let mut client = Client {
            outbox: outbox_tx,
            inbound: seen_rx,
            _drain: DrainGuard(task),
        };
        client.send(&format!("NICK {nick}"));
        client.send(&format!("USER {nick} 0 * :{nick}"));
        client
            .wait_for("the welcome numeric", |m| m.command == "001")
            .await;
        client
    }

    fn send(&self, line: &str) {
        self.outbox
            .send(line.to_string())
            .unwrap_or_else(|e| panic!("the test client sends `{line}`: {e}"));
    }

    /// Read until a message matches. Panics with the step's name on timeout, so
    /// a failure says which step never happened. PINGs are already answered by
    /// the drain, whether or not anyone is in here.
    async fn wait_for(&mut self, what: &str, pred: impl Fn(&IrcMessage) -> bool) -> IrcMessage {
        match self.try_wait_for(STEP, pred).await {
            Some(msg) => msg,
            None => panic!("timed out after {STEP:?} waiting for {what}"),
        }
    }

    /// The same, but a timeout is an answer (`None`) rather than a failure.
    async fn try_wait_for(
        &mut self,
        limit: Duration,
        pred: impl Fn(&IrcMessage) -> bool,
    ) -> Option<IrcMessage> {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let event = tokio::time::timeout_at(deadline, self.inbound.recv())
                .await
                .ok()??;
            let line = match event {
                FromServer::Line(line) => line,
                FromServer::Closed(why) => panic!("the test client's connection closed: {why}"),
            };
            let msg = IrcMessage::parse(&line);
            if pred(&msg) {
                return Some(msg);
            }
        }
    }
}

/// How many times to try for a broker of our own before giving up. Each attempt
/// takes a fresh ephemeral port, so this only ever runs more than once when
/// something else on the host wins the race for one.
const BROKER_ATTEMPTS: usize = 5;

/// How long one attempt waits for the spawned broker to answer.
const BROKER_READY: Duration = Duration::from_secs(10);

/// The NATS url to bridge to, and the broker process to keep alive for the run
/// when the harness started one. `None` means `nats-server` is not on this host
/// at all, which is a skip and not a failure.
///
/// The handle is returned rather than leaked on purpose: an un-reaped child with
/// inherited descriptors keeps the test binary itself from exiting, so a run
/// that passed in four seconds otherwise sits there until something kills it.
///
/// # Why this is fussier than "start it and connect"
///
/// The broker this returns is the only mesh the run has, so "a broker answered"
/// is not the question — "did the broker WE started answer" is. A derived port
/// (this used to be `14_600 + pid % 300`) collides with a concurrent run and
/// with anything already listening there, and a bare `TcpStream::connect`
/// succeeds against whatever holds the port. Between them, a run could bridge to
/// a stranger's broker and pass, proving nothing about an isolated mesh. So the
/// port comes from the OS, the child is started under a name nothing else has
/// ([`broker_name`]), and readiness is [`broker_ready`], which wants our own
/// child alive *and* a NATS `INFO` on that port *announcing that name*.
async fn mesh_url() -> Option<(String, Option<NatsServer>)> {
    if let Some(url) = env("MU_IRC_TEST_NATS") {
        return Some((url, None));
    }
    let bin = std::env::var("NATS_BIN").unwrap_or_else(|_| "nats-server".to_string());
    let mut last = String::new();
    for _ in 0..BROKER_ATTEMPTS {
        let port = free_port().expect("the OS hands out an ephemeral loopback port");
        let name = broker_name();
        let child = match spawn_broker(&bin, port, &name) {
            Ok(child) => child,
            // The binary is not here. Nothing to retry, and a skip — not a
            // failure — is the honest answer.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            // Anything else is a broker that exists and would not start, which
            // is a different fact from absence and must not be reported as one:
            // an operator who set NATS_BIN and got a green skip would never
            // learn the round trip was not exercised.
            Err(e) => panic!(
                "the live harness could not start a NATS broker: spawning `{bin}` failed \
                 ({e}). The binary is present but unrunnable — permissions, the wrong \
                 format, or no room for another process — so this is a failure and not the \
                 missing-binary skip. Point NATS_BIN at a runnable `nats-server`, or set \
                 MU_IRC_TEST_NATS to a broker that is already up."
            ),
        };
        let mut server = NatsServer(child);
        match broker_ready(&mut server, port, &name, BROKER_READY).await {
            Ok(()) => return Some((format!("127.0.0.1:{port}"), Some(server))),
            // Someone else took the port between the probe and the bind, or the
            // broker died. Either way this attempt owns nothing; drop it (which
            // kills and reaps it) and take a different port.
            Err(why) => last = why,
        }
    }
    panic!(
        "the live harness could not start a NATS broker it owns after {BROKER_ATTEMPTS} \
         attempts on ephemeral loopback ports. Last attempt: {last}. Refusing to continue: \
         bridging to a broker this harness did not start would test someone else's mesh. \
         Set MU_IRC_TEST_NATS to use a broker deliberately."
    );
}

/// Take a loopback port the OS says is free, and let it go again. There is a
/// window between the release and the broker's bind — that is what
/// [`BROKER_ATTEMPTS`] is for — but it beats deriving a port from the pid, which
/// is not an allocation at all.
fn free_port() -> Option<u16> {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
    let port = probe.local_addr().ok()?.port();
    drop(probe);
    Some(port)
}

/// A broker name nothing else on this host is using, for `nats-server -n`.
///
/// This is the identity half of [`broker_ready`]. The port is an OS ephemeral
/// one and [`free_port`] lets it go before the child binds, so the only way to
/// tell our child's listener from a stranger that took the same number is to
/// have told our child something to say — and then to hear it said.
fn broker_name() -> String {
    use std::hash::{BuildHasher as _, Hasher as _};

    // Three sources because one is not enough: `RandomState` is OS-seeded, so
    // two harnesses that start in the same nanosecond still differ; the pid and
    // the attempt counter keep every name within one process distinct too.
    static ATTEMPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u32(std::process::id());
    hasher.write_u64(ATTEMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or_default(),
    );
    format!("mu-irc-live-{:016x}", hasher.finish())
}

/// Start `nats-server` on `port` under `name`, or report why it could not be.
///
/// The spawn error is kept rather than flattened into an `Option`, because only
/// one kind of failure is a skip. [`NotFound`](std::io::ErrorKind::NotFound)
/// means there is no broker binary on this host — nothing to run, nothing to
/// prove, say so and move on. `PermissionDenied`, a file that is not an
/// executable, and process-resource exhaustion all describe a binary that IS
/// there and will not start; collapsing those into the same `None` turned an
/// explicitly requested live run into a green "skipped" with the wrong reason.
fn spawn_broker(bin: &str, port: u16, name: &str) -> std::io::Result<std::process::Child> {
    std::process::Command::new(bin)
        .args(["-p", &port.to_string(), "-a", "127.0.0.1", "-n", name])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
}

/// Wait up to `limit` for the broker THIS harness spawned — the one started as
/// `name` — to be serving `port`, or say why it is not.
///
/// Three checks, none of them redundant:
///
/// - **The child is alive.** `nats-server` exits when it cannot bind, so a dead
///   child is a lost port, reported at once instead of waiting out the clock.
/// - **The listener speaks NATS.** A completed TCP connect proves a socket
///   exists; an `INFO` greeting proves the protocol.
/// - **That `INFO` carries our name.** This is the one the other two cannot
///   make. [`free_port`] releases its probe socket before the child binds, so an
///   unrelated NATS can take the port in that window and answer the handshake
///   while our child is merely *starting* — not yet bound, so not yet exited,
///   so still alive at both `try_wait` checks. "A live child" and "a broker on
///   the port" are then two true statements about two different processes, and
///   only a name we invented and handed to our own child joins them up.
///
/// A stranger holding the port is not fatal here: it returns an error naming
/// what it found, and [`mesh_url`] retries on a fresh port.
async fn broker_ready(
    server: &mut NatsServer,
    port: u16,
    name: &str,
    limit: Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + limit;
    // The last name heard from something that was NOT ours, kept for the
    // timeout message: "a stranger is on this port" and "nothing is on this
    // port" are different diagnoses.
    let mut stranger: Option<String> = None;
    loop {
        if let Some(status) = server.exited() {
            return Err(format!(
                "`nats-server` on port {port} exited ({status}) before it was ready; \
                 the port was most likely already taken"
            ));
        }
        match nats_server_name(port).await {
            Some(served) if served == name => {
                return match server.exited() {
                    None => Ok(()),
                    Some(status) => Err(format!(
                        "port {port} answers the NATS handshake as `{name}`, but the broker \
                         this harness spawned has exited ({status}) — whatever is on that \
                         port now, the run cannot claim to own it"
                    )),
                };
            }
            Some(served) => stranger = Some(served),
            None => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(match stranger {
                Some(served) => format!(
                    "port {port} is served by a NATS broker calling itself `{served}`, not by \
                     the one this harness spawned (`{name}`) — something else took the port \
                     between the probe and our child's bind"
                ),
                None => format!(
                    "nothing answered the NATS `INFO` handshake on port {port} within \
                     {limit:?}, though the spawned `nats-server` is still alive"
                ),
            });
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// How much of a greeting to read before giving up on finding a line in it.
const GREETING_LIMIT: u64 = 8 * 1024;

/// The `server_name` the NATS broker on `port` announces, or `None` when
/// nothing there greets as one.
///
/// Every NATS server opens the conversation with `INFO <json>`, so reading that
/// line is proof of the protocol where a completed TCP connect is proof only
/// that a socket exists — and the `server_name` inside it is proof of *which*
/// broker, which the `INFO` prefix on its own is not.
///
/// Bounded on both axes, because the thing on the other end is precisely the
/// thing being distrusted: the byte cap ends a peer that streams without ever
/// sending a newline, and the timeout ends one that says nothing at all.
async fn nats_server_name(port: u16) -> Option<String> {
    use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _};

    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .ok()?;
    let mut reader = tokio::io::BufReader::new(stream.take(GREETING_LIMIT));
    let mut greeting = String::new();
    tokio::time::timeout(Duration::from_secs(1), reader.read_line(&mut greeting))
        .await
        .ok()?
        .ok()?;
    let info: serde_json::Value =
        serde_json::from_str(greeting.strip_prefix("INFO ")?.trim()).ok()?;
    Some(info.get("server_name")?.as_str()?.to_string())
}

/// The broker this harness started. Killed and reaped when the test ends,
/// whether it passed or panicked.
struct NatsServer(std::process::Child);

impl NatsServer {
    /// The exit status if the child is already gone, `None` while it lives.
    /// A wait error counts as gone: a child we can no longer ask about is not a
    /// child we can claim owns a port.
    fn exited(&mut self) -> Option<String> {
        match self.0.try_wait() {
            Ok(Some(status)) => Some(status.to_string()),
            Ok(None) => None,
            Err(e) => Some(format!("unknown status: {e}")),
        }
    }
}

impl Drop for NatsServer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// SASL credentials from the environment.
///
/// Only three of the four shapes are runs. Both variables set over TLS is the
/// SASL run; neither set is a deliberate no-SASL run. The other two used to
/// return `None` silently, which is the failure mode worth closing: an operator
/// who set credentials and got a green run had no way to tell whether the
/// authenticated path had been exercised or quietly dropped.
///
/// - **One of the two set** is a configuration error, not a run. It panics.
/// - **Both set with `MU_IRC_TEST_TLS=0`** is a skip *with the reason printed*.
///   The adapter fails closed on credentials over cleartext, so they cannot be
///   offered; the rest of the harness still runs, and the operator is told in
///   as many words that the SASL leg did not.
fn sasl(tls: bool) -> Option<SaslCreds> {
    let user = env("MU_IRC_TEST_SASL_USER");
    let password = env("MU_IRC_TEST_SASL_PASSWORD");
    let (user, password) = match (user, password) {
        (Some(user), Some(password)) => (user, password),
        (None, None) => return None,
        (Some(_), None) => panic!(
            "MU_IRC_TEST_SASL_USER is set but MU_IRC_TEST_SASL_PASSWORD is not. SASL PLAIN \
             needs both, and half a credential pair is a configuration error rather than a \
             run without SASL — set the password, or unset the user to run without it."
        ),
        (None, Some(_)) => panic!(
            "MU_IRC_TEST_SASL_PASSWORD is set but MU_IRC_TEST_SASL_USER is not. SASL PLAIN \
             needs both, and half a credential pair is a configuration error rather than a \
             run without SASL — set the user, or unset the password to run without it."
        ),
    };
    if !tls {
        skip(
            "SASL credentials are configured but MU_IRC_TEST_TLS=0. The adapter fails closed \
             on credentials over cleartext, so this run registers WITHOUT SASL and the \
             authenticated registration path is NOT being checked. Drop MU_IRC_TEST_TLS=0 to \
             exercise it.",
        );
        return None;
    }
    Some(SaslCreds {
        user,
        password: Secret::new(password),
    })
}

/// Confirm from the server's side that the gateway's connection is authenticated.
///
/// Registration reaching ready is already the adapter's own fail-closed proof —
/// with SASL configured it refuses to become ready without `903`, so the join
/// the harness has just watched could not have happened unauthenticated. This
/// asks the server the same question out loud, because "the code would have
/// refused" is an argument and `RPL_WHOISACCOUNT` is an observation.
async fn assert_authenticated(human: &mut Client, gateway_nick: &str, account: &str) {
    human.send(&format!("WHOIS {gateway_nick}"));
    let reply = human
        .wait_for("the server's WHOIS answer for the gateway", |m| {
            // 330 RPL_WHOISACCOUNT, or 318 RPL_ENDOFWHOIS if there is no account.
            (m.command == "330" || m.command == "318")
                && m.params
                    .get(1)
                    .is_some_and(|n| n.eq_ignore_ascii_case(gateway_nick))
        })
        .await;
    assert_eq!(
        reply.command, "330",
        "SASL is configured, but the server's WHOIS of {gateway_nick} reports no account: \
         either the gateway registered unauthenticated, or this server does not report \
         accounts in WHOIS. Either way the authenticated path is unconfirmed."
    );
    assert!(
        reply
            .params
            .get(2)
            .is_some_and(|a| a.eq_ignore_ascii_case(account)),
        "the gateway is logged in, but not as the configured SASL user `{account}`: {:?}",
        reply.params
    );
}

/// Poll `check` until it is true, or fail naming the step.
async fn wait_until<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + STEP;
    loop {
        if check().await {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out after {STEP:?} waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// A non-empty environment variable, or `None`.
fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Report a skip on both streams: `cargo test` hides stdout unless
/// `--nocapture`, and a silent skip reads exactly like a pass.
fn skip(reason: &str) {
    println!("SKIP live IRC harness: {reason}");
    eprintln!("SKIP live IRC harness: {reason}");
}

// ───────────────────── Offline checks on the harness itself ─────────────────
//
// The broker-isolation helpers are the one part of this file that can be held to
// account without a server, and they are worth holding: their failure mode is
// not a red test, it is a green one that tested the wrong broker. These run on
// every `cargo test -p mu-irc-gateway`, with or without `MU_IRC_TEST_SERVER`.

/// The port `free_port` hands back is one the OS actually released, which is the
/// whole difference from deriving it from the pid: `14_600 + pid % 300` names a
/// port whether or not anything else is sitting on it.
#[test]
fn a_free_port_is_one_that_can_be_bound() {
    // Releasing the probe socket and binding the same port again is a race
    // against every other process on the host (and against TIME_WAIT), which
    // is exactly why the harness retries on a lost race. The test tolerates
    // the same race the same way: a handful of attempts, at least one binds.
    let mut last = None;
    for _ in 0..5 {
        let port = free_port().expect("the OS hands out an ephemeral loopback port");
        match std::net::TcpListener::bind(("127.0.0.1", port)) {
            Ok(taken) => {
                drop(taken);
                return;
            }
            Err(e) => last = Some((port, e)),
        }
    }
    panic!("no released port could be bound again in 5 attempts: {last:?}");
}

/// The readiness check the old harness had was `TcpStream::connect(..).is_ok()`,
/// which any listener satisfies. This is that listener: it accepts and then says
/// nothing, exactly like an unrelated service holding the port.
#[tokio::test]
async fn a_silent_tcp_listener_is_not_a_ready_broker() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binding a loopback listener");
    let port = listener
        .local_addr()
        .expect("the listener has an address")
        .port();
    let accepting = tokio::spawn(async move {
        // Accept and hold it open without a word.
        let _accepted = listener.accept().await;
        tokio::time::sleep(Duration::from_secs(5)).await;
    });

    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok(),
        "the old check — a bare TCP connect — would have called this ready"
    );
    assert!(
        nats_server_name(port).await.is_none(),
        "a listener that never sent an `INFO` was accepted as a NATS broker"
    );
    accepting.abort();
}

/// …and one that does talk, but is not NATS. The greeting has to BE `INFO `,
/// not merely exist.
#[tokio::test]
async fn a_listener_that_greets_with_something_else_is_not_a_broker() {
    use tokio::io::AsyncWriteExt as _;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binding a loopback listener");
    let port = listener
        .local_addr()
        .expect("the listener has an address")
        .port();
    let greeting = tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let _ = sock.write_all(b"+OK ready\r\n").await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    assert!(
        nats_server_name(port).await.is_none(),
        "a listener greeting with something other than `INFO` was accepted as a NATS broker"
    );
    greeting.abort();
}

/// A broker that is not there is not ready, however long the wait — and the
/// reason names the port, so a failing run says what it was looking for.
#[tokio::test]
async fn readiness_reports_a_child_that_exited_rather_than_waiting_out_the_clock() {
    // `false` is the smallest thing that behaves like a broker that could not
    // bind: it starts, and it is gone immediately.
    let Ok(child) = std::process::Command::new("false")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        skip("no `false` binary to stand in for a broker that exits; helper check skipped");
        return;
    };
    let port = free_port().expect("the OS hands out an ephemeral loopback port");
    let mut server = NatsServer(child);
    let started = std::time::Instant::now();
    let why = broker_ready(&mut server, port, "mu-irc-live-ours", BROKER_READY)
        .await
        .expect_err("a dead child must never be reported ready");
    assert!(
        why.contains(&port.to_string()),
        "the failure has to name the port it was waiting on: {why}"
    );
    assert!(
        started.elapsed() < BROKER_READY,
        "a child that is already gone should be reported at once, not after the full wait"
    );
}

/// The ordering neither the child check nor the `INFO` check can rule out on its
/// own: a real NATS broker on the port, a live child of ours, and they are not
/// the same process. [`free_port`] releases the port before the child binds, so
/// a stranger can win that window; our child has not failed its bind *yet*, so
/// it is still alive at both `try_wait` checks. Only the name separates them.
#[tokio::test]
async fn a_broker_greeting_under_another_name_is_not_the_one_we_spawned() {
    use tokio::io::AsyncWriteExt as _;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binding a loopback listener");
    let port = listener
        .local_addr()
        .expect("the listener has an address")
        .port();
    // A well-formed NATS greeting from a broker that is not ours, served for as
    // long as the readiness check keeps asking.
    let stranger = tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let _ = sock
                .write_all(
                    b"INFO {\"server_id\":\"NSTRANGER\",\"server_name\":\"somebody-elses-broker\",\"version\":\"2.14.3\"} \r\n",
                )
                .await;
        }
    });

    assert_eq!(
        nats_server_name(port).await.as_deref(),
        Some("somebody-elses-broker"),
        "the greeting has to parse, or the rejection below would be about the protocol \
         rather than about identity"
    );

    // A child that is alive and did not bind anything: the half-truth that used
    // to be read as ownership.
    let Ok(child) = std::process::Command::new("sleep")
        .arg("30")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        skip("no `sleep` binary to stand in for a live broker child; helper check skipped");
        stranger.abort();
        return;
    };
    let mut server = NatsServer(child);
    let why = broker_ready(
        &mut server,
        port,
        "mu-irc-live-ours",
        Duration::from_millis(300),
    )
    .await
    .expect_err("a stranger's broker must never be accepted as the one this harness spawned");
    assert!(
        why.contains("somebody-elses-broker") && why.contains(&port.to_string()),
        "the failure has to name the port and the broker that answered on it: {why}"
    );
    stranger.abort();
}

/// Absence is the only spawn failure that may skip a run. A binary that is
/// there and will not start is a failure, and the two have to be distinguishable
/// at the point [`mesh_url`] decides between them.
#[cfg(unix)]
#[test]
fn only_an_absent_broker_binary_reads_as_absence() {
    use std::os::unix::fs::PermissionsExt as _;

    let missing = spawn_broker(
        "mu-irc-gateway-no-such-nats-binary",
        4222,
        "mu-irc-live-ours",
    )
    .expect_err("a binary that is not on this host cannot be spawned");
    assert_eq!(
        missing.kind(),
        std::io::ErrorKind::NotFound,
        "an absent binary must be recognisable as absent — that is the skip path: {missing}"
    );

    // Present, and not runnable: the case that used to take the skip path too.
    let unrunnable =
        std::env::temp_dir().join(format!("mu-irc-live-not-executable-{}", std::process::id()));
    std::fs::write(&unrunnable, b"#!/bin/sh\nexit 0\n").expect("writing the stand-in binary");
    std::fs::set_permissions(&unrunnable, std::fs::Permissions::from_mode(0o600))
        .expect("dropping the execute bit on the stand-in binary");
    let denied = spawn_broker(
        unrunnable.to_str().expect("a UTF-8 temporary path"),
        4222,
        "mu-irc-live-ours",
    )
    .expect_err("a file without the execute bit cannot be spawned");
    let _ = std::fs::remove_file(&unrunnable);
    assert_ne!(
        denied.kind(),
        std::io::ErrorKind::NotFound,
        "a broker binary that exists and will not run was reported as absent, which is the \
         skip path: an operator who set NATS_BIN would get a green run that proved nothing \
         ({denied})"
    );
}

/// ASCII-case channel comparison, enough for the lobby name this test picks.
fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// The nick out of a message's prefix.
fn nick_of(msg: &IrcMessage) -> String {
    msg.prefix
        .as_deref()
        .unwrap_or_default()
        .split(['!', '@'])
        .next()
        .unwrap_or_default()
        .to_string()
}
