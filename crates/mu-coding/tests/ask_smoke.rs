//! Integration smoke tests for `mu ask`.
//!
//! Runs the actual `mu` binary as a subprocess and asserts on its
//! stdout/exit status.

mod common;

use common::run_mu_ask;

#[tokio::test]
async fn b1_echo_round_trip() {
    let (stdout, status) = run_mu_ask(&["hello"]).await;
    assert!(status.success(), "non-zero exit: {status}");
    assert_eq!(stdout.trim(), "hello");
}

#[tokio::test]
async fn b2_multi_word() {
    let (stdout, status) = run_mu_ask(&["hello world"]).await;
    assert!(status.success(), "non-zero exit: {status}");
    assert_eq!(stdout.trim(), "hello world");
}

#[tokio::test]
async fn b3_clean_child_shutdown() {
    let (_, status) = run_mu_ask(&["test"]).await;
    assert!(status.success());
}

#[tokio::test]
async fn b4_empty_prompt() {
    let (stdout, status) = run_mu_ask(&[""]).await;
    assert!(status.success(), "non-zero exit: {status}");
    assert_eq!(stdout, "\n");
}
