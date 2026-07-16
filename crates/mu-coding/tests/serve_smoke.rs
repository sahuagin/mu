//! Integration smoke tests for `mu serve`. Drive the JSON-RPC surface
//! end-to-end via `tokio::io::duplex` with `FauxProvider` as the LLM.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;

use mu_ai::{FauxProvider, FauxResponse};
use mu_coding::serve;
use mu_core::agent::{
    AssistantMessage, ContentBlock, Provider, ProviderEvent, StopReason, Tool, ToolArgs, ToolCall,
};
use mu_core::config::{AuthConfig, Config, McpConfig, McpServerConfig};

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
        // Hermetic: no startup ollama probe from tests (LAN-baked base
        // is unroutable on CI runners).
        routes: mu_core::config::RoutesConfig {
            ollama_discover: false,
        },
        ..Default::default()
    };
    spawn_server_with_config(provider, tools, config).await
}

/// Like [`spawn_server_with_tools`], but takes an explicit `Config` so a test
/// can exercise config-driven daemon-startup behavior — e.g. mu-yc6's
/// `[[mcp.servers]]`, which makes the daemon connect to MCP servers at
/// startup and import their tools. The caller must set `auth` to
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
    // spec mu-046: the command journal is NOT optional in the daemon
    // path. Point it at a throwaway dir so tests never write into the
    // developer's ~/.local/share/mu/journal.
    let mut config = config;
    if config.journal.dir.is_none() {
        config.journal.dir = Some(unique_test_dir("journal"));
    }
    // Adapt the single Arc<dyn Provider> into a per-session factory
    // that just hands out clones — preserves the smoke-test semantic
    // (one provider for all sessions) under the new factory API.
    let factory: serve::ProviderFactory =
        std::sync::Arc::new(move |_selector, _cache_ttl| Ok(provider.clone()));
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

