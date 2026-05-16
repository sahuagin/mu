//! mu-7rk (mu-yox): server-side BEARER handler + dispatcher smoke.
//!
//! Seven test cases covering:
//!
//! 1. `bearer_happy_path` — allowlisted token via `BearerHandler::step_initial`
//!    yields `Done(Capability::root())`.
//! 2. `bearer_rejects_bad_token` — non-allowlisted token yields
//!    `Denied { code: InvalidCredentials, .. }`.
//! 3. `unsupported_mechanism_rejection` — wire dispatch of
//!    `peer.auth_initiate` with `mechanism = "foo"` (deserializes to
//!    `AuthMechanism::Other("foo")`) — registry has no handler, server
//!    responds `Denied { code: UnsupportedMechanism, .. }`. Exercises
//!    the dispatcher's unsupported-mechanism path end-to-end.
//! 4. `malformed_exchange_missing_initial_response` — BEARER with
//!    `step_initial(None)` returns `Denied { code: MalformedExchange,
//!    .. }`.
//! 5. `oversized_token_rejected` — a token of `MAX_BEARER_TOKEN_LEN +
//!    1` bytes is rejected with `MalformedExchange` *before* digest
//!    computation (codex review important #2).
//! 6. `constant_time_comparison_smoke` — empirical variance check on
//!    `step_initial` timing across equal-length candidate tokens.
//!    Smoke-shaped: this can't prove timing-safety, only catch a
//!    regression to `==`/`HashSet::contains`.
//! 7. `duplicate_mechanism_registration_errors` — `AuthRegistry::new`
//!    with two handlers for the same mechanism returns
//!    `Err(DuplicateMechanismError(_))` (codex review minor #1).
//!
//! What's **not** here, by intent:
//!
//! - Tests that pin unauthenticated session.\*/mailbox.\* as allowed
//!   (codex review blocker #3 explicitly called the v0 test L5 out as
//!   an anti-pattern). Behavior of non-auth RPCs against an
//!   `Unauthenticated` connection is *not* asserted here — it belongs
//!   to mu-fnn (mu-7rk-c), which lands the enforcement gate.
//! - Tests for transport close on Denied (mu-1p6 / mu-7rk-d).
//! - Tests for the client-side connect flow (mu-5pn / mu-7rk-e).
//! - Tests for rate limiting (mu-7rk-f).
//! - Tests for multi-step challenge/response state (mu-oeo / mu-7rk-g).

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;

use mu_ai::FauxProvider;
use mu_coding::serve;
use mu_coding::serve::auth::{
    AuthMechanismHandler, AuthRegistry, AuthStepOutcome, BearerHandler, DuplicateMechanismError,
    MAX_BEARER_TOKEN_LEN,
};
use mu_core::agent::Provider;
use mu_core::capability::Capability;
use mu_core::config::{AuthConfig, Config};
use mu_core::protocol::{AuthDenialCode, AuthMechanism};

/// L1: BEARER happy path. Allowlisted token → `Done(Capability::root())`.
#[test]
fn bearer_happy_path() {
    let h = BearerHandler::new(vec!["secret-1".to_string()]);
    match h.step_initial(Some("secret-1")) {
        AuthStepOutcome::Done(c) => assert_eq!(c, Capability::root()),
        other => panic!("expected Done(Capability::root()); got {other:?}"),
    }
}

/// L2: BEARER bad-token rejection. Non-allowlisted token →
/// `Denied { code: InvalidCredentials }`.
#[test]
fn bearer_rejects_bad_token() {
    let h = BearerHandler::new(vec!["secret-1".to_string()]);
    match h.step_initial(Some("not-the-token")) {
        AuthStepOutcome::Denied { code, .. } => {
            assert_eq!(code, AuthDenialCode::InvalidCredentials);
        }
        other => panic!("expected Denied{{InvalidCredentials}}; got {other:?}"),
    }
}

/// L3: unsupported mechanism. Wire dispatch of `peer.auth_initiate`
/// with a mechanism string the registry has no handler for. The
/// dispatcher must respond `Denied { code: UnsupportedMechanism, .. }`
/// — NOT a JSON-RPC INVALID_PARAMS error.
#[tokio::test]
async fn unsupported_mechanism_rejection() {
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
    let (mut client, server_handle) =
        spawn_server(provider, config_with_bearer_tokens(&["secret-1"]));

    let req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "peer.auth_initiate",
        "params": { "mechanism": "foo", "initial_response": "anything" }
    });
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write");
    let resp = await_response(&mut client, 1).await;
    assert_eq!(resp["result"]["outcome"], "denied");
    assert_eq!(resp["result"]["code"], "unsupported_mechanism");

    drop(client);
    let _ = timeout(Duration::from_millis(500), server_handle).await;
}

