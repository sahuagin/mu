//! The real network transport behind [`crate::adapter::Transport`]: one TCP
//! connection, TLS by default, split into a line reader and a line writer.
//!
//! **Why a plain line client and not an IRC crate.** The plan of record
//! originally called for a maintained IRC client crate here; it was amended
//! instead of quietly contradicted. The decision, its rationale, what it costs
//! and what would reopen it are recorded in `specs/plans/mu-irc-gateway-v0.md`
//! under *Amendment, 2026-09-10* (the plain TLS line transport). In short: the
//! adapter already owns registration, and what was missing is a socket, TLS,
//! CRLF framing and reconnection. That is this module, over `tokio` +
//! `tokio-rustls`.
//!
//! **A connection is a unit.** Both halves share one `Lifecycle`: whichever
//! fails first ends the other and puts exactly one [`FromServer::Closed`] on
//! the inbound stream. A consumer watches that one stream, so a writer that
//! died silently would leave it parked on a socket nobody drives.
//!
//! **Ending is never the consumer's to block.** The `Closed` slot is RESERVED
//! when the inbound channel is built, so a consumer that has stopped draining
//! delays LINES and nothing else: `Lifecycle::close` is synchronous, the
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
//! **Trust is per configuration, the system store is cached.** What a server
//! certificate is verified against is a [`TlsTrust`] a caller may pass in
//! ([`connect_with_trust`]; plain [`connect`] is the system store), so an
//! operator on a private LAN can add their own CA's PEM bundle (`[irc]
//! tls_ca_file`) on top of — or instead of — the system anchors. What stays
//! process-wide is the expensive part: the system store is read and parsed at
//! most once. There is no state anywhere in here that skips verification, and
//! server-name checking is untouched by any of it — connecting to an IP address
//! requires that address in the certificate's SAN.
//!
//! **Secret-safe.** No line content is ever logged here, at any level: the
//! outbound stream carries the SASL `AUTHENTICATE` response, which is a
//! reversibly-encoded password. Diagnostics name counts, lengths and error
//! classes only.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use rustls_pki_types::pem::PemObject as _;
use rustls_pki_types::CertificateDer;
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
    /// The configured trust store came out EMPTY, so no server certificate
    /// could ever be validated. Refused rather than silently trusting anything.
    #[error("no trust anchors are available for TLS ({0})")]
    NoTrustAnchors(String),
    #[error("TLS client configuration: {0}")]
    TlsConfig(String),
}

/// Why a `tls_ca_file` bundle cannot be used as a trust anchor.
///
/// Every variant but [`CaFault::Read`] is a FIXED phrase, and `Read` carries an
/// `io::Error` that describes a syscall. Nothing here can quote the file: the
/// PEM reader's own `Display` renders the offending line verbatim
/// (`IllegalSectionStart { line }`), and a bundle that an operator got wrong is
/// as likely to be a private key as a certificate. A diagnostic from this module
/// is safe to log, which only holds if it never carries file content.
#[derive(Debug, thiserror::Error)]
pub enum CaFault {
    #[error("it cannot be read: {0}")]
    Read(io::Error),
    #[error("it is not a readable PEM bundle (a section is truncated or its body is not base64)")]
    NotPem,
    #[error("it holds no CERTIFICATE section")]
    Empty,
    #[error("it holds a CERTIFICATE section that is not a usable trust anchor")]
    Unusable,
}

/// What one connection verifies the server certificate against.
///
/// [`TlsTrust::system()`] — also the [`Default`] — is the system trust store and
/// nothing else, which is every public IRC network. An operator running IRC on
/// a private LAN behind a CA of their own adds that CA's PEM bundle with
/// [`TlsTrust::with_ca_file`], which LAYERS the bundle on top of the system
/// anchors; passing `system_roots = false` narrows the store to the bundle
/// alone, for a network that should trust nothing else.
///
/// There is deliberately NO "skip verification" state. The adapter refuses to
/// send a SASL password over anything but TLS, and a transport that could be
/// told to accept any certificate would make that gate decorative — a private CA
/// is the supported way to run a self-signed server, precisely because it still
/// verifies.
///
/// Server-name verification is unaffected and stays exact: the anchors decide
/// WHO may issue the certificate, never WHICH name it is for. Connecting to a
/// bare IP means rustls builds a [`ServerName::IpAddress`], and the certificate
/// has to carry that address in a SAN — a CN, or a DNS SAN naming the host, does
/// not satisfy it.
#[derive(Clone)]
pub struct TlsTrust {
    /// Whether the system trust store is part of the store. Default `true`.
    system_roots: bool,
    /// Where the extra anchors came from, for diagnostics only.
    ca_file: Option<PathBuf>,
    /// Extra anchors, each already proven addable to a [`RootCertStore`] by
    /// [`TlsTrust::with_ca_file`] — so a connect-time failure to add one is
    /// impossible rather than merely unlikely.
    extra: Vec<CertificateDer<'static>>,
}

impl TlsTrust {
    /// The system trust store and nothing else.
    pub fn system() -> Self {
        Self {
            system_roots: true,
            ca_file: None,
            extra: Vec::new(),
        }
    }

    /// Add every certificate in the PEM bundle at `path`, keeping the system
    /// anchors when `system_roots`.
    ///
    /// The bundle is read and PARSED here rather than at connect time, so a
    /// path that does not exist, a file that is not PEM, a bundle with no
    /// certificate in it, and a certificate that is not a usable anchor are all
    /// configuration errors the operator hears about at startup — the same
    /// moment `--check-config` would tell them — instead of a TLS failure on
    /// some later reconnect.
    pub fn with_ca_file(path: &Path, system_roots: bool) -> Result<Self, CaFault> {
        let mut extra = Vec::new();
        // A scratch store, used only to prove each anchor is usable. The real
        // one is built per connection on top of the cached native roots.
        let mut probe = RootCertStore::empty();
        for item in CertificateDer::pem_file_iter(path).map_err(pem_fault)? {
            let cert = item.map_err(pem_fault)?;
            probe.add(cert.clone()).map_err(|_| CaFault::Unusable)?;
            extra.push(cert);
        }
        if extra.is_empty() {
            return Err(CaFault::Empty);
        }
        Ok(Self {
            system_roots,
            ca_file: Some(path.to_path_buf()),
            extra,
        })
    }

    /// Whether the system trust store is part of this store.
    pub fn system_roots(&self) -> bool {
        self.system_roots
    }

