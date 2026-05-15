//! mu-lho (mu-037 Phase 1) integration smoke: two sessions in the
//! same daemon exchange a mailbox message end-to-end via the JSON-RPC
//! wire surface.
//!
//! Mirrors `serve_smoke.rs`'s `tokio::io::duplex` harness.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;

use mu_ai::FauxProvider;
use mu_coding::serve;
use mu_core::agent::Provider;

fn spawn_server(
    provider: Arc<dyn Provider>,
) -> (
    tokio::io::DuplexStream,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let server_buf = BufReader::new(server_read);
    let factory: serve::ProviderFactory =
        std::sync::Arc::new(move |_selector| Ok(provider.clone()));
    let handle = tokio::spawn(serve::serve_with_io(
        server_buf,
        server_write,
        factory,
        Vec::new(),
        None,
    ));
    (client, handle)
}

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

async fn await_response<R: tokio::io::AsyncRead + Unpin>(reader: &mut R, id: i64) -> Value {
    timeout(Duration::from_millis(2000), async {
        loop {
            let line = read_line(reader).await;
            if line.get("id").and_then(|v| v.as_i64()) == Some(id) {
                return line;
            }
        }
    })
    .await
    .expect("response did not arrive within 2s")
}

/// Drain notifications until one matching `method` arrives, or
/// `timeout_ms` elapses. Returns the matching notification.
async fn await_notification<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    method: &str,
    timeout_ms: u64,
) -> Value {
    timeout(Duration::from_millis(timeout_ms), async {
        loop {
            let line = read_line(reader).await;
            if line.get("method").and_then(|v| v.as_str()) == Some(method) {
                return line;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("notification {method} did not arrive within {timeout_ms}ms"))
}

async fn create_session<W: tokio::io::AsyncWrite + Unpin>(client: &mut W, id: i64) {
    let req = json!({
        "jsonrpc": "2.0", "id": id, "method": "create_session",
        "params": { "provider": { "kind": "anthropic_api", "model": "x" } }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write");
}

/// mu-lho L1: full round-trip — A peer.hello → B accepts; A mailbox.post →
/// B mailbox.list sees it → B mailbox.consume → B re-lists, message gone.
#[tokio::test]
async fn l1_two_sessions_exchange_mailbox_message_round_trip() {
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server(provider);

    // Step 1: create A.
    create_session(&mut client, 1).await;
    let resp = await_response(&mut client, 1).await;
    let session_a = resp["result"]["session_id"].as_str().unwrap().to_string();

    // Step 2: create B.
    create_session(&mut client, 2).await;
    let resp = await_response(&mut client, 2).await;
    let session_b = resp["result"]["session_id"].as_str().unwrap().to_string();

    // Step 3: discover the daemon_id so we can fill `from.daemon_id`
    // honestly. The dispatch handler rejects mismatched daemon_id.
    let req = json!({
        "jsonrpc": "2.0", "id": 3, "method": "daemon.stats", "params": {}
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = await_response(&mut client, 3).await;
    let daemon_id = resp["result"]["daemon_id"].as_str().unwrap().to_string();

    // Step 4: peer.hello from A asking B for a mailbox.post handle.
    let req = json!({
        "jsonrpc": "2.0", "id": 4, "method": "peer.hello",
        "params": {
            "to_session_id": session_b,
            "from": {
                "daemon_id": daemon_id,
                "session_id": session_a,
                "advertised_capabilities": []
            },
            "want": { "method": "mailbox.post" }
        }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = await_response(&mut client, 4).await;
    assert_eq!(resp["result"]["outcome"], "accepted");
    let peer_handle = resp["result"]["peer_handle"].as_str().unwrap().to_string();
    assert_eq!(peer_handle.len(), 32);

    // Step 5: A posts a message to B.
    let req = json!({
        "jsonrpc": "2.0", "id": 5, "method": "mailbox.post",
        "params": {
            "to_session_id": session_b,
            "peer_handle": peer_handle,
            "from": {
                "daemon_id": daemon_id,
                "session_id": session_a
            },
            "kind": "fyi",
            "subject": "first contact",
            "body": { "note": "hi B from A" }
        }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();

    // Drain until we see both the response AND the wire notification.
    let mut saw_response: Option<Value> = None;
    let mut saw_notification: Option<Value> = None;
    timeout(Duration::from_millis(2000), async {
        while saw_response.is_none() || saw_notification.is_none() {
            let line = read_line(&mut client).await;
            if line.get("id").and_then(|v| v.as_i64()) == Some(5) {
                saw_response = Some(line);
            } else if line.get("method").and_then(|v| v.as_str()) == Some("session.mailbox_message")
            {
                saw_notification = Some(line);
            }
        }
    })
    .await
    .expect("mailbox.post response + notification did not both arrive");

    let resp = saw_response.unwrap();
    assert_eq!(resp["result"]["posted"], true);
    let seq = resp["result"]["seq"].as_u64().unwrap();
    assert_eq!(seq, 1);

    let notif = saw_notification.unwrap();
    assert_eq!(notif["params"]["session_id"], session_b);
    assert_eq!(notif["params"]["seq"], 1);
    assert_eq!(notif["params"]["from_session_id"], session_a);
    assert_eq!(notif["params"]["kind"], "fyi");
    assert_eq!(notif["params"]["subject"], "first contact");

    // Step 6: B lists its own mailbox (self-access; no handle needed).
    let req = json!({
        "jsonrpc": "2.0", "id": 6, "method": "mailbox.list",
        "params": { "session_id": session_b, "include_consumed": false }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = await_response(&mut client, 6).await;
    let messages = resp["result"]["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["seq"], 1);
    assert_eq!(messages[0]["consumed"], false);
    assert_eq!(messages[0]["subject"], "first contact");
    assert_eq!(messages[0]["body"]["note"], "hi B from A");

    // Step 7: B consumes the message.
    let req = json!({
        "jsonrpc": "2.0", "id": 7, "method": "mailbox.consume",
        "params": { "session_id": session_b, "seqs": [1] }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = await_response(&mut client, 7).await;
    assert_eq!(resp["result"]["consumed_count"], 1);

    // Step 8: B re-lists with include_consumed=false → empty.
    let req = json!({
        "jsonrpc": "2.0", "id": 8, "method": "mailbox.list",
        "params": { "session_id": session_b, "include_consumed": false }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = await_response(&mut client, 8).await;
    let messages = resp["result"]["messages"].as_array().unwrap();
    assert!(
        messages.is_empty(),
        "consumed message should be filtered out"
    );

    // Step 9: include_consumed=true returns the message as consumed.
    let req = json!({
        "jsonrpc": "2.0", "id": 9, "method": "mailbox.list",
        "params": { "session_id": session_b, "include_consumed": true }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = await_response(&mut client, 9).await;
    let messages = resp["result"]["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["consumed"], true);

    // Step 10: idempotent re-consume — consumed_count is 0 because
    // seq 1 is already consumed.
    let req = json!({
        "jsonrpc": "2.0", "id": 10, "method": "mailbox.consume",
        "params": { "session_id": session_b, "seqs": [1] }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = await_response(&mut client, 10).await;
    assert_eq!(resp["result"]["consumed_count"], 0);

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}

/// mu-lho L2: mailbox.post without a peer handle → rejected.
#[tokio::test]
async fn l2_mailbox_post_without_handle_is_rejected() {
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server(provider);

    // Create A and B.
    create_session(&mut client, 1).await;
    let resp = await_response(&mut client, 1).await;
    let session_a = resp["result"]["session_id"].as_str().unwrap().to_string();
    create_session(&mut client, 2).await;
    let resp = await_response(&mut client, 2).await;
    let session_b = resp["result"]["session_id"].as_str().unwrap().to_string();

    let req = json!({
        "jsonrpc": "2.0", "id": 3, "method": "daemon.stats", "params": {}
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = await_response(&mut client, 3).await;
    let daemon_id = resp["result"]["daemon_id"].as_str().unwrap().to_string();

    // Skip peer.hello — go straight to mailbox.post with a bogus handle.
    let req = json!({
        "jsonrpc": "2.0", "id": 4, "method": "mailbox.post",
        "params": {
            "to_session_id": session_b,
            "peer_handle": "00000000000000000000000000000000",
            "from": { "daemon_id": daemon_id, "session_id": session_a },
            "kind": "fyi",
            "subject": "unauthorized attempt",
            "body": {}
        }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = await_response(&mut client, 4).await;
    assert!(
        resp["error"].is_object(),
        "expected error response; got {resp:?}"
    );
    let msg = resp["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("invalid or expired peer handle"),
        "unexpected error: {msg}",
    );

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}

/// mu-lho L3: peer.hello with `want.method != "mailbox.post"` is
/// denied with a reasoned response (not an error). Phase 1 policy
/// only grants `mailbox.post` handles.
#[tokio::test]
async fn l3_peer_hello_unsupported_method_is_denied() {
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server(provider);

    create_session(&mut client, 1).await;
    let resp = await_response(&mut client, 1).await;
    let session_a = resp["result"]["session_id"].as_str().unwrap().to_string();
    create_session(&mut client, 2).await;
    let resp = await_response(&mut client, 2).await;
    let session_b = resp["result"]["session_id"].as_str().unwrap().to_string();

    let req = json!({
        "jsonrpc": "2.0", "id": 3, "method": "daemon.stats", "params": {}
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = await_response(&mut client, 3).await;
    let daemon_id = resp["result"]["daemon_id"].as_str().unwrap().to_string();

    let req = json!({
        "jsonrpc": "2.0", "id": 4, "method": "peer.hello",
        "params": {
            "to_session_id": session_b,
            "from": { "daemon_id": daemon_id, "session_id": session_a, "advertised_capabilities": [] },
            "want": { "method": "session.ask_session" }
        }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = await_response(&mut client, 4).await;
    assert_eq!(resp["result"]["outcome"], "denied");
    let reason = resp["result"]["reason"].as_str().unwrap();
    assert!(
        reason.contains("session.ask_session") && reason.contains("mailbox.post"),
        "denial should name the refused method and the offered one; got: {reason}",
    );

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}

#[allow(dead_code)]
async fn unused_notification_helper() {
    let _ = await_notification::<tokio::io::DuplexStream>;
}
