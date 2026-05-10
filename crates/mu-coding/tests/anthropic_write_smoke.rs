//! mu-012: end-to-end vertical-slice verification for the write tool.
//!
//! Spawns `mu ask --provider anthropic-api --tools write "..."`,
//! asks Claude to write a known string to a temp file, asserts the
//! file ends up containing that string.
//!
//! Gated on `MU_LIVE_ANTHROPIC=1`. Mirrors `anthropic_read_smoke.rs`.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

const MU_BIN: &str = env!("CARGO_BIN_EXE_mu");

fn live_enabled() -> bool {
    std::env::var("MU_LIVE_ANTHROPIC").ok().as_deref() == Some("1")
}

#[tokio::test]
async fn mu_012_write_tool_end_to_end_via_anthropic() {
    if !live_enabled() {
        eprintln!(
            "skipping mu_012_write_tool_end_to_end_via_anthropic \
             (set MU_LIVE_ANTHROPIC=1 to run)"
        );
        return;
    }

    let tmp = std::env::temp_dir().join("mu_012_write_test.txt");
    // Make sure the file doesn't exist before we start, so we can
    // distinguish "Claude wrote it" from "it was already there".
    let _ = std::fs::remove_file(&tmp);

    let secret = "mu-012-write-secret-7c8e3";
    let prompt = format!(
        "Use the write tool to write the exact string '{secret}' (no \
         surrounding quotes, no newline before/after) to the file {}. \
         Then reply with just the word 'done' and nothing else.",
        tmp.display()
    );

    let output = timeout(
        Duration::from_secs(60),
        Command::new(MU_BIN)
            .arg("ask")
            .arg("--provider")
            .arg("anthropic-api")
            .arg("--tools")
            .arg("write")
            .arg(&prompt)
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
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let file_contents = std::fs::read_to_string(&tmp).ok();
    let _ = std::fs::remove_file(&tmp);

    assert!(
        output.status.success(),
        "mu ask exit status: {}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        output.status
    );

    let contents = file_contents.unwrap_or_else(|| {
        panic!(
            "expected the temp file to exist after mu ask; \
             stdout was:\n{stdout}\n--- stderr ---\n{stderr}"
        )
    });
    assert!(
        contents.contains(secret),
        "expected file to contain the secret '{secret}', got:\n{contents:?}\n\
         (stdout was: {stdout})"
    );
}
