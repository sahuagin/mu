//! spec mu-046 WP6 load-bearing tests: the boot sequence at the serve
//! level. Same hermetic harness shape as `pipeline_smoke.rs`
//! (`tokio::io::duplex` + `[journal].dir` tempdir), asserting on the
//! raw journal:
//!
//! - boot order (INV-9): record 1 is `JournalOpened`, record 2 is
//!   `ConfigLoaded` — the resolved config is a journaled, sequenced
//!   message BEFORE any adapter accepts traffic, so the very first
//!   adapter command's seq is strictly greater.
//! - provenance: `sources` lists at least `"defaults"`.
//! - redaction (INV-6): the configured bearer token appears NOWHERE in
//!   the raw journal bytes — `redact_config` ran before append.
//! - `--bare` reflection: the bare rewrite (`run()` flips
//!   `[recall].enabled`/`bare` before serving; tests pass the
//!   equivalent config) is visible in the journaled effective config.
//! - `[journal].journal_queries = false`: read-only queries leave no
//!   `CommandReceived`/receipt but still answer; mutating commands
//!   still journal.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;

use mu_ai::FauxProvider;
use mu_coding::serve;
use mu_core::agent::Provider;
use mu_core::command_journal::{CommandJournal, JournalPayload, JournalRecord};
use mu_core::config::{AuthConfig, Config, JournalConfig, RecallConfig};

/// Shared bearer token — also the secret the INV-6 assertion greps the
/// raw journal bytes for.
const TEST_BEARER_TOKEN: &str = "boot-smoke-secret-token";

/// A unique throwaway journal dir under the system temp dir.
fn unique_journal_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "mu-boot-smoke-journal-{}-{}",
        std::process::id(),
        n
    ))
}

/// The boot-test config: bearer auth (the redaction secret), the
/// journal at `journal_dir`, and the `--bare`-EQUIVALENT rewrite that
/// `serve::run` applies for the `--bare` CLI flag (`[recall].enabled =
/// false`, `bare = true`) — `serve_with_io_with_config` takes the
/// already-rewritten config, so tests reproduce the rewrite here.
fn boot_config(journal_dir: PathBuf, journal_queries: bool) -> Config {
    Config {
        auth: AuthConfig::Bearer {
            tokens: vec![TEST_BEARER_TOKEN.to_string()],
        },
        journal: JournalConfig {
            dir: Some(journal_dir),
            journal_queries,
            ..Default::default()
        },
        recall: RecallConfig {
            enabled: false,
            bare: true,
            ..Default::default()
        },
        // Hermetic: no startup ollama probe from tests (LAN-baked base
        // is unroutable on CI runners).
        routes: mu_core::config::RoutesConfig {
            ollama_discover: false,
        },
        ..Default::default()
    }
}

