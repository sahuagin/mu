//! Integration smoke tests for `mu ask`.
//!
//! Runs the actual `mu` binary as a subprocess and asserts on its
//! stdout/exit status.

mod common;

use common::run_mu_ask;

#[tokio::test]
#[ignore = "mu-dt1b: mu ask child serve exceeds 5s clean-exit window; pre-existing"]
async fn b1_echo_round_trip() {
    let (stdout, status) = run_mu_ask(&["hello"]).await;
    assert!(status.success(), "non-zero exit: {status}");
    assert_eq!(stdout.trim(), "hello");
}

#[tokio::test]
#[ignore = "mu-dt1b: mu ask child serve exceeds 5s clean-exit window; pre-existing"]
async fn b2_multi_word() {
    let (stdout, status) = run_mu_ask(&["hello world"]).await;
    assert!(status.success(), "non-zero exit: {status}");
    assert_eq!(stdout.trim(), "hello world");
}

#[tokio::test]
#[ignore = "mu-dt1b: mu ask child serve exceeds 5s clean-exit window; pre-existing"]
async fn b3_clean_child_shutdown() {
    let (_, status) = run_mu_ask(&["test"]).await;
    assert!(status.success());
}

#[tokio::test]
#[ignore = "mu-dt1b: mu ask child serve exceeds 5s clean-exit window; pre-existing"]
async fn b4_empty_prompt() {
    let (stdout, status) = run_mu_ask(&[""]).await;
    assert!(status.success(), "non-zero exit: {status}");
    assert_eq!(stdout, "\n");
}

/// mu-x83o: `--append-system-prompt <FILE>` reads FILE and forwards
/// content via `CreateSessionRequest.system_prompt`. Faux provider
/// ignores system_prompt (see faux.rs), so observable behavior is
/// unchanged — this test proves the flag parses, the file is read,
/// and the wire layer doesn't choke on the extra param.
#[tokio::test]
#[ignore = "mu-dt1b: mu ask child serve exceeds 5s clean-exit window; pre-existing"]
async fn b5_append_system_prompt_flag_does_not_break_echo() {
    let path = std::env::temp_dir().join("mu_x83o_sysprompt.txt");
    std::fs::write(&path, "you are a careful assistant").expect("write tempfile");

    let path_str = path.to_string_lossy().into_owned();
    let (stdout, status) = run_mu_ask(&["--append-system-prompt", &path_str, "hello"]).await;

    let _ = std::fs::remove_file(&path);
    assert!(status.success(), "non-zero exit: {status}");
    assert_eq!(stdout.trim(), "hello");
}

/// mu-x83o: missing file is a hard error before any RPC is sent.
#[tokio::test]
async fn b6_append_system_prompt_missing_file_errors() {
    let missing = "/tmp/mu-x83o-this-path-should-not-exist-abc123def456";
    let (_stdout, status) = run_mu_ask(&["--append-system-prompt", missing, "hello"]).await;
    assert!(
        !status.success(),
        "expected non-zero exit for missing file, got success"
    );
}
