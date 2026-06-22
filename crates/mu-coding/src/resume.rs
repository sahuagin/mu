//! `mu resume` (a.k.a. `mu --resume`) — STRICT fork-at-tail resume of a
//! dead session (mu-mh4).
//!
//! Spawns `mu serve`, authenticates, then calls `session.resume` with the
//! predecessor's `daemon:session` / `mu:<daemon>/<session>` ref. The
//! daemon no longer rehydrates every log at startup
//! (mu-lazy-session-rehydration-bh4f); it lazily find-by-ids and parses
//! the predecessor's one log on demand when `session.resume` resolves it. The
//! daemon projects the predecessor's log to its last clean boundary and
//! births a fresh live session seeded with that history — or REFUSES
//! with a precise diagnosis if the log is ragged (and the CLI prints the
//! `mu --recover` hint the daemon supplies).
//!
//! On success the resumed session id is printed. When a `prompt` is
//! given, the resumed session is immediately asked it (the new head
//! continues the conversation); otherwise the command just reports the
//! attach and exits, leaving the session resumable by a TUI/console.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::BufReader;
use tokio::process::{ChildStdin, ChildStdout};
use tokio::time::timeout;

use mu_core::protocol::{ResumeSessionRequest, ResumeSessionResponse};

use crate::ask::{
    ask_and_drain, authenticate, generate_bearer_token, read_line, spawn_serve, write_line,
};

/// Options for [`run`] — the CLI-flag bundle for one `mu resume`.
#[derive(Debug, Default)]
pub struct ResumeOptions {
    /// Predecessor session ref: `daemon:session` or
    /// `mu:<daemon>/<session>`.
    pub session_ref: String,
    /// Optional prompt to ask the resumed session immediately. When
    /// None, the command attaches the head and exits.
    pub prompt: Option<String>,
    /// Provider backend for the resumed (live) session.
    pub provider: String,
    /// Model id for the resumed session.
    pub model: Option<String>,
    /// Tools to enable (forwarded to the spawned `mu serve`).
    pub tools: String,
    pub ephemeral: bool,
    pub thinking: Option<String>,
    pub bash_yolo: bool,
    pub bash_allow: Vec<String>,
    pub bash_prompt: bool,
    pub bare: bool,
}

