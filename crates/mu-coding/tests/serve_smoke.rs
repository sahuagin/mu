//! Integration smoke tests for `mu serve`. Drive the JSON-RPC surface
//! end-to-end via `tokio::io::duplex` with `FauxProvider` as the LLM.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;

use mu_ai::{FauxProvider, FauxResponse};
use mu_coding::serve;
use mu_core::agent::{
    AssistantMessage, ContentBlock, Provider, ProviderEvent, StopReason, Tool, ToolArgs, ToolCall,
};
use mu_core::config::{AuthConfig, Config, IndexConfig};

/// Shared bearer token used by the test harness. The dispatcher's
/// mu-fnn enforcement gate (mu-7rk-c) rejects every protected RPC
/// against an unauthenticated connection, so the harness authenticates
/// during `spawn_server` and returns an already-authed client.
const TEST_BEARER_TOKEN: &str = "smoke-test-token";

/// Build a duplex pair, spawn `serve_with_io_with_config` with a
/// bearer-token allowlist, authenticate the client, and return the
/// authenticated client + server JoinHandle. Tests can then issue
/// protected RPCs against the returned client without further
/// handshaking.
async fn spawn_server(
    provider: Arc<dyn Provider>,
) -> (
    tokio::io::DuplexStream,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    spawn_server_with_tools(provider, Vec::new()).await
}

/// Like [`spawn_server`], but seeds the daemon with a session tool set.
/// `tools` is handed to every session the daemon builds, so tests can
/// exercise tool-dispatch paths — e.g. the in-loop `discover` tool
/// (mu-onq8), which the daemon registers per session alongside whatever
/// base tools are configured here.
async fn spawn_server_with_tools(
    provider: Arc<dyn Provider>,
    tools: Vec<Arc<dyn Tool>>,
) -> (
    tokio::io::DuplexStream,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let config = Config {
        auth: AuthConfig::Bearer {
            tokens: vec![TEST_BEARER_TOKEN.to_string()],
        },
        ..Default::default()
    };
    spawn_server_with_config(provider, tools, config).await
}

/// Like [`spawn_server_with_tools`], but takes an explicit `Config` so a test
/// can exercise config-driven daemon-startup behavior — e.g. mu-re0s's
/// `[index].lsp_addr`, which makes the daemon connect to a code-index LSP at
/// startup and register the `index_recall` tool. The caller must set `auth` to
/// the bearer token the harness authenticates with (`TEST_BEARER_TOKEN`).
async fn spawn_server_with_config(
    provider: Arc<dyn Provider>,
    tools: Vec<Arc<dyn Tool>>,
    config: Config,
) -> (
    tokio::io::DuplexStream,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    // events_dir=None: integration tests do NOT write to disk (mu-upb).
    // Setting Some(<path>) would pollute the developer's
    // ~/.local/share/mu/events with test fixtures.
    spawn_server_full(provider, tools, config, None).await
}

/// Like [`spawn_server_with_config`] but with an explicit `events_dir`.
///
/// mu-wnsp: the daemon-shutdown lifecycle test (a mu-qc08 regression guard)
/// needs `events_dir = Some(..)`, because the per-session `spawn_worker`
/// tool — the structure whose strong `Sessions` clone caused the shutdown
/// deadlock — is injected ONLY in production (events_dir set, see
/// `session_spawn_tools`). With `None` that tool never exists and the
/// deadlock cannot reproduce, which is precisely why the other smoke tests
/// missed it.
async fn spawn_server_full(
    provider: Arc<dyn Provider>,
    tools: Vec<Arc<dyn Tool>>,
    config: Config,
    events_dir: Option<std::path::PathBuf>,
) -> (
    tokio::io::DuplexStream,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let server_buf = BufReader::new(server_read);
    // Adapt the single Arc<dyn Provider> into a per-session factory
    // that just hands out clones — preserves the smoke-test semantic
    // (one provider for all sessions) under the new factory API.
    let factory: serve::ProviderFactory =
        std::sync::Arc::new(move |_selector| Ok(provider.clone()));
    let handle = tokio::spawn(serve::serve_with_io_with_config(
        server_buf,
        server_write,
        factory,
        tools,
        events_dir,
        config,
    ));
    authenticate(&mut client).await;
    (client, handle)
}

