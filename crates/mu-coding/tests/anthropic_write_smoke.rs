//! mu-012: end-to-end vertical-slice verification for the write tool.
//!
//! Mirrors `anthropic_read_smoke.rs`. Asks Claude to write a known
//! string to a temp file via the write tool; asserts the file ends
//! up containing that string.
//!
//! Gated on `MU_LIVE_ANTHROPIC=1`.

mod common;

use common::{live_anthropic_enabled, run_mu_ask};

#[tokio::test]
async fn mu_012_write_tool_end_to_end_via_anthropic() {
    if !live_anthropic_enabled() {
        eprintln!(
            "skipping mu_012_write_tool_end_to_end_via_anthropic \
             (set MU_LIVE_ANTHROPIC=1 to run)"
        );
        return;
    }

    let tmp = std::env::temp_dir().join("mu_012_write_test.txt");
    let _ = std::fs::remove_file(&tmp);

    let secret = "mu-012-write-secret-7c8e3";
    let prompt = format!(
        "Use the write tool to write the exact string '{secret}' (no \
         surrounding quotes, no newline before/after) to the file {}. \
         Then reply with just the word 'done' and nothing else.",
        tmp.display()
    );

    let (stdout, status) = run_mu_ask(&[
        "--provider",
        "anthropic-api",
        "--tools",
        "write",
        &prompt,
    ])
    .await;

    let file_contents = std::fs::read_to_string(&tmp).ok();
    let _ = std::fs::remove_file(&tmp);

    assert!(
        status.success(),
        "mu ask exit status: {status}\n--- stdout ---\n{stdout}"
    );

    let contents = file_contents.unwrap_or_else(|| {
        panic!(
            "expected the temp file to exist after mu ask; \
             stdout was:\n{stdout}"
        )
    });
    assert!(
        contents.contains(secret),
        "expected file to contain the secret '{secret}', got:\n{contents:?}\n\
         (stdout was: {stdout})"
    );
}
