//! `mu ask` — one-shot CLI frontend over `mu serve`.
//!
//! Spawns `mu serve` as a subprocess, speaks JSON-RPC over its
//! stdio, sends `create_session` + `ask_session`, drains notifications
//! until `session.done`, prints the assistant text, exits.
//!
//! See spec mu-005.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

use mu_core::protocol::{AskSessionRequest, CreateSessionRequest, CreateSessionResponse};

/// Options for [`run`] — the CLI-flag bundle for a single `mu ask`
/// invocation. Constructed by the CLI binary's argument parser.
#[derive(Debug, Default)]
pub struct AskOptions {
    pub prompt: String,
    pub provider: String,
    pub model: Option<String>,
    pub tools: String,
    pub ephemeral: bool,
    pub thinking: Option<String>,
    pub bash_yolo: bool,
    pub bash_allow: Vec<String>,
    pub bash_prompt: bool,
    /// System prompt for the session. When present, sent as
    /// `CreateSessionRequest.system_prompt` (mu-n48 plumbing); when
    /// None, the daemon default is used. Populated by the CLI from
    /// `--append-system-prompt <FILE>` (file content read by the
    /// binary, not here, so this layer stays I/O-free).
    pub system_prompt: Option<String>,
}

/// Run a single `mu ask` invocation. Flags (`provider`, `model`,
/// `tools`) are forwarded to the spawned `mu serve`.
pub async fn run(opts: AskOptions) -> Result<()> {
    // Map the CLI provider flag to a wire-level selector. This is what
    // gets sent in create_session; the daemon constructs the provider
    // per session from this.
    let selector = crate::serve::selector_from_cli(&opts.provider, opts.model.as_deref())?;

    // mu-fnn: generate a per-spawn bearer token for the trust-on-spawn
    // handshake with the child `mu serve`. The child reads
    // `MU_BEARER_TOKEN` from its env (see `serve::run`) and configures
    // BEARER auth with this single token; we then present the same
    // token in `peer.auth_initiate` before any session.* RPC.
    let bearer_token = generate_bearer_token();

    let mut child = spawn_serve(
        &opts.tools,
        opts.ephemeral,
        opts.thinking.as_deref(),
        opts.bash_yolo,
        &opts.bash_allow,
        opts.bash_prompt,
        &bearer_token,
    )?;
    let mut stdin = child.stdin.take().context("child stdin not captured")?;
    let stdout = child.stdout.take().context("child stdout not captured")?;
    let mut stdout = BufReader::new(stdout);

    let mut next_id: u64 = 1;

    // Authenticate before any protected RPC. Failure here is fatal —
    // the gate will reject every subsequent call.
    authenticate(&mut stdin, &mut stdout, &mut next_id, &bearer_token).await?;

    // mu-phl v0 / mu-lfgh: capture the operator's cwd at the entry of
    // the ask path so the daemon's session-start recall (subprocess
    // agent memory + project-file hierarchy) scopes to the operator's
    // actual project. Falls back to `None` if cwd can't be determined
    // (extremely unusual; the daemon resolves its own fallback in
    // build_project_context).
    let invocation_cwd = std::env::current_dir().ok();
    let session_id = create_session(
        &mut stdin,
        &mut stdout,
        &mut next_id,
        &selector,
        opts.system_prompt.as_deref(),
        invocation_cwd,
    )
    .await?;
    let text = ask_and_drain(
        &mut stdin,
        &mut stdout,
        &session_id,
        &opts.prompt,
        &mut next_id,
    )
    .await?;

    println!("{}", text);

    // Closing stdin signals the daemon to exit cleanly.
    drop(stdin);
    match timeout(Duration::from_secs(5), child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => bail!("mu serve exited with status {status}"),
        Ok(Err(e)) => Err(e).context("waiting for child"),
        Err(_) => {
            let _ = child.kill().await;
            bail!("mu serve did not exit within 5 seconds; killed")
        }
    }
}

/// Generate a per-spawn opaque bearer token for the parent↔child
/// handshake. The strength bar is "unguessable across this process
/// lifetime"; SHA-256 + constant-time comparison on the daemon side
/// already absorb timing concerns. 32 hex chars / 128 bits is plenty.
fn generate_bearer_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Run the `peer.auth_initiate` BEARER handshake. Returns Ok on
/// `Accepted`; surfaces a clear error on `Denied` or any non-success.
async fn authenticate(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    next_id: &mut u64,
    token: &str,
) -> Result<()> {
    let id = *next_id;
    *next_id += 1;
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "peer.auth_initiate",
        "params": {
            "mechanism": "bearer",
            "initial_response": token,
        },
    });
    write_line(stdin, &req).await?;
    loop {
        let line = read_line(stdout).await?;
        if line.get("id").and_then(|v| v.as_u64()) == Some(id) {
            let outcome = line
                .get("result")
                .and_then(|r| r.get("outcome"))
                .and_then(|v| v.as_str());
            if outcome == Some("accepted") {
                return Ok(());
            }
            bail!("peer.auth_initiate did not accept the spawn-time token: {line}");
        }
        // Skip unrelated notifications.
    }
}

