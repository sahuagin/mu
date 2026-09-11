//! The real network transport behind [`crate::adapter::Transport`]: one TCP
//! connection, TLS by default, split into a line reader and a line writer.
//!
//! **Why a plain line client and not an IRC crate.** The plan of record
//! originally assigned a maintained IRC client crate to the integration
//! increment; it was amended instead of quietly contradicted. The decision,
//! its rationale, what it costs and what would reopen it are recorded in
//! `specs/plans/mu-irc-gateway-v0.md`, increment 2b, *Amendment, 2026-09-10 —
//! the integration increment ships a plain TLS line transport*. In short: the
//! adapter already owns registration, and what was missing is a socket, TLS,
//! CRLF framing and reconnection. That is this module, over `tokio` +
//! `tokio-rustls`.
//!
//! **A connection is a unit.** Both halves share one [`Lifecycle`]: whichever
//! fails first ends the other and puts exactly one [`FromServer::Closed`] on
//! the inbound stream. A consumer watches that one stream, so a writer that
//! died silently would leave it parked on a socket nobody drives.
//!
//! **Ending is never the consumer's to block.** The `Closed` slot is RESERVED
//! when the inbound channel is built, so a consumer that has stopped draining
//! delays LINES and nothing else: [`Lifecycle::close`] is synchronous, the
//! teardown it reports has already happened, and no half of the connection can
//! be parked by backpressure on its way out.
//!
//! **Nothing is buffered across a connection.** The writer's queue and the
//! reader's task are created per connection and die with it, so a reconnect
//! starts with an empty queue: the gateway is a live mirror, never a replay
//! buffer. The queue is bounded for the same reason — a server that stops
//! reading makes [`LineWriter::send_line`] report [`SendError::Overflow`] and
//! the line is dropped, rather than growing memory until the process dies.
//!
//! **Secret-safe.** No line content is ever logged here, at any level: the
//! outbound stream carries the SASL `AUTHENTICATE` response, which is a
//! reversibly-encoded password. Diagnostics name counts, lengths and error
//! classes only.

use std::fmt;
use std::io;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch, OnceCell};
use tokio::task::JoinHandle;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

use crate::adapter::Transport;

/// The default port when `server` names no port: 6697 with TLS, 6667 without.
/// Config documents `host:port`; a bare host is accepted rather than refused,
/// because getting the conventional port wrong is not a failure mode worth a
/// startup error.
const DEFAULT_TLS_PORT: u16 = 6697;
const DEFAULT_PLAIN_PORT: u16 = 6667;

/// The most bytes one inbound line may occupy before the connection is treated
/// as broken.
///
/// IRCv3 allows 8191 bytes of tags plus the 512-byte message, so this is roughly
/// double the largest legal line. Without a cap, a server (or anything holding
/// the socket) that never sends `\n` grows the read buffer without limit.
const MAX_LINE_BYTES: usize = 16 * 1024;

/// How many outbound lines may be queued for the writer task.
///
/// A live mirror, not a queue: the depth exists to absorb a burst (a NAMES-driven
/// reconcile emitting a dozen JOINs, a long DM framed into continuations), not to
/// hold traffic for a server that has stopped reading. Past it, lines are
/// refused and dropped.
const OUTBOUND_QUEUE: usize = 512;

/// How long the writer waits for a graceful socket shutdown before giving up.
///
/// The shutdown is a courtesy: it lets a QUIT reach the peer instead of being
/// truncated by an abrupt drop. It also runs on a socket that may be exactly as
/// stalled as the write it was just cancelled off, so it is BOUNDED — the
/// writer task has to end, or the only thing that ever reclaims it is the
/// guard's abort, which is the defect this bound exists to rule out.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Why a connection could not be established. Body-free and secret-free: it
/// names the endpoint and a failure class, never a credential.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("`{0}` is not a valid `host[:port]`")]
    BadServer(String),
    #[error("connecting to {0} timed out after {1:?}")]
    Timeout(String, Duration),
    #[error("connecting to {0}: {1}")]
    Connect(String, io::Error),
    #[error("TLS handshake with {0}: {1}")]
    Tls(String, io::Error),
    #[error("`{0}` is not a valid TLS server name")]
    BadServerName(String),
    /// No system trust anchors could be loaded, so no server certificate could
    /// ever be validated. Refused rather than silently trusting anything.
    #[error("no system trust anchors are available for TLS ({0})")]
    NoTrustAnchors(String),
    #[error("TLS client configuration: {0}")]
    TlsConfig(String),
}

/// Why an outbound line was not handed to the socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    /// The connection is gone (the writer task ended).
    #[error("the IRC connection is closed")]
    Disconnected,
    /// The outbound queue is full: the server is not reading fast enough. The
    /// line is dropped — best-effort mirroring, never a stored queue.
    #[error("the outbound queue is full; the line was dropped")]
    Overflow,
}

/// What the reader task reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FromServer {
    /// One protocol line, CRLF stripped.
    Line(String),
    /// The connection ended, with a body-free reason for the log. Exactly
    /// one per connection, whichever half failed first — see [`Lifecycle`].
    Closed(String),
}

/// The write half: an [`adapter::Transport`](crate::adapter::Transport) that
/// hands lines to the connection's writer task.
///
/// `send_line` is synchronous because the adapter's trait is — the state
/// machines decide lines inside ordinary functions. The queue is what bridges
/// that to an async socket, and it is bounded, so this never blocks and never
/// grows without limit.
pub struct LineWriter {
    tx: mpsc::Sender<String>,
    /// The connection's shared lifetime, held WEAKLY.
    ///
    /// Weak and not strong on purpose: a strong reference would keep the
    /// inbound sender alive for as long as a consumer held the writer, and the
    /// inbound stream would never end after its `Closed`.
    life: Weak<Lifecycle>,
}

