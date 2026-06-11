use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::broadcast;

use crate::command_journal::Origin;
use crate::protocol::{ErrorObject, Notification, Request, Response, JSONRPC_VERSION};

// ===== Public API =====

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    /// Outbound channel closed before all messages were flushed.
    #[error("outbound channel closed")]
    OutboundClosed,
}

/// Standard JSON-RPC 2.0 error codes.
pub mod codes {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;

    // mu-fnn (mu-7rk-c): application-defined codes for the connect-time
    // auth enforcement gate. JSON-RPC 2.0 reserves -32099..=-32000 for
    // server-defined application errors.
    /// The method requires an authenticated connection (per-connection
    /// `AuthState` is `Unauthenticated`).
    pub const AUTH_REQUIRED: i32 = -32001;
    /// The connection's `AuthState` is terminally `Denied` — including
    /// re-attempts of pre-auth methods are rejected until reconnect.
    pub const AUTH_DENIED: i32 = -32002;
    /// spec mu-046 INV-2 (fail closed): the command could not be made
    /// durable — the journal append errored — so it was rejected and
    /// never processed.
    pub const JOURNAL_UNAVAILABLE: i32 = -32003;
}

/// Build a successful Response<Value>. Caller has already serialized
/// the result type into a Value.
pub fn ok_response(id: Value, result: Value) -> Response<Value> {
    Response::Ok {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id,
        result,
    }
}

/// Build an error Response<Value>.
pub fn err_response(id: Value, code: i32, message: impl Into<String>) -> Response<Value> {
    Response::Err {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id,
        error: ErrorObject {
            code,
            message: message.into(),
            data: None,
        },
    }
}

/// Capacity of the daemon-wide outbound broadcast stream. Generous so
/// a healthy connection writer never lags under normal load; see
/// [`write_loop`] for the (documented) lossy-under-extreme-lag
/// tradeoff.
pub const OUTBOUND_STREAM_CAPACITY: usize = 8192;

/// A tagged item on the daemon-wide outbound stream (spec mu-046
/// INV-8: all responses and notifications leave through this stream —
/// no writer bypasses it).
///
/// `origin: None` means broadcast — every connection delivers it.
/// `Some(o)` means only the connection whose [`Origin`] matches
/// delivers.
#[derive(Clone, Debug)]
pub struct OutboundEnvelope {
    /// Which connection this belongs to; `None` ⇒ broadcast.
    pub origin: Option<Origin>,
    /// JSON-RPC id for response correlation (`None` for notifications).
    pub request_id: Option<Value>,
    /// Command-journal correlation (spec mu-046). Tagged by the ingest
    /// pipeline once commands are journaled (WP3); `None` until then.
    pub command_seq: Option<u64>,
    /// The serialized Response or Notification.
    pub item: Outbound,
}

/// The daemon-wide outbound stream (spec mu-046 INV-8): the one way
/// bytes leave the daemon. spmc over [`tokio::sync::broadcast`] —
/// producers (handlers, forwarders, the response path) send tagged
/// [`OutboundEnvelope`]s; each transport writer subscribes and forwards
/// the envelopes addressed to its connection (or broadcasts).
///
/// Cheap to clone (the broadcast sender is Arc-y internally).
#[derive(Clone, Debug)]
pub struct OutboundStream {
    tx: broadcast::Sender<OutboundEnvelope>,
}

impl OutboundStream {
    /// Create a stream with [`OUTBOUND_STREAM_CAPACITY`].
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(OUTBOUND_STREAM_CAPACITY);
        Self { tx }
    }

    /// Subscribe a new per-connection receiver. Only envelopes sent
    /// after this call are observed.
    pub fn subscribe(&self) -> broadcast::Receiver<OutboundEnvelope> {
        self.tx.subscribe()
    }

    /// Send an envelope. Never panics: a send with zero receivers is
    /// silently ignored — the daemon may emit before any connection
    /// attaches.
    pub fn send(&self, envelope: OutboundEnvelope) {
        let _ = self.tx.send(envelope);
    }
}

