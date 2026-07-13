//! spec mu-046 WP3 load-bearing tests: the ingest pipeline at the
//! serve level. Drives the JSON-RPC surface end-to-end via
//! `tokio::io::duplex` (same harness shape as `serve_smoke.rs`) with
//! the command journal pointed at a throwaway dir, then asserts on the
//! raw journal — the matching-engine paper trail:
//!
//! - crash test (INV-1/INV-4): a handler that panics after ingest
//!   leaves exactly one `CommandReceived`, no receipt; replay surfaces
//!   it as an orphan.
//! - fail-closed (INV-2): a daemon that cannot open its journal at
//!   boot does not serve. (The append-failure-at-ingest arm of INV-2
//!   is unit-tested in `serve/pipeline.rs`, where the seam can be
//!   poisoned in-process.)
//! - auth rejection (INV-6 + receipts): unauthenticated protected
//!   calls journal `CommandReceived` + `CommandRejected{auth_gate}`,
//!   and the bearer token never hits the journal bytes.
//! - seq == order (INV-3): receipts land in journal-seq order — the
//!   single-writer consumer processes commands as sequenced.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;

use mu_ai::FauxProvider;
use mu_coding::serve;
use mu_core::agent::Provider;
use mu_core::command_journal::{CommandJournal, JournalPayload, JournalRecord, RejectStage};
// `orphaned_command_seqs` is used only by the `#[cfg(debug_assertions)]` crash
// test below; gate its import the same way, or it's an unused import in release
// builds where that test is compiled out.
#[cfg(debug_assertions)]
use mu_core::command_journal::orphaned_command_seqs;
use mu_core::config::{AuthConfig, Config, JournalConfig};
use mu_core::event_log::{EventPayload, SessionEvent, SessionEventLog};

/// Shared bearer token used by the harness — also the secret the
/// INV-6 test greps the raw journal bytes for.
const TEST_BEARER_TOKEN: &str = "pipeline-smoke-secret-token";

/// A unique throwaway journal dir under the system temp dir —
/// uniqueness = pid + a process-local counter.
fn unique_journal_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "mu-pipeline-smoke-journal-{}-{}",
        std::process::id(),
        n
    ))
}

/// Spawn `serve_with_io_with_config` with bearer auth and the journal
/// at a fresh tempdir. Does NOT authenticate — callers that need an
/// authed client call [`authenticate`] themselves (the INV-6 test
/// needs the unauthenticated phase).
///
/// `events_dir = None`: sessions get in-memory-only logs, so
/// session-scoped commands take the WP4 documented FALLBACK into the
/// daemon journal — which is exactly what the WP3-era tests below
/// assert against. The WP4 session-log tests use
/// [`spawn_server_with_events`] instead.
fn spawn_server_raw(
    provider: Arc<dyn Provider>,
    journal_dir: PathBuf,
) -> (
    tokio::io::DuplexStream,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    spawn_server_with_events(provider, journal_dir, None)
}

/// [`spawn_server_raw`] with an optional events dir: `Some(dir)` gives
/// every created session a DISK-BACKED event log, activating the WP4
/// session-pipeline path (session-scoped commands journal into the
/// session's own log).
fn spawn_server_with_events(
    provider: Arc<dyn Provider>,
    journal_dir: PathBuf,
    events_dir: Option<PathBuf>,
) -> (
    tokio::io::DuplexStream,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let config = Config {
        auth: AuthConfig::Bearer {
            tokens: vec![TEST_BEARER_TOKEN.to_string()],
        },
        journal: JournalConfig {
            dir: Some(journal_dir),
            ..Default::default()
        },
        // Hermetic: never probe the (LAN-baked) ollama base from tests —
        // unroutable on CI runners, the connect timeout stalls boot.
        routes: mu_core::config::RoutesConfig {
            ollama_discover: false,
        },
        ..Default::default()
    };
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let server_buf = BufReader::new(server_read);
    let factory: serve::ProviderFactory =
        std::sync::Arc::new(move |_selector, _cache_ttl| Ok(provider.clone()));
    let handle = tokio::spawn(serve::serve_with_io_with_config(
        server_buf,
        server_write,
        factory,
        Vec::new(),
        events_dir,
        config,
    ));
    (client, handle)
}

/// Perform the BEARER handshake so subsequent RPCs pass the gate.
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
    let resp = await_response(client, 0).await;
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

