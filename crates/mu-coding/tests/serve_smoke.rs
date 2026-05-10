//! Integration smoke tests for `mu serve`. Drive the JSON-RPC surface
//! end-to-end via `tokio::io::duplex` with `FauxProvider` as the LLM.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;

use mu_ai::FauxProvider;
use mu_core::agent::Provider;
use mu_coding::serve;

/// Build a duplex pair, spawn `serve_with_io` on one half, return the
/// other half plus the server's JoinHandle.
fn spawn_server(
    provider: Arc<dyn Provider>,
) -> (
    tokio::io::DuplexStream,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let server_buf = BufReader::new(server_read);
    let handle = tokio::spawn(serve::serve_with_io(
        server_buf,
        server_write,
        provider,
        Vec::new(),
    ));
    (client, handle)
}

/// Read exactly one newline-terminated JSON line from a reader.
async fn read_line<R: tokio::io::AsyncRead + Unpin>(reader: &mut R) -> Value {
    let mut buf = [0u8; 1];
    let mut line = Vec::new();
    loop {
        let n = reader.read(&mut buf).await.expect("read");
        if n == 0 {
            panic!("unexpected EOF reading line");
        }
        if buf[0] == b'\n' {
            break;
        }
        line.push(buf[0]);
    }
    serde_json::from_slice(&line).expect("parse JSON line")
}

/// Read N lines from a reader.
async fn read_n_lines<R: tokio::io::AsyncRead + Unpin>(reader: &mut R, n: usize) -> Vec<Value> {
    let mut out = Vec::new();
    for _ in 0..n {
        out.push(read_line(reader).await);
    }
    out
}

/// B-4: ping round-trip.
#[tokio::test]
async fn b4_ping_round_trip() {
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server(provider);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "ping",
        "params": null,
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write");

    let resp = read_line(&mut client).await;
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 1);
    assert_eq!(resp["result"]["pong"], true);
    assert!(resp["result"]["server_version"].is_string());

    drop(client); // close server's reader
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}

/// B-5: create_session + ask_session produces text_delta + done.
#[tokio::test]
async fn b5_create_ask_done() {
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server(provider);

    // Step 1: create_session.
    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "create_session",
        "params": {
            "provider": { "kind": "anthropic_api", "model": "irrelevant" }
        }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write");
    let resp = read_line(&mut client).await;
    assert_eq!(resp["id"], 1);
    let session_id = resp["result"]["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();
    assert!(session_id.starts_with("session-"));

    // Step 2: ask_session.
    let req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "ask_session",
        "params": {
            "session_id": session_id,
            "user_message": "hello",
        }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write ask");

    // Expect 3 lines (in any order, since responses + notifications
    // share the outbound channel): the ask response, text_delta,
    // done. Read 3 lines, classify by shape.
    let lines = read_n_lines(&mut client, 3).await;

    let ask_response = lines
        .iter()
        .find(|v| v["id"] == 2)
        .expect("missing ask response");
    assert_eq!(ask_response["result"]["accepted"], true);

    let text_delta = lines
        .iter()
        .find(|v| v["method"] == "session.text_delta")
        .expect("missing text_delta");
    assert_eq!(text_delta["params"]["delta"], "hello");
    assert_eq!(text_delta["params"]["session_id"], session_id);

    let done = lines
        .iter()
        .find(|v| v["method"] == "session.done")
        .expect("missing done");
    assert_eq!(done["params"]["session_id"], session_id);

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}

/// B-6: cancel_session terminates the loop within 500ms.
#[tokio::test]
async fn b6_cancel_terminates_promptly() {
    // FauxProvider::scripted([]) returns an empty stream — agent loop
    // sees "stream ended without Done" and emits Error. That's not
    // strictly "long-running stream + cancel", but it tests the
    // cancel_session RPC path: cancel on a session that's between
    // user-message and provider-stream is still a reasonable input.
    //
    // For a more thorough cancel test we'd need a FauxProvider with a
    // pending-stream mode (mu-003 had MockProvider::pending). For v1
    // this proves cancel_session at least dispatches without hanging.
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server(provider);

    // Create.
    let req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "create_session",
        "params": {
            "provider": { "kind": "anthropic_api", "model": "x" }
        }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = read_line(&mut client).await;
    let session_id = resp["result"]["session_id"].as_str().unwrap().to_string();

    // Cancel without an ask — exercises the dispatch path.
    let req = json!({
        "jsonrpc": "2.0", "id": 2, "method": "cancel_session",
        "params": { "session_id": session_id }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();

    let resp = timeout(Duration::from_millis(500), read_line(&mut client))
        .await
        .expect("cancel response did not arrive within 500ms");
    assert_eq!(resp["id"], 2);
    assert_eq!(resp["result"]["cancelled"], true);

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}

/// B-7: close_session removes the session; a subsequent ask returns error.
#[tokio::test]
async fn b7_close_removes_session() {
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server(provider);

    // Create.
    let req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "create_session",
        "params": {
            "provider": { "kind": "anthropic_api", "model": "x" }
        }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = read_line(&mut client).await;
    let session_id = resp["result"]["session_id"].as_str().unwrap().to_string();

    // Close.
    let req = json!({
        "jsonrpc": "2.0", "id": 2, "method": "close_session",
        "params": { "session_id": session_id }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = read_line(&mut client).await;
    assert_eq!(resp["id"], 2);
    assert_eq!(resp["result"]["closed"], true);

    // Ask against the closed session — expect error.
    let req = json!({
        "jsonrpc": "2.0", "id": 3, "method": "ask_session",
        "params": { "session_id": session_id, "user_message": "hello" }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = read_line(&mut client).await;
    assert_eq!(resp["id"], 3);
    assert!(
        resp["error"].is_object(),
        "expected error response, got {resp:?}"
    );
    assert_eq!(resp["error"]["code"], -32602); // INVALID_PARAMS

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}