impl Default for OutboundStream {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle on the daemon-wide outbound stream for emitting
/// notifications. Cheap to clone. Pass into request handlers so they
/// can emit notifications mid-flight.
///
/// Carries an `Option<Origin>`: a writer created for a connection tags
/// its notifications with that connection's origin, so they deliver
/// only there (today's semantics — a session's notifications go to the
/// connection that spawned it). An origin-less writer broadcasts to
/// every connection.
#[derive(Clone, Debug)]
pub struct NotificationWriter {
    origin: Option<Origin>,
    stream: OutboundStream,
}

impl NotificationWriter {
    /// Create a no-op writer whose notifications are silently dropped.
    /// Used by the MCP server surface where notifications don't need to
    /// be forwarded to the MCP client.
    pub fn sink() -> Self {
        // A private stream with no subscribers: every send is ignored.
        Self {
            origin: None,
            stream: OutboundStream::new(),
        }
    }

    /// Origin-less writer: notifications fan out to every connection
    /// subscribed to `stream`.
    pub fn broadcast(stream: OutboundStream) -> Self {
        Self {
            origin: None,
            stream,
        }
    }

    /// Writer whose notifications deliver only to the connection whose
    /// [`Origin`] matches `origin`.
    pub fn for_origin(stream: OutboundStream, origin: Origin) -> Self {
        Self {
            origin: Some(origin),
            stream,
        }
    }

