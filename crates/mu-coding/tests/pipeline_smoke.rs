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
use mu_core::command_journal::{
    orphaned_command_seqs, CommandJournal, JournalPayload, JournalRecord, RejectStage,
};
use mu_core::config::{AuthConfig, Config, JournalConfig};

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
fn spawn_server_raw(
    provider: Arc<dyn Provider>,
    journal_dir: PathBuf,
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
        None,
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
/// dropped). Times out at 2s.
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