/// Like [`spawn_server_full`] but WITHOUT the bearer handshake — for configs
/// whose auth is non-enforcing (empty BEARER allowlist), where the daemon runs
/// pre-authenticated (mu-ddua) and a token handshake would be *denied* against
/// the empty allowlist. Returns the (unauthenticated-but-pre-authed) client +
/// handle. Used by the mesh test, which exercises the mesh transport rather
/// than stdio auth.
async fn spawn_server_no_handshake(
    provider: Arc<dyn Provider>,
    config: Config,
) -> (
    tokio::io::DuplexStream,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let server_buf = BufReader::new(server_read);
    let mut config = config;
    if config.journal.dir.is_none() {
        config.journal.dir = Some(unique_test_dir("journal"));
    }
    let factory: serve::ProviderFactory =
        std::sync::Arc::new(move |_selector, _cache_ttl| Ok(provider.clone()));
    let handle = tokio::spawn(serve::serve_with_io_with_config(
        server_buf,
        server_write,
        factory,
        Vec::new(),
        None,
        config,
    ));
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

/// mu-recall-provenance-audit-vnc9.1 (P0): session creation records the
/// recall injection set as a `recall_provenance` event — refs only
/// (source + content-hash + tokens), never the injected text — ordered
/// after `session_created` on the same log.
#[tokio::test]
async fn recall_provenance_event_records_injection_refs_without_content() {
    const SENTINEL: &str = "SENTINEL-PROJECT-CONTEXT-zq9";

    // A project cwd with a deterministic MU.md: the project-file
    // provider resolves `./MU.md` against the session cwd, so this
    // guarantees at least one recalled item with no dependence on the
    // host's memory binary (memory=false below) or env overrides.
    let cwd = unique_test_dir("recall-provenance-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("MU.md"), format!("# project\n{SENTINEL}\n")).unwrap();
    // The provider canonicalizes file paths before reporting them, so
    // compare against the canonical cwd (temp_dir may be a symlink).
    let cwd = cwd.canonicalize().unwrap();

    let config = Config {
        auth: AuthConfig::Bearer {
            tokens: vec![TEST_BEARER_TOKEN.to_string()],
        },
        routes: mu_core::config::RoutesConfig {
            ollama_discover: false,
        },
        recall: mu_core::config::RecallConfig {
            // Hermetic: no subprocess to the host's agent binary.
            memory: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server_with_config(provider, Vec::new(), config).await;

    let req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "create_session",
        "params": {
            "provider": { "kind": "anthropic_api", "model": "x" },
            "cwd": cwd,
        }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = await_response(&mut client, 1).await;
    let session_id = resp["result"]["session_id"].as_str().unwrap().to_string();

    let req = json!({
        "jsonrpc": "2.0", "id": 2, "method": "session.events",
        "params": { "session_id": session_id }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = await_response(&mut client, 2).await;
    let events = resp["result"]["events"].as_array().expect("events").clone();

    // Exactly one provenance event per session creation, after
    // session_created. Membership-based otherwise: the provider may
    // also pick up the operator's config-dir MU.md on a dev host.
    let created_idx = events
        .iter()
        .position(|e| e["payload"]["kind"] == "session_created")
        .expect("session_created");
    let provenance: Vec<(usize, &Value)> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| e["payload"]["kind"] == "recall_provenance")
        .collect();
    assert_eq!(provenance.len(), 1, "one recall_provenance per creation");
    let (idx, event) = provenance[0];
    assert!(idx > created_idx, "provenance follows session_created");
    assert_eq!(event["actor"]["kind"], "system");

    let items = event["payload"]["items"].as_array().expect("items");
    let mu_md = items
        .iter()
        .find(|i| {
            i["source"] == "project_file"
                && i["name"]
                    .as_str()
                    .is_some_and(|n| n.starts_with(cwd.to_str().unwrap()))
        })
        .expect("project_file entry for the session cwd's MU.md");
    assert_eq!(mu_md["redacted"], false);
    assert_eq!(mu_md["content_hash"].as_str().unwrap().len(), 64);
    assert!(mu_md["token_count"].as_u64().unwrap() > 0);
    assert!(mu_md["stable_id"].as_str().unwrap().starts_with("file-"));

    // The invariants, over every entry: refs never carry content, and
    // memory-sourced entries are always redacted tombstones.
    let event_json = serde_json::to_string(event).unwrap();
    assert!(
        !event_json.contains(SENTINEL),
        "injected content leaked into the provenance event"
    );
    for item in items {
        if item["source"] == "memory" {
            assert_eq!(item["redacted"], true, "memory entries must be redacted");
            assert!(item.get("name").is_none(), "redacted entries carry no name");
        }
    }

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}

/// mu-recall-provenance-audit-vnc9.1: recall disabled ⇒ no providers ⇒
/// NO recall_provenance event. Absence means nothing was injected;
/// `--bare` / disabled sessions stay byte-identical.
#[tokio::test]
async fn recall_disabled_emits_no_provenance_event() {
    let config = Config {
        auth: AuthConfig::Bearer {
            tokens: vec![TEST_BEARER_TOKEN.to_string()],
        },
        routes: mu_core::config::RoutesConfig {
            ollama_discover: false,
        },
        recall: mu_core::config::RecallConfig {
            enabled: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server_with_config(provider, Vec::new(), config).await;

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
        "jsonrpc": "2.0", "id": 2, "method": "session.events",
        "params": { "session_id": session_id }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let resp = await_response(&mut client, 2).await;
    let events = resp["result"]["events"].as_array().expect("events").clone();
    assert!(
        events
            .iter()
            .all(|e| e["payload"]["kind"] != "recall_provenance"),
        "no recall_provenance event when recall is disabled"
    );

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

/// Minimal stub MCP server for the mu-yc6 tests. Serves ONE tool
/// (`code_recall`) over rmcp Streamable HTTP (axum hosting rmcp's tower
/// `StreamableHttpService` — the same shape code-index-mcp serves in
/// production). Any call returns a fixed text containing `symbol_name`.
/// Returns the `http://…/mcp` URL; the server task runs for the test's
/// lifetime.
async fn spawn_stub_mcp_http(symbol_name: &'static str) -> String {
    use rmcp::model::{
        CallToolRequestParams, CallToolResult, Content, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerInfo,
    };
    use rmcp::service::{RequestContext, RoleServer};
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, tower::StreamableHttpService,
    };
    use rmcp::{ErrorData as McpError, ServerHandler};

    #[derive(Clone)]
    struct StubMcpServer {
        symbol: &'static str,
    }

    impl ServerHandler for StubMcpServer {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
        }

        // rmcp's ServerHandler declares these as `fn -> impl Future` (not
        // `async fn`), so implementing them mirrors that shape — same
        // allowance as serve/mcp.rs.
        #[allow(clippy::manual_async_fn)]
        fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> impl std::future::Future<Output = Result<ListToolsResult, McpError>> + Send + '_
        {
            async move {
                let schema = match json!({
                    "type": "object",
                    "properties": { "query": { "type": "string" } },
                    "required": ["query"]
                }) {
                    Value::Object(m) => Arc::new(m),
                    _ => unreachable!(),
                };
                Ok(ListToolsResult {
                    tools: vec![rmcp::model::Tool::new(
                        "code_recall",
                        "stub hybrid retrieval",
                        schema,
                    )],
                    ..Default::default()
                })
            }
        }

        #[allow(clippy::manual_async_fn)]
        fn call_tool(
            &self,
            _request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> impl std::future::Future<Output = Result<CallToolResult, McpError>> + Send + '_
        {
            let symbol = self.symbol;
            async move {
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "## {symbol} (0.91) — src/lib.rs:42-60"
                ))]))
            }
        }
    }

    let service: StreamableHttpService<StubMcpServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || {
                Ok(StubMcpServer {
                    symbol: symbol_name,
                })
            },
            Default::default(),
            Default::default(),
        );
    let app = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stub mcp");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}/mcp")
}