/// Perform the BEARER handshake on `client` so subsequent RPCs pass
/// the mu-fnn enforcement gate. Panics on any non-success — the
/// happy-path here is contract, not the test under verification.
async fn authenticate(client: &mut tokio::io::DuplexStream) {
    let req = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "peer.auth_initiate",
        "params": {
            "mechanism": "bearer",
            "initial_response": TEST_BEARER_TOKEN,
        },
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("auth write");
    let resp = read_line(client).await;
    assert_eq!(resp["id"], 0, "auth response id mismatch: {resp}");
    assert_eq!(
        resp["result"]["outcome"], "accepted",
        "auth handshake did not accept the test token: {resp}",
    );
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

/// B-4: ping round-trip.
#[tokio::test]
async fn b4_ping_round_trip() {
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server(provider).await;

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
    let (mut client, server_handle) = spawn_server(provider).await;

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
    let (mut client, server_handle) = spawn_server(provider).await;

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
    let (mut client, server_handle) = spawn_server(provider).await;

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
    let (mut client, server_handle) = spawn_server(provider).await;

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
    let (mut client, server_handle) = spawn_server(provider).await;

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
    let parent_id = resp["result"]["session_id"].as_str().unwrap().to_string();

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
    let (mut client, server_handle) = spawn_server(provider).await;

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
    let (mut client, server_handle) = spawn_server(provider).await;

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
    timeout(Duration::from_millis(500), async {
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
    let (mut client, server_handle) = spawn_server(provider).await;

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

/// mu-035 Phase D: `daemon.outstanding_calls` returns an empty list
/// when no sessions exist. Exercises the dispatch wiring + response
/// envelope without depending on agent-loop timing.
#[tokio::test]
async fn b13_daemon_outstanding_calls_empty_when_no_sessions() {
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server(provider).await;

    let req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "daemon.outstanding_calls", "params": {}
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = await_response(&mut client, 1).await;
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["result"]["calls"].as_array().unwrap().len(), 0);
    assert!(resp["result"]["snapshot_at_unix_ms"].as_u64().unwrap() > 0);

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}

/// mu-035 Phase D: after an echo-ask completes, the forwarder has
/// cleared the tracker on `AgentEvent::Done`. So
/// `daemon.outstanding_calls` should return an empty list even though
/// the session is still registered. This verifies the forwarder's
/// clear-on-done wiring, not just the dispatch handler.
#[tokio::test]
async fn b13_daemon_outstanding_calls_empty_after_ask_completes() {
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server(provider).await;

    // Create.
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

    // Ask.
    let req = json!({
        "jsonrpc": "2.0", "id": 2, "method": "ask_session",
        "params": { "session_id": session_id, "user_message": "hello" }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();

    // Drain notifications until we see session.done (forwarder has
    // observed the AgentEvent::Done by the time the wire-side
    // session.done is emitted — both are produced from the same
    // event-loop tick).
    timeout(Duration::from_millis(2000), async {
        loop {
            let line = read_line(&mut client).await;
            if line["method"] == "session.done" {
                break;
            }
        }
    })
    .await
    .expect("session.done did not arrive within 2s");

    // Now query outstanding_calls — tracker should be cleared.
    let req = json!({
        "jsonrpc": "2.0", "id": 3, "method": "daemon.outstanding_calls", "params": {}
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = await_response(&mut client, 3).await;
    let calls = resp["result"]["calls"].as_array().unwrap();
    assert!(
        calls.is_empty(),
        "expected empty after Done cleared tracker; got {calls:?}",
    );

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}

/// mu-035 Phase D: `session.cancel_outstanding` on an idle session
/// (no provider call in flight) should return `was_in: "idle"` and
/// `canceled: false`. Pre-Phase-D this returned `awaiting_first_token`
/// as a placeholder regardless of state; this test guards against
/// regressing to that placeholder.
#[tokio::test]
async fn b13_cancel_outstanding_on_idle_session_reports_idle() {
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server(provider).await;

    // Create — session is idle (no ask yet).
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

    // Cancel-outstanding on an idle session.
    let req = json!({
        "jsonrpc": "2.0", "id": 2, "method": "session.cancel_outstanding",
        "params": { "session_id": session_id }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = await_response(&mut client, 2).await;
    assert_eq!(resp["result"]["was_in"], "idle");
    assert_eq!(resp["result"]["canceled"], false);

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}

/// B-8: assistant_text_finalized notification is emitted when streaming
/// completes, with the finalized text that matches what's in the durable
/// event log. See mu-wk2.
#[tokio::test]
async fn b8_assistant_text_finalized_on_stream_complete() {
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server(provider).await;

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
    let session_id = resp["result"]["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();

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

    // Read lines until we've seen all expected events: ack, text_delta,
    // assistant_text_finalized, and done. Collect them for verification.
    let mut lines: Vec<Value> = Vec::new();
    let mut saw_ack = false;
    let mut saw_text_delta = false;
    let mut saw_finalized = false;
    let mut saw_done = false;
    while !(saw_ack && saw_text_delta && saw_finalized && saw_done) {
        let line = read_line(&mut client).await;
        if line["id"] == 2 {
            saw_ack = true;
        }
        if line["method"] == "session.text_delta" {
            saw_text_delta = true;
        }
        if line["method"] == "session.assistant_text_finalized" {
            saw_finalized = true;
        }
        if line["method"] == "session.done" {
            saw_done = true;
        }
        lines.push(line);
    }

    // Verify the finalized event is present and has the correct structure.
    let finalized = lines
        .iter()
        .find(|v| v["method"] == "session.assistant_text_finalized")
        .expect("missing assistant_text_finalized");
    assert_eq!(finalized["params"]["session_id"], session_id);
    // FauxProvider::echo() echoes back the user message, so the finalized
    // text should be "hello".
    assert_eq!(finalized["params"]["text"], "hello");

    // Verify that finalized is emitted before done.
    let finalized_idx = lines
        .iter()
        .position(|v| v["method"] == "session.assistant_text_finalized")
        .expect("finalized index");
    let done_idx = lines
        .iter()
        .position(|v| v["method"] == "session.done")
        .expect("done index");
    assert!(
        finalized_idx < done_idx,
        "assistant_text_finalized should arrive before session.done"
    );

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}

/// mu-x83o: create_session accepts and honors a `system_prompt`
/// param (the field has existed in CreateSessionRequest since
/// mu-n48; this test pins the wire contract so the
/// `--append-system-prompt` CLI flag has a guarantee to lean on).
/// FauxProvider ignores system_prompt at the provider boundary, so
/// what this test proves is the *handler* path: params parse, the
/// session builds without INVALID_PARAMS, ask_session still works.
#[tokio::test]
async fn b14_create_session_with_system_prompt() {
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server(provider).await;

    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "create_session",
        "params": {
            "provider": { "kind": "anthropic_api", "model": "irrelevant" },
            "system_prompt": "you are a careful assistant"
        }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write");
    let resp = read_line(&mut client).await;
    assert_eq!(resp["id"], 1);
    assert!(
        resp.get("error").is_none(),
        "create_session errored: {resp}"
    );
    let session_id = resp["result"]["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();
    assert!(session_id.starts_with("session-"));

    // Round-trip an ask to confirm the session is fully alive.
    let req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "ask_session",
        "params": {
            "session_id": session_id,
            "user_message": "ping",
        }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write ask");

    let mut saw_ack = false;
    let mut saw_done = false;
    while !(saw_ack && saw_done) {
        let line = read_line(&mut client).await;
        if line["id"] == 2 {
            saw_ack = true;
        }
        if line["method"] == "session.done" {
            saw_done = true;
        }
    }

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}

/// mu-onq8: the in-loop `discover` tool is exposed to the agent and is
/// invokable end-to-end. `capabilities/discover` (mu-kex4.6.4) ranks a
/// session's permission-attenuated manifest by intent — but only over
/// the RPC surface. The in-loop agent's only path to that ranking was a
/// bash shell-out the strict allowlist blocks. mu-onq8 registered a
/// native `DiscoverTool` per session in `build_and_register_session`.
///
/// This drives a faux-scripted `discover` tool call through the real
/// daemon session path and asserts the agent invokes the native tool
/// and it completes successfully — i.e. discovery is a first-class
/// in-loop tool, not a blocked bash call. The acceptance smoke named in
/// the mu-onq8 bead.
#[tokio::test]
async fn discover_tool_invoked_in_loop_mu_onq8() {
    use mu_coding::tools::ReadTool;

    // Two scripted turns: (1) the model calls `discover` with an intent
    // that should rank the session's `read` tool; (2) once the tool
    // result returns, the model emits text and ends the turn.
    let discover_call = ProviderEvent::Done(AssistantMessage {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: "tc-discover-1".into(),
            name: "discover".into(),
            arguments: ToolArgs::new(json!({ "intent": "read a file from disk" }))
                .expect("valid tool args"),
        })],
        stop_reason: StopReason::ToolUse,
        usage: None,
    });
    let final_turn = vec![
        ProviderEvent::TextDelta("found it".into()),
        ProviderEvent::Done(AssistantMessage {
            content: vec![ContentBlock::Text {
                text: "found it".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: None,
        }),
    ];
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::scripted(vec![
        FauxResponse::Script(vec![discover_call]),
        FauxResponse::Script(final_turn),
    ]));

    // Seed the daemon with a real sibling tool so `discover` has
    // something to rank. The daemon additionally registers `discover`
    // itself per session (the mu-onq8 wiring under test).
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(ReadTool::new())];
    let (mut client, server_handle) = spawn_server_with_tools(provider, tools).await;

    // create_session (root → unrestricted capability, so `read` is in
    // the discoverable manifest).
    let req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "create_session",
        "params": { "provider": { "kind": "anthropic_api", "model": "irrelevant" } }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write create");
    let resp = read_line(&mut client).await;
    let session_id = resp["result"]["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();

    // ask_session — the scripted provider responds with a `discover` call.
    let req = json!({
        "jsonrpc": "2.0", "id": 2, "method": "ask_session",
        "params": { "session_id": session_id, "user_message": "what can I use to read a file?" }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write ask");

    // Drain until we've seen the discover tool start, its completion (by
    // id), and the terminal done.
    let mut started: Option<Value> = None;
    let mut completed: Option<Value> = None;
    let mut saw_done = false;
    while !(started.is_some() && completed.is_some() && saw_done) {
        let line = read_line(&mut client).await;
        match line["method"].as_str() {
            Some("session.tool_call_started") if line["params"]["tool_name"] == "discover" => {
                started = Some(line.clone());
            }
            Some("session.tool_call_completed")
                if line["params"]["tool_call_id"] == "tc-discover-1" =>
            {
                completed = Some(line.clone());
            }
            Some("session.done") => saw_done = true,
            _ => {}
        }
    }

    let started = started.expect("discover tool_call_started");
    assert_eq!(started["params"]["tool_name"], "discover");
    assert_eq!(started["params"]["session_id"], session_id);

    // The discover call must SUCCEED in-loop (outcome kind == "ok"),
    // proving the agent used the native tool rather than the blocked
    // bash path.
    let completed = completed.expect("discover tool_call_completed");
    assert_eq!(
        completed["params"]["outcome"]["kind"], "ok",
        "discover should complete ok, got: {completed}"
    );
    let result = completed["params"]["outcome"]["result"]
        .as_str()
        .expect("ok result is a string");
    // The ranked manifest should surface the session's `read` tool.
    assert!(
        result.contains("read"),
        "discover result should rank the read tool: {result:?}"
    );

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}

/// Minimal stub code-index LSP server for the mu-re0s test. Binds
/// 127.0.0.1:0, accepts ONE connection, and speaks just enough LSP
/// (Content-Length framed JSON-RPC) for `mu_core::lsp_client::LspClient`:
/// answers `initialize` and returns a single fixed symbol for any
/// `workspace/symbol` query. Returns the bound address; the server task runs
/// until the client connection closes.
async fn spawn_stub_index_lsp(symbol_name: &'static str) -> std::net::SocketAddr {
    use tokio::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stub lsp");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        let (sock, _) = match listener.accept().await {
            Ok(p) => p,
            Err(_) => return,
        };
        let (read, mut write) = sock.into_split();
        let mut reader = BufReader::new(read);
        loop {
            // Read one Content-Length framed message.
            let mut header = String::new();
            if reader.read_line(&mut header).await.unwrap_or(0) == 0 {
                break; // EOF
            }
            let len: usize = match header.trim().strip_prefix("Content-Length:") {
                Some(n) => n.trim().parse().unwrap_or(0),
                None => continue,
            };
            let mut sep = String::new(); // blank separator line
            let _ = reader.read_line(&mut sep).await;
            let mut body = vec![0u8; len];
            if reader.read_exact(&mut body).await.is_err() {
                break;
            }
            let msg: Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let id = msg.get("id").cloned();
            let result = match method {
                "initialize" => {
                    Some(json!({"capabilities": {}, "serverInfo": {"name": "stub-index"}}))
                }
                "workspace/symbol" => Some(json!([{
                    "name": symbol_name,
                    "kind": 12,
                    "location": {
                        "uri": "file:///repo/src/lib.rs",
                        "range": {
                            "start": {"line": 41, "character": 0},
                            "end": {"line": 60, "character": 0}
                        }
                    }
                }])),
                "shutdown" => Some(Value::Null),
                _ => None, // notifications (initialized / exit) get no response
            };
            if let (Some(id), Some(result)) = (id, result) {
                let resp = json!({"jsonrpc": "2.0", "id": id, "result": result});
                let body = serde_json::to_string(&resp).expect("serialize stub resp");
                let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
                if write.write_all(framed.as_bytes()).await.is_err() {
                    break;
                }
                let _ = write.flush().await;
            }
        }
    });
    addr
}

/// mu-re0s: the `index_recall` tool (code-index / code_recall search — the
/// highest-value instance of Friction B) is wired into the in-loop agent when
/// `[index].lsp_addr` is configured. The tool exists in the registry but was
/// never constructed/registered anywhere; mu-re0s connects to the code-index
/// LSP at daemon startup and registers it.
///
/// This drives the full path: config → daemon connects to a stub LSP →
/// registers `index_recall` → a scripted agent invokes it → LSP round-trip →
/// result. Asserts the agent invokes the native tool (not a grep fallback) and
/// it completes ok with the LSP's symbol.
#[tokio::test]
async fn index_recall_tool_wired_from_config_mu_re0s() {
    // Stub code-index LSP returning a known symbol for any query.
    let lsp_addr = spawn_stub_index_lsp("build_and_register_session").await;

    // FauxProvider scripted to call index_recall, then end the turn.
    let call = ProviderEvent::Done(AssistantMessage {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: "tc-idx-1".into(),
            name: "index_recall".into(),
            arguments: ToolArgs::new(json!({"query": "where are sessions built", "limit": 5}))
                .expect("valid tool args"),
        })],
        stop_reason: StopReason::ToolUse,
        usage: None,
    });
    let final_turn = vec![
        ProviderEvent::TextDelta("done".into()),
        ProviderEvent::Done(AssistantMessage {
            content: vec![ContentBlock::Text {
                text: "done".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: None,
        }),
    ];
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::scripted(vec![
        FauxResponse::Script(vec![call]),
        FauxResponse::Script(final_turn),
    ]));

    // Config points the daemon at the stub LSP → connect at startup + register
    // index_recall (mu-re0s). No base tools needed; the daemon adds the tool.
    let config = Config {
        auth: AuthConfig::Bearer {
            tokens: vec![TEST_BEARER_TOKEN.to_string()],
        },
        index: IndexConfig {
            lsp_addr: Some(lsp_addr.to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    let (mut client, server_handle) = spawn_server_with_config(provider, Vec::new(), config).await;

    // create_session
    let req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "create_session",
        "params": { "provider": { "kind": "anthropic_api", "model": "irrelevant" } }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write create");
    let resp = read_line(&mut client).await;
    let session_id = resp["result"]["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();

    // ask_session — the scripted provider responds with an index_recall call.
    let req = json!({
        "jsonrpc": "2.0", "id": 2, "method": "ask_session",
        "params": { "session_id": session_id, "user_message": "where is a session built?" }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write ask");

    let mut started: Option<Value> = None;
    let mut completed: Option<Value> = None;
    let mut saw_done = false;
    while !(started.is_some() && completed.is_some() && saw_done) {
        let line = read_line(&mut client).await;
        match line["method"].as_str() {
            Some("session.tool_call_started") if line["params"]["tool_name"] == "index_recall" => {
                started = Some(line.clone());
            }
            Some("session.tool_call_completed") if line["params"]["tool_call_id"] == "tc-idx-1" => {
                completed = Some(line.clone());
            }
            Some("session.done") => saw_done = true,
            _ => {}
        }
    }

    let started = started.expect("index_recall tool_call_started");
    assert_eq!(started["params"]["tool_name"], "index_recall");
    assert_eq!(started["params"]["session_id"], session_id);

    // Must complete ok (outcome.kind == "ok"), proving the daemon connected to
    // the LSP, registered the tool, and the agent invoked it in-loop.
    let completed = completed.expect("index_recall tool_call_completed");
    assert_eq!(
        completed["params"]["outcome"]["kind"], "ok",
        "index_recall should complete ok, got: {completed}"
    );
    let result = completed["params"]["outcome"]["result"]
        .as_str()
        .expect("ok result is a string");
    // The LSP's symbol must surface in the tool result.
    assert!(
        result.contains("build_and_register_session"),
        "index_recall result should contain the LSP symbol: {result:?}"
    );

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}

/// A unique throwaway directory under the system temp dir for the
/// lifecycle test's `events_dir`. Avoids a `tempfile` dev-dep (none is
/// configured) and avoids polluting the developer's `~/.local/share/mu`.
/// Uniqueness = pid + a process-local counter, enough to keep parallel
/// tests from colliding on the same path.
fn unique_events_dir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("mu-wnsp-lifecycle-{}-{}", std::process::id(), n))
}

/// mu-wnsp / mu-qc08 regression: `mu serve` must exit cleanly on stdin-EOF
/// even when a live session's agent loop holds the per-session
/// `spawn_worker` tool.
///
/// Before the fix, `SpawnWorkerTool` held a STRONG `Sessions` clone. The
/// agent loop owns its tools, so that clone kept the registry map — and
/// thus the loop's own `input_tx` — alive; on stdin-EOF the input channel
/// never closed, `input_rx.recv()` never returned `None`, the loop never
/// exited, and `serve_with_io_with_config` hung forever. `mu ask` then
/// SIGKILLed the daemon after 5s and exited non-zero, which made
/// agent-spawn misreport every worker as failed (mu-qc08, fixed by holding
/// a `Weak<Sessions>` in PR #150).
///
/// The other smoke tests run with `events_dir = None`, so the spawn tool is
/// never injected and the deadlock cannot reproduce — the whole
/// shutdown-lifecycle test class was absent. This closes that gap:
/// `events_dir = Some(..)` to inject the tool, a real created session to
/// hold it, then stdin-EOF, then ASSERT the server future returns within
/// the same 5s window `mu ask` uses. A strong clone times out here; the
/// `Weak` fix exits promptly. (Verified to fail on a re-pinned registry —
/// see the bead's verification note.)
#[tokio::test]
async fn serve_exits_on_eof_with_spawn_worker_session_mu_qc08() {
    // A real (throwaway) events dir so `session_spawn_tools` actually
    // injects the per-session SpawnWorkerTool (it is gated on
    // `events_dir.is_some()`).
    let events_dir = unique_events_dir();
    std::fs::create_dir_all(&events_dir).expect("create events dir");

    let config = Config {
        auth: AuthConfig::Bearer {
            tokens: vec![TEST_BEARER_TOKEN.to_string()],
        },
        ..Default::default()
    };
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) =
        spawn_server_full(provider, Vec::new(), config, Some(events_dir.clone())).await;

    // Create a session: its agent loop registers `input_tx` in the Sessions
    // map and its tool list includes the per-session SpawnWorkerTool — the
    // exact shape that used to deadlock shutdown.
    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "create_session",
        "params": { "provider": { "kind": "anthropic_api", "model": "irrelevant" } }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write create_session");
    let resp = read_line(&mut client).await;
    let session_id = resp["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("create_session did not return a session_id: {resp}"));
    assert!(
        session_id.starts_with("session-"),
        "unexpected create_session response: {resp}"
    );

    // stdin-EOF: drop the client so the server's reader sees end-of-stream.
    drop(client);

    // The crux: the daemon must SHUT DOWN, not hang. 5s mirrors `mu ask`'s
    // real kill window; the fixed daemon exits in well under that.
    let outcome = timeout(Duration::from_secs(5), server_handle).await;
    // Best-effort cleanup of the throwaway dir regardless of outcome.
    let _ = std::fs::remove_dir_all(&events_dir);

    let joined = outcome.expect(
        "mu-qc08 regression: `mu serve` did NOT exit within 5s on stdin-EOF — \
         a session-held strong `Sessions` clone is deadlocking shutdown",
    );
    joined
        .expect("server task panicked")
        .expect("serve_with_io_with_config returned an error");
}