/// Spawn `serve_with_io_with_config` over a duplex pipe. Does NOT
/// authenticate.
fn spawn_server(
    config: Config,
) -> (
    tokio::io::DuplexStream,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
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

/// Read exactly one newline-terminated JSON line.
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

/// Skim lines until the response with `id` arrives. 30s budget —
/// generous on purpose: passing tests complete in milliseconds, and a
/// tight budget only converts CI runner contention into flakes.
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

/// BEARER handshake so subsequent RPCs pass the gate.
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

/// Replay the single `<daemon_id>.jsonl` journal in `dir`.
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

/// Boot order + provenance + redaction + bare reflection (INV-9,
/// INV-6): after serve starts, record 1 is `JournalOpened`, record 2
/// is `ConfigLoaded`; `sources` lists at least `"defaults"`; the raw
/// journal bytes never contain the configured bearer token; and the
/// `--bare`-equivalent rewrite is visible in the journaled config.
/// Plus the sequencing assertion: a ping sent immediately at connect
/// gets a `CommandReceived` seq strictly greater than ConfigLoaded's.
#[tokio::test]
async fn boot_journals_config_loaded_before_any_adapter_command_inv9() {
    let journal_dir = unique_journal_dir();
    let (mut client, server_handle) = spawn_server(boot_config(journal_dir.clone(), true));

    // The very first thing this client does is ping — no auth first.
    // The unauthenticated ping is rejected at the gate, but the border
    // record (`CommandReceived`) exists either way, which is all the
    // sequencing assertion needs.
    let req = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping", "params": null });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write ping");
    let resp = await_response(&mut client, 1).await;
    assert!(
        resp.get("error").is_some() || resp.get("result").is_some(),
        "ping must be answered: {resp}"
    );

    let (path, records) = read_journal(&journal_dir);
    // Record 1: JournalOpened (appended by open()).
    assert!(
        matches!(records[0].payload, JournalPayload::JournalOpened { .. }),
        "record 1 must be JournalOpened: {records:?}"
    );
    assert_eq!(records[0].seq, 1);
    // Record 2: ConfigLoaded — the resolved config as a message.
    let config_seq = records[1].seq;
    let (sources, config) = match &records[1].payload {
        JournalPayload::ConfigLoaded { sources, config } => (sources.clone(), config.clone()),
        other => panic!("record 2 must be ConfigLoaded: {other:?}"),
    };
    assert!(
        sources.iter().any(|s| s == "defaults"),
        "sources lists at least defaults: {sources:?}"
    );
    // The --bare-equivalent rewrite is reflected in the journaled
    // effective config.
    assert_eq!(config["recall"]["bare"], true, "bare rewrite journaled");
    assert_eq!(config["recall"]["enabled"], false);
    // Secrets are redacted in the structured record...
    assert_eq!(config["auth"]["tokens"], "[REDACTED]");
    // ...and absent from the raw bytes (INV-6, the load-bearing form).
    let raw = std::fs::read_to_string(&path).expect("read raw journal");
    assert!(
        !raw.contains(TEST_BEARER_TOKEN),
        "bearer token leaked into the journal"
    );
    // Sequencing: the first adapter command lands strictly AFTER
    // ConfigLoaded.
    let ping_seq = records
        .iter()
        .find_map(|r| match &r.payload {
            JournalPayload::CommandReceived { method, .. } if method == "ping" => Some(r.seq),
            _ => None,
        })
        .expect("ping CommandReceived in journal");
    assert!(
        ping_seq > config_seq,
        "adapter command (seq {ping_seq}) must sequence after ConfigLoaded (seq {config_seq})"
    );
    // And exactly one ConfigLoaded — boot writes it once.
    let config_loaded_count = records
        .iter()
        .filter(|r| matches!(&r.payload, JournalPayload::ConfigLoaded { .. }))
        .count();
    assert_eq!(config_loaded_count, 1);

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
    let _ = std::fs::remove_dir_all(&journal_dir);
}

/// `[journal].journal_queries = false` (WP6): a read-only query (ping)
/// still gets its response but leaves NO `CommandReceived` and no
/// receipt in the journal; mutating commands (`peer.auth_initiate`,
/// `create_session`) still journal. ConfigLoaded is unaffected by the
/// knob — it is not a query.
#[tokio::test]
async fn journal_queries_false_skips_reads_but_journals_mutations() {
    let journal_dir = unique_journal_dir();
    let (mut client, server_handle) = spawn_server(boot_config(journal_dir.clone(), false));

    // Authenticate (mutating: journaled) so the ping passes the gate.
    authenticate(&mut client).await;

    let req = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping", "params": null });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write ping");
    let resp = await_response(&mut client, 1).await;
    assert_eq!(resp["result"]["pong"], true, "ping still answered: {resp}");

    // A mutating command afterwards, so the journal demonstrably kept
    // writing around the skipped query.
    let req = json!({
        "jsonrpc": "2.0", "id": 2, "method": "create_session",
        "params": { "provider": { "kind": "anthropic_api", "model": "x" } }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write create");
    let resp = await_response(&mut client, 2).await;
    assert!(
        resp["result"]["session_id"].is_string(),
        "create ok: {resp}"
    );

    let (_path, records) = read_journal(&journal_dir);
    // No trace of the ping: no border record...
    assert!(
        !records.iter().any(|r| matches!(
            &r.payload,
            JournalPayload::CommandReceived { method, .. } if method == "ping"
        )),
        "ping must not be journaled with journal_queries=false: {records:?}"
    );
    // ...and no receipt echoing it.
    assert!(
        !records.iter().any(|r| match &r.payload {
            JournalPayload::CommandSucceeded { command, .. }
            | JournalPayload::CommandFailed { command, .. }
            | JournalPayload::CommandRejected { command, .. } => command.method == "ping",
            _ => false,
        }),
        "ping must leave no receipt: {records:?}"
    );
    // Mutating commands still journal — border record + receipt.
    for mutating in ["peer.auth_initiate", "create_session"] {
        let seq = records
            .iter()
            .find_map(|r| match &r.payload {
                JournalPayload::CommandReceived { method, .. } if method == mutating => Some(r.seq),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{mutating} must still journal: {records:?}"));
        assert!(
            records.iter().any(|r| matches!(
                &r.payload,
                JournalPayload::CommandSucceeded { command_seq, .. } if *command_seq == seq
            )),
            "{mutating} must still gain its receipt: {records:?}"
        );
    }

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
    let _ = std::fs::remove_dir_all(&journal_dir);
}
