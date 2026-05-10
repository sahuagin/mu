//! Integration smoke tests for `mu ask`.
//!
//! Runs the actual `mu` binary as a subprocess (via the cargo-provided
//! CARGO_BIN_EXE_mu env constant) and asserts on its stdout/exit
//! status.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

const MU_BIN: &str = env!("CARGO_BIN_EXE_mu");

/// Run `mu ask <prompt>` and return (stdout, status).
async fn run_ask(prompt: &str) -> (String, std::process::ExitStatus) {
    let output = timeout(
        Duration::from_secs(15),
        Command::new(MU_BIN)
            .arg("ask")
            .arg(prompt)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Critical: the spawned `mu ask` itself spawns `mu serve`.
            // Make sure the child process can find the same binary by
            // setting MU_BINARY explicitly.
            .env("MU_BINARY", MU_BIN)
            .output(),
    )
    .await
    .expect("mu ask did not finish within 15 seconds")
    .expect("running mu ask");

    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    (stdout, output.status)
}

#[tokio::test]
async fn b1_echo_round_trip() {
    let (stdout, status) = run_ask("hello").await;
    assert!(status.success(), "non-zero exit: {status}");
    assert_eq!(stdout.trim(), "hello");
}

#[tokio::test]
async fn b2_multi_word() {
    let (stdout, status) = run_ask("hello world").await;
    assert!(status.success(), "non-zero exit: {status}");
    assert_eq!(stdout.trim(), "hello world");
}

#[tokio::test]
async fn b3_clean_child_shutdown() {
    // If the child weren't shutting down cleanly, run_ask's 15s timeout
    // would fire — and we'd see a panic, not a passing assertion. The
    // fact that this test completes within ~5 seconds proves clean
    // shutdown.
    let (_, status) = run_ask("test").await;
    assert!(status.success());
}

#[tokio::test]
async fn b4_empty_prompt() {
    let (stdout, status) = run_ask("").await;
    assert!(status.success(), "non-zero exit: {status}");
    // Empty echo + println adds a newline.
    assert_eq!(stdout, "\n");
}