fn spawn_serve(
    tools: &str,
    ephemeral: bool,
    thinking: Option<&str>,
    bash_yolo: bool,
    bash_allow: &[String],
    bash_prompt: bool,
    bearer_token: &str,
) -> Result<tokio::process::Child> {
    // MU_BINARY env override allows integration tests to point at a
    // specific binary path (`env!("CARGO_BIN_EXE_mu")`); production
    // falls back to the current executable.
    let binary = match std::env::var("MU_BINARY") {
        Ok(v) if !v.is_empty() => v,
        _ => std::env::current_exe()
            .context("could not determine current_exe")?
            .to_string_lossy()
            .into_owned(),
    };

    let mut cmd = Command::new(&binary);
    cmd.arg("serve");
    // mu-fnn: hand the child the same BEARER token we'll present at
    // `peer.auth_initiate`. Single source of truth: this string.
    cmd.env("MU_BEARER_TOKEN", bearer_token);
    if !tools.is_empty() {
        cmd.arg("--tools").arg(tools);
    }
    if ephemeral {
        cmd.arg("--ephemeral");
    }
    if let Some(t) = thinking {
        if !t.is_empty() {
            cmd.arg("--thinking").arg(t);
        }
    }
    if bash_yolo {
        cmd.arg("--bash-yolo");
    }
    for entry in bash_allow {
        if !entry.is_empty() {
            cmd.arg("--bash-allow").arg(entry);
        }
    }
    if bash_prompt {
        cmd.arg("--bash-prompt");
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        // stderr inherited — daemon logs go to the user's terminal.
        .spawn()
        .with_context(|| format!("failed to spawn `{binary} serve`"))
}

async fn create_session(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    next_id: &mut u64,
    selector: &mu_core::protocol::ProviderSelector,
    system_prompt: Option<&str>,
    cwd: Option<std::path::PathBuf>,
) -> Result<String> {
    let id = *next_id;
    *next_id += 1;
    // Build params from the typed protocol struct so that
    // serde's `skip_serializing_if = "Option::is_none"` on
    // CreateSessionRequest.system_prompt is honored — no
    // explicit null field when unset (mu-x83o).
    //
    // mu-phl v0 / mu-lfgh: cwd is plumbed through from the operator's
    // invocation (set by the `ask()` entry point to
    // std::env::current_dir()) so the daemon-side recall providers
    // (subprocess agent memory + project-file hierarchy) scope to the
    // operator's actual project rather than the daemon's process cwd.
    let body = CreateSessionRequest {
        provider: selector.clone(),
        system_prompt: system_prompt.map(str::to_owned),
        cwd,
        // mu-f1a0: `mu ask` is a batch-shaped one-shot — the 5m
        // default tier is correct (no human gaps to survive).
        cache_ttl: None,
    };
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": CreateSessionRequest::METHOD,
        "params": body,
    });
    write_line(stdin, &req).await?;

    loop {
        let line = read_line(stdout).await?;
        if line.get("id") == Some(&Value::from(id)) {
            if let Some(error) = line.get("error") {
                bail!("create_session failed: {error}");
            }
            let result = line
                .get("result")
                .cloned()
                .ok_or_else(|| anyhow!("create_session response missing `result`"))?;
            let resp: CreateSessionResponse =
                serde_json::from_value(result).context("parse CreateSessionResponse")?;
            return Ok(resp.session_id);
        }
        // Other notifications (none expected this early) — ignore.
    }
}

async fn ask_and_drain(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    session_id: &str,
    prompt: &str,
    next_id: &mut u64,
) -> Result<String> {
    let id = *next_id;
    *next_id += 1;
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": AskSessionRequest::METHOD,
        "params": {
            "session_id": session_id,
            "user_message": prompt,
        }
    });
    write_line(stdin, &req).await?;

    let mut text = String::new();
    let mut got_done = false;
    let mut got_response = false;

    loop {
        let line = read_line(stdout).await?;
        match line.get("method").and_then(Value::as_str) {
            Some("session.text_delta") => {
                if line["params"]["session_id"] == session_id {
                    if let Some(delta) = line["params"]["delta"].as_str() {
                        text.push_str(delta);
                    }
                }
            }
            Some("session.done") => {
                if line["params"]["session_id"] == session_id {
                    got_done = true;
                }
            }
            Some("session.error") => {
                if line["params"]["session_id"] == session_id {
                    let msg = line["params"]["message"].as_str().unwrap_or("(no message)");
                    bail!("session error: {msg}");
                }
            }
            _ => {
                // Could be the ask_session response itself.
                if line.get("id") == Some(&Value::from(id)) {
                    if let Some(error) = line.get("error") {
                        bail!("ask_session failed: {error}");
                    }
                    got_response = true;
                }
                // Else: tool events or other notifications — drop in v1.
            }
        }
        if got_done && got_response {
            return Ok(text);
        }
    }
}

async fn write_line(stdin: &mut ChildStdin, value: &Value) -> Result<()> {
    let mut s = serde_json::to_string(value)?;
    s.push('\n');
    stdin.write_all(s.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_line(stdout: &mut BufReader<ChildStdout>) -> Result<Value> {
    let mut line = String::new();
    let n = stdout.read_line(&mut line).await?;
    if n == 0 {
        bail!("mu serve closed stdout unexpectedly");
    }
    serde_json::from_str(line.trim_end_matches('\n')).context("parse JSON line from daemon")
}
