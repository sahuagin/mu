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
    // Adapt the single Arc<dyn Provider> into a per-session factory
    // that just hands out clones — preserves the smoke-test semantic
    // (one provider for all sessions) under the new factory API.
    let factory: serve::ProviderFactory =
        std::sync::Arc::new(move |_selector| Ok(provider.clone()));
    // events_dir=None: integration tests do NOT write to disk
    // (mu-upb). Setting Some(<path>) would pollute the developer's
    // ~/.local/share/mu/events with test fixtures.
    let handle = tokio::spawn(serve::serve_with_io(
        server_buf,
        server_write,
        factory,
        Vec::new(),
        None,
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

    // Read lines until we've seen the ack, a text_delta, and a done.
    // The wire surface now also emits session.provider_status (mu-035)
    // and possibly other notifications between these, so a fixed-count
    // read is brittle. Use a target-set drain instead.
    let mut lines: Vec<Value> = Vec::new();
    let mut saw_ack = false;
    let mut saw_text_delta = false;
    let mut saw_done = false;
    while !(saw_ack && saw_text_delta && saw_done) {
        let line = read_line(&mut client).await;
        if line["id"] == 2 {
            saw_ack = true;
        }
        if line["method"] == "session.text_delta" {
            saw_text_delta = true;
        }
        if line["method"] == "session.done" {
            saw_done = true;
        }
        lines.push(line);
    }

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

/// B-8: session.stats returns a usable snapshot of the event log.
/// Verifies (a) the dispatch path is wired, (b) running an
/// ask_session populates ask_count / total_turn_count / event_count,
/// (c) the SessionCreated provenance shows up in provider_kind /
/// model. Usage will be None for FauxProvider (it doesn't report
/// usage), but the structure of the response is still validated.
#[tokio::test]
async fn b8_session_stats_after_ask() {
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server(provider);

    // Create with a real-ish provider selector so we can verify it
    // round-trips into the SessionCreated event.
    let req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "create_session",
        "params": {
            "provider": { "kind": "openrouter", "model": "test/model" }
        }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = read_line(&mut client).await;
    let session_id = resp["result"]["session_id"].as_str().unwrap().to_string();

    // Ask once.
    let req = json!({
        "jsonrpc": "2.0", "id": 2, "method": "ask_session",
        "params": { "session_id": session_id, "user_message": "hi" }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    // Drain until we see session.done (the post mu-035 wire surface
    // additionally emits session.provider_status notifications;
    // fixed-count read is brittle).
    loop {
        let line = read_line(&mut client).await;
        if line["method"] == "session.done" {
            break;
        }
    }

    // Query stats.
    let req = json!({
        "jsonrpc": "2.0", "id": 3, "method": "session.stats",
        "params": { "session_id": session_id }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = read_line(&mut client).await;
    assert_eq!(resp["id"], 3);
    let result = &resp["result"];
    assert_eq!(result["session_id"], session_id);
    assert_eq!(result["provider_kind"], "openrouter");
    assert_eq!(result["model"], "test/model");
    // Event count: SessionCreated + UserMessage(MessageEnd) +
    // AssistantMessage(MessageEnd) + Done = at least 3 (UserMessage
    // may or may not arrive depending on how the loop turns out).
    let event_count = result["event_count"].as_u64().expect("event_count is u64");
    assert!(event_count >= 3, "event_count too low: {event_count}");
    assert_eq!(result["ask_count"], 1);
    assert!(result["total_turn_count"].as_u64().unwrap_or(0) >= 1);
    // Timestamps are present.
    assert!(result["started_at_unix_ms"].is_number());
    assert!(result["last_activity_unix_ms"].is_number());
    let started = result["started_at_unix_ms"].as_u64().unwrap();
    let last = result["last_activity_unix_ms"].as_u64().unwrap();
    assert!(last >= started, "last < started: {last} vs {started}");

    // Stats query on a missing session is INVALID_PARAMS.
    let req = json!({
        "jsonrpc": "2.0", "id": 4, "method": "session.stats",
        "params": { "session_id": "nonexistent-session" }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = read_line(&mut client).await;
    assert_eq!(resp["id"], 4);
    assert_eq!(resp["error"]["code"], -32602);

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}

/// B-9: session.delegate creates a child session that references its
/// parent. Verifies (a) the RPC is wired, (b) the child gets a fresh
/// session_id distinct from the parent's, (c) child can be queried
/// independently (its own event log + stats), (d) delegating to a
/// nonexistent parent returns INVALID_PARAMS.
#[tokio::test]
async fn b9_session_delegate_creates_child() {
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server(provider);

    // 1. Create the parent.
    let req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "create_session",
        "params": {
            "provider": { "kind": "openrouter", "model": "parent/model" }
        }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = read_line(&mut client).await;
    let parent_id = resp["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    // 2. Delegate a child with a different provider selector.
    let req = json!({
        "jsonrpc": "2.0", "id": 2, "method": "session.delegate",
        "params": {
            "parent_session_id": parent_id,
            "provider": { "kind": "anthropic_api", "model": "child/model" }
        }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = read_line(&mut client).await;
    assert_eq!(resp["id"], 2);
    let child_id = resp["result"]["child_session_id"]
        .as_str()
        .expect("child_session_id")
        .to_string();
    assert!(child_id.starts_with("session-"));
    assert_ne!(child_id, parent_id);

    // 3. Query child's stats — provider_kind/model should reflect
    //    the child's selector, NOT the parent's.
    let req = json!({
        "jsonrpc": "2.0", "id": 3, "method": "session.stats",
        "params": { "session_id": child_id }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = read_line(&mut client).await;
    assert_eq!(resp["id"], 3);
    let result = &resp["result"];
    assert_eq!(result["provider_kind"], "anthropic_api");
    assert_eq!(result["model"], "child/model");
    // Child's event log has the SessionCreated event.
    assert!(result["event_count"].as_u64().unwrap_or(0) >= 1);

    // 4. Delegate to a missing parent should fail.
    let req = json!({
        "jsonrpc": "2.0", "id": 4, "method": "session.delegate",
        "params": {
            "parent_session_id": "session-does-not-exist",
            "provider": { "kind": "openrouter", "model": "x" }
        }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = read_line(&mut client).await;
    assert_eq!(resp["id"], 4);
    assert_eq!(resp["error"]["code"], -32602);

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}

// ── mu-038: projection-query round-trips ──────────────────────────

/// Helper: skim incoming lines until we find the response with `id`,
/// drop everything else (notifications). Times out at 500ms.
async fn await_response<R: tokio::io::AsyncRead + Unpin>(reader: &mut R, id: i64) -> Value {
    timeout(Duration::from_millis(500), async {
        loop {
            let line = read_line(reader).await;
            if line.get("id").and_then(|v| v.as_i64()) == Some(id) {
                return line;
            }
        }
    })
    .await
    .expect("response did not arrive within 500ms")
}

/// B-10: session.list returns sessions from this daemon, with derived
/// status + the daemon's stable id, sorted most-recent-first.
#[tokio::test]
async fn b10_session_list_round_trip() {
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server(provider);

    // Create two sessions.
    let mut session_ids: Vec<String> = Vec::new();
    for i in 1..=2 {
        let req = json!({
            "jsonrpc": "2.0", "id": i, "method": "create_session",
            "params": { "provider": { "kind": "anthropic_api", "model": "x" } }
        });
        client
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
        let resp = await_response(&mut client, i).await;
        session_ids.push(resp["result"]["session_id"].as_str().unwrap().to_string());
    }

    // session.list with default filter.
    let req = json!({
        "jsonrpc": "2.0", "id": 10, "method": "session.list", "params": {}
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = await_response(&mut client, 10).await;
    assert!(resp["error"].is_null());
    let list = resp["result"]["sessions"]
        .as_array()
        .expect("sessions array")
        .clone();
    assert_eq!(list.len(), 2);
    // daemon_id present and identical across both rows.
    let daemon_ids: Vec<&str> = list
        .iter()
        .map(|s| s["daemon_id"].as_str().unwrap())
        .collect();
    assert_eq!(daemon_ids[0], daemon_ids[1]);
    assert!(!daemon_ids[0].is_empty());
    // is_remote always false for the LocalRegistry backend.
    for s in &list {
        assert_eq!(s["is_remote"], false);
    }
    // status: provider/model present.
    for s in &list {
        assert_eq!(s["provider_kind"], "anthropic_api");
        assert_eq!(s["model"], "x");
    }
    // Both session ids appear.
    let ids: std::collections::HashSet<String> = list
        .iter()
        .map(|s| s["session_id"].as_str().unwrap().to_string())
        .collect();
    for sid in &session_ids {
        assert!(ids.contains(sid), "missing {sid}");
    }
    assert!(resp["result"]["snapshot_at_unix_ms"].as_u64().unwrap() > 0);

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}

/// B-11: session.events paginates and reflects the recorded log.
#[tokio::test]
async fn b11_session_events_round_trip() {
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server(provider);

    // Create + ask.
    let req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "create_session",
        "params": { "provider": { "kind": "anthropic_api", "model": "x" } }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = await_response(&mut client, 1).await;
    let session_id = resp["result"]["session_id"].as_str().unwrap().to_string();

    let req = json!({
        "jsonrpc": "2.0", "id": 2, "method": "ask_session",
        "params": { "session_id": session_id, "user_message": "hello" }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    // Drain the ask response + notifications until we see session.done.
    let _ = timeout(Duration::from_millis(500), async {
        loop {
            let line = read_line(&mut client).await;
            if line.get("method").and_then(|v| v.as_str()) == Some("session.done") {
                return;
            }
        }
    })
    .await
    .expect("session.done");

    // session.events: full log.
    let req = json!({
        "jsonrpc": "2.0", "id": 20, "method": "session.events",
        "params": { "session_id": session_id }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = await_response(&mut client, 20).await;
    let events = resp["result"]["events"].as_array().expect("events").clone();
    assert!(!events.is_empty(), "expected non-empty event log");

    // First event is SessionCreated.
    let first = &events[0];
    assert_eq!(first["payload"]["kind"], "session_created");
    assert_eq!(first["payload"]["provider_kind"], "anthropic_api");

    // kinds_filter restricts shape.
    let req = json!({
        "jsonrpc": "2.0", "id": 21, "method": "session.events",
        "params": { "session_id": session_id, "kinds_filter": ["user_message"] }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = await_response(&mut client, 21).await;
    let events = resp["result"]["events"].as_array().expect("events").clone();
    for e in &events {
        assert_eq!(e["payload"]["kind"], "user_message");
    }

    // Unknown session id → INVALID_PARAMS.
    let req = json!({
        "jsonrpc": "2.0", "id": 22, "method": "session.events",
        "params": { "session_id": "session-does-not-exist" }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = await_response(&mut client, 22).await;
    assert_eq!(resp["error"]["code"], -32602);

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}

/// B-12: daemon.stats reflects session creation + ask counts.
#[tokio::test]
async fn b12_daemon_stats_round_trip() {
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server(provider);

    // Baseline.
    let req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "daemon.stats", "params": {}
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = await_response(&mut client, 1).await;
    let stats = &resp["result"];
    assert_eq!(stats["session_count"], 0);
    assert_eq!(stats["total_events"], 0);
    assert!(stats["daemon_id"].as_str().unwrap().len() == 16);
    assert!(stats["version"].as_str().is_some());

    // Create a session.
    let req = json!({
        "jsonrpc": "2.0", "id": 2, "method": "create_session",
        "params": { "provider": { "kind": "anthropic_api", "model": "x" } }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let _ = await_response(&mut client, 2).await;

    // Stats again — session_count++.
    let req = json!({
        "jsonrpc": "2.0", "id": 3, "method": "daemon.stats", "params": {}
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = await_response(&mut client, 3).await;
    let stats = &resp["result"];
    assert_eq!(stats["session_count"], 1);
    assert!(stats["total_events"].as_u64().unwrap() >= 1); // SessionCreated

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}