impl LineWriter {
    /// Whether this connection is still live. `false` means it is finished —
    /// for EITHER half's failure, because the writer task drops the queue as
    /// soon as the shared [`Lifecycle`] ends, before it waits on a broken
    /// socket to finish tearing down. A [`FromServer::Closed`] is on the
    /// inbound stream by then.
    pub fn is_connected(&self) -> bool {
        !self.tx.is_closed()
    }
}

impl Drop for LineWriter {
    /// Dropping the last writer is the local side closing the connection, and
    /// it has to END it rather than merely stop feeding it.
    ///
    /// The queue alone cannot say so: `out_rx.recv()` is polled only BETWEEN
    /// writes, so a writer parked inside `write_framed` on a peer that keeps
    /// the socket open and stops reading would never reach the poll that
    /// notices its senders are gone. Ending the lifecycle here is what cancels
    /// that write.
    fn drop(&mut self) {
        if let Some(life) = self.life.upgrade() {
            life.close("local writer dropped".to_string());
        }
    }
}

impl fmt::Debug for LineWriter {
    /// Deliberately says nothing about what is queued: the queue carries the
    /// SASL response.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LineWriter")
            .field("connected", &self.is_connected())
            .finish()
    }
}

impl Transport for LineWriter {
    type Error = SendError;

    fn send_line(&mut self, line: &str) -> Result<(), Self::Error> {
        match self.tx.try_send(line.to_string()) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(SendError::Overflow),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(SendError::Disconnected),
        }
    }
}

/// One live IRC connection: a bounded outbound queue, and an inbound stream of
/// [`FromServer`].
///
/// The three parts are separate fields, not accessors, so a `select!` loop can
/// hold the inbound stream and the writer at once. Dropping the whole thing (or
/// just the [`ConnectionGuard`]) aborts both tasks and closes the socket — that
/// is the whole of the gateway's connection-aware dropping on the IRC side, and
/// there is no state to carry across because everything a connection buffered
/// dies with it.
///
/// Watching `inbound` is sufficient: a read failure, a write failure and a
/// dropped [`LineWriter`] all arrive there as one [`FromServer::Closed`].
pub struct Connection {
    /// The write half, handed to the adapter and the bridge.
    pub writer: LineWriter,
    /// Inbound lines, in order.
    pub inbound: mpsc::Receiver<FromServer>,
    /// Keeps the reader and writer tasks alive; aborts them when dropped. Held
    /// separately so the other two can be moved out independently.
    pub guard: ConnectionGuard,
}

/// The lifetime of one connection's tasks. Dropping it ends the connection.
pub struct ConnectionGuard {
    reader_task: JoinHandle<()>,
    writer_task: JoinHandle<()>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.reader_task.abort();
        self.writer_task.abort();
    }
}

/// The shared lifetime of one connection's two tasks.
///
/// A connection is a UNIT. The consumer watches ONE stream — `inbound` — and a
/// write failure is as terminal as a read failure, so a writer that died
/// without saying so there would leave the consumer parked on a socket nobody
/// is driving. The concrete case is a TLS-layer write failure over a TCP
/// connection the peer never closes: the writer errors, the reader stays
/// blocked in `fill_buf` on a still-open socket, and nothing ever arrives.
///
/// So whichever half ends first ends the other and reports the close. That
/// holds for EVERY exit, including the ones with nobody to report to: a reader
/// whose consumer dropped the inbound stream still ends the lifecycle, and a
/// writer parked mid-`write_all` is cancelled by it rather than waiting for a
/// stalled socket to answer. The report is once-only: a failure that takes down
/// both halves still puts exactly one [`FromServer::Closed`] on the stream.
///
/// And ending is never blocked by the consumer it reports to. `Closed` travels
/// through a slot reserved when the channel was built, so a consumer that is
/// alive but not draining `inbound` cannot park the half that is ending the
/// connection.
struct Lifecycle {
    inbound: mpsc::Sender<FromServer>,
    /// The capacity reserved for the one `Closed`, taken by whichever half
    /// ends the connection first. Data can fill the queue; it cannot take
    /// this.
    closed_slot: Mutex<Option<mpsc::OwnedPermit<FromServer>>>,
    /// `true` once some half has reported the close. The transition is both
    /// the once-only gate on `Closed` and the wake-up for the other half.
    ended: watch::Sender<bool>,
}

impl Lifecycle {
    fn new(
        inbound: mpsc::Sender<FromServer>,
        closed_slot: Option<mpsc::OwnedPermit<FromServer>>,
    ) -> Self {
        Self {
            inbound,
            closed_slot: Mutex::new(closed_slot),
            ended: watch::channel(false).0,
        }
    }

    /// Hand one line to the consumer. `false` means the consumer is gone, and
    /// there is nothing left to read for.
    async fn deliver(&self, line: String) -> bool {
        self.inbound.send(FromServer::Line(line)).await.is_ok()
    }

    /// Resolves when the consumer has dropped the inbound stream.
    ///
    /// Watched alongside the socket read, because a consumer can go away while
    /// the peer is silent: a delivery that fails reports it only when the next
    /// line arrives, and a silent-but-open peer never sends one.
    async fn consumer_gone(&self) {
        self.inbound.closed().await;
    }

