use std::future::Future;
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

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

/// Handle on a single shared outbound channel. Cheap to clone (Arc-y
/// internally). Pass into request handlers so they can emit
/// notifications mid-flight.
#[derive(Clone, Debug)]
pub struct NotificationWriter {
    tx: mpsc::Sender<Outbound>,
}

impl NotificationWriter {
    /// Emit a notification. Returns `Ok(())` even if the channel is
    /// closed — see §INV-5.
    pub async fn emit<P: Serialize>(&self, method: &str, params: P) -> Result<(), TransportError> {
        let params = serde_json::to_value(params)?;
        let notif = Notification {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.to_string(),
            params,
        };
        let value = serde_json::to_value(&notif)?;
        if self.tx.send(Outbound(value)).await.is_err() {
            tracing::warn!("notification dropped: outbound channel closed");
        }
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
pub async fn serve<R, W, F, Fut>(reader: R, writer: W, handler: F) -> Result<(), TransportError>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    F: Fn(Request<Value>, NotificationWriter) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<Value>> + Send + 'static,
{
    let (tx, rx) = mpsc::channel(64);
    let notif = NotificationWriter { tx: tx.clone() };
    let writer_task = tokio::spawn(write_loop(writer, rx));
    let handler = Arc::new(handler);
    let mut tasks = JoinSet::new();
    let mut lines = reader.lines();

    while let Some(line) = lines.next_line().await? {
        match parse_request_line(&line) {
            Ok(request) => {
                let handler = Arc::clone(&handler);
                let notif = notif.clone();
                let response_tx = tx.clone();
                tasks.spawn(async move {
                    let response = handler(request, notif).await;
                    match serde_json::to_value(response) {
                        Ok(value) => {
                            if response_tx.send(Outbound(value)).await.is_err() {
                                tracing::warn!("response dropped: outbound channel closed");
                            }
                        }
                        Err(err) => tracing::warn!(%err, "response serialization failed"),
                    }
                });
            }
            Err(response) => {
                let value = serde_json::to_value(response)?;
                if tx.send(Outbound(value)).await.is_err() {
                    return Err(TransportError::OutboundClosed);
                }
            }
        }
    }

    while let Some(result) = tasks.join_next().await {
        if let Err(err) = result {
            tracing::warn!(%err, "request handler task failed");
        }
    }

    drop(notif);
    drop(tx);

    match writer_task.await {
        Ok(result) => result,
        Err(err) => {
            tracing::warn!(%err, "writer task failed");
            Err(TransportError::OutboundClosed)
        }
    }
}

// ===== Internal =====

/// Anything destined for the outbound channel: a serialized Response
/// or Notification, already as a Value so it can be flushed without
/// re-borrowing the type. Pub(crate) only.
#[derive(Debug)]
pub(crate) struct Outbound(pub(crate) Value);

async fn write_loop<W>(
    mut writer: W,
    mut rx: mpsc::Receiver<Outbound>,
) -> Result<(), TransportError>
where
    W: AsyncWrite + Unpin,
{
    while let Some(Outbound(value)) = rx.recv().await {
        let line = serde_json::to_string(&value)?;
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
        let (input, server_reader) = duplex(64 * 1024);
        let (server_writer, output) = duplex(64 * 1024);
        let reader = BufReader::new(server_reader);
        let output = BufReader::new(output).lines();
        let serve_task = tokio::spawn(serve(reader, server_writer, handler));
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