    /// Emit a notification. Returns `Ok(())` even with no subscribers —
    /// see §INV-5.
    pub async fn emit<P: Serialize>(&self, method: &str, params: P) -> Result<(), TransportError> {
        let params = serde_json::to_value(params)?;
        let notif = Notification {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.to_string(),
            params,
        };
        let value = serde_json::to_value(&notif)?;
        self.stream.send(OutboundEnvelope {
            origin: self.origin.clone(),
            request_id: None,
            command_seq: None,
            item: Outbound(value),
        });
        Ok(())
    }
}

/// Convenience: serve over the process's actual stdin/stdout.
pub async fn serve_stdio<F, Fut>(handler: F) -> Result<(), TransportError>
where
    F: Fn(Request<Value>, NotificationWriter) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<Value>> + Send + 'static,
{
    let stdin = BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    serve(stdin, stdout, handler).await
}

/// Generic transport: read newline-delimited JSON requests from
/// `reader`, dispatch each to `handler` on `tokio::spawn`, write
/// responses and notifications back to `writer`.
///
/// Creates a private [`OutboundStream`] for this connection. Daemons
/// that own a daemon-wide stream (spec mu-046 INV-8) should call
/// [`serve_with_stream`] instead and pass it down.
pub async fn serve<R, W, F, Fut>(reader: R, writer: W, handler: F) -> Result<(), TransportError>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    F: Fn(Request<Value>, NotificationWriter) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<Value>> + Send + 'static,
{
    serve_with_stream(reader, writer, OutboundStream::new(), handler).await
}

/// Process-wide connection counter so every connection served by this
/// daemon gets a unique [`Origin`] at accept time.
static CONNECTION_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Allocate this connection's identity at accept time.
fn next_stdio_origin() -> Origin {
    let id = CONNECTION_COUNTER.fetch_add(1, Ordering::Relaxed);
    Origin {
        transport: "stdio".into(),
        connection_id: Some(id.to_string()),
    }
}

/// [`serve`] over a caller-owned daemon-wide [`OutboundStream`] (spec
/// mu-046 INV-8: one way out). This connection gets a fresh [`Origin`];
/// the handler's responses are enveloped with it (plus the request id)
/// and sent to the stream, and the connection's writer subscribes,
/// filtering to envelopes addressed to it (or broadcast).
///
/// Adapter shim over [`serve_with_ingest`] preserving the historical
/// handler-returns-`Response` contract: each request's handler future
/// is spawned (concurrent dispatch) and its response enveloped onto
/// the stream when it resolves. The DAEMON does not use this — it
/// flows through `serve_with_ingest` so every command is journaled
/// before processing (spec mu-046 INV-7, no side doors); this stays
/// for transports/tests that don't carry a journal.
pub async fn serve_with_stream<R, W, F, Fut>(
    reader: R,
    writer: W,
    stream: OutboundStream,
    handler: F,
) -> Result<(), TransportError>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    F: Fn(Request<Value>, NotificationWriter) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<Value>> + Send + 'static,
{
    let handler = Arc::new(handler);
    let respond_stream = stream.clone();
    serve_with_ingest(reader, writer, stream, move |request, notif, origin| {
        let handler = Arc::clone(&handler);
        let stream = respond_stream.clone();
        async move {
            let request_id = request.id.clone();
            let response_fut = handler(request, notif);
            // Spawned, not awaited inline: this shim keeps the
            // pre-ingest concurrent-dispatch semantics (a slow request
            // must not block the next line). The spawned task holds a
            // stream sender clone, so the writer drains every response
            // before observing Closed on shutdown.
            tokio::spawn(async move {
                let response = response_fut.await;
                match serde_json::to_value(response) {
                    Ok(value) => stream.send(OutboundEnvelope {
                        origin: Some(origin),
                        request_id: Some(request_id),
                        command_seq: None,
                        item: Outbound(value),
                    }),
                    Err(err) => tracing::warn!(%err, "response serialization failed"),
                }
            });
            None
        }
    })
    .await
}

/// The transport seam of the ingest pipeline (spec mu-046 WP3). Reads
/// newline-delimited JSON requests and hands each parsed request — with
/// this connection's [`Origin`] — to `handler`, which is the
/// ingest/route step:
///
/// - `Some(response)` ⇒ immediate reject (parse-adjacent failure,
///   journal unavailable): the transport envelopes and sends it, as it
///   always did.
/// - `None` ⇒ the pipeline owns the response; it arrives via the
///   outbound stream, which [`write_loop`] already delivers (INV-8).
///
/// The handler is awaited INLINE, not spawned: ingest must observe
/// commands in wire order so journal seq order == queue order (INV-3).
/// Keep ingest fast — journal append + enqueue; the heavy work belongs
/// to the pipeline consumer behind the queue.
pub async fn serve_with_ingest<R, W, F, Fut>(
    reader: R,
    writer: W,
    stream: OutboundStream,
    handler: F,
) -> Result<(), TransportError>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    F: Fn(Request<Value>, NotificationWriter, Origin) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Option<Response<Value>>> + Send + 'static,
{
    let origin = next_stdio_origin();
    let notif = NotificationWriter::for_origin(stream.clone(), origin.clone());
    let writer_task = tokio::spawn(write_loop(writer, stream.subscribe(), origin.clone()));
    let mut lines = reader.lines();

    while let Some(line) = lines.next_line().await? {
        match parse_request_line(&line) {
            Ok(request) => {
                let request_id = request.id.clone();
                if let Some(response) = handler(request, notif.clone(), origin.clone()).await {
                    match serde_json::to_value(response) {
                        Ok(value) => stream.send(OutboundEnvelope {
                            origin: Some(origin.clone()),
                            request_id: Some(request_id),
                            command_seq: None,
                            item: Outbound(value),
                        }),
                        Err(err) => tracing::warn!(%err, "response serialization failed"),
                    }
                }
            }
            Err(response) => {
                let value = serde_json::to_value(response)?;
                let request_id = value.get("id").cloned().unwrap_or(Value::Null);
                stream.send(OutboundEnvelope {
                    origin: Some(origin.clone()),
                    request_id: Some(request_id),
                    command_seq: None,
                    item: Outbound(value),
                });
            }
        }
    }

    // CRITICAL for clean shutdown post mu-035 Phase A (multi-turn fix):
    // dropping `handler` here releases the closure that captures the
    // daemon's state. On the ingest path (mu-046) that closure holds
    // the control-plane queue sender: dropping it lets the pipeline
    // consumer exit on recv()==None, which releases the daemon's
    // `sessions` map. On the shim path above, the closure holds the
    // sessions-capturing handler directly. Either way, releasing
    // sessions drops every SessionState, which drops every agent-loop
    // input sender, which lets the per-session agent loops exit on
    // recv()==None, which drops their events senders, which lets the
    // per-session forwarders exit, which drops their
    // NotificationWriter clones, which finally lets `writer_task` see
    // every stream sender clone drop (broadcast recv -> Closed) and
    // exit. In-flight spawned request tasks hold their own clones and
    // extend the writer's life exactly until their responses are sent.
    //
    // Pre-multi-turn this chain worked implicitly because the agent
    // loop returned after one Done — but with multi-turn the loop
    // now survives until its input channel actually closes, which
    // can only happen after sessions drops, which requires this
    // explicit drop.
    drop(handler);
    drop(notif);
    drop(stream);

    match writer_task.await {
        Ok(result) => result,
        Err(err) => {
            tracing::warn!(%err, "writer task failed");
            Err(TransportError::OutboundClosed)
        }
    }
}

/// Anything destined for the outbound stream: a serialized Response
/// or Notification, already as a Value so it can be flushed without
/// re-borrowing the type. Public because it rides inside
/// [`OutboundEnvelope`] (spec mu-046 INV-8).
#[derive(Clone, Debug)]
pub struct Outbound(pub Value);

// ===== Internal =====

/// Per-connection delivery: subscribe to the daemon-wide stream,
/// forward envelopes addressed to this connection (`origin` matches
/// `self`) or broadcast (`origin` is `None`) as JSONL. Broadcast
/// preserves global send order, so per-connection ordering matches the
/// old single mpsc channel.
///
/// On `Lagged` (this subscriber fell more than the stream capacity
/// behind) the skipped envelopes are gone: lossy-under-extreme-lag is
/// a known MVP tradeoff of the spmc design — lossless per-connection
/// delivery (e.g. a per-connection buffering layer) is a possible
/// follow-up.
async fn write_loop<W>(
    mut writer: W,
    mut rx: broadcast::Receiver<OutboundEnvelope>,
    origin: Origin,
) -> Result<(), TransportError>
where
    W: AsyncWrite + Unpin,
{
    loop {
        let envelope = match rx.recv().await {
            Ok(envelope) => envelope,
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(
                    skipped,
                    "outbound stream lagged: envelopes dropped for this connection"
                );
                continue;
            }
            // Every sender dropped — clean shutdown.
            Err(broadcast::error::RecvError::Closed) => break,
        };
        match &envelope.origin {
            Some(o) if *o != origin => continue,
            _ => {}
        }
        let line = serde_json::to_string(&envelope.item.0)?;
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }
    Ok(())
}