/// mu-yc6: tools from a configured `[[mcp.servers]]` entry are imported at
/// daemon startup and callable by the in-loop agent. `code_recall` (the
/// highest-value instance of Friction B) is the motivating tool.
///
/// This drives the full path: config → daemon connects to a stub MCP server
/// over Streamable HTTP → initialize + tools/list → registers `code_recall` →
/// a scripted agent invokes it → tools/call round-trip → result. Asserts the
/// agent invokes the native tool (not a grep fallback) and it completes ok
/// with the server's text.
#[tokio::test]
async fn mcp_tools_imported_from_config_mu_yc6() {
    // Stub MCP server returning a known symbol for any code_recall call.
    let url = spawn_stub_mcp_http("build_and_register_session").await;

    // FauxProvider scripted to call code_recall, then end the turn.
    let call = ProviderEvent::Done(AssistantMessage {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: "tc-mcp-1".into(),
            name: "code_recall".into(),
            arguments: ToolArgs::new(json!({"query": "where are sessions built"}))
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

    // Config points the daemon at the stub MCP server → connect at startup +
    // import code_recall (mu-yc6). No base tools needed; the daemon imports.
    let config = Config {
        auth: AuthConfig::Bearer {
            tokens: vec![TEST_BEARER_TOKEN.to_string()],
        },
        mcp: McpConfig {
            enabled: true,
            servers: vec![McpServerConfig {
                name: "stub-code-index".into(),
                url,
                tools: Some(vec!["code_recall".into()]),
                prefix: None,
                // mu-cvm5: operator classifies this trusted code-search
                // server as read-only so imported tools aren't fail-safed
                // to Execute (which a restrictive session would refuse).
                side_effects: Some(mu_core::agent::SideEffects::ReadOnly),
                tool_side_effects: Default::default(),
            }],
        },
        // Hermetic: no startup ollama probe from tests (LAN-baked base
        // is unroutable on CI runners).
        routes: mu_core::config::RoutesConfig {
            ollama_discover: false,
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

    // ask_session — the scripted provider responds with a code_recall call.
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
            Some("session.tool_call_started") if line["params"]["tool_name"] == "code_recall" => {
                started = Some(line.clone());
            }
            Some("session.tool_call_completed") if line["params"]["tool_call_id"] == "tc-mcp-1" => {
                completed = Some(line.clone());
            }
            Some("session.done") => saw_done = true,
            _ => {}
        }
    }

    let started = started.expect("code_recall tool_call_started");
    assert_eq!(started["params"]["tool_name"], "code_recall");
    assert_eq!(started["params"]["session_id"], session_id);

    // Must complete ok (outcome.kind == "ok"), proving the daemon imported the
    // tool over MCP and the agent invoked it in-loop.
    let completed = completed.expect("code_recall tool_call_completed");
    assert_eq!(
        completed["params"]["outcome"]["kind"], "ok",
        "code_recall should complete ok, got: {completed}"
    );
    let result = completed["params"]["outcome"]["result"]
        .as_str()
        .expect("ok result is a string");
    // The MCP server's text must surface in the tool result.
    assert!(
        result.contains("build_and_register_session"),
        "code_recall result should contain the server's symbol: {result:?}"
    );

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}

/// mu-yc6 graceful degradation: an unreachable `[[mcp.servers]]` entry must
/// not break daemon startup or session creation — it just contributes no
/// tools (warning logged; bounded by the importer's per-server timeout).
#[tokio::test]
async fn mcp_unreachable_server_degrades_gracefully_mu_yc6() {
    let provider: Arc<dyn Provider> =
        Arc::new(FauxProvider::scripted(vec![FauxResponse::Script(vec![
            ProviderEvent::TextDelta("hello".into()),
            ProviderEvent::Done(AssistantMessage {
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: None,
            }),
        ])]));

    let config = Config {
        auth: AuthConfig::Bearer {
            tokens: vec![TEST_BEARER_TOKEN.to_string()],
        },
        mcp: McpConfig {
            enabled: true,
            servers: vec![McpServerConfig {
                name: "ghost".into(),
                // Port 1 on loopback: nothing listens; connect fails fast.
                url: "http://127.0.0.1:1/mcp".into(),
                tools: None,
                prefix: None,
                side_effects: None,
                tool_side_effects: Default::default(),
            }],
        },
        ..Default::default()
    };
    let (mut client, server_handle) = spawn_server_with_config(provider, Vec::new(), config).await;

    // The daemon must still create sessions and answer normally.
    let req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "create_session",
        "params": { "provider": { "kind": "anthropic_api", "model": "irrelevant" } }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write create");
    let resp = read_line(&mut client).await;
    assert!(
        resp["result"]["session_id"].is_string(),
        "create_session must succeed with an unreachable MCP server: {resp}"
    );

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}

/// A unique throwaway directory under the system temp dir (events
/// dirs, journal dirs). Avoids polluting the developer's
/// `~/.local/share/mu`. Uniqueness = pid + a process-local counter,
/// enough to keep parallel tests from colliding on the same path.
fn unique_test_dir(kind: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("mu-smoke-{kind}-{}-{}", std::process::id(), n))
}

/// A unique throwaway directory for the lifecycle test's `events_dir`.
fn unique_events_dir() -> std::path::PathBuf {
    unique_test_dir("wnsp-lifecycle")
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

/// Helpers for the mu-mh4 resume tests. Create a session, run one ask to
/// completion (so the predecessor log reaches a clean boundary), and
/// return its session id.
async fn create_and_ask_to_done(
    client: &mut tokio::io::DuplexStream,
    id_base: u64,
    prompt: &str,
) -> String {
    let req = json!({
        "jsonrpc": "2.0", "id": id_base, "method": "create_session",
        "params": { "provider": { "kind": "anthropic_api", "model": "x" } }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write create");
    let resp = read_line(client).await;
    let session_id = resp["result"]["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();

    let req = json!({
        "jsonrpc": "2.0", "id": id_base + 1, "method": "ask_session",
        "params": { "session_id": session_id, "user_message": prompt }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write ask");

    let mut saw_done = false;
    while !saw_done {
        let line = read_line(client).await;
        if line["method"] == "session.done" && line["params"]["session_id"] == session_id {
            saw_done = true;
        }
    }
    session_id
}

/// mu-mh4: session.resume forks a fresh live session at a clean
/// predecessor's tail, seeding it with the continuation history.
#[tokio::test]
async fn mh4_resume_forks_clean_session_at_tail() {
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server(provider).await;

    // Predecessor: one completed exchange → clean boundary.
    let predecessor = create_and_ask_to_done(&mut client, 1, "first question").await;

    // Resume it. daemon_id is unknown to the test, but the handler
    // resolves by session id, so any daemon part parses fine.
    let req = json!({
        "jsonrpc": "2.0", "id": 100, "method": "session.resume",
        "params": {
            "session_ref": format!("anydaemon:{predecessor}"),
            "provider": { "kind": "anthropic_api", "model": "x" }
        }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write resume");

    // Drain to the resume response.
    let resp = loop {
        let line = read_line(&mut client).await;
        if line["id"] == 100 {
            break line;
        }
    };
    assert!(
        resp.get("error").is_none(),
        "resume should succeed on a clean log; got {resp}"
    );
    let new_session = resp["result"]["session_id"]
        .as_str()
        .expect("new session id");
    assert_ne!(new_session, predecessor, "resume births a NEW session");
    assert_eq!(
        resp["result"]["predecessor_session_id"], predecessor,
        "response names the predecessor"
    );
    // The clean exchange seeded 2 messages (user + assistant).
    assert_eq!(
        resp["result"]["seeded_message_count"], 2,
        "continuation seeded the prior exchange: {resp}"
    );
    assert!(
        resp["result"]["branched_at_event_id"].is_u64(),
        "forked at a concrete boundary event: {resp}"
    );

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}

/// mu-mh4: session.resume REFUSES an unknown predecessor with a clear
/// not-found error (does not panic or silently create an empty session).
#[tokio::test]
async fn mh4_resume_unknown_session_refused() {
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server(provider).await;

    let req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "session.resume",
        "params": {
            "session_ref": "d:session-does-not-exist",
            "provider": { "kind": "anthropic_api", "model": "x" }
        }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write");
    let resp = read_line(&mut client).await;
    assert_eq!(resp["id"], 1);
    let msg = resp["error"]["message"].as_str().expect("error message");
    assert!(
        msg.contains("not found"),
        "unknown predecessor must be refused with not-found: {msg}"
    );

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}

/// mu-mh4: a malformed session ref is rejected with a message naming the
/// accepted forms.
#[tokio::test]
async fn mh4_resume_bad_ref_rejected() {
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server(provider).await;

    let req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "session.resume",
        "params": {
            "session_ref": "no-separator-here",
            "provider": { "kind": "anthropic_api", "model": "x" }
        }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write");
    let resp = read_line(&mut client).await;
    let msg = resp["error"]["message"].as_str().expect("error message");
    assert!(
        msg.contains("daemon:session") || msg.contains("mu:<daemon>"),
        "bad ref error must name the accepted forms: {msg}"
    );

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}

// ---------------------------------------------------------------------------
// mu-wxc4: the mesh transport, live. Proves the daemon actually serves its
// JSON-RPC surface over NATS through the wired adapter (serve/mesh.rs,
// spawned from serve/mod.rs when [mesh].enabled) — not just the in-memory
// unit proof. A real nats-server, a real request/reply round-trip.
// ---------------------------------------------------------------------------

/// Spawn a throwaway nats-server (JetStream on, store under target/) and wait
/// for its port to accept connections. The binary is resolved from `$NATS_BIN`
/// if set, otherwise `nats-server` on `PATH` — so this runs on any host with
/// nats-server installed, not just one hardcoded location. Skips the test
/// (returns None) if the binary cannot be spawned, so the suite still passes on
/// hosts without NATS installed.
///
/// The child is returned to the caller, which kills + waits it. clippy's
/// zombie_processes lint can't see that cross-function ownership (and the
/// timeout-panic path aborts the test process regardless), so allow it here.
#[allow(clippy::zombie_processes)]
async fn spawn_nats(port: u16) -> Option<(std::process::Child, String)> {
    let bin = std::env::var("NATS_BIN").unwrap_or_else(|_| "nats-server".to_string());
    let store = format!("target/nats-js-{port}");
    let _ = std::fs::remove_dir_all(&store);
    let child = match std::process::Command::new(&bin)
        .args([
            "-p",
            &port.to_string(),
            "-js",
            "-sd",
            &store,
            "-a",
            "127.0.0.1",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            eprintln!("skipping mesh test: cannot spawn nats-server ({bin}): {e} — set NATS_BIN or add nats-server to PATH");
            return None;
        }
    };
    let url = format!("127.0.0.1:{port}");
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(&url).await.is_ok() {
            return Some((child, url));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("nats-server on {url} never accepted connections");
}

/// The daemon, mesh-enabled, over a live NATS: a mesh client requests the
/// daemon's subject and gets the JSON-RPC reply back over the bus. This is
/// the full wired path — `spawn_mesh_adapter` (serve/mod.rs startup) →
/// subscription → `ingest` → Router lane → NATS reply — with only the client
/// standing in for a peer service.
#[tokio::test]
async fn mesh_daemon_serves_jsonrpc_over_live_nats() {
    let Some((mut nats, url)) = spawn_nats(14520).await else {
        return; // no nats-server on this host; nothing to prove
    };

    const SUBJECT: &str = "mu.smoke.daemon.rpc";
    // Default auth (BEARER with an empty allowlist) is non-enforcing, so the
    // daemon runs pre-authenticated (mu-ddua) and the mesh is exposed. With an
    // *enforcing* mechanism the mesh fails closed instead — see
    // `mesh_refuses_when_auth_is_enforcing_over_live_nats`.
    let config = Config {
        routes: mu_core::config::RoutesConfig {
            ollama_discover: false,
        },
        mesh: mu_core::config::MeshConfig {
            enabled: true,
            nats_url: url.clone(),
            subject: SUBJECT.to_string(),
        },
        ..Default::default()
    };

    // Keep `client` alive (unused beyond keeping the daemon serving). No
    // handshake: the empty-allowlist config is non-enforcing, so the daemon is
    // pre-authenticated and the mesh is exposed.
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (client, server_handle) = spawn_server_no_handshake(provider, config).await;

    // A peer on the bus. Retry the request/reply until the daemon's mesh
    // subscription is up (no stdio handshake barrier gates that here), then
    // exercise the full wired path: subject → ingest → Router lane → NATS reply.
    let bus = async_nats::connect(&url).await.expect("connect nats");
    let req = json!({ "jsonrpc": "2.0", "id": 99, "method": "ping", "params": null });
    let payload = serde_json::to_vec(&req).expect("encode request");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let reply = loop {
        match timeout(
            Duration::from_secs(2),
            bus.request(SUBJECT, payload.clone().into()),
        )
        .await
        {
            Ok(Ok(reply)) => break reply,
            _ if std::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            other => panic!("mesh never responded before deadline: {other:?}"),
        }
    };
    let resp: Value = serde_json::from_slice(&reply.payload).expect("decode mesh reply");

    assert_eq!(resp["jsonrpc"], "2.0", "reply is JSON-RPC: {resp}");
    assert_eq!(
        resp["id"], 99,
        "reply restores the client's own JSON-RPC id (not the internal correlation id): {resp}"
    );
    assert_eq!(
        resp["result"]["pong"], true,
        "daemon answered ping over the mesh: {resp}"
    );
    assert!(
        resp["result"]["server_version"].is_string(),
        "ping result carries server_version: {resp}"
    );

    // An unparseable request that carries a reply subject gets a JSON-RPC
    // error back (id=null, -32700), like stdio's parse_request_line — not a
    // silent drop that leaves the peer waiting out its timeout.
    let bad = timeout(
        Duration::from_secs(5),
        bus.request(SUBJECT, b"}{ not json".to_vec().into()),
    )
    .await
    .expect("parse-error reply timed out")
    .expect("parse-error reply failed");
    let err: Value = serde_json::from_slice(&bad.payload).expect("decode error reply");
    assert_eq!(err["jsonrpc"], "2.0", "error reply is JSON-RPC: {err}");
    assert!(err["id"].is_null(), "unparseable request → id null: {err}");
    assert_eq!(err["error"]["code"], -32700, "parse error code: {err}");

    drop(client); // stdin-EOF → daemon shutdown cascade (drops the mesh guard)
    let _ = timeout(Duration::from_millis(500), server_handle).await;
    nats.kill().ok();
    nats.wait().ok();
}

/// mu-iqo8 fail-closed: with an *enforcing* auth mechanism configured, the mesh
/// adapter must NOT expose the daemon — it multiplexes peers on one subject and
/// cannot isolate their auth yet, so exposing protected commands would be a
/// cross-peer bypass. The daemon refuses to subscribe; a bus peer requesting
/// the subject gets "no responders", not a reply.
#[tokio::test]
async fn mesh_refuses_when_auth_is_enforcing_over_live_nats() {
    let Some((mut nats, url)) = spawn_nats(14521).await else {
        return; // no nats-server on this host; nothing to prove
    };

    const SUBJECT: &str = "mu.smoke.daemon.refused";
    // A non-empty BEARER allowlist IS enforcing → auth required → mesh refuses.
    let config = Config {
        auth: AuthConfig::Bearer {
            tokens: vec![TEST_BEARER_TOKEN.to_string()],
        },
        routes: mu_core::config::RoutesConfig {
            ollama_discover: false,
        },
        mesh: mu_core::config::MeshConfig {
            enabled: true,
            nats_url: url.clone(),
            subject: SUBJECT.to_string(),
        },
        ..Default::default()
    };

    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (client, server_handle) = spawn_server_with_config(provider, Vec::new(), config).await;

    let bus = async_nats::connect(&url).await.expect("connect nats");
    let req = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping", "params": null });
    let payload = serde_json::to_vec(&req).expect("encode request");
    // No subscriber on the subject: the mesh refused to come up. That surfaces
    // either as a NoResponders request error or (if the client isn't tracking
    // responders) as no reply at all — both are correct. Only an actual reply
    // means the mesh wrongly exposed the daemon under enforcing auth.
    let outcome = timeout(Duration::from_secs(2), bus.request(SUBJECT, payload.into())).await;
    if let Ok(Ok(reply)) = outcome {
        panic!(
            "mesh must refuse under enforcing auth, but a peer got a reply: {:?}",
            String::from_utf8_lossy(&reply.payload)
        );
    }

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
    nats.kill().ok();
    nats.wait().ok();
}