    /// End the connection with a body-free reason. Idempotent: only the FIRST
    /// caller's reason reaches the inbound stream. Waking the other half first
    /// is deliberate — it stops reading or writing before the `Closed` a
    /// consumer may act on immediately.
    ///
    /// Synchronous and non-blocking ON PURPOSE. `Closed` goes through the slot
    /// reserved when the channel was built, so a consumer that is alive but not
    /// draining cannot park a half inside `close` — the case where the writer
    /// would sit here still holding `out_rx`, with `is_connected` going on
    /// saying yes and lines going on queueing. Being synchronous is also what
    /// lets [`LineWriter`]'s drop end the lifecycle.
    fn close(&self, reason: String) {
        let first = self
            .ended
            .send_if_modified(|ended| !std::mem::replace(ended, true));
        if !first {
            return;
        }
        let slot = match self.closed_slot.lock() {
            Ok(mut slot) => slot.take(),
            // A panic elsewhere poisoned the lock; the reservation inside is
            // still exactly what it was.
            Err(poisoned) => poisoned.into_inner().take(),
        };
        match slot {
            Some(permit) => {
                let _ = permit.send(FromServer::Closed(reason));
            }
            // Not reachable in practice: the slot is reserved on a channel that
            // has just been created, and only the first caller takes it. A
            // non-blocking send keeps `close` total rather than panicking.
            None => {
                let _ = self.inbound.try_send(FromServer::Closed(reason));
            }
        }
    }

    /// Resolves once the connection has ended — immediately if it already has,
    /// so a half that checks late cannot miss the transition.
    async fn ended(&self) {
        let mut rx = self.ended.subscribe();
        loop {
            let already = *rx.borrow_and_update();
            if already {
                return;
            }
            if rx.changed().await.is_err() {
                // `self` owns the sender, so this cannot happen while this
                // future is alive; treating it as "ended" is the safe read.
                return;
            }
        }
    }
}

/// Anything the connection can be carried over. The blanket impl means the
/// plaintext and TLS cases differ only in how the stream is built.
trait Duplex: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Duplex for T {}

/// Connect to `server` (`host[:port]`), optionally over TLS, within `timeout`.
///
/// `timeout` is ONE budget for the whole establishment — the TCP connect, the
/// first-connection trust-store load and the TLS handshake share it — because a
/// caller's deadline is for getting a usable connection, not for a stage it
/// cannot see. A per-stage budget would let the documented bound be exceeded by
/// however many stages there happen to be.
///
/// TLS verifies the server certificate against the system trust store; there is
/// no "insecure" switch, because the SASL gate in the adapter refuses to send a
/// password over anything but TLS and a transport that could be told to skip
/// verification would make that gate decorative.
pub async fn connect(
    server: &str,
    tls: bool,
    timeout: Duration,
) -> Result<Connection, ConnectError> {
    let (host, port) = split_server(server, tls)?;
    let endpoint = format!("{host}:{port}");
    let dial = || TcpStream::connect((host.clone(), port));
    connect_with(dial, &host, endpoint, tls, timeout).await
}

/// [`connect`] with the TCP stage as a parameter.
///
/// The seam exists because the shared deadline is only observable when an
/// EARLIER stage has already spent part of it: with a real loopback peer the
/// connect is instant, and a per-stage budget looks identical to one budget. A
/// test that can make the dial cost time is what tells the two apart.
async fn connect_with<C, F>(
    dial: C,
    host: &str,
    endpoint: String,
    tls: bool,
    timeout: Duration,
) -> Result<Connection, ConnectError>
where
    C: FnOnce() -> F,
    F: std::future::Future<Output = io::Result<TcpStream>>,
{
    let established = tokio::time::timeout(timeout, establish(dial, host, &endpoint, tls)).await;
    match established {
        Ok(connected) => connected,
        Err(_) => Err(ConnectError::Timeout(endpoint, timeout)),
    }
}

/// The establishment itself, with no deadline of its own: [`connect_with`] owns
/// the budget, so every stage in here shares one.
async fn establish<C, F>(
    dial: C,
    host: &str,
    endpoint: &str,
    tls: bool,
) -> Result<Connection, ConnectError>
where
    C: FnOnce() -> F,
    F: std::future::Future<Output = io::Result<TcpStream>>,
{
    let tcp = dial()
        .await
        .map_err(|e| ConnectError::Connect(endpoint.to_string(), e))?;
    // Protocol lines are small and latency matters more than packet count.
    let _ = tcp.set_nodelay(true);

    let stream: Box<dyn Duplex> = if tls {
        let config = tls_config().await?;
        let name = ServerName::try_from(host.to_string())
            .map_err(|_| ConnectError::BadServerName(host.to_string()))?
            .to_owned();
        let connector = TlsConnector::from(config);
        let tls_stream = connector
            .connect(name, tcp)
            .await
            .map_err(|e| ConnectError::Tls(endpoint.to_string(), e))?;
        Box::new(tls_stream)
    } else {
        Box::new(tcp)
    };

    Ok(spawn_connection(stream))
}

