//! mu-014: end-to-end vertical-slice verification for the ls tool.
//!
//! Mirrors `anthropic_read_smoke.rs`. Asks Claude to list a temp
//! directory's contents via the ls tool; asserts the response
//! mentions a known entry name.
//!
//! Gated on `MU_LIVE_ANTHROPIC=1`.

mod common;

use common::{live_anthropic_enabled, run_mu_ask};

#[tokio::test]
async fn mu_014_ls_tool_end_to_end_via_anthropic() {
    if !live_anthropic_enabled() {
        eprintln!(
            "skipping mu_014_ls_tool_end_to_end_via_anthropic \
             (set MU_LIVE_ANTHROPIC=1 to run)"
        );
        return;
    }

    // Create a temp dir with a known entry name we can pin in the
    // assertion. Cleanup at the end.
    let tmp = std::env::temp_dir().join("mu_014_ls_test");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir(&tmp).expect("create tmp dir");
    let known_name = "mu_014_marker_b8d9c.txt";
    std::fs::write(tmp.join(known_name), "x").expect("write marker");

    let prompt = format!(
        "Use the ls tool to list the contents of {}. Reply with just \
         the comma-separated names of the files you see, nothing else.",
        tmp.display()
    );

    let (stdout, status) =
        run_mu_ask(&["--provider", "anthropic-api", "--tools", "ls", &prompt]).await;

    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        status.success(),
        "non-zero exit: {status}\n--- stdout ---\n{stdout}"
    );
    assert!(
        stdout.contains(known_name),
        "expected stdout to mention {known_name}; got:\n{stdout}"
    );
}