fn parse_request_line(line: &str) -> Result<Request<Value>, Response<Value>> {
    let value = match serde_json::from_str::<Value>(line) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(%err, "malformed json-rpc line");
            return Err(err_response(Value::Null, codes::PARSE_ERROR, "parse error"));
        }
    };

    let id = value.get("id").cloned().unwrap_or(Value::Null);
    match serde_json::from_value::<Request<Value>>(value) {
        Ok(request) => Ok(request),
        Err(err) => {
            tracing::warn!(%err, "invalid json-rpc request");
            Err(err_response(id, codes::INVALID_REQUEST, "invalid request"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;
    use tokio::io::{duplex, AsyncWriteExt, BufReader};

    struct Harness {
        input: tokio::io::DuplexStream,
        output: tokio::io::Lines<BufReader<tokio::io::DuplexStream>>,
        serve_task: tokio::task::JoinHandle<Result<(), TransportError>>,
    }

    fn spawn_harness<F, Fut>(handler: F) -> Harness
    where
        F: Fn(Request<Value>, NotificationWriter) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Response<Value>> + Send + 'static,
    {
        spawn_harness_on_stream(OutboundStream::new(), handler)
    }

    /// Like [`spawn_harness`] but the connection joins a caller-owned
    /// daemon-wide stream — lets tests attach multiple connections to
    /// one stream (spec mu-046 INV-8).
    fn spawn_harness_on_stream<F, Fut>(stream: OutboundStream, handler: F) -> Harness
    where
        F: Fn(Request<Value>, NotificationWriter) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Response<Value>> + Send + 'static,
    {
        let (input, server_reader) = duplex(64 * 1024);
        let (server_writer, output) = duplex(64 * 1024);
        let reader = BufReader::new(server_reader);
        let output = BufReader::new(output).lines();
        let serve_task = tokio::spawn(serve_with_stream(reader, server_writer, stream, handler));
        Harness {
            input,
            output,
            serve_task,
        }
    }

    async fn write_json_line(
        input: &mut tokio::io::DuplexStream,
        value: Value,
    ) -> Result<(), std::io::Error> {
        let line = format!("{value}\n");
        input.write_all(line.as_bytes()).await
    }

    async fn read_value(
        output: &mut tokio::io::Lines<BufReader<tokio::io::DuplexStream>>,
    ) -> Result<Value, Box<dyn std::error::Error>> {
        let line = output.next_line().await?.ok_or("output stream closed")?;
        Ok(serde_json::from_str(&line)?)
    }

    #[tokio::test]
    async fn line_framing_writes_exactly_one_lf() -> Result<(), Box<dyn std::error::Error>> {
        let mut harness =
            spawn_harness(|req, _| async move { ok_response(req.id, json!({"pong": true})) });
        write_json_line(
            &mut harness.input,
            json!({"jsonrpc":"2.0","id":1,"method":"ping","params":null}),
        )
        .await?;

        let raw = harness
            .output
            .next_line()
            .await?
            .ok_or("output stream closed")?;
        assert!(!raw.contains('\n'));
        assert!(!raw.contains('\r'));
        let mut serialized = raw.clone();
        serialized.push('\n');
        assert_eq!(serialized.matches('\n').count(), 1);
        let value: Value = serde_json::from_str(&raw)?;
        assert_eq!(value["id"], json!(1));
        Ok(())
    }

    #[tokio::test]
    async fn round_trip_request() -> Result<(), Box<dyn std::error::Error>> {
        let mut harness =
            spawn_harness(|req, _| async move { ok_response(req.id, json!({"pong": true})) });
        write_json_line(
            &mut harness.input,
            json!({"jsonrpc":"2.0","id":1,"method":"ping","params":null}),
        )
        .await?;

        let response = read_value(&mut harness.output).await?;
        assert_eq!(response["jsonrpc"], json!("2.0"));
        assert_eq!(response["id"], json!(1));
        assert_eq!(response["result"], json!({"pong": true}));
        Ok(())
    }

    #[tokio::test]
    async fn notification_emission_precedes_response() -> Result<(), Box<dyn std::error::Error>> {
        let mut harness = spawn_harness(|req, notif| async move {
            if let Err(err) = notif
                .emit("session.text_delta", json!({"session_id":"s","delta":"hi"}))
                .await
            {
                panic!("notification emit failed: {err}");
            }
            ok_response(req.id, json!({"accepted": true}))
        });
        write_json_line(
            &mut harness.input,
            json!({"jsonrpc":"2.0","id":1,"method":"ask_session","params":null}),
        )
        .await?;

        let notification = read_value(&mut harness.output).await?;
        let response = read_value(&mut harness.output).await?;
        assert_eq!(notification["method"], json!("session.text_delta"));
        assert_eq!(
            notification["params"],
            json!({"session_id":"s","delta":"hi"})
        );
        assert_eq!(response["id"], json!(1));
        assert_eq!(response["result"], json!({"accepted": true}));
        Ok(())
    }

    #[tokio::test]
    async fn malformed_and_invalid_requests_return_errors_and_transport_continues(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut harness =
            spawn_harness(|req, _| async move { ok_response(req.id, json!({"pong": true})) });
        harness.input.write_all(b"{not valid json\n").await?;
        write_json_line(
            &mut harness.input,
            json!({"jsonrpc":"2.0","id":2,"params":null}),
        )
        .await?;
        write_json_line(
            &mut harness.input,
            json!({"jsonrpc":"2.0","id":3,"method":"ping","params":null}),
        )
        .await?;

        let parse_error = read_value(&mut harness.output).await?;
        let invalid_request = read_value(&mut harness.output).await?;
        let valid_response = read_value(&mut harness.output).await?;
        assert_eq!(parse_error["id"], Value::Null);
        assert_eq!(parse_error["error"]["code"], json!(codes::PARSE_ERROR));
        assert_eq!(invalid_request["id"], json!(2));
        assert_eq!(
            invalid_request["error"]["code"],
            json!(codes::INVALID_REQUEST)
        );
        assert_eq!(valid_response["id"], json!(3));
        assert_eq!(valid_response["result"], json!({"pong": true}));
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_dispatch_returns_second_response_first(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut harness = spawn_harness(|req, _| async move {
            if req.id == json!(1) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            ok_response(req.id, json!({"done": true}))
        });
        write_json_line(
            &mut harness.input,
            json!({"jsonrpc":"2.0","id":1,"method":"slow","params":null}),
        )
        .await?;
        write_json_line(
            &mut harness.input,
            json!({"jsonrpc":"2.0","id":2,"method":"fast","params":null}),
        )
        .await?;

        let first = read_value(&mut harness.output).await?;
        let second = read_value(&mut harness.output).await?;
        assert_eq!(first["id"], json!(2));
        assert_eq!(second["id"], json!(1));
        Ok(())
    }

    #[tokio::test]
    async fn eof_terminates_serve_after_draining_response() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut harness =
            spawn_harness(|req, _| async move { ok_response(req.id, json!({"pong": true})) });
        write_json_line(
            &mut harness.input,
            json!({"jsonrpc":"2.0","id":1,"method":"ping","params":null}),
        )
        .await?;
        drop(harness.input);

        let response = read_value(&mut harness.output).await?;
        assert_eq!(response["id"], json!(1));
        let result = harness.serve_task.await?;
        assert!(result.is_ok());
        Ok(())
    }

    /// Two connections on one daemon-wide stream: a response envelope
    /// tagged with connection A's origin is written only by A. B's
    /// first output line is its own response — if A's response had
    /// leaked through B's filter, broadcast ordering would have put it
    /// first (spec mu-046 INV-8 per-connection filtering).
    #[tokio::test]
    async fn response_routes_only_to_originating_connection(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let stream = OutboundStream::new();
        let handler =
            |req: Request<Value>, _| async move { ok_response(req.id, json!({"pong": true})) };
        let mut conn_a = spawn_harness_on_stream(stream.clone(), handler);
        let mut conn_b = spawn_harness_on_stream(stream.clone(), handler);

        write_json_line(
            &mut conn_a.input,
            json!({"jsonrpc":"2.0","id":"for-a","method":"ping","params":null}),
        )
        .await?;
        let response_a = read_value(&mut conn_a.output).await?;
        assert_eq!(response_a["id"], json!("for-a"));

        // A's response is already on the stream; B subscribed before it
        // was sent, so if B failed to filter it, it would precede B's
        // own response in B's output.
        write_json_line(
            &mut conn_b.input,
            json!({"jsonrpc":"2.0","id":"for-b","method":"ping","params":null}),
        )
        .await?;
        let response_b = read_value(&mut conn_b.output).await?;
        assert_eq!(response_b["id"], json!("for-b"));
        Ok(())
    }

    /// An origin-less envelope (broadcast) reaches every connection on
    /// the stream — emitted via the `NotificationWriter::broadcast`
    /// constructor (spec mu-046 INV-8 fan-out).
    #[tokio::test]
    async fn broadcast_envelope_fans_out_to_all_connections(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let stream = OutboundStream::new();
        let handler =
            |req: Request<Value>, _| async move { ok_response(req.id, json!({"pong": true})) };
        let mut conn_a = spawn_harness_on_stream(stream.clone(), handler);
        let mut conn_b = spawn_harness_on_stream(stream.clone(), handler);

        // Prove both write loops are subscribed before broadcasting: a
        // delivered response means the connection's subscriber is live.
        write_json_line(
            &mut conn_a.input,
            json!({"jsonrpc":"2.0","id":1,"method":"ping","params":null}),
        )
        .await?;
        write_json_line(
            &mut conn_b.input,
            json!({"jsonrpc":"2.0","id":2,"method":"ping","params":null}),
        )
        .await?;
        read_value(&mut conn_a.output).await?;
        read_value(&mut conn_b.output).await?;

        let broadcast_writer = NotificationWriter::broadcast(stream.clone());
        broadcast_writer
            .emit("daemon.announce", json!({"msg": "hello"}))
            .await?;

        let seen_a = read_value(&mut conn_a.output).await?;
        let seen_b = read_value(&mut conn_b.output).await?;
        assert_eq!(seen_a["method"], json!("daemon.announce"));
        assert_eq!(seen_a["params"], json!({"msg": "hello"}));
        assert_eq!(seen_b["method"], json!("daemon.announce"));
        assert_eq!(seen_b["params"], json!({"msg": "hello"}));
        Ok(())
    }

    #[test]
    fn notification_writer_is_clone_send_and_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        fn assert_clone<T: Clone>() {}
        assert_send::<NotificationWriter>();
        assert_sync::<NotificationWriter>();
        assert_clone::<NotificationWriter>();
    }

    #[tokio::test]
    async fn id_is_preserved_as_value() -> Result<(), Box<dyn std::error::Error>> {
        for id in [json!("abc"), json!(7), Value::Null] {
            let mut harness =
                spawn_harness(|req, _| async move { ok_response(req.id, json!({"ok": true})) });
            write_json_line(
                &mut harness.input,
                json!({"jsonrpc":"2.0","id":id,"method":"echo","params":null}),
            )
            .await?;

            let response = read_value(&mut harness.output).await?;
            assert_eq!(response["id"], id);
        }
        Ok(())
    }
}