/// Run a single `mu resume` invocation.
pub async fn run(opts: ResumeOptions) -> Result<()> {
    // Resolve a possible SELECTION alias before the wire mapping (mu-eb98
    // item 2): a favorite name/alias as --model rewrites {provider, model}.
    let (provider, model) =
        crate::serve::resolve_launch_selection(&opts.provider, opts.model.as_deref());
    let selector = crate::serve::selector_from_cli(&provider, model.as_deref())?;
    let bearer_token = generate_bearer_token();

    let mut child = spawn_serve(
        &opts.tools,
        opts.ephemeral,
        opts.thinking.as_deref(),
        opts.bash_yolo,
        &opts.bash_allow,
        opts.bash_prompt,
        opts.bare,
        &bearer_token,
    )?;
    let mut stdin = child.stdin.take().context("child stdin not captured")?;
    let stdout = child.stdout.take().context("child stdout not captured")?;
    let mut stdout = BufReader::new(stdout);

    let mut next_id: u64 = 1;
    authenticate(&mut stdin, &mut stdout, &mut next_id, &bearer_token).await?;

    let invocation_cwd = std::env::current_dir().ok();
    let resp = resume_session(
        &mut stdin,
        &mut stdout,
        &mut next_id,
        &opts.session_ref,
        &selector,
        invocation_cwd,
    )
    .await?;

    eprintln!(
        "resumed {} as session {} (forked at predecessor event {}, seeded {} message(s))",
        opts.session_ref,
        resp.session_id,
        resp.branched_at_event_id
            .map(|i| i.to_string())
            .unwrap_or_else(|| "<start>".into()),
        resp.seeded_message_count,
    );

    let stop_reason = if let Some(prompt) = opts.prompt.as_deref() {
        let (text, stop_reason) = ask_and_drain(
            &mut stdin,
            &mut stdout,
            &resp.session_id,
            prompt,
            // `mu resume` has no per-turn `/effort` flag (yet); leave the
            // resumed session's standing effort untouched. (mu-bez6)
            None,
            &mut next_id,
        )
        .await?;
        println!("{text}");
        stop_reason
    } else {
        // No prompt: just report the resumed session id on stdout so a
        // caller can capture it.
        println!("{}", resp.session_id);
        None
    };

    // Dropping stdin signals EOF; the daemon's serve loop sees it and
    // begins a CLEAN shutdown (flushing each session's JSONL writer).
    // mu-mh4 (panel finding 5): give that clean shutdown a GENEROUS grace
    // window before resorting to SIGKILL. SIGKILL cannot be caught, so it
    // can truncate a session log mid-write — the ragged tail this very
    // feature exists to manage. A long patience window means we
    // effectively never SIGKILL a healthy-but-slow daemon; the kill stays
    // a true last resort for a genuinely wedged process. (A SIGTERM-then-
    // grace-then-SIGKILL escalation would be strictly nicer but needs a
    // signal dep — nix/libc — which this crate does not carry; deferred to
    // mu-nqn5's cleanup pass rather than pulling in a dep for a last-
    // resort path.)
    drop(stdin);
    match timeout(Duration::from_secs(30), child.wait()).await {
        Ok(Ok(status)) if status.success() => {}
        Ok(Ok(status)) => bail!("mu serve exited with status {status}"),
        Ok(Err(e)) => return Err(e).context("waiting for child"),
        Err(_) => {
            let _ = child.kill().await;
            bail!(
                "mu serve did not exit within 30 seconds; killed (SIGKILL — log may be truncated)"
            )
        }
    }

    match stop_reason.as_deref() {
        Some("max_tokens") => {
            bail!("response truncated (stop_reason=max_tokens). Output above may be a fragment.")
        }
        Some("degraded_eof") => {
            bail!("response degraded (stop_reason=degraded_eof). Output above may be a fragment.")
        }
        _ => Ok(()),
    }
}

async fn resume_session(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    next_id: &mut u64,
    session_ref: &str,
    selector: &mu_core::protocol::ProviderSelector,
    cwd: Option<std::path::PathBuf>,
) -> Result<ResumeSessionResponse> {
    let id = *next_id;
    *next_id += 1;
    let body = ResumeSessionRequest {
        session_ref: session_ref.to_string(),
        provider: selector.clone(),
        attenuations: None,
        cwd,
        actor: None,
    };
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": ResumeSessionRequest::METHOD,
        "params": body,
    });
    write_line(stdin, &req).await?;

    // mu-mh4 (panel finding 2): bound the read loop. Without this, an
    // unresponsive daemon (hung, deadlocked, etc.) hangs the CLI forever
    // — no terminal frame ever arrives, so a bare `loop` blocks
    // indefinitely. Mirror ask.rs's `timeout()` discipline; the window is
    // generous (the project prefers long timeouts over premature failure,
    // and resume waits on a fresh `mu serve` plus the lazy parse of the
    // predecessor's one log — mu-lazy-session-rehydration-bh4f).
    let read_loop = async {
        loop {
            let line = read_line(stdout).await?;
            if line.get("id") == Some(&Value::from(id)) {
                if let Some(error) = line.get("error") {
                    // The daemon's refusal message already carries the
                    // precise diagnosis + the `mu --recover` hint.
                    let msg = error
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("(no message)");
                    bail!("{msg}");
                }
                let result = line
                    .get("result")
                    .cloned()
                    .ok_or_else(|| anyhow!("session.resume response missing `result`"))?;
                return serde_json::from_value(result).context("parse ResumeSessionResponse");
            }
            // Skip unrelated notifications.
        }
    };
    match timeout(Duration::from_secs(120), read_loop).await {
        Ok(res) => res,
        Err(_) => bail!(
            "session.resume timed out after 120s waiting for the daemon to respond \
             (the daemon may be hung, or the predecessor log is very large to parse)"
        ),
    }
}
