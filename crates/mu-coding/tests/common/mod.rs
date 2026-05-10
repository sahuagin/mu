//! Shared helpers for mu-coding's integration tests.
//!
//! Each `tests/<file>.rs` is compiled as its own binary; `mod
//! common;` brings this module in. Use `tests/common/` rather than
//! `tests/common.rs` so cargo treats it as a subdirectory and
//! doesn't compile a `common` binary on its own.

use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

/// Path to the mu binary built by the current `cargo test` run.
pub const MU_BIN: &str = env!("CARGO_BIN_EXE_mu");

/// True when MU_LIVE_ANTHROPIC=1. Live tests should `return` early
/// (after a clear `eprintln!`) when this is false.
#[allow(dead_code)] // not all integration tests use this
pub fn live_anthropic_enabled() -> bool {
    std::env::var("MU_LIVE_ANTHROPIC").ok().as_deref() == Some("1")
}

/// Spawn `mu ask` with the given args (after `ask`), wait up to 60s,
/// return (stdout, status).
///
/// Sets `MU_BINARY` so the spawned `mu ask` can find the same
/// binary when it spawns its own `mu serve` child.
pub async fn run_mu_ask(args: &[&str]) -> (String, ExitStatus) {
    let output = timeout(
        Duration::from_secs(60),
        Command::new(MU_BIN)
            .arg("ask")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("MU_BINARY", MU_BIN)
            .output(),
    )
    .await
    .expect("mu ask did not finish within 60 seconds")
    .expect("running mu ask");

    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    (stdout, output.status)
}