/// Wire an already-connected duplex stream into a [`Connection`]. Split out so
/// the framing half is exercised over an in-process socket pair without a
/// server.
fn spawn_connection(stream: Box<dyn Duplex>) -> Connection {
    let (read_half, mut write_half) = tokio::io::split(stream);
    // One slot PAST the queue depth, reserved immediately for the single
    // `Closed`: data fills the other 512, and the connection can still say it
    // has ended without waiting for a consumer to drain them.
    let (in_tx, inbound) = mpsc::channel::<FromServer>(OUTBOUND_QUEUE + 1);
    let closed_slot = in_tx.clone().try_reserve_owned().ok();
    let (out_tx, mut out_rx) = mpsc::channel::<String>(OUTBOUND_QUEUE);
    let life = Arc::new(Lifecycle::new(in_tx, closed_slot));
    let writer_handle = Arc::downgrade(&life);

    let reader_life = life.clone();
    let reader_task = tokio::spawn(async move {
        let mut reader = BufReader::new(read_half);
        let mut partial = Vec::new();
        let reason = loop {
            // `next_line` is dropped part-way through a line when the other
            // half ends the connection. That is safe HERE and only here: the
            // branch that does it never reads again, and a half-assembled line
            // has nobody left to want it.
            let read = tokio::select! {
                biased;
                () = reader_life.ended() => break None,
                // A consumer can go away while the peer says nothing at all.
                // Watching the inbound sender directly is what makes that case
                // terminal; a failed delivery only reports it when the next
                // server line arrives, which for a silent-but-open peer is
                // never.
                () = reader_life.consumer_gone() => {
                    break Some("inbound consumer dropped".to_string())
                }
                read = next_line(&mut reader, &mut partial) => read,
            };
            match read {
                Ok(Some(line)) => {
                    // The delivery is inside the cancellation too: a full queue
                    // parks it for as long as the consumer likes, and a reader
                    // parked there could not otherwise see the writer end the
                    // connection. Dropping a half-delivered line is safe for
                    // the same reason dropping a half-read one is — that branch
                    // never reads again.
                    let delivered = tokio::select! {
                        biased;
                        () = reader_life.ended() => break None,
                        delivered = reader_life.deliver(line) => delivered,
                    };
                    if !delivered {
                        // The consumer dropped the inbound stream, so there is
                        // nobody left to read FOR. That ends the CONNECTION,
                        // not just this half: a writer left waiting on `out_rx`
                        // would keep feeding a socket with no reader behind it.
                        // The `Closed` this queues has no receiver — that is
                        // what "consumer gone" means — but ending the shared
                        // lifecycle is the part the writer needs.
                        break Some("inbound consumer dropped".to_string());
                    }
                }
                Ok(None) => break Some("server closed".to_string()),
                // The error CLASS, never a line: an unterminated read may hold
                // half a credential exchange.
                Err(e) => break Some(format!("read failed: {e}")),
            }
        };
        if let Some(reason) = reason {
            reader_life.close(reason);
        }
    });

    let writer_life = life;
    let writer_task = tokio::spawn(async move {
        let reason = loop {
            let queued = tokio::select! {
                biased;
                () = writer_life.ended() => break None,
                queued = out_rx.recv() => queued,
            };
            let Some(line) = queued else {
                // Every `LineWriter` is gone: the local side is finished with
                // this connection, which ends it as surely as a socket error
                // would — nothing can answer a PING any more. The writer's own
                // drop normally ends the lifecycle first (it has to, to cancel
                // a stalled write); an empty closed queue is the same fact
                // arriving by the slower road.
                break Some("local writer dropped".to_string());
            };
            // The WRITE is inside the cancellation too, not just the wait for
            // something to write. A peer that stops reading while keeping the
            // socket open parks `write_all`/`flush` for as long as it likes;
            // with only the `recv` covered, a writer in that state never
            // observes the reader ending the connection, holds `out_rx`, and
            // `is_connected` keeps saying yes while lines queue into a socket
            // nobody drains.
            //
            // Dropping `write_framed` part-way truncates that one line. That is
            // safe HERE and only here: this branch never writes again, the
            // connection is already over, and a peer that had stopped reading
            // was never going to see the rest of it.
            let wrote = tokio::select! {
                biased;
                () = writer_life.ended() => break None,
                wrote = write_framed(&mut write_half, &line) => wrote,
            };
            if let Err(e) = wrote {
                // Body-free for the same reason the reader's is.
                break Some(format!("write failed: {e}"));
            }
        };
        // TEARDOWN FIRST, then the report. `LineWriter::is_connected` reads
        // exactly this queue, so dropping it before anything else means the
        // connection is already gone by the time a consumer is told so —
        // never the other way round, where a consumer acting on `Closed` could
        // still be told the connection is live.
        drop(out_rx);
        if let Some(reason) = reason {
            writer_life.close(reason);
        }
        // Best-effort, and bounded: tell the peer we are done so a QUIT is not
        // truncated by an abrupt socket drop, but never park here forever on
        // the same stalled socket a write was just cancelled off — see
        // [`SHUTDOWN_GRACE`].
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, write_half.shutdown()).await;
    });

    Connection {
        writer: LineWriter {
            tx: out_tx,
            life: writer_handle,
        },
        inbound,
        guard: ConnectionGuard {
            reader_task,
            writer_task,
        },
    }
}

/// Frame one line onto the socket: the content, CRLF, then a flush.
///
/// CRLF is added here, per the [`Transport`] contract — the state machines
/// produce a line, the transport frames it. One function so a partial write of
/// the terminator is a failure of the same line, not a silently split frame.
async fn write_framed<W: AsyncWrite + Unpin>(w: &mut W, line: &str) -> io::Result<()> {
    w.write_all(line.as_bytes()).await?;
    w.write_all(b"\r\n").await?;
    w.flush().await
}