/// Skim lines until the response with `id` arrives (notifications are
/// dropped). Times out at 30s — generous on purpose: a passing test
/// completes in milliseconds, and a tight budget only converts CI
/// runner contention into flakes (the 2s original failed on GitHub
/// runners when the since-removed startup probe ate the whole window).
async fn await_response<R: tokio::io::AsyncRead + Unpin>(reader: &mut R, id: i64) -> Value {
    timeout(Duration::from_secs(30), async {
        loop {
            let line = read_line(reader).await;
            if line.get("id").and_then(|v| v.as_i64()) == Some(id) {
                return line;
            }
        }
    })
    .await
    .expect("response did not arrive within 30s")
}

/// Skim lines until the first NOTIFICATION with `method` arrives
/// (responses and other notifications are dropped). Times out at 30s —
/// same rationale as [`await_response`].
async fn await_notification<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    method: &str,
) -> Value {
    timeout(Duration::from_secs(30), async {
        loop {
            let line = read_line(reader).await;
            if line.get("method").and_then(|v| v.as_str()) == Some(method) {
                return line;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("notification {method} did not arrive within 30s"))
}

/// Locate and parse `<events_dir>/<daemon_id>/<session_id>.jsonl`
/// (the daemon id is generated, so scan one level). Returns the
/// parsed events; panics on a missing or malformed log.
fn session_log_events(events_dir: &Path, session_id: &str) -> Vec<SessionEvent> {
    let path = std::fs::read_dir(events_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .map(|d| d.join(format!("{session_id}.jsonl")))
        .find(|p| p.exists())
        .unwrap_or_else(|| {
            panic!(
                "session log for {session_id} not found under {}",
                events_dir.display()
            )
        });
    let (log, malformed) = SessionEventLog::from_jsonl(&path).expect("parse session log");
    assert_eq!(malformed, 0, "session log has malformed lines");
    log.snapshot()
}

/// Replay the single `<daemon_id>.jsonl` journal in `dir`. Receipts
/// are appended BEFORE responses are emitted, so once a command's
/// response has been observed on the wire its records are durable.
fn read_journal(dir: &Path) -> (PathBuf, Vec<JournalRecord>) {
    let path = std::fs::read_dir(dir)
        .expect("read journal dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .expect("journal file present");
    let (records, malformed) = CommandJournal::replay(&path).expect("replay journal");
    assert_eq!(malformed, 0, "journal has malformed records");
    (path, records)
}

fn received_seq(records: &[JournalRecord], wanted_method: &str) -> Vec<u64> {
    records
        .iter()
        .filter_map(|r| match &r.payload {
            JournalPayload::CommandReceived { method, .. } if method == wanted_method => {
                Some(r.seq)
            }
            _ => None,
        })
        .collect()
}

/// Crash test (INV-1/INV-4): `mu.test.panic` dies after ingest, before
/// any receipt. The journal holds exactly one `CommandReceived` for it
/// and NO receipt; replay surfaces it as an orphan. The control plane
/// survives (the panic is in a spawned session-scope task) — a
/// follow-up ping completes WITH a receipt.
///
/// WP4 note: `mu.test.panic` is session-scoped but addresses a session
/// that does not exist (`s-doomed`), so it takes the documented
/// unresolvable-session fallback and still journals into the DAEMON
/// journal — these assertions stay against the right journal.
#[cfg(debug_assertions)] // the crash seam exists in debug builds only
#[tokio::test]
async fn crash_after_ingest_leaves_orphaned_command_received_inv1_inv4() {
    let journal_dir = unique_journal_dir();
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server_raw(provider, journal_dir.clone());
    authenticate(&mut client).await;

    let req = json!({
        "jsonrpc": "2.0", "id": 7, "method": "mu.test.panic", "params": { "session_id": "s-doomed" }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write panic method");
    // No response will ever arrive for id 7 — the handler died after
    // ingest. Prove the daemon survived with a ping.
    let req = json!({
        "jsonrpc": "2.0", "id": 8, "method": "ping", "params": null
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write ping");
    let resp = await_response(&mut client, 8).await;
    assert_eq!(resp["result"]["pong"], true);

    let (_path, records) = read_journal(&journal_dir);
    let panic_seqs = received_seq(&records, "mu.test.panic");
    assert_eq!(
        panic_seqs.len(),
        1,
        "exactly one CommandReceived for the crashed command: {records:?}"
    );
    let panic_seq = panic_seqs[0];
    // No receipt of any kind for the crashed command.
    let receipted = records.iter().any(|r| {
        matches!(
            &r.payload,
            JournalPayload::CommandSucceeded { command_seq, .. }
            | JournalPayload::CommandFailed { command_seq, .. }
            | JournalPayload::CommandRejected { command_seq, .. }
                if *command_seq == panic_seq
        )
    });
    assert!(!receipted, "crashed command must have NO receipt");
    // Replay surfaces it as THE orphan — the legible crash marker.
    let orphans = orphaned_command_seqs(&records);
    assert!(
        orphans.contains(&panic_seq),
        "orphan detection must surface seq {panic_seq}: {orphans:?}"
    );
    // The ping was receipted, so the orphan is not just "consumer died".
    let ping_seq = received_seq(&records, "ping")[0];
    assert!(
        !orphans.contains(&ping_seq),
        "the surviving ping must NOT be an orphan"
    );

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
    let _ = std::fs::remove_dir_all(&journal_dir);
}

/// Fail-closed at boot (INV-2): a daemon that cannot OPEN its journal
/// does not serve — `serve_with_io_with_config` aborts with an error
/// before reading any input, so no handler can ever run.
#[tokio::test]
async fn journal_open_failure_aborts_serve_inv2() {
    // A FILE where the journal directory should be: create_dir_all
    // fails, so CommandJournal::open fails, so serve refuses to start.
    let journal_dir = unique_journal_dir();
    std::fs::write(&journal_dir, b"not a directory").expect("plant blocking file");

    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (client, server_handle) = spawn_server_raw(provider, journal_dir.clone());

    let outcome = timeout(Duration::from_secs(5), server_handle)
        .await
        .expect("serve must abort promptly when the journal cannot open")
        .expect("server task must not panic");
    let err = outcome.expect_err("serve must REFUSE to run without a journal (INV-2)");
    assert!(
        err.to_string().contains("cannot open command journal"),
        "error names the journal: {err}"
    );

    drop(client);
    let _ = std::fs::remove_file(&journal_dir);
}

/// Auth rejection is a receipt too (INV-6 + receipt semantics): an
/// unauthenticated protected call journals `CommandReceived` +
/// `CommandRejected{stage: auth_gate}`, and after a real
/// `peer.auth_initiate` the configured bearer token appears NOWHERE in
/// the raw journal bytes — params are redacted before append.
#[tokio::test]
async fn auth_rejection_journaled_and_token_redacted_inv6() {
    let journal_dir = unique_journal_dir();
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server_raw(provider, journal_dir.clone());

    // Unauthenticated protected call → AUTH_REQUIRED.
    let req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "create_session",
        "params": { "provider": { "kind": "anthropic_api", "model": "x" } }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write create");
    let resp = await_response(&mut client, 1).await;
    assert_eq!(resp["error"]["code"], -32001, "expected AUTH_REQUIRED");

    // Now a REAL auth attempt with the live token (the secret under
    // test), which must be redacted in the journal.
    authenticate(&mut client).await;

    let (path, records) = read_journal(&journal_dir);

    // The rejected create_session: received + rejected at the gate.
    let create_seq = received_seq(&records, "create_session")[0];
    let gate_reject = records.iter().any(|r| {
        matches!(
            &r.payload,
            JournalPayload::CommandRejected { command_seq, stage: RejectStage::AuthGate, .. }
                if *command_seq == create_seq
        )
    });
    assert!(
        gate_reject,
        "auth-gate rejection must be journaled as a receipt: {records:?}"
    );

    // The auth attempt is journaled with its secret redacted.
    let auth_params = records
        .iter()
        .find_map(|r| match &r.payload {
            JournalPayload::CommandReceived { method, params, .. }
                if method == "peer.auth_initiate" =>
            {
                Some(params.clone())
            }
            _ => None,
        })
        .expect("peer.auth_initiate CommandReceived");
    assert_eq!(auth_params["initial_response"], "[REDACTED]");
    assert_eq!(auth_params["mechanism"], "bearer");

    // INV-6, the load-bearing assertion: raw journal bytes are clean.
    let raw = std::fs::read_to_string(&path).expect("read raw journal");
    assert!(
        !raw.contains(TEST_BEARER_TOKEN),
        "the bearer token leaked into the journal"
    );

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
    let _ = std::fs::remove_dir_all(&journal_dir);
}

/// Seq == order (INV-3): N daemon-scoped commands through ingest are
/// processed in journal order. Receipts are appended by the
/// single-writer consumer as each command completes, so the receipts'
/// file order IS processing order — assert it matches seq order.
#[tokio::test]
async fn daemon_commands_process_in_seq_order_inv3() {
    let journal_dir = unique_journal_dir();
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server_raw(provider, journal_dir.clone());
    authenticate(&mut client).await;

    // One batched write: all N pings hit the read loop back-to-back.
    let mut batch = String::new();
    for id in 1..=8 {
        let req = json!({
            "jsonrpc": "2.0", "id": id, "method": "ping", "params": null
        });
        batch.push_str(&format!("{req}\n"));
    }
    client
        .write_all(batch.as_bytes())
        .await
        .expect("write ping batch");
    for id in 1..=8 {
        let resp = await_response(&mut client, id).await;
        assert_eq!(resp["result"]["pong"], true);
    }

    let (_path, records) = read_journal(&journal_dir);
    let ping_seqs = received_seq(&records, "ping");
    assert_eq!(ping_seqs.len(), 8);
    // Receipts in FILE order (== processing order, single writer),
    // restricted to the pings.
    let receipt_order: Vec<u64> = records
        .iter()
        .filter_map(|r| match &r.payload {
            JournalPayload::CommandSucceeded {
                command_seq,
                command,
                ..
            } if command.method == "ping" => Some(*command_seq),
            _ => None,
        })
        .collect();
    assert_eq!(
        receipt_order, ping_seqs,
        "processing order must equal journal seq order (INV-3)"
    );
    assert!(
        receipt_order.windows(2).all(|w| w[0] < w[1]),
        "receipt command_seqs must be strictly increasing: {receipt_order:?}"
    );

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
    let _ = std::fs::remove_dir_all(&journal_dir);
}

/// Receipts wrap the original command (INV-5) and correlate by seq: a
/// full create→ask round trip through the new path leaves
/// `CommandSucceeded` receipts whose echoes carry the original method
/// + params.
///
/// WP4 note: this server runs with `events_dir = None`, so the
/// session's log is in-memory-only and the ask takes the DOCUMENTED
/// fallback into the daemon journal with the WP3 receipt shape
/// (immediate `accepted: true` receipt) — this test now pins that
/// fallback. The session-log path is covered by
/// `ask_receipt_lands_in_session_log_at_done_wp4` below.
#[tokio::test]
async fn receipts_wrap_the_original_command_inv5() {
    let journal_dir = unique_journal_dir();
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) = spawn_server_raw(provider, journal_dir.clone());
    authenticate(&mut client).await;

    let req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "create_session",
        "params": { "provider": { "kind": "anthropic_api", "model": "x" } }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write create");
    let resp = await_response(&mut client, 1).await;
    let session_id = resp["result"]["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();

    let req = json!({
        "jsonrpc": "2.0", "id": 2, "method": "ask_session",
        "params": { "session_id": session_id, "user_message": "hello" }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write ask");
    let resp = await_response(&mut client, 2).await;
    assert_eq!(resp["result"]["accepted"], true);

    let (_path, records) = read_journal(&journal_dir);
    let ask_seq = received_seq(&records, "ask_session")[0];
    let ask_receipt = records
        .iter()
        .find_map(|r| match &r.payload {
            JournalPayload::CommandSucceeded {
                command_seq,
                command,
                result,
                ..
            } if *command_seq == ask_seq => Some((command.clone(), result.clone())),
            _ => None,
        })
        .expect("ask_session receipt");
    // The receipt is self-contained evidence: original method, params,
    // and what came of it.
    assert_eq!(ask_receipt.0.method, "ask_session");
    assert_eq!(ask_receipt.0.params["session_id"], session_id);
    assert_eq!(ask_receipt.0.params["user_message"], "hello");
    assert_eq!(ask_receipt.1["accepted"], true);

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
    let _ = std::fs::remove_dir_all(&journal_dir);
}

// ───────────────────────────────────────────────────────────────────
// spec mu-046 WP4: session-scoped commands journal into THEIR
// SESSION's event log; completion receipts pair by command_event_id.
// ───────────────────────────────────────────────────────────────────

/// A unique throwaway events dir (gives sessions disk-backed logs).
fn unique_events_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "mu-pipeline-smoke-events-{}-{}",
        std::process::id(),
        n
    ))
}

/// create_session and return the new session id.
async fn create_session(client: &mut tokio::io::DuplexStream, id: i64) -> String {
    let req = json!({
        "jsonrpc": "2.0", "id": id, "method": "create_session",
        "params": { "provider": { "kind": "anthropic_api", "model": "x" } }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write create");
    let resp = await_response(client, id).await;
    resp["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("create_session failed: {resp}"))
        .to_string()
}

/// Receipt-at-Done (spec mu-046 WP4, receipt semantics): with a
/// disk-backed session log, `ask_session` journals `CommandReceived`
/// into the SESSION's log BEFORE the loop processes the message, the
/// wire `accepted: true` stays immediate, and after the turn's `Done`
/// exactly one `CommandSucceeded` wraps the original ask with the
/// matching `command_event_id`. The daemon journal carries NO
/// session-scoped command — but the unresolvable-session fallback
/// still lands there (border record always exists).
#[tokio::test]
async fn ask_receipt_lands_in_session_log_at_done_wp4() {
    let journal_dir = unique_journal_dir();
    let events_dir = unique_events_dir();
    std::fs::create_dir_all(&events_dir).expect("create events dir");
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) =
        spawn_server_with_events(provider, journal_dir.clone(), Some(events_dir.clone()));
    authenticate(&mut client).await;

    let session_id = create_session(&mut client, 1).await;

    let req = json!({
        "jsonrpc": "2.0", "id": 2, "method": "ask_session",
        "params": { "session_id": session_id, "user_message": "hello" }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write ask");
    let resp = await_response(&mut client, 2).await;
    assert_eq!(
        resp["result"]["accepted"], true,
        "wire response stays immediate: {resp}"
    );
    // The receipt is appended BEFORE the wire `session.done` leaves,
    // so observing the notification means the receipt is durable.
    let done = await_notification(&mut client, "session.done").await;
    assert_eq!(done["params"]["session_id"], session_id.as_str());

    let events = session_log_events(&events_dir, &session_id);
    // CommandReceived for the ask is in the session log, BEFORE the
    // UserMessage the loop projected for it (durable before
    // processed, INV-1).
    let received_id = events
        .iter()
        .find_map(|e| match &e.payload {
            EventPayload::CommandReceived { method, params, .. }
                if method == "ask_session" && params["user_message"] == "hello" =>
            {
                Some(e.id)
            }
            _ => None,
        })
        .expect("CommandReceived in session log");
    let user_msg_id = events
        .iter()
        .find_map(|e| match &e.payload {
            EventPayload::UserMessage { content } if content == "hello" => Some(e.id),
            _ => None,
        })
        .expect("UserMessage in session log");
    assert!(
        received_id < user_msg_id,
        "CommandReceived (id {received_id}) must precede processing (UserMessage id {user_msg_id})"
    );
    // Exactly one CommandSucceeded, wrapping the original ask,
    // pairing by command_event_id.
    let receipts: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::CommandSucceeded {
                command_event_id,
                command,
                ..
            } => Some((*command_event_id, command.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(receipts.len(), 1, "exactly one success receipt: {events:?}");
    let (command_event_id, echo) = &receipts[0];
    assert_eq!(*command_event_id, received_id, "receipt pairs the ask");
    assert_eq!(echo.method, "ask_session");
    assert_eq!(echo.params["user_message"], "hello");
    // No failure/rejection receipts for the ask.
    assert!(
        !events.iter().any(|e| matches!(
            &e.payload,
            EventPayload::CommandFailed { .. } | EventPayload::CommandRejected { .. }
        )),
        "no failure receipts expected: {events:?}"
    );

    // The DAEMON journal no longer carries the session-scoped ask...
    let (_path, records) = read_journal(&journal_dir);
    assert!(
        received_seq(&records, "ask_session").is_empty(),
        "daemon journal must not carry session-scoped commands: {records:?}"
    );

    // ...but an UNRESOLVABLE session still falls back there (the
    // border record always exists).
    let req = json!({
        "jsonrpc": "2.0", "id": 3, "method": "ask_session",
        "params": { "session_id": "no-such-session", "user_message": "hi" }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write bogus ask");
    let resp = await_response(&mut client, 3).await;
    assert!(resp.get("error").is_some(), "expected an error: {resp}");
    let (_path, records) = read_journal(&journal_dir);
    let bogus_seqs = received_seq(&records, "ask_session");
    assert_eq!(
        bogus_seqs.len(),
        1,
        "unresolvable-session ask lands in the daemon journal: {records:?}"
    );
    let rejected = records.iter().any(|r| {
        matches!(&r.payload, JournalPayload::CommandRejected { command_seq, .. }
            if *command_seq == bogus_seqs[0])
    });
    assert!(rejected, "and gains its rejection receipt: {records:?}");

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
    let _ = std::fs::remove_dir_all(&journal_dir);
    let _ = std::fs::remove_dir_all(&events_dir);
}

/// mu-z9ol: the wire `session.done` names the ask(s) it satisfied via
/// `command_receipts`, end to end — journaled ticket → agent loop →
/// forwarder → wire — and the receipt's `command_event_id` pairs with
/// the session-log `CommandReceived`. Clients (mu-solo queued
/// interjections) reconcile queued prompts against these instead of
/// awaiting a per-ask done that a shared/absorbed Done never sends.
#[tokio::test]
async fn z9ol_wire_done_names_the_ask_it_satisfies() {
    let journal_dir = unique_journal_dir();
    let events_dir = unique_events_dir();
    std::fs::create_dir_all(&events_dir).expect("create events dir");
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) =
        spawn_server_with_events(provider, journal_dir.clone(), Some(events_dir.clone()));
    authenticate(&mut client).await;

    let session_id = create_session(&mut client, 1).await;

    let req = json!({
        "jsonrpc": "2.0", "id": 7, "method": "ask_session",
        "params": { "session_id": session_id, "user_message": "hello" }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write ask");

    let done = await_notification(&mut client, "session.done").await;
    let receipts = done["params"]["command_receipts"]
        .as_array()
        .expect("session.done carries command_receipts for the ask");
    assert_eq!(receipts.len(), 1, "one ask → one receipt: {done}");
    assert_eq!(receipts[0]["request_id"], 7);
    assert_eq!(receipts[0]["method"], "ask_session");
    // The full original params must NOT be echoed on the wire.
    assert!(receipts[0].get("params").is_none());

    // The wire receipt pairs with the session-log CommandReceived.
    let events = session_log_events(&events_dir, &session_id);
    let received_id = events
        .iter()
        .find_map(|e| match &e.payload {
            EventPayload::CommandReceived { method, .. } if method == "ask_session" => Some(e.id),
            _ => None,
        })
        .expect("CommandReceived in session log");
    assert_eq!(
        receipts[0]["command_event_id"].as_u64(),
        Some(received_id),
        "wire receipt pairs with the session-log command"
    );

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
    let _ = std::fs::remove_dir_all(&journal_dir);
    let _ = std::fs::remove_dir_all(&events_dir);
}

/// A provider whose stream never yields: the ask wedges in-flight so
/// the test can cancel it mid-turn. The agent loop's cancel path does
/// not need provider cooperation — it selects on its input channel.
struct StallProvider;

#[async_trait::async_trait]
impl Provider for StallProvider {
    async fn stream(
        &self,
        _system_prompt: Option<&str>,
        _effort: Option<&str>,
        _input: mu_core::agent::MessageInput<'_>,
        _tools: &[mu_core::agent::ToolSpec],
        _cancel_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<
        futures::stream::BoxStream<'static, mu_core::agent::ProviderEvent>,
        mu_core::agent::ProviderError,
    > {
        Ok(Box::pin(futures::stream::pending()))
    }
}

/// Cancel pairing (spec mu-046 WP4): `cancel_session` journals its own
/// `CommandReceived` + `CommandSucceeded` receipt in the session log,
/// and the in-flight ask's terminal `Done(Aborted)` produces THAT
/// ask's receipt — a `CommandFailed` (documented choice: the ask was
/// accepted and entered processing; abort is a processing outcome)
/// pairing the ask's `command_event_id`.
#[tokio::test]
async fn cancel_session_pairs_aborted_ask_receipt_wp4() {
    let journal_dir = unique_journal_dir();
    let events_dir = unique_events_dir();
    std::fs::create_dir_all(&events_dir).expect("create events dir");
    let provider: Arc<dyn Provider> = Arc::new(StallProvider);
    let (mut client, server_handle) =
        spawn_server_with_events(provider, journal_dir.clone(), Some(events_dir.clone()));
    authenticate(&mut client).await;

    let session_id = create_session(&mut client, 1).await;

    let req = json!({
        "jsonrpc": "2.0", "id": 2, "method": "ask_session",
        "params": { "session_id": session_id, "user_message": "spin forever" }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write ask");
    let resp = await_response(&mut client, 2).await;
    assert_eq!(resp["result"]["accepted"], true);

    // Wait until the loop has actually STARTED the ask (its
    // UserMessage is in the log) so the receipt ticket is pending
    // inside the loop before we cancel.
    timeout(Duration::from_secs(5), async {
        loop {
            let events = session_log_events(&events_dir, &session_id);
            if events
                .iter()
                .any(|e| matches!(&e.payload, EventPayload::UserMessage { content } if content == "spin forever"))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("ask never reached the loop");

    let req = json!({
        "jsonrpc": "2.0", "id": 3, "method": "cancel_session",
        "params": { "session_id": session_id }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write cancel");
    let resp = await_response(&mut client, 3).await;
    assert_eq!(resp["result"]["cancelled"], true);

    // The dying loop flushes Done(Aborted) carrying the ask's ticket;
    // the forwarder writes the CommandFailed receipt. Poll the log.
    let events = timeout(Duration::from_secs(5), async {
        loop {
            let events = session_log_events(&events_dir, &session_id);
            if events
                .iter()
                .any(|e| matches!(&e.payload, EventPayload::CommandFailed { .. }))
            {
                return events;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("aborted ask never gained its CommandFailed receipt");

    let ask_received_id = events
        .iter()
        .find_map(|e| match &e.payload {
            EventPayload::CommandReceived { method, params, .. }
                if method == "ask_session" && params["user_message"] == "spin forever" =>
            {
                Some(e.id)
            }
            _ => None,
        })
        .expect("ask CommandReceived in session log");
    let cancel_received_id = events
        .iter()
        .find_map(|e| match &e.payload {
            EventPayload::CommandReceived { method, .. } if method == "cancel_session" => {
                Some(e.id)
            }
            _ => None,
        })
        .expect("cancel CommandReceived in session log");
    // cancel_session: immediate-completion receipt, paired.
    let cancel_receipted = events.iter().any(|e| {
        matches!(&e.payload, EventPayload::CommandSucceeded { command_event_id, command, .. }
            if *command_event_id == cancel_received_id && command.method == "cancel_session")
    });
    assert!(
        cancel_receipted,
        "cancel_session gains its own paired receipt: {events:?}"
    );
    // The aborted ask: CommandFailed pairing the ASK's id, wrapping
    // the original ask.
    let ask_failed = events
        .iter()
        .find_map(|e| match &e.payload {
            EventPayload::CommandFailed {
                command_event_id,
                command,
                message,
                ..
            } => Some((*command_event_id, command.clone(), message.clone())),
            _ => None,
        })
        .expect("CommandFailed receipt present");
    assert_eq!(
        ask_failed.0, ask_received_id,
        "Done(Aborted) receipt pairs the in-flight ask"
    );
    assert_eq!(ask_failed.1.method, "ask_session");
    assert_eq!(ask_failed.1.params["user_message"], "spin forever");
    assert!(
        ask_failed.2.contains("abort"),
        "failure message names the abort: {}",
        ask_failed.2
    );
    // Exactly one receipt per command (INV-4): one success (cancel),
    // one failure (ask), nothing else.
    let receipt_count = events
        .iter()
        .filter(|e| {
            matches!(
                &e.payload,
                EventPayload::CommandSucceeded { .. }
                    | EventPayload::CommandFailed { .. }
                    | EventPayload::CommandRejected { .. }
            )
        })
        .count();
    assert_eq!(receipt_count, 2, "one receipt per command: {events:?}");

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
    let _ = std::fs::remove_dir_all(&journal_dir);
    let _ = std::fs::remove_dir_all(&events_dir);
}

/// Per-session FIFO ordering end-to-end (spec mu-046 WP8, the INV-3
/// blocker's regression test at the serve level): an `ask_session` and
/// a `cancel_session` PIPELINED in one write — no waiting for the ask
/// to start, unlike the WP4 test above — must reach the session in
/// journal order. Deterministic outcome via receipts: the ask is
/// always delivered first, enters the (stalling) turn, and is aborted
/// by the cancel — `CommandFailed` from `Done(Aborted)` pairing the
/// ask, `CommandSucceeded` pairing the cancel. Pre-WP8 the cancel
/// could overtake the ask (hitting an idle loop, leaving the ask
/// spinning forever with no abort and no receipt). Repeated across
/// sessions for scheduling-noise margin; each repetition is an
/// independent shot at the race.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pipelined_ask_then_cancel_keep_journal_order_wp8() {
    let journal_dir = unique_journal_dir();
    let events_dir = unique_events_dir();
    std::fs::create_dir_all(&events_dir).expect("create events dir");
    let provider: Arc<dyn Provider> = Arc::new(StallProvider);
    let (mut client, server_handle) =
        spawn_server_with_events(provider, journal_dir.clone(), Some(events_dir.clone()));
    authenticate(&mut client).await;

    for round in 0..3 {
        let session_id = create_session(&mut client, 100 + round).await;
        let ask_id = 200 + round * 2;
        let cancel_id = ask_id + 1;

        // ONE batched write: ask + cancel hit the read loop
        // back-to-back, journaling in that order.
        let ask = json!({
            "jsonrpc": "2.0", "id": ask_id, "method": "ask_session",
            "params": { "session_id": session_id, "user_message": "spin forever" }
        });
        let cancel = json!({
            "jsonrpc": "2.0", "id": cancel_id, "method": "cancel_session",
            "params": { "session_id": session_id }
        });
        client
            .write_all(format!("{ask}\n{cancel}\n").as_bytes())
            .await
            .expect("write pipelined ask+cancel");
        let resp = await_response(&mut client, ask_id).await;
        assert_eq!(resp["result"]["accepted"], true, "round {round}: {resp}");
        let resp = await_response(&mut client, cancel_id).await;
        assert_eq!(resp["result"]["cancelled"], true, "round {round}: {resp}");

        // FIFO guarantees the ask entered the loop BEFORE the cancel,
        // so the cancel always aborts it: CommandFailed (abort) for
        // the ask, CommandSucceeded for the cancel. Pre-WP8 the
        // inverted interleaving leaves the ask receiptless forever —
        // this poll times out.
        let events = timeout(Duration::from_secs(5), async {
            loop {
                let events = session_log_events(&events_dir, &session_id);
                if events
                    .iter()
                    .any(|e| matches!(&e.payload, EventPayload::CommandFailed { .. }))
                {
                    return events;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!("round {round}: pipelined ask was never aborted — cancel overtook it (INV-3)")
        });

        let ask_received_id = events
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::CommandReceived { method, .. } if method == "ask_session" => {
                    Some(e.id)
                }
                _ => None,
            })
            .expect("ask CommandReceived in session log");
        let cancel_received_id = events
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::CommandReceived { method, .. } if method == "cancel_session" => {
                    Some(e.id)
                }
                _ => None,
            })
            .expect("cancel CommandReceived in session log");
        assert!(
            ask_received_id < cancel_received_id,
            "round {round}: journal order is ask then cancel"
        );
        let ask_failed = events.iter().any(|e| {
            matches!(&e.payload, EventPayload::CommandFailed { command_event_id, command, message, .. }
                if *command_event_id == ask_received_id
                    && command.method == "ask_session"
                    && message.contains("abort"))
        });
        assert!(
            ask_failed,
            "round {round}: ask gains CommandFailed from Done(Aborted): {events:?}"
        );
        let cancel_succeeded = events.iter().any(|e| {
            matches!(&e.payload, EventPayload::CommandSucceeded { command_event_id, command, .. }
                if *command_event_id == cancel_received_id && command.method == "cancel_session")
        });
        assert!(
            cancel_succeeded,
            "round {round}: cancel gains CommandSucceeded: {events:?}"
        );
    }

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
    let _ = std::fs::remove_dir_all(&journal_dir);
    let _ = std::fs::remove_dir_all(&events_dir);
}