    /// The bundle these extra anchors were read from, if any.
    pub fn ca_file(&self) -> Option<&Path> {
        self.ca_file.as_deref()
    }

    /// How many anchors the bundle contributed.
    pub fn extra_anchors(&self) -> usize {
        self.extra.len()
    }
}

impl Default for TlsTrust {
    fn default() -> Self {
        Self::system()
    }
}

/// Names the bundle and counts its anchors. NEVER the anchors themselves: a
/// `[irc]` config is printed in full by `--check-config`, and a DER dump there
/// would be noise at best.
impl fmt::Debug for TlsTrust {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsTrust")
            .field("system_roots", &self.system_roots)
            .field("ca_file", &self.ca_file)
            .field("extra_anchors", &self.extra.len())
            .finish()
    }
}

/// Classify a PEM reader failure WITHOUT forwarding its `Display`. See
/// [`CaFault`] for why the parser is not allowed to phrase the complaint.
fn pem_fault(e: rustls_pki_types::pem::Error) -> CaFault {
    match e {
        rustls_pki_types::pem::Error::Io(e) => CaFault::Read(e),
        // MissingSectionEnd / IllegalSectionStart / Base64Decode / SectionTooLarge:
        // the file has a section that cannot be read as one.
        _ => CaFault::NotPem,
    }
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
    /// one per connection, whichever half failed first — see `Lifecycle`.
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
    /// Out-of-band graceful-stop wiring to the writer task. `None` for a
    /// [`scripted`](LineWriter::scripted) writer, which has no task to signal.
    /// See [`StopPhase`] and [`LineWriter::begin_graceful_stop`].
    stop: Option<StopControl>,
}

/// The writer task's graceful-stop wiring: a phase signal the task watches and
/// a completion flag it sets. Held by [`LineWriter`], never by a
/// [`Lifecycle`], so a graceful-stop handle cannot resurrect the "strong
/// reference keeps the inbound sender alive" hazard the `Weak` in `LineWriter`
/// exists to avoid — forcing the connection closed goes through the signal, not
/// a direct `Lifecycle` handle.
struct StopControl {
    signal: watch::Sender<StopPhase>,
    done: watch::Receiver<bool>,
}

/// Where a connection's writer is in an out-of-band graceful stop.
///
/// A graceful stop is NOT a queue drain: queued application writes are
/// discarded, a frame already in flight is allowed to finish (so the `QUIT`
/// lands on a line boundary), and the `QUIT` is then written directly,
/// bypassing the bounded outbound queue. The bound on the whole thing is the
/// caller's — [`StopPhase::Forced`] ends the connection at a deadline whether
/// or not the frame or the `QUIT` completed, and it is the forced deadline,
/// not the graceful request, that interrupts a stalled write (after which no
/// `QUIT` is written, since it would be the tail of that frame).
#[derive(Debug, Clone, PartialEq, Eq)]
enum StopPhase {
    /// Normal operation: application writes flow through the queue.
    Run,
    /// Stop requested: discard queued writes and send this `QUIT` line directly.
    Draining(String),
    /// The caller's deadline elapsed: end the connection now, `QUIT` or not.
    Forced,
}

/// Why the writer task's normal loop ended.
enum WriterExit {
    /// The connection ended on its own (socket error, dropped writer, or the
    /// other half): the optional body-free reason to report through
    /// [`Lifecycle::close`].
    Ended(Option<String>),
    /// A graceful stop was requested: the tail below handles the `QUIT`.
    Stopped,
}