/// Read one CRLF-terminated line, capped at [`MAX_LINE_BYTES`].
///
/// Lossy UTF-8 on purpose: IRC is a byte protocol and a server (or another
/// client) may put Latin-1 in a body. Replacing the invalid bytes keeps the
/// connection usable; refusing the line would let one badly-encoded message from
/// a stranger disconnect the gateway.
async fn next_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    partial: &mut Vec<u8>,
) -> io::Result<Option<String>> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            // EOF. A trailing unterminated fragment is not a line.
            partial.clear();
            return Ok(None);
        }
        match available.iter().position(|b| *b == b'\n') {
            Some(idx) => {
                if partial.len() + idx > MAX_LINE_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "protocol line exceeds the maximum length",
                    ));
                }
                partial.extend_from_slice(&available[..idx]);
                reader.consume(idx + 1);
                while partial.last() == Some(&b'\r') {
                    partial.pop();
                }
                let line = String::from_utf8_lossy(partial).into_owned();
                partial.clear();
                return Ok(Some(line));
            }
            None => {
                let n = available.len();
                if partial.len() + n > MAX_LINE_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "protocol line exceeds the maximum length",
                    ));
                }
                partial.extend_from_slice(available);
                reader.consume(n);
            }
        }
    }
}

/// The process-wide TLS client configuration, built at most once.
///
/// Reading the system trust store is blocking file I/O and is not cheap, and a
/// reconnect loop would otherwise pay it on every attempt, on the runtime, with
/// no deadline of its own. A failed load is NOT cached: a trust store that was
/// unreadable once (a store still being written at boot, say) is retried by the
/// next connection rather than poisoning every one of them.
static TLS_CONFIG: OnceCell<Arc<ClientConfig>> = OnceCell::const_new();

/// The shared TLS client configuration, loading it on first use.
///
/// The one-time load runs on a blocking thread, so it never stalls the runtime,
/// and it happens inside [`connect`]'s budget, so it cannot make a connection
/// overrun the deadline its caller asked for.
async fn tls_config() -> Result<Arc<ClientConfig>, ConnectError> {
    TLS_CONFIG
        .get_or_try_init(|| async {
            tokio::task::spawn_blocking(build_tls_config)
                .await
                .map_err(|e| ConnectError::TlsConfig(format!("trust store load failed: {e}")))?
                .map(Arc::new)
        })
        .await
        .cloned()
}

/// Build the TLS client configuration: system trust anchors, safe defaults, no
/// client certificate.
///
/// The crypto provider is named explicitly rather than left to the process
/// default, so which implementation performs the handshake is a property of this
/// file rather than of whichever crate in the build happened to install a
/// default first.
fn build_tls_config() -> Result<ClientConfig, ConnectError> {
    let native = rustls_native_certs::load_native_certs();
    let mut roots = RootCertStore::empty();
    for cert in native.certs {
        // A single unparseable certificate in the system store is not a reason
        // to refuse every connection; an EMPTY store is, and is checked next.
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        let why = if native.errors.is_empty() {
            "the system trust store is empty".to_string()
        } else {
            native
                .errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        };
        return Err(ConnectError::NoTrustAnchors(why));
    }
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| ConnectError::TlsConfig(e.to_string()))
        .map(|b| b.with_root_certificates(roots).with_no_client_auth())
}

