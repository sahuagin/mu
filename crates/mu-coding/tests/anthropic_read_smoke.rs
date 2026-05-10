//! mu-010: end-to-end vertical-slice verification for the read tool.
//!
//! Spawns `mu ask --provider anthropic-api --tools read "..."`,
//! asks Claude to read a real file using the read tool, asserts
//! the answer reflects the file's contents.
//!
//! Gated on `MU_LIVE_ANTHROPIC=1` so CI never spends.

mod common;

use std::io::Write;

use common::{live_anthropic_enabled, run_mu_ask};

#[tokio::test]
async fn mu_010_read_tool_end_to_end_via_anthropic() {
    if !live_anthropic_enabled() {
        eprintln!(
            "skipping mu_010_read_tool_end_to_end_via_anthropic \
             (set MU_LIVE_ANTHROPIC=1 to run)"
        );
        return;
    }

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

    let (stdout, status) = run_mu_ask(&[
        "--provider",
        "anthropic-api",
        "--tools",
        "read",
        &prompt,
    ])
    .await;

    let _ = std::fs::remove_file(&tmp);

    assert!(status.success(), "non-zero exit: {status}\n--- stdout ---\n{stdout}");
    assert!(
        stdout.contains(secret),
        "expected stdout to contain the secret token; got:\n{stdout}"
    );
}