/// Resolve once `rx` carries anything but [`StopPhase::Run`] — a graceful-stop
/// request. When every sender is gone a stop can never come, so this parks
/// forever rather than busy-looping on a closed watch, leaving the writer's
/// other select branches (the ended lifecycle, the closed queue) to end it.
async fn stop_requested(rx: &mut watch::Receiver<StopPhase>) {
    loop {
        if !matches!(&*rx.borrow_and_update(), StopPhase::Run) {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Resolve once `rx` carries [`StopPhase::Forced`]. Parks forever if the signal
/// is gone, for the same reason as [`stop_requested`]. A forced stop ends the
/// lifecycle BEFORE it sets this phase ([`LineWriter::force_stop`]), so a
/// select that lists the ended lifecycle first never takes a forced-stop arm
/// beside it; this is asked only where the lifecycle has ended already and
/// a bounded wait is still to be cut.
async fn forced(rx: &mut watch::Receiver<StopPhase>) {
    loop {
        if matches!(&*rx.borrow_and_update(), StopPhase::Forced) {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

impl LineWriter {
    /// Whether this connection is still live. `false` means it is finished —
    /// for EITHER half's failure, because the writer task drops the queue as
    /// soon as the shared `Lifecycle` ends, before it waits on a broken
    /// socket to finish tearing down. A [`FromServer::Closed`] is on the
    /// inbound stream by then.
    pub fn is_connected(&self) -> bool {
        !self.tx.is_closed() && !self.stopping()
    }

    /// Whether a graceful stop has been requested on this writer. Read at
    /// the API edge so the stop takes effect for callers the moment it is
    /// requested, not when the writer task next wakes and drops the queue.
    fn stopping(&self) -> bool {
        self.stop
            .as_ref()
            .is_some_and(|s| !matches!(&*s.signal.borrow(), StopPhase::Run))
    }

    /// Request an out-of-band graceful stop. From this call on, every
    /// [`send_line`](Transport::send_line) fails and
    /// [`is_connected`](LineWriter::is_connected) is false (the stop phase is
    /// read at the API edge, not when the writer task next wakes); the writer
    /// task discards whatever is queued, lets a frame already in flight
    /// finish so the `QUIT` lands on a line boundary, and sends `quit_line`
    /// directly. Completion is observable through
    /// [`stop_completion`](LineWriter::stop_completion); [`force_stop`] ends
    /// it at the caller's deadline. Non-blocking, and a no-op on a scripted
    /// writer.
    ///
    /// Idempotent only up to the QUIT line: the FIRST request's `quit_line` is
    /// the one sent; a later request cannot change it, which is what makes the
    /// completion signal single-shot.
    pub fn begin_graceful_stop(&self, quit_line: String) {
        if let Some(stop) = &self.stop {
            stop.signal.send_if_modified(|phase| {
                if matches!(phase, StopPhase::Run) {
                    *phase = StopPhase::Draining(quit_line);
                    true
                } else {
                    false
                }
            });
        }
    }

    /// The caller-supplied deadline elapsed before the graceful `QUIT`
    /// completed, or before the server closed after it: end the connection
    /// now, whatever state it is in. The shared lifecycle is closed FIRST,
    /// from here — that is what ends a reader waiting on a server that
    /// never closes after a landed QUIT, when the writer task is already
    /// gone, and what cancels a write in flight when it is not (the writer
    /// sees the ended lifecycle, abandons the frame and writes no `QUIT`
    /// after it, and fires completion). The phase is set after, so no task
    /// can see the phase before the end: the one thing it still does is
    /// cut the writer's bounded shutdown wait. Non-blocking, and a no-op
    /// on a scripted writer.
    pub fn force_stop(&self) {
        if let Some(life) = self.life.upgrade() {
            life.close("forced stop".to_string());
        }
        if let Some(stop) = &self.stop {
            let _ = stop.signal.send(StopPhase::Forced);
        }
    }

    /// A receiver that turns `true` once a graceful stop has finished — the
    /// `QUIT` was written, or its write was forced closed. A QUIT that landed
    /// leaves the READER up until the server closes the connection (or the
    /// guard is dropped): for a consumer that orders on the server being
    /// done with the connection, the server's close ([`SERVER_CLOSED`], with
    /// what it does and does not say) is the word to wait for. A forced or
    /// failed stop ends the connection outright. `None` for a scripted
    /// writer, which has no task to complete.
    ///
    /// A CLOSED channel (the receiver's `changed()` errs) is also terminal:
    /// the writer task ended before, or without, a stop being requested — for
    /// a socket that died on its own, say — and there is nothing left to
    /// complete. Callers wait for `true` OR closure, never for `true` alone.
    pub fn stop_completion(&self) -> Option<watch::Receiver<bool>> {
        self.stop.as_ref().map(|s| s.done.clone())
    }

    /// A writer wired to a plain channel instead of a socket: the seam the
    /// bridge's own tests script an IRC connection through, so a handler's
    /// effects can be read as lines without a server.
    ///
    /// `capacity` is the caller's precisely so an overflow is reachable — the
    /// real [`OUTBOUND_QUEUE`] takes 512 lines before it refuses one, which is
    /// not a state a test can arrive at by typing.
    ///
    /// There is no connection behind it and so no lifetime to end: the queue is
    /// the whole of it, and dropping this writer has nothing to cancel.
    #[cfg(test)]
    pub(crate) fn scripted(capacity: usize) -> (LineWriter, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel(capacity);
        (
            LineWriter {
                tx,
                life: Weak::new(),
                stop: None,
            },
            rx,
        )
    }
}

impl LineWriter {
    /// [`scripted`](LineWriter::scripted) with a stop control whose
    /// completion the TEST drives: the seam for ordering a connection's
    /// close against its writer's completion, which the two live tasks
    /// order only by the scheduler. The stop signal goes nowhere; the
    /// returned sender is the completion (`true` = the stop finished).
    #[cfg(test)]
    pub(crate) fn scripted_with_stop(
        capacity: usize,
    ) -> (LineWriter, mpsc::Receiver<String>, watch::Sender<bool>) {
        let (tx, rx) = mpsc::channel(capacity);
        let (signal, _) = watch::channel(StopPhase::Run);
        let (done_tx, done) = watch::channel(false);
        (
            LineWriter {
                tx,
                life: Weak::new(),
                stop: Some(StopControl { signal, done }),
            },
            rx,
            done_tx,
        )
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
        if self.stopping() {
            // A stop has been requested: nothing more is admitted, whether or
            // not the writer task has dropped the queue yet.
            return Err(SendError::Disconnected);
        }
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
pub(crate) trait Duplex: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Duplex for T {}

/// Connect to `server` (`host[:port]`), optionally over TLS, within `timeout`,
/// verifying the server certificate against the system trust store.
///
/// This is the whole of what a caller with no opinion about trust anchors
/// needs, and it is deliberately the shorter signature: an operator's private
/// CA is the exception, not the shape every call site has to spell out. That
/// exception is [`connect_with_trust`], which is this function with the
/// [`TlsTrust`] passed in; the two are identical in every other respect.
pub async fn connect(
    server: &str,
    tls: bool,
    timeout: Duration,
) -> Result<Connection, ConnectError> {
    connect_with_trust(server, tls, &TlsTrust::system(), timeout).await
}

/// [`connect`], with what a server certificate is verified against as a
/// parameter.
///
/// `timeout` is ONE budget for the whole establishment — the TCP connect, the
/// first-connection trust-store load and the TLS handshake share it — because a
/// caller's deadline is for getting a usable connection, not for a stage it
/// cannot see. A per-stage budget would let the documented bound be exceeded by
/// however many stages there happen to be.
///
/// TLS verifies the server certificate against `trust` — the system store, an
/// operator's private CA bundle, or both (see [`TlsTrust`]). There is no
/// "insecure" switch on any of those paths, because the SASL gate in the adapter
/// refuses to send a password over anything but TLS and a transport that could
/// be told to skip verification would make that gate decorative.
///
/// The name checked is the host half of `server`, exactly as written. A `server`
/// that names an IP address is verified as an IP address: the certificate has to
/// carry it in a SAN, and a DNS name on the certificate does not stand in for
/// one.
pub async fn connect_with_trust(
    server: &str,
    tls: bool,
    trust: &TlsTrust,
    timeout: Duration,
) -> Result<Connection, ConnectError> {
    let (host, port) = split_server(server, tls)?;
    let endpoint = format!("{host}:{port}");
    let dial = || TcpStream::connect((host.clone(), port));
    connect_with(dial, &host, endpoint, tls, trust, timeout).await
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
    trust: &TlsTrust,
    timeout: Duration,
) -> Result<Connection, ConnectError>
where
    C: FnOnce() -> F,
    F: std::future::Future<Output = io::Result<TcpStream>>,
{
    let established =
        tokio::time::timeout(timeout, establish(dial, host, &endpoint, tls, trust)).await;
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
    trust: &TlsTrust,
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
        let config = tls_config(trust).await?;
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

/// The [`FromServer::Closed`] reason for the one close that is the PEER's:
/// EOF on the read side — the other end closed the connection. Every other
/// reason is this side's doing (a forced stop, a failed write, a dropped
/// writer, a read error). It is the one signal there is that the server is
/// done with this connection, and no more: an IRC server drops a client
/// after deregistering it — after its QUIT, and everything sent before it,
/// has been processed and its departure broadcast — but the same EOF comes
/// from a server that timed the client out or killed it, from a server that
/// crashed, and from an intermediary that cut the connection, none of which
/// says anything about what was processed. A consumer that orders on the
/// server having seen its last lines — the bridge's puppet executor, in the
/// increment above this one, which orders a departure behind it — compares
/// against this as the best word available and carries that residual itself.
/// Nothing in this crate compares against it yet.
pub const SERVER_CLOSED: &str = "server closed";

/// Wire an already-connected duplex stream into a [`Connection`]. Split out so
/// the framing half is exercised over an in-process socket pair without a
/// server. Crate-visible so a consumer with connections of its own (the
/// bridge's puppet executor, above this increment) can be tested over an
/// in-process duplex pair the way the framing half is here.
pub(crate) fn spawn_connection(stream: Box<dyn Duplex>) -> Connection {
    spawn_connection_with_queue(stream, OUTBOUND_QUEUE)
}

/// [`spawn_connection`] with the outbound queue depth chosen by the caller.
/// The main connection takes [`OUTBOUND_QUEUE`], and is the only caller in
/// this increment; a consumer with connections of its own sizes them from
/// its own configuration (the puppet executor above this one does), and a
/// test that needs `Overflow` (a full queue behind a peer that is not
/// reading) can reach it with a handful of lines instead of 512.
pub(crate) fn spawn_connection_with_queue(stream: Box<dyn Duplex>, outbound: usize) -> Connection {
    let (read_half, mut write_half) = tokio::io::split(stream);
    // One slot PAST the queue depth, reserved immediately for the single
    // `Closed`: data fills the other 512, and the connection can still say it
    // has ended without waiting for a consumer to drain them.
    let (in_tx, inbound) = mpsc::channel::<FromServer>(OUTBOUND_QUEUE + 1);
    let closed_slot = in_tx.clone().try_reserve_owned().ok();
    let (out_tx, out_rx) = mpsc::channel::<String>(outbound.max(1));
    // Out-of-band graceful-stop wiring: a phase the writer task watches and a
    // completion flag it sets when a graceful stop finishes.
    let (stop_tx, mut stop_rx) = watch::channel(StopPhase::Run);
    let (done_tx, done_rx) = watch::channel(false);
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
                Ok(None) => break Some(SERVER_CLOSED.to_string()),
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
        // The queue is an Option so a graceful stop requested MID-FRAME can
        // drop it at once (every later `send_line` is rejected) while this
        // frame is still allowed to finish.
        let mut out_rx = Some(out_rx);
        let mut force_rx = stop_rx.clone();
        let exit = loop {
            let queued = tokio::select! {
                biased;
                () = writer_life.ended() => break WriterExit::Ended(None),
                // A graceful stop requested while the queue is idle: stop
                // draining application writes and go send the QUIT directly.
                () = stop_requested(&mut stop_rx) => break WriterExit::Stopped,
                queued = async {
                    match out_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => None,
                    }
                } => queued,
            };
            let Some(line) = queued else {
                // Every `LineWriter` is gone: the local side is finished with
                // this connection, which ends it as surely as a socket error
                // would — nothing can answer a PING any more. The writer's own
                // drop normally ends the lifecycle first (it has to, to cancel
                // a stalled write); an empty closed queue is the same fact
                // arriving by the slower road.
                break WriterExit::Ended(Some("local writer dropped".to_string()));
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
            // safe only where this branch never writes again: the connection
            // ending, or a FORCED stop. A GRACEFUL stop must not do it — the
            // QUIT it goes on to write would land after a half-sent body and be
            // read by the server as the tail of that line, not as a QUIT. So a
            // graceful stop requested mid-frame rejects further application
            // writes at once (the queue is dropped) but lets THIS frame finish;
            // the caller's forced deadline is what abandons a frame that never
            // finishes — it ends the lifecycle, which cancels the write here
            // — and then no QUIT is written at all.
            enum Mid {
                Wrote(io::Result<()>),
                Ended,
            }
            let mid = {
                let write = write_framed(&mut write_half, &line);
                tokio::pin!(write);
                let mut stopping = false;
                loop {
                    tokio::select! {
                        biased;
                        () = writer_life.ended() => break Mid::Ended,
                        () = stop_requested(&mut stop_rx), if !stopping => {
                            stopping = true;
                            out_rx = None;
                        }
                        wrote = &mut write => break Mid::Wrote(wrote),
                    }
                }
            };
            match mid {
                Mid::Ended => break WriterExit::Ended(None),
                Mid::Wrote(Err(e)) => {
                    // Body-free for the same reason the reader's is.
                    break WriterExit::Ended(Some(format!("write failed: {e}")));
                }
                Mid::Wrote(Ok(())) => {
                    if out_rx.is_none() {
                        // The frame finished on a line boundary; now the QUIT.
                        break WriterExit::Stopped;
                    }
                }
            }
        };
        match exit {
            WriterExit::Ended(reason) => {
                // TEARDOWN FIRST, then the report. `LineWriter::is_connected`
                // reads exactly this queue, so dropping it before anything else
                // means the connection is already gone by the time a consumer is
                // told so — never the other way round, where a consumer acting
                // on `Closed` could still be told the connection is live.
                drop(out_rx.take());
                if let Some(reason) = reason {
                    writer_life.close(reason);
                }
                // Best-effort, and bounded: tell the peer we are done so a QUIT
                // is not truncated by an abrupt socket drop, but never park here
                // forever on the same stalled socket a write was just cancelled
                // off — see [`SHUTDOWN_GRACE`]. A forced stop cuts even this
                // wait: the caller's deadline applies to every teardown path.
                tokio::select! {
                    biased;
                    () = forced(&mut force_rx) => {}
                    _ = tokio::time::timeout(SHUTDOWN_GRACE, write_half.shutdown()) => {}
                }
                // A stop that ended here (the connection dying under it) is
                // still complete for whoever asked: the single-shot signal
                // fires either way.
                if !matches!(&*stop_rx.borrow(), StopPhase::Run) {
                    let _ = done_tx.send(true);
                }
            }
            WriterExit::Stopped => {
                // Reject and discard every application write at once: dropping
                // the queue makes `is_connected` false and every later
                // `send_line` return `Disconnected`. Nothing queued is drained.
                drop(out_rx.take());
                let quit_line = match &*stop_rx.borrow_and_update() {
                    StopPhase::Draining(line) => Some(line.clone()),
                    // Forced since the request was seen: the lifecycle has
                    // ended already, and there is no QUIT to write.
                    _ => None,
                };
                // The reason reported through `Closed` says what actually
                // happened to the QUIT, body-free like every other reason
                // here: written, failed (error CLASS only), or never
                // attempted. A forced stop's reason is its own, on the
                // lifecycle it ended first.
                let mut reason = match quit_line {
                    Some(_) => "graceful stop",
                    None => "forced stop (no QUIT written)",
                };
                // A QUIT that LANDED does not end the connection here: the
                // write half is shut, but the reader stays up until the
                // SERVER closes — the best word there is that it is done
                // with the connection (`SERVER_CLOSED`, with what that does
                // and does not say) — or the caller drops the guard. A
                // consumer that orders on the server's close (the puppet
                // executor above this increment, ordering a departure
                // behind it) reads on until `Closed`; one that does not
                // (the main connection's own QUIT, today's only caller)
                // drops the guard. A QUIT that failed or was forced
                // still ends the connection: there is nothing to wait for.
                let mut landed = false;
                if let Some(line) = quit_line {
                    // Attempt QUIT directly, bypassing the discarded queue. A
                    // stalled write here is bounded by the caller forcing the
                    // connection closed at its deadline, which ends the
                    // lifecycle and cancels this write, and by SHUTDOWN_GRACE
                    // on the shutdown itself.
                    let write = async {
                        let wrote = write_framed(&mut write_half, &line).await;
                        let _ = tokio::time::timeout(SHUTDOWN_GRACE, write_half.shutdown()).await;
                        wrote
                    };
                    tokio::pin!(write);
                    tokio::select! {
                        biased;
                        () = writer_life.ended() => {}
                        wrote = &mut write => {
                            match wrote {
                                Ok(()) => landed = true,
                                Err(e) => {
                                    // The class, never the line: `graceful stop` would
                                    // claim a QUIT the server never got.
                                    let _ = e;
                                    reason = "graceful stop, QUIT write failed";
                                }
                            }
                        }
                    }
                }
                if !landed {
                    // The connection is over; close is idempotent, so a forced
                    // close that already ran is a no-op here.
                    writer_life.close(reason.to_string());
                }
                let _ = done_tx.send(true);
            }
        }
    });

    Connection {
        writer: LineWriter {
            tx: out_tx,
            life: writer_handle,
            stop: Some(StopControl {
                signal: stop_tx,
                done: done_rx,
            }),
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

/// The NATIVE trust anchors, read at most once.
///
/// The cache is here and NOT around the finished [`ClientConfig`] because the
/// configuration is now per-[`TlsTrust`]: an operator's `tls_ca_file` layers
/// extra anchors on top of these, and one process-wide `ClientConfig` could only
/// serve one such set. What was expensive is unchanged, though — reading and
/// parsing the system store is blocking file I/O a reconnect loop must not
/// repeat — so that is what stays cached, and assembling a `ClientConfig` from
/// an already-parsed store is the cheap part that is paid per connection.
///
/// A failed load is NOT cached: a trust store that was unreadable once (a store
/// still being written at boot, say) is retried by the next connection rather
/// than poisoning every one of them. An EMPTY native store counts as a failure
/// here for the same reason — and, because it is reported rather than fatal, a
/// host with no system store can still connect on a `tls_ca_file` alone.
static NATIVE_ROOTS: OnceCell<Arc<RootCertStore>> = OnceCell::const_new();

/// The cached native trust anchors, loading them on first use. The `Err` is the
/// body-free reason the store is unusable, for whoever has to explain it.
///
/// The one-time load runs on a blocking thread, so it never stalls the runtime,
/// and it happens inside [`connect`]'s budget, so it cannot make a connection
/// overrun the deadline its caller asked for.
async fn native_roots() -> Result<Arc<RootCertStore>, String> {
    NATIVE_ROOTS
        .get_or_try_init(|| async {
            tokio::task::spawn_blocking(load_native_roots)
                .await
                .map_err(|e| format!("the trust store load task failed: {e}"))?
                .map(Arc::new)
        })
        .await
        .cloned()
}

/// Read the system trust store into a [`RootCertStore`].
fn load_native_roots() -> Result<RootCertStore, String> {
    let native = rustls_native_certs::load_native_certs();
    let mut roots = RootCertStore::empty();
    for cert in native.certs {
        // A single unparseable certificate in the system store is not a reason
        // to refuse every connection; an EMPTY store is, and is checked next.
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        return Err(if native.errors.is_empty() {
            "the system trust store is empty".to_string()
        } else {
            native
                .errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        });
    }
    Ok(roots)
}

/// Build the TLS client configuration for one [`TlsTrust`]: its anchors, safe
/// defaults, no client certificate.
///
/// The crypto provider is named explicitly rather than left to the process
/// default, so which implementation performs the handshake is a property of this
/// file rather than of whichever crate in the build happened to install a
/// default first.
///
/// The refusal rule is unchanged and is about the RESULT, not its sources: an
/// empty store is refused, because nothing could ever be validated against it.
/// A system store that will not load is therefore fatal when it was the only
/// source asked for, and survivable — loudly — when a `tls_ca_file` supplied
/// anchors of its own.
async fn tls_config(trust: &TlsTrust) -> Result<Arc<ClientConfig>, ConnectError> {
    let mut roots = RootCertStore::empty();
    let mut native_fault = None;
    if trust.system_roots {
        match native_roots().await {
            Ok(native) => roots = (*native).clone(),
            Err(why) => native_fault = Some(why),
        }
    }
    for anchor in &trust.extra {
        // Proven addable by `TlsTrust::with_ca_file`; mapped rather than
        // ignored so this cannot become a silently smaller trust store.
        roots
            .add(anchor.clone())
            .map_err(|e| ConnectError::TlsConfig(format!("CA bundle anchor: {e}")))?;
    }
    if roots.is_empty() {
        return Err(ConnectError::NoTrustAnchors(native_fault.unwrap_or_else(
            || "no CA bundle was configured and the system store was not asked for".to_string(),
        )));
    }
    if let Some(why) = native_fault {
        // Asked for and unavailable, but the bundle carried it. Said out loud:
        // the store is smaller than the configuration describes.
        tracing::warn!(
            anchors = roots.roots.len(),
            "the system trust store is unusable ({why}); verifying against the configured CA \
             bundle alone"
        );
    }
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| ConnectError::TlsConfig(e.to_string()))
        .map(|b| Arc::new(b.with_root_certificates(roots).with_no_client_auth()))
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

    /// A socket whose reads stay pending and whose writes FAIL: the shape of a
    /// peer that dropped the write side. Used to prove the reported close
    /// reason says what happened to the QUIT.
    struct FailingWrite<R> {
        read: R,
    }

    impl<R: AsyncRead + Unpin> AsyncRead for FailingWrite<R> {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::pin::Pin::new(&mut self.read).poll_read(cx, buf)
        }
    }

    impl<R: Unpin> AsyncWrite for FailingWrite<R> {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            std::task::Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe)))
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

    #[tokio::test]
    async fn a_quit_that_fails_on_the_socket_is_reported_as_such_not_as_a_graceful_stop() {
        let (client, _server) = tokio::io::duplex(4096);
        let Connection {
            writer,
            mut inbound,
            mut guard,
        } = spawn_connection(Box::new(FailingWrite { read: client }));
        writer.begin_graceful_stop("QUIT :bye".to_string());
        let mut done = writer
            .stop_completion()
            .expect("a real writer has stop wiring");
        promptly("completion", done.wait_for(|d| *d))
            .await
            .expect("completion fires even though the QUIT failed");
        let closed = promptly("the close report", inbound.recv()).await;
        match closed {
            Some(FromServer::Closed(reason)) => {
                assert!(
                    reason.contains("QUIT write failed"),
                    "the reason says the QUIT did not land, got {reason:?}"
                );
                assert!(
                    !reason.contains("BrokenPipe") && !reason.contains("bye"),
                    "body-free and class-free: {reason:?}"
                );
            }
            other => panic!("expected Closed, got {other:?}"),
        }
        tokio::time::timeout(SHUTDOWN_GRACE * 2, &mut guard.writer_task)
            .await
            .expect("the writer task ends")
            .expect("no panic");
    }

    #[tokio::test]
    async fn a_force_stop_with_no_graceful_request_reports_a_forced_close() {
        let (client, _server) = tokio::io::duplex(4096);
        let Connection {
            writer,
            mut inbound,
            guard: _guard,
        } = spawn_connection(Box::new(client));
        writer.force_stop();
        let closed = promptly("the close report", inbound.recv()).await;
        assert!(
            matches!(&closed, Some(FromServer::Closed(r)) if r.contains("forced stop")),
            "no QUIT was ever requested, so the close is a forced one: {closed:?}"
        );
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
        if tls_config(&TlsTrust::system()).await.is_err() {
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
        let trust = TlsTrust::system();
        let outcome = connect_with(dial, "127.0.0.1", addr.to_string(), true, &trust, budget).await;
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

    // ── Private-CA trust ────────────────────────────────────────────────

    /// The committed fixtures: a test CA, and a leaf it signed carrying
    /// `IP:127.0.0.1` and `DNS:irc.test.invalid`. Generated once by
    /// `tests/fixtures/make-tls-fixtures.sh` (10-year validity, P-256/SHA-256);
    /// `rcgen` is not in this workspace's Cargo.lock, so they are committed
    /// rather than minted at test time.
    const CA_FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/ca.pem");
    const LEAF_CERT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/server.pem");
    const LEAF_KEY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/server.key.pem");

    fn test_ca() -> TlsTrust {
        // No system anchors: the positive leg then proves the bundle alone
        // verified the certificate, on a host with a trust store or without one.
        TlsTrust::with_ca_file(Path::new(CA_FILE), false).expect("the fixture CA loads")
    }

    /// A TLS server presenting the fixture leaf, on an OS-assigned port. It
    /// accepts and then holds the connection open: the only question here is
    /// whether the client's handshake completed.
    async fn private_ca_server() -> (std::net::SocketAddr, JoinHandle<()>) {
        use rustls_pki_types::PrivateKeyDer;
        use tokio_rustls::rustls::ServerConfig;
        use tokio_rustls::TlsAcceptor;

        let certs = CertificateDer::pem_file_iter(LEAF_CERT)
            .expect("the fixture leaf opens")
            .collect::<Result<Vec<_>, _>>()
            .expect("the fixture leaf parses");
        let key = PrivateKeyDer::from_pem_file(LEAF_KEY).expect("the fixture key parses");
        let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("safe defaults")
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .expect("the fixture chain and key agree");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if let Ok(tls) = acceptor.accept(stream).await {
                        let _held = tls;
                        std::future::pending::<()>().await;
                    }
                });
            }
        });
        (addr, task)
    }

    /// The point of configurable trust: a server certificate signed by an
    /// operator's own CA is accepted when that CA is configured, and rejected
    /// when it is not.
    ///
    /// The negative leg is the one that matters — it is what says the positive
    /// leg proved the CA rather than some general looseness. On a host with no
    /// system trust store the rejection arrives as `NoTrustAnchors` instead of a
    /// handshake failure; both are refusals, which is the assertion.
    #[tokio::test]
    async fn a_private_ca_is_trusted_when_configured_and_not_otherwise() {
        let (addr, server) = private_ca_server().await;
        let endpoint = addr.to_string();
        let budget = Duration::from_secs(20);

        connect_with_trust(&endpoint, true, &test_ca(), budget)
            .await
            .expect("a certificate signed by the configured CA verifies");

        let Err(refused) = connect_with_trust(&endpoint, true, &TlsTrust::system(), budget).await
        else {
            panic!("a privately-signed certificate must NOT verify against the system anchors");
        };
        assert!(
            matches!(
                refused,
                ConnectError::Tls(..) | ConnectError::NoTrustAnchors(..)
            ),
            "expected a verification refusal, got {refused:?}"
        );

        // And the bundle ADDS: with the system anchors kept (the default), the
        // private CA still verifies.
        let layered = TlsTrust::with_ca_file(Path::new(CA_FILE), true).unwrap();
        connect_with_trust(&endpoint, true, &layered, budget)
            .await
            .expect("a configured CA is layered on the system anchors, not swapped for them");

        server.abort();
    }

    /// Trusting a CA says who may ISSUE a certificate, never which name it is
    /// for. A configured CA must not make name verification any softer — which
    /// is exactly what an operator connecting to a LAN box by IP is relying on.
    ///
    /// `connect_with` rather than `connect` because the name and the address
    /// have to differ: the fixture's SANs are `IP:127.0.0.1` and
    /// `DNS:irc.test.invalid`, and `localhost` is neither.
    #[tokio::test]
    async fn a_configured_ca_does_not_loosen_server_name_verification() {
        let (addr, server) = private_ca_server().await;
        let trust = test_ca();
        let budget = Duration::from_secs(20);

        for (name, expected) in [("irc.test.invalid", true), ("localhost", false)] {
            let dial = || TcpStream::connect(addr);
            let outcome = connect_with(dial, name, addr.to_string(), true, &trust, budget).await;
            assert_eq!(
                outcome.is_ok(),
                expected,
                "`{name}` against SANs IP:127.0.0.1 + DNS:irc.test.invalid: got {:?}",
                outcome.err()
            );
        }

        // The IP SAN is what `connect("127.0.0.1:…")` verifies against: rustls
        // builds a `ServerName::IpAddress`, and the DNS SAN does not stand in.
        connect_with_trust(&addr.to_string(), true, &trust, budget)
            .await
            .expect("the certificate carries IP:127.0.0.1 in a SAN");

        server.abort();
    }

    #[test]
    fn a_ca_bundle_that_cannot_anchor_anything_is_refused_without_echoing_it() {
        // `CARGO_TARGET_TMPDIR` is set for integration tests only, so a unit
        // test takes the process temp dir (which .cargo/config.toml points
        // inside `target/`, so `cargo clean` sweeps it).
        let dir = std::env::temp_dir().join(format!("mu-irc-gw-ca-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sentinel = "NOT-BASE64-DO-NOT-ECHO!!";
        let bad = dir.join("bad-ca.pem");
        std::fs::write(
            &bad,
            format!("-----BEGIN CERTIFICATE-----\n{sentinel}\n-----END CERTIFICATE-----\n"),
        )
        .unwrap();
        let fault = TlsTrust::with_ca_file(&bad, true).unwrap_err();
        assert!(matches!(fault, CaFault::NotPem), "got {fault:?}");
        for rendered in [format!("{fault}"), format!("{fault:?}")] {
            assert!(
                !rendered.contains(sentinel),
                "the fault echoed the file: {rendered}"
            );
        }

        // A section that reads as PEM and decodes, but is not a certificate.
        // The bundle is refused WHOLE: half a trust store is not a trust store.
        let junk = dir.join("junk-ca.pem");
        std::fs::write(
            &junk,
            "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        assert!(matches!(
            TlsTrust::with_ca_file(&junk, true).unwrap_err(),
            CaFault::Unusable
        ));

        let empty = dir.join("empty-ca.pem");
        std::fs::write(&empty, "").unwrap();
        assert!(matches!(
            TlsTrust::with_ca_file(&empty, true).unwrap_err(),
            CaFault::Empty
        ));
        assert!(matches!(
            TlsTrust::with_ca_file(&dir.join("absent.pem"), true).unwrap_err(),
            CaFault::Read(_)
        ));
    }

    /// The default is the v0 behaviour, unchanged: the system store, nothing
    /// added, and no way to say "skip verification".
    #[test]
    fn the_default_trust_is_the_system_store_alone() {
        let trust = TlsTrust::default();
        assert!(trust.system_roots());
        assert_eq!(trust.extra_anchors(), 0);
        assert!(trust.ca_file().is_none());
        // Debug is printed verbatim by `--check-config`: it names the bundle and
        // counts anchors, never their bytes.
        let printed = format!("{:?}", test_ca());
        assert!(printed.contains("extra_anchors: 1"), "{printed}");
        assert!(printed.contains("ca.pem"), "{printed}");
        assert!(!printed.contains("CERTIFICATE"), "{printed}");
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

    /// A graceful stop bypasses the queue: it sends `QUIT` directly and reports
    /// completion, without a forced deadline, on a peer that reads. This is the
    /// puppet-teardown path in isolation — a registered puppet is asked to leave
    /// and does.
    #[tokio::test]
    async fn a_graceful_stop_sends_quit_directly_and_completes() {
        use tokio::io::AsyncReadExt as _;

        let (client, mut server) = tokio::io::duplex(4096);
        let Connection {
            writer,
            // Kept alive so the reader half does not end the connection out from
            // under the QUIT write before it is sent.
            inbound: _inbound,
            mut guard,
        } = spawn_connection(Box::new(client));

        writer.begin_graceful_stop("QUIT :bye".to_string());

        // The peer receives the QUIT, framed, with nothing else ahead of it.
        let mut buf = Vec::new();
        promptly("the QUIT to reach the peer", async {
            let mut tmp = [0u8; 256];
            loop {
                let n = server.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if buf.ends_with(b"\r\n") {
                    break;
                }
            }
        })
        .await;
        assert_eq!(String::from_utf8_lossy(&buf), "QUIT :bye\r\n");

        // Completion fires and the writer task ends on its own — no task outlives
        // the stop.
        let mut done = writer
            .stop_completion()
            .expect("a real writer has stop wiring");
        promptly("graceful stop completion", done.wait_for(|d| *d))
            .await
            .expect("the done signal must not be dropped");
        promptly("the writer task to end", &mut guard.writer_task)
            .await
            .expect("the writer task must not panic");
        assert!(!writer.is_connected());
    }

    #[tokio::test]
    async fn a_scripted_writer_with_a_stop_control_stops_at_the_api_edge() {
        // The scripted seam with a stop control behaves like a live writer
        // at its edge: a graceful stop refuses every later write at once,
        // and completion is whatever the test says it is — nothing
        // completes on its own, since there is no task behind it.
        let (mut writer, mut lines, done) = LineWriter::scripted_with_stop(4);
        assert!(writer.send_line("PING :a").is_ok());
        assert_eq!(lines.try_recv().ok().as_deref(), Some("PING :a"));
        let completion = writer.stop_completion().expect("a stop control");
        assert!(!*completion.borrow());
        writer.begin_graceful_stop("QUIT :bye".into());
        assert!(!writer.is_connected());
        assert!(matches!(
            writer.send_line("PING :b"),
            Err(SendError::Disconnected)
        ));
        assert!(lines.try_recv().is_err(), "nothing after the stop");
        done.send(true).expect("the completion is watched");
        assert!(*completion.borrow());
        writer.force_stop();
        assert!(!writer.is_connected());
    }

    #[tokio::test]
    async fn a_landed_quit_keeps_the_reader_up_until_the_server_closes_or_a_forced_stop() {
        use tokio::io::AsyncReadExt as _;
        let (client, server) = tokio::io::duplex(4096);
        let Connection {
            writer,
            mut inbound,
            guard: _guard,
        } = spawn_connection(Box::new(client));
        let (mut srv_r, mut srv_w) = tokio::io::split(server);
        writer.begin_graceful_stop("QUIT :bye".to_string());
        let mut done = writer.stop_completion().expect("stop wiring");
        promptly("completion", done.wait_for(|d| *d))
            .await
            .expect("completion fires");
        let mut buf = [0u8; 64];
        let n = promptly("the QUIT on the wire", srv_r.read(&mut buf))
            .await
            .expect("read");
        assert!(String::from_utf8_lossy(&buf[..n]).starts_with("QUIT :bye"));
        // The QUIT landed: the connection is NOT reported closed — the
        // reader is waiting on the server — and the server can still talk.
        assert!(
            tokio::time::timeout(Duration::from_millis(200), inbound.recv())
                .await
                .is_err(),
            "a landed QUIT must not end the connection by itself"
        );
        srv_w.write_all(b"ERROR :Closing link\r\n").await.unwrap();
        assert!(matches!(
            promptly("the server's line", inbound.recv()).await,
            Some(FromServer::Line(l)) if l.starts_with("ERROR")
        ));
        // A server that never closes: the caller's forced stop ends it, and
        // the reader reports so.
        writer.force_stop();
        assert!(
            matches!(
                promptly("the forced close", inbound.recv()).await,
                Some(FromServer::Closed(r)) if r.contains("forced stop")
            ),
            "a forced stop after a landed QUIT must close the connection"
        );
    }

    #[tokio::test]
    async fn a_requested_stop_rejects_writes_at_once_not_when_the_task_wakes() {
        let (client, _server) = tokio::io::duplex(4096);
        let Connection {
            mut writer,
            inbound: _inbound,
            guard: _guard,
        } = spawn_connection(Box::new(client));
        assert!(writer.is_connected());
        writer.begin_graceful_stop("QUIT :bye".to_string());
        // No yield: the writer task has not run since the request.
        assert!(
            !writer.is_connected(),
            "the stop is visible the moment it is requested"
        );
        assert_eq!(
            writer.send_line("PRIVMSG #x :late"),
            Err(SendError::Disconnected),
            "nothing is admitted after the request"
        );
    }

    #[tokio::test]
    async fn a_graceful_stop_lands_quit_on_a_line_boundary_after_the_frame_in_flight() {
        use tokio::io::AsyncReadExt as _;

        // A peer that reads in tiny pieces, so a frame is genuinely in flight
        // when the stop arrives: the frame must finish, and the QUIT must be
        // its own line — never the tail of the PRIVMSG.
        let (client, mut server) = tokio::io::duplex(8);
        let Connection {
            mut writer,
            inbound: _inbound,
            mut guard,
        } = spawn_connection(Box::new(client));
        writer.send_line("PRIVMSG #x :hello there").unwrap();
        writer.send_line("PRIVMSG #x :never sent").unwrap();
        tokio::task::yield_now().await;
        writer.begin_graceful_stop("QUIT :bye".to_string());

        let mut buf = Vec::new();
        promptly("the stream to end", async {
            let mut tmp = [0u8; 64];
            loop {
                let n = server.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
        })
        .await;
        let text = String::from_utf8_lossy(&buf);
        let lines: Vec<&str> = text.split("\r\n").filter(|l| !l.is_empty()).collect();
        assert!(
            lines
                .first()
                .is_some_and(|l| *l == "PRIVMSG #x :hello there")
                || lines.first().is_some_and(|l| *l == "QUIT :bye"),
            "the first line is a whole frame or the QUIT, never a fragment: {text:?}"
        );
        assert_eq!(lines.last(), Some(&"QUIT :bye"), "{text:?}");
        assert!(
            !text.contains("never sent"),
            "queued writes behind the stop are discarded: {text:?}"
        );
        tokio::time::timeout(SHUTDOWN_GRACE * 2, &mut guard.writer_task)
            .await
            .expect("the writer task ends")
            .expect("no panic");
    }

    /// The stop is bounded even when the socket is stalled: a full application
    /// queue is discarded at once, the stalled write is interrupted, and the
    /// caller's deadline (a forced close) ends the connection and the task —
    /// which is the guarantee puppet teardown needs against a stalled server.
    #[tokio::test(start_paused = true)]
    async fn a_graceful_stop_discards_a_full_queue_and_a_deadline_bounds_a_stalled_frame() {
        let started = Arc::new(AtomicBool::new(false));
        let (client, _server) = tokio::io::duplex(4096);
        let Connection {
            mut writer,
            mut inbound,
            mut guard,
        } = spawn_connection(Box::new(StalledWrite {
            read: client,
            started: started.clone(),
        }));

        // Park the writer inside a write, then fill the outbound queue behind it.
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
        let mut queued = 0;
        while writer.send_line("PRIVMSG #x :spam").is_ok() {
            queued += 1;
        }
        assert!(
            queued > 0,
            "the queue must fill for this to test a full queue"
        );

        // Graceful stop: the stalled write is interrupted and the whole queue is
        // discarded at once, so every later write is rejected.
        writer.begin_graceful_stop("QUIT :bye".to_string());
        let mut done = writer
            .stop_completion()
            .expect("a real writer has stop wiring");
        for _ in 0..100 {
            if !writer.is_connected() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            !writer.is_connected(),
            "a graceful stop discards the queue and rejects further application writes"
        );
        assert_eq!(
            writer.send_line("PRIVMSG #x :more"),
            Err(SendError::Disconnected)
        );

        // The parked write is the PING frame the stop found in flight: a
        // graceful stop lets a frame finish, so on a socket that never
        // completes nothing happens until the caller forces the connection
        // closed at its deadline — at which point the frame is abandoned and
        // no QUIT is attempted at all (it would be that frame's tail).
        assert!(
            !*done.borrow_and_update(),
            "a stop stalled inside a frame has not completed"
        );
        writer.force_stop();

        promptly("completion after a forced deadline", done.wait_for(|d| *d))
            .await
            .expect("the done signal must not be dropped");
        tokio::time::timeout(SHUTDOWN_GRACE * 4, &mut guard.writer_task)
            .await
            .expect("a forced graceful stop must not outlive its deadline")
            .expect("the writer task must not panic");
        let closed = promptly("the connection to report closed", inbound.recv()).await;
        assert!(
            matches!(closed, Some(FromServer::Closed(_))),
            "a forced graceful stop still reports the connection closed, got {closed:?}"
        );
    }
}
