//! mu-010: end-to-end vertical-slice verification.
//!
//! Spawns `mu ask --provider anthropic-api --tools read "..."` as a
//! subprocess, asks Claude to read a real file using the read tool,
//! asserts the answer reflects the file's contents.
//!
//! Gated on `MU_LIVE_ANTHROPIC=1` so CI never spends. Set the env
//! var and `ANTHROPIC_API_KEY` to run.
//!
//! After this test passes, the read-tool vertical slice is proven:
//! mu-001 protocol ↔ mu-002 transport ↔ mu-003 agent loop ↔ mu-006
//! Anthropic API ↔ mu-007 read tool ↔ mu-008 tool support ↔ mu-009
//! config wiring all working together.

use std::io::Write;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

const MU_BIN: &str = env!("CARGO_BIN_EXE_mu");

fn live_enabled() -> bool {
    std::env::var("MU_LIVE_ANTHROPIC").ok().as_deref() == Some("1")
}

#[tokio::test]
async fn mu_010_read_tool_end_to_end_via_anthropic() {
    if !live_enabled() {
        eprintln!(
            "skipping mu_010_read_tool_end_to_end_via_anthropic \
             (set MU_LIVE_ANTHROPIC=1 to run)"
        );
        return;
    }

    // Write a deterministic file for Claude to read.
    let tmp = std::env::temp_dir().join("mu_010_read_test.txt");
    let secret = "the-mu-010-secret-token-93f1a";
    {
        let mut f = std::fs::File::create(&tmp).expect("create tmp file");
        writeln!(f, "{secret}").expect("write tmp file");
    }

    let prompt = format!(
        "Use the read tool to read {}. Tell me what's on the first line of \
         that file. Reply with only that single word, nothing else.",
        tmp.display()
    );

    let output = timeout(
        Duration::from_secs(60),
        Command::new(MU_BIN)
            .arg("ask")
            .arg("--provider")
            .arg("anthropic-api")
            .arg("--tools")
            .arg("read")
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

    let _ = std::fs::remove_file(&tmp);

    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        output.status.success(),
        "mu ask exit status: {}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        output.status
    );
    assert!(
        stdout.contains(secret),
        "expected stdout to contain the secret token; got:\n{stdout}\n\n\
         (stderr was: {stderr})"
    );
}