/// L4: BEARER `step_initial(None)` → `Denied { code: MalformedExchange }`.
#[test]
fn malformed_exchange_missing_initial_response() {
    let h = BearerHandler::new(vec!["secret-1".to_string()]);
    match h.step_initial(None) {
        AuthStepOutcome::Denied { code, .. } => {
            assert_eq!(code, AuthDenialCode::MalformedExchange);
        }
        other => panic!("expected Denied{{MalformedExchange}}; got {other:?}"),
    }
}

/// L5: token over the length cap is rejected with `MalformedExchange`
/// before any hashing — protecting against allocation/CPU burn and
/// length-dependent timing leaks (codex review important #2).
#[test]
fn oversized_token_rejected() {
    let h = BearerHandler::new(vec!["secret-1".to_string()]);
    let oversized = "x".repeat(MAX_BEARER_TOKEN_LEN + 1);
    match h.step_initial(Some(&oversized)) {
        AuthStepOutcome::Denied { code, .. } => {
            assert_eq!(code, AuthDenialCode::MalformedExchange);
        }
        other => panic!("expected Denied{{MalformedExchange}}; got {other:?}"),
    }
}

/// L6: constant-time comparison smoke. We measure `step_initial`
/// timing across two candidate tokens that share an equal prefix-length
/// with the configured token but differ at the first byte vs. the last
/// byte. With a non-constant-time compare (`==` / `HashSet::contains`),
/// the differ-late candidate takes detectably longer; with `ct_eq` on
/// fixed-length SHA-256 digests, both candidates incur the same scan
/// time.
///
/// This is a *smoke* test — it asserts the ratio is within a generous
/// envelope, only enough to catch a regression to the v0 `String`
/// compare. CI noise + scheduler jitter prevent a tight assertion.
#[test]
fn constant_time_comparison_smoke() {
    // Use a 64-char token so both candidates share an equal byte
    // length, making digest input length identical.
    let token: String = "a".repeat(64);
    let h = BearerHandler::new(vec![token.clone()]);

    // Candidate A: differs at byte 0; would short-circuit fastest in
    // a non-CT `==`.
    let mut bad_first = token.clone().into_bytes();
    bad_first[0] = b'Z';
    let bad_first = String::from_utf8(bad_first).expect("utf8");

    // Candidate B: differs at the last byte; would short-circuit
    // slowest in a non-CT `==`.
    let mut bad_last = token.clone().into_bytes();
    let last = bad_last.len() - 1;
    bad_last[last] = b'Z';
    let bad_last = String::from_utf8(bad_last).expect("utf8");

    let iterations = 5_000;
    // Warm up — JIT/cache effects unrelated to the compare.
    for _ in 0..1_000 {
        let _ = h.step_initial(Some(&bad_first));
        let _ = h.step_initial(Some(&bad_last));
    }

    let start = Instant::now();
    for _ in 0..iterations {
        let _ = h.step_initial(Some(&bad_first));
    }
    let first_elapsed = start.elapsed();

    let start = Instant::now();
    for _ in 0..iterations {
        let _ = h.step_initial(Some(&bad_last));
    }
    let last_elapsed = start.elapsed();

    // With CT compare on fixed-length digests, ratio should be ~1.0.
    // A non-CT String compare would make `bad_first` MUCH faster than
    // `bad_last` (early-exit at byte 0 vs. byte 63), giving a ratio
    // well below 0.5. The 0.5-2.0 envelope leaves room for scheduler
    // jitter and CPU frequency scaling on CI runners while still
    // catching the regression cleanly.
    let ratio = first_elapsed.as_nanos() as f64 / last_elapsed.as_nanos() as f64;
    assert!(
        (0.5..=2.0).contains(&ratio),
        "constant-time compare regressed: first={first_elapsed:?}, last={last_elapsed:?}, ratio={ratio}",
    );
}

/// L7: registering two handlers for the same `AuthMechanism` returns
/// `DuplicateMechanismError`. Silent overwrite hid wiring bugs in v0
/// (codex review minor #1).
#[test]
fn duplicate_mechanism_registration_errors() {
    let handlers: Vec<Box<dyn AuthMechanismHandler + Send + Sync>> = vec![
        Box::new(BearerHandler::new(Vec::new())),
        Box::new(BearerHandler::new(Vec::new())),
    ];
    match AuthRegistry::new(handlers) {
        Err(DuplicateMechanismError(m)) => {
            assert_eq!(m, AuthMechanism::Bearer);
        }
        Ok(_) => panic!("expected DuplicateMechanismError; got Ok(_)"),
    }
}

// ───────────────────────── test harness ─────────────────────────

fn config_with_bearer_tokens(tokens: &[&str]) -> Config {
    Config {
        auth: AuthConfig::Bearer {
            tokens: tokens.iter().map(|t| (*t).to_string()).collect(),
        },
        ..Default::default()
    }
}

fn spawn_server(
    provider: Arc<dyn Provider>,
    config: Config,
) -> (
    tokio::io::DuplexStream,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let server_buf = BufReader::new(server_read);
    let factory: serve::ProviderFactory =
        std::sync::Arc::new(move |_selector| Ok(provider.clone()));
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