/// Split `host[:port]`, defaulting the port by scheme. Accepts a bracketed IPv6
/// literal (`[::1]:6697`) and a bare one (`::1`).
fn split_server(server: &str, tls: bool) -> Result<(String, u16), ConnectError> {
    let default = if tls {
        DEFAULT_TLS_PORT
    } else {
        DEFAULT_PLAIN_PORT
    };
    let bad = || ConnectError::BadServer(server.to_string());
    let server = server.trim();
    if server.is_empty() {
        return Err(bad());
    }
    if let Some(rest) = server.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').ok_or_else(bad)?;
        if host.is_empty() {
            return Err(bad());
        }
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| bad())?,
            None if tail.is_empty() => default,
            None => return Err(bad()),
        };
        return Ok((host.to_string(), port));
    }
    match server.rsplit_once(':') {
        // A single colon separates host and port. More than one means a bare
        // IPv6 literal, which carries no port at all.
        Some((host, port)) if !host.is_empty() && !host.contains(':') => {
            Ok((host.to_string(), port.parse().map_err(|_| bad())?))
        }
        _ => Ok((server.to_string(), default)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn a_server_without_a_port_takes_the_conventional_one_for_its_scheme() {
        assert_eq!(
            split_server("irc.example.org", true).unwrap(),
            ("irc.example.org".to_string(), DEFAULT_TLS_PORT)
        );
        assert_eq!(
            split_server("irc.example.org", false).unwrap(),
            ("irc.example.org".to_string(), DEFAULT_PLAIN_PORT)
        );
    }

    #[test]
    fn an_explicit_port_wins_and_ipv6_literals_parse_either_way() {
        assert_eq!(
            split_server("irc.example.org:6667", true).unwrap(),
            ("irc.example.org".to_string(), 6667)
        );
        assert_eq!(
            split_server("[::1]:6697", false).unwrap(),
            ("::1".to_string(), 6697)
        );
        // A bare v6 literal has no port to find: every colon belongs to the host.
        assert_eq!(
            split_server("::1", true).unwrap(),
            ("::1".to_string(), DEFAULT_TLS_PORT)
        );
    }

    #[tokio::test]
    async fn the_framing_half_adds_crlf_outbound_and_strips_it_inbound() {
        use tokio::io::AsyncReadExt as _;

        let (client, mut server) = tokio::io::duplex(4096);
        let mut conn = spawn_connection(Box::new(client));

        conn.writer.send_line("CAP LS 302").unwrap();
        let mut got = [0u8; 12];
        server.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"CAP LS 302\r\n");

        // Two lines in one write, and a bare-LF line, all split correctly.
        server
            .write_all(b":srv PING x\r\n:srv 001 gw :hi\r\nNOTICE :bare\n")
            .await
            .unwrap();
        for expected in [":srv PING x", ":srv 001 gw :hi", "NOTICE :bare"] {
            assert_eq!(
                conn.inbound.recv().await,
                Some(FromServer::Line(expected.to_string()))
            );
        }

        // A closed socket is reported once, not treated as a line.
        drop(server);
        assert!(matches!(
            conn.inbound.recv().await,
            Some(FromServer::Closed(_))
        ));
    }

    /// A socket whose write side fails while its read side stays open and
    /// silent: a TLS-layer write failure over a TCP connection the peer never
    /// closes. Reading from it parks forever, which is exactly the case where
    /// a writer that only shut its own half would strand the consumer.
    struct WriteFailsWhileReadHangs;

    impl AsyncRead for WriteFailsWhileReadHangs {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Pending
        }
    }

    impl AsyncWrite for WriteFailsWhileReadHangs {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            std::task::Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, "peer reset")))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// Await `f`, failing the test rather than hanging it if the transport
    /// never answers — the defect under test is precisely a consumer that waits
    /// forever.
    async fn promptly<T>(what: &str, f: impl std::future::Future<Output = T>) -> T {
        match tokio::time::timeout(Duration::from_secs(5), f).await {
            Ok(v) => v,
            Err(_) => panic!("{what}: the transport never answered"),
        }
    }

    /// A write failure is a failure of the CONNECTION, not of the writer: the
    /// consumer watches only `inbound`, so the failure has to arrive there,
    /// the reader has to stop waiting on a socket nobody drives, and the
    /// connection has to report itself gone.
    #[tokio::test]
    async fn a_write_failure_ends_the_connection_as_a_unit() {
        let mut conn = spawn_connection(Box::new(WriteFailsWhileReadHangs));

        // The read side is open and silent, so nothing but the write failure
        // can ever end this connection.
        conn.writer.send_line("PING :probe").unwrap();

        let closed = promptly("a write failure", conn.inbound.recv()).await;
        assert!(
            matches!(closed, Some(FromServer::Closed(ref why)) if why.starts_with("write failed:")),
            "expected a body-free write failure, got {closed:?}"
        );

        // The teardown is already DONE by the time the report is readable: a
        // consumer acting on `Closed` the instant it arrives must not be told
        // the connection is still live.
        assert!(!conn.writer.is_connected());
        assert_eq!(
            conn.writer.send_line("PING :again"),
            Err(SendError::Disconnected)
        );

        // Exactly one `Closed`: the stream ends instead of reporting a second
        // one from the reader the writer just ended.
        assert_eq!(
            promptly("the stream to end", conn.inbound.recv()).await,
            None,
            "a connection reports exactly one Closed"
        );
    }

    /// The other direction of the same contract: the reader ending reports one
    /// `Closed` and takes the writer with it.
    #[tokio::test]
    async fn a_server_close_reports_closed_once_and_ends_the_writer() {
        let (client, server) = tokio::io::duplex(4096);
        let mut conn = spawn_connection(Box::new(client));
        assert!(conn.writer.is_connected());

        drop(server);

        let closed = promptly("a server close", conn.inbound.recv()).await;
        assert_eq!(
            closed,
            Some(FromServer::Closed("server closed".to_string()))
        );
        assert_eq!(
            promptly("the stream to end", conn.inbound.recv()).await,
            None,
            "a connection reports exactly one Closed"
        );
        assert!(
            !conn.writer.is_connected(),
            "the reader ending must end the connection, not just its own half"
        );
    }

    /// Dropping the write half is the local side closing the connection, and
    /// it ends the reader too — otherwise a bridge that tore down its writer
    /// would keep a reader task parked on a live socket forever.
    #[tokio::test]
    async fn dropping_the_writer_ends_the_reader_too() {
        let (client, _server) = tokio::io::duplex(4096);
        let Connection {
            writer,
            mut inbound,
            guard,
        } = spawn_connection(Box::new(client));

        drop(writer);

        let closed = promptly("a dropped writer", inbound.recv()).await;
        assert_eq!(
            closed,
            Some(FromServer::Closed("local writer dropped".to_string()))
        );
        assert_eq!(promptly("the stream to end", inbound.recv()).await, None);
        drop(guard);
    }

    /// A socket whose writes never complete, over a real read half: the peer
    /// stopped reading but left the connection open. `started` flips the moment
    /// the writer is parked inside `write_framed`, so the test can end the
    /// connection at exactly the moment the defect needs — not before.
    struct StalledWrite<R> {
        read: R,
        started: Arc<AtomicBool>,
    }

    impl<R: AsyncRead + Unpin> AsyncRead for StalledWrite<R> {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::pin::Pin::new(&mut self.read).poll_read(cx, buf)
        }
    }

    impl<R: Unpin> AsyncWrite for StalledWrite<R> {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            self.started.store(true, Ordering::SeqCst);
            std::task::Poll::Pending
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Pending
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Pending
        }
    }

    /// The cancellation has to reach the WRITE, not just the wait for a line to
    /// write. A writer parked in `write_all` on a stalled-but-open socket is
    /// precisely the case where "whichever half ends first ends the other"
    /// used to be false: the reader ended the connection and the writer kept
    /// `out_rx`, so `is_connected` went on reporting a live connection.
    ///
    /// Time is paused: the only real waiting here is the bounded shutdown
    /// grace, and the point of the test is that the task ENDS, not how long a
    /// courtesy shutdown takes.
    #[tokio::test(start_paused = true)]
    async fn a_stalled_write_is_cancelled_when_the_connection_ends() {
        let started = Arc::new(AtomicBool::new(false));
        let (client, server) = tokio::io::duplex(4096);
        let Connection {
            mut writer,
            mut inbound,
            mut guard,
        } = spawn_connection(Box::new(StalledWrite {
            read: client,
            started: started.clone(),
        }));

        writer.send_line("PING :probe").unwrap();
        // Hand the writer the line and let it park inside `write_framed`.
        for _ in 0..100 {
            if started.load(Ordering::SeqCst) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            started.load(Ordering::SeqCst),
            "the writer must be parked inside the write for this to test anything"
        );

        // The peer closes its sending direction: the READER ends the
        // connection while the writer is mid-write.
        drop(server);

        let closed = promptly("a server close", inbound.recv()).await;
        assert_eq!(
            closed,
            Some(FromServer::Closed("server closed".to_string()))
        );

        // The writer has to finish, not linger on a socket that never completes
        // a write. Its last wait is the bounded shutdown grace, so the window
        // here is wider than that.
        tokio::time::timeout(SHUTDOWN_GRACE * 4, &mut guard.writer_task)
            .await
            .expect("a stalled writer must not outlive the connection")
            .expect("the writer task must not panic");
        assert!(
            !writer.is_connected(),
            "a cancelled write must drop the queue, not keep reporting a live connection"
        );
        assert_eq!(
            promptly("the stream to end", inbound.recv()).await,
            None,
            "a connection reports exactly one Closed"
        );
    }

    /// A reader whose consumer is gone has ended, and a connection is a unit:
    /// the writer must not be left feeding a socket with nothing behind it.
    /// There is no `Closed` to deliver — that is what "consumer gone" means —
    /// but the shared lifecycle still has to end.
    #[tokio::test]
    async fn a_dropped_inbound_consumer_ends_the_writer_too() {
        let (client, mut server) = tokio::io::duplex(4096);
        let Connection {
            writer,
            inbound,
            mut guard,
        } = spawn_connection(Box::new(client));
        assert!(writer.is_connected());

        // Nobody is listening for inbound lines any more.
        drop(inbound);
        server.write_all(b":srv PING x\r\n").await.unwrap();

        promptly("the writer task to end", &mut guard.writer_task)
            .await
            .expect("the writer task must not panic");
        assert!(
            !writer.is_connected(),
            "a connection with no consumer is finished, not half-alive"
        );
    }

    /// A peer that talks without pause and never reads a byte: the inbound
    /// queue fills while the write side fails. `lines` counts what the read
    /// half has produced so a test can wait for the queue to be genuinely full
    /// before failing the write.
    struct FloodsWhileWritesFail {
        lines: Arc<AtomicUsize>,
    }

    impl AsyncRead for FloodsWhileWritesFail {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            const LINE: &[u8] = b":srv PING x\r\n";
            let mut produced = 0;
            while buf.remaining() >= LINE.len() {
                buf.put_slice(LINE);
                produced += 1;
            }
            self.lines.fetch_add(produced, Ordering::SeqCst);
            std::task::Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for FloodsWhileWritesFail {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            std::task::Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, "peer reset")))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// Ending a connection is not the consumer's to authorise. A consumer that
    /// is ALIVE but has stopped draining `inbound` fills the queue; if the
    /// writer had to queue its `Closed` behind that data, a write failure would
    /// leave the writer parked, still holding `out_rx`, with `is_connected`
    /// going on saying yes and lines going on queueing into a dead connection.
    #[tokio::test]
    async fn a_consumer_that_stopped_draining_cannot_block_the_teardown() {
        let lines = Arc::new(AtomicUsize::new(0));
        let Connection {
            mut writer,
            mut inbound,
            mut guard,
        } = spawn_connection(Box::new(FloodsWhileWritesFail {
            lines: lines.clone(),
        }));

        // Nobody reads `inbound`: let the reader fill every DATA slot and park.
        while inbound.len() < OUTBOUND_QUEUE {
            tokio::task::yield_now().await;
        }
        assert!(
            lines.load(Ordering::SeqCst) > OUTBOUND_QUEUE,
            "the peer must have produced more than the queue holds"
        );

        // Now fail the write, with the queue exactly as full as it can be.
        writer.send_line("PING :probe").unwrap();

        promptly("the writer task to end", &mut guard.writer_task)
            .await
            .expect("the writer task must not panic");
        assert!(
            !writer.is_connected(),
            "a failed write must tear the connection down before reporting it, \
             not after a consumer gets round to draining"
        );
        assert_eq!(
            writer.send_line("PING :again"),
            Err(SendError::Disconnected)
        );

        // And the report still arrives, through the slot the data could not
        // take: exactly one `Closed`, after the lines that were already queued.
        let mut delivered = 0;
        let closed = loop {
            match promptly("the inbound stream", inbound.recv()).await {
                Some(FromServer::Line(_)) => delivered += 1,
                Some(FromServer::Closed(why)) => break why,
                None => panic!("a full queue must not swallow the Closed"),
            }
        };
        assert!(closed.starts_with("write failed:"), "got {closed:?}");
        assert_eq!(delivered, OUTBOUND_QUEUE, "the queued lines are not lost");
        assert_eq!(
            promptly("the stream to end", inbound.recv()).await,
            None,
            "a connection reports exactly one Closed"
        );
    }

    /// A consumer can go away while the peer says nothing at all. Detecting it
    /// only when the next server line fails to deliver leaves a silent-but-open
    /// connection running forever with nobody behind it.
    #[tokio::test]
    async fn a_dropped_inbound_consumer_ends_a_silent_connection() {
        // The peer stays open and never speaks, so nothing but the dropped
        // consumer can end this.
        let (client, _server) = tokio::io::duplex(4096);
        let Connection {
            writer,
            inbound,
            mut guard,
        } = spawn_connection(Box::new(client));
        assert!(writer.is_connected());

        drop(inbound);

        promptly("the reader task to end", &mut guard.reader_task)
            .await
            .expect("the reader task must not panic");
        promptly("the writer task to end", &mut guard.writer_task)
            .await
            .expect("the writer task must not panic");
        assert!(
            !writer.is_connected(),
            "a connection with no consumer is finished, not half-alive"
        );
    }

    /// The mirror of `a_stalled_write_is_cancelled_when_the_connection_ends`,
    /// for the LOCAL close: dropping the writer while a write is parked on a
    /// peer that stopped reading. The queue cannot report that — `out_rx` is
    /// only polled between writes — so the drop has to end the lifecycle
    /// itself.
    ///
    /// Time is paused: the only real waiting is the bounded shutdown grace.
    #[tokio::test(start_paused = true)]
    async fn dropping_the_writer_cancels_a_write_parked_on_a_stalled_peer() {
        let started = Arc::new(AtomicBool::new(false));
        // A read half that stays open and silent: only the drop can end this.
        let (client, _server) = tokio::io::duplex(4096);
        let Connection {
            mut writer,
            mut inbound,
            mut guard,
        } = spawn_connection(Box::new(StalledWrite {
            read: client,
            started: started.clone(),
        }));

        writer.send_line("PING :probe").unwrap();
        for _ in 0..100 {
            if started.load(Ordering::SeqCst) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            started.load(Ordering::SeqCst),
            "the writer must be parked inside the write for this to test anything"
        );

        drop(writer);

        let closed = promptly("a dropped writer", inbound.recv()).await;
        assert_eq!(
            closed,
            Some(FromServer::Closed("local writer dropped".to_string()))
        );
        // Its last wait is the bounded shutdown grace, so the window is wider.
        tokio::time::timeout(SHUTDOWN_GRACE * 4, &mut guard.writer_task)
            .await
            .expect("a dropped writer must not leave its task parked in a write")
            .expect("the writer task must not panic");
        promptly("the reader task to end", &mut guard.reader_task)
            .await
            .expect("the reader task must not panic");
        assert_eq!(
            promptly("the stream to end", inbound.recv()).await,
            None,
            "a connection reports exactly one Closed"
        );
    }

    /// `connect`'s budget is for getting a usable connection, so every stage
    /// shares it. A peer that accepts instantly and then never answers the
    /// handshake is the case where a per-stage budget quietly doubles: TCP
    /// costs nothing and TLS gets a fresh one.
    ///
    /// Time is paused, so the deadline is exact rather than approximately
    /// whatever the host was doing.
    #[tokio::test(start_paused = true)]
    async fn one_budget_covers_the_whole_establishment_not_each_stage() {
        // Warm the shared trust store first: its one-time load is not the thing
        // under test. A host with no trust anchors cannot handshake at all —
        // there is no budget to measure there, so there is nothing to assert.
        if tls_config().await.is_err() {
            return;
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            // Accepted, and then silent: hold it open so the handshake waits
            // rather than failing.
            std::future::pending::<()>().await;
            drop(stream);
        });

        // Connect for real BEFORE the clock matters: a paused clock advances to
        // the next deadline instead of waiting on the I/O driver, so a socket
        // still completing its handshake inside the measured window would never
        // finish.
        let tcp = TcpStream::connect(addr).await.unwrap();

        let budget = Duration::from_secs(10);
        // The dial spends most of the budget, and the handshake that follows
        // never answers. A budget applied per stage would hand the handshake a
        // fresh ten seconds and take eighteen.
        let dial = move || async move {
            tokio::time::sleep(budget * 4 / 5).await;
            Ok(tcp)
        };

        let started = tokio::time::Instant::now();
        let outcome = connect_with(dial, "127.0.0.1", addr.to_string(), true, budget).await;
        let took = started.elapsed();
        let Err(failed) = outcome else {
            panic!("a peer that never answers the handshake cannot connect");
        };

        assert!(
            matches!(failed, ConnectError::Timeout(_, budget_reported) if budget_reported == budget),
            "expected the documented timeout, got {failed:?}"
        );
        assert!(
            took <= budget,
            "every stage shares one budget of {budget:?}; establishment took {took:?}"
        );
        peer.abort();
    }

    #[test]
    fn a_malformed_server_is_refused_rather_than_guessed() {
        for server in ["", "host:", "host:notaport", "[::1", "[]:6697"] {
            assert!(
                split_server(server, true).is_err(),
                "{server} should not parse"
            );
        }
    }
}
